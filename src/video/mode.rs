//! Video mode UI: playback, frame-accurate stepping, A/B comparison with
//! metrics, scopes, frame index timeline and media info.

use super::audio::AudioOut;
use super::engine::{self, Cmd, Event, IndexEntry, MediaInfo, Player, VFrame};
use super::metrics::{self, FrameMetrics, YuvFrame};
use super::scopes::{self, Scope};
use crate::app::Core;
use crate::ui::commands::Command;
use crate::ui::icons as I;
use crate::ui::theme::{self, size, Palette};
use crate::ui::widgets;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompareMode {
    SideBySide,
    Wipe,
    Flicker,
    Difference,
    Overlay,
    Checker,
}

impl CompareMode {
    const ALL: [CompareMode; 6] = [CompareMode::SideBySide, CompareMode::Wipe, CompareMode::Flicker, CompareMode::Difference, CompareMode::Overlay, CompareMode::Checker];
    fn label(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "Side by side",
            CompareMode::Wipe => "Wipe",
            CompareMode::Flicker => "Flicker A/B",
            CompareMode::Difference => "Difference",
            CompareMode::Overlay => "Overlay 50%",
            CompareMode::Checker => "Checkerboard",
        }
    }
    /// The name in the top bar's segmented control.
    fn short_label(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "Side by side",
            CompareMode::Wipe => "Wipe",
            CompareMode::Flicker => "Flicker",
            CompareMode::Difference => "Difference",
            CompareMode::Overlay => "Overlay",
            CompareMode::Checker => "Checker",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            CompareMode::SideBySide => I::COLUMNS,
            CompareMode::Wipe => I::SPLIT_HORIZONTAL,
            CompareMode::Flicker => I::SWAP,
            CompareMode::Difference => I::MINUS_CIRCLE,
            CompareMode::Overlay => I::STACK_SIMPLE,
            CompareMode::Checker => I::CHECKERBOARD,
        }
    }
    fn key(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "S",
            CompareMode::Wipe => "W",
            CompareMode::Flicker => "Tab",
            CompareMode::Difference => "D",
            CompareMode::Overlay => "O",
            CompareMode::Checker => "C",
        }
    }
}

struct Source {
    path: PathBuf,
    info: MediaInfo,
    player: Player,
    gen: u64,
    current: Option<VFrame>,
    queue: VecDeque<VFrame>,
    history: VecDeque<VFrame>,
    tex: Option<egui::TextureHandle>,
    tex_key: (u64, u64),
    index: engine::SharedIndex,
    index_progress: Arc<AtomicU32>,
    index_cancel: Arc<AtomicBool>,
    eof: bool,
    out_size: (u32, u32),
    error: Option<String>,
    /// A seek/step result is expected; when paused, only that frame becomes
    /// current (frames decoded ahead stay queued for stepping).
    awaiting: bool,
    /// Mirrors the mode's playing state (set every frame by `poll`).
    playing_hint: bool,
    /// When the pending seek was sent (drives the "Seeking" badge).
    seek_started: Option<Instant>,
}

impl Source {
    fn open(path: &Path, audio: Option<Arc<super::audio::AudioShared>>, ctx: &egui::Context) -> anyhow::Result<Source> {
        let info = engine::probe(path)?;
        let index: engine::SharedIndex = Arc::new(Mutex::new(None));
        let player = Player::open(path, audio, index.clone(), ctx.clone())?;
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let (index, progress, cancel, path, ctx) = (index.clone(), progress.clone(), cancel.clone(), path.to_path_buf(), ctx.clone());
            std::thread::spawn(move || {
                let p2 = progress.clone();
                let res = engine::build_index(&path, &cancel, &|f| p2.store(f.to_bits(), Ordering::Relaxed));
                if let Ok(v) = res {
                    *index.lock() = Some(v);
                }
                progress.store(1.0f32.to_bits(), Ordering::Relaxed);
                ctx.request_repaint();
            });
        }
        let out = (info.width.max(2), info.height.max(2));
        Ok(Source {
            path: path.to_path_buf(),
            info,
            player,
            gen: 0,
            current: None,
            queue: VecDeque::new(),
            history: VecDeque::new(),
            tex: None,
            tex_key: (u64::MAX, 0),
            index,
            index_progress: progress,
            index_cancel: cancel,
            eof: false,
            out_size: out,
            error: None,
            awaiting: true,
            playing_hint: false,
            seek_started: None,
        })
    }

    fn fps(&self) -> f64 {
        if self.info.avg_fps > 0.0 {
            self.info.avg_fps
        } else if self.info.fps > 0.0 {
            self.info.fps
        } else {
            25.0
        }
    }

    fn frame_dur(&self) -> f64 {
        1.0 / self.fps()
    }

    fn pts(&self) -> f64 {
        self.current.as_ref().map(|f| f.pts).unwrap_or(0.0)
    }

    fn duration(&self) -> f64 {
        if let Some(idx) = self.index.lock().as_ref() {
            if let Some(last) = idx.last() {
                return last.pts + self.frame_dur();
            }
        }
        self.info.duration
    }

    fn frame_count(&self) -> Option<usize> {
        self.index.lock().as_ref().map(|v| v.len())
    }

    /// Frame number (0-based) for a timestamp.
    fn frame_of(&self, pts: f64) -> usize {
        if let Some(idx) = self.index.lock().as_ref() {
            if idx.is_empty() {
                return 0;
            }
            let i = idx.partition_point(|e| e.pts <= pts + 1e-4);
            return i.saturating_sub(1);
        }
        (pts * self.fps()).round().max(0.0) as usize
    }

    fn pts_of(&self, frame: usize) -> f64 {
        if let Some(idx) = self.index.lock().as_ref() {
            if let Some(e) = idx.get(frame.min(idx.len().saturating_sub(1))) {
                return e.pts;
            }
        }
        frame as f64 / self.fps()
    }

    fn entry(&self, frame: usize) -> Option<IndexEntry> {
        self.index.lock().as_ref().and_then(|v| v.get(frame).copied())
    }

    fn seek(&mut self, t: f64, exact: bool) {
        self.gen += 1;
        self.queue.clear();
        self.eof = false;
        self.awaiting = true;
        self.seek_started = Some(Instant::now());
        self.player.send(Cmd::Seek { t, exact, gen: self.gen });
    }

    fn set_current(&mut self, f: VFrame) {
        // Learn frame types from decoded frames.
        if f.kind != b'?' {
            if let Some(idx) = self.index.lock().as_mut() {
                let i = idx.partition_point(|e| e.pts < f.pts - 1e-4);
                if let Some(e) = idx.get_mut(i) {
                    if (e.pts - f.pts).abs() < 1e-3 {
                        e.kind = f.kind;
                    }
                }
            }
        }
        // During playback the back-step history is not needed and would hold
        // hundreds of MB of frames; it is filled when paused or stepping.
        if !self.playing_hint {
            self.remember(f.clone());
        }
        self.current = Some(f);
    }

    fn remember(&mut self, f: VFrame) {
        let pos = self.history.partition_point(|h| h.pts < f.pts - 1e-6);
        if self.history.get(pos).map(|h| (h.pts - f.pts).abs() < 1e-6).unwrap_or(false) {
            self.history[pos] = f;
        } else {
            self.history.insert(pos, f);
        }
        // Bound memory (160 MB of display frames; the decoder keeps its own
        // cache of decoded frames for stepping further back).
        let bytes_per = self.history.front().map(|h| h.rgba.as_raw().len()).unwrap_or(1).max(1);
        let cap = ((160usize << 20) / bytes_per).clamp(8, 240);
        while self.history.len() > cap {
            // Drop the frame farthest from the current position.
            let cur = self.pts();
            let first = (self.history.front().unwrap().pts - cur).abs();
            let last = (self.history.back().unwrap().pts - cur).abs();
            if first > last {
                self.history.pop_front();
            } else {
                self.history.pop_back();
            }
        }
    }

    fn from_history(&self, pts: f64) -> Option<VFrame> {
        let pos = self.history.partition_point(|h| h.pts < pts - 1e-4);
        self.history.get(pos).filter(|h| (h.pts - pts).abs() < 1e-3).cloned()
    }

    fn poll(&mut self, playing: bool) {
        // Bound the decode-ahead: once enough frames are buffered we stop
        // draining, the worker's channel fills and decoding pauses itself.
        const MAX_QUEUE: usize = 8;
        self.playing_hint = playing;
        if playing && !self.history.is_empty() && self.history.len() > 16 {
            self.history.clear();
        }
        loop {
            if self.queue.len() >= MAX_QUEUE && !self.awaiting {
                break;
            }
            let Ok(ev) = self.player.rx.try_recv() else { break };
            match ev {
                Event::Frame(f) => {
                    if f.gen != self.gen {
                        continue;
                    }
                    let awaited = self.awaiting;
                    self.awaiting = false;
                    self.seek_started = None;
                    if playing || !awaited {
                        if playing && self.current.is_none() {
                            self.set_current(f);
                        } else {
                            self.queue.push_back(f);
                        }
                    } else {
                        self.set_current(f);
                    }
                }
                Event::Eof { gen } => {
                    if gen == self.gen {
                        self.eof = true;
                        self.awaiting = false;
                        self.seek_started = None;
                    }
                }
                Event::Error(e) => self.error = Some(e),
            }
        }
    }

    fn upload(&mut self, ctx: &egui::Context, nearest: bool) {
        let Some(f) = &self.current else { return };
        let key = (f.pts.to_bits(), Arc::as_ptr(&f.rgba) as u64);
        if key == self.tex_key && self.tex.is_some() {
            return;
        }
        self.tex_key = key;
        let ci = rgba_to_color_image(&f.rgba);
        let opts = if nearest { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
        match &mut self.tex {
            Some(t) => t.set(ci, opts),
            None => self.tex = Some(ctx.load_texture(format!("video:{}", self.path.display()), ci, opts)),
        }
    }

    fn display_size(&self) -> (u32, u32) {
        let (w, h) = (self.info.width.max(1), self.info.height.max(1));
        if self.info.rotation.rem_euclid(180) == 90 {
            (h, w)
        } else {
            (w, h)
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.index_cancel.store(true, Ordering::Relaxed);
    }
}

fn rgba_to_color_image(img: &RgbaImage) -> egui::ColorImage {
    let (w, h) = img.dimensions();
    // Video frames are opaque, so straight RGBA bytes are already premultiplied.
    let pixels: Vec<Color32> = bytemuck::cast_slice::<u8, [u8; 4]>(img.as_raw()).iter().map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], 255)).collect();
    egui::ColorImage::new([w as usize, h as usize], pixels)
}

struct Clock {
    base_pts: f64,
    started: Instant,
    running: bool,
    speed: f64,
}

impl Clock {
    fn now(&self) -> f64 {
        if self.running {
            self.base_pts + self.started.elapsed().as_secs_f64() * self.speed
        } else {
            self.base_pts
        }
    }
    fn start(&mut self, at: f64) {
        self.base_pts = at;
        self.started = Instant::now();
        self.running = true;
    }
    fn stop(&mut self) {
        self.base_pts = self.now();
        self.running = false;
    }
}

struct Analysis {
    rows: Arc<Mutex<Vec<(usize, FrameMetrics)>>>,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    bit_max: f64,
}

/// Radius of the A / B badges on the picture (20 pt circles).
const BADGE_R: f32 = 10.0;

/// A and B colors, the same as Compare's badges (so image and video compare match).
fn ab_color(p: &Palette, i: usize) -> Color32 {
    if i == 0 { p.accent } else { Color32::from_rgb(232, 120, 60) }
}

/// A round A / B badge: the letter in white on its color, with a soft dark
/// ring so it stays visible on bright and dark pictures alike.
fn paint_badge(painter: &egui::Painter, center: Pos2, label: &str, color: Color32, r: f32) {
    painter.circle_filled(center, r + 1.0, Color32::from_black_alpha(70));
    painter.circle_filled(center, r, color);
    painter.text(center, Align2::CENTER_CENTER, label, theme::semibold(r + 1.5), Color32::WHITE);
}

/// Horizontal padding of the picture's readout pills.
const OSD_PAD: f32 = 12.0;

/// A readout pill over the picture (overlay fill, 1 pt border, radius 8),
/// anchored at `pos` by `align`.
fn osd_pill(painter: &egui::Painter, p: &Palette, pos: Pos2, align: Align2, g: Arc<egui::Galley>) {
    let size = Vec2::new(g.size().x + 2.0 * OSD_PAD, (g.size().y + 10.0).max(size::COMPACT + 4.0));
    let r = align.anchor_size(pos, size);
    painter.add(theme::popover_shadow(p).as_shape(r, CornerRadius::same(theme::radius::CARD)));
    painter.rect(r, CornerRadius::same(theme::radius::CARD), p.overlay, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.galley(Pos2::new(r.min.x + OSD_PAD, r.center().y - g.size().y / 2.0), g, p.text);
}

/// `name` cut to `max` characters, keeping its start and its extension.
fn shorten(name: &str, max: usize) -> String {
    let n = name.chars().count();
    if n <= max || max < 8 {
        return name.to_string();
    }
    let tail = 6.min(max / 3);
    let head: String = name.chars().take(max - tail - 3).collect();
    let end: String = name.chars().skip(n - tail).collect();
    format!("{head}...{end}")
}

/// Playback speeds offered in the transport bar.
const SPEEDS: [f64; 7] = [0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 4.0];

/// Names of the top bar's Save frame, Scopes and Info buttons (shown when
/// there is room).
const RIGHT_LABELS: [&str; 3] = ["Save frame", "Scopes", "Info"];

/// A monospace readout of the transport bar, centered on its digits: a
/// label centers its line box, which leaves digits above the icons'
/// middle. With a click sense it highlights on hover like a flat button.
fn readout(ui: &mut Ui, text: String, color: Color32, sense: Sense) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text, FontId::monospace(12.5), color);
    let pad = if sense.senses_click() { 4.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 2.0 * pad, 26.0), sense);
    if pad > 0.0 && resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let y = rect.center().y - g.mesh_bounds.center().y;
    ui.painter().galley(Pos2::new(rect.min.x + pad, y), g, color);
    resp
}

/// Why a video did not open, in a few words for the error card: the
/// innermost cause (FFmpeg's or the system's message), in sentence case.
fn open_reason(e: &anyhow::Error) -> String {
    let root = e.root_cause().to_string();
    let low = root.to_lowercase();
    let text = if low.contains("no such file") || low.contains("cannot find") {
        "The file was not found.".to_string()
    } else if low.contains("permission denied") || low.contains("access is denied") {
        "Access to the file was denied.".to_string()
    } else if low.contains("invalid data") {
        "The file is damaged or not a video FFmpeg can read.".to_string()
    } else if low.contains("no video stream") {
        "The file has no video stream.".to_string()
    } else {
        let t = root.trim().trim_end_matches('.');
        let mut c = t.chars();
        match c.next() {
            Some(f) => format!("{}{}.", f.to_uppercase(), c.as_str()),
            None => "The decoder could not read it.".to_string(),
        }
    };
    shorten(&text, 160)
}

/// The "Cannot open the video" card of the empty view, laid out for a width.
struct ErrorCard {
    title: Arc<egui::Galley>,
    name: Arc<egui::Galley>,
    why: Arc<egui::Galley>,
    size: Vec2,
}

impl ErrorCard {
    const PAD: f32 = 16.0;
    const ICON: f32 = 28.0;

    fn layout(painter: &egui::Painter, p: &Palette, name: &str, why: &str, width: f32) -> ErrorCard {
        let text_w = (width - 2.0 * Self::PAD - Self::ICON).max(40.0);
        let wrapped = |text: &str, font: FontId, color: Color32, rows: usize| {
            let mut job = egui::text::LayoutJob::simple(text.to_string(), font, color, text_w);
            job.wrap.max_rows = rows;
            job.wrap.break_anywhere = rows == 1;
            job.wrap.overflow_character = None;
            painter.layout_job(job)
        };
        let title = wrapped("Cannot open the video", crate::ui::theme::semibold(14.0), p.text, 1);
        // A long name keeps its start and extension.
        let max_chars = (text_w / 6.5) as usize;
        let name = wrapped(&shorten(name, max_chars.max(12)), FontId::proportional(12.5), p.text, 1);
        let why = wrapped(why, FontId::proportional(12.0), p.muted, 3);
        let h = Self::PAD * 2.0 + title.size().y + 6.0 + name.size().y + 4.0 + why.size().y;
        ErrorCard { title, name, why, size: Vec2::new(width, h) }
    }

    fn paint(self, painter: &egui::Painter, p: &Palette, min: Pos2) {
        let r = Rect::from_min_size(min, self.size);
        painter.rect(r, CornerRadius::same(10), p.panel, Stroke::new(1.0, p.danger.gamma_multiply(0.55)), egui::StrokeKind::Inside);
        let icon = Pos2::new(r.min.x + Self::PAD + 9.0, r.min.y + Self::PAD + self.title.size().y / 2.0);
        painter.text(icon, Align2::CENTER_CENTER, I::WARNING_CIRCLE, FontId::proportional(18.0), p.danger);
        let x = r.min.x + Self::PAD + Self::ICON;
        let mut y = r.min.y + Self::PAD;
        let (th, nh) = (self.title.size().y, self.name.size().y);
        painter.galley(Pos2::new(x, y), self.title, p.text);
        y += th + 6.0;
        painter.galley(Pos2::new(x, y), self.name, p.text);
        y += nh + 4.0;
        painter.galley(Pos2::new(x, y), self.why, p.muted);
    }
}

/// Parts of the transport bar shown at its width (see `VideoMode::transport_fit`).
#[derive(Clone, Copy, Debug)]
struct TransportFit {
    legend: bool,
    speed_label: bool,
    skip_secs: bool,
    loop_label: bool,
    volume: bool,
    first_last: bool,
    frame_word: bool,
    duration: bool,
    /// The time readout (the frame counter stays).
    time: bool,
    /// The A-B loop button (Ctrl+L still works).
    loop_button: bool,
}

impl Default for TransportFit {
    fn default() -> Self {
        TransportFit { legend: true, speed_label: true, skip_secs: true, loop_label: true, volume: true, first_last: true, frame_word: true, duration: true, time: true, loop_button: true }
    }
}

pub struct VideoMode {
    sources: Vec<Source>,
    audio: Option<AudioOut>,
    audio_error: Option<String>,
    playing: bool,
    clock: Clock,
    speed: f64,
    zoom: f32,
    center: Vec2,
    auto_fit: bool,
    mode: CompareMode,
    offset_frames: i64,
    gain: f32,
    wipe_pos: f32,
    wipe_frozen: bool,
    flicker_b: bool,
    flicker_auto: bool,
    flicker_at: Instant,
    show_info: bool,
    show_scopes: bool,
    scope: Scope,
    scope_tex: Option<(u64, egui::TextureHandle)>,
    diff_tex: Option<((u64, u64, u32), egui::TextureHandle)>,
    diff_stats: Option<(f64, f64)>,
    metrics_now: Option<((u64, u64), FrameMetrics)>,
    metrics_rx: Option<crossbeam_channel::Receiver<((u64, u64), Option<FrameMetrics>)>>,
    analysis: Option<Analysis>,
    loop_ab: (Option<f64>, Option<f64>),
    goto: Option<String>,
    pixel: Option<(u32, u32)>,
    volume: f32,
    muted: bool,
    scrubbing: bool,
    hover_view: bool,
    last_out_req: Instant,
    /// The last video that failed to open: (file name, short reason).
    open_error: Option<(String, String)>,
    /// "Close video" in the picture's menu: leave video mode this frame.
    close_requested: bool,
    pending_swap: Option<(PathBuf, PathBuf, f64)>,
    snap_rx: Option<crossbeam_channel::Receiver<Result<String, String>>>,
    /// (index entries, columns, per-column bars from `timeline_columns`, built at)
    timeline_cache: Option<(usize, usize, Vec<TimelineColumn>, Instant)>,
    /// Frame number most recently requested by stepping (until it arrives).
    pending_step: Option<usize>,
    /// A text field had keyboard focus at the end of the last frame. egui drops
    /// focus on Esc before this frame's widgets run, so Esc that only leaves a
    /// field must not also leave video mode.
    was_editing: bool,
    /// Widget id of the B offset field (last frame), so + and - keep
    /// shifting B while that field has keyboard focus.
    offset_field: Option<egui::Id>,
    /// The B offset field had keyboard focus at the end of the last frame.
    offset_editing: bool,
    /// Frames left in which the B offset field's change is ignored (Esc).
    offset_cancel: u8,
    /// Dev hook only: start time and press count of the current step burst.
    dev_burst: Option<(Instant, u32)>,
    /// A alone decodes native planes for the Analysis panel's Metrics tab.
    metrics_native: bool,
}

impl VideoMode {
    pub fn new(paths: Vec<PathBuf>, core: &Core) -> Self {
        let (audio, audio_error) = match AudioOut::open() {
            Ok(a) => (Some(a), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut vm = VideoMode {
            sources: Vec::new(),
            audio,
            audio_error,
            playing: false,
            clock: Clock { base_pts: 0.0, started: Instant::now(), running: false, speed: 1.0 },
            speed: 1.0,
            zoom: 1.0,
            center: Vec2::ZERO,
            auto_fit: true,
            mode: CompareMode::Wipe,
            offset_frames: 0,
            gain: 4.0,
            wipe_pos: 0.5,
            wipe_frozen: false,
            flicker_b: false,
            flicker_auto: false,
            flicker_at: Instant::now(),
            show_info: false,
            show_scopes: false,
            scope: Scope::Waveform,
            scope_tex: None,
            diff_tex: None,
            diff_stats: None,
            metrics_now: None,
            metrics_rx: None,
            analysis: None,
            loop_ab: (None, None),
            goto: None,
            pixel: None,
            volume: 1.0,
            muted: false,
            scrubbing: false,
            hover_view: false,
            last_out_req: Instant::now(),
            open_error: None,
            close_requested: false,
            pending_swap: None,
            snap_rx: None,
            timeline_cache: None,
            pending_step: None,
            was_editing: false,
            offset_field: None,
            offset_editing: false,
            offset_cancel: 0,
            dev_burst: None,
            metrics_native: false,
        };
        for p in paths.into_iter().take(2) {
            vm.add_source(&p, &core.ctx);
        }
        vm
    }

    /// Open `path` as A (or as B when A is open). A failure is kept in
    /// `open_error` for the empty view's card (or a toast, see `ui`).
    fn add_source(&mut self, path: &Path, ctx: &egui::Context) {
        if self.sources.len() >= 2 {
            self.sources.truncate(1);
        }
        let audio = if self.sources.is_empty() { self.audio.as_ref().map(|a| a.shared.clone()) } else { None };
        match Source::open(path, audio, ctx) {
            Ok(mut s) => {
                self.open_error = None;
                if self.sources.len() == 1 {
                    // B renders at A's output size so pixels line up.
                    s.player.send(Cmd::Native(true));
                    self.sources[0].player.send(Cmd::Native(true));
                    let t = self.sources[0].pts() + self.offset_secs_for(&s);
                    s.seek(t, true);
                }
                self.sources.push(s);
                self.auto_fit = true;
            }
            Err(e) => {
                let name = crate::ui::viewer::file_name(path);
                self.open_error = Some((if name.is_empty() { path.display().to_string() } else { name }, open_reason(&e)));
            }
        }
    }

    fn offset_secs_for(&self, b: &Source) -> f64 {
        self.offset_frames as f64 * b.frame_dur()
    }

    fn offset_secs(&self) -> f64 {
        self.sources.get(1).map(|b| self.offset_secs_for(b)).unwrap_or(0.0)
    }

    fn audio_shared(&self) -> Option<Arc<super::audio::AudioShared>> {
        self.audio.as_ref().map(|a| a.shared.clone())
    }

    fn play(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        if self.sources[0].eof && self.sources[0].queue.is_empty() {
            // Restart from the beginning when at the end.
            self.seek_all(0.0, true);
        }
        self.playing = true;
        let at = self.sources[0].pts();
        self.clock.speed = self.speed;
        self.clock.start(at);
        for s in &mut self.sources {
            s.player.send(Cmd::Play);
        }
        if let Some(a) = self.audio_shared() {
            a.playing.store((self.speed - 1.0).abs() < 1e-6, Ordering::Relaxed);
        }
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.clock.stop();
        for s in &mut self.sources {
            s.player.send(Cmd::Pause);
        }
        if let Some(a) = self.audio_shared() {
            a.playing.store(false, Ordering::Relaxed);
        }
        // Frames already decoded ahead stay queued for instant stepping.
    }

    fn toggle_play(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Run a command from the command layer (palette, menus, keymap, dev
    /// hook) in video mode. Returns false for commands video mode does not
    /// handle, which the app then runs as usual.
    pub fn run_command(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::PlayPause => self.toggle_play(),
            Command::NextFrame => self.step(1),
            Command::PrevFrame => self.step(-1),
            Command::ToggleInfo => self.show_info = !self.show_info,
            Command::ZoomFit => self.auto_fit = true,
            Command::ZoomActual => {
                self.auto_fit = false;
                self.zoom = 1.0;
            }
            _ => return false,
        }
        true
    }

    fn seek_all(&mut self, t: f64, exact: bool) {
        self.pending_step = None;
        let was = self.playing;
        if was {
            self.pause();
        }
        let off = self.offset_secs();
        let dur = self.sources.first().map(|s| s.duration()).unwrap_or(0.0);
        let t = t.clamp(0.0, (dur - 0.001).max(0.0));
        for (i, s) in self.sources.iter_mut().enumerate() {
            let target = if i == 0 { t } else { t + off };
            s.seek(target.max(0.0), exact);
        }
        self.clock.base_pts = t;
        if was && exact {
            // Resume after the seek lands.
            self.playing = true;
            self.clock.start(t);
            for s in &mut self.sources {
                s.player.send(Cmd::Play);
            }
            if let Some(a) = self.audio_shared() {
                a.playing.store(true, Ordering::Relaxed);
            }
        }
    }

    fn step(&mut self, delta: i64) {
        if self.sources.is_empty() {
            return;
        }
        if self.playing {
            self.pause();
        }
        if dev_dir().is_some() {
            if self.dev_burst.is_none() {
                dev_log(&format!("at={:.1} press", dev_clock()));
            }
            let b = self.dev_burst.get_or_insert((Instant::now(), 0));
            b.1 += 1;
        }
        let cur = self.sources[0].pts();
        // Rapid key presses: step from the frame already requested, not from
        // the frame still on screen, so every press counts.
        let cur_frame = self.pending_step.filter(|_| self.sources[0].awaiting).unwrap_or(self.sources[0].frame_of(cur)) as i64;
        let target_frame = (cur_frame + delta).max(0) as usize;
        self.pending_step = Some(target_frame);
        let target = self.sources[0].pts_of(target_frame);
        let off = self.offset_secs();
        for (i, s) in self.sources.iter_mut().enumerate() {
            let t = if i == 0 { target } else { target + off };
            if delta == 1 {
                if let Some(next) = s.queue.pop_front() {
                    if (next.pts - t).abs() < 1e-3 || i == 0 {
                        s.set_current(next);
                        continue;
                    }
                    s.queue.push_front(next);
                }
            }
            if let Some(f) = s.from_history(t) {
                s.queue.clear();
                s.set_current(f);
                // Re-anchor the decoder so the next forward step continues from here.
                s.gen += 1;
                s.awaiting = true;
                s.player.send(Cmd::Seek { t, exact: true, gen: s.gen });
                continue;
            }
            s.seek(t, true);
        }
    }

    /// Save (or copy) the displayed frame of A at native resolution. The decode
    /// runs on a background thread; the result is reported by `poll_snapshot`.
    fn snapshot(&mut self, copy: bool, _core: &mut Core) {
        let Some(s) = self.sources.first() else { return };
        let pts = s.pts();
        let target = if copy {
            None
        } else {
            let frame = s.frame_of(pts);
            let stem = s.path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let mut dlg = rfd::FileDialog::new().add_filter("PNG", &["png"]).set_file_name(format!("{stem}_frame{frame:06}.png"));
            // Not the video's own folder: it is often read-only or a media library.
            if let Some(d) = last_snapshot_dir().or_else(dirs::picture_dir) {
                dlg = dlg.set_directory(d);
            }
            match dlg.save_file() {
                Some(p) => {
                    if let Some(d) = p.parent() {
                        remember_snapshot_dir(d);
                    }
                    Some(p)
                }
                None => return,
            }
        };
        let path = s.path.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let res = engine::frame_at(&path, pts).map_err(|e| e.to_string()).and_then(|img| match &target {
                Some(p) => img.save(p).map(|_| format!("Saved {}", p.display())).map_err(|e| e.to_string()),
                None => crate::fileops::copy_image_to_clipboard(&img).map(|_| "Frame copied to clipboard".to_string()).map_err(|e| e.to_string()),
            });
            let _ = tx.send(res);
        });
        self.snap_rx = Some(rx);
    }

    fn poll_snapshot(&mut self, core: &mut Core) {
        let Some(rx) = &self.snap_rx else { return };
        match rx.try_recv() {
            Ok(Ok(msg)) => {
                core.toasts.success(msg);
                self.snap_rx = None;
            }
            Ok(Err(e)) => {
                core.toasts.error(e);
                self.snap_rx = None;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => self.snap_rx = None,
            Err(_) => core.ctx.request_repaint_after(Duration::from_millis(100)),
        }
    }

    fn start_analysis(&mut self) {
        if self.sources.len() < 2 {
            return;
        }
        let a = self.sources[0].path.clone();
        let b = self.sources[1].path.clone();
        let offset = self.offset_frames;
        let rows = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let total = self.sources[0].frame_count().unwrap_or(0).max(1);
        let bit_max = ((1u64 << self.sources[0].info.bit_depth.clamp(8, 16)) - 1) as f64;
        {
            let (rows, progress, cancel, done, error) = (rows.clone(), progress.clone(), cancel.clone(), done.clone(), error.clone());
            std::thread::spawn(move || {
                let r = super::analyze::run(&a, &b, offset, &cancel, |i, m| {
                    rows.lock().push((i, m));
                    progress.store(((i + 1) as f32 / total as f32).min(1.0).to_bits(), Ordering::Relaxed);
                });
                if let Err(e) = r {
                    *error.lock() = Some(e.to_string());
                }
                done.store(true, Ordering::Relaxed);
            });
        }
        self.analysis = Some(Analysis { rows, progress, cancel, done, error, bit_max });
    }

    fn export_csv(&self, core: &mut Core) {
        let Some(an) = &self.analysis else { return };
        let rows = an.rows.lock().clone();
        let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("lumen_metrics.csv").save_file() else { return };
        match std::fs::write(&p, metrics::to_csv(&rows)) {
            Ok(()) => core.toasts.success(format!("Exported {}", p.display())),
            Err(e) => core.toasts.error(e.to_string()),
        }
    }

    // ---------------------------------------------------------------- frame

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let ctx = ui.ctx().clone();
        let p = core.pal;
        ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::from_rgb(10, 10, 12));
        // Drag & drop: first file → A, second → B.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            if self.sources.len() >= 2 || dropped.len() >= 2 {
                self.sources.clear();
            }
            for d in dropped.into_iter().take(2) {
                self.add_source(&d, &ctx);
            }
        }

        self.poll_snapshot(core);
        // Poll decoders.
        let playing = self.playing;
        for s in &mut self.sources {
            s.poll(playing);
        }
        if let Some(rx) = &self.metrics_rx {
            if let Ok((k, m)) = rx.try_recv() {
                self.metrics_rx = None;
                if let Some(m) = m {
                    self.metrics_now = Some((k, m));
                }
            }
        }
        self.advance_playback(&ctx);
        self.metrics_hooks(core);
        self.dev_report_step();
        if self.flicker_auto && self.flicker_at.elapsed() > Duration::from_millis(500) {
            self.flicker_b = !self.flicker_b;
            self.flicker_at = Instant::now();
        }
        if self.flicker_auto {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // A file that failed to open while another plays: say so (the
        // empty view shows a card instead).
        if !self.sources.is_empty() {
            if let Some((name, why)) = self.open_error.take() {
                core.toasts.error(format!("Cannot open {name}: {why}"));
            }
        }
        // Keys first, so this frame already shows their effect (B offset,
        // panels, compare mode) instead of the state before the press.
        let mut keep = self.keys(&ctx, core);
        egui::Panel::top("video-top")
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(8, 6)))
            .show(ui, |ui| keep &= self.top_bar(ui, core));
        egui::Panel::bottom("video-transport")
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 6)))
            .show(ui, |ui| self.transport(ui, core));
        if self.show_info && !self.sources.is_empty() {
            egui::Panel::right("video-info")
                .resizable(true)
                .default_size(360.0)
                .size_range(300.0..=560.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::same(12)))
                .show(ui, |ui| self.info_panel(ui, core));
        }
        if self.show_scopes && !self.sources.is_empty() {
            egui::Panel::left("video-scopes")
                .resizable(true)
                .default_size(340.0)
                .size_range(260.0..=640.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::same(10)))
                .show(ui, |ui| self.scopes_panel(ui, &p));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.viewport(ui, core));

        if self.goto.is_some() {
            self.goto_dialog(&ctx);
        }
        keep &= !std::mem::take(&mut self.close_requested);
        if !keep {
            self.pause();
        }
        self.was_editing = ctx.text_edit_focused();
        keep
    }

    fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playing || self.sources.is_empty() {
            return;
        }
        // Prefer the audio clock (keeps A/V in sync), else the system clock.
        let audio_clock = if (self.speed - 1.0).abs() < 1e-6 { self.audio_shared().and_then(|a| a.clock()) } else { None };
        let mut t = self.clock.now();
        if let Some(ac) = audio_clock {
            if (ac - t).abs() < 1.0 {
                t = ac;
                // Keep the system clock aligned so a fallback is seamless.
                self.clock.start(ac);
            }
        }
        // A-B loop
        if let (Some(a), Some(b)) = self.loop_ab {
            if t >= b {
                self.seek_all(a, true);
                return;
            }
        }
        let off = self.offset_secs();
        let mut next_due: Option<f64> = None;
        for (i, s) in self.sources.iter_mut().enumerate() {
            let target = if i == 0 { t } else { t + off };
            let half = s.frame_dur() * 0.5;
            while s.queue.front().map(|f| f.pts <= target + half).unwrap_or(false) {
                let f = s.queue.pop_front().unwrap();
                s.set_current(f);
            }
            if let Some(f) = s.queue.front() {
                let due = f.pts - half - target;
                next_due = Some(next_due.map(|d| d.min(due)).unwrap_or(due));
            }
        }
        let a = &self.sources[0];
        if a.eof && a.queue.is_empty() {
            self.pause();
            return;
        }
        let wait = next_due.unwrap_or(0.005).clamp(0.001, 0.05) / self.speed.max(0.1);
        ctx.request_repaint_after(Duration::from_secs_f64(wait));
    }

    fn top_bar(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let p = core.pal;
        let mut keep = true;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back to browser (Esc)", false).clicked() {
                keep = false;
            }
            // In a narrow window (a large UI scale, the Analysis panel open)
            // the title shrinks, the compare modes lose their labels and then
            // move into a menu, and long file names are shortened, so nothing
            // runs under the buttons on the right.
            let avail = ui.available_width();
            let narrow = avail < 900.0;
            let pair = self.sources.len() == 2;
            // Very narrow: the names become A / B badges; with two videos
            // Remove B, Scopes and Save frame join the compare menu.
            let tiny = avail < 560.0;
            if !tiny {
                let title = if narrow { I::FILM_STRIP.to_string() } else { format!("{}  Video", I::FILM_STRIP) };
                ui.label(RichText::new(title).font(theme::text::title()));
                widgets::separator_v(ui);
            }
            // Points: back, title, the four buttons on the right, the badges and name buttons' padding.
            let fixed = 34.0 + if narrow { 30.0 } else { 80.0 } + 13.0 + 4.0 * 34.0 + 12.0 + 2.0 * 48.0 + if pair { 34.0 } else { 0.0 };
            // Mode names from 1200 points of room (and while they fit), then icons,
            // then the compare menu.
            let min_names = 2.0 * 12.0 * 7.5;
            let room = avail - fixed - min_names;
            let labels = avail >= 1200.0 && self.compare_controls_width(ui, true) <= room;
            let controls = if pair { self.compare_controls_width(ui, labels) } else { 0.0 };
            let compact = pair && room < controls;
            let controls = if compact { 50.0 } else { controls };
            // Save frame, Scopes and Info show their names while the file
            // names keep room for about 24 characters each.
            let label_extra = RIGHT_LABELS.iter().map(|l| widgets::labeled_icon_button_width(ui, l) + 4.0).sum::<f32>() - 3.0 * 34.0;
            let icon_labels = !narrow && avail - fixed - controls - label_extra >= 2.0 * 24.0 * 7.5;
            let fixed = fixed + if icon_labels { label_extra } else { 0.0 };
            let name_chars = ((avail - fixed - controls) / 2.0 / 7.5).clamp(8.0, 48.0) as usize;
            for (i, label) in ["A", "B"].iter().enumerate() {
                let name = self.sources.get(i).map(|s| shorten(&crate::ui::viewer::file_name(&s.path), name_chars));
                let full = self.sources.get(i).map(|s| crate::ui::viewer::file_name(&s.path)).unwrap_or_default();
                let name = name.unwrap_or_else(|| match (i, avail < 700.0) {
                    (0, _) => "Open video...".into(),
                    (_, true) => "Add B...".into(),
                    _ => "Add B to compare...".into(),
                });
                // One chip per side: the round A / B badge (Compare's colors),
                // then the file name (muted while it only invites to open one).
                let mut job = egui::text::LayoutJob::default();
                if !tiny {
                    let format = egui::TextFormat { font_id: theme::text::body(), color: if full.is_empty() { p.muted } else { p.text }, valign: egui::Align::Center, ..Default::default() };
                    job.append(&name, BADGE_R * 2.0 + 6.0, format);
                }
                let tip = match (full.is_empty(), tiny || full != name) {
                    (true, _) => name.clone(),
                    (false, true) => format!("{full}\nChoose a file"),
                    (false, false) => "Choose a file".into(),
                };
                let resp = ui.add(egui::Button::new(job).min_size(Vec2::new(if tiny { size::CONTROL } else { 60.0 }, size::CONTROL))).on_hover_text(tip);
                let pad = ui.spacing().button_padding.x;
                let c = if tiny { resp.rect.center() } else { Pos2::new(resp.rect.left() + pad + BADGE_R, resp.rect.center().y) };
                paint_badge(ui.painter(), c, label, ab_color(&p, i), BADGE_R - 1.0);
                if resp.clicked() {
                    let exts: Vec<&str> = super::VIDEO_EXTS.to_vec();
                    if let Some(f) = rfd::FileDialog::new().add_filter("Videos", &exts).add_filter("All files", &["*"]).pick_file() {
                        if i == 0 {
                            self.sources.clear();
                        } else {
                            self.sources.truncate(1);
                        }
                        self.add_source(&f, &core.ctx);
                    }
                }
                if i == 1 && self.sources.len() == 2 && !tiny && widgets::icon_button(ui, I::X, "Remove B", false).clicked() {
                    self.remove_b();
                }
            }
            if pair {
                widgets::separator_v(ui);
                if compact || tiny {
                    let r = ui.menu_button(RichText::new(format!("{}  {}", self.mode.icon(), I::CARET_DOWN)).size(15.0), |ui| {
                        ui.set_min_width(220.0);
                        self.compare_controls(ui, &p, true, false);
                        if tiny {
                            ui.separator();
                            if widgets::menu_toggle(ui, self.show_scopes, "Scopes", "F2").clicked() {
                                self.show_scopes = !self.show_scopes;
                                ui.close();
                            }
                            if widgets::menu_item(ui, I::CAMERA, "Save frame as PNG", "Ctrl+S").clicked() {
                                ui.close();
                                self.snapshot(false, core);
                            }
                            if widgets::menu_item(ui, I::X, "Remove B", "").clicked() {
                                ui.close();
                                self.remove_b();
                            }
                        }
                    });
                    r.response.on_hover_text(format!("{}: compare mode, B offset", self.mode.label()));
                } else {
                    self.compare_controls(ui, &p, false, labels);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = |ui: &mut Ui, icon: &str, label: &str, tip: &str, on: bool| {
                    if icon_labels { widgets::labeled_icon_button(ui, icon, label, tip, on) } else { widgets::icon_button(ui, icon, tip, on) }
                };
                crate::analysis_ui::toolbar_button(ui, core);
                if button(ui, I::INFO, RIGHT_LABELS[2], "Info & metrics (I)", self.show_info).clicked() {
                    self.show_info = !self.show_info;
                }
                if !(tiny && pair) && button(ui, I::WAVEFORM, RIGHT_LABELS[1], "Scopes (F2)", self.show_scopes).clicked() {
                    self.show_scopes = !self.show_scopes;
                }
                if !(tiny && pair) && button(ui, I::CAMERA, RIGHT_LABELS[0], "Save frame as PNG (Ctrl+S)", false).clicked() {
                    self.snapshot(false, core);
                }
            });
        });
        keep
    }

    fn remove_b(&mut self) {
        if self.sources.len() == 2 {
            self.sources.truncate(1);
            self.sources[0].player.send(Cmd::Native(false));
        }
    }

    /// The compare mode segments' labels: names when there is room, else icons.
    fn mode_labels(labels: bool) -> Vec<&'static str> {
        CompareMode::ALL.iter().map(|m| if labels { m.short_label() } else { m.icon() }).collect()
    }

    /// Points the compare controls take in the top bar.
    fn compare_controls_width(&self, ui: &Ui, labels: bool) -> f32 {
        let widest = Self::mode_labels(labels).iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), theme::text::body(), Color32::WHITE).size().x).fold(0.0, f32::max);
        let modes = CompareMode::ALL.len() as f32 * (widest + 24.0) + 4.0 + 4.0;
        let gain = if self.mode == CompareMode::Difference { 50.0 + 90.0 + 4.0 + size::VALUE_W } else { 0.0 };
        // The separator, "B offset", its box, Swap and the gaps between them.
        modes + gain + 13.0 + 48.0 + size::VALUE_W + 30.0 + 16.0
    }

    /// Compare modes, the difference gain and B's offset: in the top bar (as
    /// one segmented control, with names when `labels`), or listed in its
    /// menu (`menu`) when the bar is narrow.
    fn compare_controls(&mut self, ui: &mut Ui, p: &Palette, menu: bool, labels: bool) {
        let p = *p;
        if menu {
            for m in CompareMode::ALL {
                if widgets::menu_toggle(ui, self.mode == m, m.label(), m.key()).clicked() {
                    self.mode = m;
                }
            }
            ui.separator();
        } else {
            let mut idx = CompareMode::ALL.iter().position(|m| *m == self.mode).unwrap_or(0);
            let tips: Vec<String> = CompareMode::ALL.iter().map(|m| format!("{} ({})", m.label(), m.key())).collect();
            let tips: Vec<&str> = tips.iter().map(|t| t.as_str()).collect();
            if widgets::segmented_with_tips(ui, &mut idx, &Self::mode_labels(labels), &tips) {
                self.mode = CompareMode::ALL[idx];
            }
        }
        let mut rest = |ui: &mut Ui| {
            if self.mode == CompareMode::Difference {
                ui.add_space(4.0);
                ui.label(RichText::new("Gain").color(p.muted).size(theme::text::SMALL));
                ui.add(widgets::Slider::new(&mut self.gain, 1.0..=64.0).logarithmic(true).suffix("×").fixed_decimals(1).width(90.0).default_value(4.0));
            }
            if !menu {
                widgets::separator_v(ui);
            }
            ui.label(RichText::new("B offset").color(p.muted).size(theme::text::SMALL));
            let mut off = self.offset_frames;
            // Typed values apply on Enter (or when the field loses focus);
            // Esc goes back to the offset in use. egui reports the lost
            // focus for two frames and applies the typed text on the second,
            // after the Esc press is gone, so that change is dropped here.
            if self.offset_editing && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.offset_cancel = 2;
            }
            let r = widgets::value_box(ui, egui::DragValue::new(&mut off).suffix(" f").speed(0.2).update_while_editing(false)).on_hover_text("Shift B in frames (+ / - keys; Ctrl x10)");
            self.offset_field = Some(r.id);
            self.offset_editing = ui.memory(|m| m.has_focus(r.id));
            if self.offset_cancel > 0 {
                self.offset_cancel -= 1;
            } else if r.changed() {
                self.set_offset(off);
            }
            if widgets::icon_button(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B (X)", false).clicked() {
                self.swap();
            }
        };
        if menu {
            ui.horizontal(|ui| rest(ui));
        } else {
            rest(ui);
        }
    }

    fn set_offset(&mut self, off: i64) {
        self.offset_frames = off;
        if self.sources.len() == 2 {
            let t = self.sources[0].pts() + self.offset_secs();
            self.sources[1].seek(t.max(0.0), true);
            self.metrics_now = None;
        }
    }

    fn swap(&mut self) {
        if self.sources.len() == 2 {
            let t = self.sources[0].pts();
            let b = self.sources.pop().unwrap();
            let a = self.sources.pop().unwrap();
            let pa = a.path.clone();
            let pb = b.path.clone();
            drop(a);
            drop(b);
            self.offset_frames = -self.offset_frames;
            self.pending_swap = Some((pb, pa, t));
        }
    }

    // ---------------------------------------------------------------- viewport

    fn viewport(&mut self, ui: &mut Ui, core: &mut Core) {
        if let Some((a, b, t)) = self.pending_swap.take() {
            self.add_source(&a, &core.ctx);
            self.add_source(&b, &core.ctx);
            self.seek_all(t, true);
        }
        let p = core.pal;
        let view = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(view, Sense::click_and_drag());
        self.hover_view = resp.hovered();
        if self.sources.is_empty() {
            // The hero (icon and hints), the error card when a file failed to
            // open, then the button: one column centered in the view.
            let painter = ui.painter().with_clip_rect(view);
            let wrap = (view.width() - 32.0).max(40.0);
            let line = |text: &str, size: f32, color: Color32| {
                let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(size), color, wrap);
                job.halign = egui::Align::Center;
                painter.layout_job(job)
            };
            let hint = line("Drop one video to play it, or two videos to compare them", 15.0, p.text);
            let sub = line("Frame-accurate stepping · wipe / flicker / difference · PSNR & SSIM · scopes", 12.5, p.muted);
            let card = self.open_error.as_ref().map(|(name, why)| ErrorCard::layout(&painter, &p, name, why, wrap.min(440.0)));
            let icon_h = 64.0;
            let card_h = card.as_ref().map(|c| c.size.y + 20.0).unwrap_or(0.0);
            let total = icon_h + 14.0 + hint.size().y + 8.0 + sub.size().y + 20.0 + card_h + 34.0;
            let mut y = (view.center().y - total / 2.0).max(view.min.y + 16.0);
            let cx = view.center().x;
            painter.text(Pos2::new(cx, y + icon_h / 2.0), Align2::CENTER_CENTER, I::FILM_STRIP, FontId::proportional(56.0), p.faint);
            y += icon_h + 14.0;
            let (hint_h, sub_h) = (hint.size().y, sub.size().y);
            painter.galley(Pos2::new(cx, y), hint, p.text);
            y += hint_h + 8.0;
            painter.galley(Pos2::new(cx, y), sub, p.muted);
            y += sub_h + 20.0;
            if let Some(card) = card {
                let size = card.size;
                card.paint(&painter, &p, Pos2::new(cx - size.x / 2.0, y));
                y += size.y + 20.0;
            }
            let br = Rect::from_min_size(Pos2::new(cx - 100.0, y), Vec2::new(200.0, 34.0));
            let open = ui.scope_builder(egui::UiBuilder::new().max_rect(br).layout(egui::Layout::top_down_justified(egui::Align::Center)), |ui| widgets::primary_button(ui, &p, &format!("{}  Open video...", I::FOLDER_OPEN))).inner;
            if open.clicked() {
                let exts: Vec<&str> = super::VIDEO_EXTS.to_vec();
                if let Some(files) = rfd::FileDialog::new().add_filter("Videos", &exts).pick_files() {
                    for f in files.into_iter().take(2) {
                        self.add_source(&f, &core.ctx);
                    }
                }
            }
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let (dw, dh) = self.sources[0].display_size();
        let compare = self.sources.len() == 2;
        let sbs = compare && self.mode == CompareMode::SideBySide;
        let pane_w = if sbs { view.width() / 2.0 } else { view.width() };
        let fit = ((pane_w * ppp) / dw as f32).min(view.height() * ppp / dh as f32);
        if self.auto_fit {
            self.zoom = fit;
            self.center = Vec2::new(dw as f32 / 2.0, dh as f32 / 2.0);
        }
        // Zoom with wheel at cursor, drag to pan.
        let wheel = ui.input(|i| crate::ui::wheel_delta(i));
        if resp.hovered() && wheel != 0.0 {
            let f = (1.0 + wheel.clamp(-200.0, 200.0) * 0.0045).max(0.2);
            let anchor = ui.input(|i| i.pointer.hover_pos()).unwrap_or(view.center());
            let pane_center = if sbs && anchor.x > view.center().x { Pos2::new(view.min.x + pane_w * 1.5, view.center().y) } else if sbs { Pos2::new(view.min.x + pane_w * 0.5, view.center().y) } else { view.center() };
            let new = (self.zoom * f).clamp(0.02, 64.0);
            let img_pt = self.center + (anchor - pane_center) * ppp / self.zoom;
            self.center = img_pt - (anchor - pane_center) * ppp / new;
            self.zoom = new;
            self.auto_fit = false;
        }
        if resp.dragged_by(egui::PointerButton::Primary) && !(compare && self.mode == CompareMode::Wipe && ui.input(|i| i.modifiers.shift)) {
            self.center -= resp.drag_delta() * ppp / self.zoom;
            self.auto_fit = false;
        }
        if resp.double_clicked() {
            self.auto_fit = !self.auto_fit;
            if !self.auto_fit {
                self.zoom = 1.0;
            }
        }
        // A click that only dismisses the picture's menu (or another popup)
        // does not also play or pause.
        if resp.clicked() && !compare && !egui::Popup::is_any_open(ui.ctx()) {
            self.toggle_play();
        }
        // Request an output size matching the on-screen size (saves decode→upload bandwidth).
        let scale = (self.zoom).clamp(0.05, 1.0);
        let q = ((scale * 16.0).ceil() / 16.0).min(1.0);
        let (nw, nh) = (self.sources[0].info.width, self.sources[0].info.height);
        let want = (((nw as f32 * q) as u32) & !1, ((nh as f32 * q) as u32) & !1);
        if want != self.sources[0].out_size && self.last_out_req.elapsed() > Duration::from_millis(150) {
            self.last_out_req = Instant::now();
            for i in 0..self.sources.len() {
                self.sources[i].out_size = want;
                self.sources[i].player.send(Cmd::OutputSize(want.0.max(2), want.1.max(2)));
            }
            if !self.playing {
                let t = self.sources[0].pts();
                let off = self.offset_secs();
                for (i, s) in self.sources.iter_mut().enumerate() {
                    s.history.clear();
                    s.seek(if i == 0 { t } else { t + off }, true);
                }
            }
        }
        let nearest = self.zoom >= 2.0;
        for s in &mut self.sources {
            s.upload(ui.ctx(), nearest);
        }
        let draw_w = dw as f32 * self.zoom / ppp;
        let draw_h = dh as f32 * self.zoom / ppp;
        let painter = ui.painter().with_clip_rect(view);
        let pane = |idx: usize| -> Rect {
            if sbs {
                Rect::from_min_size(Pos2::new(view.min.x + pane_w * idx as f32, view.min.y), Vec2::new(pane_w, view.height()))
            } else {
                view
            }
        };
        let dest_in = |r: Rect, center: Vec2, zoom: f32| -> Rect {
            let min = r.center() - Vec2::new(center.x * zoom / ppp, center.y * zoom / ppp);
            Rect::from_min_size(Pos2::new((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp), Vec2::new(draw_w, draw_h))
        };
        let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let draw_tex = |painter: &egui::Painter, s: &Source, dest: Rect, clip: Rect, tint: Color32| {
            if let Some(t) = &s.tex {
                let p2 = painter.with_clip_rect(clip);
                let mut mesh = egui::Mesh::with_texture(t.id());
                mesh.add_rect_with_uv(dest, full_uv, tint);
                rotate_mesh(&mut mesh, dest, s.info.rotation);
                p2.add(egui::Shape::mesh(mesh));
            }
        };
        // Where A and B are drawn, for the Analysis panel's overlays.
        let publish = |slot, dest: Rect, clip: Rect| crate::analysis_ui::canvas::publish(ui.ctx(), slot, dest, (dw, dh), clip);
        use crate::analysis_ui::canvas::Slot;
        if !compare {
            let dest = dest_in(view, self.center, self.zoom);
            publish(Slot::A, dest, view);
            draw_tex(&painter, &self.sources[0], dest, view, Color32::WHITE);
            self.pixel_probe(ui, dest, (dw, dh));
        } else {
            let a = &self.sources[0];
            let b = &self.sources[1];
            match self.mode {
                CompareMode::SideBySide => {
                    for (i, s) in [a, b].iter().enumerate() {
                        let r = pane(i);
                        let dest = dest_in(r, self.center, self.zoom);
                        publish(if i == 0 { Slot::A } else { Slot::B }, dest, r);
                        draw_tex(&painter, s, dest, r, Color32::WHITE);
                    }
                    painter.line_segment([Pos2::new(view.center().x, view.min.y), Pos2::new(view.center().x, view.max.y)], Stroke::new(1.0, Color32::from_gray(60)));
                }
                CompareMode::Wipe => {
                    let dest = dest_in(view, self.center, self.zoom);
                    // Not while the pointer is over a menu above the picture.
                    if !self.wipe_frozen && !ui.input(|i| i.modifiers.shift) && self.hover_view {
                        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                            if view.contains(pos) {
                                self.wipe_pos = ((pos.x - view.min.x) / view.width()).clamp(0.0, 1.0);
                            }
                        }
                    }
                    let x = view.min.x + view.width() * self.wipe_pos;
                    publish(Slot::A, dest, Rect::from_min_max(view.min, Pos2::new(x, view.max.y)));
                    publish(Slot::B, dest, Rect::from_min_max(Pos2::new(x, view.min.y), view.max));
                    draw_tex(&painter, a, dest, Rect::from_min_max(view.min, Pos2::new(x, view.max.y)), Color32::WHITE);
                    draw_tex(&painter, b, dest, Rect::from_min_max(Pos2::new(x, view.min.y), view.max), Color32::WHITE);
                    painter.line_segment([Pos2::new(x, view.min.y), Pos2::new(x, view.max.y)], Stroke::new(1.5, Color32::from_white_alpha(200)));
                    painter.circle_filled(Pos2::new(x, view.center().y), 12.0, Color32::from_black_alpha(160));
                    painter.text(Pos2::new(x, view.center().y), Align2::CENTER_CENTER, I::ARROWS_LEFT_RIGHT, FontId::proportional(13.0), Color32::WHITE);
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Flicker => {
                    let dest = dest_in(view, self.center, self.zoom);
                    let held = ui.input(|i| i.key_down(Key::Tab));
                    let show_b = self.flicker_b ^ held;
                    publish(if show_b { Slot::B } else { Slot::A }, dest, view);
                    draw_tex(&painter, if show_b { b } else { a }, dest, view, Color32::WHITE);
                    let (label, i) = if show_b { ("B", 1) } else { ("A", 0) };
                    paint_badge(&painter, view.min + Vec2::splat(12.0 + BADGE_R), label, ab_color(&p, i), BADGE_R);
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Overlay => {
                    let dest = dest_in(view, self.center, self.zoom);
                    publish(Slot::A, dest, view);
                    draw_tex(&painter, a, dest, view, Color32::WHITE);
                    draw_tex(&painter, b, dest, view, Color32::from_white_alpha(128));
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Checker => {
                    let dest = dest_in(view, self.center, self.zoom);
                    publish(Slot::A, dest, view);
                    draw_tex(&painter, a, dest, view, Color32::WHITE);
                    let cell = 64.0;
                    let mut y = dest.min.y.max(view.min.y - cell) - ((dest.min.y.max(view.min.y - cell) - dest.min.y) % cell);
                    let mut row = ((y - dest.min.y) / cell).round() as i64;
                    while y < dest.max.y.min(view.max.y) {
                        let mut x = dest.min.x;
                        let mut col = 0i64;
                        while x < dest.max.x.min(view.max.x) {
                            if (row + col) % 2 == 1 {
                                let r = Rect::from_min_size(Pos2::new(x, y), Vec2::splat(cell)).intersect(dest).intersect(view);
                                if r.is_positive() {
                                    draw_tex(&painter, b, dest, r, Color32::WHITE);
                                }
                            }
                            x += cell;
                            col += 1;
                        }
                        y += cell;
                        row += 1;
                    }
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Difference => {
                    let dest = dest_in(view, self.center, self.zoom);
                    publish(Slot::A, dest, view);
                    self.update_diff(ui.ctx());
                    if let Some((_, t)) = &self.diff_tex {
                        let mut mesh = egui::Mesh::with_texture(t.id());
                        mesh.add_rect_with_uv(dest, full_uv, Color32::WHITE);
                        rotate_mesh(&mut mesh, dest, self.sources[0].info.rotation);
                        painter.add(egui::Shape::mesh(mesh));
                    }
                    if let Some((mean, max)) = self.diff_stats {
                        let txt = format!("|A-B| × {:.0}  ·  mean {:.2}  ·  max {:.0}", self.gain, mean, max);
                        let g = painter.layout_no_wrap(txt, theme::text::small(), p.text);
                        osd_pill(&painter, &p, view.min + Vec2::splat(12.0), Align2::LEFT_TOP, g);
                    }
                    self.pixel_probe(ui, dest, (dw, dh));
                }
            }
            // Pane badges: A top left, B top left of its pane (side by side)
            // or top right (wipe).
            let inset = 12.0 + BADGE_R;
            let badges: Vec<(Pos2, usize)> = match self.mode {
                CompareMode::SideBySide => vec![(pane(0).left_top() + Vec2::splat(inset), 0), (pane(1).left_top() + Vec2::splat(inset), 1)],
                CompareMode::Wipe => vec![(view.left_top() + Vec2::splat(inset), 0), (view.right_top() + Vec2::new(-inset, inset), 1)],
                _ => vec![],
            };
            for (c, i) in badges {
                paint_badge(&painter, c, ["A", "B"][i], ab_color(&p, i), BADGE_R);
            }
            self.maybe_metrics();
        }
        // Clamp pan.
        let half_w = pane_w * ppp / self.zoom / 2.0;
        let half_h = view.height() * ppp / self.zoom / 2.0;
        self.center.x = if dw as f32 <= half_w * 2.0 { dw as f32 / 2.0 } else { self.center.x.clamp(half_w, dw as f32 - half_w) };
        self.center.y = if dh as f32 <= half_h * 2.0 { dh as f32 / 2.0 } else { self.center.y.clamp(half_h, dh as f32 - half_h) };

        // HUD: frame / time / zoom
        let a = &self.sources[0];
        let f = a.frame_of(a.pts());
        let kind = a.current.as_ref().map(|c| c.kind as char).unwrap_or('?');
        // One pill: the timecode (monospace), then frame, type and zoom.
        let mut rest = format!("Frame {f}  ·  {}  ·  {:.0}%", kind_name(kind), self.zoom * 100.0);
        if let Some(e) = &a.error {
            rest = format!("{rest}  ·  {} {e}", I::WARNING);
        }
        let job = |time: bool, rest: &str| {
            let mut job = egui::text::LayoutJob::default();
            let fmt = |font: FontId, color: Color32| egui::TextFormat { font_id: font, color, valign: egui::Align::Center, ..Default::default() };
            if time {
                job.append(&fmt_time(a.pts()), 0.0, fmt(theme::text::mono(), p.text));
                job.append("  ·  ", 0.0, fmt(theme::text::small(), p.muted));
            }
            job.append(rest, 0.0, fmt(theme::text::small(), p.text));
            job
        };
        // In a narrow view the readout shortens (the transport bar has the
        // same numbers), down to nothing, rather than running off the left.
        let fits = |job: egui::text::LayoutJob| {
            let g = painter.layout_job(job);
            (g.size().x + 2.0 * OSD_PAD + 24.0 <= view.width()).then_some(g)
        };
        if let Some(g) = fits(job(true, &rest)).or_else(|| fits(job(false, &format!("Frame {f}  ·  {}", kind_name(kind))))) {
            osd_pill(&painter, &p, view.right_bottom() - Vec2::splat(12.0), Align2::RIGHT_BOTTOM, g);
        }
        // A slow seek (long GOP) keeps the last frame on screen with a badge,
        // so stepping never looks frozen.
        if let Some(t0) = a.seek_started {
            let delay = Duration::from_millis(150);
            if t0.elapsed() >= delay {
                let txt = match self.pending_step {
                    Some(f) => format!("Seeking to frame {f}..."),
                    None => "Seeking...".to_string(),
                };
                let g = painter.layout_no_wrap(txt, theme::text::body(), p.text);
                osd_pill(&painter, &p, Pos2::new(view.center().x, view.min.y + 20.0), Align2::CENTER_TOP, g);
            } else {
                ui.ctx().request_repaint_after(delay - t0.elapsed());
            }
        }
        if self.audio.is_none() {
            if let Some(e) = &self.audio_error {
                painter.text(view.left_bottom() + Vec2::new(14.0, -20.0), Align2::LEFT_CENTER, format!("{}  No audio output: {e}", I::SPEAKER_SLASH), FontId::proportional(11.5), p.muted);
            }
        }
        resp.context_menu(|ui| self.canvas_menu(ui, core));
    }

    /// The picture's right-click menu, laid out like the viewer's: the
    /// transport, snapshots, compare modes (two videos), panels and leaving.
    /// Esc closes only the menu (see `keys`).
    fn canvas_menu(&mut self, ui: &mut Ui, core: &mut Core) {
        ui.set_min_width(230.0);
        let item = |ui: &mut Ui, icon: &str, label: &str, key: &str| widgets::menu_item(ui, icon, label, key).clicked();
        // On / off rows show a check mark instead of their icon while on.
        let check = |ui: &mut Ui, icon: &str, label: &str, key: &str, on: bool| widgets::menu_check(ui, on, icon, label, key).clicked();
        let (icon, label) = if self.playing { (I::PAUSE, "Pause") } else { (I::PLAY, "Play") };
        if item(ui, icon, label, "Space") {
            self.toggle_play();
            ui.close();
        }
        if item(ui, I::CARET_LEFT, "Previous frame", ",") {
            self.step(-1);
            ui.close();
        }
        if item(ui, I::CARET_RIGHT, "Next frame", ".") {
            self.step(1);
            ui.close();
        }
        if item(ui, I::HASH, "Go to frame...", "G") {
            self.goto = Some(String::new());
            ui.close();
        }
        ui.separator();
        if item(ui, I::CAMERA, "Save frame as PNG...", "Ctrl+S") {
            ui.close();
            self.snapshot(false, core);
        }
        if item(ui, I::COPY, "Copy frame", "Ctrl+C") {
            ui.close();
            self.snapshot(true, core);
        }
        if self.sources.len() == 2 {
            ui.separator();
            widgets::submenu(ui, self.mode.icon(), "Compare", |ui| {
                ui.set_min_width(210.0);
                for m in CompareMode::ALL {
                    if check(ui, m.icon(), m.label(), m.key(), self.mode == m) {
                        self.mode = m;
                        ui.close();
                    }
                }
                ui.separator();
                if item(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B", "X") {
                    self.swap();
                    ui.close();
                }
            });
        }
        ui.separator();
        // Opens the Analysis panel (never closes it, unlike Ctrl+Q).
        let analysis = Command::ToggleAnalysis;
        if item(ui, analysis.icon(), "Analyze...", &analysis.shortcut()) {
            core.analysis_request = Some(crate::analysis_ui::PanelRequest::Open);
            ui.close();
        }
        if check(ui, I::INFO, "Media info", "I", self.show_info) {
            self.show_info = !self.show_info;
            ui.close();
        }
        if check(ui, I::WAVEFORM, "Scopes", "F2", self.show_scopes) {
            self.show_scopes = !self.show_scopes;
            ui.close();
        }
        ui.separator();
        if item(ui, I::X, "Close video", "Esc") {
            self.close_requested = true;
            ui.close();
        }
    }

    /// What the Analysis panel works on: the displayed frame of A (and of
    /// B), decoded at native resolution through `engine::frame_at` on Run.
    pub fn analysis_subject(&self) -> crate::analysis_ui::Subject {
        use crate::analysis_ui::{FrameInfo, Pixels, Subject, SubjectImage, SubjectKind};
        let img = |s: &Source| {
            let pts = s.pts();
            SubjectImage {
                path: s.path.clone(),
                pixels: Pixels::VideoFrame { path: s.path.clone(), pts },
                size: Some(s.display_size()),
                frame: Some(FrameInfo { timestamp_s: pts, index: s.frame_of(pts) as u64 }),
            }
        };
        let b = self.sources.get(1).map(img);
        let kind = if b.is_some() { SubjectKind::VideoPair } else { SubjectKind::Video };
        Subject { kind, a: self.sources.first().map(img), b }
    }

    /// Shows `r` (fractions 0..1 of the frame) in a pane of `pane` points.
    pub fn zoom_to_region(&mut self, r: [f32; 4], pane: Vec2, ppp: f32) {
        let Some(s) = self.sources.first() else { return };
        let (w, h) = s.display_size();
        let (w, h) = (w as f32, h as f32);
        let (rw, rh) = (((r[2] - r[0]) * w).max(16.0), ((r[3] - r[1]) * h).max(16.0));
        self.zoom = ((pane.x * ppp / rw).min(pane.y * ppp / rh) * 0.6).clamp(0.02, 8.0);
        self.center = Vec2::new((r[0] + r[2]) / 2.0 * w, (r[1] + r[3]) / 2.0 * h);
        self.auto_fit = false;
    }

    fn pixel_probe(&mut self, ui: &Ui, dest: Rect, (dw, dh): (u32, u32)) {
        self.pixel = None;
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            if dest.contains(pos) {
                let x = ((pos.x - dest.min.x) / dest.width() * dw as f32) as u32;
                let y = ((pos.y - dest.min.y) / dest.height() * dh as f32) as u32;
                self.pixel = Some((x.min(dw - 1), y.min(dh - 1)));
            }
        }
    }

    fn update_diff(&mut self, ctx: &egui::Context) {
        let (Some(a), Some(b)) = (self.sources[0].current.as_ref(), self.sources[1].current.as_ref()) else { return };
        let key = (a.pts.to_bits(), b.pts.to_bits(), self.gain.to_bits());
        if self.diff_tex.as_ref().map(|(k, _)| *k == key).unwrap_or(false) {
            return;
        }
        let (ia, ib) = (&a.rgba, &b.rgba);
        let (w, h) = (ia.width().min(ib.width()), ia.height().min(ib.height()));
        let gain = self.gain;
        let mut out = vec![Color32::BLACK; (w * h) as usize];
        let mut sum = 0f64;
        let mut max = 0u8;
        for y in 0..h {
            for x in 0..w {
                let pa = ia.get_pixel(x, y).0;
                let pb = ib.get_pixel(x, y).0;
                let d = [pa[0].abs_diff(pb[0]), pa[1].abs_diff(pb[1]), pa[2].abs_diff(pb[2])];
                let m = d[0].max(d[1]).max(d[2]);
                max = max.max(m);
                sum += (d[0] as f64 + d[1] as f64 + d[2] as f64) / 3.0;
                let amp = |v: u8| ((v as f32 * gain).min(255.0)) as u8;
                out[(y * w + x) as usize] = Color32::from_rgb(amp(d[0]), amp(d[1]), amp(d[2]));
            }
        }
        self.diff_stats = Some((sum / (w as f64 * h as f64).max(1.0), max as f64));
        let ci = egui::ColorImage::new([w as usize, h as usize], out);
        match &mut self.diff_tex {
            Some((k, t)) => {
                t.set(ci, egui::TextureOptions::NEAREST);
                *k = key;
            }
            None => self.diff_tex = Some((key, ctx.load_texture("video-diff", ci, egui::TextureOptions::NEAREST))),
        }
    }

    fn maybe_metrics(&mut self) {
        if self.playing || self.metrics_rx.is_some() || self.sources.len() < 2 {
            return;
        }
        let (Some(a), Some(b)) = (self.sources[0].current.as_ref(), self.sources[1].current.as_ref()) else { return };
        let key = (a.pts.to_bits(), b.pts.to_bits());
        if self.metrics_now.as_ref().map(|(k, _)| *k == key).unwrap_or(false) {
            return;
        }
        let (Some(na), Some(nb)) = (a.native.clone(), b.native.clone()) else { return };
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send((key, metrics::compare(&na, &nb)));
        });
        self.metrics_rx = Some(rx);
    }

    // ---------------------------------------------------------------- transport

    /// The Analysis panel's Metrics tab: what it measures, the frame it
    /// asks to show, and native planes of the paused frame while it is open.
    fn metrics_hooks(&mut self, core: &mut Core) {
        use crate::analysis_ui::metrics_view::{VideoSideCtx, VideoSubject};
        let Some(a) = self.sources.first() else {
            core.metrics.set_video(None);
            return;
        };
        let side = |s: &Source| VideoSideCtx::new(&s.path, &s.info, &s.index, s.current.as_ref().and_then(|f| f.native.clone()));
        let loop_ab = match self.loop_ab {
            (Some(x), Some(y)) => Some((a.frame_of(x), a.frame_of(y))),
            _ => None,
        };
        let current = a.current.as_ref().map(|f| a.frame_of(f.pts));
        core.metrics.set_video(Some(VideoSubject { a: side(a), b: self.sources.get(1).map(side), offset: self.offset_frames, current, loop_ab }));
        if let Some(f) = core.metrics.take_seek() {
            let t = a.pts_of(f as usize);
            self.pause();
            self.seek_all(t, true);
        }
        // With B loaded both players already copy native planes.
        if self.sources.len() != 1 {
            self.metrics_native = false;
        } else if core.metrics.visible() != self.metrics_native {
            self.metrics_native = !self.metrics_native;
            self.sources[0].player.send(Cmd::Native(self.metrics_native));
            if self.metrics_native && !self.playing {
                // Decode the paused frame again, now with its planes.
                let t = self.sources[0].pts();
                self.seek_all(t, true);
            }
        }
    }

    fn transport(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        if self.sources.is_empty() {
            ui.add_space(4.0);
            return;
        }
        self.timeline(ui, &p);
        if core.metrics.has_plots() {
            // Graphs of the Metrics tab's frame range, on the timeline's frame axis.
            let a = &self.sources[0];
            let cur = a.current.as_ref().map(|f| a.frame_of(f.pts) as u64);
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("video-metric-plots").max_height(170.0).show(ui, |ui| core.metrics.video_plots(ui, &p, cur, 46.0));
        }
        ui.add_space(4.0);
        // As tall as the play button, so the readouts and the right-hand
        // controls center on the icons.
        let row = Vec2::new(ui.available_width(), 34.0);
        ui.allocate_ui_with_layout(row, egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let fit = self.transport_fit(ui);
            let b = |ui: &mut Ui, icon: &str, tip: &str| widgets::icon_button(ui, icon, tip, false).clicked();
            if fit.first_last && b(ui, I::SKIP_BACK, "First frame (Home)") {
                self.seek_all(0.0, true);
            }
            if fit.skip_secs && b(ui, I::CARET_DOUBLE_LEFT, "Back 1 s (Shift+←)") {
                let t = self.sources[0].pts() - 1.0;
                self.seek_all(t, true);
            }
            if b(ui, I::CARET_LEFT, "Previous frame (, or ← while paused)") {
                self.step(-1);
            }
            let icon = if self.playing { I::PAUSE } else { I::PLAY };
            if widgets::icon_button_sized(ui, icon, "Play / pause (Space)", true, 34.0, 18.0).clicked() {
                self.toggle_play();
            }
            if b(ui, I::CARET_RIGHT, "Next frame (. or → while paused)") {
                self.step(1);
            }
            if fit.skip_secs && b(ui, I::CARET_DOUBLE_RIGHT, "Forward 1 s (Shift+→)") {
                let t = self.sources[0].pts() + 1.0;
                self.seek_all(t, true);
            }
            if fit.first_last && b(ui, I::SKIP_FORWARD, "Last frame (End)") {
                let n = self.sources[0].frame_count().unwrap_or(1);
                let t = self.sources[0].pts_of(n.saturating_sub(1));
                self.seek_all(t, true);
            }
            widgets::separator_v(ui);
            let a = &self.sources[0];
            let (time, frame) = self.position_text(fit.duration, fit.frame_word);
            if fit.time {
                ui.add_space(4.0);
                readout(ui, time, p.text, Sense::hover()).on_hover_text(format!("{} / {}", fmt_time(a.pts()), fmt_time(a.duration())));
                ui.add_space(2.0);
            }
            let fr = readout(ui, frame, p.muted, Sense::click()).on_hover_text("Go to frame or time (G)");
            if fr.clicked() {
                self.goto = Some(String::new());
            }
            // A-B loop
            if fit.loop_button {
                widgets::separator_v(ui);
                let lab = self.loop_label();
                let text = if fit.loop_label { format!("{} {lab}", I::REPEAT) } else { I::REPEAT.to_string() };
                if ui.add(egui::Button::new(RichText::new(text).size(12.5)).selected(self.loop_ab.1.is_some()).min_size(Vec2::new(0.0, 26.0))).on_hover_text(format!("{lab} (Ctrl+L)")).clicked() {
                    self.toggle_loop();
                }
            }
            if fit.legend {
                widgets::separator_v(ui);
                ui.add_space(6.0);
                self.timeline_legend(ui, &p);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Volume: a slim slider (a smaller knob than the default).
                ui.spacing_mut().slider_width = 90.0;
                if self.audio.is_some() && self.sources[0].info.has_audio {
                    let slim = |ui: &mut Ui, v: &mut f32| ui.add(widgets::Slider::new(v, 0.0..=1.5).show_value(false).width(90.0).default_value(1.0));
                    if fit.volume && slim(ui, &mut self.volume).on_hover_text("Volume (double-click: 100%)").changed() {
                        if let Some(a) = self.audio_shared() {
                            a.set_volume(self.volume);
                        }
                    }
                    let vi = if self.muted || self.volume == 0.0 { I::SPEAKER_SLASH } else { I::SPEAKER_HIGH };
                    if widgets::icon_button(ui, vi, "Mute (M)", self.muted).clicked() {
                        self.toggle_mute();
                    }
                } else if fit.volume {
                    ui.label(RichText::new(format!("{} no audio", I::SPEAKER_SLASH)).size(11.5).color(p.faint));
                } else {
                    ui.label(RichText::new(I::SPEAKER_SLASH).size(11.5).color(p.faint)).on_hover_text("No audio");
                }
                widgets::separator_v(ui);
                let speed = ui.menu_button(RichText::new(format!("{}×  {}", self.speed, I::CARET_DOWN)).size(12.5), |ui| {
                    ui.set_min_width(80.0);
                    for s in SPEEDS {
                        if widgets::menu_toggle(ui, self.speed == s, &format!("{s}×"), "").clicked() {
                            self.set_speed(s);
                            ui.close();
                        }
                    }
                });
                speed.response.on_hover_text("Playback speed ([ and ]; Backspace: 1×)");
                if fit.speed_label {
                    ui.label(RichText::new("Speed").size(12.5).color(p.muted));
                }
            });
        });
    }

    fn loop_label(&self) -> &'static str {
        match self.loop_ab {
            (None, _) => "Loop A-B",
            (Some(_), None) => "Set B",
            _ => "Clear loop",
        }
    }

    /// The time and frame counter of the transport bar (`duration`: with
    /// the total time, `word`: "frame 12 / 300" rather than "12 / 300").
    /// The counter is padded on the left to its width on the last frame, so
    /// in the monospace readout nothing after it moves while the video plays.
    fn position_text(&self, duration: bool, word: bool) -> (String, String) {
        let a = &self.sources[0];
        let total = a.frame_count().map(|n| n.to_string()).unwrap_or_else(|| "...".into());
        let time = if duration { format!("{} / {}", fmt_time(a.pts()), fmt_time(a.duration())) } else { fmt_time(a.pts()) };
        let word = if word { "frame " } else { "" };
        let widest = format!("{word}{total} / {total}").chars().count();
        let frame = format!("{:>widest$}", format!("{word}{} / {total}", a.frame_of(a.pts())));
        (time, frame)
    }

    /// What fits in the transport bar: at a large UI scale with the
    /// Analysis panel open the legend, labels and the less used buttons go
    /// first (Prev / Play / Next, the counters and Speed always stay).
    fn transport_fit(&self, ui: &Ui) -> TransportFit {
        let avail = ui.available_width();
        let s = ui.spacing().item_spacing.x;
        let pad = ui.spacing().button_padding.x * 2.0;
        let text_w = |t: &str, f: FontId| ui.painter().layout_no_wrap(t.to_string(), f, Color32::WHITE).size().x;
        let mono = FontId::monospace(12.5);
        let small = FontId::proportional(12.5);
        let icon = 30.0 + s;
        let sep = 9.0 + s;
        let audio = self.audio.is_some() && self.sources[0].info.has_audio;
        let width = |f: &TransportFit| -> f32 {
            let (time, frame) = self.position_text(f.duration, f.frame_word);
            let mut w = 2.0 * icon + 34.0 + s + sep;
            w += if f.first_last { 2.0 * icon } else { 0.0 } + if f.skip_secs { 2.0 * icon } else { 0.0 };
            w += if f.time { text_w(&time, mono.clone()) + 2.0 * s + 6.0 } else { 0.0 } + text_w(&frame, mono.clone()) + 8.0 + s;
            if f.loop_button {
                w += sep + if f.loop_label { text_w(&format!("{} {}", I::REPEAT, self.loop_label()), small.clone()) } else { text_w(I::REPEAT, small.clone()) } + pad + s;
            }
            if f.legend {
                let cap = FontId::proportional(theme::text::CAPTION);
                let psnr = if self.analysis.is_some() { 26.0 + text_w("PSNR-Y (20-60 dB)", cap.clone()) } else { 0.0 };
                w += sep + 6.0 + 22.0 + text_w("Frame size", cap.clone()) + 20.0 + text_w("I-frame", cap) + psnr;
            }
            // Right: volume or "no audio", speed.
            w += match (audio, f.volume) {
                (true, true) => 90.0 + s + icon,
                (true, false) => icon,
                (false, true) => text_w(&format!("{} no audio", I::SPEAKER_SLASH), FontId::proportional(11.5)) + s,
                (false, false) => 16.0 + s,
            };
            w += sep + text_w(&format!("0.25×  {}", I::CARET_DOWN), small.clone()) + pad + s + if f.speed_label { text_w("Speed", small.clone()) + s } else { 0.0 };
            // Room between the left and right groups.
            w + 12.0
        };
        let mut f = TransportFit::default();
        for step in 0..10 {
            if width(&f) <= avail {
                break;
            }
            match step {
                0 => f.legend = false,
                1 => f.speed_label = false,
                2 => f.skip_secs = false,
                3 => f.loop_label = false,
                4 => f.volume = false,
                5 => f.first_last = false,
                6 => f.frame_word = false,
                7 => f.duration = false,
                8 => f.loop_button = false,
                _ => f.time = false,
            }
        }
        f
    }

    fn set_speed(&mut self, s: f64) {
        let was = self.playing;
        if was {
            self.pause();
        }
        self.speed = s;
        // Re-seek so queued audio/video from the old speed is discarded and A/V stays in sync.
        if let Some(t) = self.sources.first().map(|x| x.pts()) {
            self.seek_all(t, true);
        }
        if was {
            self.play();
        }
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(a) = self.audio_shared() {
            a.muted.store(self.muted, Ordering::Relaxed);
        }
    }

    fn toggle_loop(&mut self) {
        let t = self.sources.first().map(|s| s.pts()).unwrap_or(0.0);
        self.loop_ab = match self.loop_ab {
            (None, _) => (Some(t), None),
            (Some(a), None) if t > a => (Some(a), Some(t)),
            (Some(_), None) => (Some(t), None),
            _ => (None, None),
        };
    }

    fn timeline(&mut self, ui: &mut Ui, p: &Palette) {
        // Tick to label: half the playhead's head (5 pt) plus a 4 pt gap.
        const LABEL_GAP: f32 = 9.0;
        let h = 60.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click_and_drag());
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, CornerRadius::same(theme::radius::CONTROL), ui.visuals().extreme_bg_color);
        let a = &self.sources[0];
        let dur = a.duration().max(0.001);
        // The time axis is inset so the playhead's 10 pt head stays whole at
        // either end.
        let track = rect.shrink2(Vec2::new(6.0, 0.0));
        let x_of = |t: f64| track.min.x + (t / dur) as f32 * track.width();
        let t_of = |x: f32| (((x - track.min.x) / track.width()) as f64 * dur).clamp(0.0, dur);
        // A time ruler along the top, the frame-size bars in the middle and a
        // strip at the bottom with the frame types (I ticks; P and B dots once
        // frames are far enough apart to tell them apart).
        let ruler_h = 16.0;
        let strip_h = 10.0;
        let base = rect.max.y - strip_h;
        let bar_h = base - (rect.min.y + ruler_h) - 2.0;
        // Loop region
        if let (Some(la), lb) = self.loop_ab {
            let lb = lb.unwrap_or(a.pts());
            painter.rect_filled(Rect::from_min_max(Pos2::new(x_of(la), rect.min.y), Pos2::new(x_of(lb), rect.max.y)), 0.0, p.accent.gamma_multiply(0.18));
        }
        let step = tick_step(dur, track.width(), 84.0);
        let label_font = FontId::proportional(theme::text::CAPTION);
        for k in 0.. {
            let t = k as f64 * step;
            if t > dur {
                break;
            }
            let x = x_of(t).round() + 0.5;
            painter.line_segment([Pos2::new(x, rect.min.y + ruler_h - 5.0), Pos2::new(x, rect.min.y + ruler_h)], Stroke::new(1.0, p.faint));
            let g = painter.layout_no_wrap(fmt_tick(t, step, dur), label_font.clone(), p.faint);
            // Labels start 4 pt clear of the playhead's 10 pt head resting on
            // the tick (at 0:00 before playing); one that would run off the
            // right edge is skipped.
            let lx = x + LABEL_GAP;
            if lx + g.size().x <= rect.max.x - 2.0 {
                painter.galley(Pos2::new(lx, rect.min.y + ruler_h / 2.0 - g.size().y / 2.0), g, p.faint);
            }
        }
        painter.line_segment([Pos2::new(rect.min.x, base + 0.5), Pos2::new(rect.max.x, base + 0.5)], Stroke::new(1.0, p.border));
        // Frame-size bars (StreamEye-style), aggregated per pixel column; the
        // keyframes' share of a column is drawn a little lighter.
        let idx_guard = a.index.lock();
        if let Some(idx) = idx_guard.as_ref() {
            let cols = track.width().max(1.0) as usize;
            // Aggregating 100k+ index entries is too costly per frame: cache it
            // per (entry count, width) and refresh periodically as frame types are learned.
            let stale = match &self.timeline_cache {
                Some((n, c, _, at)) => *n != idx.len() || *c != cols || at.elapsed() > Duration::from_secs(2),
                None => true,
            };
            if stale {
                self.timeline_cache = Some((idx.len(), cols, timeline_columns(idx, dur, cols), Instant::now()));
            }
            let (_, _, columns, _) = self.timeline_cache.as_ref().unwrap();
            let tints = timeline_tints(p);
            let mut mesh = egui::Mesh::default();
            let mut last_tick = f32::NEG_INFINITY;
            let tick = p.muted.gamma_multiply(0.8);
            for (c, col) in columns.iter().enumerate() {
                let x = track.min.x + c as f32;
                // An I tick per column with a keyframe, at least 3 points apart.
                if col.key && x - last_tick >= 3.0 {
                    mesh.add_colored_rect(Rect::from_min_size(Pos2::new(x, base + 2.0), Vec2::new(1.0, 6.0)), tick);
                    last_tick = x;
                }
                if col.height <= 0.0 {
                    continue;
                }
                // Stack the frame types bottom-up by their share of the column's bytes.
                let mut y = base;
                for (share, color) in col.shares.iter().zip(tints) {
                    if *share <= 0.0 {
                        continue;
                    }
                    let top = y - share * col.height * bar_h;
                    mesh.add_colored_rect(Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + 1.0, y)), color);
                    y = top;
                }
            }
            // P and B frames as dots when there are at least 6 points per frame.
            if !idx.is_empty() && track.width() / idx.len() as f32 >= 6.0 {
                for e in idx.iter().filter(|e| !e.key && e.kind != b'I') {
                    let c = Pos2::new(x_of(e.pts), base + 5.0);
                    let r = if e.kind == b'B' { 1.0 } else { 1.5 };
                    mesh.add_colored_rect(Rect::from_center_size(c, Vec2::splat(2.0 * r)), p.faint);
                }
            }
            painter.add(egui::Shape::mesh(mesh));
        } else {
            let prog = f32::from_bits(a.index_progress.load(Ordering::Relaxed));
            let text = if prog >= 1.0 { "Frame index unavailable for this file".to_string() } else { format!("Indexing frames... {:.0}%", prog * 100.0) };
            painter.text(Pos2::new(rect.center().x, (rect.min.y + ruler_h + base) / 2.0), Align2::CENTER_CENTER, text, theme::text::small(), p.faint);
            if prog < 1.0 {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
        }
        drop(idx_guard);
        // Metric graph (PSNR-Y) from analysis.
        if let Some(an) = &self.analysis {
            let rows = an.rows.lock();
            if rows.len() > 1 {
                let pts: Vec<Pos2> = rows
                    .iter()
                    .step_by((rows.len() / track.width().max(1.0) as usize).max(1))
                    .map(|(i, m)| {
                        let t = a.pts_of(*i);
                        let v = ((m.psnr_y.min(60.0) - 20.0) / 40.0).clamp(0.0, 1.0) as f32;
                        Pos2::new(x_of(t), base - v * bar_h)
                    })
                    .collect();
                painter.add(egui::Shape::line(pts, Stroke::new(1.5, PSNR_COLOR)));
            }
        }
        // Playhead (A): a 2 pt accent line with a 10 pt head on the ruler.
        let x = x_of(a.pts()).round();
        painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(2.0, p.accent));
        painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x - 5.0, rect.min.y), Pos2::new(x + 5.0, rect.min.y), Pos2::new(x + 5.0, rect.min.y + 5.0), Pos2::new(x, rect.min.y + 10.0), Pos2::new(x - 5.0, rect.min.y + 5.0)], p.accent, Stroke::NONE));
        // Hover tooltip + scrubbing.
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            let f = a.frame_of(t);
            let e = a.entry(f);
            painter.line_segment([Pos2::new(pos.x, rect.min.y), Pos2::new(pos.x, rect.max.y)], Stroke::new(1.0, p.muted.gamma_multiply(0.7)));
            let mut tip = format!("{}  ·  frame {f}", fmt_time(t));
            if let Some(e) = e {
                tip += &format!("  ·  {}  ·  {}", kind_name(e.kind as char), crate::catalog::human_size(e.size as u64));
            }
            if let Some(an) = &self.analysis {
                if let Some((_, m)) = an.rows.lock().iter().find(|(i, _)| *i == f) {
                    tip += &format!("  ·  PSNR {:.2} dB  SSIM {:.4}", metrics::cap_db(m.psnr_y), m.ssim_y);
                }
            }
            resp.clone().on_hover_text_at_pointer(tip);
        }
        if resp.dragged() || resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                self.scrubbing = resp.dragged();
                // Keyframe seeks while dragging (fast), exact on release / click.
                self.seek_all(t, !resp.dragged());
            }
        }
        if resp.drag_stopped() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                self.seek_all(t, true);
            }
            self.scrubbing = false;
        }
    }

    /// Key to the timeline, shown in the transport row so it never covers it.
    fn timeline_legend(&self, ui: &mut Ui, p: &Palette) {
        let item = |ui: &mut Ui, size: Vec2, color: Color32, round: bool, text: &str| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(size.x.max(6.0), 12.0), Sense::hover());
            let r = Rect::from_center_size(r.center(), size);
            ui.painter().rect_filled(r, CornerRadius::same(if round { 2 } else { 0 }), color);
            ui.add_space(4.0);
            ui.label(RichText::new(text).size(theme::text::CAPTION).color(p.muted));
            ui.add_space(10.0);
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            item(ui, Vec2::new(8.0, 10.0), timeline_tints(p)[1], false, "Frame size");
            item(ui, Vec2::new(1.0, 6.0), p.muted.gamma_multiply(0.8), false, "I-frame");
            if self.analysis.is_some() {
                item(ui, Vec2::new(12.0, 2.0), PSNR_COLOR, false, "PSNR-Y (20-60 dB)");
            }
        })
        .response
        .on_hover_text("Timeline: frame sizes (keyframes lighter), I-frame ticks below; P and B frames show as dots once frames are far enough apart");
    }

    // ---------------------------------------------------------------- panels

    fn info_panel(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let compare = self.sources.len() == 2;
            // Current frame
            widgets::section_title(ui, &p, "Current frame");
            for (i, s) in self.sources.iter().enumerate() {
                let f = s.frame_of(s.pts());
                let e = s.entry(f);
                let kind = s.current.as_ref().map(|c| c.kind as char).unwrap_or('?');
                let label = if i == 0 { "A" } else { "B" };
                let size = e.map(|e| crate::catalog::human_size(e.size as u64)).unwrap_or_default();
                widgets::kv_row(ui, &p, label, &format!("#{f}  {}  {}  {}", fmt_time(s.pts()), kind_name(kind), size));
            }
            if let Some((x, y)) = self.pixel {
                let mut line = format!("({x}, {y})");
                for s in &self.sources {
                    if let Some(c) = &s.current {
                        let (dw, dh) = s.display_size();
                        let sx = (x as f32 / dw as f32 * c.rgba.width() as f32) as u32;
                        let sy = (y as f32 / dh as f32 * c.rgba.height() as f32) as u32;
                        if s.info.rotation == 0 && sx < c.rgba.width() && sy < c.rgba.height() {
                            let px = c.rgba.get_pixel(sx, sy).0;
                            line += &format!("   RGB {},{},{}", px[0], px[1], px[2]);
                        }
                    }
                }
                widgets::kv_row(ui, &p, "Pixel", &line);
            }
            if compare {
                ui.add_space(8.0);
                widgets::section_title(ui, &p, "Quality (A = reference)");
                match &self.metrics_now {
                    Some((_, m)) => {
                        widgets::kv_row(ui, &p, "PSNR Y", &fmt_db(m.psnr_y));
                        widgets::kv_row(ui, &p, "PSNR U / V", &format!("{} / {}", fmt_db(m.psnr_u), fmt_db(m.psnr_v)));
                        widgets::kv_row(ui, &p, "PSNR avg", &fmt_db(m.psnr_avg));
                        widgets::kv_row(ui, &p, "SSIM Y", &fmt_ssim(m.ssim_y));
                    }
                    None => {
                        let mismatch = self.sources[0].info.width != self.sources[1].info.width || self.sources[0].info.height != self.sources[1].info.height;
                        ui.label(RichText::new(if mismatch { "Metrics need identical resolutions (A and B differ)." } else if self.playing { "Pause to measure the current frame." } else { "Measuring..." }).size(12.0).color(p.muted));
                    }
                }
                ui.add_space(6.0);
                match &self.analysis {
                    None => {
                        if ui.add(egui::Button::new(format!("{}  Analyze whole video (PSNR & SSIM per frame)", I::CHART_LINE)).min_size(Vec2::new(ui.available_width(), 28.0))).clicked() {
                            self.start_analysis();
                        }
                    }
                    Some(an) => {
                        let done = an.done.load(Ordering::Relaxed);
                        let prog = f32::from_bits(an.progress.load(Ordering::Relaxed));
                        if let Some(e) = an.error.lock().clone() {
                            ui.colored_label(p.danger, e);
                        }
                        if !done {
                            ui.add(egui::ProgressBar::new(prog).show_percentage().text(format!("Analyzing... {:.0}%", prog * 100.0)));
                            if ui.button("Cancel").clicked() {
                                an.cancel.store(true, Ordering::Relaxed);
                            }
                            ui.ctx().request_repaint_after(Duration::from_millis(200));
                        }
                        let rows = an.rows.lock().clone();
                        let ms: Vec<FrameMetrics> = rows.iter().map(|(_, m)| *m).collect();
                        let sum = metrics::summarize(&ms, an.bit_max);
                        if sum.frames > 0 {
                            widgets::kv_row(ui, &p, "Frames", &sum.frames.to_string());
                            widgets::kv_row(ui, &p, "Mean PSNR Y", &fmt_db(sum.mean_psnr_y));
                            widgets::kv_row(ui, &p, "Global PSNR", &fmt_db(sum.global_psnr));
                            widgets::kv_row(ui, &p, "Mean SSIM", &fmt_ssim(sum.mean_ssim));
                            widgets::kv_row(ui, &p, "Worst PSNR Y", &fmt_db(sum.min_psnr_y));
                            ui.add_space(4.0);
                            ui.label(RichText::new("Worst frames (click to jump)").size(11.5).color(p.faint));
                            let mut worst = rows.clone();
                            worst.sort_by(|a, b| a.1.psnr_y.partial_cmp(&b.1.psnr_y).unwrap_or(std::cmp::Ordering::Equal));
                            let mut jump = None;
                            for (i, m) in worst.iter().take(8) {
                                if ui.add(egui::Button::new(RichText::new(format!("#{i:<7} PSNR {:>6.2} dB   SSIM {:.4}", metrics::cap_db(m.psnr_y), m.ssim_y)).monospace().size(11.5)).frame(false)).clicked() {
                                    jump = Some(*i);
                                }
                            }
                            if let Some(i) = jump {
                                let t = self.sources[0].pts_of(i);
                                self.seek_all(t, true);
                            }
                            ui.horizontal(|ui| {
                                if ui.button(format!("{}  Export CSV", I::EXPORT)).clicked() {
                                    self.export_csv(core);
                                }
                                if done && ui.button("Re-run").clicked() {
                                    self.analysis = None;
                                }
                            });
                        }
                    }
                }
            }
            ui.add_space(8.0);
            widgets::section_title(ui, &p, "Media info");
            let durations: Vec<f64> = self.sources.iter().map(|s| s.duration()).collect();
            let rows = media_rows(&self.sources.iter().map(|s| &s.info).collect::<Vec<_>>(), &durations);
            media_table(ui, &p, compare, rows);
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Decoder: FFmpeg {}", engine::ffmpeg_version())).size(10.5).color(p.faint));
        });
    }

    fn scopes_panel(&mut self, ui: &mut Ui, p: &Palette) {
        // Short names so the four segments fit the panel; the full ones on hover.
        let names = ["Waveform", "Parade", "Vector", "Histogram"];
        let tips: Vec<&str> = Scope::ALL.iter().map(|s| s.label()).collect();
        let mut idx = Scope::ALL.iter().position(|s| *s == self.scope).unwrap_or(0);
        if widgets::segmented_with_tips(ui, &mut idx, &names, &tips) {
            self.scope = Scope::ALL[idx];
            self.scope_tex = None;
        }
        ui.add_space(theme::space::M);
        let Some(cur) = self.sources[0].current.clone() else { return };
        let key = cur.pts.to_bits() ^ (self.scope as u64) << 60;
        // Throttle recomputation during playback.
        let stale = self.scope_tex.as_ref().map(|(k, _)| *k != key).unwrap_or(true);
        if stale {
            let img = scopes::render(self.scope, &cur.rgba);
            let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
            match &mut self.scope_tex {
                Some((k, t)) => {
                    t.set(ci, egui::TextureOptions::LINEAR);
                    *k = key;
                }
                None => self.scope_tex = Some((key, ui.ctx().load_texture("scope", ci, egui::TextureOptions::LINEAR))),
            }
        }
        if let Some((_, t)) = &self.scope_tex {
            // The plot fills the panel's width at 16:10 (the vectorscope is square),
            // with a graticule at 0 / 25 / 50 / 75 / 100%.
            let w = ui.available_width();
            let hgt = if self.scope == Scope::Vectorscope { w } else { w * 10.0 / 16.0 };
            let (r, _) = ui.allocate_exact_size(Vec2::new(w, hgt), Sense::hover());
            let cr = CornerRadius::same(theme::radius::CONTROL);
            let painter = ui.painter();
            painter.add(egui::epaint::RectShape::filled(r, cr, Color32::WHITE).with_texture(t.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
            // The plots are always black, so the graticule is a dim white in both themes.
            let line = Stroke::new(1.0, Color32::from_white_alpha(38));
            let font = FontId::proportional(10.0);
            let label = Color32::from_white_alpha(150);
            match self.scope {
                Scope::Waveform | Scope::Parade => {
                    // Level 0..255 maps to the 256 image rows, top = 100%.
                    for pct in [0u32, 25, 50, 75, 100] {
                        let y = (r.top() + (1.0 - pct as f32 / 100.0) * (255.0 / 256.0) * r.height() + r.height() / 512.0).round() + 0.5;
                        painter.hline(r.x_range(), y, line);
                        let (pos, align) = if pct == 100 { (Pos2::new(r.left() + 4.0, y + 1.0), Align2::LEFT_TOP) } else { (Pos2::new(r.left() + 4.0, y - 1.0), Align2::LEFT_BOTTOM) };
                        painter.text(pos, align, pct.to_string(), font.clone(), label);
                    }
                }
                Scope::Histogram => {
                    for pct in [0u32, 25, 50, 75, 100] {
                        let x = (r.left() + pct as f32 / 100.0 * r.width()).round() + 0.5;
                        painter.vline(x, r.y_range(), line);
                        let (pos, align) = match pct {
                            0 => (Pos2::new(x + 3.0, r.bottom() - 2.0), Align2::LEFT_BOTTOM),
                            100 => (Pos2::new(x - 3.0, r.bottom() - 2.0), Align2::RIGHT_BOTTOM),
                            _ => (Pos2::new(x, r.bottom() - 2.0), Align2::CENTER_BOTTOM),
                        };
                        painter.text(pos, align, pct.to_string(), font.clone(), label);
                    }
                }
                Scope::Vectorscope => {}
            }
            painter.rect_stroke(r, cr, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        }
        ui.add_space(theme::space::S);
        ui.label(RichText::new(match self.scope {
            Scope::Waveform => "Luma level (bottom 0%, top 100%) per image column.",
            Scope::Parade => "R, G, B waveforms side by side.",
            Scope::Vectorscope => "Chroma (BT.709). Boxes: 75% color targets; line: skin tone.",
            Scope::Histogram => "R, G, B and luma distribution.",
        }).size(theme::text::SMALL).color(p.muted));
    }

    fn goto_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut target: Option<f64> = None;
        egui::Modal::new(egui::Id::unique("video-goto")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Go to");
            ui.label(RichText::new("Frame number (1234), time (01:02.5 or 62.5s), or relative (+10f, -2s)").size(11.5));
            let s = self.goto.as_mut().unwrap();
            let r = ui.add(egui::TextEdit::singleline(s).desired_width(f32::INFINITY));
            r.request_focus();
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                target = parse_goto(s, &self.sources[0]);
                close = true;
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.goto = None;
        }
        if let Some(t) = target {
            self.seek_all(t, true);
        }
    }

    // ---------------------------------------------------------------- keys

    fn keys(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        // A click on the B offset field starts editing it as text; + and -
        // still shift B then (typing them would only leave an unparsable
        // value behind, and the field would keep showing the old offset).
        if self.sources.len() == 2 && self.offset_field.is_some_and(|id| ctx.memory(|m| m.has_focus(id))) {
            let steps: Vec<i64> = ctx.input_mut(|i| {
                let mut steps = Vec::new();
                i.events.retain(|e| match e {
                    egui::Event::Key { key: Key::Plus | Key::Equals, pressed, modifiers, .. } => {
                        if *pressed {
                            steps.push(if modifiers.ctrl { 10 } else { 1 });
                        }
                        false
                    }
                    egui::Event::Key { key: Key::Minus, pressed, modifiers, .. } => {
                        if *pressed {
                            steps.push(if modifiers.ctrl { -10 } else { -1 });
                        }
                        false
                    }
                    egui::Event::Text(t) => !matches!(t.as_str(), "+" | "-" | "="),
                    _ => true,
                });
                steps
            });
            for d in steps {
                self.set_offset(self.offset_frames + d);
            }
        }
        if self.goto.is_some() || ctx.text_edit_focused() {
            return true;
        }
        let mut keep = true;
        let events: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        let compare = self.sources.len() == 2;
        for (k, m) in events {
            if self.sources.is_empty() {
                if k == Key::Escape {
                    keep = false;
                }
                continue;
            }
            match k {
                Key::Escape if self.was_editing => {}
                // Esc with a menu or a list open only closes it (egui does).
                Key::Escape if egui::Popup::is_any_open(ctx) => {}
                Key::Escape => keep = false,
                Key::Space | Key::K if !m.ctrl => _ = self.run_command(Command::PlayPause),
                Key::Period => _ = self.run_command(Command::NextFrame),
                Key::Comma => _ = self.run_command(Command::PrevFrame),
                Key::ArrowRight if m.ctrl => {
                    let t = self.sources[0].pts() + 10.0;
                    self.seek_all(t, true)
                }
                Key::ArrowLeft if m.ctrl => {
                    let t = self.sources[0].pts() - 10.0;
                    self.seek_all(t, true)
                }
                Key::ArrowRight if m.shift || self.playing => {
                    let t = self.sources[0].pts() + 1.0;
                    self.seek_all(t, true)
                }
                Key::ArrowLeft if m.shift || self.playing => {
                    let t = self.sources[0].pts() - 1.0;
                    self.seek_all(t, true)
                }
                Key::ArrowRight => self.step(1),
                Key::ArrowLeft => self.step(-1),
                Key::ArrowUp => {
                    let t = self.sources[0].pts() + 60.0;
                    self.seek_all(t, true)
                }
                Key::ArrowDown => {
                    let t = self.sources[0].pts() - 60.0;
                    self.seek_all(t, true)
                }
                Key::Home => self.seek_all(0.0, true),
                Key::End => {
                    let n = self.sources[0].frame_count().unwrap_or(1);
                    let t = self.sources[0].pts_of(n.saturating_sub(1));
                    self.seek_all(t, true)
                }
                Key::PageDown | Key::PageUp => self.jump_keyframe(k == Key::PageDown),
                Key::G => self.goto = Some(String::new()),
                Key::I => _ = self.run_command(Command::ToggleInfo),
                Key::F2 => self.show_scopes = !self.show_scopes,
                Key::M => self.toggle_mute(),
                Key::L if m.ctrl => self.toggle_loop(),
                Key::S if m.ctrl => self.snapshot(false, core),
                Key::C if m.ctrl => self.snapshot(true, core),
                Key::OpenBracket => self.set_speed((self.speed / 2.0).max(0.1)),
                Key::CloseBracket => self.set_speed((self.speed * 2.0).min(4.0)),
                Key::Backspace => self.set_speed(1.0),
                Key::Num0 => _ = self.run_command(Command::ZoomFit),
                Key::Num1 => _ = self.run_command(Command::ZoomActual),
                Key::S if compare => self.mode = CompareMode::SideBySide,
                Key::W if compare => self.mode = CompareMode::Wipe,
                Key::D if compare => self.mode = CompareMode::Difference,
                Key::O if compare => self.mode = CompareMode::Overlay,
                Key::C if compare => self.mode = CompareMode::Checker,
                // Tab out of a text field (the Metrics tab's range) moves the
                // keyboard focus; it is not the flicker key then.
                Key::Tab if compare && !self.was_editing && ctx.memory(|m| m.focused().is_none()) => {
                    if self.mode != CompareMode::Flicker {
                        self.mode = CompareMode::Flicker;
                    }
                }
                Key::Backtick if compare => {
                    self.mode = CompareMode::Flicker;
                    self.flicker_b = !self.flicker_b;
                }
                Key::A if compare && m.shift => {
                    self.mode = CompareMode::Flicker;
                    self.flicker_auto = !self.flicker_auto;
                }
                Key::X if compare => self.swap(),
                Key::F if compare => self.wipe_frozen = !self.wipe_frozen,
                Key::Plus | Key::Equals if compare => {
                    let d = if m.ctrl { 10 } else { 1 };
                    self.set_offset(self.offset_frames + d)
                }
                Key::Minus if compare => {
                    let d = if m.ctrl { 10 } else { 1 };
                    self.set_offset(self.offset_frames - d)
                }
                _ => {}
            }
        }
        keep
    }

    /// Dev hook: log how long a burst of steps took to land, with a hash of
    /// the landed frame so tests can check frame accuracy.
    fn dev_report_step(&mut self) {
        let Some((t0, n)) = self.dev_burst else { return };
        let Some(a) = self.sources.first() else { return };
        let Some(cur) = &a.current else { return };
        let landed = a.frame_of(cur.pts);
        // Done when the requested frame is on screen (or the decoder settled elsewhere).
        if a.awaiting && Some(landed) != self.pending_step {
            return;
        }
        self.dev_burst = None;
        let mut h: u64 = 0xcbf29ce484222325;
        for b in cur.rgba.as_raw() {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        dev_log(&format!("at={:.1} steps={n} frame={landed} pts={:.4} ms={:.1} hash={h:016x}", dev_clock(), cur.pts, t0.elapsed().as_secs_f64() * 1000.0));
    }

    fn jump_keyframe(&mut self, forward: bool) {
        let a = &self.sources[0];
        let cur = a.pts();
        let t = {
            let idx = a.index.lock();
            let Some(idx) = idx.as_ref() else { return };
            if forward {
                idx.iter().find(|e| e.key && e.pts > cur + 1e-4).map(|e| e.pts)
            } else {
                idx.iter().rev().find(|e| e.key && e.pts < cur - 1e-4).map(|e| e.pts)
            }
        };
        if let Some(t) = t {
            self.seek_all(t, true);
        }
    }
}

impl Drop for VideoMode {
    fn drop(&mut self) {
        // Stop a running whole-file analysis so it doesn't keep files open.
        if let Some(a) = &self.analysis {
            a.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(a) = self.audio_shared() {
            a.playing.store(false, Ordering::Relaxed);
        }
    }
}

fn snapshot_dir_file() -> PathBuf {
    crate::settings::data_dir().join("video-snapshot-dir.txt")
}

/// Folder the last frame snapshot was saved to, if it still exists.
fn last_snapshot_dir() -> Option<PathBuf> {
    let text = std::fs::read_to_string(snapshot_dir_file()).ok()?;
    Some(PathBuf::from(text.trim())).filter(|d| d.is_dir())
}

fn remember_snapshot_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(crate::settings::data_dir());
    let _ = std::fs::write(snapshot_dir_file(), dir.to_string_lossy().as_bytes());
}

pub(super) fn dev_dir() -> Option<&'static PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("LUMEN_DEV_DIR").map(PathBuf::from)).as_ref()
}

/// Milliseconds since the first dev log call (dev automation only).
fn dev_clock() -> f64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// Append a line to `video.log` in the dev hook folder (dev automation only).
pub(super) fn dev_log(line: &str) {
    use std::io::Write;
    if let Some(d) = dev_dir() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(d.join("video.log")) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn rotate_mesh(mesh: &mut egui::Mesh, dest: Rect, rotation: i32) {
    // Rotate UVs so a rotated video (phone footage) displays upright in `dest`.
    let r = rotation.rem_euclid(360);
    if r == 0 {
        return;
    }
    for v in mesh.vertices.iter_mut() {
        let u = (v.pos.x - dest.min.x) / dest.width();
        let w = (v.pos.y - dest.min.y) / dest.height();
        v.uv = match r {
            90 => Pos2::new(w, 1.0 - u),
            180 => Pos2::new(1.0 - u, 1.0 - w),
            270 => Pos2::new(1.0 - w, u),
            _ => v.uv,
        };
    }
}

/// Media info rows. `durations` are the video streams' own lengths (as the
/// transport bar shows them: the last frame's time plus one frame); the
/// container's duration, which also covers audio and is often a few
/// milliseconds longer, gets its own row only when it differs.
fn media_rows(infos: &[&MediaInfo], durations: &[f64]) -> Vec<(&'static str, Vec<String>)> {
    let f = |g: &dyn Fn(&MediaInfo) -> String| infos.iter().map(|i| g(i)).collect::<Vec<_>>();
    let br = |b: i64| if b > 0 { format!("{:.2} Mb/s", b as f64 / 1e6) } else { " - ".into() };
    let video_dur = |k: usize| durations.get(k).copied().unwrap_or(infos[k].duration);
    let file_dur_differs = (0..infos.len()).any(|k| infos[k].duration > 0.0 && (infos[k].duration - video_dur(k)).abs() >= 0.0005);
    let mut rows = vec![
        ("File", f(&|i| crate::ui::viewer::file_name(&i.path))),
        ("Size", f(&|i| crate::catalog::human_size(i.file_size))),
        ("Container", f(&|i| i.container.clone())),
        ("Duration", (0..infos.len()).map(|k| fmt_time(video_dur(k))).collect()),
    ];
    if file_dur_differs {
        rows.push(("File duration", f(&|i| if i.duration > 0.0 { fmt_time(i.duration) } else { " - ".into() })));
    }
    rows.extend([
        ("Overall bitrate", f(&|i| br(i.bitrate))),
        ("Video codec", f(&|i| if i.profile.is_empty() { i.codec.clone() } else { format!("{} ({})", i.codec, i.profile) })),
        ("Resolution", f(&|i| format!("{} × {}", i.width, i.height))),
        ("Pixel aspect", f(&|i| if i.sar.0 > 0 && i.sar.1 > 0 { format!("{}:{}", i.sar.0, i.sar.1) } else { "1:1".into() })),
        ("Frame rate", f(&|i| format!("{:.3} fps{}", i.avg_fps, if (i.avg_fps - i.fps).abs() > 0.01 && i.fps > 0.0 { format!(" (r {:.3}, VFR?)", i.fps) } else { String::new() }))),
        ("Video bitrate", f(&|i| br(i.video_bitrate))),
        ("Pixel format", f(&|i| format!("{} · {}-bit", i.pix_fmt, i.bit_depth))),
        ("Color matrix", f(&|i| i.color_space.clone())),
        ("Range", f(&|i| i.color_range.clone())),
        ("Primaries", f(&|i| i.color_primaries.clone())),
        ("Transfer", f(&|i| format!("{}{}", i.color_transfer, if i.hdr { " · HDR (shown without tone mapping)" } else { "" }))),
        ("Rotation", f(&|i| format!("{}°", i.rotation))),
        ("Audio", f(&|i| if i.has_audio { format!("{} · {} Hz · {} ch · {}", i.audio_codec, i.sample_rate, i.channels, br(i.audio_bitrate)) } else { "none".into() })),
    ]);
    rows
}

/// Media info as a striped table: a fixed label column and equal value
/// columns (A and B), so a long value never squeezes the other column.
/// File names stay on one line, ellipsized, with the full name on hover.
fn media_table(ui: &mut Ui, p: &Palette, compare: bool, rows: Vec<(&'static str, Vec<String>)>) {
    const LABEL_W: f32 = 96.0;
    const GAP: f32 = 8.0;
    let cols = if compare { 2 } else { 1 };
    let width = ui.available_width();
    let value_w = ((width - LABEL_W - GAP * cols as f32) / cols as f32).max(40.0);
    let cell = |ui: &mut Ui, w: f32, add: &mut dyn FnMut(&mut Ui)| {
        ui.allocate_ui_with_layout(Vec2::new(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(w);
            add(ui);
        });
    };
    let row = |ui: &mut Ui, stripe: bool, add: &mut dyn FnMut(&mut Ui)| {
        let bg = ui.painter().add(egui::Shape::Noop);
        let r = ui
            .horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                add(ui);
            })
            .response
            .rect;
        if stripe {
            ui.painter().set(bg, egui::Shape::rect_filled(r.expand2(Vec2::new(4.0, 2.0)), CornerRadius::same(3), ui.visuals().faint_bg_color));
        }
    };
    ui.spacing_mut().item_spacing.y = 4.0;
    if compare {
        row(ui, false, &mut |ui| {
            cell(ui, LABEL_W, &mut |_| {});
            cell(ui, value_w, &mut |ui| {
                ui.label(RichText::new("A").strong().color(ab_color(p, 0)));
            });
            cell(ui, value_w, &mut |ui| {
                ui.label(RichText::new("B").strong().color(ab_color(p, 1)));
            });
        });
    }
    for (n, (k, vals)) in rows.into_iter().enumerate() {
        let mismatch = compare && vals.len() == 2 && vals[0] != vals[1];
        let one_line = k == "File";
        row(ui, n % 2 == 0, &mut |ui| {
            cell(ui, LABEL_W, &mut |ui| {
                ui.add(egui::Label::new(RichText::new(k).size(11.5).color(if mismatch { p.warn } else { p.muted })).wrap());
            });
            for v in &vals {
                cell(ui, value_w, &mut |ui| {
                    let text = RichText::new(v.as_str()).size(11.5).color(if mismatch { p.warn } else { p.text });
                    // Truncated labels show the full text as a tooltip.
                    let label = if one_line { egui::Label::new(text).truncate() } else { egui::Label::new(text).wrap() };
                    ui.add(label);
                });
            }
        });
    }
}

/// Timeline bar tints for I, P, B and not yet known frame types: one neutral
/// color, keyframes lighter (the frame types themselves are marked below the bars).
fn timeline_tints(p: &Palette) -> [Color32; 4] {
    [p.muted.gamma_multiply(0.6), p.muted.gamma_multiply(0.42), p.muted.gamma_multiply(0.34), p.muted.gamma_multiply(0.42)]
}

/// The PSNR-Y graph over the timeline.
const PSNR_COLOR: Color32 = Color32::from_rgb(250, 210, 80);

/// One pixel column of the frame-size timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct TimelineColumn {
    /// Mean frame size in the column relative to the 98th percentile (0..=1).
    height: f32,
    /// Share of the column's bytes in I, P, B and unknown frames.
    shares: [f32; 4],
    /// The column holds a keyframe (an I tick in the type strip).
    key: bool,
}

/// Aggregate the frame index into `cols` columns. Heights follow the mean
/// frame size (so one large I-frame does not turn a column of a long film
/// into a full red bar) and each bar is split by the bytes of each type.
fn timeline_columns(idx: &[IndexEntry], dur: f64, cols: usize) -> Vec<TimelineColumn> {
    let cols = cols.max(1);
    let mut sums = vec![([0u64; 4], 0u32); cols];
    let mut keys = vec![false; cols];
    for e in idx {
        let c = (((e.pts / dur.max(1e-9)) * cols as f64).max(0.0) as usize).min(cols - 1);
        let k = match e.kind {
            b'I' => 0,
            b'P' => 1,
            b'B' => 2,
            _ if e.key => 0,
            _ => 3,
        };
        sums[c].0[k] += e.size as u64;
        sums[c].1 += 1;
        keys[c] |= e.key || e.kind == b'I';
    }
    let means: Vec<f64> = sums.iter().map(|(b, n)| if *n == 0 { 0.0 } else { b.iter().sum::<u64>() as f64 / *n as f64 }).collect();
    let mut sorted: Vec<f64> = means.iter().copied().filter(|m| *m > 0.0).collect();
    let p98 = if sorted.is_empty() {
        1.0
    } else {
        let k = (sorted.len() * 98 / 100).min(sorted.len() - 1);
        *sorted.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
    };
    sums.iter()
        .zip(&means)
        .zip(keys)
        .map(|(((bytes, _), mean), key)| {
            let total = bytes.iter().sum::<u64>().max(1) as f32;
            TimelineColumn { height: (mean / p98.max(1.0)).min(1.0) as f32, shares: bytes.map(|b| b as f32 / total), key }
        })
        .collect()
}

fn kind_name(k: char) -> &'static str {
    match k {
        'I' => "I-frame",
        'P' => "P-frame",
        'B' => "B-frame",
        _ => "frame",
    }
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let s = t % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:06.3}")
    } else {
        format!("{m:02}:{s:06.3}")
    }
}

/// Seconds between timeline ticks: the smallest round step that keeps ticks
/// at least `min_px` apart on a `width` pixel timeline of `dur` seconds.
fn tick_step(dur: f64, width: f32, min_px: f32) -> f64 {
    const STEPS: [f64; 22] = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0];
    let per_px = dur.max(1e-6) / width.max(1.0) as f64;
    STEPS.into_iter().find(|s| s / per_px >= min_px as f64).unwrap_or(STEPS[STEPS.len() - 1])
}

/// Timeline tick label: as many decimals as the step needs, hours only on
/// videos of an hour or more.
fn fmt_tick(t: f64, step: f64, dur: f64) -> String {
    let decimals = if step >= 1.0 { 0 } else if step >= 0.1 { 1 } else { 2 };
    let scale = 10f64.powi(decimals);
    let t = (t * scale).round() / scale;
    let (h, m, s) = ((t / 3600.0) as u64, ((t % 3600.0) / 60.0) as u64, t % 60.0);
    let w = if decimals == 0 { 2 } else { 3 + decimals as usize };
    let sec = format!("{s:0w$.prec$}", prec = decimals as usize);
    if dur >= 3600.0 {
        format!("{h}:{m:02}:{sec}")
    } else {
        format!("{m}:{sec}")
    }
}

/// PSNR for display. Identical planes (infinite PSNR, or a mean of values
/// all at the cap, see `metrics::PSNR_CAP_DB`) read as "Identical".
fn fmt_db(v: f64) -> String {
    if v.is_nan() {
        " - ".into()
    } else if v >= metrics::PSNR_CAP_DB {
        "Identical".into()
    } else {
        format!("{v:.3} dB")
    }
}

/// SSIM for display, with its dB equivalent unless the frames are identical.
fn fmt_ssim(v: f64) -> String {
    let db = metrics::ssim_db(v);
    if v.is_nan() {
        " - ".into()
    } else if db >= metrics::PSNR_CAP_DB {
        format!("{v:.4} (identical)")
    } else {
        format!("{v:.4} ({db:.2} dB)")
    }
}

fn parse_goto(s: &str, src: &Source) -> Option<f64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let cur = src.pts();
    let (rel, body) = if let Some(r) = s.strip_prefix('+') {
        (1.0, r)
    } else if let Some(r) = s.strip_prefix('-') {
        (-1.0, r)
    } else {
        (0.0, s.as_str())
    };
    let secs = if let Some(f) = body.strip_suffix('f') {
        let n: f64 = f.trim().parse().ok()?;
        if rel != 0.0 {
            return Some(src.pts_of((src.frame_of(cur) as f64 + rel * n).max(0.0) as usize));
        }
        return Some(src.pts_of(n as usize));
    } else if let Some(x) = body.strip_suffix('s') {
        x.trim().parse::<f64>().ok()?
    } else if body.contains(':') {
        let mut total = 0.0;
        for part in body.split(':') {
            total = total * 60.0 + part.parse::<f64>().ok()?;
        }
        total
    } else {
        // Bare number = frame number.
        let n: f64 = body.parse().ok()?;
        if rel != 0.0 {
            return Some(src.pts_of((src.frame_of(cur) as f64 + rel * n).max(0.0) as usize));
        }
        return Some(src.pts_of(n as usize));
    };
    Some(if rel != 0.0 { cur + rel * secs } else { secs })
}

#[allow(dead_code)]
fn _yuv(_: &YuvFrame) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_names_keep_start_and_extension() {
        assert_eq!(shorten("clip.mp4", 20), "clip.mp4");
        let s = shorten("a_very_long_recording_name_2026.mp4", 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.starts_with("a_very_long") && s.ends_with("26.mp4"), "{s}");
    }

    #[test]
    fn timeline_heights_ignore_lone_iframes() {
        // A long film: 10 frames per column, one big keyframe in every column.
        let mut idx = Vec::new();
        for i in 0..1000 {
            let key = i % 10 == 0;
            // Mid-frame times keep the binning clear of rounding at column edges.
            idx.push(IndexEntry { pts: (i as f64 + 0.5) / 25.0, size: if key { 100_000 } else { 10_000 }, key, kind: if key { b'I' } else { b'?' } });
        }
        let cols = timeline_columns(&idx, 40.0, 100);
        assert_eq!(cols.len(), 100);
        for c in &cols {
            assert!((c.height - 1.0).abs() < 1e-6);
            // The keyframe is about half of the bytes, not the whole bar.
            assert!((c.shares[0] - 100.0 / 190.0).abs() < 1e-4, "{:?}", c.shares);
            assert!((c.shares[3] - 90.0 / 190.0).abs() < 1e-4);
        }
        // Few frames: one per column, colored by its own type.
        let small = [IndexEntry { pts: 0.0, size: 50, key: true, kind: b'I' }, IndexEntry { pts: 1.0, size: 10, key: false, kind: b'B' }];
        let cols = timeline_columns(&small, 2.0, 2);
        assert_eq!(cols[0].shares, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(cols[1].shares, [0.0, 0.0, 1.0, 0.0]);
        assert!(cols[1].height < cols[0].height);
        // The type strip marks the keyframe's column only.
        assert!(cols[0].key && !cols[1].key);
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(62.5), "01:02.500");
        assert_eq!(fmt_time(3723.25), "1:02:03.250");
    }

    #[test]
    fn timeline_ticks() {
        // A feature film on a wide timeline: five-minute ticks, with hours.
        let film = 6324.8;
        let step = tick_step(film, 1900.0, 84.0);
        assert_eq!(step, 300.0);
        assert_eq!(fmt_tick(3000.0, step, film), "0:50:00");
        // Short clips get finer steps and decimals when needed.
        assert_eq!(tick_step(30.0, 1900.0, 84.0), 2.0);
        assert_eq!(fmt_tick(62.0, 2.0, 90.0), "1:02");
        assert_eq!(tick_step(1.5, 1200.0, 84.0), 0.2);
        assert_eq!(fmt_tick(0.4, 0.2, 1.5), "0:00.4");
        assert_eq!(fmt_tick(0.05, 0.05, 0.5), "0:00.05");
        // Ticks never crowd, however narrow the timeline.
        for (dur, w) in [(10.0, 300.0), (600.0, 500.0), (86_400.0, 400.0), (0.2, 900.0)] {
            let s = tick_step(dur, w, 84.0);
            assert!(s / dur * w as f64 >= 84.0 || s == 21600.0, "{dur} {w} {s}");
        }
    }

    #[test]
    fn metric_format() {
        assert_eq!(fmt_db(f64::INFINITY), "Identical");
        assert_eq!(fmt_db(metrics::PSNR_CAP_DB), "Identical");
        assert_eq!(fmt_db(42.1234), "42.123 dB");
        assert_eq!(fmt_db(f64::NAN), " - ");
        assert_eq!(fmt_ssim(1.0), "1.0000 (identical)");
        assert_eq!(fmt_ssim(0.99), "0.9900 (20.00 dB)");
    }

    #[test]
    fn duration_rows() {
        let info = MediaInfo { duration: 6324.832, ..Default::default() };
        let rows = media_rows(&[&info], &[6324.818]);
        let get = |name: &str| rows.iter().find(|r| r.0 == name).map(|r| r.1.clone());
        // The transport bar's duration first, the container's next to it.
        assert_eq!(get("Duration"), Some(vec!["1:45:24.818".to_string()]));
        assert_eq!(get("File duration"), Some(vec!["1:45:24.832".to_string()]));
        let rows = media_rows(&[&info], &[6324.832]);
        assert!(!rows.iter().any(|r| r.0 == "File duration"));
    }
}
