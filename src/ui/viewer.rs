//! Viewer mode: smooth zoom/pan on images of any size, magnifier, crop, live
//! preview editing, info panel, filmstrip and animation playback.

use super::commands::Command;
use super::roi::{ImageTransform, RoiEditor, RoiOptions};
use super::icons as I;
use super::theme::Palette;
use super::widgets;
use crate::app::Core;
use crate::catalog;
use crate::edit::{EditSession, EditTool, Histogram, HistogramPaint, ToolUiCtx};
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
    /// Number of frames this state was built for (`frame_tex` is empty for
    /// large animations, so its length cannot stand in for this).
    frame_count: usize,
}

impl Anim {
    /// Whether this playback state belongs to a different animation.
    fn is_stale(&self, path: &Path, frame_count: usize) -> bool {
        self.path != path || self.frame_count != frame_count
    }
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
    /// The user changed something (parameters, strokes, shapes) since the tool
    /// opened or was reset: closing it without Apply asks first.
    touched: bool,
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

pub struct CropState {
    /// The area (in pixels of the displayed image; rescaled when that changes,
    /// e.g. right after a rotate).
    pub roi: RoiEditor,
    /// The area the crop started with (in the same pixels): anything else is the user's work.
    initial: [f32; 4],
}

impl CropState {
    /// x0, y0, x1, y1 in image pixels.
    pub fn rect(&self) -> [f32; 4] {
        self.roi.rect().unwrap_or(self.initial)
    }

    fn touched(&self) -> bool {
        self.rect() != self.initial || self.roi.aspect().is_some()
    }
}

/// What runs once the user answers the discard prompt with Discard.
enum Pending {
    /// Move by this many images.
    Step(isize),
    /// Leave the viewer.
    Close,
    /// Close the open tool or crop (Esc).
    CancelTool,
    /// Open another tool, or crop, in place of the current one.
    OpenTool(Box<dyn EditTool>),
    StartCrop,
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
    /// The loader keep set from the last `request_loads` (current first).
    keep: Vec<PathBuf>,
    /// After a size-changing edit (crop, resize, rotate): the displayed size
    /// at that moment and the fit mode to re-apply once the new size shows
    /// (the GPU copy of the result is built asynchronously, so fitting at
    /// apply time would fit the old size).
    refit_on_size: Option<((u32, u32), FitMode)>,
    /// Neighbours found too big to keep preloaded (huge panoramas): only
    /// decoded when actually shown.
    no_preload: std::collections::HashSet<PathBuf>,
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
    /// The action waiting for the discard prompt (`confirm_discard`).
    pending: Option<Pending>,
    pub confirm_discard: bool,
    /// A text field had keyboard focus when the tool panels were last drawn.
    /// egui drops focus on Esc before this frame's widgets run, so the Esc
    /// that only leaves a field must not also cancel the tool.
    text_was_focused: bool,
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
            keep: Vec::new(),
            refit_on_size: None,
            no_preload: Default::default(),
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
            pending: None,
            confirm_discard: false,
            text_was_focused: false,
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

    /// An open tool or crop holds changes that are not applied yet.
    pub fn tool_has_work(&self) -> bool {
        self.tool.as_ref().is_some_and(|t| t.touched) || self.crop.as_ref().is_some_and(|c| c.touched())
    }

    /// Anything leaving this image would lose: unsaved edits or unapplied tool work.
    /// Drop the on-screen image's GPU tiles and mip pyramid when it is larger
    /// than `max_bytes` (RGBA), so leaving the viewer frees huge photos.
    pub fn release_gpu_over(&mut self, max_bytes: u64) {
        if self.gpu.as_ref().is_some_and(|(_, _, g)| g.width as u64 * g.height as u64 * 4 > max_bytes) {
            self.gpu = None;
            self.gpu_build = None;
        }
    }

    pub fn has_unsaved_work(&self) -> bool {
        self.is_dirty() || self.tool_has_work()
    }

    /// Close the open tool or crop without applying it.
    pub fn close_tool(&mut self) {
        self.tool = None;
        self.crop = None;
    }

    /// Ask before leaving the viewer (unsaved edits or unapplied tool work).
    pub fn confirm_close(&mut self) {
        self.ask(Pending::Close);
    }

    fn ask(&mut self, what: Pending) {
        self.pending = Some(what);
        self.confirm_discard = true;
    }

    /// Undo inside the open edit tool (strokes, points). False when nothing changed.
    pub fn tool_undo(&mut self) -> bool {
        let Some(t) = &mut self.tool else { return false };
        let changed = t.tool.undo();
        if changed {
            t.version += 1;
            t.touched = true;
        }
        changed
    }

    pub fn tool_redo(&mut self) -> bool {
        let Some(t) = &mut self.tool else { return false };
        let changed = t.tool.redo();
        if changed {
            t.version += 1;
            t.touched = true;
        }
        changed
    }

    /// Navigate by `delta` images. Returns false if blocked by unsaved edits
    /// or unapplied tool work (the discard prompt asks first).
    pub fn step(&mut self, delta: isize, wrap: bool, fit: FitMode) -> bool {
        if self.list.is_empty() {
            return true;
        }
        if self.has_unsaved_work() {
            self.ask(Pending::Step(delta));
            return false;
        }
        let n = self.list.len() as isize;
        let mut i = self.index as isize + delta;
        if wrap {
            i = i.rem_euclid(n);
        } else {
            i = i.clamp(0, (n - 1).max(0));
        }
        if i as usize != self.index {
            self.index = i as usize;
            self.refit_on_size = None;
            self.edit = None;
            self.tool = None;
            self.crop = None;
            self.after_index_change(fit);
        }
        true
    }

    /// Show image `index` (filmstrip, end of a slideshow); asks first like `step`.
    pub fn go_to(&mut self, index: usize, fit: FitMode) {
        if index >= self.list.len() || index == self.index {
            return;
        }
        if self.has_unsaved_work() {
            self.ask(Pending::Step(index as isize - self.index as isize));
            return;
        }
        self.index = index;
        self.edit = None;
        self.tool = None;
        self.crop = None;
        self.after_index_change(fit);
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

    /// After Save as to `path`: show that file (added after the current
    /// image unless the list has it) with no edit session, as if just
    /// opened. The view stays: the saved copy has the edited image's size.
    pub fn show_saved_copy(&mut self, path: &Path) {
        self.index = match self.list.iter().position(|p| p == path) {
            Some(i) => i,
            None => {
                let at = (self.index + 1).min(self.list.len());
                self.list.insert(at, path.to_path_buf());
                at
            }
        };
        self.refit_on_size = None;
        self.edit = None;
        self.tool = None;
        self.crop = None;
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
        let resized = out.dimensions() != sess.current.dimensions();
        sess.push(out, what);
        // Same size (flip, auto enhance): keep the zoom and position.
        if resized {
            self.refit_on_size = Some((self.view_img, self.auto_fit.unwrap_or(FitMode::FitLarge)));
        }
        true
    }

    pub fn open_tool(&mut self, tool: Box<dyn EditTool>, core: &Core) {
        let Some(img) = self.current_image(core) else { return };
        if self.tool_has_work() {
            self.ask(Pending::OpenTool(tool));
            return;
        }
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
        self.tool = Some(ActiveTool { tool, base: None, base_rx: Some(rx), version: 1, preview: None, preview_version: 0, rx: None, running: None, final_rx: None, show_original: false, error: None, touched: false });
    }

    pub fn start_crop(&mut self, core: &Core) {
        let Some(img) = self.current_image(core) else { return };
        if self.tool_has_work() {
            self.ask(Pending::StartCrop);
            return;
        }
        self.tool = None;
        let (w, h) = (img.width() as f32, img.height() as f32);
        let roi = RoiEditor::new((w, h), RoiOptions::crop()).with_rect([w * 0.1, h * 0.1, w * 0.9, h * 0.9]);
        let initial = roi.rect().unwrap_or_default();
        self.crop = Some(CropState { roi, initial });
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

    /// Shows `r` (fractions 0..1 of the image: x0, y0, x1, y1) with some
    /// context around it (Analysis panel: a finding clicked in the list).
    pub fn zoom_to_region(&mut self, r: [f32; 4], ppp: f32) {
        let (w, h) = (self.view_img.0.max(1) as f32, self.view_img.1.max(1) as f32);
        let view = self.last_view_rect;
        let (rw, rh) = (((r[2] - r[0]) * w).max(16.0), ((r[3] - r[1]) * h).max(16.0));
        let z = (view.width() * ppp / rw).min(view.height() * ppp / rh) * 0.6;
        self.auto_fit = None;
        self.zoom_target = None;
        self.zoom = sane_zoom(z.min(8.0));
        self.center = Vec2::new((r[0] + r[2]) / 2.0 * w, (r[1] + r[3]) / 2.0 * h);
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
        let mut keep = self.preload_set(&core.loader, core.settings.preload_neighbors);
        // A preloaded neighbour that turned out huge is dropped again and not
        // preloaded any more (it is decoded when the user moves to it).
        let limit = preload_limit(core.loader.budget_bytes);
        let mut dropped = false;
        for p in keep.iter().skip(1) {
            if core.loader.bytes_of(p).is_some_and(|b| b > limit) {
                self.no_preload.insert(p.clone());
                core.loader.invalidate(p);
                dropped = true;
            }
        }
        if dropped {
            keep = self.preload_set(&core.loader, core.settings.preload_neighbors);
        }
        for (pr, p) in keep.iter().enumerate().skip(1) {
            core.loader.request(p, pr as u32, None);
        }
        core.loader.retain_queued(&keep);
        self.keep = keep;
    }

    /// The paths the viewer wants cached, most important first: the current
    /// image, then neighbours (next, previous, next + 1) as long as the
    /// estimated total fits the loader's memory budget. Unknown neighbour
    /// sizes are estimated from the current image.
    fn preload_set(&self, loader: &crate::loader::Loader, preload: bool) -> Vec<PathBuf> {
        let Some(cur) = self.current().cloned() else { return Vec::new() };
        let mut keep = vec![cur.clone()];
        if !preload {
            return keep;
        }
        let estimate = loader.bytes_of(&cur);
        let mut total = estimate.unwrap_or(0);
        for d in [1isize, -1, 2] {
            let i = self.index as isize + d;
            if i < 0 || i as usize >= self.list.len() {
                continue;
            }
            let p = &self.list[i as usize];
            if !crate::formats::is_image_path(p) || keep.contains(p) || self.no_preload.contains(p) {
                continue;
            }
            if loader.bytes_of(p).is_some_and(|b| b > preload_limit(loader.budget_bytes)) {
                continue; // a decoded neighbour too big to keep preloaded
            }
            let Some(b) = loader.bytes_of(p).or(estimate) else {
                // Current size unknown yet: preload only the next image.
                if keep.len() == 1 {
                    keep.push(p.clone());
                }
                continue;
            };
            if total.saturating_add(b) > loader.budget_bytes {
                break;
            }
            total += b;
            keep.push(p.clone());
        }
        keep
    }

    /// Paths the loader must not evict for the memory budget: the image on
    /// screen and the neighbours being preloaded for it.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        match self.current() {
            Some(cur) if self.keep.first() == Some(cur) => self.keep.clone(),
            cur => cur.cloned().into_iter().collect(),
        }
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
                .show(ui, |ui| self.filmstrip(ui, core));
        }
        self.text_was_focused = ui.ctx().text_edit_focused();

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
            if let Some((old, mode)) = self.refit_on_size {
                if sz != old {
                    self.refit_on_size = None;
                    self.pending_lock = None;
                    self.zoom_target = None;
                    self.auto_fit = Some(mode);
                }
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
            crate::analysis_ui::canvas::publish(ui.ctx(), crate::analysis_ui::canvas::Slot::A, dest, disp, view);
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
            // Edit-tool canvas hook (interactive tools: draw board, retouch,
            // regions): overlays + pointer input in image coordinates.
            if let Some(t) = &mut self.tool {
                if t.tool.wants_canvas() && !t.tool.changes_size() {
                    let cx = crate::edit::CanvasCtx {
                        palette: &p,
                        image_size: sz,
                        dest,
                        view,
                        preview: t.base.as_ref().map(|b| &*b.image),
                        preview_scale: t.scale(),
                        preview_current: t.preview_version >= t.version,
                        interactive: !ui.input(|i| i.key_down(Key::Space)),
                    };
                    if t.tool.canvas(&cx, &painter, &resp) {
                        t.version += 1;
                        t.touched = true;
                        ui.ctx().request_repaint();
                    }
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
        // The caption sits top-left: hide it while a top/left pop-out panel covers that corner.
        let caption_covered = matches!(self.edge.open, Some(Edge::Top) | Some(Edge::Left));
        if (core.settings.show_info_overlay || !fullscreen) && !caption_covered {
            self.overlay_caption(ui, core, view, size, &path);
        }
        if self.show_histogram {
            self.histogram_overlay(ui, core, view, &path);
        }
        let edge_open = self.edge_panels_ui(ui, core, view, &mut ev);
        let idle = self.last_activity.elapsed() > Duration::from_millis(2200);
        let near_bottom = ui.input(|i| i.pointer.hover_pos()).map(|pos| pos.y > view.max.y - 90.0 && view.contains(pos)).unwrap_or(false);
        if !idle || near_bottom || !fullscreen {
            self.bottom_bar(ui, core, view, size.is_some(), &mut ev);
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
        if self.crop.is_none() && !self.tool_grabs_pointer(ui) {
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
        self.center.x = if w <= half_vw * 2.0 { w / 2.0 } else { safe_clamp(self.center.x, half_vw, w - half_vw) };
        self.center.y = if h <= half_vh * 2.0 { h / 2.0 } else { safe_clamp(self.center.y, half_vh, h - half_vh) };
    }

    /// Edit-tool canvas hook: an interactive tool owns primary clicks/drags
    /// (hold Space to pan instead).
    fn tool_grabs_pointer(&self, ui: &Ui) -> bool {
        self.tool.as_ref().is_some_and(|t| t.tool.wants_canvas()) && !ui.input(|i| i.key_down(Key::Space))
    }

    fn nav_input(&mut self, ui: &mut Ui, resp: &egui::Response, core: &mut Core, ev: &mut Vec<ViewerEvent>, can_pan: bool) {
        if !resp.hovered() && !resp.contains_pointer() {
            return;
        }
        let (si, ctrl, now) = ui.input(|i| (scroll_input(i), i.modifiers.ctrl, i.time));
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let ppp = ui.ctx().pixels_per_point();
        // While a tool or crop is open the wheel always zooms (the panels say
        // so): stepping to another image would throw the tool's work away.
        let zoom_mode = core.settings.wheel_action == WheelAction::Zoom;
        let tool_open = self.has_tool();
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
            } else if tool_open || ctrl != zoom_mode {
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
        if resp.double_clicked() && !tool_open {
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
            // Opens the Analysis panel (never closes it, unlike Ctrl+Q).
            let analysis = Command::ToggleAnalysis;
            if ui.add(egui::Button::new(format!("{}   Analyze...", analysis.icon())).shortcut_text(analysis.shortcut())).clicked() {
                core.analysis_request = Some(crate::analysis_ui::PanelRequest::Open);
                ui.close();
            }
            ui.separator();
            for c in [Command::ZoomFit, Command::ZoomActual, Command::ToggleFullscreen] {
                menu_item(ui, c, ev);
            }
            ui.separator();
            for c in [Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::Crop, Command::Resize] {
                menu_item(ui, c, ev);
            }
            ui.menu_button(format!("{}   Edit", I::SLIDERS_HORIZONTAL), |ui| {
                for (gi, g) in Command::EDIT_GROUPS.iter().enumerate() {
                    if gi > 0 {
                        ui.separator();
                    }
                    for c in g.iter() {
                        menu_item(ui, *c, ev);
                    }
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
        if self.anim.as_ref().map(|a| a.is_stale(path, frames.len())).unwrap_or(true) {
            let total: u64 = frames.iter().map(|f| f.image.width() as u64 * f.image.height() as u64).sum();
            let frame_tex = if total <= ANIM_CACHE_PIXELS { vec![None; frames.len()] } else { Vec::new() };
            self.anim = Some(Anim { path: path.to_path_buf(), frame: 0, next_at: now + frame_delay(frames, 0), playing: !frames.iter().all(|f| f.delay_ms == 0), frame_tex, tex: None, shown_frame: usize::MAX, frame_count: frames.len() });
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
            // Pages of a document can differ in size: fit each one inside `dest`.
            let (fw, fh) = (frames[a.frame].image.width().max(1) as f32, frames[a.frame].image.height().max(1) as f32);
            let k = (dest.width() / fw).min(dest.height() / fh);
            let r = Rect::from_center_size(dest.center(), Vec2::new(fw * k, fh * k));
            let mut mesh = egui::Mesh::with_texture(t);
            mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
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
                // Size to the menu's content (about 29 rows) instead of the full
                // height; it scrolls when the window is shorter.
                let h = (view.height() - 16.0 - bottom_reserve).clamp(120.0, 790.0);
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
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(margin as i8))
                    .show(ui, |ui| {
                        let inner = rect.size() - Vec2::splat(margin * 2.0);
                        ui.set_min_size(inner);
                        ui.set_max_size(inner);
                        match edge {
                            Edge::Top => {
                                ui.allocate_ui(inner, |ui| self.filmstrip(ui, core));
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
        let dirty = self.is_dirty();
        // A solid bar takes its own column when the window is too short for
        // the whole menu; a floating one would cover the shortcut column.
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        egui::ScrollArea::vertical().id_salt("edge-tools").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new("Edit").font(super::theme::semibold(13.0)));
            ui.add_space(4.0);
            for (gi, g) in Command::EDIT_GROUPS.iter().enumerate() {
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
        // No zoom while nothing is shown (still loading, or not decodable).
        if let Some((w, h)) = size {
            parts.push(format!("{w} × {h}"));
            parts.push(format!("{:.0}%{}", self.zoom * 100.0, if self.lock_zoom { " (locked)" } else { "" }));
        }
        if self.is_dirty() {
            parts.push("● edited".into());
        }
        if let Some((f, n, playing)) = self.animation_info(core) {
            parts.push(format!("frame {}/{}{}", f + 1, n, if playing { "" } else { " (paused)" }));
        }
        let text = parts.join("   ·   ");
        // Wrap inside the image area: a long name at a large UI scale must not
        // run under the tool / info panel beside it.
        let wrap = (view.width() - 44.0).max(40.0);
        let galley = ui.painter().layout(text, FontId::proportional(12.5), Color32::from_white_alpha(230), wrap);
        let r = Rect::from_min_size(view.min + Vec2::new(12.0, 12.0), galley.size() + Vec2::new(20.0, 10.0));
        let painter = ui.painter().with_clip_rect(view);
        painter.rect_filled(r, CornerRadius::same(8), Color32::from_black_alpha(150));
        painter.galley(r.min + Vec2::new(10.0, 5.0), galley, p.text);
    }

    /// `shown`: an image is on screen (no zoom figure otherwise).
    fn bottom_bar(&mut self, ui: &mut Ui, core: &mut Core, view: Rect, shown: bool, ev: &mut Vec<ViewerEvent>) {
        use BarItem as B;
        let p = core.pal;
        // Fit the bar to the view: drop separators first, then move the least
        // important buttons into a "..." menu (never Prev / Next / zoom / fit).
        let avail = (view.width() - 20.0 - 16.0).max(0.0);
        let (items, hidden) = bar_layout(avail);
        let content: f32 = items.iter().map(|i| i.width()).sum();
        let w = (content + 16.0).min(view.width() - 8.0).max(0.0);
        let rect = Rect::from_center_size(Pos2::new(view.center().x, view.max.y - 34.0), Vec2::new(w, 44.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(rect.intersect(view));
        child.painter().rect_filled(rect, CornerRadius::same(12), p.overlay);
        child.painter().rect_stroke(rect, CornerRadius::same(12), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        child.add_space(8.0);
        child.spacing_mut().item_spacing.x = 1.0;
        let dirty = self.is_dirty();
        let analysis_open = core.analysis_open;
        for item in items {
            let cmd = self.bar_command(item, dirty);
            match item {
                B::Sep => widgets::separator_v(&mut child),
                B::ZoomPct => {
                    let pct = if shown { format!("{:.0}%", self.zoom * 100.0) } else { "-".into() };
                    let zl = child.add_enabled_ui(shown, |ui| ui.add_sized([54.0, 26.0], egui::Button::new(RichText::new(pct).size(12.0)).frame(false))).inner;
                    if zl.clicked() {
                        ev.push(ViewerEvent::Command(Command::ZoomActual));
                    }
                    zl.on_hover_text("Click: 100%");
                }
                B::More => {
                    child.menu_button(RichText::new(I::DOTS_THREE).size(17.0), |ui| {
                        for &h in &hidden {
                            if let Some(c) = self.bar_command(h, dirty) {
                                menu_item(ui, c, ev);
                            }
                        }
                    });
                }
                _ => {
                    let Some(cmd) = cmd else { continue };
                    let (icon, tip, sel) = self.bar_button(item, cmd, dirty);
                    let sel = sel || (item == B::Analysis && analysis_open);
                    if widgets::icon_button(&mut child, icon, &tip, sel).clicked() {
                        if item == B::Lock {
                            self.toggle_lock_zoom();
                        } else {
                            ev.push(ViewerEvent::Command(cmd));
                        }
                    }
                }
            }
        }
    }

    /// The command behind a toolbar item.
    fn bar_command(&self, item: BarItem, dirty: bool) -> Option<Command> {
        use BarItem as B;
        Some(match item {
            B::Browser => Command::CloseViewer,
            B::Prev => Command::PrevImage,
            B::Next => Command::NextImage,
            B::ZoomOut => Command::ZoomOut,
            B::ZoomIn => Command::ZoomIn,
            B::Fit => Command::ZoomFit,
            B::Lock => Command::ToggleLockZoom,
            B::RotL => Command::RotateLeft,
            B::RotR => Command::RotateRight,
            B::Crop => Command::Crop,
            B::Adjust => Command::AdjustColors,
            B::Resize => Command::Resize,
            B::Save => {
                if dirty {
                    Command::Save
                } else {
                    Command::SaveAs
                }
            }
            B::Delete => Command::Delete,
            B::Info => Command::ToggleInfo,
            B::Analysis => Command::ToggleAnalysis,
            B::Film => Command::ToggleFilmstrip,
            B::Slideshow => Command::Slideshow,
            B::Fullscreen => Command::ToggleFullscreen,
            B::Sep | B::ZoomPct | B::More => return None,
        })
    }

    /// Icon, tooltip and selected state of a toolbar button.
    fn bar_button(&self, item: BarItem, cmd: Command, dirty: bool) -> (&'static str, String, bool) {
        use BarItem as B;
        let plain = |icon: &'static str, tip: &str| (icon, format!("{tip}  ({})", cmd.shortcut()), false);
        match item {
            B::Browser => plain(I::SQUARES_FOUR, "Back to browser"),
            B::Prev => plain(I::CARET_LEFT, "Previous"),
            B::Next => plain(I::CARET_RIGHT, "Next"),
            B::ZoomOut => plain(I::MAGNIFYING_GLASS_MINUS, "Zoom out"),
            B::ZoomIn => plain(I::MAGNIFYING_GLASS_PLUS, "Zoom in"),
            B::Fit => (I::ARROWS_IN, "Fit to window (/)".into(), self.auto_fit.is_some()),
            B::Lock => {
                if self.lock_zoom {
                    (I::LOCK_SIMPLE, "Zoom ratio locked: next/previous keep zoom and position (click to unlock)".into(), true)
                } else {
                    (I::LOCK_SIMPLE_OPEN, "Lock zoom ratio: keep zoom and position when changing images".into(), false)
                }
            }
            B::RotL => plain(I::ARROW_COUNTER_CLOCKWISE, "Rotate left"),
            B::RotR => plain(I::ARROW_CLOCKWISE, "Rotate right"),
            B::Crop => plain(I::CROP, "Crop"),
            B::Adjust => plain(I::SLIDERS_HORIZONTAL, "Adjust colors"),
            B::Resize => plain(I::RESIZE, "Resize"),
            B::Save => {
                if dirty {
                    (I::FLOPPY_DISK, "Save changes (Ctrl+S)".into(), true)
                } else {
                    (I::FLOPPY_DISK_BACK, "Save as... (Ctrl+Shift+S)".into(), false)
                }
            }
            B::Delete => plain(I::TRASH, "Delete"),
            B::Info => (I::INFO, "Info panel (I)".into(), self.show_info),
            B::Analysis => plain(I::CHART_LINE, "Analysis: metrics and AI"),
            B::Film => (I::FILM_STRIP, "Filmstrip (F)".into(), self.show_filmstrip),
            B::Slideshow => plain(I::PLAY_CIRCLE, "Slideshow"),
            B::Fullscreen => plain(I::CORNERS_OUT, "Full screen"),
            B::Sep | B::ZoomPct | B::More => (I::DOTS_THREE, String::new(), false),
        }
    }

    fn filmstrip(&mut self, ui: &mut Ui, core: &mut Core) {
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
                            match core.thumbs.get(&key) {
                                Thumb::Ready(t) => {
                                    let ts = t.tex.size_vec2();
                                    let s = ((cell.x - 8.0) / ts.x).min((cell.y - 8.0) / ts.y);
                                    let r = Rect::from_center_size(rect.center(), ts * s);
                                    let mut mesh = egui::Mesh::with_texture(t.tex.id());
                                    mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                                    ui.painter().add(egui::Shape::mesh(mesh));
                                }
                                // Undecodable file: a placeholder instead of an empty slot.
                                Thumb::Failed => {
                                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional((cell.y * 0.4).clamp(12.0, 32.0)), p.faint);
                                }
                                Thumb::Pending => {}
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
                            ui.label(RichText::new(k.as_str()).size(11.5).color(p.muted)).on_hover_text(g);
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
        // Esc / Enter belong to the panel only if no text field had them as
        // the frame began (Esc in a text box just leaves the box, keeping the
        // text), and not while the discard prompt shows.
        let keys = keyboard_free(ui) && !self.text_was_focused && !self.confirm_discard;
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
            t.touched = true;
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
        let can_apply = t.final_rx.is_none() && t.base.is_some() && t.tool.can_apply();
        ui.horizontal(|ui| {
            if ui.button("Reset").clicked() {
                t.tool.reset();
                t.version += 1;
                t.touched = false;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(can_apply, egui::Button::new(RichText::new("Apply").color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(80.0, 28.0))).clicked() {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        // Esc with unapplied work asks first (Cancel is the explicit way out).
        if keys && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if t.touched {
                self.ask(Pending::CancelTool);
                return;
            }
            close = true;
        }
        if keys && ui.input(|i| i.key_pressed(Key::Enter)) {
            apply = true;
        }
        if apply && can_apply {
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
                    let mut resized = false;
                    if let Some(sess) = self.ensure_edit(core) {
                        resized = img.dimensions() != sess.current.dimensions();
                        sess.push(img, &title);
                    }
                    // Keep the zoom and position unless the size changed; then
                    // fit the new size once it shows.
                    if resized {
                        self.refit_on_size = Some((self.view_img, core.settings.fit_mode));
                    }
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
        // Esc / Enter belong to the panel only if no number field had them as
        // the frame began, and not while the discard prompt shows.
        let keys = keyboard_free(ui) && !self.text_was_focused && !self.confirm_discard;
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
                let sel = match (c.roi.aspect(), a) {
                    (None, None) => true,
                    (Some(x), Some(y)) => (x - y).abs() < 1e-3,
                    _ => false,
                };
                if ui.selectable_label(sel, name).clicked() {
                    // Fits the current crop to the new aspect around its center.
                    c.roi.set_aspect(a);
                }
            }
        });
        ui.add_space(8.0);
        let r = c.rect();
        let mut w = (r[2] - r[0]).round();
        let mut h = (r[3] - r[1]).round();
        let mut x = r[0].round();
        let mut y = r[1].round();
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
                if let Some(a) = c.roi.aspect() {
                    if cw {
                        h = w / a;
                    } else if ch {
                        w = h * a;
                    }
                }
                c.roi.set_rect(Some([x, y, x + w, y + h]));
            }
        });
        ui.add_space(4.0);
        let r = c.rect();
        ui.label(RichText::new(format!("{} × {} px", (r[2] - r[0]).round(), (r[3] - r[1]).round())).color(p.muted));
        ui.add_space(12.0);
        ui.label(RichText::new("Drag inside to move, drag edges or corners to resize, drag outside to draw a new area. Enter applies, Esc cancels.").size(11.5).color(p.faint));
        ui.add_space(12.0);
        // Esc after changing the area asks first, like the tools.
        let mut apply = keys && ui.input(|i| i.key_pressed(Key::Enter));
        let mut cancel = false;
        if keys && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if c.touched() {
                self.ask(Pending::CancelTool);
                return;
            }
            cancel = true;
        }
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            if ui.add(egui::Button::new(RichText::new("Crop").color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(80.0, 28.0))).clicked() {
                apply = true;
            }
        });
        if apply {
            let (x, y, w, h) = super::roi::pixel_rect(c.rect());
            self.crop = None;
            self.apply_simple(core, "Crop", |img| crate::edit::ops::crop(img, x, y, w, h));
            self.auto_fit = Some(core.settings.fit_mode);
            self.refit_on_size = Some((self.view_img, core.settings.fit_mode));
        } else if cancel {
            self.crop = None;
        }
    }

    fn crop_overlay(&mut self, ui: &mut Ui, painter: &egui::Painter, dest: Rect, img: (u32, u32), resp: &egui::Response) {
        let Some(c) = &mut self.crop else { return };
        let xf = ImageTransform::new(dest, img);
        if let Some((kx, ky)) = c.roi.set_image_size((xf.size.x, xf.size.y)) {
            c.initial = [c.initial[0] * kx, c.initial[1] * ky, c.initial[2] * kx, c.initial[3] * ky];
        }
        c.roi.show(ui, painter, &xf, resp);
    }

    /// The prompt behind `confirm_discard`: unapplied tool work first (Discard /
    /// Keep editing), then unsaved edits (Save / Discard / Cancel). Discard
    /// carries on with the pending action, which may ask again for the next level.
    fn discard_dialog(&mut self, ctx: &egui::Context, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        let p = core.pal;
        let tool_title = match (&self.tool, &self.crop) {
            (Some(t), _) if t.touched => Some(t.tool.title()),
            (_, Some(c)) if c.touched() => Some("Crop".to_string()),
            _ => None,
        };
        if tool_title.is_none() && !self.is_dirty() {
            // Nothing to lose any more (the tool was applied meanwhile).
            self.confirm_discard = false;
            self.continue_pending(core, ev);
            return;
        }
        // 0 keep / cancel, 1 discard, 2 save
        let mut choice: Option<u8> = None;
        egui::Modal::new(egui::Id::unique("discard-edits")).show(ctx, |ui| {
            ui.set_width(380.0);
            match &tool_title {
                Some(title) => {
                    ui.heading("Discard changes?");
                    ui.add_space(6.0);
                    ui.label(format!("{title}: changes not applied yet."));
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("Discard").color(Color32::WHITE)).fill(p.accent)).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                            choice = Some(1);
                        }
                        if ui.button("Keep editing").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(0);
                        }
                    });
                }
                None => {
                    ui.heading("Save changes?");
                    ui.add_space(6.0);
                    ui.label("This image has unsaved edits.");
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(0);
                        }
                        if ui.button("Discard").clicked() {
                            choice = Some(1);
                        }
                        if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(p.accent)).clicked() {
                            choice = Some(2);
                        }
                    });
                }
            }
        });
        match choice {
            Some(0) => {
                self.confirm_discard = false;
                self.pending = None;
            }
            Some(1) => {
                self.confirm_discard = false;
                if tool_title.is_some() {
                    self.close_tool();
                } else {
                    self.edit = None;
                    self.gpu = None;
                }
                self.continue_pending(core, ev);
            }
            Some(_) => {
                self.confirm_discard = false;
                self.pending = None;
                ev.push(ViewerEvent::Command(Command::Save));
            }
            None => {}
        }
    }

    /// Run the action that waited for the discard prompt.
    fn continue_pending(&mut self, core: &mut Core, ev: &mut Vec<ViewerEvent>) {
        match self.pending.take() {
            Some(Pending::Step(d)) => {
                self.step(d, core.settings.loop_navigation, core.settings.fit_mode);
            }
            Some(Pending::Close) => ev.push(ViewerEvent::Close),
            Some(Pending::OpenTool(t)) => self.open_tool(t, core),
            Some(Pending::StartCrop) => self.start_crop(core),
            Some(Pending::CancelTool) | None => {}
        }
    }
}

/// True when no text field has keyboard focus and no popup, menu or modal
/// dialog is open, so Esc / Enter belong to the viewer's tool panels.
fn keyboard_free(ui: &Ui) -> bool {
    !ui.ctx().egui_wants_keyboard_input() && !egui::Popup::is_any_open(ui.ctx()) && ui.ctx().memory(|m| m.top_modal_layer().is_none())
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

/// `v` limited to `lo..=hi` that never panics: an inverted range collapses
/// to `lo` and NaN bounds are ignored (`f32::clamp` panics on both).
fn safe_clamp(v: f32, lo: f32, hi: f32) -> f32 {
    v.max(lo).min(hi.max(lo))
}

/// Neighbours bigger than this (decoded bytes) are not kept preloaded: half
/// the cache budget, and never more than a 60 MP image.
fn preload_limit(budget: u64) -> u64 {
    (budget / 2).min(60_000_000 * 4)
}

/// Items of the viewer's bottom toolbar.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BarItem {
    Sep,
    Browser,
    Prev,
    Next,
    ZoomOut,
    ZoomPct,
    ZoomIn,
    Fit,
    Lock,
    RotL,
    RotR,
    Crop,
    Adjust,
    Resize,
    Save,
    Delete,
    Info,
    Analysis,
    Film,
    Slideshow,
    Fullscreen,
    More,
}

impl BarItem {
    /// Width in points including the 1 pt item spacing.
    fn width(self) -> f32 {
        match self {
            BarItem::Sep => 10.0,
            BarItem::ZoomPct => 55.0,
            _ => 31.0,
        }
    }
}

/// Toolbar items that fit in `avail` points, plus the ones moved to the
/// "..." menu (in toolbar order).
fn bar_layout(avail: f32) -> (Vec<BarItem>, Vec<BarItem>) {
    use BarItem as B;
    let full = [
        B::Browser, B::Sep, B::Prev, B::Next, B::Sep, B::ZoomOut, B::ZoomPct, B::ZoomIn, B::Fit, B::Lock, B::Sep, B::RotL, B::RotR, B::Crop, B::Adjust, B::Resize, B::Sep,
        B::Save, B::Delete, B::Sep, B::Info, B::Analysis, B::Film, B::Slideshow, B::Fullscreen,
    ];
    let width = |v: &[BarItem]| v.iter().map(|i| i.width()).sum::<f32>();
    if width(&full) <= avail {
        return (full.to_vec(), Vec::new());
    }
    let mut items: Vec<BarItem> = full.iter().copied().filter(|i| *i != B::Sep).collect();
    if width(&items) <= avail {
        return (items, Vec::new());
    }
    // Least important first; Prev / Next / zoom / fit always stay.
    let drop_order = [B::Fullscreen, B::Slideshow, B::Lock, B::Film, B::Resize, B::Adjust, B::Delete, B::RotR, B::Info, B::RotL, B::Browser, B::ZoomOut, B::ZoomIn, B::Analysis, B::Crop, B::Save];
    let mut hidden = Vec::new();
    for d in drop_order {
        if width(&items) + B::More.width() <= avail {
            break;
        }
        items.retain(|i| *i != d);
        hidden.push(d);
    }
    items.push(B::More);
    hidden.sort_by_key(|h| full.iter().position(|f| f == h));
    (items, hidden)
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

    #[test]
    fn large_animation_state_is_not_rebuilt_every_frame() {
        // Above ANIM_CACHE_PIXELS frame_tex stays empty: staleness must come
        // from the stored frame count, not frame_tex.len().
        let p = PathBuf::from("Z:/a.gif");
        let a = Anim { path: p.clone(), frame: 3, next_at: Instant::now(), playing: true, frame_tex: Vec::new(), tex: None, shown_frame: 3, frame_count: 12 };
        assert!(!a.is_stale(&p, 12));
        assert!(a.is_stale(&p, 11));
        assert!(a.is_stale(Path::new("Z:/b.gif"), 12));
    }

    #[test]
    fn bottom_bar_always_fits() {
        let (all, hidden) = bar_layout(1000.0);
        assert!(hidden.is_empty() && all.contains(&BarItem::Sep));
        for avail in [600.0, 400.0, 300.0, 250.0] {
            let (items, hidden) = bar_layout(avail);
            let w: f32 = items.iter().map(|i| i.width()).sum();
            assert!(w <= avail, "{avail}: {w}");
            assert!(items.contains(&BarItem::Prev) && items.contains(&BarItem::Next) && items.contains(&BarItem::Fit));
            assert_eq!(items.contains(&BarItem::More), !hidden.is_empty());
        }
    }

    #[test]
    fn clamps_never_panic() {
        assert_eq!(safe_clamp(5.0, 0.0, -3.0), 0.0);
        assert_eq!(safe_clamp(5.0, 0.0, 2.0), 2.0);
        assert_eq!(safe_clamp(-1.0, 0.0, 2.0), 0.0);
        assert!(safe_clamp(1.0, f32::NAN, f32::NAN).is_finite());
    }

    #[test]
    fn preload_set_respects_memory_budget() {
        let mut v = Viewer::new();
        v.list = ["a", "b", "c", "d"].iter().map(|n| PathBuf::from(format!("Z:/lumen-test/{n}.png"))).collect();
        v.index = 1;
        let mut l = crate::loader::Loader::new(egui::Context::default(), 1);
        // Current size unknown yet: only the next image is preloaded.
        assert_eq!(v.preload_set(&l, true), vec![v.list[1].clone(), v.list[2].clone()]);
        let rgba = RgbaImage::new(10, 10); // 400 bytes
        l.insert(&v.list[1], crate::loader::LoadedImage { decoded: crate::decode::Decoded::from_rgba(rgba.clone(), "t"), pyramid: vec![Arc::new(rgba)], load_ms: 0, file_size: 0, modified: None });
        l.budget_bytes = 10_000;
        assert_eq!(v.preload_set(&l, true).len(), 4);
        // Room for the current image plus one neighbour of the same size.
        l.budget_bytes = 1000;
        assert_eq!(v.preload_set(&l, true), vec![v.list[1].clone(), v.list[2].clone()]);
        l.budget_bytes = 500;
        assert_eq!(v.preload_set(&l, true), vec![v.list[1].clone()]);
        assert_eq!(v.preload_set(&l, false), vec![v.list[1].clone()]);
        // protected_paths falls back to the current image before any request.
        assert_eq!(v.protected_paths(), vec![v.list[1].clone()]);
    }

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
