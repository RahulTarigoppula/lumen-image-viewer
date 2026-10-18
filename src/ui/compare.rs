//! Side-by-side comparison of 2–4 images with synchronized zoom/pan, plus
//! split-slider, flicker and difference modes for pairs.
//!
//! View model: every pane has a `View`. With the *relative* basis the zoom is a
//! multiple of the pane's fit zoom and the centre is a normalized image
//! coordinate, so images of different resolutions stay aligned (same framing).
//! With the *pixel* basis the zoom is physical px per image px and the centre
//! is an offset in image pixels from the image centre, so both show the same
//! pixel scale. When sync is on, an operation is computed on the pane under the
//! cursor and the resulting `View` is copied to every pane.

use super::icons as I;
use super::widgets;
use crate::app::Core;
use crate::edit::Histogram;
use crate::loader::{LoadState, LoadedImage};
use crate::render::{self, GpuImage};
use crossbeam_channel::{unbounded, Receiver};
use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- layout

/// Pane rectangles: 1 = full, 2 = halves, 3 = three columns, 4 = 2×2 grid.
pub fn pane_rects(n: usize, area: Rect, gap: f32) -> Vec<Rect> {
    let col = |cols: usize, i: usize, r: Rect| -> Rect {
        let w = (r.width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let x = r.min.x + i as f32 * (w + gap);
        Rect::from_min_max(Pos2::new(x, r.min.y), Pos2::new(x + w, r.max.y))
    };
    match n {
        0 => Vec::new(),
        1 => vec![area],
        2 | 3 => (0..n).map(|i| col(n, i, area)).collect(),
        _ => {
            let h = (area.height() - gap) / 2.0;
            let top = Rect::from_min_max(area.min, Pos2::new(area.max.x, area.min.y + h));
            let bottom = Rect::from_min_max(Pos2::new(area.min.x, area.max.y - h), area.max);
            vec![col(2, 0, top), col(2, 1, top), col(2, 0, bottom), col(2, 1, bottom)]
        }
    }
}

// ---------------------------------------------------------------- view maths

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    Relative,
    Pixel,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub fit: bool,
    pub zoom: f32,
    pub center: Vec2,
}

impl View {
    pub fn fitted(basis: Basis) -> View {
        match basis {
            Basis::Relative => View { fit: true, zoom: 1.0, center: Vec2::splat(0.5) },
            Basis::Pixel => View { fit: true, zoom: 1.0, center: Vec2::ZERO },
        }
    }
}

pub fn fit_of(img: (u32, u32), pane_px: Vec2) -> f32 {
    (pane_px.x / img.0.max(1) as f32).min(pane_px.y / img.1.max(1) as f32).max(1e-6)
}

fn isz(img: (u32, u32)) -> Vec2 {
    Vec2::new(img.0.max(1) as f32, img.1.max(1) as f32)
}

/// Effective (zoom in physical px per image px, image pixel at the pane centre).
/// `fit_ref` is the shared fit zoom used by the pixel basis.
pub fn pane_transform(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32) -> (f32, Vec2) {
    let s = isz(img);
    match basis {
        Basis::Relative => {
            let fz = fit_of(img, pane_px);
            if v.fit {
                (fz, s * 0.5)
            } else {
                (fz * v.zoom, Vec2::new(v.center.x * s.x, v.center.y * s.y))
            }
        }
        Basis::Pixel => {
            if v.fit {
                (fit_ref, s * 0.5)
            } else {
                (v.zoom, s * 0.5 + v.center)
            }
        }
    }
}

/// Turn a "fit" view into explicit numbers so it can be modified.
pub fn materialize(v: &View, basis: Basis, fit_ref: f32) -> View {
    if !v.fit {
        return *v;
    }
    match basis {
        Basis::Relative => View { fit: false, zoom: 1.0, center: Vec2::splat(0.5) },
        Basis::Pixel => View { fit: false, zoom: fit_ref, center: Vec2::ZERO },
    }
}

const MIN_Z: f32 = 0.005;
const MAX_Z: f32 = 64.0;

/// Zoom by `factor` keeping the image point under `anchor` (physical px from the pane centre) fixed.
pub fn zoom_at(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32, factor: f32, anchor: Vec2) -> View {
    let m = materialize(v, basis, fit_ref);
    let (z, c) = pane_transform(&m, basis, img, pane_px, fit_ref);
    let a = c + anchor / z;
    let nz = (z * factor).clamp(MIN_Z, MAX_Z);
    let nc = a - anchor / nz;
    from_effective(basis, img, pane_px, nz, nc)
}

/// Build a view from an effective zoom and image centre (inverse of `pane_transform`).
pub fn from_effective(basis: Basis, img: (u32, u32), pane_px: Vec2, z: f32, c: Vec2) -> View {
    let s = isz(img);
    let v = match basis {
        Basis::Relative => View { fit: false, zoom: z / fit_of(img, pane_px), center: Vec2::new(c.x / s.x, c.y / s.y) },
        Basis::Pixel => View { fit: false, zoom: z, center: c - s * 0.5 },
    };
    clamp_view(v, basis, img)
}

/// Pan by `delta` physical pixels (positive = content moves right/down).
pub fn pan_by(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32, delta: Vec2) -> View {
    let m = materialize(v, basis, fit_ref);
    let (z, c) = pane_transform(&m, basis, img, pane_px, fit_ref);
    from_effective(basis, img, pane_px, z, c - delta / z)
}

fn clamp_view(mut v: View, basis: Basis, img: (u32, u32)) -> View {
    let s = isz(img);
    match basis {
        Basis::Relative => {
            v.center = Vec2::new(v.center.x.clamp(0.0, 1.0), v.center.y.clamp(0.0, 1.0));
        }
        Basis::Pixel => {
            v.center = Vec2::new(v.center.x.clamp(-s.x / 2.0, s.x / 2.0), v.center.y.clamp(-s.y / 2.0, s.y / 2.0));
        }
    }
    v
}

/// Destination rectangle (points) of the whole image in a pane.
pub fn dest_rect(pane: Rect, ppp: f32, img: (u32, u32), z: f32, c: Vec2) -> Rect {
    let min = pane.center() - c * z / ppp;
    let min = Pos2::new((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
    Rect::from_min_size(min, isz(img) * z / ppp)
}

// ---------------------------------------------------------------- difference

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffStats {
    /// Mean absolute difference per channel (0–255).
    pub mean: f64,
    pub max: u8,
    pub mse: f64,
    /// Peak signal-to-noise ratio in dB (infinite for identical images).
    pub psnr: f64,
    /// Percentage of pixels where any channel differs by more than 2.
    pub changed_pct: f64,
}

pub fn psnr_from_mse(mse: f64) -> f64 {
    if mse <= 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// |A − B| per RGB channel (images must have equal dimensions) and statistics.
pub fn difference(a: &RgbaImage, b: &RgbaImage) -> (RgbaImage, DiffStats) {
    use rayon::prelude::*;
    assert_eq!(a.dimensions(), b.dimensions());
    let (w, h) = a.dimensions();
    let mut out = RgbaImage::new(w, h);
    let row = w as usize * 4;
    let rows_per = (65536 / row.max(1)).max(1);
    let chunk = row * rows_per;
    let parts: Vec<(u64, u64, u8, u64)> = out
        .par_chunks_mut(chunk)
        .zip(a.as_raw().par_chunks(chunk))
        .zip(b.as_raw().par_chunks(chunk))
        .map(|((o, pa), pb)| {
            let (mut sum, mut sq, mut mx, mut changed) = (0u64, 0u64, 0u8, 0u64);
            for ((o, x), y) in o.chunks_exact_mut(4).zip(pa.chunks_exact(4)).zip(pb.chunks_exact(4)) {
                let mut px_max = 0u8;
                for c in 0..3 {
                    let d = x[c].abs_diff(y[c]);
                    o[c] = d;
                    sum += d as u64;
                    sq += (d as u64) * (d as u64);
                    px_max = px_max.max(d);
                }
                o[3] = 255;
                mx = mx.max(px_max);
                if px_max > 2 {
                    changed += 1;
                }
            }
            (sum, sq, mx, changed)
        })
        .collect();
    let n = (w as u64 * h as u64).max(1);
    let (sum, sq, mx, changed) = parts.into_iter().fold((0u64, 0u64, 0u8, 0u64), |acc, p| (acc.0 + p.0, acc.1 + p.1, acc.2.max(p.2), acc.3 + p.3));
    let mse = sq as f64 / (n * 3) as f64;
    let stats = DiffStats { mean: sum as f64 / (n * 3) as f64, max: mx, mse, psnr: psnr_from_mse(mse), changed_pct: changed as f64 * 100.0 / n as f64 };
    (out, stats)
}

/// Multiply a difference image by `amp` (saturating) for display.
pub fn amplify(diff: &RgbaImage, amp: f32) -> RgbaImage {
    use rayon::prelude::*;
    let mut out = diff.clone();
    if (amp - 1.0).abs() < 1e-3 {
        return out;
    }
    let lut: Vec<u8> = (0..256).map(|v| ((v as f32 * amp).round().min(255.0)) as u8).collect();
    out.par_chunks_mut(4 * 16384).for_each(|c| {
        for px in c.chunks_exact_mut(4) {
            px[0] = lut[px[0] as usize];
            px[1] = lut[px[1] as usize];
            px[2] = lut[px[2] as usize];
        }
    });
    out
}

/// Pick the largest pyramid level with at most `max_px` pixels.
fn level_under(pyr: &[Arc<RgbaImage>], max_px: u64) -> Arc<RgbaImage> {
    for l in pyr {
        if l.width() as u64 * l.height() as u64 <= max_px {
            return l.clone();
        }
    }
    pyr.last().cloned().expect("pyramid has a level")
}

enum DiffMsg {
    Done { raw: Arc<RgbaImage>, stats: DiffStats, amp: f32, pyramid: Vec<Arc<RgbaImage>>, note: Option<String> },
    Amplified { amp: f32, pyramid: Vec<Arc<RgbaImage>> },
}

#[derive(Default)]
struct DiffState {
    key: Option<(PathBuf, PathBuf)>,
    raw: Option<Arc<RgbaImage>>,
    stats: Option<DiffStats>,
    gpu: Option<(f32, GpuImage)>,
    busy: Option<Receiver<DiffMsg>>,
    note: Option<String>,
}

// ---------------------------------------------------------------- panes

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpMode {
    SideBySide,
    Split,
    Flicker,
    Difference,
}

impl CmpMode {
    fn label(self) -> &'static str {
        match self {
            CmpMode::SideBySide => "Side by side",
            CmpMode::Split => "Split",
            CmpMode::Flicker => "Flicker",
            CmpMode::Difference => "Difference",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            CmpMode::SideBySide => I::COLUMNS,
            CmpMode::Split => I::SQUARE_SPLIT_HORIZONTAL,
            CmpMode::Flicker => I::SWAP,
            CmpMode::Difference => I::SUBTRACT_SQUARE,
        }
    }
    fn key(self) -> &'static str {
        match self {
            CmpMode::SideBySide => "B",
            CmpMode::Split => "W",
            CmpMode::Flicker => "F",
            CmpMode::Difference => "D",
        }
    }
}

struct Pane {
    path: PathBuf,
    loaded: Option<Arc<LoadedImage>>,
    gpu: Option<GpuImage>,
    error: Option<String>,
    hist: Option<Histogram>,
    hist_rx: Option<Receiver<Histogram>>,
}

impl Pane {
    fn dims(&self) -> Option<(u32, u32)> {
        self.gpu.as_ref().map(|g| (g.width, g.height))
    }
}

pub struct Compare {
    pub paths: Vec<PathBuf>,
    panes: Vec<Pane>,
    views: Vec<View>,
    mode: CmpMode,
    sync: bool,
    basis: Basis,
    selected: usize,
    show_hist: bool,
    split: f32,
    flicker_b: bool,
    flicker_auto: bool,
    flicker_secs: f32,
    last_flip: Instant,
    amp: f32,
    diff: DiffState,
    help: bool,
    exit: bool,
    last_area: Rect,
}

fn name_of(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn fmt_size(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

const LETTERS: [&str; 4] = ["A", "B", "C", "D"];

impl Compare {
    pub fn new(paths: Vec<PathBuf>, _core: &Core) -> Self {
        let paths: Vec<PathBuf> = paths.into_iter().take(4).collect();
        let panes = paths.iter().map(|p| Pane { path: p.clone(), loaded: None, gpu: None, error: None, hist: None, hist_rx: None }).collect();
        let views = vec![View::fitted(Basis::Relative); paths.len()];
        Compare {
            paths,
            panes,
            views,
            mode: CmpMode::SideBySide,
            sync: true,
            basis: Basis::Relative,
            selected: 0,
            show_hist: false,
            split: 0.5,
            flicker_b: false,
            flicker_auto: false,
            flicker_secs: 0.5,
            last_flip: Instant::now(),
            amp: 4.0,
            diff: DiffState::default(),
            help: false,
            exit: false,
            last_area: Rect::NOTHING,
        }
    }

    fn pair_mode(&self) -> bool {
        self.mode != CmpMode::SideBySide && self.panes.len() == 2
    }

    fn set_mode(&mut self, m: CmpMode) {
        if m != CmpMode::SideBySide && self.panes.len() != 2 {
            return;
        }
        self.mode = m;
        self.last_flip = Instant::now();
    }

    // ------------------------------------------------------------ loading

    fn update_loading(&mut self, core: &mut Core) {
        let mut keep = Vec::new();
        for pane in &mut self.panes {
            if pane.gpu.is_none() && pane.error.is_none() {
                match core.loader.state(&pane.path) {
                    Some(LoadState::Ready(_)) => {
                        if let Some(img) = core.loader.get_ready(&pane.path) {
                            pane.gpu = Some(GpuImage::new(&core.ctx, &format!("cmp:{}", pane.path.display()), img.pyramid.clone()));
                            pane.loaded = Some(img);
                        }
                    }
                    Some(LoadState::Failed(e)) => pane.error = Some(e.clone()),
                    _ => {
                        core.loader.request(&pane.path, 0, None);
                        keep.push(pane.path.clone());
                    }
                }
            }
            if self.show_hist && pane.hist.is_none() && pane.hist_rx.is_none() {
                if let Some(l) = &pane.loaded {
                    let img = l.decoded.image.clone();
                    let (tx, rx) = unbounded();
                    let ctx = core.ctx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(Histogram::compute(&img));
                        ctx.request_repaint();
                    });
                    pane.hist_rx = Some(rx);
                }
            }
            if let Some(rx) = &pane.hist_rx {
                if let Ok(h) = rx.try_recv() {
                    pane.hist = Some(h);
                    pane.hist_rx = None;
                }
            }
        }
        if !keep.is_empty() {
            core.loader.retain_queued(&keep);
        }
    }

    fn update_diff(&mut self, ctx: &egui::Context) {
        if self.mode != CmpMode::Difference || self.panes.len() != 2 {
            return;
        }
        let (Some(a), Some(b)) = (self.panes[0].loaded.clone(), self.panes[1].loaded.clone()) else { return };
        let key = (self.panes[0].path.clone(), self.panes[1].path.clone());
        if self.diff.key.as_ref() != Some(&key) {
            self.diff = DiffState { key: Some(key), ..Default::default() };
            let (tx, rx) = unbounded();
            let amp = self.amp;
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let ia = level_under(&a.pyramid, 40_000_000);
                    let mut notes = Vec::new();
                    if ia.width() != a.decoded.image.width() {
                        notes.push(format!("computed at {}×{}", ia.width(), ia.height()));
                    }
                    let ib = if b.decoded.image.dimensions() == ia.dimensions() {
                        b.decoded.image.clone()
                    } else {
                        // Use the smallest level that is still at least as large, then resample.
                        let src = b.pyramid.iter().rev().find(|l| l.width() >= ia.width() && l.height() >= ia.height()).cloned().unwrap_or_else(|| b.pyramid[0].clone());
                        if b.decoded.image.dimensions() != a.decoded.image.dimensions() {
                            notes.push("B resampled to A's size".to_string());
                        }
                        if src.dimensions() == ia.dimensions() {
                            src
                        } else {
                            Arc::new(crate::imaging::resize_exact(&src, ia.width(), ia.height(), crate::imaging::Filter::Bilinear))
                        }
                    };
                    let (raw, stats) = difference(&ia, &ib);
                    let raw = Arc::new(raw);
                    let pyramid = render::build_pyramid(Arc::new(amplify(&raw, amp)));
                    DiffMsg::Done { raw, stats, amp, pyramid, note: if notes.is_empty() { None } else { Some(notes.join(" · ")) } }
                }));
                if let Ok(msg) = res {
                    let _ = tx.send(msg);
                }
                ctx2.request_repaint();
            });
            self.diff.busy = Some(rx);
        }
        if let Some(rx) = &self.diff.busy {
            match rx.try_recv() {
                Ok(DiffMsg::Done { raw, stats, amp, pyramid, note }) => {
                    self.diff.raw = Some(raw);
                    self.diff.stats = Some(stats);
                    self.diff.note = note;
                    self.diff.gpu = Some((amp, GpuImage::new(ctx, "cmp:diff", pyramid)));
                    self.diff.busy = None;
                }
                Ok(DiffMsg::Amplified { amp, pyramid }) => {
                    self.diff.gpu = Some((amp, GpuImage::new(ctx, "cmp:diff", pyramid)));
                    self.diff.busy = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.diff.busy = None;
                    self.diff.note = Some("Could not compute the difference".into());
                }
                Err(_) => {}
            }
        }
        // Re-amplify when the slider moved.
        if self.diff.busy.is_none() {
            if let (Some(raw), Some((amp, _))) = (&self.diff.raw, &self.diff.gpu) {
                if (*amp - self.amp).abs() > 1e-3 {
                    let raw = raw.clone();
                    let amp = self.amp;
                    let (tx, rx) = unbounded();
                    let ctx2 = ctx.clone();
                    std::thread::spawn(move || {
                        let pyramid = render::build_pyramid(Arc::new(amplify(&raw, amp)));
                        let _ = tx.send(DiffMsg::Amplified { amp, pyramid });
                        ctx2.request_repaint();
                    });
                    self.diff.busy = Some(rx);
                }
            }
        }
    }

    // ------------------------------------------------------------ view ops

    fn fit_ref(&self, rects: &[Rect], ppp: f32) -> f32 {
        let mut f = f32::INFINITY;
        for (i, p) in self.panes.iter().enumerate() {
            if let (Some(d), Some(r)) = (p.dims(), rects.get(i).or(rects.first())) {
                f = f.min(fit_of(d, r.size() * ppp));
            }
        }
        if f.is_finite() {
            f
        } else {
            1.0
        }
    }

    /// Apply `op` computed on pane `src` to it (and to all panes when synced).
    fn apply(&mut self, src: usize, op: impl Fn(&View) -> View) {
        let v = op(&self.views[src]);
        if self.sync {
            for w in &mut self.views {
                *w = v;
            }
        } else {
            self.views[src] = v;
        }
    }

    fn fit_all(&mut self) {
        let b = self.basis;
        for v in &mut self.views {
            *v = View::fitted(b);
        }
    }

    fn set_basis(&mut self, b: Basis) {
        if b != self.basis {
            self.basis = b;
            self.fit_all();
        }
    }

    /// Rect each pane's image is laid out in (single shared rect for pair modes).
    fn layout(&self, area: Rect) -> Vec<Rect> {
        if !area.is_positive() {
            return Vec::new();
        }
        if self.pair_mode() {
            vec![area; self.panes.len()]
        } else {
            pane_rects(self.panes.len(), area, 1.0)
        }
    }

    fn effective(&self, i: usize, rects: &[Rect], ppp: f32, fit_ref: f32) -> Option<(f32, Vec2, (u32, u32))> {
        let d = self.panes.get(i)?.dims()?;
        let r = rects.get(i)?;
        let (z, c) = pane_transform(&self.views[i], self.basis, d, r.size() * ppp, fit_ref);
        Some((z, c, d))
    }

    fn zoom_step(&mut self, src: usize, rects: &[Rect], ppp: f32, factor: f32, anchor_pt: Option<Pos2>) {
        let (Some(d), Some(&r)) = (self.panes.get(src).and_then(|p| p.dims()), rects.get(src)) else { return };
        let fr = self.fit_ref(rects, ppp);
        let basis = self.basis;
        let anchor = anchor_pt.map(|a| (a - r.center()) * ppp).unwrap_or(Vec2::ZERO);
        self.apply(src, |v| zoom_at(v, basis, d, r.size() * ppp, fr, factor, anchor));
    }

    fn actual_size(&mut self, rects: &[Rect], ppp: f32) {
        let src = self.selected.min(self.panes.len().saturating_sub(1));
        let Some((z, _, _)) = self.effective(src, rects, ppp, self.fit_ref(rects, ppp)) else { return };
        self.zoom_step(src, rects, ppp, 1.0 / z, None);
    }

    fn pan(&mut self, src: usize, rects: &[Rect], ppp: f32, delta_pt: Vec2) {
        let (Some(d), Some(&r)) = (self.panes.get(src).and_then(|p| p.dims()), rects.get(src)) else { return };
        let fr = self.fit_ref(rects, ppp);
        let basis = self.basis;
        self.apply(src, |v| pan_by(v, basis, d, r.size() * ppp, fr, delta_pt * ppp));
    }

    fn remove_pane(&mut self, i: usize) {
        if self.panes.len() <= 2 || i >= self.panes.len() {
            return;
        }
        self.panes.remove(i);
        self.views.remove(i);
        self.paths.remove(i);
        self.selected = self.selected.min(self.panes.len() - 1);
        if self.panes.len() != 2 {
            self.mode = CmpMode::SideBySide;
        }
    }

    fn swap_ab(&mut self) {
        if self.panes.len() >= 2 {
            self.panes.swap(0, 1);
            self.views.swap(0, 1);
            self.paths.swap(0, 1);
        }
    }

    // ------------------------------------------------------------ input

    fn keys(&mut self, ui: &Ui, rects: &[Rect], ppp: f32) {
        let take = |k: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if take(Key::Escape) {
            if self.help {
                self.help = false;
            } else if !egui::Popup::is_any_open(ui.ctx()) {
                self.exit = true;
            }
        }
        if take(Key::Tab) || take(Key::Space) {
            if self.mode == CmpMode::Flicker {
                self.flicker_b = !self.flicker_b;
                self.last_flip = Instant::now();
            } else if self.mode == CmpMode::Split {
                self.split = if self.split < 0.5 { 1.0 } else { 0.0 };
            }
        }
        let src = self.selected.min(self.panes.len().saturating_sub(1));
        if take(Key::Plus) || take(Key::Equals) {
            self.zoom_step(src, rects, ppp, 1.25, None);
        }
        if take(Key::Minus) {
            self.zoom_step(src, rects, ppp, 0.8, None);
        }
        if take(Key::Num0) || take(Key::Slash) {
            self.fit_all();
        }
        if take(Key::Num1) {
            self.actual_size(rects, ppp);
        }
        if let Some(r) = rects.get(src) {
            let step = r.size() * 0.1;
            if take(Key::ArrowLeft) {
                self.pan(src, rects, ppp, Vec2::new(step.x, 0.0));
            }
            if take(Key::ArrowRight) {
                self.pan(src, rects, ppp, Vec2::new(-step.x, 0.0));
            }
            if take(Key::ArrowUp) {
                self.pan(src, rects, ppp, Vec2::new(0.0, step.y));
            }
            if take(Key::ArrowDown) {
                self.pan(src, rects, ppp, Vec2::new(0.0, -step.y));
            }
        }
        for (k, m) in [(Key::B, CmpMode::SideBySide), (Key::W, CmpMode::Split), (Key::F, CmpMode::Flicker), (Key::D, CmpMode::Difference)] {
            if take(k) {
                self.set_mode(m);
            }
        }
        if take(Key::H) {
            self.show_hist = !self.show_hist;
        }
        if take(Key::L) {
            self.sync = !self.sync;
        }
        if take(Key::X) {
            self.swap_ab();
        }
        if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "?"))) {
            self.help = !self.help;
        }
    }

    // ------------------------------------------------------------ UI

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let p = core.pal;
        self.update_loading(core);
        self.update_diff(ui.ctx());
        let ppp = ui.ctx().pixels_per_point();

        egui::Panel::top("compare-toolbar")
            .resizable(false)
            .exact_size(46.0)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| self.toolbar(ui, core));

        let area = ui.available_rect_before_wrap();
        self.last_area = area;
        let bgc = core.settings.viewer_background;
        ui.painter().rect_filled(area, 0.0, Color32::from_rgb(bgc[0], bgc[1], bgc[2]));
        ui.painter().line_segment([area.left_top(), area.right_top()], Stroke::new(1.0, p.border));
        let rects = self.layout(area);
        self.keys(ui, &rects, ppp);
        if self.exit {
            return false;
        }
        let rects = self.layout(area);
        if rects.len() != self.panes.len() {
            return true;
        }

        if self.pair_mode() {
            self.pair_view(ui, core, area, &rects, ppp);
        } else {
            self.side_by_side(ui, core, &rects, ppp);
        }
        if self.help {
            self.help_overlay(ui, area);
        }
        !self.exit
    }

    fn view_input(&mut self, ui: &Ui, resp: &egui::Response, src: usize, rects: &[Rect], ppp: f32) {
        if resp.double_clicked() {
            self.fit_all();
        } else if resp.clicked() {
            self.selected = src;
        }
        if resp.dragged_by(egui::PointerButton::Primary) || resp.dragged_by(egui::PointerButton::Middle) {
            let d = resp.drag_delta();
            if d != Vec2::ZERO {
                self.pan(src, rects, ppp, d);
            }
        }
        if resp.hovered() {
            let wd = ui.input(super::wheel_delta);
            if wd != 0.0 {
                let factor = (1.0 + 0.0015 * wd.abs()).min(1.6);
                let factor = if wd > 0.0 { factor } else { 1.0 / factor };
                let anchor = ui.input(|i| i.pointer.hover_pos());
                self.zoom_step(src, rects, ppp, factor, anchor);
            }
            let pinch = ui.input(|i| i.zoom_delta());
            if wd == 0.0 && (pinch - 1.0).abs() > 1e-4 {
                let anchor = ui.input(|i| i.pointer.hover_pos());
                self.zoom_step(src, rects, ppp, pinch, anchor);
            }
        }
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }

    fn paint_image(&mut self, ui: &Ui, core: &Core, i: usize, rect: Rect, clip: Rect, rects: &[Rect], ppp: f32, fit_ref: f32) -> Option<f32> {
        let p = core.pal;
        let (z, c, d) = self.effective(i, rects, ppp, fit_ref)?;
        let dest = dest_rect(rect, ppp, d, z, c);
        let painter = ui.painter().with_clip_rect(clip);
        let pane = &mut self.panes[i];
        if pane.loaded.as_ref().is_some_and(|l| l.decoded.has_alpha) && core.settings.checkerboard_alpha {
            render::paint_checkerboard(&painter, dest.intersect(clip), 10.0, p.dark);
        }
        let nearest = !core.settings.smooth_zoom_in && z >= 2.0;
        if let Some(g) = &mut pane.gpu {
            g.paint(ui.ctx(), &painter, dest, clip, z, nearest, Color32::WHITE);
        }
        Some(z)
    }

    fn placeholder(&self, ui: &Ui, core: &Core, i: usize, rect: Rect) {
        let p = core.pal;
        let pane = &self.panes[i];
        if let Some(err) = &pane.error {
            ui.painter().text(rect.center() - Vec2::new(0.0, 26.0), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(40.0), p.faint);
            ui.painter().text(rect.center() + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, name_of(&pane.path), FontId::proportional(14.0), p.text);
            let g = ui.painter().layout(err.clone(), FontId::proportional(12.0), p.muted, (rect.width() - 40.0).clamp(40.0, 520.0));
            ui.painter().galley(rect.center() + Vec2::new(-g.size().x / 2.0, 30.0), g, p.muted);
        } else if pane.gpu.is_none() {
            let r = Rect::from_center_size(rect.center(), Vec2::splat(28.0));
            egui::Spinner::new().size(28.0).paint_at(ui, r);
        }
    }

    fn pill(ui: &Ui, pos: Pos2, anchor: Align2, text: &str, badge: Option<(&str, Color32)>) -> Rect {
        let font = FontId::proportional(12.0);
        let g = ui.painter().layout_no_wrap(text.to_string(), font.clone(), Color32::from_white_alpha(235));
        let badge_w = if badge.is_some() { 22.0 } else { 0.0 };
        let size = Vec2::new(g.size().x + 20.0 + badge_w, 24.0);
        let r = anchor.anchor_size(pos, size);
        ui.painter().rect_filled(r, CornerRadius::same(12), Color32::from_black_alpha(150));
        let mut x = r.min.x + 10.0;
        if let Some((b, col)) = badge {
            let br = Rect::from_center_size(Pos2::new(r.min.x + 14.0, r.center().y), Vec2::splat(18.0));
            ui.painter().rect_filled(br, CornerRadius::same(9), col);
            ui.painter().text(br.center(), Align2::CENTER_CENTER, b, super::theme::semibold(11.0), Color32::WHITE);
            x += badge_w - 2.0;
        }
        ui.painter().galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g, Color32::WHITE);
        r
    }

    fn caption_text(&self, i: usize, zoom: Option<f32>) -> String {
        let pane = &self.panes[i];
        let mut parts = vec![name_of(&pane.path)];
        if let Some((w, h)) = pane.dims() {
            parts.push(format!("{w} × {h}"));
        }
        if let Some(l) = &pane.loaded {
            parts.push(fmt_size(l.file_size));
        }
        if let Some(z) = zoom {
            parts.push(format!("{:.0}%", z * 100.0));
        }
        parts.join("  ·  ")
    }

    fn badge_color(core: &Core, i: usize) -> Color32 {
        [core.pal.accent, Color32::from_rgb(232, 120, 60), Color32::from_rgb(80, 180, 120), Color32::from_rgb(170, 110, 230)][i % 4]
    }

    fn side_by_side(&mut self, ui: &mut Ui, core: &mut Core, rects: &[Rect], ppp: f32) {
        let p = core.pal;
        let fit_ref = self.fit_ref(rects, ppp);
        let mut remove = None;
        for i in 0..self.panes.len() {
            let rect = rects[i];
            let resp = ui.interact(rect, ui.scope_id().with(("cmp-pane", i)), Sense::click_and_drag());
            let zoom = self.paint_image(ui, core, i, rect, rect, rects, ppp, fit_ref);
            if zoom.is_none() {
                self.placeholder(ui, core, i, rect);
            }
            self.view_input(ui, &resp, i, rects, ppp);
            // Caption pill.
            let text = self.caption_text(i, zoom);
            Self::pill(ui, rect.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &text, Some((LETTERS[i], Self::badge_color(core, i))));
            if self.show_hist {
                self.paint_hist(ui, core, i, rect);
            }
            if self.panes.len() > 2 {
                let br = Rect::from_min_size(Pos2::new(rect.max.x - 34.0, rect.min.y + 8.0), Vec2::splat(26.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(br));
                child.painter().rect_filled(br, CornerRadius::same(13), Color32::from_black_alpha(140));
                child.visuals_mut().override_text_color = Some(Color32::WHITE);
                if widgets::icon_button_sized(&mut child, I::X, "Remove from comparison", false, 26.0, 14.0).clicked() {
                    remove = Some(i);
                }
            }
            if self.selected == i && self.panes.len() > 1 {
                ui.painter().rect_stroke(rect.shrink(1.0), CornerRadius::ZERO, Stroke::new(2.0, p.accent.gamma_multiply(0.8)), egui::StrokeKind::Inside);
            }
        }
        // Thin separators.
        for (i, r) in rects.iter().enumerate() {
            if i > 0 {
                let prev = rects[i - 1];
                if (prev.min.y - r.min.y).abs() < 0.5 {
                    let x = (prev.max.x + r.min.x) / 2.0;
                    ui.painter().line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], Stroke::new(1.0, p.border));
                } else {
                    let y = (prev.max.y.min(r.min.y) + r.min.y) / 2.0;
                    let full = self.last_area;
                    ui.painter().line_segment([Pos2::new(full.min.x, y), Pos2::new(full.max.x, y)], Stroke::new(1.0, p.border));
                }
            }
        }
        if let Some(i) = remove {
            self.remove_pane(i);
        }
    }

    fn paint_hist(&self, ui: &Ui, _core: &Core, i: usize, rect: Rect) {
        let r = Rect::from_min_size(Pos2::new(rect.max.x - 222.0, rect.max.y - 104.0), Vec2::new(210.0, 92.0));
        if r.min.x < rect.min.x + 4.0 {
            return;
        }
        match &self.panes[i].hist {
            Some(h) => {
                ui.painter().rect_filled(r.expand(4.0), CornerRadius::same(8), Color32::from_black_alpha(120));
                h.paint(ui.painter(), r, true);
            }
            None if self.panes[i].gpu.is_some() => {
                egui::Spinner::new().size(16.0).paint_at(ui, Rect::from_center_size(r.center(), Vec2::splat(16.0)));
            }
            None => {}
        }
    }

    fn pair_view(&mut self, ui: &mut Ui, core: &mut Core, area: Rect, rects: &[Rect], ppp: f32) {
        let p = core.pal;
        let fit_ref = self.fit_ref(rects, ppp);
        let resp = ui.interact(area, ui.scope_id().with("cmp-pair"), Sense::click_and_drag());
        match self.mode {
            CmpMode::Split => {
                let x = area.min.x + area.width() * self.split;
                let left = Rect::from_min_max(area.min, Pos2::new(x, area.max.y));
                let right = Rect::from_min_max(Pos2::new(x, area.min.y), area.max);
                let za = self.paint_image(ui, core, 0, area, left, rects, ppp, fit_ref);
                let zb = self.paint_image(ui, core, 1, area, right, rects, ppp, fit_ref);
                if za.is_none() {
                    self.placeholder(ui, core, 0, Rect::from_min_max(area.min, Pos2::new(area.center().x, area.max.y)));
                }
                if zb.is_none() {
                    self.placeholder(ui, core, 1, Rect::from_min_max(Pos2::new(area.center().x, area.min.y), area.max));
                }
                // Divider + handle (registered after the view so it wins the drag).
                let handle = Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(14.0, area.height()));
                let hresp = ui.interact(handle, ui.scope_id().with("cmp-split-handle"), Sense::drag());
                if hresp.hovered() || hresp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if hresp.dragged() {
                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                        self.split = ((pos.x - area.min.x) / area.width()).clamp(0.0, 1.0);
                    }
                }
                let col = if hresp.hovered() || hresp.dragged() { p.accent } else { Color32::from_white_alpha(220) };
                ui.painter().line_segment([Pos2::new(x, area.min.y), Pos2::new(x, area.max.y)], Stroke::new(2.0, col));
                let knob = Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(26.0, 40.0));
                ui.painter().rect_filled(knob, CornerRadius::same(13), Color32::from_black_alpha(170));
                ui.painter().rect_stroke(knob, CornerRadius::same(13), Stroke::new(1.5, col), egui::StrokeKind::Inside);
                ui.painter().text(knob.center(), Align2::CENTER_CENTER, I::ARROWS_HORIZONTAL, FontId::proportional(15.0), Color32::WHITE);
                let ta = self.caption_text(0, za);
                let tb = self.caption_text(1, zb);
                Self::pill(ui, area.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &ta, Some(("A", Self::badge_color(core, 0))));
                Self::pill(ui, Pos2::new(area.max.x - 10.0, area.min.y + 10.0), Align2::RIGHT_TOP, &tb, Some(("B", Self::badge_color(core, 1))));
                if self.show_hist {
                    self.paint_hist(ui, core, 1, area);
                    let left_hist = Rect::from_min_max(area.min, Pos2::new(area.min.x + 240.0, area.max.y));
                    self.paint_hist(ui, core, 0, left_hist);
                }
                if !hresp.dragged() {
                    self.view_input(ui, &resp, 0, rects, ppp);
                }
            }
            CmpMode::Flicker => {
                if self.flicker_auto {
                    let el = self.last_flip.elapsed().as_secs_f32();
                    if el >= self.flicker_secs {
                        self.flicker_b = !self.flicker_b;
                        self.last_flip = Instant::now();
                    }
                    let left = (self.flicker_secs - self.last_flip.elapsed().as_secs_f32()).max(0.01);
                    ui.ctx().request_repaint_after(Duration::from_secs_f32(left));
                }
                let i = if self.flicker_b { 1 } else { 0 };
                let z = self.paint_image(ui, core, i, area, area, rects, ppp, fit_ref);
                if z.is_none() {
                    self.placeholder(ui, core, i, area);
                }
                let t = self.caption_text(i, z);
                Self::pill(ui, area.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &t, Some((LETTERS[i], Self::badge_color(core, i))));
                let hint = if self.flicker_auto { format!("Auto flicker every {:.1} s", self.flicker_secs) } else { "Tab / Space to switch".to_string() };
                Self::pill(ui, Pos2::new(area.center().x, area.max.y - 14.0), Align2::CENTER_BOTTOM, &hint, None);
                if self.show_hist {
                    self.paint_hist(ui, core, i, area);
                }
                self.view_input(ui, &resp, i, rects, ppp);
            }
            CmpMode::Difference => {
                let mut zshow = None;
                if let (Some((z, c, d)), Some((_, g))) = (self.effective(0, rects, ppp, fit_ref), &mut self.diff.gpu) {
                    let dest = dest_rect(area, ppp, d, z, c);
                    let painter = ui.painter().with_clip_rect(area);
                    // The diff may be computed at a reduced level: scale the zoom accordingly.
                    let scale = d.0 as f32 / g.width.max(1) as f32;
                    g.paint(ui.ctx(), &painter, dest, area, z * scale, z >= 2.0, Color32::WHITE);
                    zshow = Some(z);
                }
                let busy = self.diff.busy.is_some();
                if self.diff.gpu.is_none() {
                    if self.panes.iter().any(|p| p.error.is_some()) {
                        let i = if self.panes[0].error.is_some() { 0 } else { 1 };
                        self.placeholder(ui, core, i, area);
                    } else {
                        egui::Spinner::new().size(28.0).paint_at(ui, Rect::from_center_size(area.center(), Vec2::splat(28.0)));
                        ui.painter().text(area.center() + Vec2::new(0.0, 30.0), Align2::CENTER_TOP, "Computing difference…", FontId::proportional(13.0), p.muted);
                    }
                } else if busy {
                    egui::Spinner::new().size(16.0).paint_at(ui, Rect::from_center_size(area.right_top() + Vec2::new(-22.0, 22.0), Vec2::splat(16.0)));
                }
                let mut text = format!("|A − B|  ×{:.0}", self.amp);
                if let Some(z) = zshow {
                    text.push_str(&format!("  ·  {:.0}%", z * 100.0));
                }
                Self::pill(ui, area.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &text, None);
                if let Some(s) = self.diff.stats {
                    let psnr = if s.psnr.is_infinite() { "∞ (identical)".to_string() } else { format!("{:.2} dB", s.psnr) };
                    let mut st = format!("PSNR {psnr}  ·  mean Δ {:.2}  ·  max Δ {}  ·  {:.2}% pixels differ", s.mean, s.max, s.changed_pct);
                    if let Some(n) = &self.diff.note {
                        st.push_str(&format!("  ·  {n}"));
                    }
                    Self::pill(ui, Pos2::new(area.center().x, area.max.y - 14.0), Align2::CENTER_BOTTOM, &st, None);
                }
                let src = 0;
                self.view_input(ui, &resp, src, rects, ppp);
            }
            CmpMode::SideBySide => {}
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back  (Esc)", false).clicked() {
                self.exit = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Compare").font(super::theme::semibold(14.0)).color(p.text));
            ui.label(RichText::new(format!("{} images", self.panes.len())).size(12.0).color(p.muted));
            widgets::separator_v(ui);
            let pair = self.panes.len() == 2;
            for m in [CmpMode::SideBySide, CmpMode::Split, CmpMode::Flicker, CmpMode::Difference] {
                let enabled = m == CmpMode::SideBySide || pair;
                let tip = if enabled { format!("{}  ({})", m.label(), m.key()) } else { format!("{} — needs exactly 2 images", m.label()) };
                let r = ui.add_enabled(enabled, egui::Button::new(format!("{}  {}", m.icon(), m.label())).selected(self.mode == m).corner_radius(CornerRadius::same(6)));
                if r.on_hover_text(tip).clicked() {
                    self.set_mode(m);
                }
            }
            widgets::separator_v(ui);
            let (icon, tip) = if self.sync { (I::LINK, "Zoom & pan synchronized  (L)") } else { (I::LINK_BREAK, "Zoom & pan independent  (L)") };
            if widgets::icon_button(ui, icon, tip, self.sync).clicked() {
                self.sync = !self.sync;
                if self.sync {
                    let v = self.views[self.selected.min(self.views.len() - 1)];
                    for w in &mut self.views {
                        *w = v;
                    }
                }
            }
            let mut basis = self.basis;
            egui::ComboBox::from_id_salt("cmp-basis")
                .width(118.0)
                .selected_text(match basis {
                    Basis::Relative => "Match framing",
                    Basis::Pixel => "Match pixels",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut basis, Basis::Relative, "Match framing").on_hover_text("Same relative position/zoom: images of different sizes stay aligned");
                    ui.selectable_value(&mut basis, Basis::Pixel, "Match pixels").on_hover_text("Same pixel scale: 100% in one pane is 100% in all");
                });
            self.set_basis(basis);
            widgets::separator_v(ui);
            let rects = self.layout(self.last_area);
            let ppp = ui.ctx().pixels_per_point();
            let src = self.selected.min(self.panes.len().saturating_sub(1));
            if widgets::icon_button(ui, I::MAGNIFYING_GLASS_MINUS, "Zoom out  (−)", false).clicked() && !rects.is_empty() {
                self.zoom_step(src, &rects, ppp, 0.8, None);
            }
            let z = if rects.is_empty() { None } else { self.effective(src, &rects, ppp, self.fit_ref(&rects, ppp)).map(|e| e.0) };
            let zl = ui.add_sized([52.0, 26.0], egui::Button::new(RichText::new(z.map(|z| format!("{:.0}%", z * 100.0)).unwrap_or_else(|| "—".into())).size(12.0)).frame(false));
            if zl.on_hover_text("Click: 100%  (1)").clicked() && !rects.is_empty() {
                self.actual_size(&rects, ppp);
            }
            if widgets::icon_button(ui, I::MAGNIFYING_GLASS_PLUS, "Zoom in  (+)", false).clicked() && !rects.is_empty() {
                self.zoom_step(src, &rects, ppp, 1.25, None);
            }
            let fitted = self.views.iter().all(|v| v.fit);
            if widgets::icon_button(ui, I::ARROWS_IN, "Fit  (0 / double-click)", fitted).clicked() {
                self.fit_all();
            }
            widgets::separator_v(ui);
            if widgets::icon_button(ui, I::CHART_BAR, "Histograms  (H)", self.show_hist).clicked() {
                self.show_hist = !self.show_hist;
            }
            if pair && widgets::icon_button(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B  (X)", false).clicked() {
                self.swap_ab();
            }
            // Mode-specific controls.
            match self.mode {
                CmpMode::Flicker if pair => {
                    widgets::separator_v(ui);
                    ui.toggle_value(&mut self.flicker_auto, "Auto");
                    if self.flicker_auto {
                        ui.spacing_mut().slider_width = 90.0;
                        ui.add(egui::Slider::new(&mut self.flicker_secs, 0.2..=2.0).suffix(" s").fixed_decimals(1));
                    }
                }
                CmpMode::Difference if pair => {
                    widgets::separator_v(ui);
                    ui.label(RichText::new("Amplify").size(12.0).color(p.muted));
                    ui.spacing_mut().slider_width = 110.0;
                    ui.add(egui::Slider::new(&mut self.amp, 1.0..=50.0).logarithmic(true).prefix("×").fixed_decimals(0));
                }
                CmpMode::Split if pair => {
                    widgets::separator_v(ui);
                    ui.label(RichText::new(format!("Divider {:.0}%", self.split * 100.0)).size(12.0).color(p.muted));
                }
                _ => {}
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, I::QUESTION, "Shortcuts  (?)", self.help).clicked() {
                    self.help = !self.help;
                }
            });
        });
    }

    fn help_overlay(&self, ui: &Ui, area: Rect) {
        let rows: [(&str, &str); 12] = [
            ("Wheel / pinch", "Zoom at cursor (all panes when linked)"),
            ("Drag", "Pan"),
            ("Double-click / 0", "Fit"),
            ("1", "100% (selected pane)"),
            ("+ / −  arrows", "Zoom / pan"),
            ("B  W  F  D", "Side by side · Split · Flicker · Difference"),
            ("Tab / Space", "Flicker A ↔ B (Split: jump divider)"),
            ("X", "Swap A and B"),
            ("L", "Link / unlink zoom & pan"),
            ("H", "Histograms"),
            ("?", "This help"),
            ("Esc", "Back"),
        ];
        let r = Rect::from_center_size(area.center(), Vec2::new(460.0, 60.0 + rows.len() as f32 * 24.0));
        let painter = ui.painter();
        painter.rect_filled(r, CornerRadius::same(14), Color32::from_rgba_unmultiplied(18, 19, 22, 240));
        painter.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, Color32::from_white_alpha(26)), egui::StrokeKind::Inside);
        painter.text(r.min + Vec2::new(22.0, 24.0), Align2::LEFT_CENTER, "Compare shortcuts", super::theme::semibold(15.0), Color32::WHITE);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = r.min.y + 56.0 + i as f32 * 24.0;
            painter.text(Pos2::new(r.min.x + 22.0, y), Align2::LEFT_CENTER, *k, FontId::proportional(13.0), Color32::from_gray(150));
            painter.text(Pos2::new(r.min.x + 170.0, y), Align2::LEFT_CENTER, *v, FontId::proportional(13.0), Color32::from_gray(235));
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 40.0), Vec2::new(1201.0, 801.0))
    }

    #[test]
    fn layout_two_three_four() {
        let a = area();
        assert_eq!(pane_rects(1, a, 1.0), vec![a]);
        let r2 = pane_rects(2, a, 1.0);
        assert_eq!(r2.len(), 2);
        assert!((r2[0].width() - 600.0).abs() < 1e-3 && (r2[1].width() - 600.0).abs() < 1e-3);
        assert_eq!(r2[0].min, a.min);
        assert_eq!(r2[1].max, a.max);
        assert!((r2[1].min.x - r2[0].max.x - 1.0).abs() < 1e-3);
        let r3 = pane_rects(3, a, 1.0);
        assert_eq!(r3.len(), 3);
        for r in &r3 {
            assert!((r.width() - 399.6667).abs() < 1e-2);
            assert_eq!(r.height(), a.height());
        }
        assert!((r3[2].max.x - a.max.x).abs() < 1e-3);
        let r4 = pane_rects(4, a, 1.0);
        assert_eq!(r4.len(), 4);
        assert_eq!(r4[0].min, a.min);
        assert_eq!(r4[3].max, a.max);
        assert!((r4[0].height() - 400.0).abs() < 1e-3);
        assert!((r4[2].min.y - r4[0].max.y - 1.0).abs() < 1e-3);
        assert!((r4[1].min.y - r4[0].min.y).abs() < 1e-6);
        // No overlap.
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    let x = r4[i].intersect(r4[j]);
                    assert!(x.width() <= 0.0 || x.height() <= 0.0);
                }
            }
        }
    }

    #[test]
    fn relative_sync_keeps_images_aligned() {
        let pane = Vec2::new(1000.0, 800.0);
        let small = (1000, 500);
        let big = (4000, 2000);
        let v = View::fitted(Basis::Relative);
        let (zs, cs) = pane_transform(&v, Basis::Relative, small, pane, 1.0);
        let (zb, cb) = pane_transform(&v, Basis::Relative, big, pane, 1.0);
        assert!((zs - 1.0).abs() < 1e-6 && (zb - 0.25).abs() < 1e-6);
        assert_eq!(cs, Vec2::new(500.0, 250.0));
        assert_eq!(cb, Vec2::new(2000.0, 1000.0));
        // Zoom at an anchor on the small one; apply the same view to both.
        let anchor = Vec2::new(200.0, -100.0);
        let v2 = zoom_at(&v, Basis::Relative, small, pane, 1.0, 2.0, anchor);
        let (zs2, cs2) = pane_transform(&v2, Basis::Relative, small, pane, 1.0);
        let (zb2, cb2) = pane_transform(&v2, Basis::Relative, big, pane, 1.0);
        assert!((zs2 - 2.0).abs() < 1e-5);
        assert!((zb2 - 0.5).abs() < 1e-5);
        // The anchor maps to the same image point before and after (in the source pane).
        let before = cs + anchor / zs;
        let after = cs2 + anchor / zs2;
        assert!((before - after).length() < 1e-3);
        // Both panes show the same normalized point at their centre.
        assert!((cs2.x / 1000.0 - cb2.x / 4000.0).abs() < 1e-5);
        assert!((cs2.y / 500.0 - cb2.y / 2000.0).abs() < 1e-5);
    }

    #[test]
    fn pixel_sync_uses_same_scale() {
        let pane = Vec2::new(1000.0, 800.0);
        let a = (1000, 500);
        let b = (4000, 2000);
        let fit_ref = fit_of(a, pane).min(fit_of(b, pane));
        assert!((fit_ref - 0.25).abs() < 1e-6);
        let v = View::fitted(Basis::Pixel);
        assert_eq!(pane_transform(&v, Basis::Pixel, a, pane, fit_ref).0, fit_ref);
        let v2 = zoom_at(&v, Basis::Pixel, a, pane, fit_ref, 4.0, Vec2::ZERO);
        let (za, ca) = pane_transform(&v2, Basis::Pixel, a, pane, fit_ref);
        let (zb, cb) = pane_transform(&v2, Basis::Pixel, b, pane, fit_ref);
        assert!((za - 1.0).abs() < 1e-5 && (zb - 1.0).abs() < 1e-5);
        assert_eq!(ca, Vec2::new(500.0, 250.0));
        assert_eq!(cb, Vec2::new(2000.0, 1000.0));
        // Pan moves both by the same number of image pixels.
        let v3 = pan_by(&v2, Basis::Pixel, a, pane, fit_ref, Vec2::new(100.0, 0.0));
        let (_, ca3) = pane_transform(&v3, Basis::Pixel, a, pane, fit_ref);
        let (_, cb3) = pane_transform(&v3, Basis::Pixel, b, pane, fit_ref);
        assert!((ca3.x - 400.0).abs() < 1e-3 && (cb3.x - 1900.0).abs() < 1e-3);
    }

    #[test]
    fn pan_is_clamped() {
        let pane = Vec2::new(500.0, 500.0);
        let v = pan_by(&View::fitted(Basis::Relative), Basis::Relative, (100, 100), pane, 1.0, Vec2::new(1e6, -1e6));
        assert_eq!(v.center, Vec2::new(0.0, 1.0));
    }

    #[test]
    fn dest_rect_centres_image() {
        let pane = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        let d = dest_rect(pane, 1.0, (1000, 500), 1.0, Vec2::new(500.0, 250.0));
        assert_eq!(d, Rect::from_min_size(Pos2::new(0.0, 150.0), Vec2::new(1000.0, 500.0)));
        let d2 = dest_rect(pane, 2.0, (1000, 500), 1.0, Vec2::new(500.0, 250.0));
        assert_eq!(d2.size(), Vec2::new(500.0, 250.0));
    }

    fn solid(w: u32, h: u32, v: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(v))
    }

    #[test]
    fn difference_of_identical_is_zero() {
        let a = RgbaImage::from_fn(64, 33, |x, y| image::Rgba([(x * 3) as u8, (y * 7) as u8, 200, 255]));
        let (d, s) = difference(&a, &a);
        assert!(d.pixels().all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] == 255));
        assert_eq!(s.mse, 0.0);
        assert!(s.psnr.is_infinite());
        assert_eq!(s.max, 0);
        assert_eq!(s.changed_pct, 0.0);
    }

    #[test]
    fn difference_stats_and_psnr() {
        let a = solid(100, 50, [100, 100, 100, 255]);
        let b = solid(100, 50, [110, 90, 100, 255]);
        let (d, s) = difference(&a, &b);
        assert_eq!(d.get_pixel(3, 4).0, [10, 10, 0, 255]);
        assert!((s.mean - 20.0 / 3.0).abs() < 1e-9);
        assert!((s.mse - 200.0 / 3.0).abs() < 1e-9);
        let expect = 10.0 * (65025.0f64 / (200.0 / 3.0)).log10();
        assert!((s.psnr - expect).abs() < 1e-9);
        assert_eq!(s.max, 10);
        assert!((s.changed_pct - 100.0).abs() < 1e-9);
        // Half the image different.
        let mut c = a.clone();
        for y in 0..25 {
            for x in 0..100 {
                c.put_pixel(x, y, image::Rgba([0, 100, 100, 255]));
            }
        }
        let (_, s2) = difference(&a, &c);
        assert!((s2.changed_pct - 50.0).abs() < 1e-9);
        assert_eq!(s2.max, 100);
        assert!((psnr_from_mse(65025.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn amplify_saturates() {
        let mut d = solid(4, 4, [10, 100, 0, 255]);
        d.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        let a = amplify(&d, 4.0);
        assert_eq!(a.get_pixel(1, 1).0, [40, 255, 0, 255]);
        assert_eq!(a.get_pixel(0, 0).0, [4, 8, 12, 255]);
        assert_eq!(amplify(&d, 1.0), d);
    }
}
