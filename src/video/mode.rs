//! Video mode UI: playback, frame-accurate stepping, A/B comparison with
//! metrics, scopes, frame index timeline and media info.

use super::audio::AudioOut;
use super::engine::{self, Cmd, Event, IndexEntry, MediaInfo, Player, VFrame};
use super::metrics::{self, FrameMetrics, YuvFrame};
use super::scopes::{self, Scope};
use crate::app::Core;
use crate::ui::icons as I;
use crate::ui::theme::Palette;
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
    message: Option<String>,
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
    /// Dev hook only: start time and press count of the current step burst.
    dev_burst: Option<(Instant, u32)>,
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
            message: None,
            pending_swap: None,
            snap_rx: None,
            timeline_cache: None,
            pending_step: None,
            was_editing: false,
            offset_field: None,
            dev_burst: None,
        };
        for p in paths.into_iter().take(2) {
            vm.add_source(&p, &core.ctx, &mut None);
        }
        vm
    }

    fn add_source(&mut self, path: &Path, ctx: &egui::Context, err: &mut Option<String>) {
        if self.sources.len() >= 2 {
            self.sources.truncate(1);
        }
        let audio = if self.sources.is_empty() { self.audio.as_ref().map(|a| a.shared.clone()) } else { None };
        match Source::open(path, audio, ctx) {
            Ok(mut s) => {
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
                let msg = format!("{}: {e:#}", path.display());
                *err = Some(msg.clone());
                self.message = Some(msg);
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
                self.add_source(&d, &ctx, &mut None);
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
        self.dev_report_step();
        if self.flicker_auto && self.flicker_at.elapsed() > Duration::from_millis(500) {
            self.flicker_b = !self.flicker_b;
            self.flicker_at = Instant::now();
        }
        if self.flicker_auto {
            ctx.request_repaint_after(Duration::from_millis(100));
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
            ui.label(RichText::new(format!("{}  Video", I::FILM_STRIP)).font(crate::ui::theme::semibold(15.0)));
            widgets::separator_v(ui);
            for (i, label) in ["A", "B"].iter().enumerate() {
                let name = self.sources.get(i).map(|s| crate::ui::viewer::file_name(&s.path)).unwrap_or_else(|| if i == 0 { "Open video...".into() } else { "Add B to compare...".into() });
                let color = if i == 0 { Color32::from_rgb(255, 170, 60) } else { Color32::from_rgb(90, 190, 255) };
                ui.label(RichText::new(*label).strong().color(color));
                let resp = ui.add(egui::Button::new(RichText::new(name).size(12.5)).min_size(Vec2::new(60.0, 24.0))).on_hover_text("Choose a file");
                if resp.clicked() {
                    let exts: Vec<&str> = super::VIDEO_EXTS.to_vec();
                    if let Some(f) = rfd::FileDialog::new().add_filter("Videos", &exts).add_filter("All files", &["*"]).pick_file() {
                        if i == 0 {
                            self.sources.clear();
                        } else {
                            self.sources.truncate(1);
                        }
                        self.add_source(&f, &core.ctx, &mut None);
                    }
                }
                if i == 1 && self.sources.len() == 2 && widgets::icon_button(ui, I::X, "Remove B", false).clicked() {
                    self.sources.truncate(1);
                    self.sources[0].player.send(Cmd::Native(false));
                }
            }
            if self.sources.len() == 2 {
                widgets::separator_v(ui);
                for m in CompareMode::ALL {
                    if widgets::icon_button(ui, m.icon(), &format!("{} ({})", m.label(), m.key()), self.mode == m).clicked() {
                        self.mode = m;
                    }
                }
                if self.mode == CompareMode::Difference {
                    ui.label(RichText::new("Gain").color(p.muted).size(12.0));
                    ui.spacing_mut().slider_width = 90.0;
                    ui.add(egui::Slider::new(&mut self.gain, 1.0..=64.0).logarithmic(true).suffix("×"));
                }
                widgets::separator_v(ui);
                ui.label(RichText::new("B offset").color(p.muted).size(12.0));
                let mut off = self.offset_frames;
                let r = ui.add(egui::DragValue::new(&mut off).suffix(" f").speed(0.2)).on_hover_text("Shift B in frames (+ / - keys; Ctrl x10)");
                self.offset_field = Some(r.id);
                if r.changed() {
                    self.set_offset(off);
                }
                if widgets::icon_button(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B (X)", false).clicked() {
                    self.swap();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, I::INFO, "Info & metrics (I)", self.show_info).clicked() {
                    self.show_info = !self.show_info;
                }
                if widgets::icon_button(ui, I::WAVEFORM, "Scopes (F2)", self.show_scopes).clicked() {
                    self.show_scopes = !self.show_scopes;
                }
                if widgets::icon_button(ui, I::CAMERA, "Save frame as PNG (Ctrl+S)", false).clicked() {
                    self.snapshot(false, core);
                }
            });
        });
        keep
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
            self.add_source(&a, &core.ctx, &mut None);
            self.add_source(&b, &core.ctx, &mut None);
            self.seek_all(t, true);
        }
        let p = core.pal;
        let view = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(view, Sense::click_and_drag());
        self.hover_view = resp.hovered();
        if self.sources.is_empty() {
            let c = view.center();
            ui.painter().text(c - Vec2::new(0.0, 40.0), Align2::CENTER_CENTER, I::FILM_STRIP, FontId::proportional(56.0), p.faint);
            ui.painter().text(c + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, "Drop one video to play it, or two videos to compare them", FontId::proportional(15.0), p.text);
            ui.painter().text(c + Vec2::new(0.0, 36.0), Align2::CENTER_CENTER, "Frame-accurate stepping · wipe / flicker / difference · PSNR & SSIM · scopes", FontId::proportional(12.5), p.muted);
            if let Some(m) = &self.message {
                ui.painter().text(c + Vec2::new(0.0, 70.0), Align2::CENTER_CENTER, m, FontId::proportional(12.0), p.danger);
            }
            let br = Rect::from_center_size(c + Vec2::new(0.0, 110.0), Vec2::new(200.0, 34.0));
            if ui.put(br, egui::Button::new(RichText::new(format!("{}  Open video...", I::FOLDER_OPEN)).color(Color32::WHITE)).fill(p.accent)).clicked() {
                let exts: Vec<&str> = super::VIDEO_EXTS.to_vec();
                if let Some(files) = rfd::FileDialog::new().add_filter("Videos", &exts).pick_files() {
                    for f in files.into_iter().take(2) {
                        self.add_source(&f, &core.ctx, &mut None);
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
        if resp.clicked() && !compare {
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
        if !compare {
            let dest = dest_in(view, self.center, self.zoom);
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
                        draw_tex(&painter, s, dest, r, Color32::WHITE);
                    }
                    painter.line_segment([Pos2::new(view.center().x, view.min.y), Pos2::new(view.center().x, view.max.y)], Stroke::new(1.0, Color32::from_gray(60)));
                }
                CompareMode::Wipe => {
                    let dest = dest_in(view, self.center, self.zoom);
                    if !self.wipe_frozen && !ui.input(|i| i.modifiers.shift) {
                        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                            if view.contains(pos) {
                                self.wipe_pos = ((pos.x - view.min.x) / view.width()).clamp(0.0, 1.0);
                            }
                        }
                    }
                    let x = view.min.x + view.width() * self.wipe_pos;
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
                    draw_tex(&painter, if show_b { b } else { a }, dest, view, Color32::WHITE);
                    let label = if show_b { "B" } else { "A" };
                    let col = if show_b { Color32::from_rgb(90, 190, 255) } else { Color32::from_rgb(255, 170, 60) };
                    let r = Rect::from_min_size(view.min + Vec2::new(14.0, 14.0), Vec2::new(36.0, 30.0));
                    painter.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(170));
                    painter.text(r.center(), Align2::CENTER_CENTER, label, crate::ui::theme::semibold(18.0), col);
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Overlay => {
                    let dest = dest_in(view, self.center, self.zoom);
                    draw_tex(&painter, a, dest, view, Color32::WHITE);
                    draw_tex(&painter, b, dest, view, Color32::from_white_alpha(128));
                    self.pixel_probe(ui, dest, (dw, dh));
                }
                CompareMode::Checker => {
                    let dest = dest_in(view, self.center, self.zoom);
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
                    self.update_diff(ui.ctx());
                    if let Some((_, t)) = &self.diff_tex {
                        let mut mesh = egui::Mesh::with_texture(t.id());
                        mesh.add_rect_with_uv(dest, full_uv, Color32::WHITE);
                        rotate_mesh(&mut mesh, dest, self.sources[0].info.rotation);
                        painter.add(egui::Shape::mesh(mesh));
                    }
                    if let Some((mean, max)) = self.diff_stats {
                        let txt = format!("|A-B| x {:.0}   mean {:.2}   max {:.0}", self.gain, mean, max);
                        let g = painter.layout_no_wrap(txt, FontId::proportional(12.0), Color32::WHITE);
                        let r = Rect::from_min_size(view.min + Vec2::new(14.0, 14.0), g.size() + Vec2::new(16.0, 8.0));
                        painter.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(170));
                        painter.galley(r.min + Vec2::new(8.0, 4.0), g, Color32::WHITE);
                    }
                    self.pixel_probe(ui, dest, (dw, dh));
                }
            }
            // Pane labels
            let labels: Vec<(Pos2, &str, Color32)> = match self.mode {
                CompareMode::SideBySide => vec![(pane(0).left_top(), "A", Color32::from_rgb(255, 170, 60)), (pane(1).left_top(), "B", Color32::from_rgb(90, 190, 255))],
                CompareMode::Wipe => vec![(view.left_top(), "A", Color32::from_rgb(255, 170, 60)), (Pos2::new(view.max.x - 50.0, view.min.y), "B", Color32::from_rgb(90, 190, 255))],
                _ => vec![],
            };
            for (pos, l, c) in labels {
                let r = Rect::from_min_size(pos + Vec2::new(14.0, 14.0), Vec2::new(30.0, 26.0));
                painter.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(170));
                painter.text(r.center(), Align2::CENTER_CENTER, l, crate::ui::theme::semibold(15.0), c);
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
        let mut hud = format!("{}   frame {}   {}   {:.0}%", fmt_time(a.pts()), f, kind_name(kind), self.zoom * 100.0);
        if let Some(e) = &a.error {
            hud = format!("{hud}   ⚠ {e}");
        }
        let g = painter.layout_no_wrap(hud, FontId::monospace(12.0), Color32::from_white_alpha(230));
        let r = Rect::from_min_size(Pos2::new(view.max.x - g.size().x - 30.0, view.max.y - 36.0), g.size() + Vec2::new(16.0, 8.0));
        painter.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(150));
        painter.galley(r.min + Vec2::new(8.0, 4.0), g, Color32::WHITE);
        // A slow seek (long GOP) keeps the last frame on screen with a badge,
        // so stepping never looks frozen.
        if let Some(t0) = a.seek_started {
            let delay = Duration::from_millis(150);
            if t0.elapsed() >= delay {
                let txt = match self.pending_step {
                    Some(f) => format!("Seeking to frame {f}..."),
                    None => "Seeking...".to_string(),
                };
                let g = painter.layout_no_wrap(txt, FontId::proportional(13.0), Color32::WHITE);
                let r = Rect::from_center_size(Pos2::new(view.center().x, view.min.y + 34.0), g.size() + Vec2::new(24.0, 12.0));
                painter.rect_filled(r, CornerRadius::same(8), Color32::from_black_alpha(190));
                painter.galley(r.min + Vec2::new(12.0, 6.0), g, Color32::WHITE);
            } else {
                ui.ctx().request_repaint_after(delay - t0.elapsed());
            }
        }
        if self.audio.is_none() {
            if let Some(e) = &self.audio_error {
                painter.text(view.left_bottom() + Vec2::new(14.0, -20.0), Align2::LEFT_CENTER, format!("{}  No audio output: {e}", I::SPEAKER_SLASH), FontId::proportional(11.5), p.muted);
            }
        }
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

    fn transport(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        if self.sources.is_empty() {
            ui.add_space(4.0);
            return;
        }
        self.timeline(ui, &p);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let b = |ui: &mut Ui, icon: &str, tip: &str| widgets::icon_button(ui, icon, tip, false).clicked();
            if b(ui, I::SKIP_BACK, "First frame (Home)") {
                self.seek_all(0.0, true);
            }
            if b(ui, I::CARET_DOUBLE_LEFT, "Back 1 s (Shift+←)") {
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
            if b(ui, I::CARET_DOUBLE_RIGHT, "Forward 1 s (Shift+→)") {
                let t = self.sources[0].pts() + 1.0;
                self.seek_all(t, true);
            }
            if b(ui, I::SKIP_FORWARD, "Last frame (End)") {
                let n = self.sources[0].frame_count().unwrap_or(1);
                let t = self.sources[0].pts_of(n.saturating_sub(1));
                self.seek_all(t, true);
            }
            widgets::separator_v(ui);
            let a = &self.sources[0];
            let total = a.frame_count().map(|n| n.to_string()).unwrap_or_else(|| "...".into());
            ui.label(RichText::new(format!("{} / {}", fmt_time(a.pts()), fmt_time(a.duration()))).monospace().color(p.text));
            ui.add_space(6.0);
            let fr = ui.add(egui::Button::new(RichText::new(format!("frame {} / {}", a.frame_of(a.pts()), total)).monospace().size(12.0)).frame(false)).on_hover_text("Go to frame or time (G)");
            if fr.clicked() {
                self.goto = Some(String::new());
            }
            widgets::separator_v(ui);
            // A-B loop
            let lab = match self.loop_ab {
                (None, _) => "Loop A-B",
                (Some(_), None) => "Set B",
                _ => "Clear loop",
            };
            if ui.add(egui::Button::new(RichText::new(format!("{} {lab}", I::REPEAT)).size(12.0)).selected(self.loop_ab.1.is_some())).on_hover_text("Ctrl+L").clicked() {
                self.toggle_loop();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Volume
                ui.spacing_mut().slider_width = 90.0;
                if self.audio.is_some() && self.sources[0].info.has_audio {
                    if ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.5).show_value(false)).changed() {
                        if let Some(a) = self.audio_shared() {
                            a.set_volume(self.volume);
                        }
                    }
                    let vi = if self.muted || self.volume == 0.0 { I::SPEAKER_SLASH } else { I::SPEAKER_HIGH };
                    if widgets::icon_button(ui, vi, "Mute (M)", self.muted).clicked() {
                        self.toggle_mute();
                    }
                } else {
                    ui.label(RichText::new(format!("{} no audio", I::SPEAKER_SLASH)).size(11.5).color(p.faint));
                }
                widgets::separator_v(ui);
                egui::ComboBox::from_id_salt("speed").width(70.0).selected_text(format!("{}×", self.speed)).show_ui(ui, |ui| {
                    for s in [0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 4.0] {
                        if ui.selectable_label(self.speed == s, format!("{s}×")).clicked() {
                            self.set_speed(s);
                        }
                    }
                });
                ui.label(RichText::new("Speed").size(12.0).color(p.muted));
            });
        });
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
        let h = 58.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click_and_drag());
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(16, 17, 20));
        let a = &self.sources[0];
        let dur = a.duration().max(0.001);
        let x_of = |t: f64| rect.min.x + (t / dur) as f32 * rect.width();
        // Loop region
        if let (Some(la), lb) = self.loop_ab {
            let lb = lb.unwrap_or(a.pts());
            painter.rect_filled(Rect::from_min_max(Pos2::new(x_of(la), rect.min.y), Pos2::new(x_of(lb), rect.max.y)), 0.0, p.accent.gamma_multiply(0.18));
        }
        // Frame-size bars colored by type (StreamEye-style), aggregated per pixel column.
        let idx_guard = a.index.lock();
        if let Some(idx) = idx_guard.as_ref() {
            let cols = rect.width().max(1.0) as usize;
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
            let bar_h = h - 18.0;
            let base = rect.max.y - 4.0;
            let mut mesh = egui::Mesh::default();
            for (c, col) in columns.iter().enumerate() {
                if col.height <= 0.0 {
                    continue;
                }
                // Stack the frame types bottom-up by their share of the column's bytes.
                let x = rect.min.x + c as f32;
                let mut y = base;
                for (share, color) in col.shares.iter().zip(TIMELINE_COLORS) {
                    if *share <= 0.0 {
                        continue;
                    }
                    let top = y - share * col.height * bar_h;
                    mesh.add_colored_rect(Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + 1.0, y)), color.gamma_multiply(0.85));
                    y = top;
                }
            }
            painter.add(egui::Shape::mesh(mesh));
        } else {
            let prog = f32::from_bits(a.index_progress.load(Ordering::Relaxed));
            if prog >= 1.0 {
                painter.text(rect.center(), Align2::CENTER_CENTER, "Frame index unavailable for this file", FontId::proportional(11.5), p.faint);
            } else {
                painter.text(rect.center(), Align2::CENTER_CENTER, format!("Indexing frames... {:.0}%", prog * 100.0), FontId::proportional(11.5), p.faint);
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
                    .step_by((rows.len() / rect.width().max(1.0) as usize).max(1))
                    .map(|(i, m)| {
                        let t = a.pts_of(*i);
                        let v = ((m.psnr_y.min(60.0) - 20.0) / 40.0).clamp(0.0, 1.0) as f32;
                        Pos2::new(x_of(t), rect.max.y - 4.0 - v * (h - 10.0))
                    })
                    .collect();
                painter.add(egui::Shape::line(pts, Stroke::new(1.5, Color32::from_rgb(250, 210, 80))));
            }
        }
        // Playhead (A) and B position.
        let x = x_of(a.pts());
        painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(2.0, Color32::WHITE));
        painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x - 5.0, rect.min.y), Pos2::new(x + 5.0, rect.min.y), Pos2::new(x, rect.min.y + 7.0)], Color32::WHITE, Stroke::NONE));
        // Hover tooltip + scrubbing.
        if let Some(pos) = resp.hover_pos() {
            let t = (((pos.x - rect.min.x) / rect.width()) as f64 * dur).clamp(0.0, dur);
            let f = a.frame_of(t);
            let e = a.entry(f);
            painter.line_segment([Pos2::new(pos.x, rect.min.y), Pos2::new(pos.x, rect.max.y)], Stroke::new(1.0, Color32::from_white_alpha(90)));
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
                let t = (((pos.x - rect.min.x) / rect.width()) as f64 * dur).clamp(0.0, dur);
                self.scrubbing = resp.dragged();
                // Keyframe seeks while dragging (fast), exact on release / click.
                self.seek_all(t, !resp.dragged());
            }
        }
        if resp.drag_stopped() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = (((pos.x - rect.min.x) / rect.width()) as f64 * dur).clamp(0.0, dur);
                self.seek_all(t, true);
            }
            self.scrubbing = false;
        }
        // Legend
        let legend = [("I", TIMELINE_COLORS[0]), ("P", TIMELINE_COLORS[1]), ("B", TIMELINE_COLORS[2])];
        let mut lx = rect.min.x + 8.0;
        for (l, c) in legend {
            painter.rect_filled(Rect::from_min_size(Pos2::new(lx, rect.min.y + 6.0), Vec2::new(8.0, 8.0)), CornerRadius::same(2), c);
            painter.text(Pos2::new(lx + 11.0, rect.min.y + 10.0), Align2::LEFT_CENTER, l, FontId::proportional(10.0), p.muted);
            lx += 26.0;
        }
        if self.analysis.is_some() {
            painter.rect_filled(Rect::from_min_size(Pos2::new(lx, rect.min.y + 9.0), Vec2::new(12.0, 2.0)), 0.0, Color32::from_rgb(250, 210, 80));
            painter.text(Pos2::new(lx + 15.0, rect.min.y + 10.0), Align2::LEFT_CENTER, "PSNR-Y (20-60 dB)", FontId::proportional(10.0), p.muted);
        }
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
                        widgets::kv_row(ui, &p, "SSIM Y", &format!("{:.5}  ({})", m.ssim_y, fmt_db(metrics::ssim_db(m.ssim_y))));
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
                            widgets::kv_row(ui, &p, "Mean SSIM", &format!("{:.5}", sum.mean_ssim));
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
            let rows = media_rows(&self.sources.iter().map(|s| &s.info).collect::<Vec<_>>());
            media_table(ui, &p, compare, rows);
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Decoder: FFmpeg {}", engine::ffmpeg_version())).size(10.5).color(p.faint));
        });
    }

    fn scopes_panel(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal_wrapped(|ui| {
            for s in Scope::ALL {
                if ui.selectable_label(self.scope == s, s.label()).clicked() {
                    self.scope = s;
                    self.scope_tex = None;
                }
            }
        });
        ui.add_space(6.0);
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
            let ts = t.size_vec2();
            let w = ui.available_width();
            let hgt = if self.scope == Scope::Vectorscope { w } else { (w * ts.y / ts.x).min(w * 0.8) };
            let (r, _) = ui.allocate_exact_size(Vec2::new(w, hgt), Sense::hover());
            let mut mesh = egui::Mesh::with_texture(t.id());
            mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            ui.painter().add(egui::Shape::mesh(mesh));
            ui.painter().rect_stroke(r, 0.0, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        }
        ui.add_space(4.0);
        ui.label(RichText::new(match self.scope {
            Scope::Waveform => "Luma level (bottom 0%, top 100%) per image column.",
            Scope::Parade => "R, G, B waveforms side by side.",
            Scope::Vectorscope => "Chroma (BT.709). Boxes: 75% color targets; line: skin tone.",
            Scope::Histogram => "R, G, B and luma distribution.",
        }).size(11.0).color(p.faint));
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
                Key::Escape => keep = false,
                Key::Space | Key::K if !m.ctrl => self.toggle_play(),
                Key::Period => self.step(1),
                Key::Comma => self.step(-1),
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
                Key::I => self.show_info = !self.show_info,
                Key::F2 => self.show_scopes = !self.show_scopes,
                Key::M => self.toggle_mute(),
                Key::L if m.ctrl => self.toggle_loop(),
                Key::S if m.ctrl => self.snapshot(false, core),
                Key::C if m.ctrl => self.snapshot(true, core),
                Key::OpenBracket => self.set_speed((self.speed / 2.0).max(0.1)),
                Key::CloseBracket => self.set_speed((self.speed * 2.0).min(4.0)),
                Key::Backspace => self.set_speed(1.0),
                Key::Num0 => self.auto_fit = true,
                Key::Num1 => {
                    self.auto_fit = false;
                    self.zoom = 1.0;
                }
                Key::S if compare => self.mode = CompareMode::SideBySide,
                Key::W if compare => self.mode = CompareMode::Wipe,
                Key::D if compare => self.mode = CompareMode::Difference,
                Key::O if compare => self.mode = CompareMode::Overlay,
                Key::C if compare => self.mode = CompareMode::Checker,
                Key::Tab if compare => {
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

fn media_rows(infos: &[&MediaInfo]) -> Vec<(&'static str, Vec<String>)> {
    let f = |g: &dyn Fn(&MediaInfo) -> String| infos.iter().map(|i| g(i)).collect::<Vec<_>>();
    let br = |b: i64| if b > 0 { format!("{:.2} Mb/s", b as f64 / 1e6) } else { " - ".into() };
    vec![
        ("File", f(&|i| crate::ui::viewer::file_name(&i.path))),
        ("Size", f(&|i| crate::catalog::human_size(i.file_size))),
        ("Container", f(&|i| i.container.clone())),
        ("Duration", f(&|i| fmt_time(i.duration))),
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
    ]
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
                ui.label(RichText::new("A").strong().color(Color32::from_rgb(255, 170, 60)));
            });
            cell(ui, value_w, &mut |ui| {
                ui.label(RichText::new("B").strong().color(Color32::from_rgb(90, 190, 255)));
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

/// Timeline colors for I, P, B and not yet known frame types.
const TIMELINE_COLORS: [Color32; 4] = [Color32::from_rgb(235, 90, 80), Color32::from_rgb(80, 150, 255), Color32::from_rgb(80, 200, 120), Color32::from_rgb(110, 116, 128)];

/// One pixel column of the frame-size timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct TimelineColumn {
    /// Mean frame size in the column relative to the 98th percentile (0..=1).
    height: f32,
    /// Share of the column's bytes in I, P, B and unknown frames.
    shares: [f32; 4],
}

/// Aggregate the frame index into `cols` columns. Heights follow the mean
/// frame size (so one large I-frame does not turn a column of a long film
/// into a full red bar) and each bar is split by the bytes of each type.
fn timeline_columns(idx: &[IndexEntry], dur: f64, cols: usize) -> Vec<TimelineColumn> {
    let cols = cols.max(1);
    let mut sums = vec![([0u64; 4], 0u32); cols];
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
        .map(|((bytes, _), mean)| {
            let total = bytes.iter().sum::<u64>().max(1) as f32;
            TimelineColumn { height: (mean / p98.max(1.0)).min(1.0) as f32, shares: bytes.map(|b| b as f32 / total) }
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

/// dB value for display, capped like the CSV export (see `metrics::PSNR_CAP_DB`).
fn fmt_db(v: f64) -> String {
    if v.is_nan() {
        " - ".into()
    } else if v.is_infinite() {
        format!("{:.2} dB (identical)", metrics::PSNR_CAP_DB)
    } else {
        format!("{:.3} dB", metrics::cap_db(v))
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
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(62.5), "01:02.500");
        assert_eq!(fmt_time(3723.25), "1:02:03.250");
    }
}
