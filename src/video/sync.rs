//! Pure parts of video compare: start points and offsets, the A-B loop,
//! pane layout, the shared relative view, output size debouncing and the
//! names and composite of saved frames. No FFmpeg, no UI state.

use egui::{Pos2, Rect, Vec2};
use image::RgbaImage;
use std::time::{Duration, Instant};

/// Videos open at once.
pub const MAX_VIDEOS: usize = 4;

/// Pane letters, the same as image Compare.
pub const LETTERS: [&str; MAX_VIDEOS] = ["A", "B", "C", "D"];

// ---------------------------------------------------------------- start points

/// Seconds each video is ahead of A when linked: its start point minus A's
/// (A without a start point counts from 0; a video without one runs at
/// A's time).
pub fn offsets(starts: &[Option<f64>]) -> Vec<f64> {
    let base = starts.first().copied().flatten().unwrap_or(0.0);
    starts.iter().enumerate().map(|(i, s)| if i == 0 { 0.0 } else { s.map(|s| s - base).unwrap_or(0.0) }).collect()
}

/// Where video `i` should be when video `lead` is at `t`.
pub fn target(t: f64, lead: usize, i: usize, offs: &[f64]) -> f64 {
    let o = |k: usize| offs.get(k).copied().unwrap_or(0.0);
    t + o(i) - o(lead)
}

/// The offset in frames of a video running `secs` ahead at `fps`
/// (what the whole-file analysis and the Metrics tab take).
pub fn offset_frames(secs: f64, fps: f64) -> i64 {
    if !secs.is_finite() || !fps.is_finite() || fps <= 0.0 {
        return 0;
    }
    (secs * fps).round() as i64
}

// ---------------------------------------------------------------- A-B loop

/// The A-B loop (a section played over and over), in the lead video's time.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Loop {
    pub a: Option<f64>,
    pub b: Option<f64>,
}

impl Loop {
    /// The loop key: set A at `t`, then B at `t` (after A), then clear. A
    /// second press at or before A moves A instead.
    pub fn press(self, t: f64) -> Loop {
        match (self.a, self.b) {
            (None, _) => Loop { a: Some(t), b: None },
            (Some(a), None) if t > a + 1e-6 => Loop { a: Some(a), b: Some(t) },
            (Some(_), None) => Loop { a: Some(t), b: None },
            _ => Loop::default(),
        }
    }

    pub fn range(&self) -> Option<(f64, f64)> {
        match (self.a, self.b) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        }
    }

    /// Where Play starts: at A when `t` is outside the section (or at its very
    /// end, so it does not stop at once).
    pub fn play_from(&self, t: f64, frame_dur: f64) -> Option<f64> {
        let (a, b) = self.range()?;
        (t < a - 1e-4 || t >= b - frame_dur * 0.25).then_some(a)
    }

    /// While playing at `t`: true when the section is over and playback goes
    /// back to A. Frame B stays on screen for its own duration; `ended`
    /// (the videos ran out, e.g. B is the last frame) also wraps.
    pub fn wrap_due(&self, t: f64, frame_dur: f64, ended: bool) -> bool {
        match self.range() {
            Some((_, b)) => ended || t >= b + frame_dur * 0.5,
            None => false,
        }
    }
}

// ---------------------------------------------------------------- layout

/// Pane rectangles: 1 = full, 2 = halves, 3 = three columns, 4 = 2x2 grid.
pub fn pane_rects(n: usize, area: Rect, gap: f32) -> Vec<Rect> {
    let col = |cols: usize, i: usize, r: Rect| -> Rect {
        let w = ((r.width() - gap * (cols as f32 - 1.0)) / cols as f32).max(0.0);
        let x = r.min.x + i as f32 * (w + gap);
        Rect::from_min_max(Pos2::new(x, r.min.y), Pos2::new(x + w, r.max.y))
    };
    match n {
        0 => Vec::new(),
        1 => vec![area],
        2 | 3 => (0..n).map(|i| col(n, i, area)).collect(),
        _ => {
            let h = ((area.height() - gap) / 2.0).max(0.0);
            let top = Rect::from_min_max(area.min, Pos2::new(area.max.x, area.min.y + h));
            let bottom = Rect::from_min_max(Pos2::new(area.min.x, area.max.y - h), area.max);
            vec![col(2, 0, top), col(2, 1, top), col(2, 0, bottom), col(2, 1, bottom)]
        }
    }
}

/// Zoom factors stay finite and positive (a collapsed window gives 0 or NaN,
/// and `f32::clamp` panics on NaN bounds).
pub fn sane_zoom(z: f32) -> f32 {
    if z.is_finite() && z > 0.0 {
        z.clamp(0.001, 64.0)
    } else {
        1.0
    }
}

/// `v` limited to `lo..=hi` without panicking: an inverted range collapses
/// to `lo`, NaN bounds are ignored and a NaN value becomes `lo`.
pub fn safe_clamp(v: f32, lo: f32, hi: f32) -> f32 {
    let v = if v.is_nan() { lo } else { v };
    let hi = if hi.is_nan() { f32::INFINITY } else { hi.max(lo) };
    v.max(lo).min(hi)
}

/// The shared view of every pane, relative to each video's own size so
/// videos of any resolution and aspect show the same field of view: `zoom`
/// is a multiple of the pane's fit zoom, `center` the frame point (0..1)
/// at the pane's center.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub fit: bool,
    pub zoom: f32,
    pub center: Vec2,
}

impl Default for View {
    fn default() -> Self {
        View { fit: true, zoom: 1.0, center: Vec2::splat(0.5) }
    }
}

const MIN_REL: f32 = 0.05;
const MAX_REL: f32 = 256.0;

/// Physical pixels per frame pixel that fit `img` into `pane_px`; None
/// while the pane has no area (minimized, mid DPI change).
pub fn fit_zoom(img: (u32, u32), pane_px: Vec2) -> Option<f32> {
    if !(pane_px.x >= 1.0 && pane_px.y >= 1.0) {
        return None;
    }
    let z = (pane_px.x / img.0.max(1) as f32).min(pane_px.y / img.1.max(1) as f32);
    (z.is_finite() && z > 0.0).then_some(z)
}

impl View {
    /// (physical px per frame px, frame pixel at the pane center) for a
    /// video of `img` pixels in a pane of `pane_px`, kept inside the frame.
    pub fn effective(&self, img: (u32, u32), pane_px: Vec2) -> Option<(f32, Vec2)> {
        let fit = fit_zoom(img, pane_px)?;
        let s = Vec2::new(img.0.max(1) as f32, img.1.max(1) as f32);
        if self.fit {
            return Some((fit, s * 0.5));
        }
        let z = sane_zoom(fit * self.zoom);
        let c = Vec2::new(self.center.x * s.x, self.center.y * s.y);
        // A frame smaller than the pane stays centered; a larger one keeps its
        // edges at the pane's edges.
        let half = pane_px / z / 2.0;
        let cx = if s.x <= half.x * 2.0 { s.x / 2.0 } else { safe_clamp(c.x, half.x, s.x - half.x) };
        let cy = if s.y <= half.y * 2.0 { s.y / 2.0 } else { safe_clamp(c.y, half.y, s.y - half.y) };
        Some((z, Vec2::new(cx, cy)))
    }

    fn from_effective(img: (u32, u32), pane_px: Vec2, z: f32, c: Vec2) -> View {
        let Some(fit) = fit_zoom(img, pane_px) else { return View::default() };
        let s = Vec2::new(img.0.max(1) as f32, img.1.max(1) as f32);
        let rel = safe_clamp(z / fit, MIN_REL, MAX_REL);
        View { fit: false, zoom: rel, center: Vec2::new(safe_clamp(c.x / s.x, 0.0, 1.0), safe_clamp(c.y / s.y, 0.0, 1.0)) }
    }

    /// Zoom by `factor` keeping the frame point under `anchor` (physical px
    /// from the pane center) where it is.
    pub fn zoom_at(&self, img: (u32, u32), pane_px: Vec2, factor: f32, anchor: Vec2) -> View {
        let Some((z, c)) = self.effective(img, pane_px) else { return *self };
        if !factor.is_finite() || factor <= 0.0 {
            return *self;
        }
        let at = c + anchor / z;
        let fit = fit_zoom(img, pane_px).unwrap_or(z);
        let nz = safe_clamp(z * factor, fit * MIN_REL, fit * MAX_REL);
        let v = View::from_effective(img, pane_px, nz, at - anchor / nz);
        // Back to fit when zoomed out to it.
        if v.zoom <= 1.0 + 1e-3 && factor < 1.0 {
            return View::default();
        }
        v
    }

    /// Pan by `delta` physical pixels (positive: the picture moves right / down).
    pub fn pan_by(&self, img: (u32, u32), pane_px: Vec2, delta: Vec2) -> View {
        let Some((z, c)) = self.effective(img, pane_px) else { return *self };
        View::from_effective(img, pane_px, z, c - delta / z)
    }

    /// 100%: one frame pixel per screen pixel, keeping the center.
    pub fn actual(&self, img: (u32, u32), pane_px: Vec2) -> View {
        let Some((_, c)) = self.effective(img, pane_px) else { return *self };
        View::from_effective(img, pane_px, 1.0, c)
    }
}

/// Destination rectangle (points) of the whole frame in a pane.
pub fn dest_rect(pane: Rect, ppp: f32, img: (u32, u32), z: f32, c: Vec2) -> Rect {
    let ppp = if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 };
    let min = pane.center() - c * z / ppp;
    let min = Pos2::new((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
    Rect::from_min_size(min, Vec2::new(img.0.max(1) as f32, img.1.max(1) as f32) * z / ppp)
}

// ---------------------------------------------------------------- output size

/// Decode size for a frame of `native` pixels shown at `zoom` physical px
/// per frame px: never above native, in steps of 1/16, even, at least 2.
pub fn wanted_size(native: (u32, u32), zoom: f32) -> (u32, u32) {
    let q = ((sane_zoom(zoom).clamp(0.05, 1.0) * 16.0).ceil() / 16.0).min(1.0);
    let side = |n: u32| (((n as f32 * q) as u32) & !1).max(2);
    (side(native.0.max(2)), side(native.1.max(2)))
}

/// Whether to ask the decoder for `want` instead of `cur`: when sharper
/// frames are needed, or when far smaller ones would do (a slightly larger
/// frame than needed is kept, so small window changes do not re-decode).
pub fn needs_resize(cur: (u32, u32), want: (u32, u32)) -> bool {
    let grow = want.0 as f32 > cur.0 as f32 * 1.02 || want.1 as f32 > cur.1 as f32 * 1.02;
    let shrink = (want.0 as f32) < cur.0 as f32 * 0.6 && (want.1 as f32) < cur.1 as f32 * 0.6;
    grow || shrink
}

/// Lets a value through once it has stayed the same for a while (window
/// resizes and DPI changes settle before the decoders are reconfigured).
#[derive(Default)]
pub struct Debounce<T: PartialEq + Clone> {
    seen: Option<(T, Instant)>,
}

impl<T: PartialEq + Clone> Debounce<T> {
    /// Feed the current value; Some(value) once it was stable for `settle`.
    pub fn update(&mut self, v: T, now: Instant, settle: Duration) -> Option<T> {
        match &self.seen {
            Some((old, at)) if *old == v => {
                if now.duration_since(*at) >= settle {
                    self.seen = None;
                    return Some(v);
                }
                None
            }
            _ => {
                self.seen = Some((v, now));
                None
            }
        }
    }

    /// Forget a pending value (it no longer differs from what is in use).
    pub fn clear(&mut self) {
        self.seen = None;
    }

    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.seen.is_some()
    }
}

// ---------------------------------------------------------------- saved frames

/// A timestamp usable in a file name: `00-01-02.500` (hours, minutes,
/// seconds and milliseconds).
pub fn file_timecode(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}-{:02}-{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// A name without characters Windows does not allow in file names.
pub fn safe_stem(s: &str) -> String {
    let t: String = s.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }).collect();
    let t = t.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        "video".into()
    } else {
        t
    }
}

/// `<video name>_<timecode>.png` for each video; the same name twice (one
/// file opened twice) gets the pane letter added.
pub fn frame_file_names(items: &[(String, f64)]) -> Vec<String> {
    let base: Vec<String> = items.iter().map(|(stem, t)| format!("{}_{}", safe_stem(stem), file_timecode(*t))).collect();
    base.iter()
        .enumerate()
        .map(|(i, b)| {
            let dup = base.iter().filter(|o| o.eq_ignore_ascii_case(b)).count() > 1;
            if dup {
                format!("{b}_{}.png", LETTERS[i.min(MAX_VIDEOS - 1)])
            } else {
                format!("{b}.png")
            }
        })
        .collect()
}

/// The frames laid out like the panes (one row of up to three, or 2x2),
/// every frame scaled to the same height, on a dark background.
pub fn composite(frames: &[RgbaImage]) -> Option<RgbaImage> {
    const GAP: u32 = 8;
    let bg = image::Rgba([24, 24, 26, 255]);
    if frames.is_empty() {
        return None;
    }
    // The shortest frame's height, at most 2160 px.
    let h = frames.iter().map(|f| f.height().max(1)).min()?.min(2160);
    let scaled: Vec<RgbaImage> = frames
        .iter()
        .map(|f| {
            if f.height() == h {
                f.clone()
            } else {
                let w = ((f.width() as f64 * h as f64 / f.height().max(1) as f64).round() as u32).max(1);
                image::imageops::resize(f, w, h, image::imageops::FilterType::Triangle)
            }
        })
        .collect();
    let rows: Vec<&[RgbaImage]> = if scaled.len() == 4 { vec![&scaled[..2], &scaled[2..]] } else { vec![&scaled[..]] };
    let row_w = |r: &[RgbaImage]| r.iter().map(|f| f.width()).sum::<u32>() + GAP * (r.len() as u32).saturating_sub(1);
    let width = rows.iter().map(|r| row_w(r)).max()?;
    let height = h * rows.len() as u32 + GAP * (rows.len() as u32 - 1);
    let mut out = RgbaImage::from_pixel(width, height, bg);
    for (ri, r) in rows.iter().enumerate() {
        let mut x = (width - row_w(r)) / 2;
        let y = ri as u32 * (h + GAP);
        for f in r.iter() {
            image::imageops::overlay(&mut out, f, x as i64, y as i64);
            x += f.width() + GAP;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_points_give_offsets() {
        // B's event is 1.5 s later in its file than in A's: B runs 1.5 s ahead.
        let offs = offsets(&[Some(2.0), Some(3.5), None]);
        assert_eq!(offs, vec![0.0, 1.5, 0.0]);
        assert_eq!(target(10.0, 0, 1, &offs), 11.5);
        assert_eq!(target(10.0, 0, 2, &offs), 10.0);
        // A without a start point counts from its first frame.
        assert_eq!(offsets(&[None, Some(0.5)]), vec![0.0, 0.5]);
        assert_eq!(offsets(&[Some(1.0), Some(0.25)]), vec![0.0, -0.75]);
        // Led by B (its clock): A is 1.5 s behind.
        assert_eq!(target(11.5, 1, 0, &offs), 10.0);
        // Out of range indices count as no offset.
        assert_eq!(target(1.0, 0, 9, &offs), 1.0);
        // No start points: all play together.
        assert_eq!(offsets(&[None, None]), vec![0.0, 0.0]);
        assert_eq!(offset_frames(1.5, 30.0), 45);
        assert_eq!(offset_frames(-0.02, 25.0), -1);
        assert_eq!(offset_frames(f64::NAN, 25.0), 0);
        assert_eq!(offset_frames(1.0, 0.0), 0);
    }

    #[test]
    fn loop_presses() {
        let l = Loop::default().press(2.0);
        assert_eq!(l, Loop { a: Some(2.0), b: None });
        // B before A moves A.
        assert_eq!(l.press(1.0), Loop { a: Some(1.0), b: None });
        assert_eq!(l.press(2.0), Loop { a: Some(2.0), b: None });
        let l = l.press(4.0);
        assert_eq!(l.range(), Some((2.0, 4.0)));
        // Third press clears.
        assert_eq!(l.press(3.0), Loop::default());
    }

    #[test]
    fn loop_wraps_and_play_starts_at_a() {
        let fd = 0.04;
        let l = Loop { a: Some(2.0), b: Some(4.0) };
        assert!(!l.wrap_due(3.0, fd, false));
        // Frame B is shown for its duration before wrapping.
        assert!(!l.wrap_due(4.0, fd, false));
        assert!(l.wrap_due(4.02, fd, false));
        // B is the last frame: the end of the videos wraps.
        assert!(l.wrap_due(3.99, fd, true));
        assert!(!Loop::default().wrap_due(100.0, fd, true));
        assert!(!Loop { a: Some(1.0), b: None }.wrap_due(100.0, fd, false));
        // Play from outside starts at A, from inside where it is.
        assert_eq!(l.play_from(0.5, fd), Some(2.0));
        assert_eq!(l.play_from(5.0, fd), Some(2.0));
        assert_eq!(l.play_from(4.0, fd), Some(2.0));
        assert_eq!(l.play_from(3.0, fd), None);
        assert_eq!(l.play_from(2.0, fd), None);
        assert_eq!(Loop { a: Some(1.0), b: None }.play_from(0.0, fd), None);
    }

    #[test]
    fn panes() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0));
        assert!(pane_rects(0, area, 2.0).is_empty());
        assert_eq!(pane_rects(1, area, 2.0), vec![area]);
        let two = pane_rects(2, area, 2.0);
        assert_eq!(two[0].width(), 499.0);
        assert_eq!(two[1].min.x, 501.0);
        let three = pane_rects(3, area, 2.0);
        assert!((three[2].max.x - 1000.0).abs() < 1e-3);
        let four = pane_rects(4, area, 2.0);
        assert_eq!(four.len(), 4);
        assert_eq!(four[3].max, area.max);
        assert_eq!(four[0].height(), 299.0);
        // A collapsed area gives empty panes, never negative ones.
        let none = Rect::from_min_size(Pos2::ZERO, Vec2::ZERO);
        for n in 1..=4 {
            for r in pane_rects(n, none, 2.0) {
                assert!(r.width() >= 0.0 && r.height() >= 0.0, "{r:?}");
            }
        }
    }

    #[test]
    fn view_matches_field_of_view() {
        // A 1920x1080 and a 1280x720 video in equal panes: fit shows each whole.
        let pane = Vec2::new(960.0, 1080.0);
        let v = View::default();
        let (za, ca) = v.effective((1920, 1080), pane).unwrap();
        let (zb, cb) = v.effective((1280, 720), pane).unwrap();
        assert!((za - 0.5).abs() < 1e-6 && (zb - 0.75).abs() < 1e-6);
        assert_eq!((ca, cb), (Vec2::new(960.0, 540.0), Vec2::new(640.0, 360.0)));
        // Zoom 4x at the center: both show the same relative region.
        let z = v.zoom_at((1920, 1080), pane, 4.0, Vec2::ZERO);
        let (za, ca) = z.effective((1920, 1080), pane).unwrap();
        let (zb, cb) = z.effective((1280, 720), pane).unwrap();
        assert!((za / 0.5 - 4.0).abs() < 1e-4 && (zb / 0.75 - 4.0).abs() < 1e-4);
        assert!((ca.x / 1920.0 - cb.x / 1280.0).abs() < 1e-4);
        // A portrait video (other aspect) keeps its own fit, not stretched.
        let (zp, _) = v.effective((1080, 1920), pane).unwrap();
        assert!((zp - 0.5625).abs() < 1e-6);
        // Pan right moves the center left, clamped inside the frame.
        let p = z.pan_by((1920, 1080), pane, Vec2::new(-100_000.0, 0.0));
        let (_, c) = p.effective((1920, 1080), pane).unwrap();
        assert!(c.x <= 1920.0 && c.x > 960.0);
        // Zooming back out below fit returns to fit.
        assert!(z.zoom_at((1920, 1080), pane, 0.01, Vec2::ZERO).fit);
        // 100%.
        let a = v.actual((1920, 1080), pane);
        assert!((a.effective((1920, 1080), pane).unwrap().0 - 1.0).abs() < 1e-4);
    }

    #[test]
    fn view_survives_collapsed_panes() {
        // Minimized / mid DPI change: zero, negative or NaN pane sizes.
        for pane in [Vec2::ZERO, Vec2::new(0.0, 500.0), Vec2::new(-5.0, -5.0), Vec2::new(f32::NAN, 10.0), Vec2::new(0.4, 0.4)] {
            assert!(fit_zoom((1920, 1080), pane).is_none());
            let v = View { fit: false, zoom: 3.0, center: Vec2::new(0.3, 0.7) };
            assert!(v.effective((1920, 1080), pane).is_none());
            assert_eq!(v.zoom_at((1920, 1080), pane, 2.0, Vec2::new(5.0, 5.0)), v);
            assert_eq!(v.pan_by((1920, 1080), pane, Vec2::new(5.0, 5.0)), v);
        }
        // Zero sized frames and NaN factors do not panic.
        let pane = Vec2::new(800.0, 600.0);
        let v = View::default();
        assert!(v.effective((0, 0), pane).is_some());
        assert_eq!(v.zoom_at((1920, 1080), pane, f32::NAN, Vec2::ZERO), v);
        let odd = View { fit: false, zoom: f32::NAN, center: Vec2::new(f32::NAN, 2.0) };
        let (z, c) = odd.effective((1920, 1080), pane).unwrap();
        assert!(z.is_finite() && c.x.is_finite() && c.y.is_finite());
        assert_eq!(sane_zoom(0.0), 1.0);
        assert_eq!(sane_zoom(f32::INFINITY), 1.0);
        assert_eq!(safe_clamp(5.0, 10.0, 0.0), 10.0);
        assert_eq!(safe_clamp(f32::NAN, 0.0, 1.0), 0.0);
        assert_eq!(safe_clamp(0.5, 0.0, f32::NAN), 0.5);
        let r = dest_rect(Rect::from_min_size(Pos2::ZERO, pane), 0.0, (100, 100), 1.0, Vec2::splat(50.0));
        assert!(r.min.x.is_finite());
    }

    #[test]
    fn output_sizes() {
        assert_eq!(wanted_size((1920, 1080), 1.0), (1920, 1080));
        assert_eq!(wanted_size((1920, 1080), 2.0), (1920, 1080));
        assert_eq!(wanted_size((1920, 1080), 0.5), (960, 540));
        assert_eq!(wanted_size((1920, 1080), f32::NAN), (1920, 1080));
        assert_eq!(wanted_size((3, 3), 0.01), (2, 2));
        // Small changes keep the frames; bigger or much smaller ones change them.
        assert!(!needs_resize((960, 540), (960, 540)));
        assert!(!needs_resize((1080, 608), (960, 540)));
        assert!(needs_resize((960, 540), (1080, 608)));
        assert!(needs_resize((1920, 1080), (480, 270)));
    }

    #[test]
    fn debounce_waits_for_a_stable_value() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let settle = Duration::from_millis(250);
        let mut d = Debounce::default();
        assert_eq!(d.update(1, ms(0), settle), None);
        assert_eq!(d.update(2, ms(100), settle), None);
        assert_eq!(d.update(2, ms(300), settle), None);
        assert_eq!(d.update(2, ms(351), settle), Some(2));
        assert!(!d.pending());
        assert_eq!(d.update(3, ms(400), settle), None);
        d.clear();
        assert_eq!(d.update(3, ms(700), settle), None);
    }

    #[test]
    fn frame_names() {
        assert_eq!(file_timecode(62.5), "00-01-02.500");
        assert_eq!(file_timecode(3723.0004), "01-02-03.000");
        assert_eq!(file_timecode(-1.0), "00-00-00.000");
        let names = frame_file_names(&[("clip".into(), 1.0), ("other".into(), 1.0), ("clip".into(), 1.0), ("a:b?".into(), 0.0)]);
        assert_eq!(names, vec!["clip_00-00-01.000_A.png", "other_00-00-01.000.png", "clip_00-00-01.000_C.png", "a_b__00-00-00.000.png"]);
        assert_eq!(safe_stem("  . "), "video");
    }

    #[test]
    fn composite_layout() {
        let img = |w, h| RgbaImage::from_pixel(w, h, image::Rgba([200, 0, 0, 255]));
        assert!(composite(&[]).is_none());
        let one = composite(&[img(64, 32)]).unwrap();
        assert_eq!(one.dimensions(), (64, 32));
        // Different heights: scaled to the shortest, side by side.
        let two = composite(&[img(64, 32), img(32, 32)]).unwrap();
        assert_eq!(two.dimensions(), (64 + 8 + 32, 32));
        let two = composite(&[img(128, 64), img(64, 32)]).unwrap();
        assert_eq!(two.dimensions(), (64 + 8 + 64, 32));
        // Four: 2x2.
        let four = composite(&[img(40, 20), img(40, 20), img(40, 20), img(40, 20)]).unwrap();
        assert_eq!(four.dimensions(), (88, 48));
        assert_eq!(four.get_pixel(0, 0).0, [200, 0, 0, 255]);
        assert_eq!(four.get_pixel(42, 0).0, [24, 24, 26, 255]);
    }
}
