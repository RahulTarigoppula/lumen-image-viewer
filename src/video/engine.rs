//! FFmpeg-backed media engine: probing, frame index, and a decoder worker per
//! opened file that supports playback, exact seeking and frame stepping.

use super::audio::AudioShared;
use super::metrics::{Plane, PlaneData, YuvFrame};
use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use ffmpeg_next as ff;
use image::RgbaImage;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

static INIT: Once = Once::new();

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
    let mut pkt = ff::Packet::empty();
    let mut n = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("cancelled"));
        }
        match pkt.read(&mut input) {
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
    pub key: bool,
    pub rgba: Arc<RgbaImage>,
    pub native: Option<Arc<YuvFrame>>,
    /// Only for filling the back-step history; don't display.
    pub history: bool,
}

pub enum Cmd {
    Play,
    Pause,
    Seek { t: f64, exact: bool, gen: u64, fill_history: bool },
    Step { gen: u64 },
    OutputSize(u32, u32),
    Native(bool),
    Snapshot(Sender<Result<RgbaImage, String>>),
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

impl Player {
    pub fn open(path: &Path, audio: Option<Arc<AudioShared>>, ctx: egui::Context) -> Result<Player> {
        init();
        let (ctx_tx, ctx_rx) = unbounded::<Cmd>();
        let (ev_tx, ev_rx) = bounded::<Event>(6);
        let mut worker = Worker::new(path, audio)?;
        std::thread::Builder::new()
            .name("lumen-video".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(ctx_rx, ev_tx.clone(), ctx.clone())));
                if r.is_err() {
                    let _ = ev_tx.send(Event::Error("The video decoder crashed".into()));
                    ctx.request_repaint();
                }
            })?;
        Ok(Player { tx: ctx_tx, rx: ev_rx })
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
    scaler: Option<(ff::software::scaling::Context, (ff::format::Pixel, u32, u32, u32, u32))>,
    native_scaler: Option<(ff::software::scaling::Context, (ff::format::Pixel, u32, u32))>,
    out_size: (u32, u32),
    want_native: bool,
    playing: bool,
    gen: u64,
    eof_sent: bool,
    demux_eof: bool,
    vdec_eof: bool,
    pending_vpkts: std::collections::VecDeque<ff::Packet>,
    last_frame: Option<ff::frame::Video>,
    audio_min_pts: f64,
}

impl Worker {
    fn new(path: &Path, audio: Option<Arc<AudioShared>>) -> Result<Worker> {
        let input = ff::format::input(path).with_context(|| format!("Cannot open {}", path.display()))?;
        let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
        let vidx = vs.index();
        let vtb = q2f(vs.time_base());
        let vstart = if vs.start_time() != ff::ffi::AV_NOPTS_VALUE { vs.start_time() as f64 * vtb } else { 0.0 };
        let mut vctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
        vctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
        let vdec = vctx.decoder().video()?;
        let (vw, vh) = (vdec.width(), vdec.height());
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
            scaler: None,
            native_scaler: None,
            out_size: (vw, vh),
            want_native: false,
            playing: false,
            gen: 0,
            eof_sent: false,
            demux_eof: false,
            vdec_eof: false,
            pending_vpkts: Default::default(),
            last_frame: None,
            audio_min_pts: f64::NEG_INFINITY,
        })
    }

    fn run(&mut self, rx: Receiver<Cmd>, tx: Sender<Event>, ctx: egui::Context) {
        // First frame so the UI shows something immediately.
        self.seek(0.0, false, 0, false, &tx, &ctx);
        loop {
            let cmd = if self.playing { rx.try_recv().ok() } else { rx.recv().ok() };
            if let Some(cmd) = cmd {
                match cmd {
                    Cmd::Quit => return,
                    Cmd::Play => self.playing = true,
                    Cmd::Pause => self.playing = false,
                    Cmd::Seek { t, exact, gen, fill_history } => {
                        // Coalesce bursts of seeks (scrubbing): only the latest matters.
                        let mut latest = (t, exact, gen, fill_history);
                        let mut others = Vec::new();
                        while let Ok(c) = rx.try_recv() {
                            match c {
                                Cmd::Seek { t, exact, gen, fill_history } => latest = (t, exact, gen, fill_history),
                                other => others.push(other),
                            }
                        }
                        self.seek(latest.0, latest.1, latest.2, latest.3, &tx, &ctx);
                        for o in others {
                            if self.handle_simple(o, &tx, &ctx) {
                                return;
                            }
                        }
                    }
                    other => {
                        if self.handle_simple(other, &tx, &ctx) {
                            return;
                        }
                    }
                }
                continue;
            }
            if self.playing {
                // Keep audio fed even when the video queue is full.
                let audio_low = self.audio.as_ref().map(|a| a.buffered_secs() < 0.4).unwrap_or(false) && self.aidx.is_some() && !self.demux_eof;
                if tx.len() < 5 {
                    match self.next_frame() {
                        Some(f) => {
                            let vf = self.to_vframe(&f, false);
                            self.last_frame = Some(f);
                            if tx.send(Event::Frame(vf)).is_err() {
                                return;
                            }
                            ctx.request_repaint();
                        }
                        None => {
                            if !self.eof_sent {
                                self.eof_sent = true;
                                let _ = tx.send(Event::Eof { gen: self.gen });
                                ctx.request_repaint();
                            }
                            self.playing = false;
                        }
                    }
                } else if audio_low {
                    self.read_packet();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(3));
                }
            }
        }
    }

    /// Returns true when the worker should exit.
    fn handle_simple(&mut self, cmd: Cmd, tx: &Sender<Event>, ctx: &egui::Context) -> bool {
        match cmd {
            Cmd::Quit => return true,
            Cmd::Play => self.playing = true,
            Cmd::Pause => self.playing = false,
            Cmd::Seek { t, exact, gen, fill_history } => self.seek(t, exact, gen, fill_history, tx, ctx),
            Cmd::Step { gen } => {
                self.gen = gen;
                match self.next_frame() {
                    Some(f) => {
                        let vf = self.to_vframe(&f, false);
                        self.last_frame = Some(f);
                        let _ = tx.send(Event::Frame(vf));
                    }
                    None => {
                        let _ = tx.send(Event::Eof { gen });
                    }
                }
                ctx.request_repaint();
            }
            Cmd::OutputSize(w, h) => self.out_size = (w.max(2), h.max(2)),
            Cmd::Native(on) => self.want_native = on,
            Cmd::Snapshot(reply) => {
                let res = match &self.last_frame {
                    Some(f) => {
                        let (w, h) = (f.width(), f.height());
                        self.convert_rgba(f, w, h).map_err(|e| e.to_string())
                    }
                    None => Err("No frame decoded yet".into()),
                };
                let _ = reply.send(res);
            }
        }
        false
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

    fn seek(&mut self, t: f64, exact: bool, gen: u64, fill_history: bool, tx: &Sender<Event>, ctx: &egui::Context) {
        self.gen = gen;
        self.eof_sent = false;
        self.demux_eof = false;
        self.vdec_eof = false;
        self.pending_vpkts.clear();
        let ts = ((t.max(0.0) + self.vstart) / self.vtb) as i64;
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
        let mut history: Vec<ff::frame::Video> = Vec::new();
        let mut chosen: Option<ff::frame::Video> = None;
        let mut decoded = 0;
        while let Some(f) = self.next_frame() {
            decoded += 1;
            let pts = self.frame_pts(&f);
            if !exact || pts >= t - 1e-4 {
                chosen = Some(f);
                break;
            }
            if fill_history {
                history.push(f.clone());
                if history.len() > 90 {
                    history.remove(0);
                }
            }
            // Keep the latest frame before the target in case the target is past the end.
            chosen = Some(f);
            if decoded > 20_000 {
                break;
            }
        }
        for h in &history {
            let vf = self.to_vframe(h, true);
            if tx.send(Event::Frame(vf)).is_err() {
                return;
            }
        }
        match chosen {
            Some(f) => {
                let vf = self.to_vframe(&f, false);
                self.last_frame = Some(f);
                let _ = tx.send(Event::Frame(vf));
            }
            None => {
                let _ = tx.send(Event::Eof { gen });
            }
        }
        ctx.request_repaint();
    }

    fn to_vframe(&mut self, f: &ff::frame::Video, history: bool) -> VFrame {
        let (ow, oh) = self.out_size;
        let rgba = self.convert_rgba(f, ow, oh).unwrap_or_else(|_| RgbaImage::new(ow.max(1), oh.max(1)));
        let native = if self.want_native { self.native_planes(f).ok().map(Arc::new) } else { None };
        let kind = match f.kind() {
            ff::picture::Type::I | ff::picture::Type::SI => b'I',
            ff::picture::Type::P | ff::picture::Type::SP => b'P',
            ff::picture::Type::B | ff::picture::Type::BI => b'B',
            _ => b'?',
        };
        VFrame { gen: self.gen, pts: self.frame_pts(f), kind, key: f.is_key(), rgba: Arc::new(rgba), native, history }
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
        use ff::format::Pixel as P;
        let fmt = f.format();
        let direct8 = matches!(fmt, P::YUV420P | P::YUVJ420P | P::YUV422P | P::YUVJ422P | P::YUV444P | P::YUVJ444P | P::GRAY8);
        let direct16 = matches!(fmt, P::YUV420P10LE | P::YUV420P12LE | P::YUV422P10LE | P::YUV422P12LE | P::YUV444P10LE | P::YUV444P12LE | P::YUV420P16LE | P::YUV444P16LE);
        if direct8 || direct16 {
            return Ok(copy_planes(f, direct16));
        }
        // Normalise everything else (NV12, P010, RGB, ...) to planar YUV.
        let depth = unsafe {
            let d = ff::ffi::av_pix_fmt_desc_get(fmt.into());
            if d.is_null() { 8 } else { (*d).comp[0].depth as u32 }
        };
        let target = if depth > 8 { P::YUV420P16LE } else { P::YUV420P };
        let key = (fmt, f.width(), f.height());
        if self.native_scaler.as_ref().map(|(_, k)| *k != key).unwrap_or(true) {
            let sc = ff::software::scaling::Context::get(fmt, f.width(), f.height(), target, f.width(), f.height(), ff::software::scaling::Flags::POINT)?;
            self.native_scaler = Some((sc, key));
        }
        let (sc, _) = self.native_scaler.as_mut().unwrap();
        let mut out = ff::frame::Video::empty();
        sc.run(f, &mut out)?;
        let mut y = copy_planes(&out, depth > 8);
        if depth > 8 {
            y.bit_depth = 16;
        }
        Ok(y)
    }
}

fn copy_planes(f: &ff::frame::Video, wide: bool) -> YuvFrame {
    let depth = unsafe {
        let d = ff::ffi::av_pix_fmt_desc_get(f.format().into());
        if d.is_null() { 8 } else { (*d).comp[0].depth as u32 }
    };
    let mut planes = Vec::new();
    for i in 0..f.planes().min(3) {
        let (w, h) = (f.plane_width(i) as usize, f.plane_height(i) as usize);
        let stride = f.stride(i);
        let data = f.data(i);
        let pd = if wide {
            let mut v = Vec::with_capacity(w * h);
            for y in 0..h {
                let row = &data[y * stride..y * stride + w * 2];
                v.extend(row.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            }
            PlaneData::U16(v)
        } else {
            let mut v = Vec::with_capacity(w * h);
            for y in 0..h {
                v.extend_from_slice(&data[y * stride..y * stride + w]);
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
    let mut pkt = ff::Packet::empty();
    let mut reads = 0;
    while !got && reads < 4000 {
        reads += 1;
        match pkt.read(&mut input) {
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


/// Sequential decoder producing native planes in presentation order (used by
/// whole-file analysis; independent from playback).
pub struct SeqDecoder {
    input: ff::format::context::Input,
    vidx: usize,
    dec: ff::decoder::Video,
    scaler: Option<(ff::software::scaling::Context, (ff::format::Pixel, u32, u32))>,
    eof: bool,
    flushed: bool,
}

impl SeqDecoder {
    pub fn open(path: &Path) -> Result<SeqDecoder> {
        init();
        let input = ff::format::input(path)?;
        let vs = input.streams().best(ff::media::Type::Video).ok_or_else(|| anyhow!("No video stream"))?;
        let vidx = vs.index();
        let mut ctx = ff::codec::context::Context::from_parameters(vs.parameters())?;
        ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
        let dec = ctx.decoder().video()?;
        Ok(SeqDecoder { input, vidx, dec, scaler: None, eof: false, flushed: false })
    }

    fn next_raw(&mut self) -> Option<ff::frame::Video> {
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
                    if pkt.stream() == self.vidx {
                        let _ = self.dec.send_packet(&pkt);
                    }
                }
                Err(_) => self.eof = true,
            }
        }
    }

    pub fn next(&mut self) -> Option<YuvFrame> {
        use ff::format::Pixel as P;
        let f = self.next_raw()?;
        let fmt = f.format();
        let direct8 = matches!(fmt, P::YUV420P | P::YUVJ420P | P::YUV422P | P::YUVJ422P | P::YUV444P | P::YUVJ444P | P::GRAY8);
        let direct16 = matches!(fmt, P::YUV420P10LE | P::YUV420P12LE | P::YUV422P10LE | P::YUV422P12LE | P::YUV444P10LE | P::YUV444P12LE | P::YUV420P16LE | P::YUV444P16LE);
        if direct8 || direct16 {
            return Some(copy_planes(&f, direct16));
        }
        let depth = unsafe {
            let d = ff::ffi::av_pix_fmt_desc_get(fmt.into());
            if d.is_null() { 8 } else { (*d).comp[0].depth as u32 }
        };
        let target = if depth > 8 { P::YUV420P16LE } else { P::YUV420P };
        let key = (fmt, f.width(), f.height());
        if self.scaler.as_ref().map(|(_, k)| *k != key).unwrap_or(true) {
            let sc = ff::software::scaling::Context::get(fmt, f.width(), f.height(), target, f.width(), f.height(), ff::software::scaling::Flags::POINT).ok()?;
            self.scaler = Some((sc, key));
        }
        let (sc, _) = self.scaler.as_mut()?;
        let mut out = ff::frame::Video::empty();
        sc.run(&f, &mut out).ok()?;
        let mut y = copy_planes(&out, depth > 8);
        if depth > 8 {
            y.bit_depth = 16;
        }
        Some(y)
    }
}
