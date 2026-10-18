//! Viewer mode: smooth zoom/pan on images of any size, magnifier, crop, live
//! preview editing, info panel, filmstrip and animation playback.

use super::commands::Command;
use super::icons as I;
use super::theme::Palette;
use super::widgets;
use crate::app::Core;
use crate::catalog;
use crate::edit::{EditSession, EditTool, Histogram, ToolUiCtx};
use crate::loader::{LoadState, LoadedImage};
use crate::render::{self, GpuImage};
use crate::settings::{FitMode, WheelAction};
use crate::thumbs::{Thumb, ThumbKey};
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum ViewerEvent {
    Command(Command),
    Close,
    /// The current file changed on disk (saved / rotated / deleted).
    FileChanged(PathBuf),
    IndexChanged(PathBuf),
}

struct Anim {
    path: PathBuf,
    frame: usize,
    next_at: Instant,
    playing: bool,
    /// One texture per frame when the whole animation is small enough
    /// (no re-upload on every loop), otherwise a single reused texture.
    frame_tex: Vec<Option<egui::TextureHandle>>,
    tex: Option<egui::TextureHandle>,
    shown_frame: usize,
}

/// Cache all frames of an animation as textures when it has at most this many pixels in total.
const ANIM_CACHE_PIXELS: u64 = 24_000_000;

/// The screen-sized working copy a tool previews on, prepared off the UI thread.
struct ToolBase {
    image: Arc<RgbaImage>,
    scale: f32,
    histogram: Histogram,
}

struct ActiveTool {
    tool: Box<dyn EditTool>,
    base: Option<ToolBase>,
    base_rx: Option<Receiver<ToolBase>>,
    version: u64,
    preview: Option<GpuImage>,
    preview_version: u64,
    rx: Option<Receiver<(u64, RgbaImage)>>,
    running: Option<u64>,
    final_rx: Option<Receiver<RgbaImage>>,
    show_original: bool,
    error: Option<String>,
}

impl ActiveTool {
    fn scale(&self) -> f32 {
        self.base.as_ref().map(|b| b.scale).unwrap_or(1.0)
    }
}

/// Identifies what a cached `GpuImage` was built from. Keying decoded images by
/// the pyramid allocation means a re-decode of the same path (lossless rotate,
/// external change) always gets a fresh texture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GpuKey {
    Decoded(usize),
    Edit(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

/// Which edge of `view` the pointer is within `margin` points of (nearest wins).
pub fn edge_at(view: Rect, pos: Pos2, margin: f32) -> Option<Edge> {
    if !view.is_positive() || pos.x < view.min.x - 1.0 || pos.x > view.max.x + 1.0 || pos.y < view.min.y - 1.0 || pos.y > view.max.y + 1.0 {
        return None;
    }
    let d = [(Edge::Top, pos.y - view.min.y), (Edge::Right, view.max.x - pos.x), (Edge::Bottom, view.max.y - pos.y), (Edge::Left, pos.x - view.min.x)];
    d.iter().filter(|(_, v)| *v <= margin).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(e, _)| *e)
}

/// Pop-out edge panel state (FastStone-style full-screen panels).
#[derive(Default)]
struct EdgeState {
    dwell: Option<(Edge, Instant)>,
    open: Option<Edge>,
    /// Last opened edge, kept while the panel fades out.
    shown: Option<Edge>,
    panel_rect: Option<Rect>,
}

const EDGE_MARGIN: f32 = 6.0;
const EDGE_DWELL: Duration = Duration::from_millis(250);

/// Wheel delta in points per mouse-wheel notch.
pub const WHEEL_NOTCH: f32 = 40.0;

/// Turns wheel / touchpad scrolling into whole navigation steps. A mouse steps
/// once per notch; a precision touchpad swipe steps once per gesture.
#[derive(Default, Debug)]
pub struct WheelNav {
    acc: f32,
    last: Option<f64>,
    latched: bool,
}

impl WheelNav {
    /// Feed this frame's vertical delta (points, + = up). Returns -1, 0 or +1.
    pub fn feed(&mut self, delta: f32, precise: bool, now: f64) -> i32 {
        if delta == 0.0 {
            return 0;
        }
        let idle = self.last.map(|t| now - t > 0.3).unwrap_or(true);
        if idle || (self.acc != 0.0 && self.acc.signum() != delta.signum()) {
            self.acc = 0.0;
            self.latched = false;
        }
        self.last = Some(now);
        if self.latched {
            return 0;
        }
        self.acc += delta;
        let threshold = if precise { WHEEL_NOTCH * 1.5 } else { WHEEL_NOTCH * 0.9 };
        if self.acc.abs() >= threshold {
            let s = self.acc.signum() as i32;
            if precise {
                // One image per touchpad swipe: wait for the gesture to end.
                self.latched = true;
                self.acc = 0.0;
            } else {
                self.acc = (self.acc - self.acc.signum() * WHEEL_NOTCH).clamp(-WHEEL_NOTCH * 0.5, WHEEL_NOTCH * 0.5);
            }
            return s;
        }
        0
    }
}

/// True for scroll events from a precision touchpad (smooth deltas) rather
/// than a notched mouse wheel. On Windows both arrive as line deltas; wheels
/// report whole notches.
pub fn is_precise_scroll(unit: egui::MouseWheelUnit, delta: Vec2) -> bool {
    match unit {
        egui::MouseWheelUnit::Point => true,
        egui::MouseWheelUnit::Line => {
            let frac = |v: f32| (v - v.round()).abs() > 0.01;
            delta.x != 0.0 || frac(delta.y) || (delta.y != 0.0 && delta.y.abs() < 1.0)
        }
        egui::MouseWheelUnit::Page => false,
    }
}

/// Scroll / gesture input of one frame, with ctrl-wheel and pinch kept apart
/// (egui's `zoom_delta()` folds ctrl+wheel in, which would count it twice).
#[derive(Default, Debug, Clone, Copy)]
pub struct ScrollInput {
    /// Vertical wheel delta in points (all sources).
    pub wheel: f32,
    /// Delta of precise (touchpad) events, both axes, in points.
    pub precise: Vec2,
    pub any_precise: bool,
    /// Pinch zoom factor from touch screens / gesture events (1 = none).
    pub pinch: f32,
    /// Two-finger touch-screen pan in points.
    pub touch_pan: Vec2,
}

pub fn scroll_input(i: &egui::InputState) -> ScrollInput {
    let mut s = ScrollInput { pinch: 1.0, ..Default::default() };
    let mut gesture_zoom = 1.0;
    for e in &i.raw.events {
        match e {
            egui::Event::MouseWheel { unit, delta, .. } => {
                let k = match unit {
                    egui::MouseWheelUnit::Point => 1.0,
                    egui::MouseWheelUnit::Line => WHEEL_NOTCH,
                    egui::MouseWheelUnit::Page => 400.0,
                };
                s.wheel += delta.y * k;
                if is_precise_scroll(*unit, *delta) {
                    s.any_precise = true;
                    s.precise += *delta * k;
                }
            }
            egui::Event::Zoom(f) => gesture_zoom *= *f,
            _ => {}
        }
    }
    match i.multi_touch() {
        Some(t) => {
            s.pinch = t.zoom_delta;
            s.touch_pan = t.translation_delta;
        }
        None => s.pinch = gesture_zoom,
    }
    s
}

/// Image point (pixels) for a normalized centre when the zoom ratio is locked.
pub fn locked_center(norm: Vec2, img: (u32, u32)) -> Vec2 {
    Vec2::new(norm.x.clamp(0.0, 1.0) * img.0 as f32, norm.y.clamp(0.0, 1.0) * img.1 as f32)
}

#[derive(Clone, Copy, PartialEq)]
enum CropHandle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
    New,
}

pub struct CropState {
    /// x0, y0, x1, y1 in image pixels.
    pub r: [f32; 4],
    pub aspect: Option<f32>,
    drag: Option<(CropHandle, Pos2, [f32; 4])>,
}

pub struct Viewer {
    pub list: Vec<PathBuf>,
    pub index: usize,
    pub zoom: f32,
    pub center: Vec2,
    pub auto_fit: Option<FitMode>,
    zoom_target: Option<(f32, Pos2)>,
    gpu: Option<(PathBuf, GpuKey, GpuImage)>,
    /// Pyramid of an edited image being built off the UI thread.
    gpu_build: Option<(PathBuf, GpuKey, Receiver<Vec<Arc<RgbaImage>>>)>,
    preview_tex: Option<(PathBuf, egui::TextureHandle)>,
    anim: Option<Anim>,
    pub edit: Option<EditSession>,
    tool: Option<ActiveTool>,
    pub crop: Option<CropState>,
    pub show_info: bool,
    pub show_histogram: bool,
    pub show_filmstrip: bool,
    pub magnifier_locked: bool,
    magnifier_active: bool,
    last_activity: Instant,
    last_view_rect: Rect,
    pub pending_nav: Option<isize>,
    pub confirm_discard: bool,
    hist_cache: crate::bg::BgSlot<(PathBuf, u64, usize), Histogram>,
    exif_cache: crate::bg::BgSlot<PathBuf, Option<crate::meta::ExifSummary>>,
    /// (size, mtime) per file so thumbnail lookups never stat() every frame.
    stat_cache: std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>)>,
    drag_pan: bool,
    /// Show the pop-out panels when the pointer dwells at a window edge.
    pub edge_panels: bool,
    /// Keep zoom and (normalized) position when stepping to another image.
    pub lock_zoom: bool,
    /// (zoom, normalized centre) to apply once the next image's size is known.
    pending_lock: Option<(f32, Vec2)>,
    /// Size of the image the current zoom/centre refer to.
    view_img: (u32, u32),
    edge: EdgeState,
    wheel_nav: WheelNav,
}

impl Viewer {
    pub fn new() -> Self {
        Viewer {
            list: Vec::new(),
            index: 0,
            zoom: 1.0,
            center: Vec2::ZERO,
            auto_fit: Some(FitMode::FitLarge),
            zoom_target: None,
            gpu: None,
            gpu_build: None,
            preview_tex: None,
            anim: None,
            edit: None,
            tool: None,
            crop: None,
            show_info: false,
            show_histogram: false,
            show_filmstrip: false,
            magnifier_locked: false,
            magnifier_active: false,
            last_activity: Instant::now(),
            last_view_rect: Rect::NOTHING,
            pending_nav: None,
            confirm_discard: false,
            hist_cache: Default::default(),
            exif_cache: Default::default(),
            stat_cache: Default::default(),
            drag_pan: false,
            edge_panels: true,
            lock_zoom: false,
            pending_lock: None,
            view_img: (0, 0),
            edge: EdgeState::default(),
            wheel_nav: WheelNav::default(),
        }
    }

    /// Toggle "lock zoom ratio" (keep zoom and position while browsing).
    /// Returns the new state.
    pub fn toggle_lock_zoom(&mut self) -> bool {
        self.lock_zoom = !self.lock_zoom;
        self.pending_lock = None;
        self.lock_zoom
    }

    /// What step()/go_to() do with the view: reset to `fit`, or keep the
    /// current zoom and normalized centre when the zoom is locked. A fitted
    /// view stays fitted (the fit mode is the "ratio" then).
    fn after_index_change(&mut self, fit: FitMode) {
        if self.lock_zoom && self.auto_fit.is_none() && self.view_img.0 > 0 && self.view_img.1 > 0 {
            let (zoom, norm) = match self.pending_lock {
                // Stepping again before the previous image loaded: keep the original lock.
                Some(l) => l,
                None => {
                    let z = self.zoom_target.map(|t| t.0).unwrap_or(self.zoom);
                    (z, Vec2::new(self.center.x / self.view_img.0 as f32, self.center.y / self.view_img.1 as f32))
                }
            };
            self.pending_lock = Some((sane_zoom(zoom), norm));
            self.zoom_target = None;
        } else {
            self.pending_lock = None;
            self.reset_view(fit);
        }
    }

    fn thumb_key(&mut self, path: &Path) -> ThumbKey {
        let (size, mtime) = *self.stat_cache.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::metadata(path).map(|m| (m.len(), m.modified().ok())).unwrap_or((0, None))
        });
        ThumbKey::new(path, size, mtime)
    }

    /// Drop cached file stats after the file changed on disk.
    pub fn forget_stat(&mut self, path: &Path) {
        self.stat_cache.remove(path);
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.list.get(self.index)
    }

    pub fn open(&mut self, list: Vec<PathBuf>, index: usize, fit: FitMode) {
        self.list = list;
        self.index = index.min(self.list.len().saturating_sub(1));
        self.reset_view(fit);
        self.edit = None;
        self.tool = None;
        self.crop = None;
    }

    pub fn reset_view(&mut self, fit: FitMode) {
        self.auto_fit = Some(fit);
        self.zoom_target = None;
    }

    pub fn is_dirty(&self) -> bool {
        self.edit.as_ref().map(|e| e.is_dirty()).unwrap_or(false)
    }

    pub fn has_tool(&self) -> bool {
        self.tool.is_some() || self.crop.is_some()
    }

    /// Navigate by `delta` images. Returns false if blocked by unsaved edits.
    pub fn step(&mut self, delta: isize, wrap: bool, fit: FitMode) -> bool {
        if self.list.is_empty() {
            return true;
        }
        if self.is_dirty() {
            self.pending_nav = Some(delta);
            self.confirm_discard = true;
            return false;
        }
        let n = self.list.len() as isize;
        let mut i = self.index as isize + delta;
        if wrap {
            i = i.rem_euclid(n);
        } else {
            i = i.clamp(0, n - 1);
        }
        if i as usize != self.index {
            self.index = i as usize;
            self.edit = None;
            self.tool = None;
            self.crop = None;
            self.after_index_change(fit);
        }
        true
    }

    pub fn go_to(&mut self, index: usize, fit: FitMode) {
        if index < self.list.len() && index != self.index && !self.is_dirty() {
            self.index = index;
            self.edit = None;
            self.tool = None;
            self.crop = None;
            self.after_index_change(fit);
        }
    }

    pub fn discard_edits(&mut self) {
        self.edit = None;
        self.gpu = None;
    }

    /// Remove `path` from the list (after delete/move) and keep a sensible index.
    pub fn remove_path(&mut self, path: &Path) {
        if let Some(pos) = self.list.iter().position(|p| p == path) {
            self.list.remove(pos);
            if self.index > pos || self.index >= self.list.len() {
                self.index = self.index.saturating_sub(1).min(self.list.len().saturating_sub(1));
            }
            self.edit = None;
            self.tool = None;
            self.crop = None;
            self.gpu = None;
            self.gpu_build = None;
            self.pending_lock = None;
            self.auto_fit = self.auto_fit.or(Some(FitMode::FitLarge));
        }
    }

    pub fn replace_path(&mut self, from: &Path, to: &Path) {
        for p in self.list.iter_mut() {
            if p == from {
                *p = to.to_path_buf();
            }
        }
        if let Some(e) = &mut self.edit {
            if e.path == from {
                e.path = to.to_path_buf();
            }
        }
        self.gpu = None;
    }

    /// The image currently displayed (edited version if any).
    pub fn current_image(&self, core: &Core) -> Option<Arc<RgbaImage>> {
        let path = self.current()?;
        if let Some(e) = &self.edit {
            if &e.path == path {
                return Some(e.current.clone());
            }
        }
        core.loader.get_ready(path).map(|l| l.decoded.image.clone())
    }

    pub fn loaded(&self, core: &Core) -> Option<Arc<LoadedImage>> {
        self.current().and_then(|p| core.loader.get_ready(p))
    }

    /// Start (or get) an edit session for the current image.
    pub fn ensure_edit(&mut self, core: &Core) -> Option<&mut EditSession> {
        let path = self.current()?.clone();
        if self.edit.as_ref().map(|e| e.path != path).unwrap_or(true) {
            let img = core.loader.get_ready(&path)?.decoded.image.clone();
            self.edit = Some(EditSession::new(path, img));
        }
        self.edit.as_mut()
    }

    pub fn apply_simple(&mut self, core: &Core, what: &str, f: impl FnOnce(&RgbaImage) -> RgbaImage) -> bool {
        let Some(sess) = self.ensure_edit(core) else { return false };
        let out = f(&sess.current);
        sess.push(out, what);
        self.auto_fit = self.auto_fit.or(Some(FitMode::FitLarge));
        true
    }

    pub fn open_tool(&mut self, tool: Box<dyn EditTool>, core: &Core) {
        let Some(img) = self.current_image(core) else { return };
        self.crop = None;
        // Preview runs on a copy sized to the screen for interactivity. Resizing a
        // big photo takes a while, so it is prepared off the UI thread.
        let screen = core.ctx.content_rect().size() * core.ctx.pixels_per_point();
        let max = (screen.x.max(screen.y) as u32).clamp(800, 2400);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = core.ctx.clone();
        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let image = if img.width().max(img.height()) > max { Arc::new(crate::imaging::resize_to_fit(&img, max, max)) } else { img.clone() };
                let scale = image.width() as f32 / img.width().max(1) as f32;
                let histogram = Histogram::compute(&image);
                ToolBase { image, scale, histogram }
            }));
            if let Ok(b) = res {
                let _ = tx.send(b);
            }
            ctx.request_repaint();
        });
        self.tool = Some(ActiveTool { tool, base: None, base_rx: Some(rx), version: 1, preview: None, preview_version: 0, rx: None, running: None, final_rx: None, show_original: false, error: None });
    }

    pub fn start_crop(&mut self, core: &Core) {
        let Some(img) = self.current_image(core) else { return };
        self.tool = None;
        let (w, h) = (img.width() as f32, img.height() as f32);
        self.crop = Some(CropState { r: [w * 0.1, h * 0.1, w * 0.9, h * 0.9], aspect: None, drag: None });
    }

    fn fit_zoom(&self, mode: FitMode, img: (u32, u32), view: Vec2, ppp: f32) -> f32 {
        let (w, h) = (img.0.max(1) as f32, img.1.max(1) as f32);
        let vw = view.x * ppp;
        let vh = view.y * ppp;
        let fit = (vw / w).min(vh / h);
        let z = match mode {
            FitMode::FitLarge => fit.min(1.0),
            FitMode::FitAlways => fit,
            FitMode::Fill => (vw / w).max(vh / h),
            FitMode::FitWidth => vw / w,
            FitMode::FitHeight => vh / h,
            FitMode::Actual => 1.0,
        };
        sane_zoom(z)
    }

    pub fn zoom_by(&mut self, factor: f32, anchor: Option<Pos2>) {
        let target = self.zoom_target.map(|t| t.0).unwrap_or(self.zoom);
        let new = (target * factor).clamp(0.01, 64.0);
        let a = anchor.unwrap_or(self.last_view_rect.center());
        self.auto_fit = None;
        self.zoom_target = Some((new, a));
    }

    pub fn set_zoom(&mut self, z: f32, anchor: Option<Pos2>) {
        self.auto_fit = None;
        self.zoom_target = Some((z.clamp(0.01, 64.0), anchor.unwrap_or(self.last_view_rect.center())));
    }

    /// Requests loading of the current image and neighbours.
    pub fn request_loads(&mut self, core: &mut Core) {
        let Some(cur) = self.current().cloned() else { return };
        let screen = core.ctx.content_rect().size() * core.ctx.pixels_per_point();
        core.loader.request(&cur, 0, Some(screen.x.max(screen.y) as u32));
        let mut keep = vec![cur];
        if core.settings.preload_neighbors {
            for (d, pr) in [(1isize, 1u32), (-1, 2), (2, 3)] {
                let i = self.index as isize + d;
                if i >= 0 && (i as usize) < self.list.len() {
                    let p = self.list[i as usize].clone();
                    if crate::formats::is_image_path(&p) {
                        core.loader.request(&p, pr, None);
                        keep.push(p);
                    }
                }
            }
        }
        core.loader.retain_queued(&keep);
    }

    // ------------------------------------------------------------------ UI

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core, fullscreen: bool) -> Vec<ViewerEvent> {
        let mut ev = Vec::new();
        self.request_loads(core);
        let p = core.pal;
        let full = ui.max_rect();
        let bgc = core.settings.viewer_background;
        ui.painter().rect_filled(full, 0.0, Color32::from_rgb(bgc[0], bgc[1], bgc[2]));

        // Info panel on the right.
        if self.show_info {
            egui::Panel::right("viewer-info")
                .resizable(true)
                .default_size(320.0)
                .size_range(260.0..=520.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::same(12)))
                .show(ui, |ui| self.info_panel(ui, core, &mut ev));
        }
        if self.tool.is_some() {
            egui::Panel::right("viewer-tool")
                .resizable(false)
                .exact_size(340.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::same(14)))
                .show(ui, |ui| self.tool_panel(ui, core, &mut ev));
        }
        if self.crop.is_some() {
            egui::Panel::right("viewer-crop")
                .resizable(false)
                .exact_size(300.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::same(14)))
                .show(ui, |ui| self.crop_panel(ui, core));
        }
        if self.show_filmstrip {
            egui::Panel::bottom("viewer-filmstrip")
                .resizable(false)
                .exact_size(96.0)
                .frame(egui::Frame::NONE.fill(p.panel.gamma_multiply(0.96)).inner_margin(egui::Margin::symmetric(6, 6)))
                .show(ui, |ui| self.filmstrip(ui, core, &mut ev));
        }

        let view = ui.available_rect_before_wrap();
        self.last_view_rect = view;
        let resp = ui.allocate_rect(view, Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();

        if ui.input(|i| i.pointer.delta() != Vec2::ZERO || !i.events.is_empty()) {
            self.last_activity = Instant::now();
        }

        let Some(path) = self.current().cloned() else {
            ui.painter().text(view.center(), Align2::CENTER_CENTER, "No images", FontId::proportional(16.0), p.muted);
            return ev;
        };

        // ---- Pick what to show
        let mut size: Option<(u32, u32)> = None;
        let mut has_alpha = false;
        let edit_img = self.edit.as_ref().filter(|e| e.path == path).map(|e| (e.version, e.current.clone()));
        let state_ready = core.loader.get_ready(&path);
        // Receive an edited image's pyramid built in the background.
        if let Some((bp, key, rx)) = &self.gpu_build {
            match rx.try_recv() {
                Ok(pyr) => {
                    if bp == &path {
                        let name = format!("{}-edit{:?}", path.display(), key);
                        self.gpu = Some((path.clone(), *key, GpuImage::new(ui.ctx(), &name, pyr)));
                    }
                    self.gpu_build = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.gpu_build = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        if let Some((ver, img)) = &edit_img {
            let key = GpuKey::Edit(*ver);
            let have = matches!(&self.gpu, Some((gp, gk, _)) if gp == &path && *gk == key);
            let building = matches!(&self.gpu_build, Some((bp, bk, _)) if bp == &path && *bk == key);
            if !have && !building {
                if img.width() as u64 * img.height() as u64 <= 4_000_000 {
                    // Small enough to do right away without a visible hitch.
                    let pyr = render::build_pyramid(img.clone());
                    self.gpu = Some((path.clone(), key, GpuImage::new(ui.ctx(), &format!("{}-edit{ver}", path.display()), pyr)));
                } else {
                    // Keep showing the previous image while the pyramid is built.
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    let img = img.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(render::build_pyramid(img));
                        ctx.request_repaint();
                    });
                    self.gpu_build = Some((path.clone(), key, rx));
                }
            }
            has_alpha = state_ready.as_ref().map(|l| l.decoded.has_alpha).unwrap_or(false);
        } else if let Some(loaded) = &state_ready {
            let key = GpuKey::Decoded(Arc::as_ptr(&loaded.pyramid[0]) as usize);
            if !matches!(&self.gpu, Some((gp, gk, _)) if gp == &path && *gk == key) {
                self.gpu = Some((path.clone(), key, GpuImage::new(ui.ctx(), &path.display().to_string(), loaded.pyramid.clone())));
                self.anim = None;
            }
            has_alpha = loaded.decoded.has_alpha;
        }
        // Size of what is actually on the GPU (the previous edit stays visible,
        // with its own size, while a new pyramid is built).
        if edit_img.is_some() || state_ready.is_some() {
            if let Some((gp, _, g)) = &self.gpu {
                if gp == &path {
                    size = Some((g.width, g.height));
                }
            }
        }
        // Locked zoom ratio: apply the previous image's zoom and position once
        // the new image's size is known.
        if let Some(sz) = size {
            if let Some((z, norm)) = self.pending_lock.take() {
                self.auto_fit = None;
                self.zoom_target = None;
                self.zoom = z;
                self.center = locked_center(norm, sz);
            }
            self.view_img = sz;
        }

        // Tool preview replaces the displayed image (same aspect unless the tool resizes).
        let mut tool_preview_size = None;
        if let Some(t) = &self.tool {
            if t.preview.is_some() && !t.show_original {
                tool_preview_size = t.preview.as_ref().map(|g| (g.width, g.height));
            }
        }

        // ---- Zoom / pan maths
        if let Some(sz) = size {
            let disp = match (tool_preview_size, &self.tool) {
                (Some(ps), Some(t)) if t.tool.changes_size() => ((ps.0 as f32 / t.scale()) as u32, (ps.1 as f32 / t.scale()) as u32),
                _ => sz,
            };
            self.update_zoom(ui, &resp, view, disp, ppp, core, &mut ev);
            let draw_w = disp.0 as f32 * self.zoom / ppp;
            let draw_h = disp.1 as f32 * self.zoom / ppp;
            let mut min = view.center() - Vec2::new(self.center.x * self.zoom / ppp, self.center.y * self.zoom / ppp);
            // Snap to the physical pixel grid for crisp 1:1 display.
            min.x = (min.x * ppp).round() / ppp;
            min.y = (min.y * ppp).round() / ppp;
            let dest = Rect::from_min_size(min, Vec2::new(draw_w, draw_h));
            let painter = ui.painter().with_clip_rect(view);
            if has_alpha && core.settings.checkerboard_alpha {
                render::paint_checkerboard(&painter, dest.intersect(view), 10.0, p.dark);
            }
            let nearest = !core.settings.smooth_zoom_in && self.zoom >= 2.0;
            let mut drawn = false;
            if let Some(t) = &mut self.tool {
                let ts = t.scale();
                if !t.show_original {
                    if let Some(g) = &mut t.preview {
                        g.paint(ui.ctx(), &painter, dest, view, self.zoom * ts, nearest, Color32::WHITE);
                        drawn = true;
                    }
                }
            }
            if !drawn {
                if let Some(frames) = state_ready.as_ref().and_then(|l| l.decoded.frames.clone()).filter(|_| edit_img.is_none()) {
                    self.paint_animation(ui, &painter, &path, &frames, dest);
                } else if let Some((_, _, g)) = &mut self.gpu {
                    g.paint(ui.ctx(), &painter, dest, view, self.zoom, nearest, Color32::WHITE);
                }
            }
            if self.crop.is_some() {
                self.crop_overlay(ui, &painter, dest, sz, &resp);
            }
            if (self.magnifier_active || self.magnifier_locked) && self.crop.is_none() {
                self.paint_magnifier(ui, core, dest, sz, view);
            }
        } else {
            // Not decoded yet (or failed): show the cached thumbnail / quick preview.
            match core.loader.state(&path) {
                Some(LoadState::Failed(err)) => {
                    let err = err.clone();
                    ui.painter().text(view.center() - Vec2::new(0.0, 28.0), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(42.0), p.faint);
                    ui.painter().text(view.center() + Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, file_name(&path), FontId::proportional(14.0), p.text);
                    let galley = ui.painter().layout(err, FontId::proportional(12.5), p.muted, (view.width() - 80.0).min(620.0));
                    let pos = view.center() + Vec2::new(-galley.size().x / 2.0, 34.0);
                    ui.painter().galley(pos, galley, p.muted);
                }
                _ => {
                    let mut shown = false;
                    if let Some(LoadState::Preview(img)) = core.loader.state(&path) {
                        if !matches!(&self.preview_tex, Some((pp, _)) if pp == &path) {
                            let tex = ui.ctx().load_texture(format!("preview:{}", path.display()), render::color_image_from_rgba(img), egui::TextureOptions::LINEAR);
                            self.preview_tex = Some((path.clone(), tex));
                        }
                    }
                    let key = self.thumb_key(&path);
                    let tex = self.preview_tex.as_ref().filter(|(pp, _)| pp == &path).map(|(_, t)| t.clone()).or_else(|| {
                        match core.thumbs.get(&key) {
                            Thumb::Ready(t) => Some(t.tex.clone()),
                            _ => None,
                        }
                    });
                    if let Some(tex) = tex {
                        let ts = tex.size_vec2();
                        let dims = core.meta.get(&path).and_then(|m| m.dims);
                        let target = match dims {
                            Some((w, h)) => {
                                let z = self.fit_zoom(self.auto_fit.unwrap_or(FitMode::FitLarge), (w, h), view.size(), ppp);
                                Vec2::new(w as f32, h as f32) * z / ppp
                            }
                            None => {
                                let s = (view.width() / ts.x).min(view.height() / ts.y);
                                ts * s
                            }
                        };
                        let r = Rect::from_center_size(view.center(), target);
                        let mut mesh = egui::Mesh::with_texture(tex.id());
                        mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                        ui.painter().add(egui::Shape::mesh(mesh));
                        shown = true;
                    }
                    let spinner_at = if shown { Rect::from_center_size(view.right_bottom() - Vec2::new(30.0, 30.0), Vec2::splat(20.0)) } else { Rect::from_center_size(view.center(), Vec2::splat(28.0)) };
                    egui::Spinner::new().size(spinner_at.width()).paint_at(ui, spinner_at);
                }
            }
            // Still allow navigation while loading.
            self.nav_input(ui, &resp, core, &mut ev, false);
        }

        // ---- Overlays
        if core.settings.show_info_overlay || !fullscreen {
            self.overlay_caption(ui, core, view, size, &path);
        }
        if self.show_histogram {
            self.histogram_overlay(ui, core, view, &path);
        }
        let edge_open = self.edge_panels_ui(ui, core, view, &mut ev);
        let idle = self.last_activity.elapsed() > Duration::from_millis(2200);
        let near_bottom = ui.input(|i| i.pointer.hover_pos()).map(|pos| pos.y > view.max.y - 90.0 && view.contains(pos)).unwrap_or(false);
        if !idle || near_bottom || !fullscreen {
            self.bottom_bar(ui, core, view, &mut ev);
        } else if fullscreen && self.crop.is_none() && self.tool.is_none() && !edge_open {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }
        if !idle {
            ui.ctx().request_repaint_after(Duration::from_millis(2300));
        }
        if self.confirm_discard {
            self.discard_dialog(ui.ctx(), core, &mut ev);
        }
        ev
    }

    #[allow(clippy::too_many_arguments)]
    fn update_zoom(&mut self, ui: &mut Ui, resp: &egui::Response, view: Rect, img: (u32, u32), ppp: f32, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        if !view.is_positive() || img.0 == 0 || img.1 == 0 {
            return; // window minimized / collapsed: keep the old view
        }
        let (w, h) = (img.0 as f32, img.1 as f32);
        self.zoom = sane_zoom(self.zoom);
        if let Some(mode) = self.auto_fit {
            self.zoom = self.fit_zoom(mode, img, view.size(), ppp);
            self.center = Vec2::new(w / 2.0, h / 2.0);
            if matches!(mode, FitMode::FitWidth) {
                self.center.y = (view.height() * ppp / self.zoom / 2.0).min(h / 2.0);
            }
            if matches!(mode, FitMode::FitHeight) {
                self.center.x = (view.width() * ppp / self.zoom / 2.0).min(w / 2.0);
            }
            self.zoom_target = None;
        }
        // Smooth zoom animation towards the target, anchored at a screen point.
        if let Some((target, anchor)) = self.zoom_target {
            let dt = ui.input(|i| i.stable_dt).min(0.05);
            let k = if core.settings.animate_zoom { 1.0 - (-dt * 22.0).exp() } else { 1.0 };
            let new = if (target / self.zoom - 1.0).abs() < 0.004 { target } else { self.zoom + (target - self.zoom) * k };
            let img_pt = self.center + (anchor - view.center()) * ppp / self.zoom;
            self.center = img_pt - (anchor - view.center()) * ppp / new;
            self.zoom = new;
            if new == target {
                self.zoom_target = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        let fits = w * self.zoom / ppp <= view.width() + 1.0 && h * self.zoom / ppp <= view.height() + 1.0;
        self.nav_input(ui, resp, core, ev, !fits);

        // Drag to pan (or press-and-hold magnifier when the whole image fits).
        let fits = w * self.zoom / ppp <= view.width() + 1.0 && h * self.zoom / ppp <= view.height() + 1.0;
        let primary_down = ui.input(|i| i.pointer.primary_down());
        if self.crop.is_none() {
            if resp.drag_started_by(egui::PointerButton::Primary) {
                self.drag_pan = !fits;
            }
            if resp.dragged_by(egui::PointerButton::Primary) && self.drag_pan {
                let d = resp.drag_delta();
                self.center -= d * ppp / self.zoom;
                self.auto_fit = None;
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if !fits && resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            self.magnifier_active = fits && primary_down && resp.is_pointer_button_down_on() && resp.dragged() && !self.drag_pan || (fits && primary_down && resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.press_start_time().map(|t| i.time - t > 0.18).unwrap_or(false)));
            if !primary_down {
                self.drag_pan = false;
            }
        }
        // Clamp so the image never drifts off-screen.
        let half_vw = view.width() * ppp / self.zoom / 2.0;
        let half_vh = view.height() * ppp / self.zoom / 2.0;
        self.center.x = if w <= half_vw * 2.0 { w / 2.0 } else { self.center.x.clamp(half_vw, w - half_vw) };
        self.center.y = if h <= half_vh * 2.0 { h / 2.0 } else { self.center.y.clamp(half_vh, h - half_vh) };
    }

    fn nav_input(&mut self, ui: &mut Ui, resp: &egui::Response, core: &mut Core, ev: &mut Vec<ViewerEvent>, can_pan: bool) {
        if !resp.hovered() && !resp.contains_pointer() {
            return;
        }
        let (si, ctrl, now) = ui.input(|i| (scroll_input(i), i.modifiers.ctrl, i.time));
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let ppp = ui.ctx().pixels_per_point();
        let zoom_mode = core.settings.wheel_action == WheelAction::Zoom;
        // Pinch: touch screens and gesture events. Ctrl+wheel (which is also how
        // Windows delivers touchpad pinches) is handled below, never both.
        if (si.pinch - 1.0).abs() > 1e-4 && si.pinch.is_finite() && si.pinch > 0.0 {
            self.zoom_by(si.pinch, pointer);
        }
        if si.touch_pan != Vec2::ZERO && can_pan {
            self.center -= si.touch_pan * ppp / self.zoom;
            self.auto_fit = None;
        }
        if si.wheel != 0.0 || si.precise != Vec2::ZERO {
            if si.any_precise && can_pan && !ctrl {
                // Two-finger touchpad scroll pans an image larger than the window.
                self.center -= si.precise * ppp / self.zoom;
                self.auto_fit = None;
            } else if ctrl != zoom_mode {
                if si.wheel != 0.0 {
                    // Proportional zoom so precision touchpads feel smooth too.
                    let f = (1.0 + si.wheel.clamp(-200.0, 200.0) * 0.0045).max(0.2);
                    self.zoom_by(f, pointer);
                }
            } else {
                match self.wheel_nav.feed(si.wheel, si.any_precise, now) {
                    1 => ev.push(ViewerEvent::Command(Command::PrevImage)),
                    -1 => ev.push(ViewerEvent::Command(Command::NextImage)),
                    _ => {}
                }
            }
        }
        if resp.double_clicked() && self.crop.is_none() {
            ev.push(ViewerEvent::Close);
        }
        if resp.middle_clicked() {
            if self.auto_fit.is_some() {
                self.set_zoom(1.0, pointer);
            } else {
                self.auto_fit = Some(core.settings.fit_mode);
            }
        }
        // Back/forward mouse buttons navigate.
        if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Extra1)) {
            ev.push(ViewerEvent::Command(Command::PrevImage));
        }
        if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Extra2)) {
            ev.push(ViewerEvent::Command(Command::NextImage));
        }
        let _ = Key::Escape;
        resp.context_menu(|ui| {
            ui.set_min_width(230.0);
            for c in [Command::ZoomFit, Command::ZoomActual, Command::ToggleFullscreen] {
                menu_item(ui, c, ev);
            }
            ui.separator();
            for c in [Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::Crop, Command::Resize] {
                menu_item(ui, c, ev);
            }
            ui.menu_button(format!("{}   Adjust", I::SLIDERS_HORIZONTAL), |ui| {
                for c in [Command::AutoEnhance, Command::AdjustColors, Command::Levels, Command::Curves, Command::Sharpen, Command::Blur, Command::Effects, Command::FreeRotate, Command::CanvasSize, Command::AddBorder, Command::AddText, Command::Watermark] {
                    menu_item(ui, c, ev);
                }
            });
            ui.separator();
            for c in [Command::Save, Command::SaveAs, Command::CopyImage, Command::SetWallpaper, Command::Print] {
                menu_item(ui, c, ev);
            }
            ui.separator();
            for c in [Command::ToggleInfo, Command::ToggleFilmstrip, Command::Slideshow, Command::ShowInExplorer, Command::Delete, Command::CloseViewer] {
                menu_item(ui, c, ev);
            }
        });
    }

    fn paint_animation(&mut self, ui: &mut Ui, painter: &egui::Painter, path: &Path, frames: &Arc<Vec<crate::decode::AnimFrame>>, dest: Rect) {
        let now = Instant::now();
        if self.anim.as_ref().map(|a| a.path != path || a.frame_tex.len() != frames.len()).unwrap_or(true) {
            let total: u64 = frames.iter().map(|f| f.image.width() as u64 * f.image.height() as u64).sum();
            let frame_tex = if total <= ANIM_CACHE_PIXELS { vec![None; frames.len()] } else { Vec::new() };
            self.anim = Some(Anim { path: path.to_path_buf(), frame: 0, next_at: now + frame_delay(frames, 0), playing: true, frame_tex, tex: None, shown_frame: usize::MAX });
        }
        let a = self.anim.as_mut().unwrap();
        if a.frame >= frames.len() {
            a.frame = 0;
        }
        if a.playing && now >= a.next_at {
            let (frame, next_at) = advance_animation(a.frame, a.next_at, now, |i| frame_delay(frames, i), frames.len());
            a.frame = frame;
            a.next_at = next_at;
        }
        let tex_id = if !a.frame_tex.is_empty() {
            let i = a.frame;
            a.frame_tex[i]
                .get_or_insert_with(|| ui.ctx().load_texture(format!("anim:{}#{i}", path.display()), render::color_image_from_rgba(&frames[i].image), egui::TextureOptions::LINEAR))
                .id()
        } else {
            if a.shown_frame != a.frame || a.tex.is_none() {
                let ci = render::color_image_from_rgba(&frames[a.frame].image);
                match &mut a.tex {
                    Some(t) => t.set(ci, egui::TextureOptions::LINEAR),
                    None => a.tex = Some(ui.ctx().load_texture("anim", ci, egui::TextureOptions::LINEAR)),
                }
                a.shown_frame = a.frame;
            }
            a.tex.as_ref().map(|t| t.id()).unwrap_or_default()
        };
        {
            let t = tex_id;
            let mut mesh = egui::Mesh::with_texture(t);
            mesh.add_rect_with_uv(dest, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            painter.add(egui::Shape::mesh(mesh));
        }
        if a.playing {
            ui.ctx().request_repaint_after(a.next_at.saturating_duration_since(now));
        }
    }

    pub fn toggle_animation(&mut self) {
        if let Some(a) = &mut self.anim {
            a.playing = !a.playing;
            if a.playing {
                // Resume shortly instead of fast-forwarding through the pause.
                a.next_at = Instant::now() + Duration::from_millis(100);
            }
        }
    }

    pub fn step_frame(&mut self, core: &Core, delta: isize) {
        let Some(loaded) = self.loaded(core) else { return };
        let Some(frames) = &loaded.decoded.frames else { return };
        if let Some(a) = &mut self.anim {
            a.playing = false;
            a.frame = (a.frame as isize + delta).rem_euclid(frames.len() as isize) as usize;
        }
    }

    pub fn animation_info(&self, core: &Core) -> Option<(usize, usize, bool)> {
        let loaded = self.loaded(core)?;
        let frames = loaded.decoded.frames.as_ref()?;
        let a = self.anim.as_ref()?;
        Some((a.frame, frames.len(), a.playing))
    }

    fn paint_magnifier(&mut self, ui: &mut Ui, core: &Core, dest: Rect, img: (u32, u32), view: Rect) {
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
        if !view.contains(pos) || !dest.contains(pos) {
            return;
        }
        let zoom = self.zoom;
        let Some(gpu) = self.gpu.as_mut().map(|g| &mut g.2) else { return };
        let size = core.settings.magnifier_size;
        let mz = core.settings.magnifier_zoom.max(1.5);
        let ppp = ui.ctx().pixels_per_point();
        // Image coords under cursor.
        let u = (pos.x - dest.min.x) / dest.width();
        let v = (pos.y - dest.min.y) / dest.height();
        let lens = Rect::from_center_size(pos, Vec2::splat(size));
        let painter = ui.painter().with_clip_rect(lens);
        // Magnified zoom in physical px per image px.
        let z = (zoom * mz).max(1.0 / ppp);
        let big = Vec2::new(img.0 as f32 * z / ppp, img.1 as f32 * z / ppp);
        let big_min = pos - Vec2::new(u * big.x, v * big.y);
        let big_dest = Rect::from_min_size(big_min, big);
        painter.rect_filled(lens, CornerRadius::same(12), Color32::BLACK);
        gpu.paint(ui.ctx(), &painter, big_dest, lens, z, z >= 3.0, Color32::WHITE);
        ui.painter().rect_stroke(lens, CornerRadius::same(12), Stroke::new(2.0, Color32::from_white_alpha(200)), egui::StrokeKind::Outside);
        let px = ((u * img.0 as f32) as u32, (v * img.1 as f32) as u32);
        ui.painter().text(lens.left_bottom() + Vec2::new(8.0, -8.0), Align2::LEFT_BOTTOM, format!("{}, {}  ·  {:.0}%", px.0, px.1, z * 100.0), FontId::proportional(11.0), Color32::from_white_alpha(220));
    }

    /// Histogram of the displayed image (computed off the UI thread, cached per
    /// image version) for the overlay and the info panel.
    fn histogram_for(&mut self, core: &Core, ctx: &egui::Context, path: &Path) -> Option<&Histogram> {
        let img = self.current_image(core)?;
        let ver = self.edit.as_ref().filter(|e| e.path == path).map(|e| e.version).unwrap_or(0);
        let key = (path.to_path_buf(), ver, Arc::as_ptr(&img) as usize);
        self.hist_cache.get(&key, ctx, move || Histogram::compute(&img))
    }

    fn histogram_overlay(&mut self, ui: &mut Ui, core: &Core, view: Rect, path: &Path) {
        let r = Rect::from_min_size(Pos2::new(view.max.x - 12.0 - 240.0, view.min.y + 12.0), Vec2::new(240.0, 96.0));
        if r.min.x < view.min.x + 8.0 || r.max.y > view.max.y - 8.0 {
            return;
        }
        let ctx = ui.ctx().clone();
        let painter = ui.painter().with_clip_rect(view);
        if let Some(h) = self.histogram_for(core, &ctx, path) {
            painter.rect_filled(r.expand(6.0), CornerRadius::same(10), Color32::from_black_alpha(150));
            h.paint(&painter, r, true);
        }
    }

    /// FastStone-style pop-out panels: dwell at a window edge to show a floating
    /// panel over the image (top: filmstrip, right: info, left: edit tools; the
    /// bottom edge reveals the toolbar). Returns true while a panel is visible.
    fn edge_panels_ui(&mut self, ui: &mut Ui, core: &mut Core, view: Rect, ev: &mut Vec<ViewerEvent>) -> bool {
        let ctx = ui.ctx().clone();
        let enabled = self.edge_panels && self.crop.is_none() && self.tool.is_none() && !self.confirm_discard && view.is_positive();
        let pos = ctx.input(|i| i.pointer.hover_pos());
        let busy = ctx.input(|i| i.pointer.any_down()) || ctx.dragged_id().is_some() || egui::Popup::is_any_open(&ctx);
        let allowed = |e: Edge, s: &Self| match e {
            Edge::Top => true,
            Edge::Left => true,
            Edge::Right => !s.show_info,
            Edge::Bottom => false, // the toolbar already pops up at the bottom
        };
        if !enabled {
            self.edge.open = None;
            self.edge.dwell = None;
        } else if let Some(open) = self.edge.open {
            // Hide once the pointer leaves the panel (with a little slack).
            let inside = match (pos, self.edge.panel_rect) {
                (Some(p), Some(r)) => r.expand(14.0).contains(p) || edge_at(view, p, EDGE_MARGIN) == Some(open),
                (Some(p), None) => edge_at(view, p, EDGE_MARGIN) == Some(open),
                _ => false,
            };
            if !inside && !egui::Popup::is_any_open(&ctx) {
                self.edge.open = None;
                self.edge.dwell = None;
            }
        } else {
            match pos.and_then(|p| edge_at(view, p, EDGE_MARGIN)).filter(|e| allowed(*e, self)) {
                Some(e) if !busy => {
                    let since = match self.edge.dwell {
                        Some((d, t)) if d == e => t,
                        _ => {
                            let now = Instant::now();
                            self.edge.dwell = Some((e, now));
                            now
                        }
                    };
                    let waited = since.elapsed();
                    if waited >= EDGE_DWELL {
                        self.edge.open = Some(e);
                        self.edge.shown = Some(e);
                        self.edge.panel_rect = None;
                    } else {
                        ctx.request_repaint_after(EDGE_DWELL - waited);
                    }
                }
                _ => self.edge.dwell = None,
            }
        }
        let t = ctx.animate_bool_with_time(ui.scope_id().with("viewer-edge-fade"), self.edge.open.is_some(), 0.16);
        let Some(edge) = self.edge.shown else { return false };
        if t <= 0.0 {
            self.edge.shown = None;
            self.edge.panel_rect = None;
            return false;
        }
        let p = core.pal;
        let interact = self.edge.open.is_some();
        let slide = (1.0 - t) * 18.0;
        let bottom_reserve = 70.0; // keep clear of the bottom toolbar
        let rect = match edge {
            Edge::Top => Rect::from_min_size(view.min + Vec2::new(8.0, 4.0 - slide), Vec2::new(view.width() - 16.0, 112.0)),
            Edge::Right => {
                let w = 340.0_f32.min(view.width() - 16.0);
                Rect::from_min_size(Pos2::new(view.max.x - w - 6.0 + slide, view.min.y + 8.0), Vec2::new(w, (view.height() - 16.0 - bottom_reserve).max(120.0)))
            }
            Edge::Left => {
                let h = (view.height() - 16.0 - bottom_reserve).clamp(120.0, 900.0);
                Rect::from_min_size(Pos2::new(view.min.x + 6.0 - slide, view.center().y - h / 2.0 - bottom_reserve / 2.0), Vec2::new(262.0, h))
            }
            Edge::Bottom => return false,
        };
        let margin = 10.0;
        let area = egui::Area::new(ui.scope_id().with(("viewer-edge-panel", edge)))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .constrain(false)
            .interactable(interact)
            .show(&ctx, |ui| {
                ui.multiply_opacity(t);
                egui::Frame::NONE
                    .fill(p.panel.gamma_multiply(0.97))
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(margin as i8))
                    .show(ui, |ui| {
                        let inner = rect.size() - Vec2::splat(margin * 2.0);
                        ui.set_min_size(inner);
                        ui.set_max_size(inner);
                        match edge {
                            Edge::Top => {
                                ui.allocate_ui(inner, |ui| self.filmstrip(ui, core, ev));
                            }
                            Edge::Right => self.info_panel(ui, core, ev),
                            Edge::Left => self.edge_tool_menu(ui, ev),
                            Edge::Bottom => {}
                        }
                    });
            });
        self.edge.panel_rect = Some(area.response.rect);
        true
    }

    /// Compact vertical list of edit commands for the left pop-out panel.
    fn edge_tool_menu(&mut self, ui: &mut Ui, ev: &mut Vec<ViewerEvent>) {
        let groups: [&[Command]; 4] = [
            &[Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::FlipVertical, Command::FreeRotate],
            &[Command::Crop, Command::Resize, Command::CanvasSize],
            &[Command::AutoEnhance, Command::AdjustColors, Command::Levels, Command::Curves, Command::Sharpen, Command::Blur, Command::Effects],
            &[Command::AddText, Command::AddBorder, Command::Watermark],
        ];
        let dirty = self.is_dirty();
        egui::ScrollArea::vertical().id_salt("edge-tools").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new("Edit").font(super::theme::semibold(13.0)));
            ui.add_space(4.0);
            for (gi, g) in groups.iter().enumerate() {
                if gi > 0 {
                    ui.add_space(3.0);
                    ui.separator();
                    ui.add_space(3.0);
                }
                for c in g.iter() {
                    edge_menu_item(ui, *c, ev);
                }
            }
            ui.add_space(3.0);
            ui.separator();
            ui.add_space(3.0);
            for c in [Command::Undo, Command::Redo] {
                edge_menu_item(ui, c, ev);
            }
            edge_menu_item(ui, if dirty { Command::Save } else { Command::SaveAs }, ev);
        });
    }

    fn overlay_caption(&self, ui: &mut Ui, core: &Core, view: Rect, size: Option<(u32, u32)>, path: &Path) {
        let p = core.pal;
        let mut parts = vec![format!("{} / {}", self.index + 1, self.list.len()), file_name(path)];
        if let Some((w, h)) = size {
            parts.push(format!("{w} × {h}"));
        }
        parts.push(format!("{:.0}%{}", self.zoom * 100.0, if self.lock_zoom { " (locked)" } else { "" }));
        if self.is_dirty() {
            parts.push("● edited".into());
        }
        if let Some((f, n, playing)) = self.animation_info(core) {
            parts.push(format!("frame {}/{}{}", f + 1, n, if playing { "" } else { " (paused)" }));
        }
        let text = parts.join("   ·   ");
        let galley = ui.painter().layout_no_wrap(text, FontId::proportional(12.5), Color32::from_white_alpha(230));
        let r = Rect::from_min_size(view.min + Vec2::new(12.0, 12.0), galley.size() + Vec2::new(20.0, 10.0));
        ui.painter().rect_filled(r, CornerRadius::same(8), Color32::from_black_alpha(150));
        ui.painter().galley(r.min + Vec2::new(10.0, 5.0), galley, p.text);
    }

    fn bottom_bar(&mut self, ui: &mut Ui, core: &mut Core, view: Rect, ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        let w = 676.0_f32.min(view.width() - 20.0);
        let rect = Rect::from_center_size(Pos2::new(view.center().x, view.max.y - 34.0), Vec2::new(w, 44.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.painter().rect_filled(rect, CornerRadius::same(12), p.overlay);
        child.painter().rect_stroke(rect, CornerRadius::same(12), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        child.add_space(8.0);
        child.spacing_mut().item_spacing.x = 1.0;
        let b = |ui: &mut Ui, icon: &str, tip: &str, cmd: Command, ev: &mut Vec<ViewerEvent>| {
            if widgets::icon_button(ui, icon, &format!("{tip}  ({})", cmd.shortcut()), false).clicked() {
                ev.push(ViewerEvent::Command(cmd));
            }
        };
        b(&mut child, I::SQUARES_FOUR, "Back to browser", Command::CloseViewer, ev);
        widgets::separator_v(&mut child);
        b(&mut child, I::CARET_LEFT, "Previous", Command::PrevImage, ev);
        b(&mut child, I::CARET_RIGHT, "Next", Command::NextImage, ev);
        widgets::separator_v(&mut child);
        b(&mut child, I::MAGNIFYING_GLASS_MINUS, "Zoom out", Command::ZoomOut, ev);
        let zl = child.add_sized([54.0, 26.0], egui::Button::new(RichText::new(format!("{:.0}%", self.zoom * 100.0)).size(12.0)).frame(false));
        if zl.clicked() {
            ev.push(ViewerEvent::Command(Command::ZoomActual));
        }
        zl.on_hover_text("Click: 100%");
        b(&mut child, I::MAGNIFYING_GLASS_PLUS, "Zoom in", Command::ZoomIn, ev);
        let fit_sel = self.auto_fit.is_some();
        if widgets::icon_button(&mut child, I::ARROWS_IN, "Fit to window (/)", fit_sel).clicked() {
            ev.push(ViewerEvent::Command(Command::ZoomFit));
        }
        let (lock_icon, lock_tip) = if self.lock_zoom { (I::LOCK_SIMPLE, "Zoom ratio locked: next/previous keep zoom and position (click to unlock)") } else { (I::LOCK_SIMPLE_OPEN, "Lock zoom ratio: keep zoom and position when changing images") };
        if widgets::icon_button(&mut child, lock_icon, lock_tip, self.lock_zoom).clicked() {
            self.toggle_lock_zoom();
        }
        widgets::separator_v(&mut child);
        b(&mut child, I::ARROW_COUNTER_CLOCKWISE, "Rotate left", Command::RotateLeft, ev);
        b(&mut child, I::ARROW_CLOCKWISE, "Rotate right", Command::RotateRight, ev);
        b(&mut child, I::CROP, "Crop", Command::Crop, ev);
        b(&mut child, I::SLIDERS_HORIZONTAL, "Adjust colors", Command::AdjustColors, ev);
        b(&mut child, I::RESIZE, "Resize", Command::Resize, ev);
        widgets::separator_v(&mut child);
        let dirty = self.is_dirty();
        if widgets::icon_button(&mut child, if dirty { I::FLOPPY_DISK } else { I::FLOPPY_DISK_BACK }, if dirty { "Save changes (Ctrl+S)" } else { "Save as... (Ctrl+Shift+S)" }, dirty).clicked() {
            ev.push(ViewerEvent::Command(if dirty { Command::Save } else { Command::SaveAs }));
        }
        b(&mut child, I::TRASH, "Delete", Command::Delete, ev);
        widgets::separator_v(&mut child);
        if widgets::icon_button(&mut child, I::INFO, "Info panel (I)", self.show_info).clicked() {
            ev.push(ViewerEvent::Command(Command::ToggleInfo));
        }
        if widgets::icon_button(&mut child, I::FILM_STRIP, "Filmstrip (F)", self.show_filmstrip).clicked() {
            ev.push(ViewerEvent::Command(Command::ToggleFilmstrip));
        }
        b(&mut child, I::PLAY_CIRCLE, "Slideshow", Command::Slideshow, ev);
        b(&mut child, I::CORNERS_OUT, "Full screen", Command::ToggleFullscreen, ev);
    }

    fn filmstrip(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        let h = ui.available_height();
        let cell = Vec2::new(h * 1.25, h);
        egui::ScrollArea::horizontal().id_salt("filmstrip").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for i in 0..self.list.len() {
                    let (rect, resp) = ui.allocate_exact_size(cell, Sense::click());
                    if ui.is_rect_visible(rect) {
                        let path = &self.list[i];
                        let sel = i == self.index;
                        ui.painter().rect_filled(rect, CornerRadius::same(6), if sel { p.accent_soft } else if resp.hovered() { p.hover } else { Color32::TRANSPARENT });
                        let path = path.clone();
                        let key = self.thumb_key(&path);
                        {
                            if let Thumb::Ready(t) = core.thumbs.get(&key) {
                                let ts = t.tex.size_vec2();
                                let s = ((cell.x - 8.0) / ts.x).min((cell.y - 8.0) / ts.y);
                                let r = Rect::from_center_size(rect.center(), ts * s);
                                let mut mesh = egui::Mesh::with_texture(t.tex.id());
                                mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                                ui.painter().add(egui::Shape::mesh(mesh));
                            }
                        }
                        if sel {
                            ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
                        }
                    }
                    if i == self.index && resp.rect.is_positive() {
                        // Keep the current image in view.
                        if !ui.clip_rect().contains_rect(resp.rect) {
                            resp.scroll_to_me(Some(egui::Align::Center));
                        }
                    }
                    if resp.clicked() {
                        self.go_to(i, core.settings.fit_mode);
                        ev.push(ViewerEvent::IndexChanged(self.list[i].clone()));
                    }
                }
            });
        });
    }

    fn info_panel(&mut self, ui: &mut Ui, core: &mut Core, _ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        let Some(path) = self.current().cloned() else { return };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(file_name(&path)).font(super::theme::semibold(15.0))).wrap());
            ui.add(egui::Label::new(RichText::new(path.parent().map(|d| d.display().to_string()).unwrap_or_default()).size(11.5).color(p.faint)).wrap());
            ui.add_space(8.0);
            let tag = core.tags.get(&path);
            ui.horizontal(|ui| {
                if let Some(r) = widgets::star_rating(ui, tag.rating, 15.0, &p) {
                    core.tags.update(&path, |t| t.rating = r);
                }
                ui.add_space(8.0);
                for l in 1..=5u8 {
                    let c = crate::tags::LABEL_COLORS[l as usize];
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                    let col = Color32::from_rgb(c[0], c[1], c[2]);
                    if tag.label == l {
                        ui.painter().circle_filled(r.center(), 7.0, col);
                        ui.painter().circle_stroke(r.center(), 7.5, Stroke::new(1.5, p.text));
                    } else {
                        ui.painter().circle_filled(r.center(), 5.5, col.gamma_multiply(0.75));
                    }
                    if resp.on_hover_text(crate::tags::LABEL_NAMES[l as usize]).clicked() {
                        core.tags.update(&path, |t| t.label = if t.label == l { 0 } else { l });
                    }
                }
            });
            ui.add_space(8.0);
            // Histogram
            let ctx = ui.ctx().clone();
            if let Some(hist) = self.histogram_for(core, &ctx, &path) {
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 90.0), Sense::hover());
                hist.paint(ui.painter(), r, p.dark);
                ui.add_space(6.0);
            }
            widgets::section_title(ui, &p, "File");
            if let Some(l) = self.loaded(core) {
                let d = &l.decoded;
                widgets::kv_row(ui, &p, "Dimensions", &format!("{} × {}  ({:.1} MP)", d.width(), d.height(), d.width() as f64 * d.height() as f64 / 1e6));
                widgets::kv_row(ui, &p, "File size", &catalog::human_size(l.file_size));
                widgets::kv_row(ui, &p, "Format", &d.format);
                widgets::kv_row(ui, &p, "Color", &d.color_info);
                if d.frame_count > 1 {
                    widgets::kv_row(ui, &p, "Frames", &d.frame_count.to_string());
                }
                if d.orientation != 1 {
                    widgets::kv_row(ui, &p, "Orientation", &format!("EXIF {} (auto-rotated)", d.orientation));
                }
                widgets::kv_row(ui, &p, "Decoder", &format!("{} · {} ms", d.decoder, l.load_ms));
                widgets::kv_row(ui, &p, "Modified", &catalog::format_time(l.modified));
            }
            let p2 = path.clone();
            if let Some(Some(ex)) = self.exif_cache.get(&path, ui.ctx(), move || crate::meta::read_exif(&p2)) {
                ui.add_space(6.0);
                widgets::section_title(ui, &p, "Camera");
                let rows: [(&str, &Option<String>); 10] = [
                    ("Camera", &ex.camera),
                    ("Lens", &ex.lens),
                    ("Exposure", &ex.exposure),
                    ("Aperture", &ex.aperture),
                    ("ISO", &ex.iso),
                    ("Focal length", &ex.focal),
                    ("Flash", &ex.flash),
                    ("Taken", &ex.taken),
                    ("Software", &ex.software),
                    ("Artist", &ex.artist),
                ];
                for (k, v) in rows {
                    if let Some(v) = v {
                        widgets::kv_row(ui, &p, k, v);
                    }
                }
                if let Some((lat, lon)) = ex.gps {
                    ui.horizontal(|ui| {
                        widgets::kv_row(ui, &p, "GPS", &format!("{lat:.5}, {lon:.5}"));
                    });
                    if ui.small_button(format!("{} Open location in browser", I::MAP_PIN)).on_hover_text("Opens OpenStreetMap in your web browser (the only time Lumen touches the network, and only when you click)").clicked() {
                        let url = format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=15/{lat}/{lon}");
                        let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
                    }
                }
                ui.add_space(4.0);
                egui::CollapsingHeader::new(RichText::new(format!("All metadata ({})", ex.all.len())).color(p.muted)).show(ui, |ui| {
                    egui::Grid::new("exif-all").num_columns(2).striped(true).show(ui, |ui| {
                        for (g, k, v) in &ex.all {
                            ui.label(RichText::new(format!("{k}")).size(11.5).color(p.muted)).on_hover_text(g);
                            ui.add(egui::Label::new(RichText::new(v).size(11.5)).wrap());
                            ui.end_row();
                        }
                    });
                });
            }
        });
    }

    fn tool_panel(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        let Some(t) = &mut self.tool else { return };
        // The preview base is prepared in the background (resize + histogram).
        if let Some(rx) = &t.base_rx {
            match rx.try_recv() {
                Ok(b) => {
                    t.base = Some(b);
                    t.base_rx = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    t.base_rx = None;
                    t.error = Some("Could not prepare the preview for this image.".into());
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        // Receive preview results. A panicking tool drops the sender: treat that
        // as a failed render so the panel never waits forever.
        if let Some(rx) = &t.rx {
            match rx.try_recv() {
                Ok((ver, img)) => {
                    t.running = None;
                    t.rx = None;
                    if ver >= t.preview_version {
                        t.preview_version = ver;
                        let pyr = render::build_pyramid(Arc::new(img));
                        t.preview = Some(GpuImage::new(ui.ctx(), &format!("toolprev{ver}"), pyr));
                    }
                    t.error = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // Do not retry the same parameters in a loop; wait for a change.
                    t.preview_version = t.preview_version.max(t.running.unwrap_or(0));
                    t.running = None;
                    t.rx = None;
                    t.error = Some("The preview failed with these settings. Try other values.".into());
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(t.tool.title()).font(super::theme::semibold(16.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if t.running.is_some() || t.final_rx.is_some() || t.base_rx.is_some() {
                    ui.spinner();
                }
            });
        });
        ui.add_space(8.0);
        let mut changed = false;
        if let Some(base) = &t.base {
            let size = self
                .edit
                .as_ref()
                .map(|e| e.current.dimensions())
                .or_else(|| core.loader.get_ready(&self.list[self.index]).map(|l| l.decoded.image.dimensions()))
                .unwrap_or((base.image.width(), base.image.height()));
            let cx = ToolUiCtx { palette: &p, image_size: size, histogram: &base.histogram, preview: Some(&base.image) };
            let w = ui.available_width();
            changed = egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 110.0)
                .show(ui, |ui| {
                    ui.set_max_width(w);
                    t.tool.ui(ui, &cx)
                })
                .inner;
        } else if t.base_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Preparing preview...").color(p.muted));
            });
        }
        if let Some(err) = &t.error {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{}  {err}", I::WARNING)).size(12.0).color(Color32::from_rgb(235, 110, 90)));
        }
        if changed {
            t.version += 1;
        }
        // Launch a preview render when parameters changed and no render is in flight.
        if let Some(base) = &t.base {
            if t.version > t.preview_version && t.running.is_none() {
                let tool = t.tool.clone_box();
                let img = base.image.clone();
                let scale = base.scale;
                let ver = t.version;
                let (tx, rx) = crossbeam_channel::bounded(1);
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.apply(&img, scale)));
                    if let Ok(out) = out {
                        let _ = tx.send((ver, out));
                    }
                    ctx.request_repaint();
                });
                t.rx = Some(rx);
                t.running = Some(ver);
            }
        }
        ui.add_space(10.0);
        let hold = ui.add(egui::Button::new(format!("{}  Hold to compare with original", I::EYE)).min_size(Vec2::new(ui.available_width(), 28.0)).sense(Sense::click_and_drag()));
        t.show_original = hold.is_pointer_button_down_on() || (ui.input(|i| i.key_down(Key::Backslash)) && !ui.ctx().egui_wants_keyboard_input());
        ui.add_space(6.0);
        let mut close = false;
        let mut apply = false;
        ui.horizontal(|ui| {
            if ui.button("Reset").clicked() {
                t.tool.reset();
                t.version += 1;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = t.final_rx.is_none() && t.base.is_some();
                if ui.add_enabled(enabled, egui::Button::new(RichText::new("Apply").color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(80.0, 28.0))).clicked() {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if keyboard_free(ui) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            close = true;
        }
        if keyboard_free(ui) && ui.input(|i| i.key_pressed(Key::Enter)) {
            apply = true;
        }
        if apply && t.final_rx.is_none() && t.base.is_some() {
            let full = self.edit.as_ref().filter(|e| e.path == self.list[self.index]).map(|e| e.current.clone()).or_else(|| core.loader.get_ready(&self.list[self.index]).map(|l| l.decoded.image.clone()));
            if let Some(full) = full {
                let tool = t.tool.clone_box();
                let (tx, rx) = crossbeam_channel::bounded(1);
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    if let Ok(out) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.apply(&full, 1.0))) {
                        let _ = tx.send(out);
                    }
                    ctx.request_repaint();
                });
                t.final_rx = Some(rx);
            }
        }
        if let Some(rx) = &t.final_rx {
            match rx.try_recv() {
                Ok(img) => {
                    let title = t.tool.title();
                    self.tool = None;
                    if let Some(sess) = self.ensure_edit(core) {
                        sess.push(img, &title);
                    }
                    self.auto_fit = Some(core.settings.fit_mode);
                    core.toasts.success(format!("{title} applied - Ctrl+Z to undo, Ctrl+S to save"));
                    return;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    t.final_rx = None;
                    t.error = Some("Applying the tool failed. The image was not changed.".into());
                    core.toasts.error(format!("{} failed", t.tool.title()));
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        if close {
            self.tool = None;
        }
        let _ = ev;
    }

    fn crop_panel(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        let Some(img) = self.current_image(core) else { return };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let Some(c) = &mut self.crop else { return };
        ui.label(RichText::new("Crop").font(super::theme::semibold(16.0)));
        ui.add_space(8.0);
        ui.label(RichText::new("Aspect ratio").color(p.muted));
        let presets: [(&str, Option<f32>); 10] = [
            ("Free", None),
            ("Original", Some(iw / ih)),
            ("1:1", Some(1.0)),
            ("4:3", Some(4.0 / 3.0)),
            ("3:2", Some(1.5)),
            ("16:9", Some(16.0 / 9.0)),
            ("5:4", Some(1.25)),
            ("3:4", Some(0.75)),
            ("2:3", Some(2.0 / 3.0)),
            ("9:16", Some(9.0 / 16.0)),
        ];
        ui.horizontal_wrapped(|ui| {
            for (name, a) in presets {
                let sel = match (c.aspect, a) {
                    (None, None) => true,
                    (Some(x), Some(y)) => (x - y).abs() < 1e-3,
                    _ => false,
                };
                if ui.selectable_label(sel, name).clicked() {
                    c.aspect = a;
                    if let Some(a) = a {
                        // Fit the current crop to the new aspect around its center.
                        let cx = (c.r[0] + c.r[2]) / 2.0;
                        let cy = (c.r[1] + c.r[3]) / 2.0;
                        let mut w = c.r[2] - c.r[0];
                        let mut h = w / a;
                        if h > ih {
                            h = ih;
                            w = h * a;
                        }
                        if w > iw {
                            w = iw;
                            h = w / a;
                        }
                        c.r = [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0];
                        clamp_crop(&mut c.r, iw, ih);
                    }
                }
            }
        });
        ui.add_space(8.0);
        let mut w = (c.r[2] - c.r[0]).round();
        let mut h = (c.r[3] - c.r[1]).round();
        let mut x = c.r[0].round();
        let mut y = c.r[1].round();
        egui::Grid::new("crop-grid").num_columns(4).show(ui, |ui| {
            ui.label("X");
            let cx = ui.add(egui::DragValue::new(&mut x).range(0.0..=iw - 1.0)).changed();
            ui.label("Y");
            let cy = ui.add(egui::DragValue::new(&mut y).range(0.0..=ih - 1.0)).changed();
            ui.end_row();
            ui.label("W");
            let cw = ui.add(egui::DragValue::new(&mut w).range(1.0..=iw)).changed();
            ui.label("H");
            let ch = ui.add(egui::DragValue::new(&mut h).range(1.0..=ih)).changed();
            ui.end_row();
            if cx || cy || cw || ch {
                if let Some(a) = c.aspect {
                    if cw {
                        h = w / a;
                    } else if ch {
                        w = h * a;
                    }
                }
                c.r = [x, y, x + w, y + h];
                clamp_crop(&mut c.r, iw, ih);
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new(format!("{} × {} px", (c.r[2] - c.r[0]).round(), (c.r[3] - c.r[1]).round())).color(p.muted));
        ui.add_space(12.0);
        ui.label(RichText::new("Drag inside to move, drag edges or corners to resize, drag outside to draw a new area. Enter applies, Esc cancels.").size(11.5).color(p.faint));
        ui.add_space(12.0);
        let mut apply = keyboard_free(ui) && ui.input(|i| i.key_pressed(Key::Enter));
        let mut cancel = keyboard_free(ui) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            if ui.add(egui::Button::new(RichText::new("Crop").color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(80.0, 28.0))).clicked() {
                apply = true;
            }
        });
        if apply {
            let r = c.r;
            self.crop = None;
            let (x, y) = (r[0].round().max(0.0) as u32, r[1].round().max(0.0) as u32);
            let (w, h) = ((r[2] - r[0]).round().max(1.0) as u32, (r[3] - r[1]).round().max(1.0) as u32);
            self.apply_simple(core, "Crop", |img| crate::edit::ops::crop(img, x, y, w, h));
            self.auto_fit = Some(core.settings.fit_mode);
        } else if cancel {
            self.crop = None;
        }
    }

    fn crop_overlay(&mut self, ui: &mut Ui, painter: &egui::Painter, dest: Rect, img: (u32, u32), resp: &egui::Response) {
        let Some(c) = &mut self.crop else { return };
        let (iw, ih) = (img.0 as f32, img.1 as f32);
        let to_screen = |x: f32, y: f32| Pos2::new(dest.min.x + x / iw * dest.width(), dest.min.y + y / ih * dest.height());
        let to_img = |p: Pos2| ((p.x - dest.min.x) / dest.width() * iw, (p.y - dest.min.y) / dest.height() * ih);
        let sr = Rect::from_min_max(to_screen(c.r[0], c.r[1]), to_screen(c.r[2], c.r[3]));
        // Dim outside
        let shade = Color32::from_black_alpha(150);
        painter.rect_filled(Rect::from_min_max(dest.min, Pos2::new(dest.max.x, sr.min.y)), 0.0, shade);
        painter.rect_filled(Rect::from_min_max(Pos2::new(dest.min.x, sr.max.y), dest.max), 0.0, shade);
        painter.rect_filled(Rect::from_min_max(Pos2::new(dest.min.x, sr.min.y), Pos2::new(sr.min.x, sr.max.y)), 0.0, shade);
        painter.rect_filled(Rect::from_min_max(Pos2::new(sr.max.x, sr.min.y), Pos2::new(dest.max.x, sr.max.y)), 0.0, shade);
        painter.rect_stroke(sr, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Middle);
        // Rule of thirds
        for i in 1..3 {
            let fx = sr.min.x + sr.width() * i as f32 / 3.0;
            let fy = sr.min.y + sr.height() * i as f32 / 3.0;
            painter.line_segment([Pos2::new(fx, sr.min.y), Pos2::new(fx, sr.max.y)], Stroke::new(1.0, Color32::from_white_alpha(70)));
            painter.line_segment([Pos2::new(sr.min.x, fy), Pos2::new(sr.max.x, fy)], Stroke::new(1.0, Color32::from_white_alpha(70)));
        }
        for pt in [sr.left_top(), sr.right_top(), sr.left_bottom(), sr.right_bottom()] {
            painter.rect_filled(Rect::from_center_size(pt, Vec2::splat(10.0)), 2.0, Color32::WHITE);
        }
        // Interaction
        let hit = |pos: Pos2| -> CropHandle {
            let m = 10.0;
            let near_l = (pos.x - sr.min.x).abs() < m;
            let near_r = (pos.x - sr.max.x).abs() < m;
            let near_t = (pos.y - sr.min.y).abs() < m;
            let near_b = (pos.y - sr.max.y).abs() < m;
            let in_y = pos.y > sr.min.y - m && pos.y < sr.max.y + m;
            let in_x = pos.x > sr.min.x - m && pos.x < sr.max.x + m;
            match (near_l && in_y, near_r && in_y, near_t && in_x, near_b && in_x) {
                (true, _, true, _) => CropHandle::NW,
                (_, true, true, _) => CropHandle::NE,
                (true, _, _, true) => CropHandle::SW,
                (_, true, _, true) => CropHandle::SE,
                (true, _, _, _) => CropHandle::W,
                (_, true, _, _) => CropHandle::E,
                (_, _, true, _) => CropHandle::N,
                (_, _, _, true) => CropHandle::S,
                _ if sr.contains(pos) => CropHandle::Move,
                _ => CropHandle::New,
            }
        };
        if let Some(pos) = resp.hover_pos() {
            let icon = match hit(pos) {
                CropHandle::NW | CropHandle::SE => egui::CursorIcon::ResizeNwSe,
                CropHandle::NE | CropHandle::SW => egui::CursorIcon::ResizeNeSw,
                CropHandle::W | CropHandle::E => egui::CursorIcon::ResizeHorizontal,
                CropHandle::N | CropHandle::S => egui::CursorIcon::ResizeVertical,
                CropHandle::Move => egui::CursorIcon::Move,
                CropHandle::New => egui::CursorIcon::Crosshair,
            };
            ui.ctx().set_cursor_icon(icon);
        }
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                c.drag = Some((hit(pos), pos, c.r));
            }
        }
        if resp.dragged() {
            if let (Some((h, start, orig)), Some(pos)) = (c.drag, resp.interact_pointer_pos()) {
                let (sx, sy) = to_img(start);
                let (px, py) = to_img(pos);
                let (dx, dy) = (px - sx, py - sy);
                let mut r = orig;
                match h {
                    CropHandle::Move => {
                        let w = r[2] - r[0];
                        let hh = r[3] - r[1];
                        r[0] = (orig[0] + dx).clamp(0.0, iw - w);
                        r[1] = (orig[1] + dy).clamp(0.0, ih - hh);
                        r[2] = r[0] + w;
                        r[3] = r[1] + hh;
                    }
                    CropHandle::New => {
                        r = [sx.min(px), sy.min(py), sx.max(px), sy.max(py)];
                        if let Some(a) = c.aspect {
                            let w = r[2] - r[0];
                            let hgt = w / a;
                            if py >= sy { r[3] = r[1] + hgt } else { r[1] = r[3] - hgt }
                        }
                    }
                    _ => {
                        if matches!(h, CropHandle::W | CropHandle::NW | CropHandle::SW) {
                            r[0] = orig[0] + dx;
                        }
                        if matches!(h, CropHandle::E | CropHandle::NE | CropHandle::SE) {
                            r[2] = orig[2] + dx;
                        }
                        if matches!(h, CropHandle::N | CropHandle::NW | CropHandle::NE) {
                            r[1] = orig[1] + dy;
                        }
                        if matches!(h, CropHandle::S | CropHandle::SW | CropHandle::SE) {
                            r[3] = orig[3] + dy;
                        }
                        if let Some(a) = c.aspect {
                            let w = (r[2] - r[0]).abs();
                            let hgt = w / a;
                            if matches!(h, CropHandle::N | CropHandle::NW | CropHandle::NE) {
                                r[1] = r[3] - hgt;
                            } else {
                                r[3] = r[1] + hgt;
                            }
                        }
                    }
                }
                if r[0] > r[2] {
                    r.swap(0, 2);
                }
                if r[1] > r[3] {
                    r.swap(1, 3);
                }
                clamp_crop(&mut r, iw, ih);
                c.r = r;
            }
        }
        if resp.drag_stopped() {
            c.drag = None;
        }
    }

    fn discard_dialog(&mut self, ctx: &egui::Context, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        egui::Modal::new(egui::Id::new("discard-edits")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Save changes?");
            ui.add_space(6.0);
            ui.label("This image has unsaved edits.");
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.confirm_discard = false;
                    self.pending_nav = None;
                }
                if ui.button("Discard").clicked() {
                    self.confirm_discard = false;
                    self.edit = None;
                    self.gpu = None;
                    if let Some(d) = self.pending_nav.take() {
                        self.step(d, core.settings.loop_navigation, core.settings.fit_mode);
                    }
                }
                if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(p.accent)).clicked() {
                    self.confirm_discard = false;
                    ev.push(ViewerEvent::Command(Command::Save));
                }
            });
        });
    }
}

/// True when no text field has keyboard focus and no popup/menu is open, so
/// Esc / Enter belong to the viewer's tool panels.
fn keyboard_free(ui: &Ui) -> bool {
    !ui.ctx().egui_wants_keyboard_input() && !egui::Popup::is_any_open(ui.ctx())
}

fn frame_delay(frames: &[crate::decode::AnimFrame], i: usize) -> Duration {
    // The decoder already maps tiny delays to 100 ms (browser convention); the
    // floor here only guards against a repaint storm from other sources.
    Duration::from_millis(frames.get(i).map(|f| f.delay_ms).unwrap_or(100).max(20) as u64)
}

/// Advance an animation that is due (`now >= next_at`). Deadlines accumulate
/// from the previous deadline rather than from `now`, so frame timing does not
/// drift by a frame's render latency each step; when far behind (window was
/// hidden, a slow frame), it resynchronizes instead of fast-forwarding.
pub fn advance_animation(frame: usize, next_at: Instant, now: Instant, delay: impl Fn(usize) -> Duration, n: usize) -> (usize, Instant) {
    let n = n.max(1);
    let f = (frame + 1) % n;
    let mut next = next_at + delay(f);
    if next <= now {
        next = now + delay(f);
    }
    (f, next)
}

/// Zoom factors must stay finite and positive (a collapsed view gives 0 or NaN,
/// and `f32::clamp` panics on NaN bounds).
fn sane_zoom(z: f32) -> f32 {
    if z.is_finite() && z > 0.0 {
        z.clamp(0.001, 64.0)
    } else {
        1.0
    }
}

fn clamp_crop(r: &mut [f32; 4], iw: f32, ih: f32) {
    r[0] = r[0].clamp(0.0, iw - 1.0);
    r[1] = r[1].clamp(0.0, ih - 1.0);
    r[2] = r[2].clamp(r[0] + 1.0, iw);
    r[3] = r[3].clamp(r[1] + 1.0, ih);
}

fn menu_item(ui: &mut Ui, c: Command, ev: &mut Vec<ViewerEvent>) {
    let r = ui.add(egui::Button::new(format!("{}   {}", c.icon(), c.label())).shortcut_text(c.shortcut()));
    if r.clicked() {
        ev.push(ViewerEvent::Command(c));
        ui.close();
    }
}

fn edge_menu_item(ui: &mut Ui, c: Command, ev: &mut Vec<ViewerEvent>) {
    let text = RichText::new(format!("{}   {}", c.icon(), c.label())).size(12.5);
    let r = ui.add_sized([ui.available_width(), 24.0], egui::Button::new(text).frame(false).shortcut_text(RichText::new(c.shortcut()).size(11.0).weak()));
    if r.clicked() {
        ev.push(ViewerEvent::Command(c));
    }
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

#[allow(dead_code)]
fn _palette_unused(_: &Palette) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1000.0, 600.0))
    }

    #[test]
    fn edge_detection() {
        let v = view();
        assert_eq!(edge_at(v, Pos2::new(500.0, 2.0), 6.0), Some(Edge::Top));
        assert_eq!(edge_at(v, Pos2::new(997.0, 300.0), 6.0), Some(Edge::Right));
        assert_eq!(edge_at(v, Pos2::new(1000.0, 300.0), 6.0), Some(Edge::Right));
        assert_eq!(edge_at(v, Pos2::new(0.0, 300.0), 6.0), Some(Edge::Left));
        assert_eq!(edge_at(v, Pos2::new(500.0, 598.0), 6.0), Some(Edge::Bottom));
        assert_eq!(edge_at(v, Pos2::new(500.0, 300.0), 6.0), None);
        assert_eq!(edge_at(v, Pos2::new(500.0, 7.0), 6.0), None);
        // Corner: the nearer edge wins.
        assert_eq!(edge_at(v, Pos2::new(1.0, 4.0), 6.0), Some(Edge::Left));
        assert_eq!(edge_at(v, Pos2::new(5.0, 1.0), 6.0), Some(Edge::Top));
        // Outside the view.
        assert_eq!(edge_at(v, Pos2::new(500.0, -20.0), 6.0), None);
        assert_eq!(edge_at(Rect::NOTHING, Pos2::ZERO, 6.0), None);
    }

    #[test]
    fn wheel_nav_mouse_steps_per_notch() {
        let mut w = WheelNav::default();
        assert_eq!(w.feed(-40.0, false, 0.0), -1);
        assert_eq!(w.feed(-40.0, false, 0.05), -1);
        assert_eq!(w.feed(-40.0, false, 0.10), -1);
        // Direction change steps immediately.
        assert_eq!(w.feed(40.0, false, 0.15), 1);
        // Two notches in one frame still give one step (no burst).
        let mut w = WheelNav::default();
        assert_eq!(w.feed(-80.0, false, 0.0), -1);
        assert_eq!(w.feed(0.0, false, 0.01), 0);
    }

    #[test]
    fn wheel_nav_touchpad_steps_once_per_swipe() {
        let mut w = WheelNav::default();
        let mut steps = 0;
        let mut t = 0.0;
        // A long smooth swipe: 60 frames of 8 points.
        for _ in 0..60 {
            steps += w.feed(-8.0, true, t);
            t += 0.016;
        }
        assert_eq!(steps, -1);
        // After a pause a new swipe steps again.
        t += 0.5;
        let mut steps = 0;
        for _ in 0..20 {
            steps += w.feed(-8.0, true, t);
            t += 0.016;
        }
        assert_eq!(steps, -1);
        // Small jitter never navigates.
        let mut w = WheelNav::default();
        assert_eq!(w.feed(3.0, true, 0.0) + w.feed(-3.0, true, 0.02) + w.feed(2.0, true, 0.04), 0);
    }

    #[test]
    fn precise_scroll_classification() {
        use egui::MouseWheelUnit as U;
        assert!(!is_precise_scroll(U::Line, Vec2::new(0.0, 1.0)));
        assert!(!is_precise_scroll(U::Line, Vec2::new(0.0, -3.0)));
        assert!(is_precise_scroll(U::Line, Vec2::new(0.0, 0.25)));
        assert!(is_precise_scroll(U::Line, Vec2::new(0.0, 1.4)));
        assert!(is_precise_scroll(U::Line, Vec2::new(0.5, 0.0)));
        assert!(is_precise_scroll(U::Point, Vec2::new(0.0, 40.0)));
        assert!(!is_precise_scroll(U::Page, Vec2::new(0.0, 1.0)));
    }

    #[test]
    fn locked_center_keeps_relative_position() {
        assert_eq!(locked_center(Vec2::new(0.25, 0.5), (4000, 3000)), Vec2::new(1000.0, 1500.0));
        assert_eq!(locked_center(Vec2::new(1.5, -1.0), (100, 50)), Vec2::new(100.0, 0.0));
    }

    #[test]
    fn lock_zoom_keeps_view_across_steps() {
        let mut v = Viewer::new();
        v.list = vec![PathBuf::from("a.jpg"), PathBuf::from("b.jpg"), PathBuf::from("c.jpg")];
        v.view_img = (2000, 1000);
        v.auto_fit = None;
        v.zoom = 2.0;
        v.center = Vec2::new(500.0, 250.0);
        assert!(v.toggle_lock_zoom());
        v.step(1, false, FitMode::FitLarge);
        assert_eq!(v.index, 1);
        assert!(v.auto_fit.is_none());
        let (z, n) = v.pending_lock.unwrap();
        assert_eq!(z, 2.0);
        assert_eq!(n, Vec2::new(0.25, 0.25));
        // Stepping again before the image loaded keeps the original lock.
        v.go_to(2, FitMode::FitLarge);
        assert_eq!(v.pending_lock.unwrap(), (2.0, Vec2::new(0.25, 0.25)));
        // Unlocked: back to fit.
        v.toggle_lock_zoom();
        v.step(-1, false, FitMode::FitLarge);
        assert_eq!(v.auto_fit, Some(FitMode::FitLarge));
        assert!(v.pending_lock.is_none());
        // Locked but fitted: stays fitted.
        v.toggle_lock_zoom();
        v.step(-1, false, FitMode::FitAlways);
        assert_eq!(v.auto_fit, Some(FitMode::FitAlways));
    }

    #[test]
    fn sane_zoom_rejects_bad_values() {
        assert_eq!(sane_zoom(f32::NAN), 1.0);
        assert_eq!(sane_zoom(0.0), 1.0);
        assert_eq!(sane_zoom(-2.0), 1.0);
        assert_eq!(sane_zoom(f32::INFINITY), 1.0);
        assert_eq!(sane_zoom(0.5), 0.5);
        assert_eq!(sane_zoom(1000.0), 64.0);
        let v = Viewer::new();
        assert_eq!(v.fit_zoom(FitMode::FitAlways, (100, 100), Vec2::ZERO, 1.0), 1.0);
    }

    #[test]
    fn animation_deadlines_do_not_drift() {
        let t0 = Instant::now();
        let d = |_| Duration::from_millis(100);
        // Rendered 7 ms late: the next deadline still lands on the 100 ms grid.
        let (f, next) = advance_animation(0, t0 + Duration::from_millis(100), t0 + Duration::from_millis(107), d, 3);
        assert_eq!(f, 1);
        assert_eq!(next, t0 + Duration::from_millis(200));
        // Wraps around.
        assert_eq!(advance_animation(2, t0, t0, d, 3).0, 0);
        // Far behind (window was hidden): resync rather than fast-forward.
        let now = t0 + Duration::from_secs(5);
        let (_, next) = advance_animation(0, t0, now, d, 3);
        assert_eq!(next, now + Duration::from_millis(100));
    }
}
