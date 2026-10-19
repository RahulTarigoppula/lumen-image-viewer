//! The metric interface, result types and the static registry.
//!
//! Every metric is a stateless unit struct with a stable string id (e.g.
//! `"ssim.gauss"`), a version that is bumped whenever its numbers change (it
//! is part of the cache key), the derived inputs it needs, a minimum size and
//! a cost hint. Temporal (sequence) metrics live in [`temporal`].

pub mod reference;
pub mod noref;
pub mod temporal;

use crate::frame::{FrameCtx, InputKind};
use crate::heatmap::Heatmap;
use crate::job::CancelToken;
use serde::{Deserialize, Serialize};

/// Reported dB values (PSNR, SSIM dB) never exceed this; identical inputs
/// read 100 instead of infinity (same convention as the existing video CSV).
pub const PSNR_CAP_DB: f64 = 100.0;

/// `v` limited to [`PSNR_CAP_DB`] (NaN is kept).
pub fn cap_db(v: f64) -> f64 {
    if v.is_nan() {
        v
    } else {
        v.min(PSNR_CAP_DB)
    }
}

/// PSNR in dB from a mean squared error, capped at [`PSNR_CAP_DB`].
pub fn psnr_db(mse: f64, peak: f64) -> f64 {
    if mse <= 0.0 {
        PSNR_CAP_DB
    } else {
        cap_db(10.0 * (peak * peak / mse).log10())
    }
}

/// SSIM expressed in dB, `10 log10(1 / (1 - ssim))`, capped.
pub fn ssim_db(ssim: f64) -> f64 {
    if ssim >= 1.0 {
        PSNR_CAP_DB
    } else {
        cap_db(-10.0 * (1.0 - ssim).log10())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    FullReference,
    NoReference,
    Temporal,
}

/// Unit of a reported value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Db,
    /// Unitless index such as SSIM (0..1).
    Index,
    Percent,
    DeltaE,
    Stops,
    /// Code values at the source bit depth.
    Code,
    /// Squared code values at the source bit depth.
    CodeSq,
    /// 8-bit-equivalent code values (value * 255 / max).
    Levels8,
    /// Squared 8-bit-equivalent code values.
    Levels8Sq,
    Degrees,
    Pixels,
    Count,
    /// CIE L* (0..100).
    LStar,
    /// Dimensionless score.
    Score,
}

impl Unit {
    pub fn label(self) -> &'static str {
        match self {
            Unit::Db => "dB",
            Unit::Index => "",
            Unit::Percent => "%",
            Unit::DeltaE => "dE",
            Unit::Stops => "stops",
            Unit::Code => "code",
            Unit::CodeSq => "code^2",
            Unit::Levels8 => "levels",
            Unit::Levels8Sq => "levels^2",
            Unit::Degrees => "deg",
            Unit::Pixels => "px",
            Unit::Count => "",
            Unit::LStar => "L*",
            Unit::Score => "",
        }
    }

    /// Decimal places used by the CSV writer and the UI.
    pub fn decimals(self) -> usize {
        match self {
            Unit::Index => 6,
            Unit::Count | Unit::Pixels => 0,
            _ => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    HigherIsBetter,
    LowerIsBetter,
    Neutral,
}

/// One named number of a metric result. `key` is relative to the metric
/// (`"y"`); the full column name is `"<metric id>.<key>"` (`"psnr.y"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub key: String,
    pub value: f64,
    pub unit: Unit,
    pub direction: Direction,
}

impl Value {
    pub fn new(key: impl Into<String>, value: f64, unit: Unit, direction: Direction) -> Self {
        Value { key: key.into(), value, unit, direction }
    }
}

/// Output of one metric on one frame (or frame pair).
#[derive(Clone, Debug, Default)]
pub struct MetricResult {
    pub id: String,
    pub version: u32,
    pub values: Vec<Value>,
    pub map: Option<Heatmap>,
    pub notes: Vec<String>,
    /// Computed on a downsampled preview; a full-resolution result follows.
    pub preview: bool,
    pub elapsed_ms: f64,
}

impl MetricResult {
    pub fn new(info: &MetricInfo) -> Self {
        MetricResult { id: info.id.to_string(), version: info.version, ..Default::default() }
    }

    pub fn push(&mut self, key: impl Into<String>, value: f64, unit: Unit, direction: Direction) {
        self.values.push(Value::new(key, value, unit, direction));
    }

    /// Value by relative key (`"y"`).
    pub fn get(&self, key: &str) -> Option<f64> {
        self.values.iter().find(|v| v.key == key).map(|v| v.value)
    }

    /// The headline value (see [`headline_key`]).
    pub fn main(&self) -> Option<&Value> {
        let k = headline_key(&self.id, self.values.iter().map(|v| v.key.as_str()))?;
        self.values.iter().find(|v| v.key == k)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MetricError {
    Cancelled,
    /// The metric needs B (full reference) and none was given.
    NeedsReference,
    /// The frame (or ROI) is smaller than the metric's minimum.
    TooSmall { min: (u32, u32), got: (u32, u32) },
    /// A and B cannot be compared (size, layout or bit depth differ).
    Mismatch(String),
    NotApplicable(String),
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetricError::Cancelled => write!(f, "cancelled"),
            MetricError::NeedsReference => write!(f, "needs a reference image (B)"),
            MetricError::TooSmall { min, got } => write!(f, "needs at least {}x{} px, got {}x{}", min.0, min.1, got.0, got.1),
            MetricError::Mismatch(s) => write!(f, "A and B differ: {s}"),
            MetricError::NotApplicable(s) => write!(f, "not applicable: {s}"),
        }
    }
}

impl std::error::Error for MetricError {}

/// Tunables shared by all metrics. Defaults match the plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricOptions {
    /// Produce heatmaps / tile maps (image jobs: on; video ranges: off).
    pub want_map: bool,
    /// Tile size in pixels for no-reference tile maps.
    pub tile: u32,
    /// Gaussian SSIM: Wang's automatic `round(min(w, h) / 256)` box downsampling first.
    pub ssim_downsample: bool,
    /// Tenengrad: only gradients above this magnitude (8-bit levels) count.
    pub tenengrad_threshold: f32,
    /// Immerkaer noise: ignore pixels whose Sobel magnitude is above the 90th percentile.
    pub noise_mask_edges: bool,
    /// Abs diff: pixels whose difference exceeds this (8-bit levels) count as changed.
    pub diff_threshold: f32,
    /// Freeze: frames whose mean absolute difference is below this (8-bit levels) are duplicates.
    pub freeze_eps: f32,
    /// Scene: score (0..100) above which a cut is reported.
    pub scene_threshold: f32,
}

impl Default for MetricOptions {
    fn default() -> Self {
        MetricOptions {
            want_map: true,
            tile: 64,
            ssim_downsample: false,
            tenengrad_threshold: 0.0,
            noise_mask_edges: true,
            diff_threshold: 0.0,
            freeze_eps: 0.1,
            scene_threshold: 10.0,
        }
    }
}

/// Static description of a metric, for the UI and the scheduler.
#[derive(Debug)]
pub struct MetricInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: MetricKind,
    pub version: u32,
    pub inputs: &'static [InputKind],
    /// Minimum frame / ROI size (w, h).
    pub min_size: (u32, u32),
    /// Estimated milliseconds per 3840x2160 frame on 8 AVX2 cores.
    pub cost_ms_4k: f32,
    /// Heavy metrics run on a 1/4-area preview first in image jobs.
    pub heavy: bool,
    pub has_map: bool,
    /// Headline value direction.
    pub direction: Direction,
    /// Keys of the headline value in order of preference (the first one a
    /// result has wins; `["y", "avg"]`: Y for YUV, the average for RGB).
    /// Never "whichever value comes first": PSNR's first RGB value is R.
    pub headline: &'static [&'static str],
    pub description: &'static str,
}

impl MetricInfo {
    /// Cost estimate for a `w` x `h` frame.
    pub fn cost_ms(&self, w: u32, h: u32) -> f32 {
        self.cost_ms_4k * (w as f32 * h as f32) / (3840.0 * 2160.0)
    }
}

/// A frame metric (full-reference or no-reference). Implementations are
/// stateless; ROI cropping happens in the runner (the context already holds
/// the cropped view), so map coordinates are relative to the ROI.
pub trait Metric: Send + Sync {
    fn info(&self) -> &'static MetricInfo;

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, opts: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError>;

    fn id(&self) -> &'static str {
        self.info().id
    }
}

/// Size check shared by all metrics.
pub(crate) fn check_size(info: &MetricInfo, ctx: &FrameCtx<'_>) -> Result<(), MetricError> {
    let got = (ctx.width() as u32, ctx.height() as u32);
    if got.0 < info.min_size.0 || got.1 < info.min_size.1 {
        return Err(MetricError::TooSmall { min: info.min_size, got });
    }
    Ok(())
}

/// Returns B for a full-reference metric after checking sizes match.
pub(crate) fn reference<'c, 'a>(info: &MetricInfo, a: &FrameCtx<'_>, b: Option<&'c FrameCtx<'a>>) -> Result<&'c FrameCtx<'a>, MetricError> {
    let b = b.ok_or(MetricError::NeedsReference)?;
    check_size(info, a)?;
    if a.width() != b.width() || a.height() != b.height() {
        return Err(MetricError::Mismatch(format!("{}x{} vs {}x{}", a.width(), a.height(), b.width(), b.height())));
    }
    Ok(b)
}

static REGISTRY: &[&dyn Metric] = &[
    &reference::psnr::Psnr,
    &reference::psnr::PsnrRgb,
    &reference::ssim::SsimFast,
    &reference::ssim::SsimGauss,
    &reference::msssim::MsSsim,
    &reference::deltae::DeltaE76,
    &reference::deltae::DeltaE94,
    &reference::deltae::DeltaE2000,
    &reference::diff::DiffAbs,
    &reference::diff::DiffSigned,
    #[cfg(feature = "ssimulacra2")]
    &reference::perceptual::Ssimulacra2,
    #[cfg(feature = "butteraugli")]
    &reference::perceptual::Butteraugli,
    #[cfg(feature = "flip")]
    &reference::perceptual::Flip,
    &noref::sharpness::LaplacianVar,
    &noref::sharpness::Tenengrad,
    &noref::noise::Noise,
    &noref::exposure::Exposure,
    &noref::cast::ColorCast,
    &noref::blockiness::Blockiness,
    &noref::banding::Banding,
];

/// All frame metrics, in display order.
pub fn all() -> &'static [&'static dyn Metric] {
    REGISTRY
}

/// Metric by id.
pub fn get(id: &str) -> Option<&'static dyn Metric> {
    REGISTRY.iter().copied().find(|m| m.id() == id)
}

/// Preferred headline keys of a frame or temporal metric (relative keys).
pub fn headline_keys(id: &str) -> &'static [&'static str] {
    get(id).map(|m| m.info().headline).or_else(|| temporal::get(id).map(|t| t.headline)).unwrap_or(&[])
}

/// The headline among `keys` (relative, `"avg"`) of metric `id`: its first
/// preferred key present, else the first key (unknown metrics).
pub fn headline_key<'k>(id: &str, keys: impl Iterator<Item = &'k str> + Clone) -> Option<&'k str> {
    headline_keys(id).iter().find_map(|h| keys.clone().find(|k| k == h)).or_else(|| keys.clone().next())
}

/// Named metric sets for the UI: `"quick"`, `"encode_qa"`, `"photo_technical"`.
pub fn preset(name: &str) -> &'static [&'static str] {
    match name {
        "quick" => &["psnr", "ssim.fast", "diff.abs"],
        "encode_qa" => &["psnr", "ssim.fast", "ssim.gauss", "ms_ssim", "deltae.2000", "blockiness", "banding", "temporal.freeze", "temporal.scene"],
        "photo_technical" => &["sharpness.laplacian", "sharpness.tenengrad", "noise", "exposure", "color_cast"],
        _ => &[],
    }
}

/// Union of the derived inputs a metric set needs.
pub fn inputs_for(metrics: &[&dyn Metric]) -> Vec<InputKind> {
    let mut v: Vec<InputKind> = Vec::new();
    for m in metrics {
        for k in m.info().inputs {
            if !v.contains(k) {
                v.push(*k);
            }
        }
    }
    v
}
