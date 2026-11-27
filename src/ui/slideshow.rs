//! Full-window slideshow: GPU transitions, optional Ken Burns motion, captions,
//! a progress indicator and an auto-hiding control bar.
//!
//! Keys (shown in the on-screen help, `H`):
//!   Space / P / click  pause or resume (FastStone convention: Space pauses)
//!   → / PgDn / wheel ↓ next      ← / PgUp / wheel ↑ previous
//!   Home / End         first / last
//!   + / -              longer / shorter interval
//!   C caption, K Ken Burns, S shuffle, L loop, M sound of videos, H help, Esc exit.
//!
//! Videos play (muted unless "play sound" is on) and the show moves on when
//! one ends, or after the longest time set for videos. The next video's
//! decoder opens while the slide before it shows, and its slide only comes
//! once its first frame is there, so a change never waits on a decoder.
//!
//! Rendering never builds new textures during a transition: the next slide is
//! decoded in the background and its GPU tiles are uploaded ("pre-warmed")
//! while the current slide is on screen. Transitions are pure functions of the
//! eased progress (`transition_frame`) that describe up to two layers
//! (offset / scale / alpha / clip); the renderer turns those into tinted,
//! transformed and clipped `GpuImage::paint` calls.

use super::icons as I;
use super::widgets;
use crate::app::Core;
use crate::loader::LoadState;
use crate::render::GpuImage;
use crate::settings::FitMode;
use crate::video::inline::{InlinePlayer, SoundOut};
use crate::video::playback;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- transitions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transition {
    None,
    Crossfade,
    FadeBlack,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    PushLeft,
    PushRight,
    PushUp,
    PushDown,
    ZoomIn,
    ZoomOut,
    WipeLeft,
    WipeRight,
    WipeUp,
    WipeDown,
    Iris,
    Blinds,
    BlindsVertical,
    Random,
}

impl Transition {
    pub const ALL: [Transition; 21] = [
        Transition::Crossfade,
        Transition::FadeBlack,
        Transition::SlideLeft,
        Transition::SlideRight,
        Transition::SlideUp,
        Transition::SlideDown,
        Transition::PushLeft,
        Transition::PushRight,
        Transition::PushUp,
        Transition::PushDown,
        Transition::ZoomIn,
        Transition::ZoomOut,
        Transition::WipeLeft,
        Transition::WipeRight,
        Transition::WipeUp,
        Transition::WipeDown,
        Transition::Iris,
        Transition::Blinds,
        Transition::BlindsVertical,
        Transition::Random,
        Transition::None,
    ];

    /// Transitions `Random` picks from.
    pub const CONCRETE: [Transition; 19] = [
        Transition::Crossfade,
        Transition::FadeBlack,
        Transition::SlideLeft,
        Transition::SlideRight,
        Transition::SlideUp,
        Transition::SlideDown,
        Transition::PushLeft,
        Transition::PushRight,
        Transition::PushUp,
        Transition::PushDown,
        Transition::ZoomIn,
        Transition::ZoomOut,
        Transition::WipeLeft,
        Transition::WipeRight,
        Transition::WipeUp,
        Transition::WipeDown,
        Transition::Iris,
        Transition::Blinds,
        Transition::BlindsVertical,
    ];

    /// Stable name stored in the settings file.
    pub fn name(self) -> &'static str {
        match self {
            Transition::None => "None",
            Transition::Crossfade => "Crossfade",
            Transition::FadeBlack => "FadeBlack",
            Transition::SlideLeft => "SlideLeft",
            Transition::SlideRight => "SlideRight",
            Transition::SlideUp => "SlideUp",
            Transition::SlideDown => "SlideDown",
            Transition::PushLeft => "PushLeft",
            Transition::PushRight => "PushRight",
            Transition::PushUp => "PushUp",
            Transition::PushDown => "PushDown",
            Transition::ZoomIn => "ZoomIn",
            Transition::ZoomOut => "ZoomOut",
            Transition::WipeLeft => "WipeLeft",
            Transition::WipeRight => "WipeRight",
            Transition::WipeUp => "WipeUp",
            Transition::WipeDown => "WipeDown",
            Transition::Iris => "Iris",
            Transition::Blinds => "Blinds",
            Transition::BlindsVertical => "BlindsVertical",
            Transition::Random => "Random",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Transition::None => "None (cut)",
            Transition::Crossfade => "Crossfade",
            Transition::FadeBlack => "Fade through black",
            Transition::SlideLeft => "Slide in from right",
            Transition::SlideRight => "Slide in from left",
            Transition::SlideUp => "Slide in from bottom",
            Transition::SlideDown => "Slide in from top",
            Transition::PushLeft => "Push left",
            Transition::PushRight => "Push right",
            Transition::PushUp => "Push up",
            Transition::PushDown => "Push down",
            Transition::ZoomIn => "Zoom in",
            Transition::ZoomOut => "Zoom out",
            Transition::WipeLeft => "Wipe right → left",
            Transition::WipeRight => "Wipe left → right",
            Transition::WipeUp => "Wipe bottom → top",
            Transition::WipeDown => "Wipe top → bottom",
            Transition::Iris => "Iris (circle reveal)",
            Transition::Blinds => "Blinds (horizontal)",
            Transition::BlindsVertical => "Blinds (vertical)",
            Transition::Random => "Random",
        }
    }

    /// Parse a settings value. Accepts loose spellings ("Slide", "fade", ...).
    pub fn from_name(s: &str) -> Transition {
        let k: String = s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_ascii_lowercase();
        for t in Transition::ALL {
            if t.name().to_ascii_lowercase() == k {
                return t;
            }
        }
        match k.as_str() {
            "fade" | "fadethroughblack" | "fadetoblack" => Transition::FadeBlack,
            "slide" => Transition::SlideLeft,
            "push" => Transition::PushLeft,
            "zoom" => Transition::ZoomIn,
            "wipe" => Transition::WipeRight,
            "circle" | "irisreveal" => Transition::Iris,
            "cut" | "off" => Transition::None,
            _ => Transition::Crossfade,
        }
    }
}

/// Ease-in-out cubic on [0, 1].
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Linear progress of a timed effect, clamped to [0, 1]. A zero duration is complete.
pub fn progress(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        1.0
    } else {
        (elapsed / duration).clamp(0.0, 1.0)
    }
}

/// Where a layer is visible, in normalized view coordinates (0..1).
#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    Full,
    Rect(Rect),
    Strips(Vec<Rect>),
    /// Circle centred at `center`; `radius` 1.0 reaches the view corners.
    Circle { center: Pos2, radius: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Translation as a fraction of the view size.
    pub offset: Vec2,
    /// Scale about the view centre.
    pub scale: f32,
    pub alpha: f32,
    pub clip: Clip,
    /// Draw fully opaque where the incoming layer covers it (avoids the
    /// mid-crossfade brightness dip); `alpha` applies elsewhere.
    pub hold_under_next: bool,
}

impl Layer {
    pub fn full() -> Layer {
        Layer { offset: Vec2::ZERO, scale: 1.0, alpha: 1.0, clip: Clip::Full, hold_under_next: false }
    }
    fn alpha(a: f32) -> Layer {
        Layer { alpha: a.clamp(0.0, 1.0), ..Layer::full() }
    }
}

/// The outgoing (`from`, drawn first) and incoming (`to`, drawn on top) layers.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionFrame {
    pub from: Layer,
    pub to: Layer,
}

const BLINDS: usize = 10;

/// Describe transition `kind` at eased progress `t` (0 = only the old image, 1 = only the new one).
pub fn transition_frame(kind: Transition, t: f32) -> TransitionFrame {
    let t = t.clamp(0.0, 1.0);
    let s = 1.0 - t;
    let full = Layer::full;
    match kind {
        Transition::None | Transition::Random => TransitionFrame { from: Layer::alpha(0.0), to: full() },
        Transition::Crossfade => TransitionFrame { from: Layer { hold_under_next: true, ..Layer::alpha(s) }, to: Layer::alpha(t) },
        Transition::FadeBlack => TransitionFrame { from: Layer::alpha(1.0 - 2.0 * t), to: Layer::alpha(2.0 * t - 1.0) },
        Transition::SlideLeft => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(s, 0.0), ..full() } },
        Transition::SlideRight => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(-s, 0.0), ..full() } },
        Transition::SlideUp => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(0.0, s), ..full() } },
        Transition::SlideDown => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(0.0, -s), ..full() } },
        Transition::PushLeft => TransitionFrame {
            from: Layer { offset: Vec2::new(-t, 0.0), ..full() },
            to: Layer { offset: Vec2::new(s, 0.0), ..full() },
        },
        Transition::PushRight => TransitionFrame {
            from: Layer { offset: Vec2::new(t, 0.0), ..full() },
            to: Layer { offset: Vec2::new(-s, 0.0), ..full() },
        },
        Transition::PushUp => TransitionFrame {
            from: Layer { offset: Vec2::new(0.0, -t), ..full() },
            to: Layer { offset: Vec2::new(0.0, s), ..full() },
        },
        Transition::PushDown => TransitionFrame {
            from: Layer { offset: Vec2::new(0.0, t), ..full() },
            to: Layer { offset: Vec2::new(0.0, -s), ..full() },
        },
        Transition::ZoomIn => TransitionFrame {
            from: Layer { scale: 1.0 + 0.5 * t, hold_under_next: true, ..Layer::alpha(s) },
            to: Layer { scale: 0.7 + 0.3 * t, ..Layer::alpha(t) },
        },
        Transition::ZoomOut => TransitionFrame {
            from: Layer { scale: 1.0 - 0.3 * t, ..Layer::alpha(s) },
            to: Layer { scale: 1.4 - 0.4 * t, ..Layer::alpha(t) },
        },
        Transition::WipeLeft => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(s, 0.0, 1.0, 1.0)), ..full() } },
        Transition::WipeRight => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, 0.0, t, 1.0)), ..full() } },
        Transition::WipeUp => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, s, 1.0, 1.0)), ..full() } },
        Transition::WipeDown => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, 0.0, 1.0, t)), ..full() } },
        Transition::Iris => TransitionFrame { from: full(), to: Layer { clip: Clip::Circle { center: Pos2::new(0.5, 0.5), radius: t }, ..full() } },
        Transition::Blinds | Transition::BlindsVertical => {
            let step = 1.0 / BLINDS as f32;
            let strips = (0..BLINDS)
                .map(|k| {
                    let a = k as f32 * step;
                    if kind == Transition::Blinds {
                        nrect(0.0, a, 1.0, a + step * t)
                    } else {
                        nrect(a, 0.0, a + step * t, 1.0)
                    }
                })
                .collect();
            TransitionFrame { from: full(), to: Layer { clip: Clip::Strips(strips), ..full() } }
        }
    }
}

fn nrect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
}

fn denorm(r: Rect, view: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(view.min.x + r.min.x * view.width(), view.min.y + r.min.y * view.height()),
        Pos2::new(view.min.x + r.max.x * view.width(), view.min.y + r.max.y * view.height()),
    )
}

/// Screen-space clip rectangles for `clip` inside `view`. Circles become a
/// stack of horizontal strips (at most ~160, so GPU tiles stay resident).
pub fn clip_rects(clip: &Clip, view: Rect) -> Vec<Rect> {
    match clip {
        Clip::Full => vec![view],
        Clip::Rect(r) => vec![denorm(*r, view)],
        Clip::Strips(v) => v.iter().map(|r| denorm(*r, view)).filter(|r| r.width() > 0.0 && r.height() > 0.0).collect(),
        Clip::Circle { center, radius } => {
            let c = Pos2::new(view.min.x + center.x * view.width(), view.min.y + center.y * view.height());
            let r = radius * 0.5 * view.size().length();
            if r <= 0.0 {
                return Vec::new();
            }
            if r >= 0.5 * view.size().length() - 0.5 {
                return vec![view];
            }
            let step = (view.height() / 160.0).max(2.0);
            let y0 = (c.y - r).max(view.min.y);
            let y1 = (c.y + r).min(view.max.y);
            let mut out = Vec::new();
            let mut y = y0;
            while y < y1 {
                let yb = (y + step).min(y1);
                // Widest chord within the strip (nearest row to the centre).
                let dy = if c.y >= y && c.y <= yb { 0.0 } else { (c.y - y).abs().min((c.y - yb).abs()) };
                let hw = (r * r - dy * dy).max(0.0).sqrt();
                if hw > 0.0 {
                    out.push(Rect::from_min_max(Pos2::new(c.x - hw, y), Pos2::new(c.x + hw, yb)).intersect(view));
                }
                y = yb;
            }
            out
        }
    }
}

/// Up to four rectangles covering `outer` minus `hole`.
pub fn rect_minus(outer: Rect, hole: Rect) -> Vec<Rect> {
    let h = hole.intersect(outer);
    if h.width() <= 0.0 || h.height() <= 0.0 {
        return vec![outer];
    }
    let mut v = Vec::new();
    let push = |v: &mut Vec<Rect>, r: Rect| {
        if r.width() > 0.0 && r.height() > 0.0 {
            v.push(r);
        }
    };
    push(&mut v, Rect::from_min_max(outer.min, Pos2::new(outer.max.x, h.min.y)));
    push(&mut v, Rect::from_min_max(Pos2::new(outer.min.x, h.max.y), outer.max));
    push(&mut v, Rect::from_min_max(Pos2::new(outer.min.x, h.min.y), Pos2::new(h.min.x, h.max.y)));
    push(&mut v, Rect::from_min_max(Pos2::new(h.max.x, h.min.y), Pos2::new(outer.max.x, h.max.y)));
    v
}

// ---------------------------------------------------------------- Ken Burns / randomness

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

pub fn seed_of(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// Slow pan/zoom of one slide, deterministic per seed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KenBurns {
    pub s0: f32,
    pub s1: f32,
    /// Pan positions in [-1, 1] (fraction of the available overflow).
    pub p0: Vec2,
    pub p1: Vec2,
}

impl KenBurns {
    pub fn from_seed(seed: u64) -> KenBurns {
        let r: Vec<f32> = (0..6).map(|i| unit(splitmix(seed ^ (i as u64).wrapping_mul(0xA24B_AED4_963E_E407)))).collect();
        let lo = 1.02 + 0.04 * r[0];
        let hi = 1.14 + 0.08 * r[1];
        let (s0, s1) = if r[2] < 0.5 { (lo, hi) } else { (hi, lo) };
        let p0 = Vec2::new(r[3] * 2.0 - 1.0, r[4] * 2.0 - 1.0);
        // Head roughly to the opposite side so there is visible motion.
        let p1 = Vec2::new(-p0.x * (0.4 + 0.6 * r[5]), -p0.y * (0.4 + 0.6 * (1.0 - r[5])));
        KenBurns { s0, s1, p0, p1 }
    }

    /// (scale, pan) at linear time `u` in [0, 1].
    pub fn at(&self, u: f32) -> (f32, Vec2) {
        let u = u.clamp(0.0, 1.0);
        // Gentle ease so motion starts/stops softly.
        let e = u * u * (3.0 - 2.0 * u) * 0.5 + u * 0.5;
        (self.s0 + (self.s1 - self.s0) * e, self.p0 + (self.p1 - self.p0) * e)
    }

    /// Apply to a fitted destination rect: grows it by `scale` and pans within
    /// the overflow so the original fitted area always stays covered.
    pub fn apply(fit: Rect, scale: f32, pan: Vec2) -> Rect {
        let size = fit.size() * scale.max(1.0);
        let room = (size - fit.size()) * 0.5;
        let c = fit.center() + Vec2::new(pan.x.clamp(-1.0, 1.0) * room.x, pan.y.clamp(-1.0, 1.0) * room.y);
        Rect::from_center_size(c, size)
    }
}

/// Zoom (physical px per image px) for showing `img` in a view of `view_px` physical pixels.
pub fn fit_zoom(mode: FitMode, img: (u32, u32), view_px: Vec2) -> f32 {
    let (w, h) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let fit = (view_px.x / w).min(view_px.y / h);
    match mode {
        FitMode::FitLarge => fit.min(1.0),
        FitMode::FitAlways => fit,
        FitMode::Fill => (view_px.x / w).max(view_px.y / h),
        FitMode::FitWidth => view_px.x / w,
        FitMode::FitHeight => view_px.y / h,
        FitMode::Actual => 1.0,
    }
}

// ---------------------------------------------------------------- order

/// Next position in a show of `n` slides (None at the end when not looping).
pub fn step_pos(pos: usize, n: usize, delta: isize, looping: bool) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let p = pos as isize + delta;
    if p >= 0 && (p as usize) < n {
        Some(p as usize)
    } else if looping {
        Some(p.rem_euclid(n as isize) as usize)
    } else {
        None
    }
}

/// A play order over `n` items starting with `first`; shuffled when `random`.
pub fn make_order(n: usize, first: usize, random: bool, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    if random && n > 1 {
        let mut s = seed;
        for i in (1..n).rev() {
            s = splitmix(s);
            let j = (s % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
        if let Some(k) = v.iter().position(|&x| x == first) {
            v.swap(0, k);
        }
    }
    v
}

const INTERVALS: [f32; 15] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 15.0, 20.0, 30.0, 45.0, 60.0, 120.0];
const DURATIONS: [f32; 7] = [0.3, 0.5, 0.8, 1.0, 1.5, 2.0, 3.0];

pub fn next_interval(cur: f32, up: bool) -> f32 {
    if up {
        INTERVALS.iter().copied().find(|&v| v > cur + 0.01).unwrap_or(INTERVALS[INTERVALS.len() - 1])
    } else {
        INTERVALS.iter().rev().copied().find(|&v| v < cur - 0.01).unwrap_or(INTERVALS[0])
    }
}

fn fmt_secs(v: f32) -> String {
    if (v - v.round()).abs() < 0.01 {
        format!("{:.0} s", v)
    } else {
        format!("{:.1} s", v)
    }
}

// ------------------------------------------------------------ subfolders

/// Limits of the "include subfolders" walk: it runs off the UI thread, but a
/// slideshow started at the top of a huge tree (a whole drive, a NAS share)
/// must still not search for minutes.
const WALK_MAX_IMAGES: usize = 50_000;
const WALK_MAX_FOLDERS: usize = 20_000;
const WALK_MAX_TIME: Duration = Duration::from_secs(20);

/// Image and video files under `dir`, its subfolders included. Stops (the bool is
/// true) after `max_images` images or `max_folders` folders, at `deadline`
/// or when `cancel` is set.
fn walk_images(dir: &Path, max_images: usize, max_folders: usize, deadline: Instant, cancel: &AtomicBool) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut folders = 0;
    while let Some(d) = stack.pop() {
        if folders >= max_folders || Instant::now() >= deadline || cancel.load(Ordering::Relaxed) {
            return (out, true);
        }
        folders += 1;
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for item in rd.flatten() {
            let Ok(ft) = item.file_type() else { continue };
            let path = item.path();
            if ft.is_dir() {
                stack.push(path);
            } else if crate::formats::is_image_path(&path) || crate::video::is_video_path(&path) {
                out.push(path);
                if out.len() >= max_images {
                    return (out, true);
                }
            }
        }
    }
    (out, false)
}

/// `base` (the folder's pictures in the browser's order) followed by the
/// pictures of its subfolders from `all`, by folder and then by name. None
/// when `base` is a hand-picked selection (fewer pictures than the folder
/// holds) or the subfolders add nothing.
fn merge_subfolders(base: &[PathBuf], mut all: Vec<PathBuf>, dir: &Path) -> Option<Vec<PathBuf>> {
    let direct = all.iter().filter(|p| p.parent() == Some(dir)).count();
    if base.len() < direct || all.len() <= base.len() {
        return None;
    }
    all.sort_by(|a, b| {
        let da = a.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
        let db = b.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
        (a.parent() != Some(dir))
            .cmp(&(b.parent() != Some(dir)))
            .then_with(|| crate::catalog::natural_cmp(&da, &db))
            .then_with(|| crate::catalog::natural_cmp(&a.file_name().unwrap_or_default().to_string_lossy(), &b.file_name().unwrap_or_default().to_string_lossy()))
    });
    let mut merged = base.to_vec();
    merged.extend(all.into_iter().filter(|p| p.parent() != Some(dir)));
    Some(merged)
}

/// What the background walk found.
struct FolderWalk {
    dir: PathBuf,
    images: Vec<PathBuf>,
    /// A limit stopped the walk before it saw every subfolder.
    truncated: bool,
}

/// The subfolder walk in progress ("Finding images..." on screen).
struct SubfolderScan {
    rx: crossbeam_channel::Receiver<FolderWalk>,
    cancel: Arc<AtomicBool>,
    /// Say how many pictures the slideshow has once the walk lands (the user
    /// just turned the option on).
    announce: bool,
}

impl Drop for SubfolderScan {
    /// Leaving the slideshow (or turning the option off) stops the walk.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- slideshow

/// What a slide shows: a decoded picture on the GPU, or a video that plays.
enum Pic {
    Image(GpuImage),
    Video(InlinePlayer),
}

struct Slide {
    path: PathBuf,
    pic: Pic,
    /// Seconds since this slide started appearing (drives Ken Burns).
    clock: f32,
    kb: KenBurns,
    /// All tiles for its display zoom are on the GPU.
    warm: bool,
}

impl Slide {
    /// The picture's size in pixels.
    fn dims(&self) -> (u32, u32) {
        match &self.pic {
            Pic::Image(g) => (g.width, g.height),
            Pic::Video(v) => v.display_size().unwrap_or((16, 9)),
        }
    }

    fn video(&self) -> Option<&InlinePlayer> {
        match &self.pic {
            Pic::Video(v) => Some(v),
            Pic::Image(_) => None,
        }
    }

    fn video_mut(&mut self) -> Option<&mut InlinePlayer> {
        match &mut self.pic {
            Pic::Video(v) => Some(v),
            Pic::Image(_) => None,
        }
    }

    /// A picture is there to show (a video has decoded its first frame).
    fn showable(&self) -> bool {
        self.video().is_none_or(|v| v.has_frame())
    }

    /// Paint into `dest` with `painter` (clipped to `clip`); `zoom` in
    /// physical pixels per picture pixel. True while image tiles are still
    /// on their way to the GPU.
    fn paint(&mut self, ctx: &egui::Context, painter: &egui::Painter, dest: Rect, clip: Rect, zoom: f32, tint: Color32) -> bool {
        match &mut self.pic {
            Pic::Image(g) => g.paint(ctx, painter, dest, clip, zoom, false, tint),
            Pic::Video(v) => {
                v.fit_decode(zoom, ctx);
                v.paint(ctx, painter, dest, false, tint);
                false
            }
        }
    }
}

struct Active {
    kind: Transition,
    elapsed: f32,
    duration: f32,
}

pub struct Slideshow {
    pub list: Vec<PathBuf>,
    pub index: usize,
    base_list: Vec<PathBuf>,
    order: Vec<usize>,
    pos: usize,
    current: Option<Slide>,
    outgoing: Option<Slide>,
    active: Option<Active>,
    /// Prepared (decoded + GPU) slides waiting to be shown.
    ready: Vec<Slide>,
    /// Target position we are waiting on, and the direction to skip failures.
    pending: Option<(usize, isize)>,
    failed: HashSet<PathBuf>,
    all_failed: bool,
    display: f32,
    paused: bool,
    ended: bool,
    last_tick: Instant,
    last_activity: Instant,
    wheel_acc: f32,
    help: bool,
    flash: Option<(&'static str, Instant)>,
    captions: HashMap<PathBuf, Option<String>>,
    /// EXIF dates read off the UI thread.
    caption_rx: crossbeam_channel::Receiver<(PathBuf, Option<String>)>,
    caption_tx: crossbeam_channel::Sender<(PathBuf, Option<String>)>,
    random_count: u64,
    popup_was_open: bool,
    pending_since: Instant,
    exit_requested: bool,
    /// Searching the subfolders in the background ("include subfolders").
    scan: Option<SubfolderScan>,
    /// The sound output for videos (only while "play sound" is on).
    sound: Option<SoundOut>,
}

impl Slideshow {
    pub fn new(list: Vec<PathBuf>, start: usize, core: &Core) -> Self {
        let start = start.min(list.len().saturating_sub(1));
        let base_list = list.clone();
        let start_path = list.get(start).cloned();
        let (cap_tx, cap_rx) = crossbeam_channel::unbounded();
        let mut s = Slideshow {
            list,
            index: start,
            base_list,
            order: Vec::new(),
            pos: 0,
            current: None,
            outgoing: None,
            active: None,
            ready: Vec::new(),
            pending: None,
            failed: HashSet::new(),
            all_failed: false,
            display: 0.0,
            paused: false,
            ended: false,
            last_tick: Instant::now(),
            last_activity: Instant::now(),
            wheel_acc: 0.0,
            help: false,
            flash: None,
            captions: HashMap::new(),
            caption_rx: cap_rx,
            caption_tx: cap_tx,
            random_count: 0,
            popup_was_open: false,
            pending_since: Instant::now(),
            exit_requested: false,
            scan: None,
            sound: None,
        };
        if core.settings.slideshow.include_subfolders {
            // The folder's own pictures play while the subfolders are searched.
            s.rebuild_list(true, start_path.as_deref(), &core.ctx, false, false);
        }
        s.reorder(core.settings.slideshow.random_order);
        s.pending = if s.order.is_empty() { None } else { Some((s.pos, 1)) };
        s
    }

    /// Apply "include subfolders": off goes back to the pictures the
    /// slideshow started with; on keeps showing them while the subfolders
    /// are searched on a background thread (`poll_scan` adds what it finds).
    fn rebuild_list(&mut self, include_sub: bool, keep: Option<&Path>, ctx: &egui::Context, random: bool, announce: bool) {
        // A walk still running stops (see `SubfolderScan`'s Drop).
        self.scan = None;
        if !include_sub {
            self.replace_list(self.base_list.clone(), random);
            return;
        }
        let Some(dir) = keep.or(self.base_list.first().map(|p| p.as_path())).and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop, ctx) = (cancel.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("lumen-slideshow-scan".into()).spawn(move || {
            let (images, truncated) = walk_images(&dir, WALK_MAX_IMAGES, WALK_MAX_FOLDERS, Instant::now() + WALK_MAX_TIME, &stop);
            if !stop.load(Ordering::Relaxed) {
                let _ = tx.send(FolderWalk { dir, images, truncated });
                ctx.request_repaint();
            }
        });
        if spawned.is_ok() {
            self.scan = Some(SubfolderScan { rx, cancel, announce });
        }
    }

    /// Take the result of the subfolder walk once it is there.
    fn poll_scan(&mut self, core: &mut Core) {
        let Some(scan) = &self.scan else { return };
        let announce = scan.announce;
        let walk = match scan.rx.try_recv() {
            Ok(w) => w,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.scan = None;
                return;
            }
        };
        self.scan = None;
        if let Some(list) = merge_subfolders(&self.base_list, walk.images, &walk.dir) {
            self.replace_list(list, core.settings.slideshow.random_order);
        }
        if walk.truncated {
            core.toasts.info(format!("{} images in the slideshow (the subfolders hold too many to search them all)", self.list.len()));
        } else if announce {
            core.toasts.info(format!("{} images in the slideshow", self.list.len()));
        }
    }

    /// Position of `path` in the play order.
    fn pos_of(&self, path: &Path) -> Option<usize> {
        let i = self.list.iter().position(|p| p == path)?;
        self.order.iter().position(|&o| o == i)
    }

    /// Play `list` from now on: the slide on screen keeps its place and a
    /// move to another slide that is under way keeps its target.
    fn replace_list(&mut self, list: Vec<PathBuf>, random: bool) {
        let target = self.pending.and_then(|(p, d)| self.path_at(p).cloned().map(|path| (path, d)));
        let keep = self.current.as_ref().map(|c| c.path.clone()).or_else(|| target.as_ref().map(|t| t.0.clone())).or_else(|| self.list.get(self.index).cloned());
        self.index = keep.and_then(|k| list.iter().position(|p| *p == k)).unwrap_or(0);
        self.list = list;
        self.reorder(random);
        let pending = target.and_then(|(path, d)| self.pos_of(&path).map(|pos| (pos, d)));
        self.pending = pending;
        if self.current.is_none() && self.pending.is_none() && !self.order.is_empty() {
            // Nothing on screen yet (or every picture so far failed): start again.
            self.pending = Some((self.pos, 1));
            self.pending_since = Instant::now();
            self.all_failed = false;
        }
    }

    fn reorder(&mut self, random: bool) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let seed = splitmix(nanos ^ ((std::process::id() as u64) << 32));
        self.order = make_order(self.list.len(), self.index, random, seed);
        self.pos = self.order.iter().position(|&i| i == self.index).unwrap_or(0);
    }

    fn path_at(&self, pos: usize) -> Option<&PathBuf> {
        self.order.get(pos).and_then(|&i| self.list.get(i))
    }

    fn step(&self, pos: usize, delta: isize, looping: bool) -> Option<usize> {
        step_pos(pos, self.order.len(), delta, looping)
    }

    fn go(&mut self, pos: usize, dir: isize) {
        self.pending = Some((pos, dir));
        self.pending_since = Instant::now();
        self.ended = false;
    }

    fn go_rel(&mut self, delta: isize, looping: bool) {
        let from = self.pending.map(|(p, _)| p).unwrap_or(self.pos);
        if let Some(p) = self.step(from, delta, looping) {
            self.go(p, delta.signum());
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused && self.ended {
            self.ended = false;
            self.display = 0.0;
            if self.step(self.pos, 1, true).is_some() {
                self.go(self.step(self.pos, 1, true).unwrap_or(0), 1);
            }
        }
        // A video on screen pauses and plays on with the show.
        let paused = self.paused;
        if let Some(v) = self.current.as_mut().and_then(|c| c.video_mut()) {
            if paused {
                v.pause();
            } else if !v.ended() {
                v.play();
            }
        }
        self.flash = Some((if self.paused { I::PAUSE } else { I::PLAY }, Instant::now()));
    }

    /// Positions worth having decoded, most urgent first.
    fn wanted(&self, looping: bool) -> Vec<usize> {
        let mut v = Vec::new();
        let base = self.pending.map(|(p, _)| p).unwrap_or(self.pos);
        v.push(base);
        for d in [1isize, 2, -1] {
            if let Some(p) = self.step(base, d, looping) {
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
        v
    }

    fn holds(&self, path: &Path) -> bool {
        self.current.as_ref().is_some_and(|s| s.path == path) || self.outgoing.as_ref().is_some_and(|s| s.path == path) || self.ready.iter().any(|s| s.path == path)
    }

    fn take_ready(&mut self, path: &Path) -> Option<Slide> {
        if self.outgoing.as_ref().is_some_and(|s| s.path == path) {
            // Going back to the image we are transitioning away from.
            self.active = None;
            return self.outgoing.take();
        }
        // A video waits until its first frame is decoded, so the change to it
        // never shows black.
        if let Some(i) = self.ready.iter().position(|s| s.path == path && s.showable()) {
            return Some(self.ready.remove(i));
        }
        None
    }

    fn caption_for(&mut self, path: &Path) {
        while let Ok((p, d)) = self.caption_rx.try_recv() {
            self.captions.insert(p, d);
        }
        if !self.captions.contains_key(path) {
            // Placeholder until the background read lands.
            self.captions.insert(path.to_path_buf(), None);
            let tx = self.caption_tx.clone();
            let path = path.to_path_buf();
            rayon::spawn(move || {
                let date = crate::meta::read_exif(&path).and_then(|e| e.taken);
                let _ = tx.send((path, date));
            });
        }
    }

    /// Only the video on screen plays sound, and only with "play sound" on.
    fn sync_video_sound(&mut self, core: &Core) {
        // The others first: taking their sound away clears what is queued.
        for s in self.ready.iter_mut().chain(self.outgoing.iter_mut()) {
            if let Some(v) = s.video_mut() {
                v.set_audio(None);
            }
        }
        let want = core.settings.slideshow.video_sound && self.current.as_ref().is_some_and(|c| c.video().is_some());
        if !want {
            if let Some(v) = self.current.as_mut().and_then(|c| c.video_mut()) {
                v.set_audio(None);
            }
            return;
        }
        let out = self.sound.get_or_insert_with(|| SoundOut::open(&core.ctx));
        out.set_level(core.settings.video.volume, false);
        let shared = out.shared();
        if let (Some(v), Some(a)) = (self.current.as_mut().and_then(|c| c.video_mut()), shared) {
            v.set_audio(Some(a));
        }
    }

    /// One line for the dev hook's `state`.
    pub fn state_text(&self) -> String {
        let name = self.current.as_ref().and_then(|c| c.path.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let video = match self.current.as_ref().and_then(|c| c.video()) {
            Some(v) => format!(" video t={:.2}/{:.2} playing={} ended={} frame={}", v.position(), v.duration(), v.playing(), v.ended(), v.has_frame()),
            None => String::new(),
        };
        format!("[{}/{}] {name} paused={} changing={} waiting={}{video}", self.pos + 1, self.order.len(), self.paused, self.active.is_some(), self.pending.is_some())
    }

    /// Load/prepare slides and resolve the pending navigation target.
    fn update_loading(&mut self, core: &mut Core) {
        let looping = core.settings.slideshow.loop_show;
        let wanted = self.wanted(looping);
        let mut keep_q = Vec::new();
        for (prio, &pos) in wanted.iter().enumerate() {
            let Some(path) = self.path_at(pos).cloned() else { continue };
            if self.failed.contains(&path) {
                continue;
            }
            if !self.holds(&path) && crate::video::is_video_path(&path) {
                // A video opens its decoder (first frame, paused) when it is
                // on screen or next; it starts playing when its slide comes.
                if prio <= 1 {
                    let v = InlinePlayer::open(&path, &core.ctx, None, false);
                    self.ready.push(Slide { kb: KenBurns::from_seed(seed_of(&path)), path: path.clone(), pic: Pic::Video(v), clock: 0.0, warm: true });
                }
                continue;
            }
            if !self.holds(&path) {
                match core.loader.state(&path) {
                    Some(LoadState::Ready(_)) => {
                        if let Some(img) = core.loader.get_ready(&path) {
                            let gpu = GpuImage::new(&core.ctx, &format!("slide:{}", path.display()), img.pyramid.clone());
                            self.caption_for(&path);
                            self.ready.push(Slide { kb: KenBurns::from_seed(seed_of(&path)), path: path.clone(), pic: Pic::Image(gpu), clock: 0.0, warm: false });
                        }
                    }
                    Some(LoadState::Failed(_)) => {
                        self.failed.insert(path.clone());
                    }
                    _ => {
                        core.loader.request(&path, prio as u32, None);
                        keep_q.push(path);
                    }
                }
            }
        }
        core.loader.retain_queued(&keep_q);
        // Drop prepared slides no longer near the play position.
        let near: Vec<PathBuf> = wanted.iter().filter_map(|&p| self.path_at(p).cloned()).collect();
        self.ready.retain(|s| near.contains(&s.path));
        // A video that cannot play (or never shows a picture) is passed over.
        let broken: Vec<PathBuf> = self
            .ready
            .iter()
            .filter(|s| s.video().is_some_and(|v| v.error().is_some() || (!v.has_frame() && v.age().as_secs_f32() > playback::VIDEO_SLIDE_STALL_SECS)))
            .map(|s| s.path.clone())
            .collect();
        for p in broken {
            self.ready.retain(|s| s.path != p);
            self.failed.insert(p);
        }

        // Resolve pending target (skipping failures).
        if let Some((mut pos, dir)) = self.pending {
            let n = self.order.len();
            let mut tries = 0;
            loop {
                let Some(path) = self.path_at(pos).cloned() else {
                    self.pending = None;
                    break;
                };
                if self.failed.contains(&path) {
                    tries += 1;
                    if tries >= n {
                        self.all_failed = true;
                        self.pending = None;
                        break;
                    }
                    match self.step(pos, if dir == 0 { 1 } else { dir }, true) {
                        Some(p) if looping || (dir >= 0 && p > pos) || (dir < 0 && p < pos) => {
                            pos = p;
                            self.pending = Some((p, dir));
                            continue;
                        }
                        _ => {
                            self.pending = None;
                            if self.current.is_none() {
                                self.all_failed = true;
                            }
                            break;
                        }
                    }
                }
                if self.current.as_ref().is_some_and(|c| c.path == path) {
                    self.pending = None;
                    self.pos = pos;
                    break;
                }
                if let Some(slide) = self.take_ready(&path) {
                    self.pending = None;
                    self.pos = pos;
                    self.index = self.order[pos];
                    self.start_transition(slide, core);
                }
                break;
            }
        }
    }

    fn start_transition(&mut self, mut slide: Slide, core: &Core) {
        // A transition still running is finished instantly.
        if self.active.is_some() {
            self.outgoing = None;
        }
        slide.clock = 0.0;
        let old = self.current.take();
        let cfg = &core.settings.slideshow;
        let mut kind = Transition::from_name(&cfg.transition);
        if kind == Transition::Random {
            self.random_count += 1;
            let h = splitmix(seed_of(&slide.path) ^ self.random_count);
            kind = Transition::CONCRETE[(h % Transition::CONCRETE.len() as u64) as usize];
        }
        let mut duration = cfg.transition_secs.clamp(0.0, 5.0);
        if old.is_none() {
            // First slide fades in from black.
            kind = Transition::Crossfade;
            duration = duration.clamp(0.25, 0.6);
        }
        if kind == Transition::None {
            duration = 0.0;
        }
        // The video going away stops; the one coming plays from its start
        // (while the change runs, so the change never waits for it).
        let mut old = old;
        if let Some(v) = old.as_mut().and_then(|o| o.video_mut()) {
            v.pause();
        }
        if let Some(v) = slide.video_mut() {
            if v.position() > 0.0 || v.ended() {
                v.seek(0.0, true);
            }
            if !self.paused {
                v.play();
            }
        }
        self.outgoing = old;
        self.current = Some(slide);
        self.display = 0.0;
        self.active = if duration > 0.0 { Some(Active { kind, elapsed: 0.0, duration }) } else { None };
        if self.active.is_none() {
            if let Some(o) = self.outgoing.take() {
                self.ready.push(o);
            }
        }
    }

    // ------------------------------------------------------------ input

    fn handle_input(&mut self, ui: &mut Ui, core: &mut Core, stage: &egui::Response) -> bool {
        let looping = core.settings.slideshow.loop_show;
        let popup_open = egui::Popup::is_any_open(ui.ctx());
        let take = |k: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if take(Key::Escape) {
            if self.help {
                self.help = false;
            } else if !popup_open {
                return false;
            }
        }
        if take(Key::Space) || take(Key::P) {
            self.toggle_pause();
        }
        if take(Key::ArrowRight) || take(Key::PageDown) || take(Key::N) {
            self.go_rel(1, true);
        }
        if take(Key::ArrowLeft) || take(Key::PageUp) || take(Key::Backspace) {
            self.go_rel(-1, true);
        }
        if take(Key::Home) {
            self.go(0, 1);
        }
        if take(Key::End) {
            let n = self.order.len();
            if n > 0 {
                self.go(n - 1, -1);
            }
        }
        let cfg = &mut core.settings.slideshow;
        if take(Key::Plus) || take(Key::Equals) {
            cfg.interval_secs = next_interval(cfg.interval_secs, true);
            self.flash_text();
        }
        if take(Key::Minus) {
            cfg.interval_secs = next_interval(cfg.interval_secs, false);
            self.flash_text();
        }
        if take(Key::C) {
            cfg.show_caption = !cfg.show_caption;
        }
        if take(Key::K) {
            cfg.ken_burns = !cfg.ken_burns;
        }
        if take(Key::L) {
            cfg.loop_show = !cfg.loop_show;
        }
        if take(Key::M) {
            cfg.video_sound = !cfg.video_sound;
            self.flash = Some((if cfg.video_sound { I::SPEAKER_HIGH } else { I::SPEAKER_SLASH }, Instant::now()));
        }
        let mut reshuffle = false;
        if take(Key::S) {
            cfg.random_order = !cfg.random_order;
            reshuffle = true;
        }
        if take(Key::H) || take(Key::F1) {
            self.help = !self.help;
        }
        if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "?"))) {
            self.help = !self.help;
        }
        if reshuffle {
            let r = core.settings.slideshow.random_order;
            self.reorder(r);
        }
        // Wheel navigates.
        let wd = ui.input(super::wheel_delta);
        if stage.hovered() && wd != 0.0 {
            self.wheel_acc += wd;
            if self.wheel_acc.abs() >= 40.0 {
                let d = if self.wheel_acc > 0.0 { -1 } else { 1 };
                self.wheel_acc = 0.0;
                self.go_rel(d, looping);
            }
        }
        if stage.clicked() && !self.popup_was_open && !popup_open {
            if self.help {
                self.help = false;
            } else {
                self.toggle_pause();
            }
        }
        if stage.secondary_clicked() {
            self.go_rel(-1, true);
        }
        self.popup_was_open = popup_open;
        true
    }

    fn flash_text(&mut self) {
        self.flash = Some((I::TIMER, Instant::now()));
        self.last_activity = Instant::now();
    }

    // ------------------------------------------------------------ drawing

    fn base_dest(slide: &Slide, view: Rect, ppp: f32, fit: FitMode, kb: bool, kb_total: f32) -> (Rect, f32) {
        let dims = slide.dims();
        let zoom = fit_zoom(fit, dims, view.size() * ppp);
        let size = Vec2::new(dims.0 as f32, dims.1 as f32) * zoom / ppp;
        let mut dest = Rect::from_center_size(view.center(), size);
        let mut z = zoom;
        // Videos keep still (their own motion is enough).
        if kb && slide.video().is_none() {
            let (s, pan) = slide.kb.at(progress(slide.clock, kb_total));
            dest = KenBurns::apply(dest, s, pan);
            z *= s.max(1.0);
        } else {
            // Snap to physical pixels for a crisp still image.
            let min = Pos2::new((dest.min.x * ppp).round() / ppp, (dest.min.y * ppp).round() / ppp);
            dest = Rect::from_min_size(min, dest.size());
        }
        (dest, z)
    }

    fn layer_dest(view: Rect, base: (Rect, f32), layer: &Layer) -> (Rect, f32) {
        let (dest, zoom) = base;
        let c = view.center() + (dest.center() - view.center()) * layer.scale + layer.offset * view.size();
        (Rect::from_center_size(c, dest.size() * layer.scale), zoom * layer.scale)
    }

    fn paint_layer(ctx: &egui::Context, painter: &egui::Painter, view: Rect, slide: &mut Slide, d: (Rect, f32), layer: &Layer, hold: Option<Rect>) {
        if layer.alpha <= 0.002 {
            // Still draw the held part (under the incoming image) if any.
            if hold.is_none() {
                return;
            }
        }
        let (dest, zoom) = d;
        let tint = Color32::WHITE.gamma_multiply(layer.alpha.clamp(0.0, 1.0));
        let clips = clip_rects(&layer.clip, view);
        for clip in clips {
            let clip = clip.intersect(view);
            if clip.width() <= 0.0 || clip.height() <= 0.0 {
                continue;
            }
            match hold {
                Some(h) => {
                    let inside = clip.intersect(h);
                    if inside.width() > 0.0 && inside.height() > 0.0 {
                        slide.paint(ctx, &painter.with_clip_rect(inside), dest, inside, zoom, Color32::WHITE);
                    }
                    if layer.alpha > 0.002 {
                        for r in rect_minus(clip, h) {
                            slide.paint(ctx, &painter.with_clip_rect(r), dest, r, zoom, tint);
                        }
                    }
                }
                None => {
                    slide.paint(ctx, &painter.with_clip_rect(clip), dest, clip, zoom, tint);
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        while let Ok((p, d)) = self.caption_rx.try_recv() {
            self.captions.insert(p, d);
        }
        self.poll_scan(core);
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        let p = core.pal;
        let view = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        ui.painter().rect_filled(view, 0.0, Color32::BLACK);
        let stage = ui.interact(view, ui.scope_id().with("slideshow-stage"), Sense::click());

        if ui.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.any_pressed()) {
            self.last_activity = now;
        }

        if !self.handle_input(ui, core, &stage) {
            return false;
        }
        self.update_loading(core);

        let cfg = core.settings.slideshow.clone();
        let interval = cfg.interval_secs.clamp(0.5, 3600.0);
        let kb_total = interval + 2.0 * cfg.transition_secs.max(0.0);

        // ---- Videos: frames from their decoders, sound for the one on screen
        self.sync_video_sound(core);
        let ctx = ui.ctx().clone();
        for s in self.ready.iter_mut().chain(self.outgoing.iter_mut()).chain(self.current.iter_mut()) {
            if let Some(v) = s.video_mut() {
                v.tick(&ctx);
            }
        }

        // ---- Clocks
        if let Some(a) = &mut self.active {
            a.elapsed += dt.min(0.1);
            if a.elapsed >= a.duration {
                self.active = None;
                // Keep the previous slide around for instant "back".
                if let Some(o) = self.outgoing.take() {
                    self.ready.push(o);
                }
            }
        }
        if !self.paused {
            if let Some(c) = &mut self.current {
                c.clock += dt.min(0.5);
            }
            if let Some(o) = &mut self.outgoing {
                o.clock += dt.min(0.5);
            }
            if self.active.is_none() && self.current.is_some() {
                self.display += dt;
            }
        }

        // ---- Auto advance: a picture after the interval, a video when it
        // ends (or after its time limit).
        let due = match self.current.as_ref().and_then(|c| c.video()) {
            Some(v) => v.error().is_some() || playback::video_slide_due(v.ended(), self.display, cfg.video_max_secs) || (!v.has_frame() && self.display > playback::VIDEO_SLIDE_STALL_SECS),
            None => self.display >= interval,
        };
        if due && self.order.len() == 1 && cfg.loop_show && !self.paused && self.active.is_none() {
            // A lone video with Loop on plays again.
            if let Some(v) = self.current.as_mut().and_then(|c| c.video_mut()) {
                v.play();
                self.display = 0.0;
            }
        }
        if !self.paused && self.active.is_none() && self.pending.is_none() && self.current.is_some() && due {
            match self.step(self.pos, 1, cfg.loop_show) {
                Some(p) if self.order.len() > 1 => self.go(p, 1),
                _ => {
                    if self.order.len() > 1 {
                        self.ended = true;
                        self.paused = true;
                    }
                }
            }
            if self.pending.is_some() {
                self.update_loading(core);
            }
        }

        // ---- Draw slides
        let painter = ui.painter().with_clip_rect(view);
        let t = self.active.as_ref().map(|a| ease_in_out_cubic(progress(a.elapsed, a.duration)));
        match (t, self.active.as_ref().map(|a| a.kind)) {
            (Some(t), Some(kind)) => {
                let frame = transition_frame(kind, t);
                let to_d = self.current.as_ref().map(|c| Self::layer_dest(view, Self::base_dest(c, view, ppp, cfg.fit, cfg.ken_burns, kb_total), &frame.to));
                if let Some(o) = &mut self.outgoing {
                    let base = Self::base_dest(o, view, ppp, cfg.fit, cfg.ken_burns, kb_total);
                    let d = Self::layer_dest(view, base, &frame.from);
                    let hold = if frame.from.hold_under_next { to_d.map(|(r, _)| r.intersect(view)) } else { None };
                    Self::paint_layer(&ctx, &painter, view, o, d, &frame.from, hold);
                }
                if let (Some(c), Some(d)) = (&mut self.current, to_d) {
                    Self::paint_layer(&ctx, &painter, view, c, d, &frame.to, None);
                }
            }
            _ => {
                if let Some(c) = &mut self.current {
                    let d = Self::base_dest(c, view, ppp, cfg.fit, cfg.ken_burns, kb_total);
                    c.paint(&ctx, &painter, d.0, view, d.1, Color32::WHITE);
                    c.warm = true;
                }
            }
        }
        // Pre-warm the next slide's tiles while nothing is moving.
        if self.active.is_none() {
            let hidden = ui.painter().with_clip_rect(Rect::NOTHING);
            if let Some(s) = self.ready.iter_mut().find(|s| !s.warm) {
                let d = Self::base_dest(s, view, ppp, cfg.fit, false, kb_total);
                let pending = s.paint(&ctx, &hidden, d.0, view, d.1, Color32::TRANSPARENT);
                s.warm = !pending;
            }
        }

        // ---- Status / placeholders
        if self.current.is_none() {
            // (While the subfolders are searched, more pictures may still come.)
            if (self.all_failed || self.order.is_empty()) && self.scan.is_none() {
                painter.text(view.center() - Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(44.0), p.faint);
                painter.text(view.center() + Vec2::new(0.0, 24.0), Align2::CENTER_CENTER, "None of these files could be shown", FontId::proportional(15.0), Color32::from_gray(200));
                painter.text(view.center() + Vec2::new(0.0, 48.0), Align2::CENTER_CENTER, "Press Esc to leave the slideshow", FontId::proportional(12.5), Color32::from_gray(140));
            } else if self.pending_since.elapsed() > Duration::from_millis(250) {
                let r = Rect::from_center_size(view.center(), Vec2::splat(30.0));
                egui::Spinner::new().size(30.0).color(Color32::from_gray(160)).paint_at(ui, r);
            }
        } else if self.pending.is_some() && self.pending_since.elapsed() > Duration::from_millis(400) {
            let r = Rect::from_center_size(view.right_top() + Vec2::new(-28.0, 28.0), Vec2::splat(18.0));
            egui::Spinner::new().size(18.0).color(Color32::from_gray(180)).paint_at(ui, r);
        }
        if self.scan.is_some() {
            let g = ui.painter().layout_no_wrap("Finding images in subfolders...".into(), FontId::proportional(13.0), Color32::from_white_alpha(230));
            let r = Rect::from_min_size(view.left_top() + Vec2::new(18.0, 18.0), g.size() + Vec2::new(44.0, 14.0));
            ui.painter().rect_filled(r, CornerRadius::same(9), Color32::from_black_alpha(150));
            // The spinner keeps frames coming, so the result is picked up as soon as it lands.
            egui::Spinner::new().size(14.0).color(Color32::from_gray(200)).paint_at(ui, Rect::from_center_size(r.left_center() + Vec2::new(17.0, 0.0), Vec2::splat(14.0)));
            ui.painter().galley(r.min + Vec2::new(32.0, 7.0), g, Color32::WHITE);
        }

        // ---- Overlays
        let idle = self.last_activity.elapsed() > Duration::from_secs(2);
        let bar_rect = Rect::from_center_size(Pos2::new(view.center().x, view.max.y - 40.0), Vec2::new(780.0_f32.min(view.width() - 24.0), 56.0));
        let hover_bar = ui.input(|i| i.pointer.hover_pos()).is_some_and(|pos| bar_rect.expand(12.0).contains(pos));
        let show_bar = !idle || hover_bar || self.popup_was_open || self.help;
        if cfg.show_caption {
            let y = if show_bar { bar_rect.min.y - 12.0 } else { view.max.y - 18.0 };
            self.paint_caption(ui, view, y);
        }
        if self.ended {
            let text = "End of slideshow  ·  Space to restart  ·  Esc to exit";
            let g = ui.painter().layout_no_wrap(text.into(), FontId::proportional(14.0), Color32::from_white_alpha(235));
            let r = Rect::from_center_size(view.center_top() + Vec2::new(0.0, 40.0), g.size() + Vec2::new(28.0, 14.0));
            ui.painter().rect_filled(r, CornerRadius::same(10), Color32::from_black_alpha(170));
            ui.painter().galley(r.min + Vec2::new(14.0, 7.0), g, Color32::WHITE);
        }
        if show_bar {
            self.control_bar(ui, core, bar_rect, interval);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }
        let mut flashing = false;
        if let Some((icon, at)) = self.flash {
            let a = 1.0 - progress(at.elapsed().as_secs_f32(), 0.7);
            if a <= 0.0 {
                self.flash = None;
            } else {
                flashing = true;
                let r = Rect::from_center_size(view.center(), Vec2::splat(84.0));
                ui.painter().rect_filled(r, CornerRadius::same(42), Color32::from_black_alpha((150.0 * a) as u8));
                if icon == I::TIMER {
                    let text = fmt_secs(core.settings.slideshow.interval_secs);
                    ui.painter().text(r.center() - Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, icon, FontId::proportional(24.0), Color32::from_white_alpha((255.0 * a) as u8));
                    ui.painter().text(r.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, text, FontId::proportional(14.0), Color32::from_white_alpha((255.0 * a) as u8));
                } else {
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, icon, super::theme::icon_fill(36.0), Color32::from_white_alpha((255.0 * a) as u8));
                }
            }
        }
        if self.help {
            self.help_overlay(ui, view);
        }

        // ---- Repaint scheduling: smooth while moving, near-idle otherwise.
        let warming = self.active.is_none() && self.ready.iter().any(|s| !s.warm);
        let moving = self.active.is_some() || (cfg.ken_burns && self.current.is_some() && !self.paused) || flashing || warming;
        if moving {
            ui.ctx().request_repaint();
        } else if show_bar {
            // The bar only needs a gentle tick for its progress line and auto-hide timer.
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if !self.paused && self.current.is_some() && self.pending.is_none() {
            let left = (interval - self.display).max(0.0);
            ui.ctx().request_repaint_after(Duration::from_secs_f32(left.min(10.0) + 0.001));
        } else if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        !self.exit_requested
    }

    fn paint_caption(&self, ui: &Ui, view: Rect, bottom: f32) {
        let Some(c) = &self.current else { return };
        let name = c.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut parts = vec![format!("{} / {}", self.pos + 1, self.order.len()), name];
        let (w, h) = c.dims();
        parts.push(format!("{w} × {h}"));
        if let Some(v) = c.video() {
            parts.push(playback::position_text(v.position(), v.duration()));
        }
        if let Some(Some(d)) = self.captions.get(&c.path) {
            parts.push(d.clone());
        }
        let text = parts.join("   ·   ");
        let g = ui.painter().layout(text, FontId::proportional(13.0), Color32::from_white_alpha(235), view.width() - 60.0);
        let r = Rect::from_min_size(Pos2::new(view.min.x + 18.0, bottom - g.size().y - 12.0), g.size() + Vec2::new(24.0, 12.0));
        ui.painter().rect_filled(r, CornerRadius::same(9), Color32::from_black_alpha(140));
        ui.painter().galley(r.min + Vec2::new(12.0, 6.0), g, Color32::WHITE);
    }

    fn control_bar(&mut self, ui: &mut Ui, core: &mut Core, rect: Rect, interval: f32) {
        let p = core.pal;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(10.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        // Swallow clicks on the bar background so they don't toggle pause.
        let _ = child.interact(rect, child.scope_id().with("bar-blocker"), Sense::click());
        let bg = Color32::from_rgba_unmultiplied(18, 19, 22, 225);
        ui.painter().rect_filled(rect, CornerRadius::same(12), bg);
        ui.painter().rect_stroke(rect, CornerRadius::same(12), Stroke::new(1.0, Color32::from_white_alpha(22)), egui::StrokeKind::Inside);
        // Progress of the current slide (a video's: how far it played).
        let frac = match &self.current {
            Some(c) => match c.video() {
                Some(v) => playback::video_slide_progress(v.position(), v.duration(), self.display, core.settings.slideshow.video_max_secs),
                None => progress(self.display, interval),
            },
            None => 0.0,
        };
        let track = Rect::from_min_size(rect.min + Vec2::new(14.0, 5.0), Vec2::new(rect.width() - 28.0, 3.0));
        ui.painter().rect_filled(track, CornerRadius::same(2), Color32::from_white_alpha(28));
        let mut fill = track;
        fill.max.x = track.min.x + track.width() * frac;
        ui.painter().rect_filled(fill, CornerRadius::same(2), if self.paused { p.muted } else { p.accent });

        // Dark styling for widgets regardless of the app theme.
        let vis = child.visuals_mut();
        *vis = egui::Visuals::dark();
        vis.widgets.inactive.weak_bg_fill = Color32::from_white_alpha(14);
        vis.widgets.inactive.bg_fill = Color32::from_white_alpha(14);
        vis.widgets.hovered.weak_bg_fill = Color32::from_white_alpha(30);
        vis.widgets.inactive.bg_stroke = Stroke::NONE;
        // The one selected style (soft accent fill, accent icon), as in dark mode.
        vis.selection.bg_fill = Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 56);
        vis.selection.stroke = Stroke::new(1.0, p.accent);
        child.spacing_mut().item_spacing.x = 2.0;
        child.add_space(2.0);

        let looping = core.settings.slideshow.loop_show;
        if widgets::icon_button(&mut child, I::SKIP_BACK, "Previous  (← / PgUp)", false).clicked() {
            self.go_rel(-1, true);
        }
        let (icon, tip) = if self.paused { (I::PLAY, "Play  (Space)") } else { (I::PAUSE, "Pause  (Space)") };
        if widgets::icon_button_sized(&mut child, icon, tip, false, 34.0, 20.0).clicked() {
            self.toggle_pause();
        }
        if widgets::icon_button(&mut child, I::SKIP_FORWARD, "Next  (→ / PgDn)", false).clicked() {
            self.go_rel(1, true);
        }
        // The position is in the caption (and the progress line), not repeated here.
        widgets::separator_v(&mut child);

        let cfg = &mut core.settings.slideshow;
        child.label(RichText::new(I::TIMER).size(15.0).color(Color32::from_gray(190))).on_hover_text("Time per slide  (+ / -)");
        widgets::combo("ss-interval").width(62.0).selected_text(fmt_secs(cfg.interval_secs)).show_ui(&mut child, |ui| {
            for v in INTERVALS {
                ui.selectable_value(&mut cfg.interval_secs, v, fmt_secs(v));
            }
        })
        .response
        .on_hover_text("How long each picture stays on screen  (+ / -)");
        child.add_space(4.0);
        child.label(RichText::new(I::SPARKLE).size(15.0).color(Color32::from_gray(190))).on_hover_text("How one picture changes to the next");
        let cur = Transition::from_name(&cfg.transition);
        widgets::combo("ss-transition").width(150.0).selected_text(cur.label()).height(420.0).show_ui(&mut child, |ui| {
            for t in Transition::ALL {
                if ui.selectable_label(cur == t, t.label()).clicked() {
                    cfg.transition = t.name().to_string();
                }
            }
        })
        .response
        .on_hover_text("How one picture changes to the next");
        widgets::combo("ss-duration").width(56.0).selected_text(fmt_secs(cfg.transition_secs)).show_ui(&mut child, |ui| {
            for v in DURATIONS {
                ui.selectable_value(&mut cfg.transition_secs, v, fmt_secs(v));
            }
        }).response.on_hover_text("How long the change between pictures takes");
        widgets::separator_v(&mut child);

        let mut reshuffle = None;
        if widgets::icon_button(&mut child, I::SHUFFLE, "Shuffle: show the pictures in random order  (S)", cfg.random_order).clicked() {
            cfg.random_order = !cfg.random_order;
            reshuffle = Some(cfg.random_order);
        }
        if widgets::icon_button(&mut child, I::REPEAT, "Loop: start again after the last picture  (L)", looping).clicked() {
            cfg.loop_show = !cfg.loop_show;
        }
        if widgets::icon_button(&mut child, I::FRAME_CORNERS, "Slow pan and zoom on each picture  (K)", cfg.ken_burns).clicked() {
            cfg.ken_burns = !cfg.ken_burns;
        }
        if widgets::icon_button(&mut child, I::CLOSED_CAPTIONING, "Show the file name and position  (C)", cfg.show_caption).clicked() {
            cfg.show_caption = !cfg.show_caption;
        }
        let mut subfolders = None;
        if widgets::icon_button(&mut child, I::FOLDERS, "Also show pictures from folders inside this one", cfg.include_subfolders).clicked() {
            cfg.include_subfolders = !cfg.include_subfolders;
            subfolders = Some(cfg.include_subfolders);
        }
        let (icon, tip) = if cfg.video_sound { (I::SPEAKER_HIGH, "Videos play with sound. Click to mute them  (M)") } else { (I::SPEAKER_SLASH, "Videos play without sound. Click to hear them  (M)") };
        if widgets::icon_button(&mut child, icon, tip, cfg.video_sound).clicked() {
            cfg.video_sound = !cfg.video_sound;
        }
        widgets::separator_v(&mut child);
        if widgets::icon_button(&mut child, I::QUESTION, "Keyboard shortcuts  (H)", self.help).clicked() {
            self.help = !self.help;
        }
        if widgets::icon_button(&mut child, I::X, "Exit slideshow  (Esc)", false).clicked() {
            self.exit_requested = true;
        }
        if let Some(r) = reshuffle {
            self.reorder(r);
        }
        if let Some(sub) = subfolders {
            let keep = self.current.as_ref().map(|c| c.path.clone()).or_else(|| self.list.get(self.index).cloned());
            let r = core.settings.slideshow.random_order;
            // Turning it on reports the count once the subfolders are searched.
            self.rebuild_list(sub, keep.as_deref(), &core.ctx, r, true);
            if !sub {
                core.toasts.info(format!("{} images in the slideshow", self.list.len()));
            }
        }
    }

    fn help_overlay(&self, ui: &Ui, view: Rect) {
        let rows: [(&str, &str); 14] = [
            ("Space / P / click", "Pause or resume"),
            ("→  PgDn  wheel ↓", "Next image"),
            ("←  PgUp  wheel ↑", "Previous image"),
            ("Right-click", "Previous image"),
            ("Home / End", "First / last image"),
            ("+ / -", "Longer / shorter interval"),
            ("C", "Show / hide caption"),
            ("K", "Ken Burns pan & zoom"),
            ("S", "Shuffle"),
            ("L", "Loop"),
            ("M", "Sound of videos on / off"),
            ("H  ?", "This help"),
            ("F11", "Toggle full screen"),
            ("Esc", "Exit slideshow"),
        ];
        let w = 380.0;
        let h = 56.0 + rows.len() as f32 * 24.0;
        let r = Rect::from_center_size(view.center(), Vec2::new(w, h));
        let painter = ui.painter();
        painter.rect_filled(r, CornerRadius::same(14), Color32::from_rgba_unmultiplied(18, 19, 22, 238));
        painter.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, Color32::from_white_alpha(26)), egui::StrokeKind::Inside);
        painter.text(r.min + Vec2::new(22.0, 22.0), Align2::LEFT_CENTER, "Slideshow shortcuts", super::theme::semibold(15.0), Color32::WHITE);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = r.min.y + 52.0 + i as f32 * 24.0;
            painter.text(Pos2::new(r.min.x + 22.0, y), Align2::LEFT_CENTER, *k, FontId::proportional(13.0), Color32::from_gray(150));
            painter.text(Pos2::new(r.min.x + 170.0, y), Align2::LEFT_CENTER, *v, FontId::proportional(13.0), Color32::from_gray(235));
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints_and_symmetry() {
        assert_eq!(ease_in_out_cubic(0.0), 0.0);
        assert_eq!(ease_in_out_cubic(1.0), 1.0);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-6);
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert!((ease_in_out_cubic(t) + ease_in_out_cubic(1.0 - t) - 1.0).abs() < 1e-5);
        }
        // Monotonic.
        let mut last = -1.0;
        for i in 0..=100 {
            let v = ease_in_out_cubic(i as f32 / 100.0);
            assert!(v >= last);
            last = v;
        }
        assert_eq!(ease_in_out_cubic(-1.0), 0.0);
        assert_eq!(ease_in_out_cubic(2.0), 1.0);
    }

    #[test]
    fn progress_clamps() {
        assert_eq!(progress(0.0, 1.0), 0.0);
        assert_eq!(progress(0.5, 1.0), 0.5);
        assert_eq!(progress(3.0, 1.0), 1.0);
        assert_eq!(progress(0.0, 0.0), 1.0);
    }

    #[test]
    fn transition_names_round_trip() {
        for t in Transition::ALL {
            assert_eq!(Transition::from_name(t.name()), t);
        }
        assert_eq!(Transition::from_name("Slide"), Transition::SlideLeft);
        assert_eq!(Transition::from_name("fade through black"), Transition::FadeBlack);
        assert_eq!(Transition::from_name("garbage"), Transition::Crossfade);
    }

    #[test]
    fn transition_frames_start_and_end_states() {
        for kind in Transition::CONCRETE {
            let end = transition_frame(kind, 1.0);
            assert!((end.to.alpha - 1.0).abs() < 1e-6, "{kind:?}");
            assert_eq!(end.to.offset, Vec2::ZERO, "{kind:?}");
            assert!((end.to.scale - 1.0).abs() < 1e-6, "{kind:?}");
            let view = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0));
            let area: f32 = clip_rects(&end.to.clip, view).iter().map(|r| r.area()).sum();
            assert!(area >= view.area() * 0.999, "{kind:?} end not fully revealed ({area})");

            let start = transition_frame(kind, 0.0);
            let visible_area: f32 = clip_rects(&start.to.clip, view).iter().map(|r| r.area()).sum();
            let hidden = start.to.alpha == 0.0 || visible_area == 0.0 || start.to.offset.length() >= 1.0 || start.to.scale != 1.0;
            assert!(hidden || kind == Transition::None, "{kind:?} should start hidden");
        }
    }

    #[test]
    fn wipe_and_push_midpoints() {
        let f = transition_frame(Transition::WipeRight, 0.5);
        assert_eq!(f.to.clip, Clip::Rect(nrect(0.0, 0.0, 0.5, 1.0)));
        let f = transition_frame(Transition::PushLeft, 0.25);
        assert_eq!(f.from.offset, Vec2::new(-0.25, 0.0));
        assert_eq!(f.to.offset, Vec2::new(0.75, 0.0));
        let f = transition_frame(Transition::FadeBlack, 0.5);
        assert!(f.from.alpha.abs() < 1e-6 && f.to.alpha.abs() < 1e-6);
        let f = transition_frame(Transition::Blinds, 0.5);
        match f.to.clip {
            Clip::Strips(v) => {
                assert_eq!(v.len(), BLINDS);
                let covered: f32 = v.iter().map(|r| r.height()).sum();
                assert!((covered - 0.5).abs() < 1e-5);
            }
            _ => panic!("blinds use strips"),
        }
    }

    #[test]
    fn iris_strips_approximate_circle() {
        let view = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0));
        let half_diag = 0.5 * view.size().length();
        let radius = 0.5; // normalized
        let rects = clip_rects(&Clip::Circle { center: Pos2::new(0.5, 0.5), radius }, view);
        assert!(!rects.is_empty() && rects.len() <= 200);
        let area: f32 = rects.iter().map(|r| r.area()).sum();
        let r = radius * half_diag;
        let expect = std::f32::consts::PI * r * r;
        assert!((area - expect).abs() / expect < 0.03, "area {area} vs {expect}");
        assert!(clip_rects(&Clip::Circle { center: Pos2::new(0.5, 0.5), radius: 0.0 }, view).is_empty());
    }

    #[test]
    fn rect_minus_covers_complement() {
        let outer = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0));
        let hole = Rect::from_min_max(Pos2::new(20.0, 10.0), Pos2::new(60.0, 40.0));
        let parts = rect_minus(outer, hole);
        let area: f32 = parts.iter().map(|r| r.area()).sum();
        assert!((area - (outer.area() - hole.area())).abs() < 1e-3);
        for p in &parts {
            assert!(p.intersect(hole).area() <= 0.0 || p.intersect(hole).width() <= 0.0 || p.intersect(hole).height() <= 0.0);
        }
        assert_eq!(rect_minus(outer, Rect::from_min_size(Pos2::new(200.0, 200.0), Vec2::splat(5.0))), vec![outer]);
    }

    #[test]
    fn ken_burns_is_deterministic_and_keeps_fit_covered() {
        let a = KenBurns::from_seed(42);
        let b = KenBurns::from_seed(42);
        assert_eq!(a, b);
        assert_ne!(KenBurns::from_seed(1), KenBurns::from_seed(2));
        let fit = Rect::from_min_size(Pos2::new(100.0, 0.0), Vec2::new(800.0, 600.0));
        for i in 0..=10 {
            let (s, pan) = a.at(i as f32 / 10.0);
            assert!(s >= 1.0 && s < 1.3);
            let d = KenBurns::apply(fit, s, pan);
            assert!(d.contains_rect(fit.shrink(0.01)), "{d:?} must cover {fit:?}");
        }
    }

    #[test]
    fn order_and_stepping() {
        assert_eq!(step_pos(0, 5, 1, false), Some(1));
        assert_eq!(step_pos(4, 5, 1, false), None);
        assert_eq!(step_pos(4, 5, 1, true), Some(0));
        assert_eq!(step_pos(0, 5, -1, true), Some(4));
        assert_eq!(step_pos(0, 0, 1, true), None);
        let o = make_order(10, 3, false, 7);
        assert_eq!(o, (0..10).collect::<Vec<_>>());
        let o = make_order(10, 3, true, 7);
        assert_eq!(o[0], 3);
        let mut s = o.clone();
        s.sort();
        assert_eq!(s, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn interval_steps() {
        assert_eq!(next_interval(4.0, true), 5.0);
        assert_eq!(next_interval(4.0, false), 3.0);
        assert_eq!(next_interval(4.5, true), 5.0);
        assert_eq!(next_interval(1.0, false), 1.0);
        assert_eq!(next_interval(120.0, true), 120.0);
    }

    #[test]
    fn fit_zoom_modes() {
        let v = Vec2::new(1920.0, 1080.0);
        assert!((fit_zoom(FitMode::FitAlways, (3840, 2160), v) - 0.5).abs() < 1e-6);
        assert!((fit_zoom(FitMode::FitLarge, (960, 540), v) - 1.0).abs() < 1e-6);
        assert!((fit_zoom(FitMode::FitAlways, (960, 540), v) - 2.0).abs() < 1e-6);
        assert!((fit_zoom(FitMode::Fill, (1000, 1000), v) - 1.92).abs() < 1e-6);
    }

    #[test]
    fn subfolder_walk_has_limits() {
        let d = std::env::temp_dir().join(format!("lumen-ss-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("s1").join("s2")).unwrap();
        for f in [d.join("a.jpg"), d.join("s1").join("b.png"), d.join("s1").join("s2").join("c.gif"), d.join("s1").join("notes.txt")] {
            std::fs::write(f, b"x").unwrap();
        }
        let far = Instant::now() + Duration::from_secs(60);
        let go = AtomicBool::new(false);
        let (mut all, cut) = walk_images(&d, 100, 100, far, &go);
        all.sort();
        assert!(!cut);
        assert_eq!(all, vec![d.join("a.jpg"), d.join("s1").join("b.png"), d.join("s1").join("s2").join("c.gif")]);
        // One folder read: only the top folder's own pictures, marked as cut short.
        assert_eq!(walk_images(&d, 100, 1, far, &go), (vec![d.join("a.jpg")], true));
        assert!(walk_images(&d, 2, 100, far, &go).1, "image limit");
        assert_eq!(walk_images(&d, 100, 100, Instant::now(), &go), (Vec::new(), true), "time limit");
        let stop = AtomicBool::new(true);
        assert!(walk_images(&d, 100, 100, far, &stop).1, "cancelled");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn subfolder_pictures_follow_the_folder() {
        let d = PathBuf::from(r"C:\pics");
        let (a, b) = (d.join("b.jpg"), d.join("a.jpg"));
        let (x, y, z) = (d.join("sub 10").join("x.jpg"), d.join("sub 2").join("y2.jpg"), d.join("sub 2").join("y10.jpg"));
        let all = vec![x.clone(), a.clone(), z.clone(), b.clone(), y.clone()];
        // The folder keeps the browser's order; subfolders follow in natural order.
        assert_eq!(merge_subfolders(&[a.clone(), b.clone()], all.clone(), &d), Some(vec![a.clone(), b.clone(), y.clone(), z.clone(), x.clone()]));
        // A hand-picked selection is not expanded.
        assert_eq!(merge_subfolders(&[a.clone()], all, &d), None);
        assert_eq!(merge_subfolders(&[a.clone(), b.clone()], vec![a, b], &d), None, "nothing to add");
    }
}
