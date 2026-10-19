//! FFmpeg-backed media engine: probing, frame index, and a decoder worker per
//! opened file that supports playback, exact seeking and frame stepping.

use super::audio::AudioShared;
use super::metrics::{Plane, PlaneData, YuvFrame};
use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use std::collections::VecDeque;
use ffmpeg_next as ff;
use image::RgbaImage;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

static INIT: Once = Once::new();

/// Frame threads of the decoders that read for analysis and snapshots.
/// Each frame thread keeps its own decoder state and frames in flight
/// (tens of MB per thread for 1080p 10-bit HEVC), so these use a few
/// threads, not FFmpeg's default of one per core; playback keeps the default.
const SIDE_DECODE_THREADS: usize = 4;

/// [`frame_at`] decodes running at once (worst-frame thumbnails ask for
/// many frames together; each decode holds a decoder and its frames).
const FRAME_AT_PARALLEL: usize = 2;
static FRAME_AT_SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) = (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// A slot of [`FRAME_AT_SLOTS`], held while one [`frame_at`] decodes.
struct FrameAtSlot;

impl FrameAtSlot {
    fn acquire() -> FrameAtSlot {
        let (busy, cv) = &FRAME_AT_SLOTS;
        let mut n = busy.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= FRAME_AT_PARALLEL {
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        FrameAtSlot
    }
}

impl Drop for FrameAtSlot {
    fn drop(&mut self) {
        let (busy, cv) = &FRAME_AT_SLOTS;
        *busy.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        cv.notify_one();
    }
}

pub fn init() {
    INIT.call_once(|| {
        let _ = ff::init();
        ff::util::log::set_level(ff::util::log::Level::Error);
    });
}

#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub container: String,
    pub duration: f64,
    pub bitrate: i64,
    pub file_size: u64,
    pub codec: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub sar: (i32, i32),
    pub fps: f64,
    pub avg_fps: f64,
    pub frames_hint: i64,
    pub pix_fmt: String,
    pub bit_depth: u32,
    pub color_space: String,
    pub color_range: String,
    pub color_primaries: String,
    pub color_transfer: String,
    pub hdr: bool,
    pub rotation: i32,
    pub video_bitrate: i64,
    pub has_audio: bool,
    pub audio_codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_bitrate: i64,
    pub tags: Vec<(String, String)>,
}

fn space_name(s: ff::color::Space) -> String {
    use ff::color::Space::*;
    match s {
        BT709 => "BT.709".into(),
        BT470BG | SMPTE170M => "BT.601".into(),
        BT2020NCL | BT2020CL => "BT.2020".into(),
        SMPTE240M => "SMPTE 240M".into(),
        RGB => "RGB".into(),
        Unspecified => "unspecified".into(),
        other => format!("{other:?}"),
    }
}

fn transfer_name(t: ff::color::TransferCharacteristic) -> (String, bool) {
    use ff::color::TransferCharacteristic::*;
    match t {
        BT709 => ("BT.709".into(), false),
        SMPTE2084 => ("PQ (SMPTE 2084)".into(), true),
        ARIB_STD_B67 => ("HLG".into(), true),
        IEC61966_2_1 => ("sRGB".into(), false),
        Unspecified => ("unspecified".into(), false),
        other => (format!("{other:?}"), false),
    }
}

fn q2f(r: ff::Rational) -> f64 {
    if r.denominator() == 0 {
        0.0
    } else {
        r.numerator() as f64 / r.denominator() as f64
    }
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    init();
    let input = ff::format::input(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let mut info = MediaInfo { path: path.to_path_buf(), ..Default::default() };
    info.container = input.format().description().to_string();
    info.duration = if input.duration() > 0 { input.duration() as f64 / ff::ffi::AV_TIME_BASE as f64 } else { 0.0 };
    info.bitrate = unsafe { (*input.as_ptr()).bit_rate };
    info.file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for (k, v) in input.metadata().iter() {
        info.tags.push((k.to_string(), v.to_string()));
    }
    let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
    let ctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
    let dec = ctx.decoder().video()?;
    info.codec = dec.codec().map(|c| c.description().to_string()).unwrap_or_else(|| format!("{:?}", vs.parameters().id()));
    unsafe {
        let par = vs.parameters().as_ptr();
        let prof = (*par).profile;
        let name = ff::ffi::avcodec_profile_name((*par).codec_id, prof);
        if !name.is_null() {
            info.profile = std::ffi::CStr::from_ptr(name).to_string_lossy().to_string();
        }
        info.video_bitrate = (*par).bit_rate;
        info.sar = ((*par).sample_aspect_ratio.num, (*par).sample_aspect_ratio.den);
    }
    info.width = dec.width();
    info.height = dec.height();
    info.fps = q2f(vs.rate());
    info.avg_fps = q2f(vs.avg_frame_rate());
    info.frames_hint = vs.frames();
    let fmt = dec.format();
    info.pix_fmt = fmt.descriptor().map(|d| d.name().to_string()).unwrap_or_else(|| format!("{fmt:?}"));
    info.bit_depth = unsafe {
        let d = ff::ffi::av_pix_fmt_desc_get(fmt.into());
        if d.is_null() { 8 } else { (*d).comp[0].depth as u32 }
    };
    info.color_space = space_name(dec.color_space());
    info.color_range = match dec.color_range() {
        ff::color::Range::JPEG => "full (PC)".into(),
        ff::color::Range::MPEG => "limited (TV)".into(),
        _ => "unspecified".into(),
    };
    info.color_primaries = format!("{:?}", dec.color_primaries());
    let (tr, hdr) = transfer_name(dec.color_transfer_characteristic());
    info.color_transfer = tr;
    info.hdr = hdr;
    info.rotation = stream_rotation(&vs);
    if let Some(a) = input.streams().best(ff::media::Type::Audio) {
        info.has_audio = true;
        if let Ok(actx) = ff::codec::context::Context::from_parameters(a.parameters()) {
            if let Ok(ad) = actx.decoder().audio() {
                info.audio_codec = ad.codec().map(|c| c.description().to_string()).unwrap_or_default();
                info.sample_rate = ad.rate();
                info.channels = ad.channels();
                info.audio_bitrate = unsafe { (*a.parameters().as_ptr()).bit_rate };
            }
        }
    }
    Ok(info)
}

fn stream_rotation(s: &ff::format::stream::Stream) -> i32 {
    unsafe {
        let par = s.parameters().as_ptr();
        let sd = ff::ffi::av_packet_side_data_get((*par).coded_side_data, (*par).nb_coded_side_data, ff::ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX);
        if sd.is_null() || (*sd).size < 36 {
            return 0;
        }
        let m = (*sd).data as *const i32;
        let deg = ff::ffi::av_display_rotation_get(m);
        if deg.is_nan() { 0 } else { (-deg).round() as i32 }
    }
}

/// One entry per video frame (presentation order).
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexEntry {
    pub pts: f64,
    pub size: u32,
    pub key: bool,
    /// b'I', b'P', b'B' or b'?' (filled in as frames get decoded).
    pub kind: u8,
}

/// Demux-only pass over the file: exact frame count, timestamps, packet sizes.
pub fn build_index(path: &Path, cancel: &AtomicBool, progress: &dyn Fn(f32)) -> Result<Vec<IndexEntry>> {
    init();
    let mut input = ff::format::input(path)?;
    let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
    let vidx = vs.index();
    let tb = q2f(vs.time_base());
    let start = if vs.start_time() != ff::ffi::AV_NOPTS_VALUE { vs.start_time() as f64 * tb } else { 0.0 };
    let duration = input.duration() as f64 / ff::ffi::AV_TIME_BASE as f64;
    let mut out = Vec::new();
    let mut pkt: ff::Packet;
    let mut n = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("cancelled"));
        }
        match { pkt = ff::Packet::empty(); pkt.read(&mut input) } {
            Ok(()) => {}
            Err(ff::Error::Eof) => break,
            Err(e) => {
                if out.is_empty() {
                    return Err(e.into());
                }
                break;
            }
        }
        if pkt.stream() != vidx {
            continue;
        }
        let ts = pkt.pts().or(pkt.dts());
        let Some(ts) = ts else { continue };
        let pts = ts as f64 * tb - start;
        let key = pkt.is_key();
        out.push(IndexEntry { pts, size: pkt.size() as u32, key, kind: if key { b'I' } else { b'?' } });
        n += 1;
        if n % 500 == 0 && duration > 0.0 {
            progress((pts / duration).clamp(0.0, 1.0) as f32);
        }
    }
    out.sort_by(|a, b| a.pts.partial_cmp(&b.pts).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup_by(|a, b| (a.pts - b.pts).abs() < 1e-9);
    progress(1.0);
    Ok(out)
}

/// Decoded frame handed to the UI.
#[derive(Clone)]
pub struct VFrame {
    pub gen: u64,
    pub pts: f64,
    pub kind: u8,
    pub rgba: Arc<RgbaImage>,
    pub native: Option<Arc<YuvFrame>>,
}

pub enum Cmd {
    Play,
    Pause,
    /// Show the frame at `t` (exact) or the keyframe before it (fast scrubbing).
    Seek { t: f64, exact: bool, gen: u64 },
    OutputSize(u32, u32),
    Native(bool),
    Quit,
}

pub enum Event {
    Frame(VFrame),
    Eof { gen: u64 },
    Error(String),
}

pub struct Player {
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Event>,
}

/// Shared frame index (filled by the index thread once it finishes).
pub type SharedIndex = Arc<parking_lot::Mutex<Option<Vec<IndexEntry>>>>;

impl Player {
    pub fn open(path: &Path, audio: Option<Arc<AudioShared>>, index: SharedIndex, ctx: egui::Context) -> Result<Player> {
        init();
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let (ev_tx, ev_rx) = bounded::<Event>(6);
        // FFmpeg contexts are not Send: build the worker on its own thread and
        // report whether opening succeeded.
        let (init_tx, init_rx) = bounded::<Result<(), String>>(1);
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("lumen-video".into())
            .spawn(move || {
                let mut worker = match Worker::new(&path, audio, index, cmd_rx, ev_tx.clone(), ctx.clone()) {
                    Ok(w) => {
                        let _ = init_tx.send(Ok(()));
                        w
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                if r.is_err() {
                    let _ = ev_tx.send(Event::Error("The video decoder crashed".into()));
                    ctx.request_repaint();
                }
            })?;
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Player { tx: cmd_tx, rx: ev_rx }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("The video decoder failed to start")),
        }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

/// Memory for decoded frames kept by one worker for instant back-steps.
const FRAME_CACHE_BYTES: usize = 512 << 20;

/// Decoded frames in presentation order: one contiguous run that ends at the
/// decoder's latest output. Stepping back inside the run needs no decoding;
/// the oldest frames are dropped when the memory cap is reached.
#[derive(Default)]
struct FrameCache {
    frames: VecDeque<(f64, ff::frame::Video)>,
    bytes: usize,
}

impl FrameCache {
    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }

    fn push(&mut self, pts: f64, f: ff::frame::Video) {
        self.bytes += frame_bytes(&f);
        self.frames.push_back((pts, f));
        while self.bytes > FRAME_CACHE_BYTES && self.frames.len() > 1 {
            if let Some((_, old)) = self.frames.pop_front() {
                self.bytes = self.bytes.saturating_sub(frame_bytes(&old));
            }
        }
    }

    /// Index of the frame an exact seek to `t` shows, if the run covers `t`.
    fn find(&self, t: f64) -> Option<usize> {
        let (first, last) = (self.frames.front()?.0, self.frames.back()?.0);
        if t < first - 1e-4 || t > last + 1e-4 {
            return None;
        }
        let i = self.frames.partition_point(|(p, _)| *p < t - 1e-4);
        (i < self.frames.len()).then_some(i)
    }

    /// Index of the frame that follows the one at `pts`, if both are cached.
    fn after(&self, pts: f64) -> Option<usize> {
        let i = self.find(pts)?;
        (i + 1 < self.frames.len()).then_some(i + 1)
    }
}

fn frame_bytes(f: &ff::frame::Video) -> usize {
    (0..f.planes()).map(|i| f.stride(i) * f.plane_height(i) as usize).sum::<usize>().max(1)
}

struct Worker {
    input: ff::format::context::Input,
    vidx: usize,
    vtb: f64,
    vstart: f64,
    vdec: ff::decoder::Video,
    aidx: Option<usize>,
    atb: f64,
    adec: Option<ff::decoder::Audio>,
    resampler: Option<ff::software::resampling::Context>,
    audio: Option<Arc<AudioShared>>,
    index: SharedIndex,
    rx: Receiver<Cmd>,
    tx: Sender<Event>,
    ctx: egui::Context,
    /// Commands received while a seek was decoding, run after it.
    backlog: VecDeque<Cmd>,
    quit: bool,
    scaler: Option<(ff::software::scaling::Context, (ff::format::Pixel, u32, u32, u32, u32))>,
    native: NativeConverter,
    out_size: (u32, u32),
    want_native: bool,
    playing: bool,
    gen: u64,
    eof_sent: bool,
    demux_eof: bool,
    vdec_eof: bool,
    pending_vpkts: VecDeque<ff::Packet>,
    cache: FrameCache,
    /// Pts of the last frame sent for display.
    shown: Option<f64>,
    audio_min_pts: f64,
    /// While seeking: frames before this pts would not fit in the cache, so
    /// non-reference ones are not decoded at all and the rest not kept.
    skip_before: f64,
    frame_dur: f64,
    /// Bytes of the latest decoded frame (sizes the skip window).
    frame_bytes_hint: usize,
}

impl Worker {
    fn new(path: &Path, audio: Option<Arc<AudioShared>>, index: SharedIndex, rx: Receiver<Cmd>, tx: Sender<Event>, ctx: egui::Context) -> Result<Worker> {
        let input = ff::format::input(path).with_context(|| format!("Cannot open {}", path.display()))?;
        let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
        let vidx = vs.index();
        let vtb = q2f(vs.time_base());
        let vstart = if vs.start_time() != ff::ffi::AV_NOPTS_VALUE { vs.start_time() as f64 * vtb } else { 0.0 };
        let mut vctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
        vctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
        let vdec = vctx.decoder().video()?;
        let (vw, vh) = (vdec.width(), vdec.height());
        let fps = [q2f(vs.avg_frame_rate()), q2f(vs.rate())].into_iter().find(|f| *f > 0.0).unwrap_or(25.0);
        let mut aidx = None;
        let mut atb = 0.0;
        let mut adec = None;
        if audio.is_some() {
            if let Some(a) = input.streams().best(ff::media::Type::Audio) {
                if let Ok(actx) = ff::codec::context::Context::from_parameters(a.parameters()) {
                    if let Ok(d) = actx.decoder().audio() {
                        aidx = Some(a.index());
                        atb = q2f(a.time_base());
                        adec = Some(d);
                    }
                }
            }
        }
        Ok(Worker {
            input,
            vidx,
            vtb,
            vstart,
            vdec,
            aidx,
            atb,
            adec,
            resampler: None,
            audio,
            index,
            rx,
            tx,
            ctx,
            backlog: VecDeque::new(),
            quit: false,
            scaler: None,
            native: NativeConverter::default(),
            out_size: (vw, vh),
            want_native: false,
            playing: false,
            gen: 0,
            eof_sent: false,
            demux_eof: false,
            vdec_eof: false,
            pending_vpkts: Default::default(),
            cache: FrameCache::default(),
            shown: None,
            audio_min_pts: f64::NEG_INFINITY,
            skip_before: f64::NEG_INFINITY,
            frame_dur: 1.0 / fps,
            frame_bytes_hint: 0,
        })
    }

    fn run(&mut self) {
        // First frame so the UI shows something immediately.
        self.seek(0.0, false, 0);
        while !self.quit {
            let cmd = match self.backlog.pop_front() {
                Some(c) => Some(c),
                None if self.playing => self.rx.try_recv().ok(),
                None => match self.rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            };
            if let Some(cmd) = cmd {
                match cmd {
                    Cmd::Quit => return,
                    Cmd::Play => {
                        self.sync_audio();
                        self.playing = true;
                    }
                    Cmd::Pause => self.playing = false,
                    // Bursts of seeks (scrubbing, rapid steps) collapse to the latest.
                    Cmd::Seek { t, exact, gen } => match self.drain_commands() {
                        Some((t, exact, gen)) => self.seek(t, exact, gen),
                        None => self.seek(t, exact, gen),
                    },
                    Cmd::OutputSize(w, h) => self.out_size = (w.max(2), h.max(2)),
                    Cmd::Native(on) => self.want_native = on,
                }
                continue;
            }
            if self.playing {
                // Keep audio fed even when the video queue is full.
                let audio_low = self.audio.as_ref().map(|a| a.buffered_secs() < 0.4).unwrap_or(false) && self.aidx.is_some() && !self.demux_eof;
                if self.tx.len() < 5 {
                    match self.next_output() {
                        Some(i) => {
                            if !self.send_cached(i) {
                                return;
                            }
                        }
                        None => {
                            if !self.eof_sent {
                                self.eof_sent = true;
                                let _ = self.tx.send(Event::Eof { gen: self.gen });
                                self.ctx.request_repaint();
                            }
                            self.playing = false;
                        }
                    }
                } else if audio_low && self.pending_vpkts.len() < 600 {
                    // Read ahead for audio, but never let undisplayed video
                    // packets pile up without bound (e.g. while minimized).
                    self.read_packet();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(3));
                }
            }
        }
    }

    /// Take queued commands without blocking: returns the latest seek, applies
    /// settings at once and keeps play/pause for after the seek.
    fn drain_commands(&mut self) -> Option<(f64, bool, u64)> {
        let mut latest = None;
        loop {
            match self.rx.try_recv() {
                Ok(Cmd::Seek { t, exact, gen }) => latest = Some((t, exact, gen)),
                Ok(Cmd::OutputSize(w, h)) => self.out_size = (w.max(2), h.max(2)),
                Ok(Cmd::Native(on)) => self.want_native = on,
                Ok(Cmd::Quit) => self.quit = true,
                Ok(other) => self.backlog.push_back(other),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.quit = true;
                    break;
                }
            }
        }
        latest
    }

    fn frame_pts(&self, f: &ff::frame::Video) -> f64 {
        let ts = f.timestamp().or(f.pts()).unwrap_or(0);
        ts as f64 * self.vtb - self.vstart
    }

    /// Read one packet; route audio immediately, queue video packets.
    fn read_packet(&mut self) -> bool {
        if self.demux_eof {
            return false;
        }
        let mut pkt = ff::Packet::empty();
        match pkt.read(&mut self.input) {
            Ok(()) => {
                if pkt.stream() == self.vidx {
                    self.pending_vpkts.push_back(pkt);
                } else if Some(pkt.stream()) == self.aidx {
                    self.decode_audio(&pkt);
                }
                true
            }
            Err(ff::Error::Eof) => {
                self.demux_eof = true;
                false
            }
            Err(_) => {
                // Treat read errors like EOF (truncated files).
                self.demux_eof = true;
                false
            }
        }
    }

    fn decode_audio(&mut self, pkt: &ff::Packet) {
        let (Some(dec), Some(audio)) = (self.adec.as_mut(), self.audio.clone()) else { return };
        if dec.send_packet(pkt).is_err() {
            return;
        }
        let mut frame = ff::frame::Audio::empty();
        while dec.receive_frame(&mut frame).is_ok() {
            let pts = frame.timestamp().or(frame.pts()).map(|t| t as f64 * self.atb - self.vstart).unwrap_or(0.0);
            if self.resampler.is_none() {
                let layout = if frame.channel_layout().channels() > 0 && !frame.channel_layout().is_empty() {
                    frame.channel_layout()
                } else {
                    ff::ChannelLayout::default(frame.channels() as i32)
                };
                let dst_layout = ff::ChannelLayout::default(audio.channels as i32);
                match ff::software::resampling::Context::get(frame.format(), layout, frame.rate(), ff::format::Sample::F32(ff::format::sample::Type::Packed), dst_layout, audio.rate) {
                    Ok(r) => self.resampler = Some(r),
                    Err(_) => return,
                }
            }
            let Some(rs) = self.resampler.as_mut() else { return };
            let mut out = ff::frame::Audio::empty();
            if rs.run(&frame, &mut out).is_err() {
                continue;
            }
            let n = out.samples() * audio.channels as usize;
            if n == 0 {
                continue;
            }
            let data = out.data(0);
            let samples: &[f32] = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const f32, n.min(data.len() / 4)) };
            // Trim audio before an exact seek target.
            if pts + out.samples() as f64 / audio.rate as f64 <= self.audio_min_pts {
                continue;
            }
            if pts < self.audio_min_pts {
                let skip = ((self.audio_min_pts - pts) * audio.rate as f64) as usize * audio.channels as usize;
                if skip < samples.len() {
                    audio.push(&samples[skip..], self.audio_min_pts);
                }
                continue;
            }
            audio.push(samples, pts);
        }
    }

    /// Decode the next video frame in presentation order.
    fn next_frame(&mut self) -> Option<ff::frame::Video> {
        let mut frame = ff::frame::Video::empty();
        loop {
            match self.vdec.receive_frame(&mut frame) {
                Ok(()) => return Some(frame),
                Err(ff::Error::Eof) => return None,
                Err(_) => {}
            }
            if let Some(pkt) = self.pending_vpkts.pop_front() {
                // Frame threads pick this up per packet.
                // (A few frames of margin: decode and keep thresholds must not touch.)
                let skip = pkt.pts().map(|p| (p as f64 * self.vtb - self.vstart) < self.skip_before - 4.0 * self.frame_dur).unwrap_or(false);
                unsafe {
                    (*self.vdec.as_mut_ptr()).skip_frame = if skip { ff::ffi::AVDiscard::AVDISCARD_NONREF } else { ff::ffi::AVDiscard::AVDISCARD_DEFAULT };
                }
                let _ = self.vdec.send_packet(&pkt);
                continue;
            }
            if self.demux_eof {
                if !self.vdec_eof {
                    self.vdec_eof = true;
                    let _ = self.vdec.send_eof();
                    continue;
                }
                return None;
            }
            self.read_packet();
        }
    }

    /// Decode the next frame into the cache; returns its cache index and,
    /// while seeking, whether the frame came before the cache window (such
    /// frames are not kept, so the cached run has no holes).
    fn decode_into_cache(&mut self) -> Option<(usize, bool)> {
        let f = self.next_frame()?;
        let pts = self.frame_pts(&f);
        self.frame_bytes_hint = frame_bytes(&f);
        if pts < self.skip_before {
            // Park it alone so callers can still look at its pts.
            self.cache.clear();
            self.cache.push(pts, f);
            return Some((0, true));
        }
        if self.cache.frames.front().is_some_and(|(p, _)| *p < self.skip_before) {
            // Skipped frames lie between the parked frame and this one.
            self.cache.clear();
        }
        self.cache.push(pts, f);
        Some((self.cache.frames.len() - 1, false))
    }

    /// The frame after the one on screen: from the cache when stepping back
    /// left newer frames there, else freshly decoded.
    fn next_output(&mut self) -> Option<usize> {
        if let Some(i) = self.shown.and_then(|s| self.cache.after(s)) {
            return Some(i);
        }
        self.decode_into_cache().map(|(i, _)| i)
    }

    /// Convert cached frame `i` and send it for display. False when the UI is gone.
    fn send_cached(&mut self, i: usize) -> bool {
        // Borrow the frame out of the cache while converting it (no copy).
        let pts = self.cache.frames[i].0;
        let f = std::mem::replace(&mut self.cache.frames[i].1, ff::frame::Video::empty());
        let vf = self.to_vframe(&f);
        self.cache.frames[i].1 = f;
        self.shown = Some(pts);
        let ok = self.tx.send(Event::Frame(vf)).is_ok();
        self.ctx.request_repaint();
        ok
    }

    /// Pts of the keyframe at or before `t`, from the frame index.
    fn keyframe_before(&self, t: f64) -> Option<f64> {
        let guard = self.index.lock();
        let idx = guard.as_ref()?;
        let i = idx.partition_point(|e| e.pts <= t + 1e-4);
        idx[..i].iter().rev().find(|e| e.key).map(|e| e.pts)
    }

    /// Whether decoding on from the decoder's position reaches `t` sooner than
    /// a demuxer seek: no keyframe to jump to in between, or only a few frames
    /// before one (a seek costs about as much as decoding those).
    fn reachable_by_decoding(&self, t: f64) -> bool {
        let Some(&(pos, _)) = self.cache.frames.back() else { return false };
        if self.vdec_eof || t <= pos {
            return false;
        }
        match self.keyframe_before(t) {
            Some(k) => k <= pos + 0.3,
            None => t - pos < 1.0,
        }
    }

    /// Position the demuxer at the keyframe before `t - back`, flushing decoders.
    fn demux_seek(&mut self, t: f64, exact: bool, back: f64) {
        self.eof_sent = false;
        self.demux_eof = false;
        self.vdec_eof = false;
        self.pending_vpkts.clear();
        // Seek to the keyframe's own time when the index knows it: containers
        // seek by decode time, which for a keyframe followed by reordered
        // frames can land on the next keyframe, after the target.
        let at = (t - back).max(0.0);
        let at = self.keyframe_before(at).map(|k| k.min(at)).unwrap_or(at);
        // Round: truncating could land one tick before the keyframe.
        let ts = ((at + self.vstart) / self.vtb).round() as i64;
        unsafe {
            ff::ffi::av_seek_frame(self.input.as_mut_ptr(), self.vidx as i32, ts, ff::ffi::AVSEEK_FLAG_BACKWARD as i32);
        }
        self.vdec.flush();
        if let Some(a) = self.adec.as_mut() {
            a.flush();
        }
        self.resampler = None;
        if let Some(a) = &self.audio {
            a.flush();
        }
        self.audio_min_pts = if exact { t } else { f64::NEG_INFINITY };
        self.cache.clear();
    }

    /// Show the frame at `t`. Frames decoded on the way are cached, and newer
    /// seeks that arrive meanwhile retarget the same pass instead of queueing.
    fn seek(&mut self, mut t: f64, mut exact: bool, mut gen: u64) {
        let mut back_off = 0.0;
        // For the dev hook's timing log.
        let started = std::time::Instant::now();
        let (mut decoded, mut seeks) = (0usize, 0usize);
        'target: loop {
            self.gen = gen;
            self.eof_sent = false;
            self.skip_before = f64::NEG_INFINITY;
            if exact {
                if let Some(i) = self.cache.find(t) {
                    self.send_cached(i);
                    return;
                }
            }
            let resume = exact && self.reachable_by_decoding(t);
            if !resume {
                self.demux_seek(t, exact, back_off);
                seeks += 1;
                // Frames older than what the cache holds are only decoded
                // when other frames need them.
                if exact && self.frame_bytes_hint > 0 {
                    let fit = (FRAME_CACHE_BYTES / self.frame_bytes_hint).saturating_sub(2);
                    self.skip_before = t - fit as f64 * self.frame_dur;
                }
            }
            let mut first = !resume;
            loop {
                if let Some((t2, e2, g2)) = self.drain_commands() {
                    (t, exact, gen) = (t2, e2, g2);
                    back_off = 0.0;
                    continue 'target;
                }
                if self.quit {
                    self.skip_before = f64::NEG_INFINITY;
                    return;
                }
                let Some((i, early)) = self.decode_into_cache() else {
                    self.skip_before = f64::NEG_INFINITY;
                    // Past the end: show the last frame there is.
                    match self.cache.frames.len() {
                        0 => {
                            let _ = self.tx.send(Event::Eof { gen });
                            self.ctx.request_repaint();
                        }
                        n => {
                            self.send_cached(n - 1);
                        }
                    }
                    return;
                };
                let pts = self.cache.frames[i].0;
                decoded += 1;
                if first && exact && pts > t + 1e-3 && t > 1e-3 && back_off < 30.0 {
                    // Landed after the target (open-GOP leading frames are
                    // dropped after a seek): start from an earlier keyframe.
                    back_off = if back_off == 0.0 { 1.0 } else { back_off * 4.0 };
                    continue 'target;
                }
                first = false;
                if early {
                    continue;
                }
                if !exact || pts >= t - 1e-4 {
                    self.skip_before = f64::NEG_INFINITY;
                    if super::mode::dev_dir().is_some() {
                        super::mode::dev_log(&format!("  worker: target={t:.4} seeks={seeks} decoded={decoded} cached={} ms={:.1}", self.cache.frames.len(), started.elapsed().as_secs_f64() * 1000.0));
                    }
                    self.send_cached(i);
                    return;
                }
            }
        }
    }

    /// Before playing from a frame that came from the cache, make the queued
    /// audio start at that frame so sound stays in sync.
    fn sync_audio(&mut self) {
        let (Some(audio), Some(shown)) = (self.audio.clone(), self.shown) else { return };
        if self.aidx.is_none() {
            return;
        }
        match audio.trim_to(shown) {
            super::audio::Trim::Done => {}
            super::audio::Trim::Empty if self.audio_min_pts <= shown + 1e-3 => self.audio_min_pts = shown,
            _ => {
                // The audio for this frame was already dropped: read it again.
                let gen = self.gen;
                self.cache.clear();
                self.seek(shown, true, gen);
            }
        }
    }

    fn to_vframe(&mut self, f: &ff::frame::Video) -> VFrame {
        let (ow, oh) = self.out_size;
        let rgba = self.convert_rgba(f, ow, oh).unwrap_or_else(|_| RgbaImage::new(ow.max(1), oh.max(1)));
        let native = if self.want_native { self.native_planes(f).ok().map(Arc::new) } else { None };
        let kind = match f.kind() {
            ff::picture::Type::I | ff::picture::Type::SI => b'I',
            ff::picture::Type::P | ff::picture::Type::SP => b'P',
            ff::picture::Type::B | ff::picture::Type::BI => b'B',
            _ => b'?',
        };
        VFrame { gen: self.gen, pts: self.frame_pts(f), kind, rgba: Arc::new(rgba), native }
    }

    fn convert_rgba(&mut self, f: &ff::frame::Video, w: u32, h: u32) -> Result<RgbaImage> {
        let key = (f.format(), f.width(), f.height(), w, h);
        if self.scaler.as_ref().map(|(_, k)| *k != key).unwrap_or(true) {
            let flags = if w < f.width() { ff::software::scaling::Flags::AREA } else { ff::software::scaling::Flags::BICUBIC };
            let mut sc = ff::software::scaling::Context::get(f.format(), f.width(), f.height(), ff::format::Pixel::RGBA, w, h, flags | ff::software::scaling::Flags::ACCURATE_RND | ff::software::scaling::Flags::FULL_CHR_H_INT)?;
            set_colorspace(&mut sc, f);
            self.scaler = Some((sc, key));
        }
        let (sc, _) = self.scaler.as_mut().unwrap();
        let mut out = ff::frame::Video::empty();
        sc.run(f, &mut out)?;
        let stride = out.stride(0);
        let data = out.data(0);
        let mut buf = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            buf.extend_from_slice(&data[y * stride..y * stride + w as usize * 4]);
        }
        RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("bad frame"))
    }

    /// Copy the frame's native planes for metrics (normalising unusual layouts).
    fn native_planes(&mut self, f: &ff::frame::Video) -> Result<YuvFrame> {
        self.native.convert(f, None).map(|(y, _)| y)
    }
}

/// Copies decoded frames into [`YuvFrame`]s: planar 8-bit and 10/12/16-bit
/// layouts are copied as they are, everything else (NV12, P010, RGB, ...) is
/// first normalised to planar 4:2:0 by swscale (8-bit, or 16-bit for deeper
/// sources). Playback and [`SeqDecoder`] share it, so both hand out the same
/// samples for the same frame.
#[derive(Default)]
struct NativeConverter {
    scaler: Option<(ff::software::scaling::Context, (ff::format::Pixel, u32, u32))>,
}

impl NativeConverter {
    /// The frame's planes, cropped to `roi` (snapped outward to the chroma
    /// grid) when one is given, and the layout of what was copied.
    fn convert(&mut self, f: &ff::frame::Video, roi: Option<Roi>) -> Result<(YuvFrame, NativeFormat)> {
        use ff::format::Pixel as P;
        let fmt = f.format();
        let direct8 = matches!(fmt, P::YUV420P | P::YUVJ420P | P::YUV422P | P::YUVJ422P | P::YUV444P | P::YUVJ444P | P::GRAY8);
        let direct16 = matches!(fmt, P::YUV420P10LE | P::YUV420P12LE | P::YUV422P10LE | P::YUV422P12LE | P::YUV444P10LE | P::YUV444P12LE | P::YUV420P16LE | P::YUV444P16LE);
        if direct8 || direct16 {
            let y = copy_planes(f, direct16, roi);
            let nf = NativeFormat::of(f, f, &y);
            return Ok((y, nf));
        }
        let depth = pix_depth(fmt);
        let target = if depth > 8 { P::YUV420P16LE } else { P::YUV420P };
        let key = (fmt, f.width(), f.height());
        if self.scaler.as_ref().map(|(_, k)| *k != key).unwrap_or(true) {
            let sc = ff::software::scaling::Context::get(fmt, f.width(), f.height(), target, f.width(), f.height(), ff::software::scaling::Flags::POINT)?;
            self.scaler = Some((sc, key));
        }
        let (sc, _) = self.scaler.as_mut().unwrap();
        let mut out = ff::frame::Video::empty();
        sc.run(f, &mut out)?;
        let mut y = copy_planes(&out, depth > 8, roi);
        if depth > 8 {
            y.bit_depth = 16;
        }
        let nf = NativeFormat::of(f, &out, &y);
        Ok((y, nf))
    }
}

/// Bit depth of the first component of a pixel format.
fn pix_depth(fmt: ff::format::Pixel) -> u32 {
    unsafe {
        let d = ff::ffi::av_pix_fmt_desc_get(fmt.into());
        if d.is_null() { 8 } else { (*d).comp[0].depth as u32 }
    }
}

/// log2 of the horizontal and vertical chroma subsampling of a pixel format.
fn chroma_shift(fmt: ff::format::Pixel) -> (u32, u32) {
    unsafe {
        let d = ff::ffi::av_pix_fmt_desc_get(fmt.into());
        if d.is_null() { (0, 0) } else { ((*d).log2_chroma_w as u32, (*d).log2_chroma_h as u32) }
    }
}

fn copy_planes(f: &ff::frame::Video, wide: bool, roi: Option<Roi>) -> YuvFrame {
    let depth = pix_depth(f.format());
    let shift = chroma_shift(f.format());
    // Luma-space window to copy; the chroma planes take it shifted down.
    let roi = roi.map(|r| r.snapped(f.width(), f.height(), shift));
    let mut planes = Vec::new();
    for i in 0..f.planes().min(3) {
        let (pw, ph) = (f.plane_width(i) as usize, f.plane_height(i) as usize);
        let (x0, y0, w, h) = match roi {
            Some(r) => {
                let (sx, sy) = if i == 0 { (0, 0) } else { shift };
                let (x0, y0) = ((r.x >> sx) as usize, (r.y >> sy) as usize);
                // Round the far edge up (odd luma sizes have a partial chroma sample).
                let x1 = (((r.x + r.w) as usize + (1 << sx) - 1) >> sx).min(pw);
                let y1 = (((r.y + r.h) as usize + (1 << sy) - 1) >> sy).min(ph);
                (x0.min(x1), y0.min(y1), x1.saturating_sub(x0), y1.saturating_sub(y0))
            }
            None => (0, 0, pw, ph),
        };
        let stride = f.stride(i);
        let data = f.data(i);
        let pd = if wide {
            let mut v = Vec::with_capacity(w * h);
            for y in y0..y0 + h {
                let row = &data[y * stride + x0 * 2..y * stride + (x0 + w) * 2];
                v.extend(row.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            }
            PlaneData::U16(v)
        } else {
            let mut v = Vec::with_capacity(w * h);
            for y in y0..y0 + h {
                v.extend_from_slice(&data[y * stride + x0..y * stride + x0 + w]);
            }
            PlaneData::U8(v)
        };
        planes.push(Plane { width: w, height: h, data: pd });
    }
    YuvFrame { planes, bit_depth: depth }
}

/// Tell swscale which YUV matrix and range the source uses (it defaults to
/// BT.601 limited, which renders HD video with wrong colors).
fn set_colorspace(sc: &mut ff::software::scaling::Context, f: &ff::frame::Video) {
    use ff::color::Space;
    let cs = match f.color_space() {
        Space::BT709 => ff::ffi::SWS_CS_ITU709,
        Space::BT2020NCL | Space::BT2020CL => ff::ffi::SWS_CS_BT2020,
        Space::SMPTE240M => ff::ffi::SWS_CS_SMPTE240M,
        Space::BT470BG | Space::SMPTE170M => ff::ffi::SWS_CS_ITU601,
        Space::FCC => ff::ffi::SWS_CS_FCC,
        _ => {
            if f.height() >= 720 {
                ff::ffi::SWS_CS_ITU709
            } else {
                ff::ffi::SWS_CS_ITU601
            }
        }
    };
    let full = matches!(f.color_range(), ff::color::Range::JPEG) || matches!(f.format(), ff::format::Pixel::YUVJ420P | ff::format::Pixel::YUVJ422P | ff::format::Pixel::YUVJ444P);
    unsafe {
        let coeffs = ff::ffi::sws_getCoefficients(cs as i32);
        let dst = ff::ffi::sws_getCoefficients(ff::ffi::SWS_CS_DEFAULT as i32);
        ff::ffi::sws_setColorspaceDetails(sc.as_mut_ptr(), coeffs, full as i32, dst, 1, 0, 1 << 16, 1 << 16);
    }
}

/// A representative frame (at ~10% of the duration) scaled to fit `max_side`.
pub fn thumbnail(path: &Path, max_side: u32) -> Result<RgbaImage> {
    init();
    let mut input = ff::format::input(path)?;
    let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
    let vidx = vs.index();
    let mut vctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
    vctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 2 });
    let mut dec = vctx.decoder().video()?;
    let rotation = stream_rotation(&vs);
    let dur = input.duration();
    if dur > 0 {
        let target = dur / 10;
        let _ = input.seek(target, ..target);
    }
    let mut frame = ff::frame::Video::empty();
    let mut got = false;
    let mut pkt: ff::Packet;
    let mut reads = 0;
    while !got && reads < 4000 {
        reads += 1;
        match { pkt = ff::Packet::empty(); pkt.read(&mut input) } {
            Ok(()) => {
                if pkt.stream() != vidx {
                    continue;
                }
                if dec.send_packet(&pkt).is_ok() && dec.receive_frame(&mut frame).is_ok() {
                    got = true;
                }
            }
            Err(_) => {
                let _ = dec.send_eof();
                got = dec.receive_frame(&mut frame).is_ok();
                break;
            }
        }
    }
    if !got {
        return Err(anyhow!("Could not decode a frame"));
    }
    let (w, h) = (frame.width(), frame.height());
    let s = (max_side as f64 / w.max(h) as f64).min(1.0);
    let (tw, th) = (((w as f64 * s) as u32).max(2), ((h as f64 * s) as u32).max(2));
    let mut sc = ff::software::scaling::Context::get(frame.format(), w, h, ff::format::Pixel::RGBA, tw, th, ff::software::scaling::Flags::AREA)?;
    set_colorspace(&mut sc, &frame);
    let mut out = ff::frame::Video::empty();
    sc.run(&frame, &mut out)?;
    let stride = out.stride(0);
    let data = out.data(0);
    let mut buf = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th as usize {
        buf.extend_from_slice(&data[y * stride..y * stride + tw as usize * 4]);
    }
    let img = RgbaImage::from_raw(tw, th, buf).ok_or_else(|| anyhow!("bad thumbnail"))?;
    Ok(rotate_img(img, rotation))
}

pub fn rotate_img(img: RgbaImage, rotation: i32) -> RgbaImage {
    match rotation.rem_euclid(360) {
        90 => image::imageops::rotate90(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate270(&img),
        _ => img,
    }
}

pub fn ffmpeg_version() -> String {
    unsafe {
        let v = ff::ffi::av_version_info();
        if v.is_null() { "unknown".into() } else { std::ffi::CStr::from_ptr(v).to_string_lossy().to_string() }
    }
}

// ------------------------------------------------------------ native planes
//
// Zero-copy access to a `YuvFrame`'s samples for analysis code. A
// `YuvFrame` stores each plane tightly packed (`stride == width`, in samples)
// as `u8` (8-bit) or `u16` (10/12/16-bit, little-endian values, not shifted).
// What a `YuvFrame` does not carry (chroma subsampling, matrix, range,
// transfer) comes with it as a `NativeFormat`: `SeqDecoder::format()` and
// `RangeReader::format()` describe the frames they hand out. Build borrowed
// views with `native_view(&frame, &format)`, then narrow them with
// `NativeView::crop(roi)` / `PlaneSlice::window(..)` without copying.

/// Pixel rectangle in luma (full-resolution) coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[allow(dead_code)] // Used by the analysis panel.
impl Roi {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Roi {
        Roi { x, y, w, h }
    }

    /// Clamped to a `width` x `height` frame (never empty) and grown outward
    /// to whole chroma samples: with `chroma_shift` (1, 1) for 4:2:0 the
    /// corners land on even coordinates, so the luma and chroma windows
    /// cover exactly the same picture area.
    pub fn snapped(self, width: u32, height: u32, chroma_shift: (u32, u32)) -> Roi {
        let (width, height) = (width.max(1), height.max(1));
        let (mx, my) = ((1u32 << chroma_shift.0) - 1, (1u32 << chroma_shift.1) - 1);
        let x0 = self.x.min(width - 1) & !mx;
        let y0 = self.y.min(height - 1) & !my;
        let x1 = (self.x.saturating_add(self.w.max(1)).min(width).saturating_add(mx) & !mx).min(width).max(x0 + 1);
        let y1 = (self.y.saturating_add(self.h.max(1)).min(height).saturating_add(my) & !my).min(height).max(y0 + 1);
        Roi { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }
}

/// YUV to RGB matrix of a stream (FFmpeg `colorspace`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020Ncl,
    Bt2020Cl,
    Smpte240m,
    Fcc,
    YCgCo,
    /// RGB stored in planes (GBR order, as FFmpeg does).
    Identity,
}

/// Sample range: limited ("TV", 16-235 for 8-bit luma) or full ("PC").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvRange {
    Limited,
    Full,
}

/// Transfer characteristic of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// BT.709 / BT.601 / unspecified SDR gamma.
    Sdr,
    Srgb,
    Pq,
    Hlg,
    Other,
}

/// Layout and color meaning of the planes of a `YuvFrame`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeFormat {
    /// Size of the luma plane as copied (the ROI size when cropped).
    pub width: u32,
    pub height: u32,
    /// 1 (gray) or 3 (Y, U, V).
    pub planes: usize,
    /// Significant bits per sample: 8 for `u8` planes, 10/12/16 for `u16`
    /// planes. Deep formats that had to be normalised (P010, ...) report 16
    /// and use the full 16-bit range.
    pub bit_depth: u32,
    /// log2 of the chroma subsampling: (1, 1) 4:2:0, (1, 0) 4:2:2, (0, 0) 4:4:4.
    pub chroma_shift: (u32, u32),
    /// The matrix to decode with: as signalled, or the guess playback uses
    /// for unspecified streams (BT.709 from 720 lines up, else BT.601).
    pub matrix: YuvMatrix,
    /// False when `matrix` is a guess.
    pub matrix_signalled: bool,
    pub range: YuvRange,
    pub transfer: Transfer,
}

impl NativeFormat {
    /// `src` is the decoded frame, `copied` the frame the planes were copied
    /// from (the same frame, or its normalised version).
    fn of(src: &ff::frame::Video, copied: &ff::frame::Video, y: &YuvFrame) -> NativeFormat {
        use ff::color::Space;
        use ff::format::Pixel as P;
        let rgb_src = unsafe {
            let d = ff::ffi::av_pix_fmt_desc_get(src.format().into());
            !d.is_null() && (*d).flags & ff::ffi::AV_PIX_FMT_FLAG_RGB as u64 != 0
        };
        let (matrix, matrix_signalled) = match src.color_space() {
            // swscale converted RGB sources with its default BT.601 matrix.
            _ if rgb_src => (YuvMatrix::Bt601, false),
            Space::BT709 => (YuvMatrix::Bt709, true),
            Space::BT470BG | Space::SMPTE170M => (YuvMatrix::Bt601, true),
            Space::BT2020NCL => (YuvMatrix::Bt2020Ncl, true),
            Space::BT2020CL => (YuvMatrix::Bt2020Cl, true),
            Space::SMPTE240M => (YuvMatrix::Smpte240m, true),
            Space::FCC => (YuvMatrix::Fcc, true),
            Space::YCGCO => (YuvMatrix::YCgCo, true),
            Space::RGB => (YuvMatrix::Identity, true),
            _ if src.height() >= 720 => (YuvMatrix::Bt709, false),
            _ => (YuvMatrix::Bt601, false),
        };
        let full = !rgb_src && (matches!(src.color_range(), ff::color::Range::JPEG) || matches!(src.format(), P::YUVJ420P | P::YUVJ422P | P::YUVJ444P));
        use ff::color::TransferCharacteristic as T;
        let transfer = match src.color_transfer_characteristic() {
            T::SMPTE2084 => Transfer::Pq,
            T::ARIB_STD_B67 => Transfer::Hlg,
            T::IEC61966_2_1 => Transfer::Srgb,
            T::BT709 | T::SMPTE170M | T::GAMMA22 | T::GAMMA28 | T::BT2020_10 | T::BT2020_12 | T::Unspecified => Transfer::Sdr,
            _ => Transfer::Other,
        };
        let (width, height) = y.planes.first().map(|p| (p.width as u32, p.height as u32)).unwrap_or((0, 0));
        NativeFormat {
            width,
            height,
            planes: y.planes.len(),
            bit_depth: y.bit_depth,
            chroma_shift: chroma_shift(copied.format()),
            matrix,
            matrix_signalled,
            range: if full { YuvRange::Full } else { YuvRange::Limited },
            transfer,
        }
    }
}

/// Borrowed samples of one plane.
#[derive(Clone, Copy, Debug)]
pub enum Samples<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
}

/// A borrowed (possibly windowed) plane: row `y` starts at `y * stride`
/// samples into `samples` and holds `width` samples.
#[derive(Clone, Copy, Debug)]
pub struct PlaneSlice<'a> {
    pub samples: Samples<'a>,
    pub width: usize,
    pub height: usize,
    /// Distance between row starts, in samples (not bytes).
    pub stride: usize,
}

#[allow(dead_code)] // Used by the analysis panel.
impl<'a> PlaneSlice<'a> {
    pub fn of(p: &'a Plane) -> PlaneSlice<'a> {
        let samples = match &p.data {
            PlaneData::U8(v) => Samples::U8(v),
            PlaneData::U16(v) => Samples::U16(v),
        };
        PlaneSlice { samples, width: p.width, height: p.height, stride: p.width }
    }

    /// The `w` x `h` window at (`x`, `y`), sharing the samples; `None` when
    /// it does not fit.
    pub fn window(&self, x: usize, y: usize, w: usize, h: usize) -> Option<PlaneSlice<'a>> {
        if x + w > self.width || y + h > self.height {
            return None;
        }
        let start = y * self.stride + x;
        let len = if h == 0 || w == 0 { 0 } else { (h - 1) * self.stride + w };
        let samples = match self.samples {
            Samples::U8(s) => Samples::U8(s.get(start..start + len)?),
            Samples::U16(s) => Samples::U16(s.get(start..start + len)?),
        };
        Some(PlaneSlice { samples, width: w, height: h, stride: self.stride })
    }

    pub fn row_u8(&self, y: usize) -> Option<&'a [u8]> {
        match self.samples {
            Samples::U8(s) if y < self.height => s.get(y * self.stride..y * self.stride + self.width),
            _ => None,
        }
    }

    pub fn row_u16(&self, y: usize) -> Option<&'a [u16]> {
        match self.samples {
            Samples::U16(s) if y < self.height => s.get(y * self.stride..y * self.stride + self.width),
            _ => None,
        }
    }
}

/// Borrowed planes of a `YuvFrame` plus their format.
#[derive(Clone, Debug)]
pub struct NativeView<'a> {
    pub planes: Vec<PlaneSlice<'a>>,
    pub format: NativeFormat,
}

#[allow(dead_code)] // Used by the analysis panel.
impl<'a> NativeView<'a> {
    /// The same picture area of every plane (`roi` in luma pixels, snapped
    /// outward to the chroma grid); no samples are copied.
    pub fn crop(&self, roi: Roi) -> NativeView<'a> {
        let f = self.format;
        let r = roi.snapped(f.width, f.height, f.chroma_shift);
        let planes = self
            .planes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (sx, sy) = if i == 0 { (0, 0) } else { f.chroma_shift };
                let (x0, y0) = ((r.x >> sx) as usize, (r.y >> sy) as usize);
                let x1 = (((r.x + r.w) as usize + (1 << sx) - 1) >> sx).min(p.width);
                let y1 = (((r.y + r.h) as usize + (1 << sy) - 1) >> sy).min(p.height);
                p.window(x0.min(x1), y0.min(y1), x1.saturating_sub(x0), y1.saturating_sub(y0)).unwrap_or(*p)
            })
            .collect();
        NativeView { planes, format: NativeFormat { width: r.w, height: r.h, ..f } }
    }
}

/// Borrow `frame`'s planes (`format` from the decoder that produced it).
#[allow(dead_code)] // Used by the analysis panel.
pub fn native_view<'a>(frame: &'a YuvFrame, format: &NativeFormat) -> NativeView<'a> {
    NativeView { planes: frame.planes.iter().map(PlaneSlice::of).collect(), format: *format }
}

// ------------------------------------------------------- sequential decoding

/// Frames to read, in indices of the frame index (presentation order, as the
/// timeline numbers them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FrameRange {
    pub start: usize,
    /// Exclusive; `None` reads to the end of the file.
    pub end: Option<usize>,
    /// Every `stride`-th frame from `start` (1 = every frame). Frames in
    /// between are still decoded (they are references) but not copied, and
    /// long gaps are crossed with a keyframe seek.
    pub stride: usize,
    /// Only keyframes in the range (`stride` is ignored); nothing else is decoded.
    pub keyframes_only: bool,
}

impl Default for FrameRange {
    fn default() -> FrameRange {
        FrameRange::ALL
    }
}

#[allow(dead_code)] // Used by the analysis panel.
impl FrameRange {
    pub const ALL: FrameRange = FrameRange { start: 0, end: None, stride: 1, keyframes_only: false };

    pub fn new(start: usize, end: Option<usize>, stride: usize) -> FrameRange {
        FrameRange { start, end, stride: stride.max(1), keyframes_only: false }
    }

    pub fn is_all(&self) -> bool {
        self.start == 0 && self.end.is_none() && self.stride <= 1 && !self.keyframes_only
    }

    /// Whether `idx` is inside the range and on its stride grid (keyframes
    /// are checked separately, against the index).
    pub fn wants(&self, idx: usize) -> bool {
        idx >= self.start && self.end.is_none_or(|e| idx < e) && (idx - self.start) % self.stride.max(1) == 0
    }

    /// Frames read from a file of `total` frames (not counting `keyframes_only`).
    pub fn count(&self, total: usize) -> usize {
        let end = self.end.unwrap_or(total).min(total);
        if end <= self.start { 0 } else { (end - self.start).div_ceil(self.stride.max(1)) }
    }
}

/// A forward gap longer than this (frames) without a frame index builds one
/// so the gap can be crossed with a seek.
const INDEX_FOR_GAP: usize = 1000;
/// Forward gaps are crossed with a seek when the last keyframe in the gap is
/// more than this many frames ahead (decoding up to it costs more).
const SEEK_FOR_GAP: usize = 8;

/// Sequential decoder producing native planes in presentation order (used by
/// analysis; independent from playback). Frames are numbered like the
/// timeline: positions in the frame index. Seeking needs the index; it is
/// built on first use unless one is given with [`SeqDecoder::set_index`].
pub struct SeqDecoder {
    path: PathBuf,
    input: ff::format::context::Input,
    vidx: usize,
    tb: f64,
    start: f64,
    frame_dur: f64,
    dec: ff::decoder::Video,
    native: NativeConverter,
    eof: bool,
    flushed: bool,
    index: Option<Arc<Vec<IndexEntry>>>,
    /// A decoded frame (and its index) looked at but not handed out yet.
    pending: Option<(usize, ff::frame::Video)>,
    /// Index of the last frame decoded (none at the start and after a seek).
    last: Option<usize>,
    keyframes_only: bool,
    /// Packets well before this pts are decoded without non-reference frames.
    discard_before: f64,
    roi: Option<Roi>,
    format: Option<NativeFormat>,
}

#[allow(dead_code)] // Range, seek and ROI access are used by the analysis panel.
impl SeqDecoder {
    pub fn open(path: &Path) -> Result<SeqDecoder> {
        init();
        let input = ff::format::input(path)?;
        let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
        let vidx = vs.index();
        let tb = q2f(vs.time_base());
        let start = if vs.start_time() != ff::ffi::AV_NOPTS_VALUE { vs.start_time() as f64 * tb } else { 0.0 };
        let fps = [q2f(vs.avg_frame_rate()), q2f(vs.rate())].into_iter().find(|f| *f > 0.0).unwrap_or(25.0);
        let mut ctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
        ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: SIDE_DECODE_THREADS });
        let dec = ctx.decoder().video()?;
        Ok(SeqDecoder {
            path: path.to_path_buf(),
            input,
            vidx,
            tb,
            start,
            frame_dur: 1.0 / fps,
            dec,
            native: NativeConverter::default(),
            eof: false,
            flushed: false,
            index: None,
            pending: None,
            last: None,
            keyframes_only: false,
            discard_before: f64::NEG_INFINITY,
            roi: None,
            format: None,
        })
    }

    /// Use an index built elsewhere (the player's), so seeking does not need
    /// another pass over the file.
    pub fn set_index(&mut self, index: Arc<Vec<IndexEntry>>) {
        self.index = Some(index);
    }

    /// The frame index, built now (a demux-only pass) if not known yet.
    pub fn index(&mut self) -> Result<Arc<Vec<IndexEntry>>> {
        if let Some(ix) = &self.index {
            return Ok(ix.clone());
        }
        let ix = Arc::new(build_index(&self.path, &AtomicBool::new(false), &|_| {})?);
        self.index = Some(ix.clone());
        Ok(ix)
    }

    /// Coded size of the stream (before any ROI).
    pub fn size(&self) -> (u32, u32) {
        (self.dec.width(), self.dec.height())
    }

    /// Copy only this area of each frame from now on (luma pixels, snapped
    /// outward to the chroma grid and clamped to the frame); `None` for the
    /// whole frame.
    pub fn set_roi(&mut self, roi: Option<Roi>) {
        self.roi = roi;
    }

    /// Layout of the last frame handed out (`None` before the first).
    pub fn format(&self) -> Option<NativeFormat> {
        self.format
    }

    /// Index the next decoded frame is expected to have.
    pub fn position(&self) -> usize {
        match (&self.pending, self.last) {
            (Some((i, _)), _) => *i,
            (None, Some(l)) => l + 1,
            (None, None) => 0,
        }
    }

    /// Next frame in presentation order.
    pub fn next(&mut self) -> Option<YuvFrame> {
        self.next_indexed().map(|(_, f)| f)
    }

    /// Next frame in presentation order with its index.
    pub fn next_indexed(&mut self) -> Option<(usize, YuvFrame)> {
        let (i, f) = self.next_raw()?;
        let (y, nf) = self.native.convert(&f, self.roi).ok()?;
        self.format = Some(nf);
        Some((i, y))
    }

    /// Decode `n` frames without copying their planes; returns how many there were.
    pub fn skip(&mut self, n: usize) -> usize {
        (0..n).take_while(|_| self.next_raw().is_some()).count()
    }

    /// Position so the next frame handed out is frame `idx`: seek to the
    /// keyframe at or before it (earlier ones if open-GOP leading frames are
    /// dropped), then decode and discard up to it. Returns the index the next
    /// frame will have: `idx`, or the following one if `idx` cannot be
    /// decoded (or is not a keyframe while reading keyframes only).
    pub fn seek_to_frame(&mut self, idx: usize) -> Result<usize> {
        let index = self.index()?;
        let target = index.get(idx).ok_or_else(|| anyhow!("Frame {idx} is past the end ({} frames)", index.len()))?.pts;
        // Seek points, latest first: keyframes at or before the target, then
        // the start of the file.
        let mut points = index[..=idx].iter().rev().filter(|e| e.key).map(|e| e.pts).chain(std::iter::once(index[0].pts.min(0.0)));
        let mut at = points.next().unwrap_or(0.0);
        'seek: loop {
            self.demux_seek(at);
            self.discard_before = target;
            let mut first = true;
            loop {
                let Some((i, f)) = self.next_raw() else {
                    self.discard_before = f64::NEG_INFINITY;
                    return Err(anyhow!("Frame {idx} could not be decoded"));
                };
                if first && i > idx {
                    // Landed after the target: start from an earlier keyframe.
                    if let Some(p) = points.next() {
                        at = p;
                        continue 'seek;
                    }
                }
                first = false;
                if i >= idx {
                    self.discard_before = f64::NEG_INFINITY;
                    self.pending = Some((i, f));
                    return Ok(i);
                }
            }
        }
    }

    /// Read only keyframes from now on (non-key packets are not decoded).
    pub fn set_keyframes_only(&mut self, on: bool) {
        self.keyframes_only = on;
    }

    /// Read `range` (see [`FrameRange`]) as `(index, frame)` pairs.
    pub fn into_range(mut self, range: FrameRange) -> RangeReader {
        self.keyframes_only = range.keyframes_only;
        let range = FrameRange { stride: range.stride.max(1), ..range };
        RangeReader { dec: self, want: Want::Range { range, next: range.start }, error: None, done: false }
    }

    /// Read the frames at `indices` (sorted ascending), e.g. another
    /// stream's keyframes shifted by an offset.
    pub fn into_frames(self, mut indices: Vec<usize>) -> RangeReader {
        indices.sort_unstable();
        indices.dedup();
        RangeReader { dec: self, want: Want::List(indices.into()), error: None, done: false }
    }

    /// Make frame `target` (or the first one after it) the next one handed
    /// out, by decoding forward or seeking, whichever is cheaper. False at
    /// the end of the stream.
    fn advance_to(&mut self, target: usize) -> Result<bool> {
        let pos = self.position();
        if target < pos {
            self.seek_to_frame(target)?;
            return Ok(true);
        }
        if target == pos && (self.pending.is_some() || self.last.is_some()) {
            return Ok(true);
        }
        if self.index.is_none() && (self.keyframes_only || target - pos > INDEX_FOR_GAP) {
            self.index()?;
        }
        if let Some(ix) = &self.index {
            if !self.keyframes_only {
                let last_key = ix.get(pos + 1..=target.min(ix.len().saturating_sub(1))).and_then(|s| s.iter().rposition(|e| e.key));
                if last_key.is_some_and(|k| k + 1 > SEEK_FOR_GAP) {
                    self.seek_to_frame(target)?;
                    return Ok(true);
                }
            }
            if let Some(e) = ix.get(target) {
                self.discard_before = e.pts;
            }
        }
        let found = loop {
            match self.next_raw() {
                Some((i, f)) if i >= target => {
                    self.pending = Some((i, f));
                    break true;
                }
                Some(_) => {}
                None => break false,
            }
        };
        self.discard_before = f64::NEG_INFINITY;
        Ok(found)
    }

    /// Position the demuxer at `at` (a keyframe's pts) and flush the decoder.
    fn demux_seek(&mut self, at: f64) {
        // Round: truncating could land one tick before the keyframe.
        let ts = ((at + self.start) / self.tb).round() as i64;
        unsafe {
            ff::ffi::av_seek_frame(self.input.as_mut_ptr(), self.vidx as i32, ts, ff::ffi::AVSEEK_FLAG_BACKWARD as i32);
        }
        self.dec.flush();
        self.eof = false;
        self.flushed = false;
        self.pending = None;
        self.last = None;
    }

    /// Index of a frame just decoded: from its timestamp when the index is
    /// known (the timeline's numbering), else counted.
    fn frame_index(&self, f: &ff::frame::Video) -> usize {
        let counted = self.last.map(|l| l + 1).unwrap_or(0);
        let pts = f.timestamp().or(f.pts()).map(|t| t as f64 * self.tb - self.start);
        let i = match (&self.index, pts) {
            (Some(ix), Some(p)) if !ix.is_empty() => ix.partition_point(|e| e.pts <= p + 1e-4).saturating_sub(1),
            _ => counted,
        };
        // Repeated timestamps must not repeat an index.
        match self.last {
            Some(l) if i <= l => l + 1,
            _ => i,
        }
    }

    /// Next decoded frame (the pending one first) with its index, not copied.
    fn next_raw(&mut self) -> Option<(usize, ff::frame::Video)> {
        if let Some(p) = self.pending.take() {
            self.last = Some(p.0);
            return Some(p);
        }
        let f = self.decode()?;
        let i = self.frame_index(&f);
        self.last = Some(i);
        Some((i, f))
    }

    fn decode(&mut self) -> Option<ff::frame::Video> {
        if self.keyframes_only {
            return self.decode_keyframe();
        }
        let mut frame = ff::frame::Video::empty();
        loop {
            match self.dec.receive_frame(&mut frame) {
                Ok(()) => return Some(frame),
                Err(ff::Error::Eof) => return None,
                Err(_) => {}
            }
            if self.eof {
                if self.flushed {
                    return None;
                }
                self.flushed = true;
                let _ = self.dec.send_eof();
                continue;
            }
            let mut pkt = ff::Packet::empty();
            match pkt.read(&mut self.input) {
                Ok(()) => {
                    if pkt.stream() != self.vidx {
                        continue;
                    }
                    // Frames well before a seek target are only decoded when
                    // others need them (the index numbers the rest by pts).
                    // A few frames of margin: discarding must not reach the target.
                    let discard = self.index.is_some() && pkt.pts().is_some_and(|p| (p as f64 * self.tb - self.start) < self.discard_before - 4.0 * self.frame_dur);
                    unsafe {
                        (*self.dec.as_mut_ptr()).skip_frame = if discard { ff::ffi::AVDiscard::AVDISCARD_NONREF } else { ff::ffi::AVDiscard::AVDISCARD_DEFAULT };
                    }
                    let _ = self.dec.send_packet(&pkt);
                }
                Err(_) => self.eof = true,
            }
        }
    }

    /// The next keyframe, decoded on its own: each one goes into a freshly
    /// flushed decoder that is then drained. Feeding keyframes alone into one
    /// decoding session confuses reordering (HEVC derives picture order from
    /// the frames left out), so frames would come out late or out of order.
    fn decode_keyframe(&mut self) -> Option<ff::frame::Video> {
        let mut frame = ff::frame::Video::empty();
        while !self.eof {
            let mut pkt = ff::Packet::empty();
            if pkt.read(&mut self.input).is_err() {
                self.eof = true;
                break;
            }
            if pkt.stream() != self.vidx || !pkt.is_key() {
                continue;
            }
            self.dec.flush();
            unsafe {
                (*self.dec.as_mut_ptr()).skip_frame = ff::ffi::AVDiscard::AVDISCARD_DEFAULT;
            }
            let ok = self.dec.send_packet(&pkt).is_ok() && self.dec.send_eof().is_ok() && self.dec.receive_frame(&mut frame).is_ok();
            // Back to a decoder that accepts packets (draining ended it).
            self.dec.flush();
            if ok {
                return Some(frame);
            }
        }
        None
    }
}

enum Want {
    Range { range: FrameRange, next: usize },
    List(VecDeque<usize>),
}

/// Iterator over `(index, frame)` for a [`FrameRange`] or a list of frame
/// indices (see [`SeqDecoder::into_range`], [`SeqDecoder::into_frames`]).
/// Indices that do not exist in the file are skipped; iteration ends at the
/// end of the file or on an error (see [`RangeReader::error`]).
pub struct RangeReader {
    dec: SeqDecoder,
    want: Want,
    error: Option<anyhow::Error>,
    done: bool,
}

#[allow(dead_code)] // Used by the analysis panel.
impl RangeReader {
    pub fn decoder(&self) -> &SeqDecoder {
        &self.dec
    }

    pub fn decoder_mut(&mut self) -> &mut SeqDecoder {
        &mut self.dec
    }

    /// Layout of the frames handed out so far.
    pub fn format(&self) -> Option<NativeFormat> {
        self.dec.format()
    }

    /// Why iteration stopped early (a failed seek or index pass); `None`
    /// after a normal end.
    pub fn error(&self) -> Option<&anyhow::Error> {
        self.error.as_ref()
    }

    /// Next index wanted, at or after `from`.
    fn target(&mut self) -> Result<Option<usize>> {
        match &mut self.want {
            Want::Range { range, next } => {
                let (range, next) = (*range, *next);
                if range.end.is_some_and(|e| next >= e) {
                    return Ok(None);
                }
                if !range.keyframes_only {
                    return Ok(Some(next));
                }
                let ix = self.dec.index()?;
                let end = range.end.unwrap_or(usize::MAX).min(ix.len());
                Ok((next..end).find(|&i| ix[i].key))
            }
            Want::List(l) => Ok(l.front().copied()),
        }
    }

    /// Whether frame `i` is wanted; drops everything up to `i` from the wish list.
    fn take(&mut self, i: usize) -> bool {
        match &mut self.want {
            Want::Range { range, next } => {
                let wanted = if range.keyframes_only {
                    i >= range.start && range.end.is_none_or(|e| i < e) && self.dec.index.as_ref().is_some_and(|ix| ix.get(i).is_some_and(|e| e.key))
                } else {
                    range.wants(i)
                };
                *next = if range.keyframes_only || i < range.start { (i + 1).max(range.start) } else { range.start + ((i - range.start) / range.stride + 1) * range.stride };
                wanted
            }
            Want::List(l) => {
                while l.front().is_some_and(|&f| f < i) {
                    l.pop_front();
                }
                l.front() == Some(&i) && l.pop_front().is_some()
            }
        }
    }
}

impl Iterator for RangeReader {
    type Item = (usize, YuvFrame);

    fn next(&mut self) -> Option<(usize, YuvFrame)> {
        while !self.done {
            let target = match self.target() {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(e) => {
                    self.error = Some(e);
                    break;
                }
            };
            match self.dec.advance_to(target) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    self.error = Some(e);
                    break;
                }
            }
            // The next frame is the target, or a later one if it is missing.
            let Some((i, frame)) = self.dec.next_indexed() else { break };
            if self.take(i) {
                return Some((i, frame));
            }
        }
        self.done = true;
        None
    }
}

/// Decode the frame displayed at `pts` at native resolution (for snapshots).
/// Runs independently of playback so it never stalls or disturbs the player.
pub fn frame_at(path: &Path, pts: f64) -> Result<RgbaImage> {
    init();
    let _slot = FrameAtSlot::acquire();
    let mut input = ff::format::input(path)?;
    let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
    let vidx = vs.index();
    let tb = q2f(vs.time_base());
    let start = if vs.start_time() != ff::ffi::AV_NOPTS_VALUE { vs.start_time() as f64 * tb } else { 0.0 };
    let rotation = stream_rotation(&vs);
    let mut ctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
    ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: SIDE_DECODE_THREADS });
    let mut dec = ctx.decoder().video()?;
    let ts = ((pts.max(0.0) + start) / tb) as i64;
    unsafe {
        ff::ffi::av_seek_frame(input.as_mut_ptr(), vidx as i32, ts, ff::ffi::AVSEEK_FLAG_BACKWARD as i32);
    }
    let mut frame = ff::frame::Video::empty();
    let mut best: Option<ff::frame::Video> = None;
    let mut pkt: ff::Packet;
    let mut eof = false;
    'outer: loop {
        while dec.receive_frame(&mut frame).is_ok() {
            let fp = frame.timestamp().or(frame.pts()).unwrap_or(0) as f64 * tb - start;
            let done = fp >= pts - 1e-4;
            // Keep the latest frame by moving it (a clone copies the pixels);
            // the frame it replaces goes back to the decoder's pool.
            best = Some(std::mem::replace(&mut frame, ff::frame::Video::empty()));
            if done {
                break 'outer;
            }
        }
        if eof {
            break;
        }
        match { pkt = ff::Packet::empty(); pkt.read(&mut input) } {
            Ok(()) => {
                if pkt.stream() == vidx {
                    let _ = dec.send_packet(&pkt);
                }
            }
            Err(_) => {
                eof = true;
                let _ = dec.send_eof();
            }
        }
    }
    let f = best.ok_or_else(|| anyhow!("Could not decode the frame"))?;
    let (w, h) = (f.width(), f.height());
    let mut sc = ff::software::scaling::Context::get(
        f.format(),
        w,
        h,
        ff::format::Pixel::RGBA,
        w,
        h,
        ff::software::scaling::Flags::BICUBIC | ff::software::scaling::Flags::ACCURATE_RND | ff::software::scaling::Flags::FULL_CHR_H_INT,
    )?;
    set_colorspace(&mut sc, &f);
    let mut out = ff::frame::Video::empty();
    sc.run(&f, &mut out)?;
    let stride = out.stride(0);
    let data = out.data(0);
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        buf.extend_from_slice(&data[y * stride..y * stride + w as usize * 4]);
    }
    let img = RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("bad frame"))?;
    Ok(rotate_img(img, rotation))
}

#[cfg(test)]
pub(crate) mod test_media {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    /// A scratch directory under the system temp dir, removed on drop.
    pub struct TestDir(PathBuf);

    impl TestDir {
        pub fn new(name: &str) -> TestDir {
            let p = std::env::temp_dir().join(format!("lumen-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }

        pub fn path(&self, file: &str) -> PathBuf {
            self.0.join(file)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a 4:2:0 8-bit Y4M clip; `px(frame, plane, x, y)` gives each sample.
    pub fn write_y4m(path: &Path, w: usize, h: usize, frames: usize, px: impl Fn(usize, usize, usize, usize) -> u8) {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        writeln!(out, "YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420jpeg").unwrap();
        for f in 0..frames {
            out.write_all(b"FRAME\n").unwrap();
            for (plane, (pw, ph)) in [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))].into_iter().enumerate() {
                let data: Vec<u8> = (0..ph).flat_map(|y| (0..pw).map(move |x| (x, y))).map(|(x, y)| px(f, plane, x, y)).collect();
                out.write_all(&data).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_media::{write_y4m, TestDir};
    use super::*;
    use std::hash::{Hash, Hasher};

    fn hash(f: &YuvFrame) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        f.bit_depth.hash(&mut h);
        for p in &f.planes {
            (p.width, p.height).hash(&mut h);
            match &p.data {
                PlaneData::U8(v) => v.hash(&mut h),
                PlaneData::U16(v) => v.hash(&mut h),
            }
        }
        h.finish()
    }

    fn sample(p: &Plane, x: usize, y: usize) -> u32 {
        p.get(y * p.width + x)
    }

    /// Frame `f` of the test clip: every frame and plane differs.
    fn px(f: usize, plane: usize, x: usize, y: usize) -> u8 {
        ((f * 7 + plane * 60 + x + y * 3) % 251) as u8
    }

    fn clip(dir: &TestDir) -> PathBuf {
        let p = dir.path("clip.y4m");
        write_y4m(&p, 33, 25, 30, px);
        p
    }

    #[test]
    fn sequential_seek_skip_and_ranges_on_y4m() {
        let dir = TestDir::new("engine-seq");
        let p = clip(&dir);
        let mut d = SeqDecoder::open(&p).unwrap();
        let all: Vec<(usize, YuvFrame)> = std::iter::from_fn(|| d.next_indexed()).collect();
        assert_eq!(all.iter().map(|f| f.0).collect::<Vec<_>>(), (0..30).collect::<Vec<_>>());
        let fmt = d.format().unwrap();
        assert_eq!((fmt.width, fmt.height, fmt.planes, fmt.bit_depth, fmt.chroma_shift), (33, 25, 3, 8, (1, 1)));
        assert_eq!(sample(&all[4].1.planes[2], 3, 2), px(4, 2, 3, 2) as u32);
        let hashes: Vec<u64> = all.iter().map(|f| hash(&f.1)).collect();

        // Seeks forward, backward and to the same frame.
        let mut d = SeqDecoder::open(&p).unwrap();
        for k in [12, 3, 3, 29, 0, 17] {
            assert_eq!(d.seek_to_frame(k).unwrap(), k);
            let (i, f) = d.next_indexed().unwrap();
            assert_eq!((i, hash(&f)), (k, hashes[k]), "seek to {k}");
        }
        assert!(d.seek_to_frame(30).is_err());
        // After a seek, decoding carries on in order.
        d.seek_to_frame(5).unwrap();
        assert_eq!(d.skip(3), 3);
        assert_eq!(d.position(), 8);
        let (i, f) = d.next_indexed().unwrap();
        assert_eq!((i, hash(&f)), (8, hashes[8]));
        assert_eq!(d.skip(100), 21);
        assert!(d.next_indexed().is_none());

        let read = |r: FrameRange| -> Vec<usize> {
            SeqDecoder::open(&p)
                .unwrap()
                .into_range(r)
                .map(|(i, f)| {
                    assert_eq!(hash(&f), hashes[i]);
                    i
                })
                .collect()
        };
        assert_eq!(read(FrameRange::ALL), (0..30).collect::<Vec<_>>());
        assert_eq!(read(FrameRange::new(2, Some(20), 5)), vec![2, 7, 12, 17]);
        assert_eq!(read(FrameRange::new(25, Some(100), 1)), vec![25, 26, 27, 28, 29]);
        assert_eq!(read(FrameRange::new(40, None, 1)), Vec::<usize>::new());
        assert_eq!(read(FrameRange { keyframes_only: true, ..FrameRange::new(27, None, 9) }), vec![27, 28, 29]);
        assert_eq!(FrameRange::new(2, Some(20), 5).count(30), 4);
        let list: Vec<usize> = SeqDecoder::open(&p).unwrap().into_frames(vec![9, 1, 40, 9, 4]).map(|(i, _)| i).collect();
        assert_eq!(list, vec![1, 4, 9]);
    }

    #[test]
    fn roi_copies_and_views_the_same_samples() {
        let dir = TestDir::new("engine-roi");
        let p = clip(&dir);
        // Odd corner and size: snapped outward to even luma coordinates.
        let roi = Roi::new(5, 3, 10, 6);
        assert_eq!(roi.snapped(33, 25, (1, 1)), Roi::new(4, 2, 12, 8));
        assert_eq!(Roi::new(30, 20, 50, 50).snapped(33, 25, (1, 1)), Roi::new(30, 20, 3, 5));
        assert_eq!(roi.snapped(33, 25, (0, 0)), roi);
        let mut d = SeqDecoder::open(&p).unwrap();
        d.set_roi(Some(roi));
        d.seek_to_frame(6).unwrap();
        let (_, crop) = d.next_indexed().unwrap();
        let cf = d.format().unwrap();
        assert_eq!((cf.width, cf.height), (12, 8));
        assert_eq!((crop.planes[0].width, crop.planes[0].height, crop.planes[1].width, crop.planes[1].height), (12, 8, 6, 4));
        assert_eq!(sample(&crop.planes[0], 0, 0), px(6, 0, 4, 2) as u32);
        assert_eq!(sample(&crop.planes[1], 1, 1), px(6, 1, 3, 2) as u32);
        // Right/bottom edge of an odd-sized frame: the partial chroma sample is kept.
        let mut d = SeqDecoder::open(&p).unwrap();
        d.set_roi(Some(Roi::new(30, 20, 9, 9)));
        let (_, edge) = d.next_indexed().unwrap();
        assert_eq!((edge.planes[0].width, edge.planes[0].height, edge.planes[1].width, edge.planes[1].height), (3, 5, 2, 3));

        // A borrowed view of the full frame, cropped, matches the cropped copy.
        let mut d = SeqDecoder::open(&p).unwrap();
        d.seek_to_frame(6).unwrap();
        let (_, full) = d.next_indexed().unwrap();
        let view = native_view(&full, &d.format().unwrap());
        let v = view.crop(roi);
        assert_eq!((v.format.width, v.format.height), (12, 8));
        for (pv, pc) in v.planes.iter().zip(&crop.planes) {
            assert_eq!((pv.width, pv.height, pv.stride), (pc.width, pc.height, if pc.width == 12 { 33 } else { 17 }));
            for y in 0..pc.height {
                let row: Vec<u32> = pv.row_u8(y).unwrap().iter().map(|&s| s as u32).collect();
                let want: Vec<u32> = (0..pc.width).map(|x| sample(pc, x, y)).collect();
                assert_eq!(row, want);
            }
        }
        assert!(view.planes[0].window(30, 0, 4, 1).is_none());
        assert!(view.planes[0].row_u16(0).is_none());
    }

    /// The H.264 screen recording used for manual checks (read only), or
    /// `LUMEN_TEST_VIDEO`. Skipped when neither exists.
    /// `LUMEN_TEST_FRAMES` sets how many leading frames are checked.
    fn recording() -> Option<PathBuf> {
        let p = std::env::var_os("LUMEN_TEST_VIDEO").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Users\ttrah\Videos\Screen Recordings\Screen Recording 2025-10-01 205343.mp4"));
        if p.exists() {
            Some(p)
        } else {
            eprintln!("skipped: no test video at {}", p.display());
            None
        }
    }

    fn test_frames() -> usize {
        std::env::var("LUMEN_TEST_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(240)
    }

    #[test]
    fn frame_accurate_on_long_gop_video() {
        let Some(p) = recording() else { return };
        let index = Arc::new(build_index(&p, &AtomicBool::new(false), &|_| {}).unwrap());
        let n = index.len().min(test_frames());
        let keys: Vec<usize> = (0..index.len()).filter(|&i| index[i].key).collect();
        eprintln!("{} frames, keyframes at {:?}", index.len(), &keys[..keys.len().min(20)]);
        let mut d = SeqDecoder::open(&p).unwrap();
        let hashes: Vec<u64> = (0..n)
            .map(|i| {
                let (j, f) = d.next_indexed().unwrap();
                assert_eq!(j, i);
                hash(&f)
            })
            .collect();

        // Seeks, including frames just before and after keyframes and
        // backward jumps.
        let mut targets = vec![n - 1, 1, n / 2, 0, n / 3, n / 3 + 1];
        for &k in &keys {
            for t in [k.saturating_sub(1), k, k + 1] {
                if t < n {
                    targets.push(t);
                }
            }
        }
        let mut d = SeqDecoder::open(&p).unwrap();
        d.set_index(index.clone());
        for &t in &targets {
            assert_eq!(d.seek_to_frame(t).unwrap(), t);
            let (i, f) = d.next_indexed().unwrap();
            assert_eq!((i, hash(&f)), (t, hashes[t]), "seek to {t}");
        }
        // Without an index given (built on first use).
        let mut d = SeqDecoder::open(&p).unwrap();
        d.seek_to_frame(n - 2).unwrap();
        assert_eq!(hash(&d.next().unwrap()), hashes[n - 2]);

        // Strided ranges (gaps crossed by seeking or decoding).
        for stride in [1, 7, 61] {
            let r = FrameRange::new(3, Some(n), stride);
            let got: Vec<(usize, u64)> = SeqDecoder::open(&p).unwrap().into_range(r).map(|(i, f)| (i, hash(&f))).collect();
            let want: Vec<(usize, u64)> = (3..n).step_by(stride).map(|i| (i, hashes[i])).collect();
            assert_eq!(got, want, "stride {stride}");
        }
        // Keyframes only: exactly the keyframes, same pixels.
        let r = FrameRange { keyframes_only: true, ..FrameRange::new(0, Some(n), 1) };
        let got: Vec<(usize, u64)> = SeqDecoder::open(&p).unwrap().into_range(r).map(|(i, f)| (i, hash(&f))).collect();
        let want: Vec<(usize, u64)> = keys.iter().filter(|&&k| k < n).map(|&k| (k, hashes[k])).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn seek_matches_the_player() {
        let Some(p) = recording() else { return };
        let index = build_index(&p, &AtomicBool::new(false), &|_| {}).unwrap();
        let n = index.len().min(test_frames());
        let shared: SharedIndex = Arc::new(parking_lot::Mutex::new(Some(index.clone())));
        let player = Player::open(&p, None, shared, egui::Context::default()).unwrap();
        player.send(Cmd::Native(true));
        let mut d = SeqDecoder::open(&p).unwrap();
        d.set_index(Arc::new(index.clone()));
        for (gen, t) in [n / 2, 5, n - 1, 0, n / 4].into_iter().enumerate() {
            let gen = gen as u64 + 1;
            player.send(Cmd::Seek { t: index[t].pts, exact: true, gen });
            let shown = loop {
                match player.rx.recv_timeout(std::time::Duration::from_secs(20)).expect("player frame") {
                    Event::Frame(f) if f.gen == gen => break f,
                    Event::Error(e) => panic!("{e}"),
                    _ => {}
                }
            };
            assert!((shown.pts - index[t].pts).abs() < 1e-6);
            d.seek_to_frame(t).unwrap();
            let (i, f) = d.next_indexed().unwrap();
            assert_eq!(i, t);
            assert_eq!(hash(shown.native.as_ref().expect("native planes")), hash(&f), "frame {t}");
        }
    }
}
