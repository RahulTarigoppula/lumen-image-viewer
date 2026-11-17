//! Video mode UI: playback, frame-accurate stepping, compare of up to four
//! videos (side by side, wipe, difference) with start points, metrics,
//! scopes, frame index timeline and media info.

use super::align::{self, AlignJob, Outcome};
use super::audio::AudioOut;
use super::engine::{self, Cmd, Event, IndexEntry, MediaInfo, Player, VFrame};
use super::metrics::{self, FrameMetrics};
use super::scopes::{self, HistMode, Scope};
use super::sync::{self, Loop, View, LETTERS, MAX_VIDEOS};
use super::timealign::Confidence;
use super::timing::{self, IssueKind, TimingReport};
use crate::app::Core;
use crate::ui::commands::Command;
use crate::ui::icons as I;
use crate::ui::theme::{self, size, Palette};
use crate::ui::widgets;
use crossbeam_channel::{Receiver, TryRecvError};
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
    Difference,
}

impl CompareMode {
    const ALL: [CompareMode; 3] = [CompareMode::SideBySide, CompareMode::Wipe, CompareMode::Difference];
    fn label(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "Side by side",
            CompareMode::Wipe => "Wipe",
            CompareMode::Difference => "Difference",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            CompareMode::SideBySide => I::COLUMNS,
            CompareMode::Wipe => I::SPLIT_HORIZONTAL,
            CompareMode::Difference => I::MINUS_CIRCLE,
        }
    }
    fn key(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "S",
            CompareMode::Wipe => "W",
            CompareMode::Difference => "D",
        }
    }
    fn tip(self) -> &'static str {
        match self {
            CompareMode::SideBySide => "Side by side: every video in its own pane, showing the same part of the picture; zoom and pan move all of them (S)",
            CompareMode::Wipe => "Wipe: A left and B right of a divider that follows the mouse; F freezes it, Shift+drag moves it (W, two videos)",
            CompareMode::Difference => "Difference: black where A and B match, bright where they differ; Gain makes small differences visible (D, two videos)",
        }
    }
}

/// How the videos move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SyncMode {
    /// All play, step and seek together, kept apart by their start points.
    Linked,
    /// Only the selected video moves (to find each video's start point).
    Independent,
}

/// A result computed off the UI thread, one job at a time: a newer request
/// waits until the running job is done, which also throttles the work.
struct Bg<K, T> {
    key: Option<K>,
    running: Option<(K, Receiver<T>)>,
    started: Option<Instant>,
}

impl<K: PartialEq + Clone + Send + 'static, T: Send + 'static> Bg<K, T> {
    fn new() -> Self {
        Bg { key: None, running: None, started: None }
    }

    /// A finished result, once.
    fn poll(&mut self) -> Option<T> {
        let (k, rx) = self.running.as_ref()?;
        match rx.try_recv() {
            Ok(v) => {
                self.key = Some(k.clone());
                self.running = None;
                Some(v)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.running = None;
                None
            }
        }
    }

    /// Start `job` for `key` unless that result is shown or being made, a job
    /// is running, or the last one started less than `gap` ago.
    fn request(&mut self, key: K, gap: Duration, ctx: &egui::Context, job: impl FnOnce() -> T + Send + 'static) {
        if self.key.as_ref() == Some(&key) || self.running.is_some() {
            return;
        }
        if let Some(t) = self.started {
            if t.elapsed() < gap {
                ctx.request_repaint_after(gap.saturating_sub(t.elapsed()));
                return;
            }
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.running = Some((key, rx));
        self.started = Some(Instant::now());
    }

    fn reset(&mut self) {
        self.key = None;
    }
}

/// (frame pts, frame pixels, scope, histogram mode): what a scope image shows.
type ScopeKey = (u64, usize, u8, u8);

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
    /// Frame timing checks, made from the index once it is built.
    timing: Arc<Mutex<Option<Arc<TimingReport>>>>,
    eof: bool,
    out_size: (u32, u32),
    /// A new decode size waiting for the window to settle.
    out_wait: sync::Debounce<(u32, u32)>,
    /// The decoder copies native planes (for metrics).
    native_on: bool,
    error: Option<String>,
    /// A seek/step result is expected; when paused, only that frame becomes
    /// current (frames decoded ahead stay queued for stepping).
    awaiting: bool,
    /// Mirrors the mode's playing state (set every frame by `poll`).
    playing_hint: bool,
    /// When the pending seek was sent (drives the "Seeking" badge).
    seek_started: Option<Instant>,
    /// The start point: where the event being compared begins in this video.
    start: Option<f64>,
    scope_job: Bg<ScopeKey, egui::ColorImage>,
    scope_tex: Option<egui::TextureHandle>,
}

impl Source {
    fn open(path: &Path, audio: Option<Arc<super::audio::AudioShared>>, ctx: &egui::Context) -> anyhow::Result<Source> {
        let info = engine::probe(path)?;
        let index: engine::SharedIndex = Arc::new(Mutex::new(None));
        let player = Player::open(path, audio, index.clone(), ctx.clone())?;
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let timing_slot: Arc<Mutex<Option<Arc<TimingReport>>>> = Arc::new(Mutex::new(None));
        {
            let (index, progress, cancel, path, ctx, timing_slot) = (index.clone(), progress.clone(), cancel.clone(), path.to_path_buf(), ctx.clone(), timing_slot.clone());
            // The rate the file states: r_frame_rate unless it is a time base
            // in disguise (90000), else the average.
            let declared = if info.fps > 0.0 && info.fps <= 1000.0 { info.fps } else { info.avg_fps };
            std::thread::spawn(move || {
                let p2 = progress.clone();
                let res = engine::build_index(&path, &cancel, &|f| p2.store(f.to_bits(), Ordering::Relaxed));
                if let Ok(v) = res {
                    let stamps: Vec<timing::FrameStamp> = v.iter().map(|e| timing::FrameStamp { pts: e.pts, size: e.size, key: e.key, repeats: e.repeats as u32 }).collect();
                    *index.lock() = Some(v);
                    *timing_slot.lock() = Some(Arc::new(timing::analyze(&stamps, declared)));
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
            timing: timing_slot,
            eof: false,
            out_size: out,
            out_wait: sync::Debounce::default(),
            native_on: false,
            error: None,
            awaiting: true,
            playing_hint: false,
            seek_started: None,
            start: None,
            scope_job: Bg::new(),
            scope_tex: None,
        })
    }

    fn name(&self) -> String {
        crate::ui::viewer::file_name(&self.path)
    }

    /// The frame timing checks (once the index is built).
    fn timing(&self) -> Option<Arc<TimingReport>> {
        self.timing.lock().clone()
    }

    /// The frame index is being built.
    fn indexing(&self) -> bool {
        f32::from_bits(self.index_progress.load(Ordering::Relaxed)) < 1.0
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
        self.info.duration.max(0.0)
    }

    /// The last moment a seek can target.
    fn last_pts(&self) -> f64 {
        (self.duration() - 0.001).max(0.0)
    }

    fn frame_count(&self) -> Option<usize> {
        self.index.lock().as_ref().map(|v| v.len())
    }

    /// The video has played to its end.
    fn at_end(&self) -> bool {
        self.eof && self.queue.is_empty()
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
        let f = (pts * self.fps()).round();
        if f.is_finite() && f > 0.0 {
            f as usize
        } else {
            0
        }
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
        let t = if t.is_finite() { t.clamp(0.0, self.last_pts()) } else { 0.0 };
        self.gen += 1;
        self.queue.clear();
        self.eof = false;
        self.awaiting = true;
        self.seek_started = Some(Instant::now());
        self.player.send(Cmd::Seek { t, exact, gen: self.gen });
    }

    fn set_native(&mut self, on: bool) {
        if self.native_on != on {
            self.native_on = on;
            self.player.send(Cmd::Native(on));
        }
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
        let cur = self.pts();
        while self.history.len() > cap {
            // Drop the frame farthest from the current position.
            let (Some(front), Some(back)) = (self.history.front(), self.history.back()) else { break };
            if (front.pts - cur).abs() > (back.pts - cur).abs() {
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
        if playing && self.history.len() > 16 {
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
                Event::Error(e) => {
                    self.error = Some(e);
                    self.awaiting = false;
                    self.seek_started = None;
                }
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

/// |A - B| per channel times `gain`, with the mean and largest difference.
fn diff_image(ia: &RgbaImage, ib: &RgbaImage, gain: f32) -> (egui::ColorImage, f64, f64) {
    use rayon::prelude::*;
    let (w, h) = (ia.width().min(ib.width()).max(1), ia.height().min(ib.height()).max(1));
    if ia.width() == 0 || ib.width() == 0 || ia.height() == 0 || ib.height() == 0 {
        return (egui::ColorImage::new([1, 1], vec![Color32::BLACK]), 0.0, 0.0);
    }
    let rows: Vec<(Vec<Color32>, f64, u8)> = (0..h)
        .into_par_iter()
        .map(|y| {
            let mut out = Vec::with_capacity(w as usize);
            let (mut sum, mut max) = (0f64, 0u8);
            for x in 0..w {
                let pa = ia.get_pixel(x, y).0;
                let pb = ib.get_pixel(x, y).0;
                let d = [pa[0].abs_diff(pb[0]), pa[1].abs_diff(pb[1]), pa[2].abs_diff(pb[2])];
                max = max.max(d[0].max(d[1]).max(d[2]));
                sum += (d[0] as f64 + d[1] as f64 + d[2] as f64) / 3.0;
                let amp = |v: u8| ((v as f32 * gain).min(255.0)) as u8;
                out.push(Color32::from_rgb(amp(d[0]), amp(d[1]), amp(d[2])));
            }
            (out, sum, max)
        })
        .collect();
    let mut px = Vec::with_capacity((w * h) as usize);
    let (mut sum, mut max) = (0f64, 0u8);
    for (r, s, m) in rows {
        px.extend(r);
        sum += s;
        max = max.max(m);
    }
    (egui::ColorImage::new([w as usize, h as usize], px), sum / (w as f64 * h as f64), max as f64)
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

/// The last auto-align: what it found, and the start points before and
/// after it (Undo puts the old ones back while the new ones are unchanged).
struct AlignResult {
    paths: Vec<PathBuf>,
    outcomes: Vec<Outcome>,
    before: Vec<Option<f64>>,
    before_sync: SyncMode,
    after: Vec<Option<f64>>,
    /// At least one start point was set.
    applied: bool,
}

/// An auto-align result is used (its start point set) from this confidence on.
const ALIGN_MIN: Confidence = Confidence::Low;

const ALIGN_TIP: &str = "Auto-align start points: compare how the picture changes over the first 45 seconds of every video (motion, lights, cuts) to find how far apart they started, then set the start points so they play in step. Works across phones with different exposure and frame rates; Undo in the Info panel";

/// What to call a video whose start point moved by `off` seconds against A.
fn fmt_offset(off: f64) -> String {
    if off.abs() < 5e-4 {
        "with A".into()
    } else {
        format!("A {} {:.3} s", if off > 0.0 { "+" } else { "-" }, off.abs())
    }
}

/// One auto-align outcome in words, and whether it is trustworthy.
fn outcome_text(o: &Outcome) -> (String, bool) {
    match o {
        Outcome::Reference => ("reference".into(), true),
        Outcome::Found(a) if a.level() >= ALIGN_MIN => (format!("{}, {}", fmt_offset(a.offset), a.level().label()), a.level() > Confidence::Low),
        Outcome::Found(a) => (format!("no match (best guess {}), not set", fmt_offset(a.offset)), false),
        Outcome::NoSignal => ("too little changes in the picture, not set".into(), false),
        Outcome::Failed(e) => (format!("could not be read ({e}), not set"), false),
    }
}

/// What a frame save job does with the decoded frames.
#[derive(Clone)]
enum SaveTarget {
    /// PNG files in a folder, with a side-by-side picture when `composite`.
    Folder { dir: PathBuf, composite: bool },
    /// The side-by-side picture (or the one frame) to the clipboard.
    Clipboard,
    /// PNG files in a review folder, then open them in image Compare.
    Compare(PathBuf),
}

/// A finished frame save job.
struct SaveDone {
    message: String,
    /// Frames to open in Compare.
    open: Option<Vec<PathBuf>>,
}

/// The Save frames dialog.
struct SaveDialog {
    dir: PathBuf,
    composite: bool,
}

/// Radius of the letter badges on the picture (20 pt circles).
const BADGE_R: f32 = 10.0;

/// Gap between panes (points).
const PANE_GAP: f32 = 2.0;

/// Height of a pane's caption.
const CAP_H: f32 = 28.0;

/// A-D colors, the same as Compare's badges (so image and video compare match).
fn letter_color(p: &Palette, i: usize) -> Color32 {
    [p.accent, Color32::from_rgb(232, 120, 60), Color32::from_rgb(80, 180, 120), Color32::from_rgb(170, 110, 230)][i % MAX_VIDEOS]
}

fn letter(i: usize) -> &'static str {
    LETTERS[i.min(MAX_VIDEOS - 1)]
}

/// A round letter badge: the letter in white on its color, with a soft dark
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

/// Video mode's keys, for the keys menu in the top bar.
const VIDEO_KEYS: [(&str, &str); 27] = [
    ("Space / K", "Play or pause"),
    (", / .", "Previous / next frame (the selected video when Independent)"),
    ("← / →", "Previous / next frame while paused, 1 s while playing"),
    ("Shift+← / →", "Back / forward 1 s"),
    ("Ctrl+← / →", "Back / forward 10 s"),
    ("↓ / ↑", "Back / forward 1 min"),
    ("Home / End", "First / last frame"),
    ("PgUp / PgDn", "Previous / next keyframe"),
    ("G", "Go to a frame or time"),
    ("[ / ]", "Slower / faster (Backspace: normal speed)"),
    ("M", "Mute"),
    ("Ctrl+L", "Repeat a section: start, end, clear"),
    ("L", "Linked / Independent"),
    ("Tab", "Select the next video (Independent)"),
    ("P", "Set start point (selected video; all when Linked)"),
    ("Shift+Home", "Go to the start points"),
    ("S / W / D", "Side by side / Wipe / Difference"),
    ("X", "Swap A and B (two videos)"),
    ("F", "Freeze the wipe divider"),
    ("Ctrl+S", "Save the frames of all videos"),
    ("Ctrl+C", "Copy the frames side by side"),
    ("0 / 1", "Fit / 100% (double-click toggles)"),
    ("Wheel / drag", "Zoom at the mouse / pan"),
    ("F2", "Scopes"),
    ("I", "Info and metrics"),
    ("Ctrl+Q", "Analysis panel"),
    ("Esc", "Back to the browser"),
];

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

/// A small flat icon button drawn at `rect` (pane captions).
fn cap_button(ui: &mut Ui, rect: Rect, id: egui::Id, icon: &str, tip: &str, on: bool, p: &Palette) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::click()).on_hover_text(tip);
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let color = if on { p.accent } else if resp.hovered() { p.text } else { p.muted };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), color);
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
    /// The video whose sound plays.
    audio_src: usize,
    playing: bool,
    clock: Clock,
    speed: f64,
    view: View,
    mode: CompareMode,
    sync: SyncMode,
    /// The video Independent mode moves (and the timeline shows).
    selected: usize,
    gain: f32,
    wipe_pos: f32,
    wipe_frozen: bool,
    show_info: bool,
    show_scopes: bool,
    scope: Scope,
    hist_mode: HistMode,
    diff: Bg<(u64, u64, u32, (u32, u32)), (egui::ColorImage, f64, f64)>,
    diff_tex: Option<egui::TextureHandle>,
    diff_stats: Option<(f64, f64)>,
    metrics_now: Option<((u64, u64), FrameMetrics)>,
    metrics_rx: Option<crossbeam_channel::Receiver<((u64, u64), Option<FrameMetrics>)>>,
    analysis: Option<Analysis>,
    loop_ab: Loop,
    /// The video whose time the loop is in.
    loop_lead: usize,
    goto: Option<String>,
    /// The hovered point as a fraction of the frame.
    pixel: Option<(f32, f32)>,
    volume: f32,
    muted: bool,
    scrubbing: bool,
    hover_view: bool,
    /// The last video that failed to open: (file name, short reason).
    open_error: Option<(String, String)>,
    /// "Close video" in the picture's menu: leave video mode this frame.
    close_requested: bool,
    snap_rx: Option<Receiver<Result<SaveDone, String>>>,
    save_dialog: Option<SaveDialog>,
    /// Frames saved for image Compare, for the app to open.
    compare_request: Option<Vec<PathBuf>>,
    /// (index entries, columns, per-column bars from `timeline_columns`, built at)
    timeline_cache: Option<(usize, usize, Vec<TimelineColumn>, Instant)>,
    /// Frame number most recently requested by stepping (until it arrives).
    pending_step: Option<usize>,
    /// A text field had keyboard focus at the end of the last frame. egui drops
    /// focus on Esc before this frame's widgets run, so Esc that only leaves a
    /// field must not also leave video mode.
    was_editing: bool,
    /// Dev hook only: start time and press count of the current step burst.
    dev_burst: Option<(Instant, u32)>,
    /// A alone decodes native planes for the Analysis panel's Metrics tab.
    metrics_native: bool,
    /// A's pane in physical pixels (last frame), for 100% from the keyboard.
    pane_px: Vec2,
    /// The audio clock's last value and when it changed (a stalled clock
    /// is ignored).
    audio_seen: Option<(f64, Instant)>,
    /// Auto-align of the start points while it runs, then what it found.
    align_job: Option<AlignJob>,
    align_result: Option<AlignResult>,
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
            audio_src: 0,
            playing: false,
            clock: Clock { base_pts: 0.0, started: Instant::now(), running: false, speed: 1.0 },
            speed: 1.0,
            view: View::default(),
            mode: CompareMode::SideBySide,
            sync: SyncMode::Linked,
            selected: 0,
            gain: 4.0,
            wipe_pos: 0.5,
            wipe_frozen: false,
            show_info: false,
            show_scopes: false,
            scope: Scope::Waveform,
            hist_mode: HistMode::Both,
            diff: Bg::new(),
            diff_tex: None,
            diff_stats: None,
            metrics_now: None,
            metrics_rx: None,
            analysis: None,
            loop_ab: Loop::default(),
            loop_lead: 0,
            goto: None,
            pixel: None,
            volume: 1.0,
            muted: false,
            scrubbing: false,
            hover_view: false,
            open_error: None,
            close_requested: false,
            snap_rx: None,
            save_dialog: None,
            compare_request: None,
            timeline_cache: None,
            pending_step: None,
            was_editing: false,
            dev_burst: None,
            metrics_native: false,
            pane_px: Vec2::new(1280.0, 720.0),
            audio_seen: None,
            align_job: None,
            align_result: None,
        };
        for p in paths.into_iter().take(MAX_VIDEOS) {
            vm.add_source(&p, &core.ctx);
        }
        vm
    }

    /// Frames saved by "Open frames in Compare", for the app to open in
    /// image Compare (once).
    pub fn take_compare_request(&mut self) -> Option<Vec<PathBuf>> {
        self.compare_request.take()
    }

    /// Add `path` as the next video (A, B, C, D). A failure is kept in
    /// `open_error` for the empty view's card (or a toast, see `ui`).
    fn add_source(&mut self, path: &Path, ctx: &egui::Context) -> bool {
        if self.sources.len() >= MAX_VIDEOS {
            return false;
        }
        let first = self.sources.is_empty();
        let audio = if first { self.audio_shared() } else { None };
        match Source::open(path, audio, ctx) {
            Ok(mut s) => {
                self.open_error = None;
                if first {
                    self.audio_src = 0;
                } else {
                    // A new video joins at A's moment (it has no start point yet).
                    let t = self.sources[0].pts();
                    s.seek(t, true);
                    if self.playing {
                        self.pause();
                    }
                }
                self.sources.push(s);
                if self.sources.len() != 2 {
                    self.mode = CompareMode::SideBySide;
                }
                self.view = View::default();
                self.update_native();
                true
            }
            Err(e) => {
                let name = crate::ui::viewer::file_name(path);
                self.open_error = Some((if name.is_empty() { path.display().to_string() } else { name }, open_reason(&e)));
                false
            }
        }
    }

    /// Add videos from a file dialog, up to four in all.
    fn pick_videos(&mut self, ctx: &egui::Context) {
        let exts: Vec<&str> = super::VIDEO_EXTS.to_vec();
        if let Some(files) = rfd::FileDialog::new().add_filter("Videos", &exts).add_filter("All files", &["*"]).pick_files() {
            self.add_paths(files, ctx);
        }
    }

    /// Add `paths` to the open videos; more than fit replace them.
    fn add_paths(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        if paths.is_empty() {
            return;
        }
        if self.sources.len() + paths.len() > MAX_VIDEOS {
            self.clear_sources();
        }
        for p in paths.into_iter().take(MAX_VIDEOS) {
            self.add_source(&p, ctx);
        }
    }

    fn clear_sources(&mut self) {
        self.pause();
        self.sources.clear();
        self.audio_src = 0;
        self.selected = 0;
        self.loop_ab = Loop::default();
        self.analysis = None;
        self.metrics_now = None;
        self.mode = CompareMode::SideBySide;
        if let Some(a) = self.audio_shared() {
            a.flush();
        }
    }

    /// Close video `i`; the others keep their letters' order.
    fn remove_source(&mut self, i: usize) {
        if i >= self.sources.len() {
            return;
        }
        self.pause();
        self.sources.remove(i);
        let n = self.sources.len();
        if i == self.audio_src {
            if let Some(a) = self.audio_shared() {
                a.flush();
            }
            self.audio_src = 0;
            if n > 0 {
                if let Some(a) = self.audio_shared() {
                    self.sources[0].player.send(Cmd::Audio(Some(a)));
                    let t = self.sources[0].pts();
                    self.sources[0].seek(t, true);
                }
            }
        } else if i < self.audio_src {
            self.audio_src -= 1;
        }
        if self.selected > i || self.selected >= n {
            self.selected = self.selected.saturating_sub(1);
        }
        if self.loop_lead == i {
            self.loop_ab = Loop::default();
            self.loop_lead = 0;
        } else if self.loop_lead > i {
            self.loop_lead -= 1;
        }
        if i < 2 {
            self.analysis = None;
            self.metrics_now = None;
        }
        if n != 2 {
            self.mode = CompareMode::SideBySide;
        }
        if n < 2 {
            self.sync = SyncMode::Linked;
        }
        self.diff.reset();
        self.update_native();
    }

    /// Native planes for metrics: A and B when comparing, A alone while the
    /// Metrics tab is open.
    fn update_native(&mut self) {
        let n = self.sources.len();
        let single = self.metrics_native;
        for (i, s) in self.sources.iter_mut().enumerate() {
            s.set_native(if n >= 2 { i < 2 } else { single });
        }
    }

    /// Play the sound of video `k`.
    fn set_audio_source(&mut self, k: usize) {
        if k == self.audio_src || k >= self.sources.len() {
            return;
        }
        let Some(a) = self.audio_shared() else { return };
        let was = self.playing;
        if was {
            self.pause();
        }
        a.flush();
        if let Some(old) = self.sources.get(self.audio_src) {
            old.player.send(Cmd::Audio(None));
        }
        self.audio_src = k;
        let s = &mut self.sources[k];
        s.player.send(Cmd::Audio(Some(a)));
        // Decode its current frame again so the sound starts there.
        let t = s.pts();
        s.seek(t, true);
        if was {
            self.play();
        }
    }

    fn audio_shared(&self) -> Option<Arc<super::audio::AudioShared>> {
        self.audio.as_ref().map(|a| a.shared.clone())
    }

    // ---------------------------------------------------------------- sync

    /// The video whose time drives playback, the timeline and stepping.
    fn lead(&self) -> usize {
        if self.sync == SyncMode::Linked || self.sources.len() <= 1 {
            0
        } else {
            self.selected.min(self.sources.len() - 1)
        }
    }

    /// The videos that move: all when linked, else the selected one.
    fn members(&self) -> Vec<usize> {
        if self.sync == SyncMode::Linked {
            (0..self.sources.len()).collect()
        } else {
            vec![self.lead()]
        }
    }

    fn starts(&self) -> Vec<Option<f64>> {
        self.sources.iter().map(|s| s.start).collect()
    }

    /// Seconds each video runs ahead of A when linked.
    fn offsets(&self) -> Vec<f64> {
        sync::offsets(&self.starts())
    }

    /// Where video `i` should be when the lead is at `t`.
    fn target_of(&self, t: f64, i: usize) -> f64 {
        if self.sync == SyncMode::Linked {
            sync::target(t, self.lead(), i, &self.offsets())
        } else {
            t
        }
    }

    /// B's offset in B frames (whole-file analysis, Metrics tab).
    fn offset_frames_b(&self) -> i64 {
        match self.sources.get(1) {
            Some(b) => sync::offset_frames(self.offsets().get(1).copied().unwrap_or(0.0), b.fps()),
            None => 0,
        }
    }

    fn any_start(&self) -> bool {
        self.sources.iter().any(|s| s.start.is_some())
    }

    fn set_sync(&mut self, sync: SyncMode, core: &mut Core) {
        if sync == self.sync || self.sources.len() < 2 {
            return;
        }
        self.pause();
        self.pending_step = None;
        match sync {
            SyncMode::Linked => {
                // A video without a start point starts at the frame it shows,
                // so the alignment just made is kept.
                for s in &mut self.sources {
                    if s.start.is_none() {
                        s.start = Some(s.pts());
                    }
                }
                let sel = self.selected.min(self.sources.len() - 1);
                let t_sel = self.sources[sel].pts();
                self.sync = SyncMode::Linked;
                // The selected video stays; the others line up with it.
                let t = sync::target(t_sel, sel, 0, &self.offsets());
                self.seek_all(t, true);
                core.toasts.info("Linked: the videos play together from their start points");
            }
            SyncMode::Independent => {
                self.sync = SyncMode::Independent;
                core.toasts.info(format!("Independent: click a video, then step it with , and . (moving {} now)", letter(self.lead())));
            }
        }
    }

    fn toggle_sync(&mut self, core: &mut Core) {
        let next = if self.sync == SyncMode::Linked { SyncMode::Independent } else { SyncMode::Linked };
        self.set_sync(next, core);
    }

    /// Select video `i` (Independent mode moves it, the timeline shows it).
    fn select(&mut self, i: usize) {
        if i < self.sources.len() && i != self.selected {
            self.selected = i;
            self.pending_step = None;
            if self.sync == SyncMode::Independent {
                self.pause();
                self.clock.base_pts = self.sources[i].pts();
            }
        }
    }

    /// Start point of video `i` at its current frame.
    fn set_start(&mut self, i: usize, core: &mut Core) {
        let Some(s) = self.sources.get_mut(i) else { return };
        let t = s.pts();
        s.start = Some(t);
        let f = s.frame_of(t);
        core.toasts.info(format!("{} starts at {} (frame {f})", letter(i), fmt_time(t)));
        self.analysis = None;
    }

    /// The selected video's start point when Independent, every video's
    /// when Linked (their frames on screen are already in step).
    fn set_start_here(&mut self, core: &mut Core) {
        if self.sources.len() <= 1 || self.sync == SyncMode::Linked {
            self.set_all_starts(core);
        } else {
            self.set_start(self.lead(), core);
        }
    }

    fn set_all_starts(&mut self, core: &mut Core) {
        if self.sources.is_empty() {
            return;
        }
        for s in &mut self.sources {
            s.start = Some(s.pts());
        }
        self.analysis = None;
        let list: Vec<String> = self.sources.iter().enumerate().map(|(i, s)| format!("{} {}", letter(i), fmt_time(s.pts()))).collect();
        core.toasts.info(format!("Start points set: {}", list.join(", ")));
    }

    fn clear_starts(&mut self) {
        for s in &mut self.sources {
            s.start = None;
        }
        self.analysis = None;
    }

    /// Every video to its start point (its first frame without one).
    fn go_to_starts(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        self.pause();
        self.pending_step = None;
        for s in &mut self.sources {
            let t = s.start.unwrap_or(0.0);
            s.seek(t, true);
        }
        let lead = self.lead();
        self.clock.base_pts = self.sources[lead].start.unwrap_or(0.0);
    }

    // ---------------------------------------------------------------- auto-align

    fn source_paths(&self) -> Vec<PathBuf> {
        self.sources.iter().map(|s| s.path.clone()).collect()
    }

    /// Start estimating every video's start point against A.
    fn start_auto_align(&mut self, ctx: &egui::Context) {
        if self.sources.len() < 2 || self.align_job.is_some() {
            return;
        }
        self.pause();
        let videos = self.sources.iter().map(|s| (s.path.clone(), s.info.rotation)).collect();
        self.align_job = Some(AlignJob::start(videos, ctx.clone()));
    }

    fn cancel_auto_align(&mut self) {
        if let Some(job) = self.align_job.take() {
            job.cancel();
        }
    }

    /// Apply a finished auto-align (once).
    fn poll_auto_align(&mut self, core: &mut Core) {
        if self.align_job.is_none() {
            return;
        }
        let paths = self.source_paths();
        let Some(job) = &self.align_job else { return };
        if job.paths != paths {
            // Videos were added, removed or swapped: the result would not fit.
            self.cancel_auto_align();
            return;
        }
        let polled = job.poll();
        match polled {
            None => core.ctx.request_repaint_after(Duration::from_millis(250)),
            Some(res) => {
                self.align_job = None;
                match res {
                    Ok(outcomes) => self.apply_alignment(outcomes, core),
                    Err(e) if align::is_cancelled(&e) => {}
                    Err(e) => core.toasts.error(format!("Auto-align failed: {e}")),
                }
            }
        }
    }

    /// Set the start points from auto-align's offsets: A keeps its start
    /// point (moved later only so that no video would start before its
    /// first frame), the others get A's plus their offset. Videos without a
    /// usable match keep theirs.
    fn apply_alignment(&mut self, outcomes: Vec<Outcome>, core: &mut Core) {
        if outcomes.len() != self.sources.len() || self.sources.len() < 2 {
            return;
        }
        let before = self.starts();
        let before_sync = self.sync;
        let offs: Vec<Option<f64>> = outcomes
            .iter()
            .enumerate()
            .map(|(i, o)| match o {
                _ if i == 0 => Some(0.0),
                Outcome::Found(a) if a.level() >= ALIGN_MIN => Some(a.offset),
                _ => None,
            })
            .collect();
        let applied = offs.iter().skip(1).any(|o| o.is_some());
        if applied {
            let lowest = offs.iter().flatten().fold(0.0f64, |m, &o| m.min(o));
            let base = self.sources[0].start.unwrap_or(0.0).max(-lowest);
            for (s, o) in self.sources.iter_mut().zip(&offs) {
                if let Some(o) = o {
                    s.start = Some(base + o);
                }
            }
            self.sync = SyncMode::Linked;
            self.analysis = None;
            self.go_to_starts();
        }
        let list: Vec<String> = outcomes.iter().enumerate().skip(1).map(|(i, o)| format!("{} {}", letter(i), outcome_text(o).0)).collect();
        if !applied {
            core.toasts.error(format!("Auto-align found no reliable match: {}", list.join("; ")));
        } else if outcomes.iter().all(|o| outcome_text(o).1) {
            core.toasts.success(format!("Start points aligned: {}", list.join("; ")));
        } else {
            core.toasts.info(format!("Start points aligned, check the weak ones: {}", list.join("; ")));
        }
        if dev_dir().is_some() {
            let items: Vec<String> = outcomes
                .iter()
                .enumerate()
                .map(|(i, o)| match o {
                    Outcome::Found(a) => format!("{}:{:.4}/{:.2}", letter(i), a.offset, a.confidence),
                    other => format!("{}:{other:?}", letter(i)),
                })
                .collect();
            dev_log(&format!("autoalign {}", items.join(" ")));
        }
        self.align_result = Some(AlignResult { paths: self.source_paths(), outcomes, before, before_sync, after: self.starts(), applied });
    }

    /// Auto-align's start points are still in place and can be undone.
    fn can_undo_align(&self) -> bool {
        self.align_result.as_ref().is_some_and(|r| r.applied && r.paths == self.source_paths() && r.after == self.starts())
    }

    /// Put back the start points from before auto-align.
    fn undo_auto_align(&mut self, core: &mut Core) {
        if !self.can_undo_align() {
            return;
        }
        let Some(r) = self.align_result.take() else { return };
        for (s, b) in self.sources.iter_mut().zip(&r.before) {
            s.start = *b;
        }
        self.sync = r.before_sync;
        self.analysis = None;
        self.go_to_starts();
        core.toasts.info("Start points are back as they were before auto-align");
    }

    /// Show video `i` at its own time `t` (a frame timing problem): every
    /// video moves when linked, only `i` when independent.
    fn show_at(&mut self, i: usize, t: f64) {
        if i >= self.sources.len() {
            return;
        }
        if self.sync == SyncMode::Independent && self.sources.len() > 1 {
            self.select(i);
            self.seek_all(t, true);
        } else {
            let lead = self.lead();
            let t = sync::target(t, i, lead, &self.offsets());
            self.seek_all(t, true);
        }
    }

    // ---------------------------------------------------------------- playback

    /// The loop in the lead's time (None when there is none or it belongs to
    /// a video that is not moving).
    fn loop_in_lead(&self) -> Option<Loop> {
        self.loop_ab.a?;
        let lead = self.lead();
        if self.loop_lead == lead {
            return Some(self.loop_ab);
        }
        if self.sync != SyncMode::Linked || self.loop_lead >= self.sources.len() {
            return None;
        }
        let offs = self.offsets();
        let conv = |x: Option<f64>| x.map(|x| sync::target(x, self.loop_lead, lead, &offs));
        Some(Loop { a: conv(self.loop_ab.a), b: conv(self.loop_ab.b) })
    }

    fn play(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        let lead = self.lead();
        let members = self.members();
        let mut at = if self.sources[lead].awaiting { self.clock.base_pts } else { self.sources[lead].pts() };
        let fd = self.sources[lead].frame_dur();
        let looped = self.loop_in_lead();
        if let Some(a) = looped.and_then(|l| l.play_from(at, fd)) {
            // Outside the repeated section: start at its beginning.
            self.seek_all(a, true);
            at = a;
        } else if members.iter().all(|&i| self.sources[i].at_end()) {
            // Restart from the beginning when at the end.
            let t = looped.and_then(|l| l.a).unwrap_or(0.0);
            self.seek_all(t, true);
            at = t;
        }
        self.playing = true;
        self.clock.speed = self.speed;
        self.clock.start(at);
        for &i in &members {
            self.sources[i].player.send(Cmd::Play);
        }
        self.update_audio_playing();
    }

    /// Sound plays at normal speed while its video moves.
    fn update_audio_playing(&self) {
        if let Some(a) = self.audio_shared() {
            let on = self.playing && (self.speed - 1.0).abs() < 1e-6 && self.members().contains(&self.audio_src);
            a.playing.store(on, Ordering::Relaxed);
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
            Command::ZoomFit => self.view = View::default(),
            Command::ZoomActual => {
                // One pixel of A per screen pixel.
                if let Some(s) = self.sources.first() {
                    self.view = self.view.actual(s.display_size(), self.pane_px);
                }
            }
            _ => return false,
        }
        true
    }

    /// Seek the moving videos so the lead is at `t`.
    fn seek_all(&mut self, t: f64, exact: bool) {
        if self.sources.is_empty() {
            return;
        }
        self.pending_step = None;
        let was = self.playing;
        if was {
            self.pause();
        }
        let lead = self.lead();
        let t = if t.is_finite() { t.clamp(0.0, self.sources[lead].last_pts()) } else { 0.0 };
        for i in self.members() {
            let ti = self.target_of(t, i);
            self.sources[i].seek(ti, exact);
        }
        self.clock.base_pts = t;
        if was && exact {
            // Resume once the seek lands (the clock waits for it).
            self.playing = true;
            self.clock.start(t);
            for i in self.members() {
                self.sources[i].player.send(Cmd::Play);
            }
            self.update_audio_playing();
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
        let lead = self.lead();
        let cur = self.sources[lead].pts();
        // Rapid key presses: step from the frame already requested, not from
        // the frame still on screen, so every press counts.
        let cur_frame = self.pending_step.filter(|_| self.sources[lead].awaiting).unwrap_or(self.sources[lead].frame_of(cur)) as i64;
        let mut target_frame = (cur_frame + delta).max(0) as usize;
        if let Some(n) = self.sources[lead].frame_count() {
            target_frame = target_frame.min(n.saturating_sub(1));
        }
        self.pending_step = Some(target_frame);
        let target = self.sources[lead].pts_of(target_frame);
        for i in self.members() {
            let t = self.target_of(target, i).max(0.0);
            let s = &mut self.sources[i];
            if delta == 1 {
                if let Some(next) = s.queue.pop_front() {
                    if (next.pts - t).abs() < 1e-3 || i == lead {
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
        self.clock.base_pts = target;
    }

    fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playing || self.sources.is_empty() {
            return;
        }
        let lead = self.lead();
        let members = self.members();
        // While a moving video is still seeking (after a jump or a loop) the
        // clock waits, so the frames it lands on are not already late.
        let seeking = members.iter().any(|&i| {
            let s = &self.sources[i];
            s.awaiting && s.seek_started.map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false)
        });
        if seeking {
            let at = self.clock.base_pts;
            self.clock.start(at);
            ctx.request_repaint_after(Duration::from_millis(5));
            return;
        }
        let mut t = self.clock.now();
        // Prefer the audio clock (keeps sound and picture in sync), else the
        // system clock. Only while the sounding video moves and has not ended.
        let k = self.audio_src;
        let audio_ok = (self.speed - 1.0).abs() < 1e-6 && members.contains(&k) && k < self.sources.len() && !self.sources[k].at_end();
        let raw = self.audio_shared().filter(|_| audio_ok).and_then(|a| a.clock());
        // A sound track that ended before the picture stops its clock: the
        // picture then follows the system clock instead of freezing.
        let stalled = match (raw, self.audio_seen) {
            (Some(v), Some((old, at))) if v == old => at.elapsed() > Duration::from_millis(150),
            (Some(v), _) => {
                self.audio_seen = Some((v, Instant::now()));
                false
            }
            (None, _) => false,
        };
        if let Some(ac) = raw.filter(|_| !stalled) {
            // The sound is in its own video's time.
            let ac = ac - (self.target_of(0.0, k) - self.target_of(0.0, lead));
            if ac.is_finite() && (ac - t).abs() < 1.0 {
                t = ac;
                // Keep the system clock aligned so a fallback is seamless.
                self.clock.start(ac);
            }
        }
        let ended = members.iter().all(|&i| self.sources[i].at_end());
        // A-B loop: back to A after B's frame (or when the videos end).
        if let Some(l) = self.loop_in_lead() {
            if l.wrap_due(t, self.sources[lead].frame_dur(), ended || self.sources[lead].at_end()) {
                if let Some(a) = l.a {
                    self.seek_all(a, true);
                    ctx.request_repaint();
                    return;
                }
            }
        }
        let mut next_due: Option<f64> = None;
        for &i in &members {
            let target = self.target_of(t, i);
            let s = &mut self.sources[i];
            let half = s.frame_dur() * 0.5;
            while s.queue.front().map(|f| f.pts <= target + half).unwrap_or(false) {
                if let Some(f) = s.queue.pop_front() {
                    s.set_current(f);
                }
            }
            if let Some(f) = s.queue.front() {
                let due = f.pts - half - target;
                next_due = Some(next_due.map(|d| d.min(due)).unwrap_or(due));
            }
        }
        if ended {
            self.pause();
            return;
        }
        let wait = next_due.unwrap_or(0.005).clamp(0.001, 0.05) / self.speed.max(0.1);
        ctx.request_repaint_after(Duration::from_secs_f64(if wait.is_finite() { wait } else { 0.005 }));
    }

    // ---------------------------------------------------------------- frames

    /// Decode the frame on screen of every video at native resolution on a
    /// background thread, then save, copy or hand it to Compare (see
    /// `SaveTarget`); `poll_snapshot` reports the result.
    fn save_frames(&mut self, target: SaveTarget) {
        if self.sources.is_empty() || self.snap_rx.is_some() {
            return;
        }
        let items: Vec<(PathBuf, f64, String)> = self.sources.iter().map(|s| (s.path.clone(), s.pts(), s.path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())).collect();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(save_frames_job(&items, &target));
        });
        self.snap_rx = Some(rx);
    }

    fn poll_snapshot(&mut self, core: &mut Core) {
        let Some(rx) = &self.snap_rx else { return };
        match rx.try_recv() {
            Ok(Ok(done)) => {
                core.toasts.success(done.message);
                self.snap_rx = None;
                if let Some(paths) = done.open {
                    self.pause();
                    self.compare_request = Some(paths);
                }
            }
            Ok(Err(e)) => {
                core.toasts.error(e);
                self.snap_rx = None;
            }
            Err(TryRecvError::Disconnected) => self.snap_rx = None,
            Err(_) => core.ctx.request_repaint_after(Duration::from_millis(100)),
        }
    }

    /// Open the Save frames dialog (folder: the last one used, else next
    /// to the videos, else Pictures).
    fn open_save_dialog(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        let beside = self.sources[0].path.parent().map(Path::to_path_buf).filter(|d| dir_writable(d));
        let dir = last_snapshot_dir().or(beside).or_else(dirs::picture_dir).unwrap_or_else(|| PathBuf::from("."));
        self.save_dialog = Some(SaveDialog { dir, composite: self.sources.len() > 1 });
    }

    fn save_dialog_ui(&mut self, ctx: &egui::Context, core: &mut Core) {
        let Some(dlg) = &mut self.save_dialog else { return };
        let p = core.pal;
        let names = sync::frame_file_names(&self.sources.iter().map(|s| (s.path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), s.pts())).collect::<Vec<_>>());
        let n = self.sources.len();
        let mut result = None;
        let mut pick = false;
        widgets::modal(ctx, egui::Id::unique("video-save-frames"), 460.0, |ui| {
            widgets::modal_title(ui, if n == 1 { "Save frame" } else { "Save frames" });
            widgets::modal_body(ui, if n == 1 { "Saves the frame on screen as a PNG at the video's full resolution." } else { "Saves the frame on screen of every video as a PNG at its full resolution." });
            ui.add_space(8.0);
            for (i, name) in names.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(letter(i)).strong().color(letter_color(&p, i)));
                    ui.add(egui::Label::new(RichText::new(name).size(12.0)).truncate());
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Folder").color(p.muted));
                let shown = widgets::elide_start(ui, &dlg.dir.display().to_string(), theme::text::body(), (ui.available_width() - 110.0).max(60.0));
                ui.label(shown).on_hover_text(dlg.dir.display().to_string());
                if widgets::secondary_button(ui, "Change...").on_hover_text("Choose the folder for the frames").clicked() {
                    pick = true;
                }
            });
            if n > 1 {
                ui.add_space(4.0);
                widgets::checkbox(ui, &mut dlg.composite, "Also save one picture with all videos side by side").on_hover_text("One PNG with the frames next to each other (2x2 for four videos), scaled to the same height");
            }
            if let Some(ok) = widgets::dialog_buttons(ui, &p, "Cancel", "Save", widgets::ButtonKind::Primary, true) {
                result = Some(ok);
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                result = Some(false);
            }
        });
        if pick {
            if let Some(d) = rfd::FileDialog::new().set_directory(&dlg.dir).pick_folder() {
                dlg.dir = d;
            }
        }
        match result {
            Some(true) => {
                let dlg = self.save_dialog.take().unwrap();
                remember_snapshot_dir(&dlg.dir);
                self.save_frames(SaveTarget::Folder { dir: dlg.dir, composite: dlg.composite && n > 1 });
            }
            Some(false) => self.save_dialog = None,
            None => {}
        }
    }

    /// Save the frames for review and open them in image Compare (marking
    /// up, notes and report export live there).
    fn open_in_compare(&mut self) {
        let dir = crate::settings::data_dir().join("frames").join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
        self.save_frames(SaveTarget::Compare(dir));
    }

    fn start_analysis(&mut self) {
        if self.sources.len() < 2 {
            return;
        }
        let a = self.sources[0].path.clone();
        let b = self.sources[1].path.clone();
        let offset = self.offset_frames_b();
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
        // The frame timing of A and B as comments under the title line.
        let mut csv = metrics::to_csv(&rows);
        let notes: String = self.sources.iter().take(2).enumerate().filter_map(|(i, s)| s.timing().map(|r| format!("# {} frame timing ({}): {}\n", letter(i), s.name(), r.verdict))).collect();
        if let Some(nl) = csv.find('\n') {
            csv.insert_str(nl + 1, &notes);
        }
        match std::fs::write(&p, csv) {
            Ok(()) => core.toasts.success(format!("Exported {}", p.display())),
            Err(e) => core.toasts.error(e.to_string()),
        }
    }

    // ---------------------------------------------------------------- frame

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let ctx = ui.ctx().clone();
        let p = core.pal;
        ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::from_rgb(10, 10, 12));
        // Dropped files join the open videos (up to four).
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_paths(dropped, &ctx);
        }
        if dev_dir().is_some() {
            self.dev_commands(core);
        }

        self.poll_snapshot(core);
        self.poll_auto_align(core);
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

        // A file that failed to open while another plays: say so (the
        // empty view shows a card instead).
        if !self.sources.is_empty() {
            if let Some((name, why)) = self.open_error.take() {
                core.toasts.error(format!("Cannot open {name}: {why}"));
            }
        }
        // Keys first, so this frame already shows their effect (panels,
        // compare mode) instead of the state before the press.
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
        if self.save_dialog.is_some() {
            self.save_dialog_ui(&ctx, core);
        }
        keep &= !std::mem::take(&mut self.close_requested);
        if !keep {
            self.pause();
        }
        self.was_editing = ctx.text_edit_focused();
        keep
    }

    fn top_bar(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let p = core.pal;
        let mut keep = true;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back to the browser (Esc)", false).clicked() {
                keep = false;
            }
            let avail = ui.available_width();
            let n = self.sources.len();
            // Wide: names on the buttons. Narrow: icons, then the compare and
            // sync controls move into a menu.
            let pair_extra = if n == 2 { if self.mode == CompareMode::Difference { 650.0 } else { 400.0 } } else { 0.0 };
            let labels = avail >= 1350.0 + pair_extra;
            let inline_need = 800.0 + pair_extra * 0.6;
            let inline = n < 2 || avail >= inline_need;
            if avail >= 620.0 {
                let title = if avail < 900.0 { I::FILM_STRIP.to_string() } else { format!("{}  Video", I::FILM_STRIP) };
                ui.label(RichText::new(title).font(theme::text::title()));
                widgets::separator_v(ui);
            }
            if n < MAX_VIDEOS {
                let tip = if n == 0 { "Open one video to play it, or up to four to compare them (or drop files here)" } else { "Add another video to compare, up to four (or drop files here)" };
                let label = if n == 0 { "Open video" } else { "Add video" };
                let r = if avail >= 760.0 { widgets::labeled_icon_button(ui, I::PLUS, label, tip, false) } else { widgets::icon_button(ui, I::PLUS, tip, false) };
                if r.clicked() {
                    self.pick_videos(&core.ctx);
                }
            }
            if n >= 2 {
                widgets::separator_v(ui);
                if inline {
                    self.compare_controls(ui, core, false, labels);
                } else {
                    let label = match self.sync {
                        SyncMode::Linked => I::LINK,
                        SyncMode::Independent => I::LINK_BREAK,
                    };
                    let r = ui.menu_button(RichText::new(format!("{}  {label}  {}", self.mode.icon(), I::CARET_DOWN)).size(15.0), |ui| {
                        ui.set_min_width(260.0);
                        self.compare_controls(ui, core, true, true);
                    });
                    r.response.on_hover_text(format!("{} · {}: compare view and how the videos move", self.mode.label(), self.sync_label()));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // With the compare controls on the left, names only on very wide windows.
                let labels = n > 0 && avail >= if n > 1 { 1500.0 + pair_extra } else { 1280.0 };
                let button = |ui: &mut Ui, icon: &str, label: &str, tip: &str, on: bool| {
                    if labels { widgets::labeled_icon_button(ui, icon, label, tip, on) } else { widgets::icon_button(ui, icon, tip, on) }
                };
                crate::analysis_ui::toolbar_button(ui, core);
                if n == 0 {
                    return;
                }
                if button(ui, I::INFO, "Info", "Info and metrics: the current frame, start points and auto-align, quality of B against A, frame timing (dropped and duplicated frames) and media details (I)", self.show_info).clicked() {
                    self.show_info = !self.show_info;
                }
                let scopes_tip = if n > 1 { "Scopes for every video (F2): waveform, RGB parade, vectorscope and histogram show brightness and color levels" } else { "Scopes (F2): waveform, RGB parade, vectorscope and histogram show brightness and color levels" };
                if button(ui, I::WAVEFORM, "Scopes", scopes_tip, self.show_scopes).clicked() {
                    self.show_scopes = !self.show_scopes;
                }
                let save_tip = if n > 1 { "Save the frame on screen of every video as PNG, at full resolution, with an optional side-by-side picture (Ctrl+S)" } else { "Save the frame on screen as PNG at full resolution (Ctrl+S)" };
                if button(ui, I::CAMERA, if n > 1 { "Save frames" } else { "Save frame" }, save_tip, false).clicked() {
                    self.open_save_dialog();
                }
                if n > 1 && button(ui, I::IMAGES, "Open in Compare", "Open the frames on screen in image Compare, to mark areas, add notes and export a report", false).clicked() {
                    self.open_in_compare();
                }
                let keys = ui.menu_button(RichText::new(I::KEYBOARD).size(16.0), |ui| {
                    ui.set_min_width(360.0);
                    ui.label(RichText::new("Video shortcuts").font(theme::text::subtitle()));
                    ui.add_space(4.0);
                    egui::Grid::new("video-keys").num_columns(2).spacing([16.0, 3.0]).show(ui, |ui| {
                        for (k, what) in VIDEO_KEYS {
                            ui.label(RichText::new(k).monospace().size(11.5).color(p.text));
                            ui.label(RichText::new(what).size(12.0).color(p.muted));
                            ui.end_row();
                        }
                    });
                });
                keys.response.on_hover_text("Keyboard shortcuts of video mode");
            });
        });
        keep
    }

    fn sync_label(&self) -> &'static str {
        match self.sync {
            SyncMode::Linked => "Linked",
            SyncMode::Independent => "Independent",
        }
    }

    /// Compare view (two videos), difference gain, sync and start points: in
    /// the top bar, or listed in its menu (`menu`) when the bar is narrow.
    fn compare_controls(&mut self, ui: &mut Ui, core: &mut Core, menu: bool, labels: bool) {
        let p = core.pal;
        let n = self.sources.len();
        if n == 2 {
            if menu {
                ui.label(RichText::new("View").color(p.muted).size(theme::text::SMALL));
                for m in CompareMode::ALL {
                    if widgets::menu_toggle(ui, self.mode == m, m.label(), m.key()).on_hover_text(m.tip()).clicked() {
                        self.mode = m;
                    }
                }
            } else {
                let mut idx = CompareMode::ALL.iter().position(|m| *m == self.mode).unwrap_or(0);
                let names: Vec<&str> = CompareMode::ALL.iter().map(|m| if labels { m.label() } else { m.icon() }).collect();
                let tips: Vec<&str> = CompareMode::ALL.iter().map(|m| m.tip()).collect();
                if widgets::segmented_with_tips(ui, &mut idx, &names, &tips) {
                    self.mode = CompareMode::ALL[idx];
                }
            }
            if self.mode == CompareMode::Difference {
                let mut gain = |ui: &mut Ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Gain").color(p.muted).size(theme::text::SMALL)).on_hover_text("How much the differences are brightened");
                    ui.add(widgets::Slider::new(&mut self.gain, 1.0..=64.0).logarithmic(true).suffix("×").fixed_decimals(1).width(90.0).default_value(4.0))
                        .on_hover_text("Difference gain: multiplies |A - B| so small differences become visible (double-click: 4×)");
                };
                if menu {
                    ui.horizontal(|ui| gain(ui));
                } else {
                    gain(ui);
                }
            }
            if menu {
                ui.separator();
            } else {
                widgets::separator_v(ui);
            }
        }
        // Sync: Linked / Independent.
        let sync_tips = [
            "Linked: all videos play, step and seek together, kept apart by their start points (L)",
            "Independent: click a video, then move only it with the timeline and , / . to find where the event starts in it (L)",
        ];
        let mut idx = if self.sync == SyncMode::Linked { 0 } else { 1 };
        let names = if labels || menu { [format!("{}  Linked", I::LINK), format!("{}  Independent", I::LINK_BREAK)] } else { [I::LINK.to_string(), I::LINK_BREAK.to_string()] };
        let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        if widgets::segmented_with_tips(ui, &mut idx, &names, &sync_tips) {
            self.set_sync(if idx == 0 { SyncMode::Linked } else { SyncMode::Independent }, core);
        }
        let any = self.any_start();
        let set_tip = match self.sync {
            SyncMode::Linked => "Use the frame each video shows now as its start point (P)".to_string(),
            SyncMode::Independent => format!("Set {}'s start point at its current frame (P); then select the next video", letter(self.lead())),
        };
        let go_tip = "Move every video to its start point (Shift+Home)";
        let clear_tip = "Clear the start points (all videos then play from the same time)";
        if menu {
            if widgets::menu_item(ui, I::FLAG, if self.sync == SyncMode::Linked { "Set start points here" } else { "Set start point here" }, "P").on_hover_text(set_tip).clicked() {
                self.set_start_here(core);
                ui.close();
            }
            if ui.add_enabled(any, egui::Button::new(format!("{}  Go to start points", I::ARROW_U_UP_LEFT)).frame(false)).on_hover_text(go_tip).clicked() {
                self.go_to_starts();
                ui.close();
            }
            if any && widgets::menu_item(ui, I::X, "Clear start points", "").on_hover_text(clear_tip).clicked() {
                self.clear_starts();
                ui.close();
            }
            let running = self.align_job.is_some();
            let (label, tip) = if running { ("Cancel auto-align", "Stop auto-align; the start points stay as they are") } else { ("Auto-align start points", ALIGN_TIP) };
            if widgets::menu_item(ui, I::MAGIC_WAND, label, "").on_hover_text(tip).clicked() {
                if running {
                    self.cancel_auto_align();
                } else {
                    self.start_auto_align(&core.ctx);
                }
                ui.close();
            }
            if n == 2 {
                ui.separator();
                if widgets::menu_item(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B", "X").clicked() {
                    self.swap();
                    ui.close();
                }
            }
        } else {
            let set_label = if self.sync == SyncMode::Linked { "Set start points" } else { "Set start" };
            let r = if labels { widgets::labeled_icon_button(ui, I::FLAG, set_label, &set_tip, false) } else { widgets::icon_button(ui, I::FLAG, &set_tip, false) };
            if r.clicked() {
                self.set_start_here(core);
            }
            let r = ui.add_enabled_ui(any, |ui| if labels { widgets::labeled_icon_button(ui, I::ARROW_U_UP_LEFT, "Go to start", go_tip, false) } else { widgets::icon_button(ui, I::ARROW_U_UP_LEFT, go_tip, false) }).inner;
            if r.on_disabled_hover_text("No start points yet: set them with the flag button (P)").clicked() {
                self.go_to_starts();
            }
            if any && widgets::icon_button(ui, I::X, clear_tip, false).clicked() {
                self.clear_starts();
            }
            let running = self.align_job.is_some();
            let tip = if running { "Auto-align is running: click to stop it (the start points stay as they are)" } else { ALIGN_TIP };
            let r = if labels { widgets::labeled_icon_button(ui, I::MAGIC_WAND, "Auto-align", tip, running) } else { widgets::icon_button(ui, I::MAGIC_WAND, tip, running) };
            if r.clicked() {
                if running {
                    self.cancel_auto_align();
                } else {
                    self.start_auto_align(&core.ctx);
                }
            }
            if let Some(job) = &self.align_job {
                ui.label(RichText::new(format!("{:.0}%", job.progress() * 100.0)).size(theme::text::SMALL).color(p.muted))
                    .on_hover_text("Auto-align: decoding the first 45 seconds of every video at low resolution");
            }
            if n == 2 {
                widgets::separator_v(ui);
                if widgets::icon_button(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B (X)", false).clicked() {
                    self.swap();
                }
            }
        }
    }

    /// A and B trade places (with their start points and sound).
    fn swap(&mut self) {
        if self.sources.len() != 2 {
            return;
        }
        self.sources.swap(0, 1);
        let flip = |i: usize| if i < 2 { 1 - i } else { i };
        self.audio_src = flip(self.audio_src);
        self.selected = flip(self.selected);
        self.loop_lead = flip(self.loop_lead);
        self.analysis = None;
        self.metrics_now = None;
        self.diff.reset();
        self.update_native();
    }

    // ---------------------------------------------------------------- viewport

    /// Panes (points) for the views: one for each video side by side, or
    /// one shared pane for wipe and difference.
    fn pane_rects(&self, view: Rect) -> Vec<Rect> {
        let n = self.sources.len();
        if self.shared_pane() {
            vec![view; n]
        } else {
            sync::pane_rects(n, view, PANE_GAP)
        }
    }

    /// Wipe and difference draw A and B in one pane.
    fn shared_pane(&self) -> bool {
        self.sources.len() == 2 && self.mode != CompareMode::SideBySide
    }

    fn viewport(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        let view = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(view, Sense::click_and_drag());
        self.hover_view = resp.hovered();
        if self.sources.is_empty() {
            self.empty_view(ui, view, core);
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        // A minimized window or a monitor / DPI change can leave the view
        // without area for a frame or two: draw nothing then.
        if !(ppp.is_finite() && ppp > 0.0 && view.width() >= 1.0 && view.height() >= 1.0) {
            return;
        }
        let n = self.sources.len();
        let shared = self.shared_pane();
        let panes = self.pane_rects(view);
        let px = |r: Rect| r.size() * ppp;
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let hovered = pointer.filter(|p| view.contains(*p)).map(|pos| {
            if shared {
                // Wipe: the side of the divider; difference: A.
                let x = view.min.x + view.width() * self.wipe_pos;
                if self.mode == CompareMode::Wipe && pos.x > x {
                    1
                } else {
                    0
                }
            } else {
                panes.iter().position(|r| r.contains(pos)).unwrap_or(0)
            }
        });
        // Zoom with the wheel at the cursor, drag to pan (all panes follow).
        let wheel = ui.input(|i| crate::ui::wheel_delta(i));
        if resp.hovered() && wheel != 0.0 && wheel.is_finite() {
            if let (Some(k), Some(pos)) = (hovered, pointer) {
                let k = if shared { 0 } else { k };
                let f = (1.0 + wheel.clamp(-200.0, 200.0) * 0.0045).max(0.2);
                self.view = self.view.zoom_at(self.sources[k].display_size(), px(panes[k]), f, (pos - panes[k].center()) * ppp);
            }
        }
        if resp.dragged_by(egui::PointerButton::Primary) && !(shared && self.mode == CompareMode::Wipe && ui.input(|i| i.modifiers.shift)) {
            let k = if shared { 0 } else { hovered.unwrap_or(0) };
            self.view = self.view.pan_by(self.sources[k].display_size(), px(panes[k]), resp.drag_delta() * ppp);
        }
        if resp.double_clicked() {
            let k = if shared { 0 } else { hovered.unwrap_or(0) };
            self.view = if self.view.fit { View::default().actual(self.sources[k].display_size(), px(panes[k])) } else { View::default() };
        }
        // A click that only dismisses the picture's menu (or another popup)
        // does nothing else. One video: play / pause; Independent: select.
        if resp.clicked() && !egui::Popup::is_any_open(ui.ctx()) {
            if n == 1 {
                self.toggle_play();
            } else if self.sync == SyncMode::Independent {
                if let Some(k) = hovered {
                    self.select(k);
                }
            }
        }
        // Each video's zoom and frame point at its pane's center.
        self.pane_px = px(panes[0]);
        let eff: Vec<(f32, Vec2)> = match (0..n).map(|i| self.view.effective(self.sources[i].display_size(), px(panes[i]))).collect::<Option<Vec<_>>>() {
            Some(e) => e,
            None => return,
        };
        self.negotiate_sizes(&eff, ui.ctx());
        for (s, (z, _)) in self.sources.iter_mut().zip(&eff) {
            s.upload(ui.ctx(), *z >= 2.0);
        }
        let painter = ui.painter().with_clip_rect(view);
        if !shared && n > 1 {
            // Thin lines between the panes.
            painter.rect_filled(view, 0.0, Color32::from_gray(44));
            for r in &panes {
                painter.rect_filled(*r, 0.0, Color32::from_rgb(10, 10, 12));
            }
        }
        let dests: Vec<Rect> = (0..n).map(|i| sync::dest_rect(panes[i], ppp, self.sources[i].display_size(), eff[i].0, eff[i].1)).collect();
        let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let draw_tex = |painter: &egui::Painter, s: &Source, dest: Rect, clip: Rect| {
            if let Some(t) = &s.tex {
                let p2 = painter.with_clip_rect(clip.intersect(view));
                let mut mesh = egui::Mesh::with_texture(t.id());
                mesh.add_rect_with_uv(dest, full_uv, Color32::WHITE);
                rotate_mesh(&mut mesh, dest, s.info.rotation);
                p2.add(egui::Shape::mesh(mesh));
            }
        };
        // Where A and B are drawn, for the Analysis panel's overlays.
        use crate::analysis_ui::canvas::Slot;
        let publish = |slot, i: usize, clip: Rect| crate::analysis_ui::canvas::publish(ui.ctx(), slot, dests[i], self.sources[i].display_size(), clip);
        if !shared {
            for i in 0..n {
                if i < 2 {
                    publish(if i == 0 { Slot::A } else { Slot::B }, i, panes[i]);
                }
                draw_tex(&painter, &self.sources[i], dests[i], panes[i]);
            }
        } else if self.mode == CompareMode::Wipe {
            // Not while the pointer is over a menu above the picture.
            let shift = ui.input(|i| i.modifiers.shift);
            let dragging = resp.dragged() && shift;
            if (!self.wipe_frozen && !shift && self.hover_view) || dragging {
                if let Some(pos) = pointer.filter(|p| view.contains(*p)) {
                    self.wipe_pos = ((pos.x - view.min.x) / view.width()).clamp(0.0, 1.0);
                }
            }
            let x = view.min.x + view.width() * self.wipe_pos;
            let left = Rect::from_min_max(view.min, Pos2::new(x, view.max.y));
            let right = Rect::from_min_max(Pos2::new(x, view.min.y), view.max);
            publish(Slot::A, 0, left);
            publish(Slot::B, 1, right);
            draw_tex(&painter, &self.sources[0], dests[0], left);
            draw_tex(&painter, &self.sources[1], dests[1], right);
            painter.line_segment([Pos2::new(x, view.min.y), Pos2::new(x, view.max.y)], Stroke::new(1.5, Color32::from_white_alpha(200)));
            painter.circle_filled(Pos2::new(x, view.center().y), 12.0, Color32::from_black_alpha(160));
            painter.text(Pos2::new(x, view.center().y), Align2::CENTER_CENTER, I::ARROWS_LEFT_RIGHT, FontId::proportional(13.0), Color32::WHITE);
        } else {
            publish(Slot::A, 0, view);
            self.update_diff(ui.ctx());
            if let Some(t) = &self.diff_tex {
                let mut mesh = egui::Mesh::with_texture(t.id());
                mesh.add_rect_with_uv(dests[0], full_uv, Color32::WHITE);
                rotate_mesh(&mut mesh, dests[0], self.sources[0].info.rotation);
                painter.add(egui::Shape::mesh(mesh));
            }
            if let Some((mean, max)) = self.diff_stats {
                let txt = format!("|A-B| × {:.0}  ·  mean {:.2}  ·  max {:.0}", self.gain, mean, max);
                let g = painter.layout_no_wrap(txt, theme::text::small(), p.text);
                osd_pill(&painter, &p, Pos2::new(view.center().x, view.max.y - 12.0), Align2::CENTER_BOTTOM, g);
            }
        }
        // The pixel under the pointer, as a fraction of its frame.
        self.pixel = hovered.and_then(|k| {
            let pos = pointer?;
            let d = dests[k];
            (d.contains(pos) && d.width() > 0.0 && d.height() > 0.0).then(|| (((pos.x - d.min.x) / d.width()).clamp(0.0, 1.0), ((pos.y - d.min.y) / d.height()).clamp(0.0, 1.0)))
        });
        if n >= 2 {
            self.maybe_metrics();
        }
        // The selected video when Independent.
        if n > 1 && self.sync == SyncMode::Independent && !shared {
            let r = panes[self.lead()];
            painter.rect_stroke(r.shrink(1.0), 0.0, Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
        }
        // Captions: every video's letter, name and own time.
        for i in 0..n {
            // A caption's close button removes its video at once.
            if i >= self.sources.len() || self.sources.len() != n {
                break;
            }
            let (area, right) = if shared { (view, i == 1) } else { (panes[i], false) };
            self.caption(ui, &p, i, area.shrink(8.0), right, core);
        }
        self.hud(ui, &painter, &p, view, &eff);
        resp.context_menu(|ui| self.canvas_menu(ui, core));
    }

    /// The hero (icon and hints), the error card when a file failed to
    /// open, then the button: one column centered in the view.
    fn empty_view(&mut self, ui: &mut Ui, view: Rect, core: &mut Core) {
        let p = core.pal;
        let painter = ui.painter().with_clip_rect(view);
        let wrap = (view.width() - 32.0).max(40.0);
        let line = |text: &str, size: f32, color: Color32| {
            let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(size), color, wrap);
            job.halign = egui::Align::Center;
            painter.layout_job(job)
        };
        let hint = line("Drop one video to play it, or up to four videos to compare them", 15.0, p.text);
        let sub = line("Frame-accurate stepping · side by side, wipe and difference · start points · PSNR & SSIM · scopes", 12.5, p.muted);
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
        if open.on_hover_text("Choose one video to play, or up to four to compare").clicked() {
            self.pick_videos(&core.ctx);
        }
    }

    /// Ask each decoder for frames the size they are shown at (saves decode
    /// and upload bandwidth), once the window has settled: resizing, moving
    /// to another monitor or a DPI change would otherwise re-decode the
    /// frame many times a second.
    fn negotiate_sizes(&mut self, eff: &[(f32, Vec2)], ctx: &egui::Context) {
        let now = Instant::now();
        let settle = Duration::from_millis(250);
        let diff = self.shared_pane() && self.mode == CompareMode::Difference;
        let a_out = self.sources.first().map(|s| s.out_size);
        let moving: Vec<bool> = (0..self.sources.len()).map(|i| self.playing && self.members().contains(&i)).collect();
        for (i, s) in self.sources.iter_mut().enumerate() {
            let native = (s.info.width.max(2), s.info.height.max(2));
            // Difference compares pixels: B decodes at A's size.
            let (want, need) = match (diff && i == 1, a_out) {
                (true, Some(a)) => (a, s.out_size != a),
                _ => {
                    let w = sync::wanted_size(native, eff.get(i).map(|e| e.0).unwrap_or(1.0));
                    (w, sync::needs_resize(s.out_size, w))
                }
            };
            if !need {
                s.out_wait.clear();
                continue;
            }
            if s.awaiting {
                continue;
            }
            match s.out_wait.update(want, now, settle) {
                Some(w) => {
                    s.out_size = w;
                    s.player.send(Cmd::OutputSize(w.0, w.1));
                    if !moving[i] || s.at_end() {
                        // No new frames are coming: decode the frame on screen again at the new size.
                        let t = s.pts();
                        s.seek(t, true);
                        if moving[i] {
                            // The decoder paused itself at the end; it reports the end again.
                            s.player.send(Cmd::Play);
                        }
                    }
                }
                None => ctx.request_repaint_after(settle + Duration::from_millis(10)),
            }
        }
    }

    /// A video's caption at the top of its pane (`right`: at the top right):
    /// its letter, name, own time and frame, start point, and buttons for
    /// its sound, start point and closing it.
    fn caption(&mut self, ui: &mut Ui, p: &Palette, i: usize, area: Rect, right: bool, core: &mut Core) {
        if area.width() < 40.0 || area.height() < CAP_H {
            return;
        }
        let n = self.sources.len();
        let s = &self.sources[i];
        let f = s.frame_of(s.pts());
        let independent = self.sync == SyncMode::Independent && n > 1;
        let sound = n > 1 && self.audio.is_some() && s.info.has_audio;
        let start_btn = independent;
        let buttons = sound as usize + start_btn as usize + (n > 1) as usize;
        let btn_w = 24.0;
        let font = theme::text::small();
        let fixed = 8.0 + 2.0 * BADGE_R + 6.0 + 8.0 + buttons as f32 * btn_w + if buttons > 0 { 4.0 } else { 0.0 };
        let max_text = (area.width() - fixed).max(0.0);
        let time = format!("{}  ·  frame {f}", fmt_time(s.pts()));
        let start = s.start.map(|t| format!("  ·  {} {}", I::FLAG, fmt_time(t))).unwrap_or_default();
        let name = s.name();
        let painter = ui.painter().clone();
        let layout = |t: String| painter.layout_no_wrap(t, font.clone(), p.text);
        let mut g = layout(format!("{}  ·  {time}{start}", shorten(&name, 28)));
        if g.size().x > max_text {
            g = layout(format!("{time}{start}"));
        }
        if g.size().x > max_text {
            g = layout(fmt_time(s.pts()));
        }
        let text_w = if g.size().x > max_text { 0.0 } else { g.size().x };
        let w = (fixed + text_w).min(area.width());
        let min = if right { Pos2::new(area.max.x - w, area.min.y) } else { area.min };
        let r = Rect::from_min_size(min, Vec2::new(w, CAP_H));
        let sel = independent && self.lead() == i;
        let stroke = if sel { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.border) };
        painter.add(theme::popover_shadow(p).as_shape(r, CornerRadius::same(theme::radius::CARD)));
        painter.rect(r, CornerRadius::same(theme::radius::CARD), p.overlay, stroke, egui::StrokeKind::Inside);
        paint_badge(&painter, Pos2::new(r.min.x + 8.0 + BADGE_R, r.center().y), letter(i), letter_color(p, i), BADGE_R - 1.0);
        let text_rect = Rect::from_min_max(r.min, Pos2::new(r.min.x + 8.0 + 2.0 * BADGE_R + 6.0 + text_w + 4.0, r.max.y));
        if text_w > 0.0 {
            painter.galley(Pos2::new(r.min.x + 8.0 + 2.0 * BADGE_R + 6.0, r.center().y - g.size().y / 2.0), g, p.text);
        }
        let mut tip = format!("{}: {name}\n{} / {}  ·  frame {f}", letter(i), fmt_time(s.pts()), fmt_time(s.duration()));
        if let Some(t) = s.start {
            tip += &format!("\nStart point {} (frame {})", fmt_time(t), s.frame_of(t));
        }
        if independent {
            tip += "\nClick to select it: the timeline and , / . then move this video";
        }
        let id = egui::Id::unique(("video-caption", i));
        if ui.interact(text_rect, id, Sense::click()).on_hover_text(tip).clicked() && independent {
            self.select(i);
        }
        // Buttons, right to left.
        let mut x = r.max.x - 4.0 - btn_w;
        let btn = |x: f32| Rect::from_min_size(Pos2::new(x, r.center().y - btn_w / 2.0), Vec2::splat(btn_w));
        if n > 1 {
            if cap_button(ui, btn(x), id.with("close"), I::X, &format!("Close {} ({name})", letter(i)), false, p).clicked() {
                self.remove_source(i);
                return;
            }
            x -= btn_w;
        }
        if start_btn {
            let tip = format!("Set {}'s start point at this frame: where the event begins in this video (P)", letter(i));
            if cap_button(ui, btn(x), id.with("start"), I::FLAG, &tip, self.sources[i].start.is_some(), p).clicked() {
                self.set_start(i, core);
            }
            x -= btn_w;
        }
        if sound {
            let on = self.audio_src == i;
            let tip = if on { format!("You hear {}'s sound", letter(i)) } else { format!("Play {}'s sound instead", letter(i)) };
            if cap_button(ui, btn(x), id.with("sound"), if on { I::SPEAKER_HIGH } else { I::SPEAKER_SLASH }, &tip, on, p).clicked() {
                self.set_audio_source(i);
            }
        }
    }

    /// Readouts over the picture: zoom (and for one video its frame), a
    /// "Seeking" badge for slow seeks, and audio output errors.
    fn hud(&mut self, ui: &Ui, painter: &egui::Painter, p: &Palette, view: Rect, eff: &[(f32, Vec2)]) {
        let lead = self.lead();
        let a = &self.sources[lead];
        let zoom = if self.view.fit { "Fit".to_string() } else { format!("{:.0}%", eff.get(lead).map(|e| e.0).unwrap_or(1.0) * 100.0) };
        if self.sources.len() == 1 {
            let f = a.frame_of(a.pts());
            let kind = a.current.as_ref().map(|c| c.kind as char).unwrap_or('?');
            // One pill: the timecode (monospace), then frame, type and zoom.
            let mut rest = format!("Frame {f}  ·  {}  ·  {zoom}", kind_name(kind));
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
                osd_pill(painter, p, view.right_bottom() - Vec2::splat(12.0), Align2::RIGHT_BOTTOM, g);
            }
        } else if !(self.shared_pane() && self.mode == CompareMode::Difference) {
            let g = painter.layout_no_wrap(format!("{} {zoom}", I::MAGNIFYING_GLASS), theme::text::small(), p.text);
            if g.size().x + 60.0 < view.width() {
                osd_pill(painter, p, view.right_bottom() - Vec2::splat(12.0), Align2::RIGHT_BOTTOM, g);
            }
        }
        // A slow seek (long GOP) keeps the last frame on screen with a badge,
        // so stepping never looks frozen.
        let t0 = self.members().iter().filter_map(|&i| self.sources[i].seek_started).min();
        if let Some(t0) = t0 {
            let delay = Duration::from_millis(150);
            if t0.elapsed() >= delay {
                let txt = match self.pending_step {
                    Some(f) => format!("Seeking to frame {f}..."),
                    None => "Seeking...".to_string(),
                };
                let g = painter.layout_no_wrap(txt, theme::text::body(), p.text);
                osd_pill(painter, p, Pos2::new(view.center().x, view.min.y + 44.0), Align2::CENTER_TOP, g);
            } else {
                ui.ctx().request_repaint_after(delay.saturating_sub(t0.elapsed()));
            }
        }
        if self.audio.is_none() {
            if let Some(e) = &self.audio_error {
                painter.text(view.left_bottom() + Vec2::new(14.0, -20.0), Align2::LEFT_CENTER, format!("{}  No audio output: {e}", I::SPEAKER_SLASH), FontId::proportional(11.5), p.muted);
            }
        }
    }

    /// The picture's right-click menu, laid out like the viewer's: the
    /// transport, frames, compare views (two videos), panels and leaving.
    /// Esc closes only the menu (see `keys`).
    fn canvas_menu(&mut self, ui: &mut Ui, core: &mut Core) {
        ui.set_min_width(240.0);
        let n = self.sources.len();
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
        if item(ui, I::CAMERA, if n > 1 { "Save frames as PNG..." } else { "Save frame as PNG..." }, "Ctrl+S") {
            ui.close();
            self.open_save_dialog();
        }
        if item(ui, I::COPY, if n > 1 { "Copy frames side by side" } else { "Copy frame" }, "Ctrl+C") {
            ui.close();
            self.save_frames(SaveTarget::Clipboard);
        }
        if n > 1 && item(ui, I::IMAGES, "Open frames in Compare", "") {
            ui.close();
            self.open_in_compare();
        }
        if n > 1 {
            ui.separator();
            if n == 2 {
                widgets::submenu(ui, self.mode.icon(), "View", |ui| {
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
            let linked = self.sync == SyncMode::Linked;
            if check(ui, I::LINK, "Linked (play together)", "L", linked) {
                self.toggle_sync(core);
                ui.close();
            }
            if item(ui, I::FLAG, if linked { "Set start points here" } else { "Set start point here" }, "P") {
                self.set_start_here(core);
                ui.close();
            }
            if self.any_start() && item(ui, I::ARROW_U_UP_LEFT, "Go to start points", "Shift+Home") {
                self.go_to_starts();
                ui.close();
            }
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
        if item(ui, I::X, if n > 1 { "Close videos" } else { "Close video" }, "Esc") {
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
        let pane_px = pane * if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 };
        let Some(fit) = sync::fit_zoom((w, h), pane_px) else { return };
        let (w, h) = (w as f32, h as f32);
        let (rw, rh) = (((r[2] - r[0]) * w).max(16.0), ((r[3] - r[1]) * h).max(16.0));
        let z = sync::sane_zoom((pane_px.x / rw).min(pane_px.y / rh) * 0.6);
        let rel = sync::safe_clamp(z / fit, 0.05, 256.0);
        self.view = View { fit: false, zoom: rel, center: Vec2::new(sync::safe_clamp((r[0] + r[2]) / 2.0, 0.0, 1.0), sync::safe_clamp((r[1] + r[3]) / 2.0, 0.0, 1.0)) };
    }

    /// Start the difference image of the frames on screen (off the UI
    /// thread) and show the latest finished one.
    fn update_diff(&mut self, ctx: &egui::Context) {
        if let Some((ci, mean, max)) = self.diff.poll() {
            self.diff_stats = Some((mean, max));
            match &mut self.diff_tex {
                Some(t) => t.set(ci, egui::TextureOptions::NEAREST),
                None => self.diff_tex = Some(ctx.load_texture("video-diff", ci, egui::TextureOptions::NEAREST)),
            }
        }
        let (Some(a), Some(b)) = (self.sources[0].current.as_ref(), self.sources[1].current.as_ref()) else { return };
        let key = (a.pts.to_bits(), b.pts.to_bits(), self.gain.to_bits(), b.rgba.dimensions());
        let (ia, ib, gain) = (a.rgba.clone(), b.rgba.clone(), self.gain);
        // While playing at most about 15 a second; paused at once.
        let gap = if self.playing { Duration::from_millis(60) } else { Duration::ZERO };
        self.diff.request(key, gap, ctx, move || diff_image(&ia, &ib, gain));
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
        let loop_ab = match (self.loop_lead, self.loop_ab.range()) {
            (0, Some((x, y))) => Some((a.frame_of(x), a.frame_of(y))),
            _ => None,
        };
        let current = a.current.as_ref().map(|f| a.frame_of(f.pts));
        core.metrics.set_video(Some(VideoSubject { a: side(a), b: self.sources.get(1).map(side), offset: self.offset_frames_b(), current, loop_ab }));
        if let Some(f) = core.metrics.take_seek() {
            let t = self.sources[0].pts_of(f as usize);
            self.pause();
            let lead = self.lead();
            let t = if lead == 0 { t } else { sync::target(t, 0, lead, &self.offsets()) };
            self.seek_all(t, true);
        }
        // With B loaded both players already copy native planes.
        if self.sources.len() != 1 {
            self.metrics_native = false;
        } else if core.metrics.visible() != self.metrics_native {
            self.metrics_native = !self.metrics_native;
            self.update_native();
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
        let lead = self.lead();
        let n = self.sources.len();
        ui.allocate_ui_with_layout(row, egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let fit = self.transport_fit(ui);
            let b = |ui: &mut Ui, icon: &str, tip: &str| widgets::icon_button(ui, icon, tip, false).clicked();
            if fit.first_last && b(ui, I::SKIP_BACK, "First frame (Home)") {
                self.seek_all(0.0, true);
            }
            if fit.skip_secs && b(ui, I::CARET_DOUBLE_LEFT, "Back 1 s (Shift+←)") {
                let t = self.sources[lead].pts() - 1.0;
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
                let t = self.sources[lead].pts() + 1.0;
                self.seek_all(t, true);
            }
            if fit.first_last && b(ui, I::SKIP_FORWARD, "Last frame (End)") {
                let n = self.sources[lead].frame_count().unwrap_or(1);
                let t = self.sources[lead].pts_of(n.saturating_sub(1));
                self.seek_all(t, true);
            }
            widgets::separator_v(ui);
            if n > 1 {
                // Which video the timeline and the counters show.
                let (r, _) = ui.allocate_exact_size(Vec2::new(2.0 * BADGE_R + 6.0, 26.0), Sense::hover());
                paint_badge(ui.painter(), r.center(), letter(lead), letter_color(&p, lead), BADGE_R - 1.0);
                let tip = match self.sync {
                    SyncMode::Linked => "The timeline shows A; all videos move together (Linked)".to_string(),
                    SyncMode::Independent => format!("The timeline and frame keys move only {} (Independent: click a video or press Tab to choose)", letter(lead)),
                };
                ui.interact(r, ui.scope_id().with("lead-badge"), Sense::hover()).on_hover_text(tip);
            }
            let s = &self.sources[lead];
            let (time, frame) = self.position_text(fit.duration, fit.frame_word);
            if fit.time {
                ui.add_space(4.0);
                readout(ui, time, p.text, Sense::hover()).on_hover_text(format!("Time: {} / {}", fmt_time(s.pts()), fmt_time(s.duration())));
                ui.add_space(2.0);
            }
            let fr = readout(ui, frame, p.muted, Sense::click()).on_hover_text("Frame number. Click to go to a frame or time (G)");
            if fr.clicked() {
                self.goto = Some(String::new());
            }
            // A-B loop
            if fit.loop_button {
                widgets::separator_v(ui);
                let lab = self.loop_label();
                let text = if fit.loop_label { format!("{} {lab}", I::REPEAT) } else { I::REPEAT.to_string() };
                let tip = match self.loop_ab.range() {
                    None if self.loop_ab.a.is_some() => "Repeat a section: now go to its last frame and press again to set the end (Ctrl+L)".to_string(),
                    None => "Repeat a section: press to set its start here, again to set its end, a third time to clear (Ctrl+L)".to_string(),
                    Some((a, b)) => format!("Repeating {} - {} while playing. Press to clear (Ctrl+L)", fmt_time(a), fmt_time(b)),
                };
                if ui.add(egui::Button::new(RichText::new(text).size(12.5)).selected(self.loop_ab.a.is_some()).min_size(Vec2::new(0.0, 26.0))).on_hover_text(tip).clicked() {
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
                let k = self.audio_src.min(n - 1);
                let who = if n > 1 { format!(" of {}", letter(k)) } else { String::new() };
                if self.audio.is_some() && self.sources[k].info.has_audio {
                    let slim = |ui: &mut Ui, v: &mut f32| ui.add(widgets::Slider::new(v, 0.0..=1.5).show_value(false).width(90.0).default_value(1.0));
                    if fit.volume && slim(ui, &mut self.volume).on_hover_text(format!("Volume{who} (double-click: 100%)")).changed() {
                        if let Some(a) = self.audio_shared() {
                            a.set_volume(self.volume);
                        }
                    }
                    let vi = if self.muted || self.volume == 0.0 { I::SPEAKER_SLASH } else { I::SPEAKER_HIGH };
                    let tip = if n > 1 { format!("Mute (M). You hear {}; the speaker on a video's caption picks another", letter(k)) } else { "Mute (M)".to_string() };
                    if widgets::icon_button(ui, vi, &tip, self.muted).clicked() {
                        self.toggle_mute();
                    }
                } else if fit.volume {
                    ui.label(RichText::new(format!("{} no audio", I::SPEAKER_SLASH)).size(11.5).color(p.faint)).on_hover_text(format!("No sound{who}"));
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
                speed.response.on_hover_text("Playback speed ([ and ]; Backspace: 1×). Sound plays at 1× only");
                if fit.speed_label {
                    ui.label(RichText::new("Speed").size(12.5).color(p.muted));
                }
            });
        });
    }

    fn loop_label(&self) -> &'static str {
        match (self.loop_ab.a, self.loop_ab.b) {
            (None, _) => "Loop A-B",
            (Some(_), None) => "Set end",
            _ => "Clear loop",
        }
    }

    /// The time and frame counter of the transport bar (`duration`: with
    /// the total time, `word`: "frame 12 / 300" rather than "12 / 300").
    /// The counter is padded on the left to its width on the last frame, so
    /// in the monospace readout nothing after it moves while the video plays.
    fn position_text(&self, duration: bool, word: bool) -> (String, String) {
        let a = &self.sources[self.lead()];
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
        let n = self.sources.len();
        let k = self.audio_src.min(n.saturating_sub(1));
        let audio = self.audio.is_some() && self.sources.get(k).map(|s| s.info.has_audio).unwrap_or(false);
        let badge = if n > 1 { 2.0 * BADGE_R + 6.0 + s } else { 0.0 };
        let width = |f: &TransportFit| -> f32 {
            let (time, frame) = self.position_text(f.duration, f.frame_word);
            let mut w = 2.0 * icon + 34.0 + s + sep + badge;
            w += if f.first_last { 2.0 * icon } else { 0.0 } + if f.skip_secs { 2.0 * icon } else { 0.0 };
            w += if f.time { text_w(&time, mono.clone()) + 2.0 * s + 6.0 } else { 0.0 } + text_w(&frame, mono.clone()) + 8.0 + s;
            if f.loop_button {
                w += sep + if f.loop_label { text_w(&format!("{} {}", I::REPEAT, self.loop_label()), small.clone()) } else { text_w(I::REPEAT, small.clone()) } + pad + s;
            }
            if f.legend {
                let cap = FontId::proportional(theme::text::CAPTION);
                let psnr = if self.analysis.is_some() { 26.0 + text_w("PSNR-Y (20-60 dB)", cap.clone()) } else { 0.0 };
                w += sep + 6.0 + 22.0 + text_w("Frame size", cap.clone()) + 20.0 + text_w("I-frame", cap.clone()) + 26.0 + text_w("Start point", cap) + psnr;
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
        if let Some(t) = self.sources.get(self.lead()).map(|x| x.pts()) {
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

    /// Ctrl+L: set the repeated section's start, then its end, then clear
    /// it (in the lead video's time).
    fn toggle_loop(&mut self) {
        let Some(s) = self.sources.get(self.lead()) else { return };
        let t = if s.awaiting { self.clock.base_pts } else { s.pts() };
        let lead = self.lead();
        let current = if self.loop_ab.a.is_some() && self.loop_lead != lead {
            // Set in another video's time: carry it over, or start again.
            self.loop_in_lead().unwrap_or_default()
        } else {
            self.loop_ab
        };
        self.loop_ab = current.press(t);
        self.loop_lead = lead;
    }

    fn timeline(&mut self, ui: &mut Ui, p: &Palette) {
        // Tick to label: half the playhead's head (5 pt) plus a 4 pt gap.
        const LABEL_GAP: f32 = 9.0;
        let h = 60.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width().max(1.0), h), Sense::click_and_drag());
        if rect.width() < 20.0 {
            return;
        }
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, CornerRadius::same(theme::radius::CONTROL), ui.visuals().extreme_bg_color);
        let lead = self.lead();
        let looped = self.loop_in_lead();
        let a = &self.sources[lead];
        let dur = a.duration().max(0.001);
        // The time axis is inset so the playhead's 10 pt head stays whole at
        // either end.
        let track = rect.shrink2(Vec2::new(6.0, 0.0));
        let x_of = |t: f64| track.min.x + (t / dur).clamp(-0.01, 1.01) as f32 * track.width();
        let t_of = |x: f32| (((x - track.min.x) / track.width().max(1.0)) as f64 * dur).clamp(0.0, dur);
        // A time ruler along the top, the frame-size bars in the middle and a
        // strip at the bottom with the frame types (I ticks; P and B dots once
        // frames are far enough apart to tell them apart).
        let ruler_h = 16.0;
        let strip_h = 10.0;
        let base = rect.max.y - strip_h;
        let bar_h = base - (rect.min.y + ruler_h) - 2.0;
        // Loop region
        if let Some(l) = looped {
            if let Some(la) = l.a {
                let lb = l.b.unwrap_or(a.pts());
                painter.rect_filled(Rect::from_min_max(Pos2::new(x_of(la.min(lb)), rect.min.y), Pos2::new(x_of(la.max(lb)), rect.max.y)), 0.0, p.accent.gamma_multiply(0.18));
            }
        }
        let step = tick_step(dur, track.width(), 84.0);
        let label_font = FontId::proportional(theme::text::CAPTION);
        for k in 0.. {
            let t = k as f64 * step;
            if t > dur || k > 10_000 {
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
            let tints = timeline_tints(p);
            let mut mesh = egui::Mesh::default();
            let mut last_tick = f32::NEG_INFINITY;
            let tick = p.muted.gamma_multiply(0.8);
            if let Some((_, _, columns, _)) = self.timeline_cache.as_ref() {
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
        // Metric graph (PSNR-Y) from analysis (in A's frames).
        if let (Some(an), 0) = (&self.analysis, lead) {
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
        // The lead's start point: a flag on the ruler.
        if let Some(st) = a.start {
            let x = x_of(st).round() + 0.5;
            painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(1.0, START_COLOR));
            painter.text(Pos2::new(x + 1.0, rect.min.y + 1.0), Align2::LEFT_TOP, I::FLAG, FontId::proportional(11.0), START_COLOR);
        }
        // Playhead: a 2 pt accent line with a 10 pt head on the ruler.
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
            if let (Some(an), 0) = (&self.analysis, lead) {
                if let Some((_, m)) = an.rows.lock().iter().find(|(i, _)| *i == f) {
                    tip += &format!("  ·  PSNR {:.2} dB  SSIM {:.4}", metrics::cap_db(m.psnr_y), m.ssim_y);
                }
            }
            if self.sources.len() > 1 {
                tip = format!("{}: {tip}", letter(lead));
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
            item(ui, Vec2::new(1.0, 10.0), START_COLOR, false, "Start point");
            if self.analysis.is_some() {
                item(ui, Vec2::new(12.0, 2.0), PSNR_COLOR, false, "PSNR-Y (20-60 dB)");
            }
        })
        .response
        .on_hover_text("Timeline: frame sizes (keyframes lighter), I-frame ticks below, the start point as a flag; P and B frames show as dots once frames are far enough apart");
    }

    // ---------------------------------------------------------------- panels

    fn info_panel(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let n = self.sources.len();
            let compare = n > 1;
            // Current frame
            widgets::section_title(ui, &p, "Current frame");
            for (i, s) in self.sources.iter().enumerate() {
                let f = s.frame_of(s.pts());
                let e = s.entry(f);
                let kind = s.current.as_ref().map(|c| c.kind as char).unwrap_or('?');
                let size = e.map(|e| crate::catalog::human_size(e.size as u64)).unwrap_or_default();
                widgets::kv_row(ui, &p, letter(i), &format!("#{f}  {}  {}  {}", fmt_time(s.pts()), kind_name(kind), size));
            }
            if let Some((fx, fy)) = self.pixel {
                for (i, s) in self.sources.iter().enumerate() {
                    if let Some(c) = &s.current {
                        let (dw, dh) = s.display_size();
                        let (x, y) = (((fx * dw as f32) as u32).min(dw - 1), ((fy * dh as f32) as u32).min(dh - 1));
                        let sx = ((fx * c.rgba.width() as f32) as u32).min(c.rgba.width().saturating_sub(1));
                        let sy = ((fy * c.rgba.height() as f32) as u32).min(c.rgba.height().saturating_sub(1));
                        let mut line = format!("({x}, {y})");
                        if s.info.rotation == 0 && c.rgba.width() > 0 && c.rgba.height() > 0 {
                            let px = c.rgba.get_pixel(sx, sy).0;
                            line += &format!("   RGB {},{},{}", px[0], px[1], px[2]);
                        }
                        widgets::kv_row(ui, &p, &if compare { format!("Pixel {}", letter(i)) } else { "Pixel".into() }, &line);
                    }
                }
            }
            if compare {
                ui.add_space(8.0);
                widgets::section_title(ui, &p, "Start points");
                ui.label(RichText::new(match self.sync {
                    SyncMode::Linked => "Linked: the videos play together, this far apart.",
                    SyncMode::Independent => "Independent: only the selected video moves. Step each one to where the event begins and set its start point.",
                }).size(11.5).color(p.muted));
                let offs = self.offsets();
                for (i, s) in self.sources.iter().enumerate() {
                    let text = match s.start {
                        Some(t) => {
                            let off = offs.get(i).copied().unwrap_or(0.0);
                            let rel = if i == 0 { String::new() } else if off.abs() < 5e-4 { "  ·  with A".into() } else { format!("  ·  A {} {:.3} s", if off > 0.0 { "+" } else { "-" }, off.abs()) };
                            format!("{} (frame {}){rel}", fmt_time(t), s.frame_of(t))
                        }
                        None => "not set (runs with A)".into(),
                    };
                    widgets::kv_row(ui, &p, letter(i), &if i == 0 && s.start.is_none() { "not set (from the first frame)".into() } else { text });
                }
                ui.add_space(4.0);
                self.align_section(ui, core);
                ui.add_space(8.0);
                widgets::section_title(ui, &p, "Quality: B compared with A");
                match &self.metrics_now {
                    Some((_, m)) => {
                        widgets::kv_row(ui, &p, "PSNR Y", &fmt_db(m.psnr_y));
                        widgets::kv_row(ui, &p, "PSNR U / V", &format!("{} / {}", fmt_db(m.psnr_u), fmt_db(m.psnr_v)));
                        widgets::kv_row(ui, &p, "PSNR avg", &fmt_db(m.psnr_avg));
                        widgets::kv_row(ui, &p, "SSIM Y", &fmt_ssim(m.ssim_y));
                        ui.label(RichText::new("A is the reference. Higher is closer to A (PSNR above 40 dB and SSIM above 0.98 look the same to most people).").size(11.0).color(p.faint));
                    }
                    None => {
                        let mismatch = self.sources[0].info.width != self.sources[1].info.width || self.sources[0].info.height != self.sources[1].info.height;
                        ui.label(RichText::new(if mismatch { "Metrics need identical resolutions (A and B differ)." } else if self.playing { "Pause to measure the current frame." } else { "Measuring..." }).size(12.0).color(p.muted));
                    }
                }
                ui.add_space(6.0);
                match &self.analysis {
                    None => {
                        if ui
                            .add(egui::Button::new(format!("{}  Analyze whole video (PSNR & SSIM per frame)", I::CHART_LINE)).min_size(Vec2::new(ui.available_width(), 28.0)))
                            .on_hover_text("Measure every frame of B against A (using the start points), then list the worst frames and draw PSNR on the timeline")
                            .clicked()
                        {
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
                            if ui.button("Cancel").on_hover_text("Stop the analysis (the frames measured so far stay)").clicked() {
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
                                if ui
                                    .add(egui::Button::new(RichText::new(format!("#{i:<7} PSNR {:>6.2} dB   SSIM {:.4}", metrics::cap_db(m.psnr_y), m.ssim_y)).monospace().size(11.5)).frame(false))
                                    .on_hover_text(format!("Show A's frame {i} (and the matching frame of B)"))
                                    .clicked()
                                {
                                    jump = Some(*i);
                                }
                            }
                            if let Some(i) = jump {
                                let t = self.sources[0].pts_of(i);
                                let lead = self.lead();
                                let t = if lead == 0 { t } else { sync::target(t, 0, lead, &self.offsets()) };
                                self.seek_all(t, true);
                            }
                            ui.horizontal(|ui| {
                                if ui.button(format!("{}  Export CSV", I::EXPORT)).on_hover_text("Save the per-frame PSNR and SSIM as a CSV file (for Excel)").clicked() {
                                    self.export_csv(core);
                                }
                                if done && ui.button("Re-run").on_hover_text("Clear these results and analyze again (e.g. after changing start points)").clicked() {
                                    self.analysis = None;
                                }
                            });
                        }
                    }
                }
            }
            ui.add_space(8.0);
            self.timing_section(ui, core);
            ui.add_space(8.0);
            widgets::section_title(ui, &p, "Media info");
            let durations: Vec<f64> = self.sources.iter().map(|s| s.duration()).collect();
            let rows = media_rows(&self.sources.iter().map(|s| &s.info).collect::<Vec<_>>(), &durations);
            media_table(ui, &p, n, rows);
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Decoder: FFmpeg {}", engine::ffmpeg_version())).size(10.5).color(p.faint));
        });
    }

    /// Auto-align in the Info panel: the button (or its progress), what it
    /// found and Undo.
    fn align_section(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        if let Some(job) = &self.align_job {
            let prog = job.progress();
            let mut cancel = false;
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 72.0).max(60.0);
                ui.add(egui::ProgressBar::new(prog).desired_width(w).text(format!("Aligning... {:.0}%", prog * 100.0)))
                    .on_hover_text("Decoding the first 45 seconds of every video at low resolution");
                cancel = ui.button("Cancel").on_hover_text("Stop auto-align; the start points stay as they are").clicked();
            });
            if cancel {
                self.cancel_auto_align();
            }
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            return;
        }
        if ui.add(egui::Button::new(format!("{}  Auto-align start points", I::MAGIC_WAND)).min_size(Vec2::new(ui.available_width(), 28.0))).on_hover_text(ALIGN_TIP).clicked() {
            self.start_auto_align(&core.ctx);
        }
        let paths = self.source_paths();
        let Some(r) = self.align_result.as_ref().filter(|r| r.paths == paths) else { return };
        let mut weak = false;
        for (i, o) in r.outcomes.iter().enumerate().skip(1) {
            let (text, good) = outcome_text(o);
            weak |= !good;
            let tip = match o {
                Outcome::Found(a) => format!("Correlation {:.2} at this offset, {:.2} at the best other one: confidence {:.0}%", a.peak, a.second, a.confidence * 100.0),
                _ => "Auto-align could not use this video".to_string(),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(letter(i)).strong().color(letter_color(&p, i)));
                ui.add(egui::Label::new(RichText::new(text).size(theme::text::SMALL).color(if good { p.text } else { p.warn })).wrap()).on_hover_text(tip);
            });
        }
        if weak {
            let msg = format!("{}  Not every video could be aligned with confidence. Check them side by side and fine-tune with Independent and , / .", I::WARNING);
            ui.add(egui::Label::new(RichText::new(msg).size(theme::text::SMALL).color(p.warn)).wrap());
        }
        if self.can_undo_align() && ui.button(format!("{}  Undo auto-align", I::ARROW_COUNTER_CLOCKWISE)).on_hover_text("Put the start points back as they were before auto-align").clicked() {
            self.undo_auto_align(core);
        }
    }

    /// Frame timing in the Info panel, per video: the verdict, the numbers
    /// and the problem places (click one to show it).
    fn timing_section(&mut self, ui: &mut Ui, core: &mut Core) {
        const MAX_PLACES: usize = 8;
        let p = core.pal;
        let compare = self.sources.len() > 1;
        widgets::section_title(ui, &p, "Frame timing");
        let plural = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let mut jump: Option<(usize, f64)> = None;
        let mut any = false;
        for (i, s) in self.sources.iter().enumerate() {
            if compare {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(letter(i)).strong().color(letter_color(&p, i)));
                    ui.add(egui::Label::new(RichText::new(s.name()).size(theme::text::SMALL).color(p.muted)).truncate());
                });
            }
            let Some(r) = s.timing() else {
                let text = if s.indexing() {
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                    format!("Reading the frame index... {:.0}%", f32::from_bits(s.index_progress.load(Ordering::Relaxed)) * 100.0)
                } else {
                    "The frame index could not be read".to_string()
                };
                ui.label(RichText::new(text).size(theme::text::SMALL).color(p.muted));
                ui.add_space(6.0);
                continue;
            };
            any = true;
            let (icon, color) = if r.smooth { (I::CHECK_CIRCLE, p.ok) } else { (I::WARNING, p.warn) };
            ui.add(egui::Label::new(RichText::new(format!("{icon}  {}", r.verdict)).size(12.5).color(color)).wrap()).on_hover_text(
                "Measured from the timestamp and size of every frame. A gap longer than 1.5 frame intervals means dropped frames; a non-key frame of only a few bytes usually repeats the picture before it",
            );
            let rate = if r.vfr {
                format!("{} to {} fps, {} on average", timing::fmt_fps(r.min_fps), timing::fmt_fps(r.max_fps), timing::fmt_fps(r.measured_fps))
            } else {
                format!("{} fps", timing::fmt_fps(r.measured_fps))
            };
            let declared = if r.declared_fps > 0.0 && (r.declared_fps - r.measured_fps).abs() > 0.01 * r.declared_fps { format!("  ·  file says {}", timing::fmt_fps(r.declared_fps)) } else { String::new() };
            widgets::kv_row(ui, &p, "Frame rate", &format!("{rate}{declared}"));
            widgets::kv_row(ui, &p, "Frames", &format!("{}  ·  {}", r.frames, fmt_time(r.duration)));
            let dropped = if r.dropped_events == 0 {
                "none".to_string()
            } else {
                format!("{} in {} ({:.2} s)", plural(r.dropped_frames as u64, "frame", "frames"), plural(r.dropped_events as u64, "gap", "gaps"), r.dropped_secs)
            };
            widgets::kv_row(ui, &p, "Dropped", &dropped);
            let mut dups = Vec::new();
            if r.repeated_timestamps > 0 {
                dups.push(plural(r.repeated_timestamps as u64, "repeated timestamp", "repeated timestamps"));
            }
            if r.likely_duplicates > 0 {
                dups.push(format!("{} likely (tiny frames)", r.likely_duplicates));
            }
            widgets::kv_row(ui, &p, "Duplicated", &if dups.is_empty() { "none".to_string() } else { dups.join(", ") });
            widgets::kv_row(ui, &p, "Jitter", &format!("{:.2} ms typical, {:.2} ms at most", r.jitter_std_ms, r.jitter_max_ms));
            widgets::kv_row(ui, &p, "Stutter", &if r.stutter_events == 0 { "none".to_string() } else { plural(r.stutter_events as u64, "place", "places") });
            if !r.issues.is_empty() {
                ui.label(RichText::new("Problem places (click to show)").size(11.5).color(p.faint));
                for is in r.issues.iter().take(MAX_PLACES) {
                    let (what, tip) = match is.kind {
                        IssueKind::Dropped => (
                            format!("{} dropped ({:.0} ms missing)", plural(is.frames as u64, "frame", "frames"), is.secs * 1000.0),
                            format!("Show the last frame before the gap (frame {}); step forward with . to see the jump", is.frame.saturating_sub(1)),
                        ),
                        IssueKind::RepeatedTimestamp => (format!("Timestamp repeated by {}", plural(is.frames as u64, "frame", "frames")), format!("Show frame {}, whose timestamp an earlier frame already has", is.frame)),
                        IssueKind::LikelyDuplicate => (
                            format!("{} likely duplicated", plural(is.frames as u64, "frame", "frames")),
                            format!("Show frame {}: a non-key frame of only a few bytes, which normally repeats the picture before it", is.frame),
                        ),
                        IssueKind::Stutter => (
                            format!("Stutter, up to {:.0} ms off", is.secs * 1000.0),
                            format!("Show the frame before it: {} shown noticeably early or late from frame {}", plural(is.frames as u64, "frame", "frames"), is.frame),
                        ),
                    };
                    if ui.add(egui::Button::new(RichText::new(format!("{}  {what}", timing::fmt_pos(is.t))).monospace().size(11.5)).frame(false)).on_hover_text(tip).clicked() {
                        jump = Some((i, is.t));
                    }
                }
                if r.issues.len() > MAX_PLACES {
                    ui.label(RichText::new(format!("and {} more (all of them in Export timing CSV)", r.issues.len() - MAX_PLACES)).size(11.0).color(p.faint));
                }
            }
            ui.add_space(6.0);
        }
        if let Some((i, t)) = jump {
            self.show_at(i, t);
        }
        if any && ui.button(format!("{}  Export timing CSV", I::EXPORT)).on_hover_text("Save the frame timing of every video (a summary and every problem place) as a CSV file (for Excel)").clicked() {
            self.export_timing_csv(core);
        }
    }

    fn export_timing_csv(&self, core: &mut Core) {
        let compare = self.sources.len() > 1;
        let reports: Vec<(String, Arc<TimingReport>)> = self.sources.iter().enumerate().filter_map(|(i, s)| s.timing().map(|r| (if compare { format!("{} {}", letter(i), s.name()) } else { s.name() }, r))).collect();
        if reports.is_empty() {
            return;
        }
        let refs: Vec<(String, &TimingReport)> = reports.iter().map(|(n, r)| (n.clone(), r.as_ref())).collect();
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("lumen_timing.csv").save_file() else { return };
        match std::fs::write(&path, timing::to_csv(&refs)) {
            Ok(()) => core.toasts.success(format!("Exported {}", path.display())),
            Err(e) => core.toasts.error(e.to_string()),
        }
    }

    fn scopes_panel(&mut self, ui: &mut Ui, p: &Palette) {
        let n = self.sources.len();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Scopes").font(theme::text::subtitle()));
            ui.label(RichText::new(I::QUESTION).color(p.muted)).on_hover_text(
                "Scopes measure the picture instead of showing it:\n\
                 • Waveform: brightness of each column of the picture, from black (bottom) to white (top).\n\
                 • RGB parade: the same for red, green and blue side by side; uneven heights mean a color cast.\n\
                 • Vectorscope: the colors; distance from the center is saturation, direction is hue. The line marks skin tones.\n\
                 • Histogram: how many pixels have each brightness, from black (left) to white (right).",
            );
            if n > 1 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("All {n} videos")).size(theme::text::SMALL).color(p.muted));
                });
            }
        });
        ui.add_space(4.0);
        // Short names so the four segments fit the panel; the full ones on hover.
        let names = ["Waveform", "Parade", "Vector", "Histogram"];
        let tips = [
            "Waveform: brightness (luma) of every picture column, black at the bottom, white at the top",
            "RGB parade: red, green and blue waveforms side by side",
            "Vectorscope: hue and saturation of the colors (BT.709); boxes are 75% color targets, the line is skin tone",
            "Histogram: how many pixels have each level, black on the left, white on the right",
        ];
        let mut idx = Scope::ALL.iter().position(|s| *s == self.scope).unwrap_or(0);
        if widgets::segmented_with_tips(ui, &mut idx, &names, &tips) {
            self.scope = Scope::ALL[idx];
        }
        if self.scope == Scope::Histogram {
            ui.add_space(4.0);
            let mut h = HistMode::ALL.iter().position(|m| *m == self.hist_mode).unwrap_or(2);
            let labels: Vec<&str> = HistMode::ALL.iter().map(|m| m.label()).collect();
            let tips = ["Luminance: the brightness of the pixels only", "RGB: red, green and blue separately", "Both: red, green, blue and brightness together"];
            if widgets::segmented_with_tips(ui, &mut h, &labels, &tips) {
                self.hist_mode = HistMode::ALL[h];
            }
        }
        ui.add_space(theme::space::M);
        let ctx = ui.ctx().clone();
        let (scope, hist, playing) = (self.scope, self.hist_mode, self.playing);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for i in 0..n {
                let s = &mut self.sources[i];
                // The latest finished scope image, then the next one.
                if let Some(ci) = s.scope_job.poll() {
                    match &mut s.scope_tex {
                        Some(t) => t.set(ci, egui::TextureOptions::LINEAR),
                        None => s.scope_tex = Some(ctx.load_texture(format!("scope:{i}"), ci, egui::TextureOptions::LINEAR)),
                    }
                }
                if let Some(cur) = &s.current {
                    let key: ScopeKey = (cur.pts.to_bits(), Arc::as_ptr(&cur.rgba) as usize, scope as u8, hist as u8);
                    let rgba = cur.rgba.clone();
                    // While playing at most about 12 a second per video.
                    let gap = if playing { Duration::from_millis(80) } else { Duration::ZERO };
                    s.scope_job.request(key, gap, &ctx, move || {
                        let img = scopes::render_with(scope, &rgba, hist);
                        egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
                    });
                }
                if n > 1 {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(2.0 * BADGE_R), Sense::hover());
                        paint_badge(ui.painter(), r.center(), letter(i), letter_color(p, i), BADGE_R - 1.0);
                        ui.add(egui::Label::new(RichText::new(format!("{}  ·  {}", s.name(), fmt_time(s.pts()))).size(theme::text::SMALL)).truncate());
                    });
                    ui.add_space(2.0);
                }
                let Some(t) = &s.scope_tex else {
                    ui.label(RichText::new("Measuring...").size(theme::text::SMALL).color(p.faint));
                    continue;
                };
                // The plot fills the panel's width at 16:10 (the vectorscope is
                // square, smaller when several are stacked), with a graticule
                // at 0 / 25 / 50 / 75 / 100%.
                let w = ui.available_width().max(40.0);
                let hgt = if scope == Scope::Vectorscope { if n > 1 { w * 0.7 } else { w } } else { w * 10.0 / 16.0 };
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, hgt), Sense::hover());
                let r = if scope == Scope::Vectorscope { Rect::from_center_size(r.center(), Vec2::splat(hgt)) } else { r };
                scope_plot(ui, p, r, t.id(), scope);
                ui.add_space(if n > 1 { theme::space::M } else { theme::space::S });
            }
            ui.label(RichText::new(match scope {
                Scope::Waveform => "Luma level (bottom 0%, top 100%) per image column.",
                Scope::Parade => "R, G, B waveforms side by side.",
                Scope::Vectorscope => "Chroma (BT.709). Boxes: 75% color targets; line: skin tone.",
                Scope::Histogram => match hist {
                    HistMode::Luminance => "Brightness distribution (black left, white right).",
                    HistMode::Rgb => "R, G and B distribution (black left, white right).",
                    HistMode::Both => "R, G, B and brightness distribution (black left, white right).",
                },
            }).size(theme::text::SMALL).color(p.muted));
        });
    }

    fn goto_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut target: Option<f64> = None;
        let lead = self.lead();
        let who = if self.sources.len() > 1 { format!(" ({})", letter(lead)) } else { String::new() };
        egui::Modal::new(egui::Id::unique("video-goto")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading(format!("Go to{who}"));
            ui.label(RichText::new("Frame number (1234), time (01:02.5 or 62.5s), or relative (+10f, -2s)").size(11.5));
            let Some(s) = self.goto.as_mut() else { return };
            let r = ui.add(egui::TextEdit::singleline(s).desired_width(f32::INFINITY));
            r.request_focus();
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(src) = self.sources.get(lead) {
                    target = parse_goto(s, src);
                }
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
        if self.goto.is_some() || self.save_dialog.is_some() || ctx.text_edit_focused() {
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
        let n = self.sources.len();
        let pair = n == 2;
        for (k, m) in events {
            if self.sources.is_empty() {
                if k == Key::Escape {
                    keep = false;
                }
                continue;
            }
            let lead = self.lead();
            let now = self.sources[lead].pts();
            match k {
                Key::Escape if self.was_editing => {}
                // Esc with a menu or a list open only closes it (egui does).
                Key::Escape if egui::Popup::is_any_open(ctx) => {}
                Key::Escape => keep = false,
                Key::Space | Key::K if !m.ctrl => _ = self.run_command(Command::PlayPause),
                Key::Period => _ = self.run_command(Command::NextFrame),
                Key::Comma => _ = self.run_command(Command::PrevFrame),
                Key::ArrowRight if m.ctrl => self.seek_all(now + 10.0, true),
                Key::ArrowLeft if m.ctrl => self.seek_all(now - 10.0, true),
                Key::ArrowRight if m.shift || self.playing => self.seek_all(now + 1.0, true),
                Key::ArrowLeft if m.shift || self.playing => self.seek_all(now - 1.0, true),
                Key::ArrowRight => self.step(1),
                Key::ArrowLeft => self.step(-1),
                Key::ArrowUp => self.seek_all(now + 60.0, true),
                Key::ArrowDown => self.seek_all(now - 60.0, true),
                Key::Home if m.shift => self.go_to_starts(),
                Key::Home => self.seek_all(0.0, true),
                Key::End => {
                    let c = self.sources[lead].frame_count().unwrap_or(1);
                    let t = self.sources[lead].pts_of(c.saturating_sub(1));
                    self.seek_all(t, true)
                }
                Key::PageDown | Key::PageUp => self.jump_keyframe(k == Key::PageDown),
                Key::G => self.goto = Some(String::new()),
                Key::I => _ = self.run_command(Command::ToggleInfo),
                Key::F2 => self.show_scopes = !self.show_scopes,
                Key::M => self.toggle_mute(),
                Key::L if m.ctrl => self.toggle_loop(),
                Key::L if n > 1 => self.toggle_sync(core),
                Key::P if n > 1 && !m.ctrl => self.set_start_here(core),
                Key::S if m.ctrl => self.open_save_dialog(),
                Key::C if m.ctrl => self.save_frames(SaveTarget::Clipboard),
                Key::OpenBracket => self.set_speed((self.speed / 2.0).max(0.1)),
                Key::CloseBracket => self.set_speed((self.speed * 2.0).min(4.0)),
                Key::Backspace => self.set_speed(1.0),
                Key::Num0 => _ = self.run_command(Command::ZoomFit),
                Key::Num1 => _ = self.run_command(Command::ZoomActual),
                Key::S if n > 1 && !m.ctrl => self.mode = CompareMode::SideBySide,
                Key::W if pair => self.mode = CompareMode::Wipe,
                Key::D if pair => self.mode = CompareMode::Difference,
                // Tab out of a text field (the Metrics tab's range) moves the
                // keyboard focus; it does not change the selected video then.
                Key::Tab if n > 1 && !self.was_editing && ctx.memory(|mm| mm.focused().is_none()) => {
                    let next = if m.shift { (self.selected + n - 1) % n } else { (self.selected + 1) % n };
                    self.select(next);
                }
                Key::X if pair => self.swap(),
                Key::F if pair => self.wipe_frozen = !self.wipe_frozen,
                _ => {}
            }
        }
        keep
    }

    /// Dev hook only: commands for video mode in `video_cmd.txt` of the dev
    /// folder (one per line): `add <path>`, `remove <i>`, `select <i>`,
    /// `sync linked|independent`, `start [<i>]`, `starts`, `gotostarts`,
    /// `audio <i>`, `mode sbs|wipe|diff`, `loop`, `play`, `pause`,
    /// `step <n>`, `seek <secs>`, `save <dir> [composite]`, `copy`,
    /// `compare`, `scope <0-3>`, `hist lum|rgb|both`, `scopes`, `vstate`.
    fn dev_commands(&mut self, core: &mut Core) {
        let Some(dir) = dev_dir() else { return };
        let f = dir.join("video_cmd.txt");
        let Ok(text) = std::fs::read_to_string(&f) else { return };
        let _ = std::fs::remove_file(&f);
        for line in text.lines() {
            let (cmd, arg) = line.trim().split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line.trim(), ""));
            let num = arg.parse::<usize>().unwrap_or(0);
            match cmd {
                "add" => _ = self.add_source(Path::new(arg), &core.ctx),
                "remove" => self.remove_source(num),
                "select" => self.select(num),
                "sync" => self.set_sync(if arg == "independent" { SyncMode::Independent } else { SyncMode::Linked }, core),
                "start" if arg.is_empty() => self.set_start_here(core),
                "start" => self.set_start(num, core),
                "starts" => self.set_all_starts(core),
                "gotostarts" => self.go_to_starts(),
                "autoalign" => self.start_auto_align(&core.ctx),
                "undoalign" => self.undo_auto_align(core),
                "timing" => {
                    for (i, s) in self.sources.iter().enumerate() {
                        let line = match s.timing() {
                            Some(r) => format!("timing {}:{} fps={:.3} vfr={} dropped={} repeated={} likely_dup={} stutter={} jitter_ms={:.3} verdict={}", letter(i), s.name(), r.measured_fps, r.vfr, r.dropped_frames, r.repeated_timestamps, r.likely_duplicates, r.stutter_events, r.jitter_std_ms, r.verdict),
                            None => format!("timing {}:{} pending", letter(i), s.name()),
                        };
                        dev_log(&line);
                    }
                }
                "audio" => self.set_audio_source(num),
                "mode" => {
                    self.mode = match arg {
                        "wipe" if self.sources.len() == 2 => CompareMode::Wipe,
                        "diff" if self.sources.len() == 2 => CompareMode::Difference,
                        _ => CompareMode::SideBySide,
                    }
                }
                "loop" => self.toggle_loop(),
                "play" => self.play(),
                "pause" => self.pause(),
                "step" => self.step(arg.parse::<i64>().unwrap_or(1)),
                "seek" => self.seek_all(arg.parse::<f64>().unwrap_or(0.0), true),
                "save" => {
                    let (d, comp) = match arg.strip_suffix(" composite") {
                        Some(d) => (d, true),
                        None => (arg, false),
                    };
                    self.save_frames(SaveTarget::Folder { dir: PathBuf::from(d), composite: comp });
                }
                "copy" => self.save_frames(SaveTarget::Clipboard),
                "compare" => self.open_in_compare(),
                "scope" => self.scope = Scope::ALL[num.min(3)],
                "hist" => {
                    self.hist_mode = match arg {
                        "lum" => HistMode::Luminance,
                        "rgb" => HistMode::Rgb,
                        _ => HistMode::Both,
                    }
                }
                "scopes" => self.show_scopes = !self.show_scopes,
                "info" => self.show_info = !self.show_info,
                "vstate" => {
                    let list: Vec<String> = self
                        .sources
                        .iter()
                        .enumerate()
                        .map(|(i, s)| format!("{}:{} pts={:.3} frame={} start={:?} out={:?} eof={} awaiting={}", letter(i), s.name(), s.pts(), s.frame_of(s.pts()), s.start.map(|t| (t * 1000.0).round() / 1000.0), s.out_size, s.eof, s.awaiting))
                        .collect();
                    dev_log(&format!(
                        "vstate playing={} sync={:?} lead={} audio={} mode={:?} loop={:?}/{} view={:?} {}",
                        self.playing,
                        self.sync,
                        self.lead(),
                        self.audio_src,
                        self.mode,
                        self.loop_ab,
                        self.loop_lead,
                        self.view,
                        list.join(" | ")
                    ));
                }
                _ => {}
            }
        }
        core.ctx.request_repaint();
    }

    /// Dev hook: log how long a burst of steps took to land, with a hash of
    /// the landed frame so tests can check frame accuracy.
    fn dev_report_step(&mut self) {
        let Some((t0, n)) = self.dev_burst else { return };
        let Some(a) = self.sources.get(self.lead()) else { return };
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
        let a = &self.sources[self.lead()];
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

/// A scope image in `r` with its graticule.
fn scope_plot(ui: &Ui, p: &Palette, r: Rect, tex: egui::TextureId, scope: Scope) {
    let cr = CornerRadius::same(theme::radius::CONTROL);
    let painter = ui.painter();
    painter.add(egui::epaint::RectShape::filled(r, cr, Color32::WHITE).with_texture(tex, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
    // The plots are always black, so the graticule is a dim white in both themes.
    let line = Stroke::new(1.0, Color32::from_white_alpha(38));
    let font = FontId::proportional(10.0);
    let label = Color32::from_white_alpha(150);
    match scope {
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

/// Decode each video's frame at `pts` at native resolution and save, copy
/// or prepare it for Compare.
fn save_frames_job(items: &[(PathBuf, f64, String)], target: &SaveTarget) -> Result<SaveDone, String> {
    let mut frames = Vec::with_capacity(items.len());
    for (i, (path, pts, _)) in items.iter().enumerate() {
        let img = engine::frame_at(path, *pts).map_err(|e| format!("Cannot decode {}'s frame: {e}", letter(i)))?;
        frames.push(img);
    }
    let names = sync::frame_file_names(&items.iter().map(|(_, t, stem)| (stem.clone(), *t)).collect::<Vec<_>>());
    let save_all = |dir: &Path| -> Result<Vec<PathBuf>, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        let mut out = Vec::new();
        for (img, name) in frames.iter().zip(&names) {
            let p = dir.join(name);
            img.save(&p).map_err(|e| format!("Cannot save {}: {e}", p.display()))?;
            out.push(p);
        }
        Ok(out)
    };
    let n = frames.len();
    match target {
        SaveTarget::Folder { dir, composite } => {
            let saved = save_all(dir)?;
            let mut msg = if n == 1 { format!("Saved {}", saved[0].display()) } else { format!("Saved {n} frames ({}) to {}", (0..n).map(letter).collect::<Vec<_>>().join(", "), dir.display()) };
            if *composite && n > 1 {
                if let Some(c) = sync::composite(&frames) {
                    let name = format!("{}_side-by-side_{}.png", sync::safe_stem(&items[0].2), sync::file_timecode(items[0].1));
                    let p = dir.join(name);
                    c.save(&p).map_err(|e| format!("Cannot save {}: {e}", p.display()))?;
                    msg += " and a side-by-side picture";
                }
            }
            Ok(SaveDone { message: msg, open: None })
        }
        SaveTarget::Clipboard => {
            let img = if n == 1 { frames.into_iter().next() } else { sync::composite(&frames) };
            let img = img.ok_or("Nothing to copy")?;
            crate::fileops::copy_image_to_clipboard(&img).map_err(|e| e.to_string())?;
            let msg = if n == 1 { "Frame copied to the clipboard".to_string() } else { format!("Copied {} side by side to the clipboard", (0..n).map(letter).collect::<Vec<_>>().join(", ")) };
            Ok(SaveDone { message: msg, open: None })
        }
        SaveTarget::Compare(dir) => {
            let saved = save_all(dir)?;
            Ok(SaveDone { message: format!("Opened {n} frames in Compare (saved in {})", dir.display()), open: Some(saved) })
        }
    }
}

/// Whether files can be created in `dir` (a media library or a read-only
/// share cannot take snapshots).
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".lumen-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The start point marker's color on the timeline.
const START_COLOR: Color32 = Color32::from_rgb(80, 200, 120);

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
/// columns (one per video), so a long value never squeezes another column.
/// File names stay on one line, ellipsized, with the full name on hover.
fn media_table(ui: &mut Ui, p: &Palette, cols: usize, rows: Vec<(&'static str, Vec<String>)>) {
    const LABEL_W: f32 = 96.0;
    const GAP: f32 = 8.0;
    let cols = cols.max(1);
    let compare = cols > 1;
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
            for i in 0..cols {
                cell(ui, value_w, &mut |ui| {
                    ui.label(RichText::new(letter(i)).strong().color(letter_color(p, i)));
                });
            }
        });
    }
    for (n, (k, vals)) in rows.into_iter().enumerate() {
        let mismatch = compare && vals.len() > 1 && vals.iter().any(|v| *v != vals[0]);
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
            idx.push(IndexEntry { pts: (i as f64 + 0.5) / 25.0, size: if key { 100_000 } else { 10_000 }, key, kind: if key { b'I' } else { b'?' }, repeats: 0 });
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
        let small = [IndexEntry { pts: 0.0, size: 50, key: true, kind: b'I', repeats: 0 }, IndexEntry { pts: 1.0, size: 10, key: false, kind: b'B', repeats: 0 }];
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
