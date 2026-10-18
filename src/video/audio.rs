//! Audio output (WASAPI via cpal) that doubles as the playback master clock.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct Inner {
    buf: VecDeque<f32>,
    /// Presentation time of the next sample that will be played.
    next_pts: Option<f64>,
    /// Clock value and wall time at the last callback (for interpolation).
    last: Option<(Instant, f64)>,
    latency: f64,
}

pub struct AudioShared {
    pub rate: u32,
    pub channels: u16,
    inner: Mutex<Inner>,
    pub playing: AtomicBool,
    volume: AtomicU32,
    pub muted: AtomicBool,
}

impl AudioShared {
    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
    /// Discard queued audio (after a seek).
    pub fn flush(&self) {
        let mut i = self.inner.lock();
        i.buf.clear();
        i.next_pts = None;
        i.last = None;
    }
    /// Queue interleaved f32 samples whose first sample plays at `pts`.
    pub fn push(&self, samples: &[f32], pts: f64) {
        let mut i = self.inner.lock();
        if i.next_pts.is_none() {
            i.next_pts = Some(pts);
        }
        i.buf.extend(samples.iter().copied());
    }
    /// Seconds of audio queued.
    pub fn buffered_secs(&self) -> f64 {
        let i = self.inner.lock();
        i.buf.len() as f64 / (self.rate as f64 * self.channels as f64)
    }
    /// Current audio clock (what the listener hears now), if audio has started.
    pub fn clock(&self) -> Option<f64> {
        let i = self.inner.lock();
        let (t, c) = i.last?;
        let extra = if self.playing.load(Ordering::Relaxed) { t.elapsed().as_secs_f64().min(0.1) } else { 0.0 };
        Some(c + extra - i.latency)
    }

    fn fill(&self, out: &mut [f32], latency: f64) {
        let mut i = self.inner.lock();
        i.latency = latency;
        let playing = self.playing.load(Ordering::Relaxed);
        if !playing || i.next_pts.is_none() {
            out.fill(0.0);
            return;
        }
        let vol = if self.muted.load(Ordering::Relaxed) { 0.0 } else { self.volume() };
        let n = out.len().min(i.buf.len());
        for (o, s) in out.iter_mut().zip(i.buf.drain(..n)) {
            *o = s * vol;
        }
        out[n..].fill(0.0);
        let frames = n / self.channels as usize;
        if frames > 0 {
            // The first sample of this buffer starts playing `latency` from now.
            let first = i.next_pts.unwrap();
            i.last = Some((Instant::now(), first));
        }
        if let Some(p) = i.next_pts.as_mut() {
            *p += frames as f64 / self.rate as f64;
        }
    }
}

pub struct AudioOut {
    pub shared: Arc<AudioShared>,
    _stream: cpal::Stream,
}

impl AudioOut {
    pub fn open() -> anyhow::Result<AudioOut> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("No audio output device"))?;
        let supported = device.default_output_config()?;
        let rate = supported.sample_rate();
        let channels = supported.channels();
        let shared = Arc::new(AudioShared {
            rate,
            channels,
            inner: Mutex::new(Inner { buf: VecDeque::new(), next_pts: None, last: None, latency: 0.0 }),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
        });
        let config: cpal::StreamConfig = supported.config();
        let err = |e| eprintln!("audio stream error: {e}");
        let s2 = shared.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    s2.fill(data, latency_of(info));
                },
                err,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                let mut tmp: Vec<f32> = Vec::new();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                        tmp.resize(data.len(), 0.0);
                        s2.fill(&mut tmp, latency_of(info));
                        for (d, s) in data.iter_mut().zip(&tmp) {
                            *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    err,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let mut tmp: Vec<f32> = Vec::new();
                device.build_output_stream(
                    &config,
                    move |data: &mut [u16], info: &cpal::OutputCallbackInfo| {
                        tmp.resize(data.len(), 0.0);
                        s2.fill(&mut tmp, latency_of(info));
                        for (d, s) in data.iter_mut().zip(&tmp) {
                            *d = ((s.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                        }
                    },
                    err,
                    None,
                )?
            }
            other => return Err(anyhow::anyhow!("Unsupported audio sample format {other:?}")),
        };
        stream.play()?;
        Ok(AudioOut { shared, _stream: stream })
    }
}

fn latency_of(info: &cpal::OutputCallbackInfo) -> f64 {
    let ts = info.timestamp();
    ts.playback.duration_since(&ts.callback).map(|d| d.as_secs_f64()).unwrap_or(0.0).min(0.5)
}
