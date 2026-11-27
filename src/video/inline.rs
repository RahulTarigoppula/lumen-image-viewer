//! Videos played inside the viewer and the slideshow (as FastStone does):
//! one decoder per file on its own thread (the engine video mode uses),
//! frames uploaded into one reused texture, paced by the sound card's clock
//! when the video has sound and by the system clock otherwise.
//!
//! [`InlinePlayer`] plays one file. [`InlineVideo`] is the viewer's player
//! with its sound output and transport bar. The slideshow uses players
//! directly. Nothing here blocks the UI thread: files open on a thread, and
//! so does the sound output.

use super::audio::{AudioOut, AudioShared};
use super::engine::{self, Cmd, Event, MediaInfo, Player, VFrame};
use super::playback::{self, PlayClock, SKIP_SECS};
use super::sync;
use crate::settings::VideoSettings;
use crate::ui::commands::Command;
use crate::ui::icons as I;
use crate::ui::theme::{self, Palette};
use crate::ui::{tips, widgets};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use egui::{Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Decoded frames one inline player keeps for instant steps back (video
/// mode keeps more).
const CACHE_BYTES: usize = 160 << 20;
/// Frames taken from the decoder ahead of the clock.
const MAX_QUEUE: usize = 8;
/// How long a new view size must hold before frames are decoded at it.
const SETTLE: Duration = Duration::from_millis(250);

/// Where a player's sound goes.
pub type Sound = Arc<AudioShared>;

// ---------------------------------------------------------------- sound

/// A sound output opened on its own thread (opening the device can take a
/// moment) and closed when this is dropped.
pub struct SoundOut {
    rx: Option<Receiver<Result<Sound, String>>>,
    shared: Option<Sound>,
    error: Option<String>,
    /// Dropping it ends the thread that holds the stream.
    _alive: Sender<()>,
}

impl SoundOut {
    pub fn open(ctx: &egui::Context) -> SoundOut {
        let (tx, rx) = crossbeam_channel::bounded::<Result<Sound, String>>(1);
        let (alive, alive_rx) = crossbeam_channel::bounded::<()>(1);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("lumen-sound".into()).spawn(move || match AudioOut::open() {
            Ok(out) => {
                let _ = tx.send(Ok(out.shared.clone()));
                ctx.request_repaint();
                // Hold the stream until the owner is gone.
                let _ = alive_rx.recv();
                drop(out);
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                ctx.request_repaint();
            }
        });
        let error = spawned.err().map(|e| format!("The sound output could not start: {e}"));
        let rx = if error.is_none() { Some(rx) } else { None };
        SoundOut { rx, shared: None, error, _alive: alive }
    }

    /// The output once it is open (None while it opens, or without a device).
    pub fn shared(&mut self) -> Option<Sound> {
        let Some(got) = self.rx.as_ref().map(|rx| rx.try_recv()) else { return self.shared.clone() };
        match got {
            Ok(Ok(s)) => {
                self.shared = Some(s);
                self.rx = None;
            }
            Ok(Err(e)) => {
                self.error = Some(e);
                self.rx = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some("No sound output".into());
                self.rx = None;
            }
            Err(TryRecvError::Empty) => {}
        }
        self.shared.clone()
    }

    /// Volume (1.0 as recorded) and mute.
    pub fn set_level(&self, volume: f32, muted: bool) {
        if let Some(a) = &self.shared {
            a.set_volume(volume);
            a.muted.store(muted, Ordering::Relaxed);
        }
    }

    /// Why there is no sound, when the output could not be opened.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

// ---------------------------------------------------------------- one file

/// What the opening thread hands over.
type Opened = Result<(MediaInfo, Player), String>;

enum State {
    /// Probing the file and starting its decoder (on a thread).
    Opening(Receiver<Opened>),
    Open,
    Failed(String),
}

/// One video file playing inline: its decoder, the frames it sent, the
/// clock and the texture the frame on screen is in.
pub struct InlinePlayer {
    path: PathBuf,
    state: State,
    info: Option<MediaInfo>,
    player: Option<Player>,
    /// Seek generation: frames of older seeks are dropped.
    gen: u64,
    current: Option<VFrame>,
    queue: VecDeque<VFrame>,
    tex: Option<egui::TextureHandle>,
    /// (frame time, frame buffer, nearest filter) of the texture's contents.
    tex_key: (u64, usize, bool),
    clock: PlayClock,
    playing: bool,
    /// Play as soon as the decoder is open.
    autoplay: bool,
    eof: bool,
    /// A seek's frame is on its way.
    awaiting: bool,
    seek_started: Option<Instant>,
    /// Played to the end (and does not repeat).
    ended: bool,
    /// Start again at the end.
    pub repeat: bool,
    audio: Option<Sound>,
    /// The sound changed while the file was opening: tell the decoder once open.
    audio_changed: bool,
    /// The audio clock's last value and when it changed (a stalled clock,
    /// a sound track shorter than the picture, is ignored).
    audio_seen: Option<(f64, Instant)>,
    out_size: (u32, u32),
    out_wait: sync::Debounce<(u32, u32)>,
    /// Where a burst of frame steps is heading, until its frame arrives.
    pending_step: Option<f64>,
    born: Instant,
}

impl InlinePlayer {
    /// Start opening `path` on a thread. `audio`: the output its sound plays
    /// into (None: silent). `autoplay`: play once it is open.
    pub fn open(path: &Path, ctx: &egui::Context, audio: Option<Sound>, autoplay: bool) -> InlinePlayer {
        let (tx, rx) = crossbeam_channel::bounded::<Opened>(1);
        let (p, c, a) = (path.to_path_buf(), ctx.clone(), audio.clone());
        let spawned = std::thread::Builder::new().name("lumen-video-open".into()).spawn(move || {
            let r = engine::probe(&p).and_then(|info| {
                let index: engine::SharedIndex = Arc::new(parking_lot::Mutex::new(None));
                Player::open_with_cache(&p, a, index, c.clone(), CACHE_BYTES).map(|player| (info, player))
            });
            // When the player is gone already, the decoder is dropped (and stops) here.
            let _ = tx.send(r.map_err(|e| super::mode::open_reason(&e)));
            c.request_repaint();
        });
        let state = match spawned {
            Ok(_) => State::Opening(rx),
            Err(e) => State::Failed(format!("The video decoder could not start: {e}.")),
        };
        if let Some(a) = &audio {
            a.playing.store(false, Ordering::Relaxed);
            a.flush();
        }
        InlinePlayer {
            path: path.to_path_buf(),
            state,
            info: None,
            player: None,
            gen: 0,
            current: None,
            queue: VecDeque::new(),
            tex: None,
            tex_key: (u64::MAX, 0, false),
            clock: PlayClock::default(),
            playing: false,
            autoplay,
            eof: false,
            awaiting: true,
            seek_started: Some(Instant::now()),
            ended: false,
            repeat: false,
            audio,
            audio_changed: false,
            audio_seen: None,
            out_size: (2, 2),
            out_wait: Default::default(),
            pending_step: None,
            born: Instant::now(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why the file cannot be played.
    pub fn error(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// A picture is there to show.
    pub fn has_frame(&self) -> bool {
        self.current.is_some()
    }

    /// Time since the player was made.
    pub fn age(&self) -> Duration {
        self.born.elapsed()
    }

    /// Playing, or about to once the file is open.
    pub fn playing(&self) -> bool {
        self.playing || (self.autoplay && self.player.is_none() && self.error().is_none())
    }

    /// Played to its end (and does not repeat).
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// The file has a sound track.
    pub fn has_audio(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.has_audio)
    }

    fn rotation(&self) -> i32 {
        self.info.as_ref().map(|i| i.rotation).unwrap_or(0)
    }

    /// The size the picture shows at, upright (phone videos are rotated),
    /// once the file is open.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        let i = self.info.as_ref()?;
        let (w, h) = (i.width.max(1), i.height.max(1));
        Some(if i.rotation.rem_euclid(180) == 90 { (h, w) } else { (w, h) })
    }

    fn fps(&self) -> f64 {
        match &self.info {
            Some(i) if i.avg_fps > 0.0 && i.avg_fps <= 1000.0 => i.avg_fps,
            Some(i) if i.fps > 0.0 && i.fps <= 1000.0 => i.fps,
            _ => 25.0,
        }
    }

    pub fn frame_dur(&self) -> f64 {
        1.0 / self.fps()
    }

    /// Length in seconds (the container's; 0 until the file is open).
    pub fn duration(&self) -> f64 {
        let d = self.info.as_ref().map(|i| i.duration).filter(|d| d.is_finite()).unwrap_or(0.0).max(0.0);
        d.max(self.pts())
    }

    /// The last moment a seek can target.
    fn last_pts(&self) -> f64 {
        (self.duration() - 0.001).max(0.0)
    }

    /// Time of the frame on screen.
    pub fn pts(&self) -> f64 {
        self.current.as_ref().map(|f| f.pts).unwrap_or(0.0)
    }

    /// Where the playhead is: the frame on screen, or where a seek or a
    /// burst of frame steps is heading.
    pub fn position(&self) -> f64 {
        if self.awaiting && self.player.is_some() {
            self.pending_step.unwrap_or(self.clock.base())
        } else {
            self.pts()
        }
    }

    /// Frame number (from 0) of the frame on screen, from the frame rate.
    pub fn frame_number(&self) -> u64 {
        let f = (self.position() * self.fps()).round();
        if f.is_finite() && f > 0.0 {
            f as u64
        } else {
            0
        }
    }

    fn at_end(&self) -> bool {
        self.eof && self.queue.is_empty()
    }

    fn send(&self, c: Cmd) {
        if let Some(p) = &self.player {
            p.send(c);
        }
    }

    /// Let the sound output play (while this video plays and has sound).
    fn sound_on(&self, on: bool) {
        if let Some(a) = &self.audio {
            a.playing.store(on && self.has_audio(), Ordering::Relaxed);
        }
    }

    /// Take what the decoder sent and move the picture on with the clock.
    /// Call once a frame, before painting.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.poll_open();
        self.poll();
        self.advance(ctx);
    }

    fn poll_open(&mut self) {
        let State::Opening(rx) = &self.state else { return };
        let got = rx.try_recv();
        match got {
            Ok(Ok((info, player))) => {
                self.out_size = (info.width.max(2), info.height.max(2));
                self.info = Some(info);
                self.player = Some(player);
                self.state = State::Open;
                if self.audio_changed {
                    self.audio_changed = false;
                    self.send(Cmd::Audio(self.audio.clone()));
                }
                if self.autoplay {
                    self.autoplay = false;
                    self.play();
                }
            }
            Ok(Err(e)) => self.state = State::Failed(e),
            Err(TryRecvError::Disconnected) => self.state = State::Failed("The video decoder stopped.".into()),
            Err(TryRecvError::Empty) => {}
        }
    }

    fn poll(&mut self) {
        let Some(player) = &self.player else { return };
        loop {
            // Bound the decode-ahead: once enough frames are here, the
            // decoder's channel fills and decoding pauses itself.
            if self.queue.len() >= MAX_QUEUE && !self.awaiting {
                break;
            }
            let Ok(ev) = player.rx.try_recv() else { break };
            match ev {
                Event::Frame(f) => {
                    if f.gen != self.gen {
                        continue;
                    }
                    let awaited = self.awaiting;
                    self.awaiting = false;
                    self.seek_started = None;
                    if awaited {
                        self.pending_step = None;
                    }
                    // Playing: frames wait for their time. Paused: only the
                    // frame a seek asked for shows; frames decoded ahead stay
                    // queued for stepping.
                    if (self.playing && self.current.is_some()) || (!self.playing && !awaited) {
                        self.queue.push_back(f);
                    } else {
                        self.current = Some(f);
                    }
                }
                Event::Eof { gen } => {
                    if gen == self.gen {
                        self.eof = true;
                        self.awaiting = false;
                        self.seek_started = None;
                        self.pending_step = None;
                    }
                }
                Event::Error(e) => {
                    self.state = State::Failed(e);
                    self.awaiting = false;
                    self.seek_started = None;
                }
            }
        }
    }

    fn advance(&mut self, ctx: &egui::Context) {
        if !self.playing || self.player.is_none() {
            return;
        }
        if self.awaiting && self.seek_started.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            // The clock waits for the seek, so the frames it lands on are not late.
            let at = self.clock.base();
            self.clock.start(at);
            ctx.request_repaint_after(Duration::from_millis(5));
            return;
        }
        let mut t = self.clock.now();
        // The audio clock keeps sound and picture together.
        let raw = self.audio.as_ref().filter(|_| self.has_audio() && !self.at_end()).and_then(|a| a.clock());
        let stalled = match (raw, self.audio_seen) {
            (Some(v), Some((old, at))) if v == old => at.elapsed() > Duration::from_millis(150),
            (Some(v), _) => {
                self.audio_seen = Some((v, Instant::now()));
                false
            }
            (None, _) => false,
        };
        if let Some(ac) = raw.filter(|_| !stalled) {
            if ac.is_finite() && (ac - t).abs() < 1.0 {
                t = ac;
                // Keep the system clock aligned so a fallback is seamless.
                self.clock.start(ac);
            }
        }
        let half = self.frame_dur() * 0.5;
        while self.queue.front().is_some_and(|f| f.pts <= t + half) {
            self.current = self.queue.pop_front();
        }
        if self.at_end() {
            // The last frame stays for its own length too.
            let left = self.pts() + self.frame_dur() - t;
            if left > 0.0 && left < 1.0 {
                ctx.request_repaint_after(Duration::from_secs_f64(left));
                return;
            }
            if self.repeat {
                self.seek(0.0, true);
                ctx.request_repaint();
            } else {
                self.pause();
                self.ended = true;
            }
            return;
        }
        let due = self.queue.front().map(|f| f.pts - half - t).unwrap_or(0.005);
        let wait = if due.is_finite() { due.clamp(0.001, 0.05) } else { 0.005 };
        ctx.request_repaint_after(Duration::from_secs_f64(wait));
    }

    pub fn play(&mut self) {
        if self.error().is_some() || self.playing {
            return;
        }
        if self.player.is_none() {
            self.autoplay = true;
            return;
        }
        if self.ended || self.at_end() {
            // At the end: from the start again.
            self.seek(0.0, true);
        } else if !self.awaiting && !self.queue.is_empty() && self.audio.is_some() && self.has_audio() {
            // Frames stepped through while paused: the sound must start at the
            // frame on screen, not where the decoder got to.
            let t = self.pts();
            self.seek(t, true);
        }
        let at = if self.awaiting { self.clock.base() } else { self.pts() };
        self.ended = false;
        self.playing = true;
        self.clock.start(at);
        self.send(Cmd::Play);
        self.sound_on(true);
    }

    pub fn pause(&mut self) {
        self.autoplay = false;
        if !self.playing {
            return;
        }
        self.playing = false;
        self.clock.stop();
        self.send(Cmd::Pause);
        self.sound_on(false);
    }

    pub fn toggle(&mut self) {
        if self.playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Show the frame at `t` (`exact`), or the keyframe before it (fast,
    /// while dragging on the seek bar). Playing goes on after an exact seek.
    pub fn seek(&mut self, t: f64, exact: bool) {
        if self.player.is_none() {
            return;
        }
        let t = playback::clamp_time(t, self.last_pts());
        let was = self.playing;
        if was {
            self.send(Cmd::Pause);
            self.sound_on(false);
        }
        self.gen += 1;
        self.queue.clear();
        self.eof = false;
        self.ended = false;
        self.awaiting = true;
        self.seek_started = Some(Instant::now());
        self.pending_step = None;
        self.send(Cmd::Seek { t, exact, gen: self.gen });
        if was && exact {
            self.clock.start(t);
            self.send(Cmd::Play);
            self.sound_on(true);
        } else {
            self.playing = false;
            self.clock.stop();
            self.clock.set(t);
        }
    }

    /// `secs` seconds on (or back, when negative).
    pub fn skip(&mut self, secs: f64) {
        if self.player.is_none() {
            return;
        }
        let from = self.position();
        let t = playback::skip_target(from, secs, self.last_pts());
        self.seek(t, true);
    }

    /// Step `delta` frames (pauses first).
    pub fn step(&mut self, delta: i64) {
        if self.player.is_none() || delta == 0 {
            return;
        }
        self.pause();
        if delta == 1 && !self.awaiting {
            if let Some(f) = self.queue.pop_front() {
                self.clock.set(f.pts);
                self.current = Some(f);
                return;
            }
        }
        let from = self.pending_step.unwrap_or_else(|| self.pts());
        let (t, expect) = playback::step_target(from, self.frame_dur(), delta, self.last_pts());
        self.seek(t, true);
        self.pending_step = Some(expect);
    }

    /// Play the sound into `audio` from now on (None: silent).
    pub fn set_audio(&mut self, audio: Option<Sound>) {
        let same = match (&self.audio, &audio) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.sound_on(false);
        if let Some(old) = &self.audio {
            old.flush();
        }
        self.audio = audio;
        self.audio_seen = None;
        if self.player.is_none() {
            // The decoder is told once it is open.
            self.audio_changed = true;
            return;
        }
        self.send(Cmd::Audio(self.audio.clone()));
        if let Some(a) = &self.audio {
            a.flush();
            if !self.awaiting && self.has_frame() {
                // Decode the frame on screen again so the sound starts there.
                self.redecode();
            }
        }
        if self.playing {
            self.sound_on(true);
        }
    }

    /// Decode the frame on screen again (a new decode size).
    fn redecode(&mut self) {
        let (t, ended) = (self.pts(), self.ended);
        self.seek(t, true);
        self.ended = ended;
    }

    /// Have the decoder make frames about the size they show at (`zoom`:
    /// screen pixels per video pixel), once the view has settled.
    pub fn fit_decode(&mut self, zoom: f32, ctx: &egui::Context) {
        let (Some(info), true) = (&self.info, self.player.is_some()) else { return };
        let native = (info.width.max(2), info.height.max(2));
        let want = sync::wanted_size(native, zoom);
        if !sync::needs_resize(self.out_size, want) {
            self.out_wait.clear();
            return;
        }
        if self.awaiting {
            return;
        }
        match self.out_wait.update(want, Instant::now(), SETTLE) {
            Some(w) => {
                self.out_size = w;
                self.send(Cmd::OutputSize(w.0, w.1));
                if !self.playing {
                    // No new frames are coming: decode the one on screen again.
                    self.redecode();
                }
            }
            None => ctx.request_repaint_after(SETTLE + Duration::from_millis(10)),
        }
    }

    /// Paint the frame on screen into `dest` (the upright rectangle it
    /// shows in). False while there is no frame yet.
    pub fn paint(&mut self, ctx: &egui::Context, painter: &egui::Painter, dest: Rect, nearest: bool, tint: Color32) -> bool {
        self.upload(ctx, nearest);
        let Some(tex) = &self.tex else { return false };
        let mut mesh = egui::Mesh::with_texture(tex.id());
        mesh.add_rect_with_uv(dest, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
        super::mode::rotate_mesh(&mut mesh, dest, self.rotation());
        painter.add(egui::Shape::mesh(mesh));
        true
    }

    fn upload(&mut self, ctx: &egui::Context, nearest: bool) {
        let Some(f) = &self.current else { return };
        let key = (f.pts.to_bits(), Arc::as_ptr(&f.rgba) as usize, nearest);
        if key == self.tex_key && self.tex.is_some() {
            return;
        }
        self.tex_key = key;
        let img = super::mode::rgba_to_color_image(&f.rgba);
        let opts = if nearest { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
        match &mut self.tex {
            Some(t) => t.set(img, opts),
            None => self.tex = Some(ctx.load_texture(format!("inline-video:{}", self.path.display()), img, opts)),
        }
    }

    /// One line for the dev hook's `state`.
    pub fn state_text(&self) -> String {
        let status = match &self.state {
            State::Opening(_) => "opening".to_string(),
            State::Open if self.playing => "playing".to_string(),
            State::Open if self.ended => "ended".to_string(),
            State::Open => "paused".to_string(),
            State::Failed(e) => format!("failed ({e})"),
        };
        let (w, h) = self.display_size().unwrap_or((0, 0));
        let frame = self.current.as_ref().map(|f| format!("{}x{}", f.rgba.width(), f.rgba.height())).unwrap_or_else(|| "none".into());
        format!(
            "{status} {w}x{h} t={:.3}/{:.3} frame={frame} decode={}x{} sound={} repeat={}",
            self.position(),
            self.duration(),
            self.out_size.0,
            self.out_size.1,
            if self.audio.is_some() && self.has_audio() { "on" } else { "off" },
            self.repeat
        )
    }
}

impl Drop for InlinePlayer {
    fn drop(&mut self) {
        // The sound output outlives this file: stop its sound here.
        if let Some(a) = &self.audio {
            a.playing.store(false, Ordering::Relaxed);
            self.send(Cmd::Audio(None));
            a.flush();
        }
    }
}

// ---------------------------------------------------------------- viewer

/// The viewer's video: the player of the file on screen, the sound output
/// the files share while the viewer is open, and the seek bar's drag.
#[derive(Default)]
pub struct InlineVideo {
    player: Option<InlinePlayer>,
    sound: Option<SoundOut>,
    /// Dragging on the seek bar: the time under the pointer and whether it
    /// was playing when the drag began.
    scrub: Option<(f64, bool)>,
    /// Paused because the window was minimized: plays on when it is back.
    resume: bool,
}

impl InlineVideo {
    /// Play `path` in the viewer (opening it when it is not the file
    /// playing), or close the player when the file on screen is not a
    /// video (None).
    pub fn show(&mut self, path: Option<&Path>, ctx: &egui::Context, s: &VideoSettings) {
        let Some(path) = path else {
            self.close_player();
            return;
        };
        if self.player.as_ref().is_some_and(|p| p.path() == path) {
            return;
        }
        // The old file's decoder stops (and its sound) before the next starts.
        self.close_player();
        let sound = self.sound.get_or_insert_with(|| SoundOut::open(ctx));
        sound.set_level(s.volume, s.muted);
        let shared = sound.shared();
        let mut p = InlinePlayer::open(path, ctx, shared, true);
        p.repeat = s.repeat;
        self.player = Some(p);
    }

    fn close_player(&mut self) {
        self.player = None;
        self.scrub = None;
        self.resume = false;
    }

    /// Leave the viewer: stop playing, free the decoder and the sound output.
    pub fn close(&mut self) {
        self.close_player();
        self.sound = None;
    }

    /// Once a frame while a video is on screen. `minimized`: the window is,
    /// so playing pauses (and goes on when it is back).
    pub fn tick(&mut self, ctx: &egui::Context, minimized: bool, s: &VideoSettings) {
        let shared = self.sound.as_mut().and_then(|o| o.shared());
        if let Some(o) = &self.sound {
            o.set_level(s.volume, s.muted);
        }
        let Some(p) = &mut self.player else { return };
        if shared.is_some() && p.audio.is_none() {
            // The sound output opened after the file.
            p.set_audio(shared);
        }
        p.repeat = s.repeat;
        if minimized && p.playing() {
            p.pause();
            self.resume = true;
        } else if !minimized && self.resume {
            self.resume = false;
            p.play();
        }
        p.tick(ctx);
    }

    /// The size the video shows at, once its file is open.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.player.as_ref()?.display_size()
    }

    /// A card instead of the video: (title, why).
    pub fn unavailable(&self) -> Option<(&'static str, String)> {
        let e = self.player.as_ref()?.error()?;
        Some(("This video cannot be played", e.to_string()))
    }

    pub fn playing(&self) -> bool {
        self.player.as_ref().is_some_and(|p| p.playing())
    }

    pub fn scrubbing(&self) -> bool {
        self.scrub.is_some()
    }

    pub fn toggle_play(&mut self) {
        if let Some(p) = &mut self.player {
            p.toggle();
        }
    }

    /// Paint the frame into `dest` (`zoom`: screen pixels per video pixel).
    /// False while there is no frame yet.
    pub fn paint(&mut self, ctx: &egui::Context, painter: &egui::Painter, dest: Rect, zoom: f32, nearest: bool) -> bool {
        let Some(p) = &mut self.player else { return false };
        p.fit_decode(zoom, ctx);
        p.paint(ctx, painter, dest, nearest, Color32::WHITE)
    }

    /// Playback commands for the video `path` on screen. False when it is
    /// not the file playing.
    pub fn command(&mut self, path: &Path, cmd: Command, s: &mut VideoSettings) -> bool {
        let Some(p) = self.player.as_mut().filter(|p| p.path() == path) else { return false };
        match cmd {
            Command::PlayPause => p.toggle(),
            Command::NextFrame => p.step(1),
            Command::PrevFrame => p.step(-1),
            Command::SeekBack => p.skip(-SKIP_SECS),
            Command::SeekForward => p.skip(SKIP_SECS),
            Command::ToggleMute => s.muted = !s.muted,
            _ => return false,
        }
        true
    }

    /// One line for the dev hook's `state`.
    pub fn state_text(&self) -> String {
        match &self.player {
            Some(p) => p.state_text(),
            None => "none".into(),
        }
    }

    /// The transport bar over the bottom of the video, its bottom edge at
    /// `bottom`: play / pause, time, seek bar, repeat, sound and "Open in
    /// video mode" (returned as a command for the app).
    pub fn transport(&mut self, ui: &mut Ui, pal: &Palette, s: &mut VideoSettings, view: Rect, bottom: f32) -> Option<Command> {
        let sound_error = self.sound.as_ref().and_then(|o| o.error()).map(str::to_string);
        let player = self.player.as_mut()?;
        if player.error().is_some() {
            return None;
        }
        let w = (view.width() - 24.0).min(760.0);
        if w < 200.0 {
            return None;
        }
        let compact = w < 600.0;
        let narrow = w < 440.0;
        let rect = Rect::from_min_size(Pos2::new(view.center().x - w / 2.0, bottom - BAR_H), Vec2::new(w, BAR_H));
        crate::analysis_ui::canvas::occlude(ui.ctx(), rect);
        let radius = CornerRadius::same(theme::radius::WINDOW);
        ui.painter().add(theme::popover_shadow(pal).as_shape(rect, radius));
        ui.painter().rect_filled(rect, radius, pal.overlay);
        ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(6.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        // Clicks between the buttons stay on the bar (on the video they would pause it).
        let _ = child.interact(rect, child.scope_id().with("video-bar-blocker"), Sense::click_and_drag());
        child.spacing_mut().item_spacing.x = 2.0;
        let mut out = None;

        let (icon, what) = if player.playing() { (I::PAUSE, "Pause") } else { (I::PLAY, "Play") };
        if widgets::icon_button_sized(&mut child, icon, &tips::with_shortcut(what, &Command::PlayPause.shortcut()), false, 32.0, 18.0).clicked() {
            player.toggle();
        }
        let dur = player.duration();
        let pos = self.scrub.map(|(t, _)| t).unwrap_or_else(|| player.position());
        if !narrow {
            let text = playback::position_text(pos, dur);
            let g = child.painter().layout_no_wrap(text, FontId::monospace(12.0), pal.text);
            let (r, resp) = child.allocate_exact_size(Vec2::new(g.size().x + 14.0, 28.0), Sense::hover());
            child.painter().galley(Pos2::new(r.min.x + 7.0, r.center().y - g.size().y / 2.0), g, pal.text);
            resp.on_hover_text(format!(
                "{} of {}, frame {}. One frame back or on: {} or {}. Five seconds: {} or {}",
                super::mode::fmt_time(pos),
                super::mode::fmt_time(dur),
                player.frame_number(),
                Command::PrevFrame.shortcut(),
                Command::NextFrame.shortcut(),
                Command::SeekBack.shortcut(),
                Command::SeekForward.shortcut()
            ));
        }

        // The seek bar takes the room the buttons on the right leave.
        let right = 3.0 * (30.0 + 2.0) + if compact { 0.0 } else { 80.0 } + 12.0;
        let bar_w = (child.available_width() - right).max(40.0);
        let (bar, resp) = child.allocate_exact_size(Vec2::new(bar_w, 28.0), Sense::click_and_drag());
        let hot = resp.hovered() || resp.dragged();
        let track = Rect::from_center_size(bar.center(), Vec2::new((bar.width() - 14.0).max(10.0), if hot { 6.0 } else { 4.0 }));
        let (x0, tw) = (track.min.x, track.width());
        if resp.drag_started() {
            let was = player.playing();
            player.pause();
            self.scrub = Some((pos, was));
        }
        if resp.dragged() {
            if let (Some(ptr), Some(sc)) = (resp.interact_pointer_pos(), self.scrub.as_mut()) {
                let t = playback::time_at(ptr.x, x0, tw, dur);
                if (sc.0 - t).abs() > 1e-3 {
                    sc.0 = t;
                    // Keyframes while it moves: fast.
                    player.seek(t, false);
                }
            }
        }
        // (A drag that ended off the bar ends here too.)
        let released = !child.input(|i| i.pointer.primary_down());
        if resp.drag_stopped() || (self.scrub.is_some() && released) {
            if let Some((t, was)) = self.scrub.take() {
                // The exact frame on release.
                player.seek(t, true);
                if was {
                    player.play();
                }
            }
        } else if resp.clicked() {
            if let Some(ptr) = resp.interact_pointer_pos() {
                player.seek(playback::time_at(ptr.x, x0, tw, dur), true);
            }
        }
        {
            let pos = self.scrub.map(|(t, _)| t).unwrap_or_else(|| player.position());
            let painter = child.painter();
            let cr = CornerRadius::same(3);
            painter.rect_filled(track, cr, pal.border);
            let mut fill = track;
            fill.max.x = x0 + tw * playback::fraction(pos, dur);
            painter.rect_filled(fill, cr, pal.accent);
            painter.circle_filled(Pos2::new(fill.max.x, track.center().y), if hot { 7.0 } else { 5.5 }, pal.accent);
        }
        if let Some(hp) = resp.hover_pos() {
            let t = playback::time_at(hp.x, x0, tw, dur);
            resp.clone().on_hover_text_at_pointer(format!("{}  (click or drag to go there)", super::mode::fmt_time(t)));
        }

        if widgets::icon_button(&mut child, I::REPEAT, "Repeat: start again at the end", s.repeat).clicked() {
            s.repeat = !s.repeat;
        }
        if player.info.is_some() && !player.has_audio() {
            child.add_sized([30.0, 30.0], egui::Label::new(RichText::new(I::SPEAKER_SLASH).size(17.0).color(pal.faint))).on_hover_text("This video has no sound");
        } else if let Some(e) = &sound_error {
            child.add_sized([30.0, 30.0], egui::Label::new(RichText::new(I::SPEAKER_SLASH).size(17.0).color(pal.faint))).on_hover_text(format!("No sound: {e}"));
        } else {
            let tip = tips::with_shortcut(if s.muted { "Sound off: click to turn it on" } else { "Mute" }, &Command::ToggleMute.shortcut());
            if widgets::icon_button(&mut child, playback::volume_icon(s.volume, s.muted), &tip, s.muted).clicked() {
                s.muted = !s.muted;
            }
            if !compact {
                let r = child.allocate_ui_with_layout(Vec2::new(78.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(widgets::Slider::new(&mut s.volume, 0.0..=1.5).show_value(false).width(74.0).default_value(1.0))
                });
                if r.inner.on_hover_text("Volume (double-click: 100%)").changed() && s.muted {
                    s.muted = false;
                }
            }
        }
        widgets::separator_v(&mut child);
        let tip = tips::with_shortcut("Open in video mode: frame by frame, scopes and side by side comparison", &Command::OpenVideoMode.shortcut());
        if widgets::icon_button(&mut child, Command::OpenVideoMode.icon(), &tip, false).clicked() {
            out = Some(Command::OpenVideoMode);
        }
        out
    }
}

/// Height of the transport bar.
pub const BAR_H: f32 = 40.0;
