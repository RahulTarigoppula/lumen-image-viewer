//! Temporal (sequence) metrics: flicker, freeze / duplicate frames and the
//! FFmpeg `scdet`-style scene score. They run in frame order on compact
//! per-frame features (2x2-downsampled luma in 8-bit levels) that workers
//! compute in parallel, so the in-order stage stays cheap.

use crate::frame::FrameCtx;
use crate::metrics::{Direction, MetricOptions, Unit, Value};
use rayon::prelude::*;

/// Per-frame input of the temporal metrics.
#[derive(Clone, Debug, Default)]
pub struct TemporalFrame {
    pub width: usize,
    pub height: usize,
    /// 2x2 box-downsampled luma, 8-bit levels.
    pub small: Vec<f32>,
    /// Mean luma, 8-bit levels.
    pub mean: f64,
}

impl TemporalFrame {
    pub fn from_ctx(ctx: &FrameCtx<'_>) -> Self {
        let l = ctx.luma();
        let (w2, h2) = ((l.width / 2).max(1), (l.height / 2).max(1));
        let mut small = vec![0f32; w2 * h2];
        let (lw, lh) = (l.width, l.height);
        small.par_chunks_mut(w2).enumerate().for_each(|(y, o)| {
            let r0 = l.row((2 * y).min(lh - 1));
            let r1 = l.row((2 * y + 1).min(lh - 1));
            for (x, v) in o.iter_mut().enumerate() {
                let (x0, x1) = ((2 * x).min(lw - 1), (2 * x + 1).min(lw - 1));
                *v = (r0[x0] + r0[x1] + r1[x0] + r1[x1]) * (255.0 / 4.0);
            }
        });
        let mean = crate::stats::mean(&small);
        TemporalFrame { width: w2, height: h2, small, mean }
    }

    /// Mean absolute difference to another frame (8-bit levels); NaN if sizes differ.
    pub fn mad(&self, o: &TemporalFrame) -> f64 {
        if self.small.len() != o.small.len() || self.small.is_empty() {
            return f64::NAN;
        }
        let s: f64 = self.small.par_chunks(4096).zip(o.small.par_chunks(4096)).map(|(a, b)| a.iter().zip(b).map(|(p, q)| (p - q).abs()).sum::<f32>() as f64).sum();
        s / self.small.len() as f64
    }
}

/// A sequence metric fed in frame order.
pub trait TemporalMetric: Send {
    fn id(&self) -> &'static str;
    fn version(&self) -> u32;
    /// Values for frame `idx`; `prev` is the previous frame pushed (if any).
    fn push(&mut self, idx: u64, cur: &TemporalFrame, prev: Option<&TemporalFrame>) -> Vec<Value>;
}

/// Static description of a temporal metric.
pub struct TemporalInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub version: u32,
    pub description: &'static str,
    pub make: fn(&MetricOptions) -> Box<dyn TemporalMetric>,
}

/// `temporal.flicker`: mean luma per frame and its change.
pub struct Flicker;

impl TemporalMetric for Flicker {
    fn id(&self) -> &'static str {
        "temporal.flicker"
    }
    fn version(&self) -> u32 {
        1
    }
    fn push(&mut self, _idx: u64, cur: &TemporalFrame, prev: Option<&TemporalFrame>) -> Vec<Value> {
        let delta = prev.map(|p| cur.mean - p.mean).unwrap_or(0.0);
        vec![
            Value::new("flicker", delta.abs(), Unit::Levels8, Direction::LowerIsBetter),
            Value::new("delta", delta, Unit::Levels8, Direction::Neutral),
            Value::new("mean_luma", cur.mean, Unit::Levels8, Direction::Neutral),
        ]
    }
}

/// `temporal.freeze`: mean absolute difference to the previous frame,
/// duplicate flag and the length of the current run of duplicates.
pub struct Freeze {
    eps: f64,
    run: u64,
}

impl TemporalMetric for Freeze {
    fn id(&self) -> &'static str {
        "temporal.freeze"
    }
    fn version(&self) -> u32 {
        1
    }
    fn push(&mut self, _idx: u64, cur: &TemporalFrame, prev: Option<&TemporalFrame>) -> Vec<Value> {
        let mad = prev.map(|p| cur.mad(p)).unwrap_or(f64::NAN);
        let dup = mad.is_finite() && mad < self.eps;
        self.run = if dup { self.run + 1 } else { 0 };
        vec![
            Value::new("duplicate", dup as u8 as f64, Unit::Count, Direction::LowerIsBetter),
            Value::new("mad", mad, Unit::Levels8, Direction::Neutral),
            Value::new("run", self.run as f64, Unit::Count, Direction::LowerIsBetter),
        ]
    }
}

/// `temporal.scene`: `scdet`-style score, `min(mafd, |mafd - prev mafd|)` with
/// mafd the mean absolute frame difference in percent of full scale.
pub struct Scene {
    threshold: f64,
    prev_mafd: f64,
}

impl TemporalMetric for Scene {
    fn id(&self) -> &'static str {
        "temporal.scene"
    }
    fn version(&self) -> u32 {
        1
    }
    fn push(&mut self, _idx: u64, cur: &TemporalFrame, prev: Option<&TemporalFrame>) -> Vec<Value> {
        let (score, mafd) = match prev {
            Some(p) => {
                let m = cur.mad(p);
                let mafd = if m.is_finite() { m / 255.0 * 100.0 } else { 100.0 };
                let s = mafd.min((mafd - self.prev_mafd).abs()).clamp(0.0, 100.0);
                (s, mafd)
            }
            None => (0.0, 0.0),
        };
        self.prev_mafd = mafd;
        vec![
            Value::new("score", score, Unit::Score, Direction::Neutral),
            Value::new("cut", (score > self.threshold) as u8 as f64, Unit::Count, Direction::Neutral),
        ]
    }
}

static TEMPORAL: &[TemporalInfo] = &[
    TemporalInfo {
        id: "temporal.flicker",
        name: "Flicker",
        version: 1,
        description: "Frame-to-frame change of mean luma",
        make: |_| Box::new(Flicker),
    },
    TemporalInfo {
        id: "temporal.freeze",
        name: "Freeze / duplicates",
        version: 1,
        description: "Duplicate frames (mean absolute difference below a threshold) and run length",
        make: |o| Box::new(Freeze { eps: o.freeze_eps as f64, run: 0 }),
    },
    TemporalInfo {
        id: "temporal.scene",
        name: "Scene change",
        version: 1,
        description: "FFmpeg scdet-style scene score (0..100) and cut flag",
        make: |o| Box::new(Scene { threshold: o.scene_threshold as f64, prev_mafd: 0.0 }),
    },
];

/// All temporal metrics.
pub fn all() -> &'static [TemporalInfo] {
    TEMPORAL
}

pub fn get(id: &str) -> Option<&'static TemporalInfo> {
    TEMPORAL.iter().find(|t| t.id == id)
}
