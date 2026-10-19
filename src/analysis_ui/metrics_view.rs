//! The Metrics tab of the analysis panel: scope bar, metric picker with
//! presets and cost hints, Run / Cancel with progress, results with quality
//! chips, skipped-metric notes, heatmap controls, per-file history and
//! exports.
//!
//! Hosts describe what is on screen with a [`MetricsCtx`] every frame and
//! call [`MetricsTab::ui`]. The region of interest belongs to the panel
//! (shared with the AI tab) and comes in with the context. Over each image
//! the host calls [`MetricsTab::paint_heatmap`]; video mode also draws
//! [`MetricsTab::video_plots`] under its timeline and seeks to
//! [`MetricsTab::take_seek`].

use super::adapters;
use super::heatmap_view::HeatmapView;
use super::plots::{self, PlotAction, Plots, Series};
use super::look;
use super::summary;
use crate::ui::icons as I;
use crate::ui::roi::ImageTransform;
use crate::ui::theme::{self, Palette};
use crate::ui::widgets;
use egui::{CursorIcon, Painter, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use lumen_analysis::cache::{CacheKey, DiskCache, DEFAULT_MAX_BYTES};
use lumen_analysis::metrics::{self, Direction, MetricKind, Unit};
use lumen_analysis::{AnalysisReport, Analyzer, Heatmap, JobEvent, JobHandle, MetricResult, Roi, ScopeKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "video")]
use crate::video::engine::{MediaInfo, SharedIndex};
#[cfg(feature = "video")]
use crate::video::metrics::YuvFrame;

// ------------------------------------------------------------------ grades

/// Quality chip of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grade {
    Good,
    Fair,
    Poor,
}

/// Metric-specific limits: `good` or better is green, worse than `poor` is
/// red, amber in between.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Threshold {
    pub good: f64,
    pub poor: f64,
    pub higher_better: bool,
}

impl Threshold {
    const fn hi(good: f64, poor: f64) -> Threshold {
        Threshold { good, poor, higher_better: true }
    }

    const fn lo(good: f64, poor: f64) -> Threshold {
        Threshold { good, poor, higher_better: false }
    }

    pub fn grade(&self, v: f64) -> Grade {
        if self.higher_better {
            if v >= self.good {
                Grade::Good
            } else if v < self.poor {
                Grade::Poor
            } else {
                Grade::Fair
            }
        } else if v < self.good {
            Grade::Good
        } else if v > self.poor {
            Grade::Poor
        } else {
            Grade::Fair
        }
    }
}

/// Limits for a report column (`"psnr.y"`, `"deltae.2000.mean"`); `None`
/// for values without a meaningful scale (sharpness depends on content).
pub fn threshold(key: &str) -> Option<Threshold> {
    let (id, k) = key.rsplit_once('.')?;
    let k = k.strip_suffix("_b").unwrap_or(k);
    let de = id.starts_with("deltae.");
    Some(match (id, k) {
        ("psnr" | "psnr.rgb", "y" | "u" | "v" | "avg" | "r" | "g" | "b" | "global") => Threshold::hi(40.0, 30.0),
        ("ssim.fast", "y" | "u" | "v" | "all") | ("ssim.gauss" | "ms_ssim", "y") => Threshold::hi(0.98, 0.90),
        ("ssim.fast", "y_db") | ("ssim.gauss", "db") => Threshold::hi(17.0, 10.0),
        (_, "mean" | "median") if de => Threshold::lo(1.0, 3.0),
        (_, "p95") if de => Threshold::lo(2.3, 6.0),
        (_, "max") if de => Threshold::lo(5.0, 15.0),
        (_, "above_jnd_pct") if de => Threshold::lo(1.0, 10.0),
        ("diff.abs", "mean") => Threshold::lo(1.0, 4.0),
        ("diff.abs", "p99") => Threshold::lo(4.0, 16.0),
        ("ssimulacra2", "score") => Threshold::hi(70.0, 50.0),
        ("butteraugli", "score") => Threshold::lo(1.0, 2.0),
        ("flip", "mean") => Threshold::lo(0.05, 0.15),
        ("noise", "y") => Threshold::lo(2.0, 5.0),
        ("exposure", "clip_pct" | "clip_lo_pct" | "clip_hi_pct") => Threshold::lo(0.5, 2.0),
        ("color_cast", "delta_e") => Threshold::lo(2.0, 5.0),
        ("blockiness", "score") => Threshold::lo(0.1, 0.3),
        ("banding", "score") => Threshold::lo(1.0, 5.0),
        _ => return None,
    })
}

pub fn grade(key: &str, v: f64) -> Option<Grade> {
    threshold(key).map(|t| t.grade(v))
}

// ------------------------------------------------------------------ ranges

/// A frame range as typed: `start..end` (exclusive, `None` = to the end)
/// every `stride` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeSel {
    pub start: usize,
    pub end: Option<usize>,
    pub stride: usize,
}

/// A frame number from `"120"`, seconds `"4.5s"` or minutes:seconds
/// `"1:02.5"`; empty (or `"end"`) is `None`.
pub fn parse_frame(s: &str, fps: f64) -> Result<Option<usize>, String> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("end") {
        return Ok(None);
    }
    let bad = || format!("'{s}' is not a frame number or time");
    let secs = if let Some(t) = s.strip_suffix('s') {
        Some(t.trim().parse::<f64>().map_err(|_| bad())?)
    } else if let Some((m, sec)) = s.split_once(':') {
        Some(m.trim().parse::<f64>().map_err(|_| bad())? * 60.0 + sec.trim().parse::<f64>().map_err(|_| bad())?)
    } else {
        None
    };
    match secs {
        Some(t) if t >= 0.0 && t.is_finite() => Ok(Some((t * fps.max(1.0)).round() as usize)),
        Some(_) => Err(bad()),
        None => s.parse::<usize>().map(Some).map_err(|_| bad()),
    }
}

/// The range typed into the start, end and stride fields, checked against a
/// file of `total` frames (when known).
pub fn parse_range(start: &str, end: &str, stride: &str, fps: f64, total: Option<usize>) -> Result<RangeSel, String> {
    let s = parse_frame(start, fps)?.unwrap_or(0);
    let mut e = parse_frame(end, fps)?;
    let stride = match stride.trim() {
        "" => 1,
        t => t.parse::<usize>().ok().filter(|&v| v > 0).ok_or_else(|| "Stride must be a whole number of at least 1".to_string())?,
    };
    if let Some(n) = total {
        if s >= n {
            return Err(format!("Start is past the last frame ({})", n.saturating_sub(1)));
        }
        if e.is_some_and(|e| e >= n) {
            e = None;
        }
    }
    if e.is_some_and(|e| e <= s) {
        return Err("End must be after start".into());
    }
    Ok(RangeSel { start: s, end: e, stride })
}

/// Frames a range measures in a file of `total` frames.
pub fn range_frames(r: RangeSel, total: usize) -> usize {
    let end = r.end.unwrap_or(total).min(total);
    if end <= r.start {
        0
    } else {
        (end - r.start).div_ceil(r.stride.max(1))
    }
}

// ------------------------------------------------------------------ context

/// One video file as video mode shows it.
#[cfg(feature = "video")]
#[derive(Clone)]
pub struct VideoSideCtx {
    pub path: PathBuf,
    /// Native size.
    pub size: (u32, u32),
    pub fps: f64,
    pub color: adapters::VideoColor,
    /// Display rotation in degrees (phone footage); the panel's ROI is in
    /// displayed pixels.
    pub rotation: i32,
    pub index: SharedIndex,
    /// Native planes of the frame on screen (when paused and enabled).
    pub native: Option<Arc<YuvFrame>>,
}

#[cfg(feature = "video")]
impl VideoSideCtx {
    pub fn new(path: &Path, info: &MediaInfo, index: &SharedIndex, native: Option<Arc<YuvFrame>>) -> VideoSideCtx {
        let fps = [info.avg_fps, info.fps].into_iter().find(|f| *f > 0.0).unwrap_or(25.0);
        VideoSideCtx { path: path.to_path_buf(), size: (info.width, info.height), fps, color: adapters::VideoColor::of_info(info), rotation: info.rotation, index: index.clone(), native }
    }

    fn side(&self) -> adapters::VideoSide {
        adapters::VideoSide { path: self.path.clone(), index: self.index.lock().as_ref().map(|v| Arc::new(v.clone())), fps: self.fps }
    }

    fn frames(&self) -> Option<usize> {
        self.index.lock().as_ref().map(|v| v.len())
    }
}

/// Video mode's state: A, optional B shifted by `offset` frames.
#[cfg(feature = "video")]
#[derive(Clone)]
pub struct VideoSubject {
    pub a: VideoSideCtx,
    pub b: Option<VideoSideCtx>,
    /// B frame = A frame + offset.
    pub offset: i64,
    /// A's frame on screen.
    pub current: Option<usize>,
    /// The A-B loop in A frames (inclusive).
    pub loop_ab: Option<(usize, usize)>,
}

/// What the host shows.
#[derive(Clone)]
pub enum Subject {
    None,
    /// The viewer's image: display pixels and the file's bits per channel.
    Image { path: PathBuf, image: Arc<RgbaImage>, source_bits: u8 },
    /// Compare A and B. With 3 or 4 panes `b` is the pane picked in the tab
    /// (`letter` "B", "C" or "D") and `others` the rest, measured against A
    /// in the same run.
    Compare { a: PathBuf, image_a: Arc<RgbaImage>, b: PathBuf, image_b: Arc<RgbaImage>, source_bits: u8, letter: &'static str, others: Vec<(&'static str, PathBuf, Arc<RgbaImage>)> },
    #[cfg(feature = "video")]
    Video(VideoSubject),
}

pub struct MetricsCtx<'a> {
    pub pal: &'a Palette,
    pub subject: Subject,
    /// The panel's region of interest in A's pixels (`None`: the whole picture).
    pub roi: Option<Roi>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    None,
    Image,
    Compare,
    Video,
    VideoPair,
}

impl Subject {
    fn kind(&self) -> Kind {
        match self {
            Subject::None => Kind::None,
            Subject::Image { .. } => Kind::Image,
            Subject::Compare { .. } => Kind::Compare,
            #[cfg(feature = "video")]
            Subject::Video(v) => {
                if v.b.is_some() {
                    Kind::VideoPair
                } else {
                    Kind::Video
                }
            }
        }
    }

    fn files(&self) -> (Option<&Path>, Option<&Path>) {
        match self {
            Subject::None => (None, None),
            Subject::Image { path, .. } => (Some(path.as_path()), None),
            Subject::Compare { a, b, .. } => (Some(a.as_path()), Some(b.as_path())),
            #[cfg(feature = "video")]
            Subject::Video(v) => (Some(v.a.path.as_path()), v.b.as_ref().map(|b| b.path.as_path())),
        }
    }

    /// Size of A in pixels (native size for video).
    fn size(&self) -> Option<(u32, u32)> {
        match self {
            Subject::None => None,
            Subject::Image { image, .. } => Some(image.dimensions()),
            Subject::Compare { image_a, .. } => Some(image_a.dimensions()),
            #[cfg(feature = "video")]
            Subject::Video(v) => Some(v.a.size),
        }
    }

    fn id(&self) -> String {
        let (a, b) = self.files();
        subject_id(self.kind(), a, b)
    }

    /// Compare: the letter of the pane measured against A.
    fn letter(&self) -> &'static str {
        match self {
            Subject::Compare { letter, .. } => letter,
            _ => "B",
        }
    }

    fn is_video(&self) -> bool {
        matches!(self.kind(), Kind::Video | Kind::VideoPair)
    }

    fn fps(&self) -> f64 {
        match self {
            #[cfg(feature = "video")]
            Subject::Video(v) => v.a.fps,
            _ => 25.0,
        }
    }

    fn frames(&self) -> Option<usize> {
        match self {
            #[cfg(feature = "video")]
            Subject::Video(v) => v.a.frames(),
            _ => None,
        }
    }

    /// Video: A's frame on screen.
    fn current(&self) -> Option<usize> {
        match self {
            #[cfg(feature = "video")]
            Subject::Video(v) => v.current,
            _ => None,
        }
    }

    /// The A-B loop in A frames (inclusive).
    fn loop_ab(&self) -> Option<(usize, usize)> {
        match self {
            #[cfg(feature = "video")]
            Subject::Video(v) => v.loop_ab,
            _ => None,
        }
    }
}

/// What results are kept by: the kind of subject and its files.
fn subject_id(kind: Kind, a: Option<&Path>, b: Option<&Path>) -> String {
    format!("{kind:?}|{}|{}", a.map(|p| p.display().to_string()).unwrap_or_default(), b.map(|p| p.display().to_string()).unwrap_or_default())
}

/// Pane letters of compare mode.
const LETTERS: [&str; 4] = ["A", "B", "C", "D"];

/// A region drawn on a video displayed rotated by `rotation` degrees, in the
/// stream's native pixels (`size`, unrotated).
pub fn native_roi(r: Roi, rotation: i32, size: (u32, u32)) -> Roi {
    let (w, h) = (size.0 as i64, size.1 as i64);
    let (x0, y0, x1, y1) = (r.x as i64, r.y as i64, (r.x + r.w) as i64, (r.y + r.h) as i64);
    let (a, b, c, d) = match rotation.rem_euclid(360) {
        // Displayed (x, y) shows native (y, h - x) when turned 90 degrees.
        90 => (y0, h - x1, y1, h - x0),
        180 => (w - x1, h - y1, w - x0, h - y0),
        270 => (w - y1, x0, w - y0, x1),
        _ => (x0, y0, x1, y1),
    };
    let clamp = |v: i64, m: i64| v.clamp(0, m) as u32;
    let (a, b, c, d) = (clamp(a, w), clamp(b, h), clamp(c, w), clamp(d, h));
    Roi::new(a, b, c.saturating_sub(a).max(1), d.saturating_sub(b).max(1))
}

// ------------------------------------------------------------------ prefs and history

/// Remembered metric choices per kind of subject, the last stride typed
/// and how fast range runs went (for the automatic stride).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    image: Option<Vec<String>>,
    compare: Option<Vec<String>>,
    video: Option<Vec<String>>,
    video_pair: Option<Vec<String>>,
    stride: Option<String>,
    /// `false`: the stride typed in Details is used for whole-video runs.
    stride_auto: Option<bool>,
    /// Milliseconds per measured frame and megapixel of the last range runs
    /// of one video and of a pair.
    speed_video: Option<f64>,
    speed_pair: Option<f64>,
}

fn prefs_path() -> PathBuf {
    crate::settings::data_dir().join("analysis_metrics.json")
}

impl Prefs {
    fn load() -> Prefs {
        std::fs::read(prefs_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let p = prefs_path();
            let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
            let _ = lumen_analysis::report::write_atomic(&p, &b);
        }
    }

    fn slot(&mut self, k: Kind) -> Option<&mut Option<Vec<String>>> {
        match k {
            Kind::None => None,
            Kind::Image => Some(&mut self.image),
            Kind::Compare => Some(&mut self.compare),
            Kind::Video => Some(&mut self.video),
            Kind::VideoPair => Some(&mut self.video_pair),
        }
    }
}

/// The recommended metric set (before the user chose one): everything the
/// summary talks about, and cheap enough for one click. Pairs get the
/// metrics with a heatmap for "Show where"; video ranges stay fast.
fn default_metrics(k: Kind) -> Vec<String> {
    let ids: &[&str] = match k {
        Kind::None => &[],
        Kind::Image => &["sharpness.laplacian", "noise", "exposure", "color_cast", "blockiness", "banding"],
        Kind::Compare => &["psnr", "ssim.gauss", "deltae.2000"],
        Kind::Video => &["sharpness.laplacian", "noise", "exposure", "blockiness", "banding", "temporal.freeze", "temporal.scene"],
        Kind::VideoPair => &["psnr", "ssim.fast"],
    };
    ids.iter().map(|s| s.to_string()).collect()
}

/// Range speed (ms per measured frame and megapixel) before any run: what
/// the recommended sets take on a 1.5 MP screen recording (8 threads).
const SPEED_VIDEO: f64 = 16.0;
const SPEED_PAIR: f64 = 10.0;
/// A whole-video run with the automatic stride aims at this many seconds.
const RANGE_BUDGET_S: f64 = 30.0;

/// One earlier run of the subject's file, stored in the analysis cache.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct HistoryEntry {
    key: String,
    created_unix: u64,
    scope: String,
    metrics: Vec<String>,
}

const HISTORY_MAX: usize = 30;
/// Frames in flight of a range job (decoded A and B plus derived images).
/// The decoders hold their own frames on top (HEVC frame threads: several
/// hundred MB for 4K 10-bit), so this stays well below a GB.
const RANGE_MEMORY_BUDGET: u64 = 384 << 20;

fn parse_key(hex: &str) -> Option<CacheKey> {
    if hex.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(CacheKey(k))
}

fn history_key(a: &Path) -> CacheKey {
    CacheKey::builder().extra("history", b"metrics").file("a", a).finish()
}

fn metric_version(id: &str) -> u32 {
    metrics::get(id).map(|m| m.info().version).or_else(|| metrics::temporal::get(id).map(|t| t.version)).unwrap_or(0)
}

fn run_key(kind: &str, a: &Path, b: Option<&Path>, ids: &[String], roi: Option<Roi>, frames: Option<lumen_analysis::FrameRange>, offset: i64) -> CacheKey {
    let mut k = CacheKey::builder().extra("kind", kind.as_bytes()).file("a", a);
    if let Some(b) = b {
        k = k.file("b", b);
    }
    for id in ids {
        k = k.metric(id, metric_version(id));
    }
    k.params(&lumen_analysis::MetricOptions::default()).roi(roi).range(frames).extra("offset", &offset.to_le_bytes()).finish()
}

/// Short description of a report's scope for the history list.
fn scope_label(r: &AnalysisReport) -> String {
    let s = &r.scope;
    let mut parts = Vec::new();
    match s.kind {
        ScopeKind::Image => parts.push(if s.file_b.is_some() { "A vs B".to_string() } else { "Image".to_string() }),
        ScopeKind::VideoFrame => parts.push(format!("Frame {}", s.frames.map(|f| f.start).unwrap_or(0))),
        ScopeKind::VideoRange => {
            let f = s.frames.unwrap_or_default();
            parts.push(if f.stride > 1 { format!("Frames {}-{} / {}", f.start, f.end.saturating_sub(1), f.stride) } else { format!("Frames {}-{}", f.start, f.end.saturating_sub(1)) });
        }
    }
    if let Some(r) = s.roi {
        parts.push(format!("ROI {} × {}", r.w, r.h));
    }
    parts.join(", ")
}

fn fmt_time(unix: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// Decimals shown in the results table; the summary writes its values with
/// the same ones (see `summary::pct`), so a number reads the same in both.
fn ui_decimals(u: Unit) -> usize {
    match u {
        Unit::Index => 4,
        Unit::Count | Unit::Pixels | Unit::Levels8Sq => 0,
        Unit::Percent => 1,
        _ => 2,
    }
}

/// Unit text in the panel (the CSV keeps `Unit::label`).
pub fn unit_text(u: Unit) -> &'static str {
    match u {
        Unit::CodeSq => "sq. code",
        Unit::Levels8Sq => "sq. levels",
        Unit::DeltaE => "dE",
        u => u.label(),
    }
}

/// A human name for value `key` (relative, `"mse_avg"`) of metric `id`.
pub fn key_label(id: &str, key: &str) -> String {
    // B's sequence of a temporal metric; elsewhere `_b` is the blue channel.
    if let Some(k) = key.strip_suffix("_b").filter(|_| id.starts_with("temporal.")) {
        return format!("B: {}", key_label(id, k));
    }
    let channel = |p: &str| match p {
        "r" => "Red".to_string(),
        "g" => "Green".to_string(),
        "b" => "Blue".to_string(),
        "avg" => "Average".to_string(),
        other => other.to_uppercase(),
    };
    let plane = |p: &str| match p {
        "y" => "Y",
        "u" => "U",
        "v" => "V",
        "r" => "R",
        "g" => "G",
        "b" => "B",
        "avg" => "average",
        other => other,
    }
    .to_string();
    let fixed = match (id, key) {
        ("color_cast", "a") => "a* (green-red)",
        ("color_cast", "b") => "b* (blue-yellow)",
        ("color_cast", "delta_e") => "Cast strength",
        ("color_cast", "hue") => "Hue",
        ("color_cast", "neutral_pct") => "Near-neutral pixels",
        ("sharpness.laplacian", "p90") => "Sharpest areas",
        ("sharpness.laplacian", "var") => "Whole picture",
        (_, "y" | "u" | "v" | "r" | "g" | "b") if id.starts_with("psnr") || id.starts_with("ssim") || id == "ms_ssim" || id == "noise" => return plane(key),
        (_, "cb") => "Cb",
        (_, "cr") => "Cr",
        (_, "avg") => "Average",
        (_, "all") => "All planes",
        (_, "global") => "Global (MSE of all frames)",
        (_, "y_db") => "Y in dB",
        (_, "db") => "In dB",
        (_, "cs") => "Contrast-structure",
        (_, "ssim5") => "SSIM at scale 5",
        (_, "mean") => "Mean",
        (_, "median") => "Median",
        (_, "p95") => "95th percentile",
        (_, "p99") => "99th percentile",
        (_, "max") => "Maximum",
        (_, "min") => "Minimum",
        (_, "std") => "Standard deviation",
        (_, "above_jnd_pct") => "Above 2.3 (visible)",
        (_, "changed_pct") => "Pixels changed",
        (_, "score") => "Score",
        (_, "pnorm3") => "3-norm",
        (_, "var") => "Variance",
        (_, "mean_g2") => "Mean gradient energy",
        (_, "edge_pct") => "Edge pixels",
        (_, "clip_pct") => "Clipped (worse side)",
        (_, "clip_lo_pct") => "Clipped shadows",
        (_, "clip_hi_pct") => "Clipped highlights",
        (_, "l_mean") => "L* mean",
        (_, "l_median") => "L* median",
        (_, "dr_stops") => "Dynamic range (stops)",
        (_, "ratio_h") => "Horizontal edge ratio",
        (_, "ratio_v") => "Vertical edge ratio",
        (_, "offset_x") => "Grid offset x",
        (_, "offset_y") => "Grid offset y",
        (_, "smooth_pct") => "Smooth area",
        (_, "contour_pct") => "Contour pixels",
        (_, "flicker") => "Mean change",
        (_, "flicker_max") => "Largest change",
        (_, "delta") => "Luma change",
        (_, "mean_luma") => "Mean luma",
        (_, "duplicate") => "Duplicate",
        (_, "duplicates") => "Duplicate frames",
        (_, "duplicate_pct") => "Duplicate share",
        (_, "longest_freeze") => "Longest freeze (frames)",
        (_, "mad") => "Mean abs difference",
        (_, "run") => "Freeze run",
        (_, "cut") => "Cut",
        (_, "cuts") => "Scene cuts",
        (_, "score_max") => "Largest score",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    if let Some(p) = key.strip_prefix("mse_") {
        return format!("{} MSE", channel(p));
    }
    if let Some(p) = key.strip_prefix("maxabs_") {
        return format!("{} max difference", channel(p));
    }
    if let Some(n) = key.strip_prefix("cs") {
        return format!("Contrast-structure, scale {n}");
    }
    if let Some(c) = key.strip_prefix("clip_lo_").and_then(|k| k.strip_suffix("_pct")) {
        return format!("Clipped shadows {}", plane(c));
    }
    if let Some(c) = key.strip_prefix("clip_hi_").and_then(|k| k.strip_suffix("_pct")) {
        return format!("Clipped highlights {}", plane(c));
    }
    if let Some(z) = key.strip_prefix("zone_") {
        return format!("Zone {z}");
    }
    if let Some(p) = key.strip_prefix("l_p") {
        return format!("L* {}th percentile", p.replace('_', "."));
    }
    let mut t = key.replace('_', " ");
    if let Some(c) = t.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    t
}

pub fn fmt_value(v: f64, u: Unit) -> String {
    // dB values are capped for identical inputs (the CSV keeps the 100).
    if u == Unit::Db && v >= lumen_analysis::PSNR_CAP_DB {
        return "Identical".into();
    }
    if u == Unit::Percent {
        return summary::pct(v);
    }
    let d = ui_decimals(u);
    match unit_text(u) {
        "" => format!("{v:.d$}"),
        l => format!("{v:.d$} {l}"),
    }
}

/// A path as Windows writes it (paths typed or dropped can mix separators).
pub fn path_text(p: &Path) -> String {
    p.display().to_string().replace('/', "\\")
}

/// Rough duration text for cost hints.
fn fmt_cost(ms: f32) -> String {
    if ms < 1.0 {
        "<1 ms".into()
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        format!("{:.0} min", ms / 60_000.0)
    }
}

fn fmt_eta(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m {:02}s left", s / 60, s % 60)
    } else {
        format!("{s}s left")
    }
}

// ------------------------------------------------------------------ results rows

/// One displayed value.
struct Cell {
    key: String,
    value: Option<f64>,
    unit: Unit,
    direction: Direction,
    /// Video: min, max and worst frame.
    tip: Option<String>,
    worst: Option<u64>,
}

/// One metric in the results table: headline first.
struct Row {
    id: String,
    name: String,
    cells: Vec<Cell>,
    preview: bool,
    has_map: bool,
}

/// Puts the metric's headline cell first (see `metrics::headline_key`:
/// PSNR's is the average, not R), the others keep their order.
fn headline_first(id: &str, cells: &mut Vec<Cell>) {
    let keys: Vec<String> = cells.iter().map(|c| short_key(id, &c.key).to_string()).collect();
    let head = metrics::headline_key(id, keys.iter().map(String::as_str)).and_then(|h| keys.iter().position(|k| k == h));
    if let Some(i) = head.filter(|&i| i > 0) {
        let c = cells.remove(i);
        cells.insert(0, c);
    }
}

/// `"avg"` of `"psnr.avg"` for metric `psnr`.
fn short_key<'k>(id: &str, key: &'k str) -> &'k str {
    key.strip_prefix(id).and_then(|k| k.strip_prefix('.')).unwrap_or(key)
}

fn rows_of_live(live: &[MetricResult]) -> Vec<Row> {
    let mut v: Vec<Row> = live
        .iter()
        .map(|r| Row {
            id: r.id.clone(),
            name: plots::metric_name(&r.id),
            cells: r
                .values
                .iter()
                .map(|x| Cell { key: format!("{}.{}", r.id, x.key), value: lumen_analysis::report::sanitize(x.value, x.unit), unit: x.unit, direction: x.direction, tip: None, worst: None })
                .collect(),
            preview: r.preview,
            has_map: r.map.is_some(),
        })
        .collect();
    for row in &mut v {
        headline_first(&row.id, &mut row.cells);
    }
    // Registry order, like the final report.
    let order = |id: &str| metrics::all().iter().position(|m| m.id() == id).unwrap_or(usize::MAX);
    v.sort_by_key(|r| order(&r.id));
    v
}

fn rows_of_report(r: &AnalysisReport) -> Vec<Row> {
    if !r.metrics.is_empty() {
        return r
            .metrics
            .iter()
            .map(|e| {
                let mut cells: Vec<Cell> = e.values.iter().map(|v| Cell { key: v.key.clone(), value: v.value, unit: v.unit, direction: v.direction, tip: None, worst: None }).collect();
                headline_first(&e.id, &mut cells);
                Row { id: e.id.clone(), name: e.name.clone(), cells, preview: false, has_map: e.map.as_ref().is_some_and(|m| !m.data.is_empty()) }
            })
            .collect();
    }
    let mut rows: Vec<Row> = Vec::new();
    for s in &r.summary {
        let id = plots::metric_of(&s.key).to_string();
        // Means of per-frame flags say little: see `temporal_rows`.
        if id.starts_with("temporal.") {
            continue;
        }
        let direction = r.columns.iter().find(|c| c.key == s.key).map(|c| c.direction).unwrap_or(if s.key == "psnr.global" { Direction::HigherIsBetter } else { Direction::Neutral });
        let mut tip = format!("Mean of {} frames", s.count);
        if let (Some(lo), Some(hi)) = (s.min, s.max) {
            tip += &format!("\nMin {}, max {}", fmt_value(lo, s.unit), fmt_value(hi, s.unit));
        }
        if let Some(w) = s.worst_idx {
            tip += &format!("\nWorst frame {w} (click to show)");
        }
        if s.key == "psnr.global" {
            tip = "PSNR of the mean squared error over all frames".into();
        }
        let cell = Cell { key: s.key.clone(), value: s.mean, unit: s.unit, direction, tip: Some(tip), worst: s.worst_idx };
        match rows.iter_mut().find(|row| row.id == id) {
            Some(row) => row.cells.push(cell),
            None => rows.push(Row { name: plots::metric_name(&id), id, cells: vec![cell], preview: false, has_map: false }),
        }
    }
    for row in &mut rows {
        headline_first(&row.id, &mut row.cells);
        // A mean of per-frame PSNR is pulled up by identical (capped) frames;
        // the PSNR of the mean MSE is not.
        if let Some(i) = row.cells.iter().position(|c| c.key == "psnr.global") {
            let c = row.cells.remove(i);
            row.cells.insert(0, c);
        }
    }
    rows.extend(temporal_rows(r));
    rows
}

/// Range results of the temporal metrics, aggregated per metric: duplicate
/// frames and the longest freeze, the number of cuts and their frames, mean
/// and largest flicker (per stream; B's keys end in `_b`).
fn temporal_rows(r: &AnalysisReport) -> Vec<Row> {
    let mut rows = Vec::new();
    for t in metrics::temporal::all() {
        let mut cells = Vec::new();
        for suffix in ["", "_b"] {
            let col = |k: &str| -> Vec<(u64, f64)> { r.series(&format!("{}.{k}{suffix}", t.id)).into_iter().filter_map(|(i, v)| v.map(|v| (i, v))).collect() };
            let cell = |k: &str, value: f64, unit: Unit, direction: Direction, tip: String, worst: Option<u64>| Cell { key: format!("{}.{k}{suffix}", t.id), value: Some(value), unit, direction, tip: Some(tip), worst };
            let mean = |v: &[(u64, f64)]| v.iter().map(|x| x.1).sum::<f64>() / v.len().max(1) as f64;
            let largest = |v: &[(u64, f64)]| v.iter().copied().fold(None, |m: Option<(u64, f64)>, x| if m.is_none_or(|m| x.1 > m.1) { Some(x) } else { m });
            match t.id {
                "temporal.freeze" => {
                    let dup = col("duplicate");
                    if dup.is_empty() {
                        continue;
                    }
                    let n = dup.iter().filter(|x| x.1 > 0.5).count();
                    let pct = 100.0 * n as f64 / dup.len() as f64;
                    let first = dup.iter().find(|x| x.1 > 0.5).map(|x| x.0);
                    let tip = format!("{n} of {} frames repeat the previous one ({pct:.1}%)", dup.len());
                    cells.push(cell("duplicates", n as f64, Unit::Count, Direction::LowerIsBetter, tip.clone(), first));
                    cells.push(cell("duplicate_pct", pct, Unit::Percent, Direction::LowerIsBetter, tip, first));
                    // The run counter counts repeats; the freeze shows one frame more.
                    if let Some((at, run)) = largest(&col("run")).filter(|x| x.1 > 0.0) {
                        let start = at.saturating_sub(run as u64);
                        cells.push(cell("longest_freeze", run + 1.0, Unit::Count, Direction::LowerIsBetter, format!("Frames {start}-{at} show the same picture (click to show)"), Some(start)));
                    }
                    let mad = col("mad");
                    if !mad.is_empty() {
                        cells.push(cell("mad", mean(&mad), Unit::Levels8, Direction::Neutral, format!("Mean of {} frames", mad.len()), None));
                    }
                }
                "temporal.scene" => {
                    let cut = col("cut");
                    if cut.is_empty() {
                        continue;
                    }
                    let at: Vec<u64> = cut.iter().filter(|x| x.1 > 0.5).map(|x| x.0).collect();
                    let list = at.iter().take(24).map(|f| f.to_string()).collect::<Vec<_>>().join(", ");
                    let tip = match at.len() {
                        0 => "No cuts".to_string(),
                        n if n > 24 => format!("At frames {list} and {} more (click: the first)", n - 24),
                        _ => format!("At frames {list} (click: the first)"),
                    };
                    cells.push(cell("cuts", at.len() as f64, Unit::Count, Direction::Neutral, tip, at.first().copied()));
                    if let Some((f, v)) = largest(&col("score")) {
                        cells.push(cell("score_max", v, Unit::Score, Direction::Neutral, format!("At frame {f} (click to show)"), Some(f)));
                    }
                }
                _ => {
                    let fl = col("flicker");
                    if fl.is_empty() {
                        continue;
                    }
                    cells.push(cell("flicker", mean(&fl), Unit::Levels8, Direction::LowerIsBetter, format!("Mean change of mean luma over {} frames", fl.len()), None));
                    if let Some((f, v)) = largest(&fl) {
                        cells.push(cell("flicker_max", v, Unit::Levels8, Direction::LowerIsBetter, format!("At frame {f} (click to show)"), Some(f)));
                    }
                    let luma = col("mean_luma");
                    if !luma.is_empty() {
                        cells.push(cell("mean_luma", mean(&luma), Unit::Levels8, Direction::Neutral, format!("Mean of {} frames", luma.len()), None));
                    }
                }
            }
        }
        if !cells.is_empty() {
            rows.push(Row { id: t.id.to_string(), name: t.name.to_string(), cells, preview: false, has_map: false });
        }
    }
    rows
}

/// Analysis pool size: cores - 2, so the UI thread and the GPU driver's
/// thread keep a core each while a job runs (frames stay under 16 ms).
fn analysis_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(2).max(1)
}

/// Frees `v` on another thread: results of a 12 MP job hold several 48 MB
/// maps, and returning that memory to the system takes milliseconds.
fn drop_later<T: Send + 'static>(v: T) {
    let _ = std::thread::Builder::new().name("lumen-free".into()).spawn(move || drop(v));
}

/// Whether report `r` measured what `s` shows: the same files and, for a
/// single video frame, the frame on screen (a range covers many frames and
/// says so in its labels).
fn report_matches(r: &AnalysisReport, s: &super::Subject) -> bool {
    let same = |f: &Option<String>, i: &Option<super::SubjectImage>| match (f, i) {
        (None, None) => true,
        (Some(f), Some(i)) => f.eq_ignore_ascii_case(&path_text(&i.path)),
        _ => false,
    };
    if !same(&r.scope.file_a, &s.a) || !same(&r.scope.file_b, &s.b) {
        return false;
    }
    match r.scope.kind {
        ScopeKind::Image => !s.kind.is_video(),
        ScopeKind::VideoFrame => s.kind.is_video() && r.scope.frames.map(|f| f.start) == s.frame().map(|f| f.index),
        ScopeKind::VideoRange => s.kind.is_video(),
    }
}

/// The facts the summary of report `r` is written from (`maps`: its
/// heatmaps, `other`: what the compared image is called).
fn facts_of(r: &AnalysisReport, maps: &[(String, String, Arc<Heatmap>)], fps: f64, other: &str) -> summary::Facts {
    let mut f = summary::Facts {
        pair: r.scope.file_b.is_some(),
        video: r.scope.kind != ScopeKind::Image,
        fps,
        rescaled: r.scope.rescaled,
        other: other.to_string(),
        maps: maps.iter().map(|m| m.0.clone()).collect(),
        ..Default::default()
    };
    for row in rows_of_report(r) {
        for c in row.cells {
            if let Some(v) = c.value {
                f.values.insert(c.key, v);
            }
        }
    }
    match r.scope.kind {
        ScopeKind::VideoRange => {
            f.frames = r.frames.len();
            f.stride = r.scope.frames.map(|x| x.stride.max(1) as usize).unwrap_or(1);
            for s in &r.summary {
                let lower = r.columns.iter().find(|c| c.key == s.key).is_some_and(|c| c.direction == Direction::LowerIsBetter);
                if let (Some(i), Some(lo), Some(hi)) = (s.worst_idx, s.min, s.max) {
                    f.worst.insert(s.key.clone(), (i, if lower { hi } else { lo }));
                }
            }
            f.low_frames = r.series("psnr.y").iter().filter(|(_, v)| v.is_some_and(|v| v < summary::PSNR_POOR)).count();
            f.cuts = r.series("temporal.scene.cut").iter().filter(|(_, v)| v.is_some_and(|v| v > 0.5)).map(|x| x.0).collect();
        }
        ScopeKind::VideoFrame => {
            f.frame = r.scope.frames.map(|x| x.start);
            f.frames = 1;
        }
        ScopeKind::Image => {}
    }
    f
}

// ------------------------------------------------------------------ the tab

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Temporal {
    Frame,
    Range,
}

/// What a video run measures (the scope choice of the tab). `Custom` is the
/// range typed in Details or dragged on a graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoScope {
    Frame,
    Loop,
    Whole,
    Custom,
}

struct Run {
    handle: JobHandle,
    key: CacheKey,
    history: Option<CacheKey>,
    started: Instant,
    /// The panel's region when the run started (see `MetricsTab::result_roi`).
    roi: Option<Roi>,
    #[cfg(feature = "video")]
    decode: Option<Arc<adapters::DecodeInfo>>,
}

/// A run of another pair of a 3 or 4 image compare (C against A while B is
/// shown), kept by its subject id.
struct SideRun {
    id: String,
    letter: &'static str,
    run: Run,
}

/// The results of a pair that is not shown, ready for when it is picked.
struct Stashed {
    report: AnalysisReport,
    maps: Vec<(String, String, Arc<Heatmap>)>,
    summary: Option<summary::Summary>,
    result_roi: Option<Option<Roi>>,
    status: Option<(String, bool)>,
}

/// Takes the heatmaps out of `report` (moved, not copied: a 12 MP map is 48 MB).
fn take_maps(report: &mut AnalysisReport) -> Vec<(String, String, Arc<Heatmap>)> {
    report
        .metrics
        .iter_mut()
        .filter_map(|m| {
            let map = m.map.take().filter(|x| !x.data.is_empty())?;
            Some((m.id.clone(), m.name.clone(), Arc::new(map)))
        })
        .collect()
}

/// The video a range was measured on (for thumbnails and frame exports).
#[cfg(feature = "video")]
#[derive(Clone)]
struct VideoRun {
    a: adapters::VideoSide,
    b: Option<adapters::VideoSide>,
    offset: i64,
    /// The region measured (thumbnails show it).
    region: Option<adapters::ThumbRegion>,
}

pub struct MetricsTab {
    analyzer: Option<Analyzer>,
    cache_dir: PathBuf,
    prefs: Prefs,
    subject_id: String,
    kind: Kind,
    file_a: Option<PathBuf>,
    temporal: Temporal,
    scope: VideoScope,
    /// The stride is picked so a range run takes about `RANGE_BUDGET_S`.
    stride_auto: bool,
    range_text: [String; 3],
    selected: BTreeSet<String>,
    run: Option<Run>,
    report: Option<AnalysisReport>,
    live: Vec<MetricResult>,
    series: Series,
    plot_span: (u64, u64),
    plots: Plots,
    heat: HeatmapView,
    /// Heatmaps of the shown report (id, name, map), taken out of it so the
    /// report clones cheaply (cache, exports) and the overlay shares them.
    maps: Vec<(String, String, Arc<Heatmap>)>,
    /// The region the shown results were measured on (`Some(None)`: the
    /// whole picture); `None` for results from the history. Results go
    /// stale when the panel's region no longer matches.
    result_roi: Option<Option<Roi>>,
    /// The panel's region this frame.
    cur_roi: Option<Roi>,
    /// "Report bundle folder" was chosen: the host adds the AI cards and
    /// calls [`MetricsTab::export_bundle`] (inner `None`: ask for the folder).
    bundle_request: Option<Option<PathBuf>>,
    expanded: HashSet<String>,
    history: Vec<HistoryEntry>,
    show_history: bool,
    status: Option<(String, bool)>,
    seek: Option<u64>,
    bg: Vec<crossbeam_channel::Receiver<Result<String, String>>>,
    #[cfg(feature = "video")]
    video_run: Option<VideoRun>,
    #[cfg(feature = "video")]
    thumbs: Option<adapters::VideoThumbs>,
    /// Video mode's state, set by video mode every frame.
    #[cfg(feature = "video")]
    video: Option<VideoSubject>,
    /// When the tab was last shown (video mode keeps native planes while it is).
    shown: Option<Instant>,
    /// Run once the tab next has its context (Shift+Q).
    run_requested: bool,
    /// Dev hook: scroll the panel by this many points on the next frame.
    dev_scroll: f32,
    /// Why full-reference metrics cannot run (videos of different sizes).
    size_mismatch: Option<String>,
    /// The plain-language summary of `report` (built when first shown).
    summary: Option<summary::Summary>,
    /// Compare mode's panes (A, B, C, D), set by the app every frame.
    compare_panes: Vec<PathBuf>,
    /// The pane measured against A in a 3 or 4 image compare (1 is B).
    pair_pick: usize,
    /// The letter of the image compared with A ("B"; "C" or "D" when picked).
    letter: &'static str,
    /// Results of the other pairs of the compare, by subject id.
    stash: std::collections::HashMap<String, Stashed>,
    /// Runs of the other pairs, started with the shown one.
    side_runs: Vec<SideRun>,
    /// Frame rate of the video shown (summary times).
    fps: f64,
    /// Dev hook: open the "All numbers" and "Details and options" sections.
    dev_open: Option<bool>,
    /// Frames waited for the paused frame's planes (see `run_video`).
    frame_tries: u8,
}

impl Default for MetricsTab {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsTab {
    pub fn new() -> MetricsTab {
        let prefs = Prefs::load();
        let stride = prefs.stride.clone().unwrap_or_else(|| "1".into());
        let stride_auto = prefs.stride_auto.unwrap_or(true);
        MetricsTab {
            analyzer: None,
            cache_dir: crate::settings::cache_dir().join("analysis"),
            prefs,
            subject_id: String::new(),
            kind: Kind::None,
            file_a: None,
            temporal: Temporal::Frame,
            scope: VideoScope::Frame,
            stride_auto,
            range_text: [String::new(), String::new(), stride],
            selected: BTreeSet::new(),
            run: None,
            report: None,
            live: Vec::new(),
            series: Series::default(),
            plot_span: (0, 1),
            plots: Plots::default(),
            heat: HeatmapView::default(),
            maps: Vec::new(),
            result_roi: None,
            cur_roi: None,
            bundle_request: None,
            expanded: HashSet::new(),
            history: Vec::new(),
            show_history: false,
            status: None,
            seek: None,
            bg: Vec::new(),
            #[cfg(feature = "video")]
            video_run: None,
            #[cfg(feature = "video")]
            thumbs: None,
            #[cfg(feature = "video")]
            video: None,
            shown: None,
            run_requested: false,
            dev_scroll: 0.0,
            size_mismatch: None,
            summary: None,
            compare_panes: Vec::new(),
            pair_pick: 1,
            letter: "B",
            stash: std::collections::HashMap::new(),
            side_runs: Vec::new(),
            fps: 25.0,
            dev_open: None,
            frame_tries: 0,
        }
    }

    fn cache(&self) -> DiskCache {
        DiskCache::new(self.cache_dir.clone(), DEFAULT_MAX_BYTES)
    }

    fn analyzer(&mut self, ctx: &egui::Context) -> &Analyzer {
        self.analyzer.get_or_insert_with(|| {
            let ctx = ctx.clone();
            Analyzer::new(Some(analysis_threads()), Some(Arc::new(move || ctx.request_repaint())))
        })
    }

    /// Video mode's state for the next frames (`None` when it closes).
    #[cfg(feature = "video")]
    pub fn set_video(&mut self, v: Option<VideoSubject>) {
        self.video = v;
    }

    /// The tab was on screen in the last moment: hosts keep what it needs
    /// ready (video mode copies native planes of the paused frame).
    pub fn visible(&self) -> bool {
        self.shown.is_some_and(|t| t.elapsed() < Duration::from_millis(600))
    }

    /// Run the selected metrics the next time the tab is drawn (Shift+Q).
    pub fn request_run(&mut self) {
        self.run_requested = true;
    }

    /// Compare mode's panes in order (A first; empty outside compare mode),
    /// so 3 and 4 image compares measure C and D against A too.
    pub fn set_compare_panes(&mut self, panes: Vec<PathBuf>) {
        if panes != self.compare_panes {
            self.compare_panes = panes;
            self.pair_pick = 1;
        }
    }

    pub fn compare_panes(&self) -> &[PathBuf] {
        &self.compare_panes
    }

    /// The letter of the image measured against A ("B", or "C" / "D" when
    /// picked in a 3 or 4 image compare).
    pub fn pair_letter(&self) -> &'static str {
        if self.compare_panes.len() > 2 {
            LETTERS[self.pair_pick.clamp(1, 3)]
        } else {
            "B"
        }
    }

    /// The tab's subject from the panel's: display pixels of the viewer or
    /// of compare A and B (`source_bits` of A's file), or video mode's state
    /// (see [`MetricsTab::set_video`]) at the frame on screen. `panes`: the
    /// decoded compare panes (see [`MetricsTab::set_compare_panes`]), when
    /// there are more than two.
    pub fn subject_of(&self, s: &super::Subject, source_bits: u8, panes: &[(PathBuf, Option<Arc<RgbaImage>>)]) -> Subject {
        use super::{Pixels, SubjectKind};
        let ready = |i: &Option<super::SubjectImage>| match i.as_ref().map(|i| (&i.pixels, &i.path)) {
            Some((Pixels::Ready(img), path)) => Some((path.clone(), img.clone())),
            _ => None,
        };
        match s.kind {
            SubjectKind::Image => match ready(&s.a) {
                Some((path, image)) => Subject::Image { path, image, source_bits },
                None => Subject::None,
            },
            SubjectKind::Pair => match (ready(&s.a), ready(&s.b)) {
                // 3 or 4 panes: the picked one against A, the others in the same run.
                (Some((a, image_a)), Some(_)) if panes.len() > 2 && panes.first().is_some_and(|p| p.0 == a) => {
                    let pick = self.pair_pick.clamp(1, panes.len() - 1);
                    let Some(image_b) = panes[pick].1.clone() else { return Subject::None };
                    let others = panes.iter().enumerate().skip(1).filter(|(i, _)| *i != pick).filter_map(|(i, (p, img))| Some((LETTERS[i.min(3)], p.clone(), img.clone()?))).collect();
                    Subject::Compare { a, image_a, b: panes[pick].0.clone(), image_b, source_bits, letter: LETTERS[pick.min(3)], others }
                }
                (Some((a, image_a)), Some((b, image_b))) => Subject::Compare { a, image_a, b, image_b, source_bits, letter: "B", others: Vec::new() },
                _ => Subject::None,
            },
            #[cfg(feature = "video")]
            SubjectKind::Video | SubjectKind::VideoPair => {
                let a = s.a.as_ref();
                match &self.video {
                    Some(v) if a.is_some_and(|a| a.path == v.a.path) => {
                        let mut v = v.clone();
                        v.current = a.and_then(|a| a.frame).map(|f| f.index as usize).or(v.current);
                        Subject::Video(v)
                    }
                    _ => Subject::None,
                }
            }
            #[cfg(not(feature = "video"))]
            SubjectKind::Video | SubjectKind::VideoPair => Subject::None,
        }
    }

    /// Dev hook (`ai metrics <verb> [args]`): drive the tab in automated UI
    /// tests. Returns a state line.
    pub fn dev(&mut self, args: &str) -> String {
        let (verb, rest) = args.trim().split_once(' ').map(|(v, r)| (v, r.trim())).unwrap_or((args.trim(), ""));
        match verb {
            "preset" => self.selected = metrics::preset(rest).iter().map(|s| s.to_string()).collect(),
            "select" => self.selected = rest.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            "frames" => self.set_scope(if rest == "range" { VideoScope::Custom } else { VideoScope::Frame }),
            "range" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                for (i, t) in v.iter().take(3).enumerate() {
                    self.range_text[i] = if *t == "end" { String::new() } else { t.to_string() };
                }
                // A typed stride is used as it is.
                if v.len() > 2 {
                    self.stride_auto = false;
                }
                self.set_scope(VideoScope::Custom);
            }
            // `scope frame|loop|whole`: the tab's scope choice.
            "scope" => self.set_scope(match rest {
                "loop" => VideoScope::Loop,
                "whole" => VideoScope::Whole,
                _ => VideoScope::Frame,
            }),
            // `stride <n>` or `stride auto`.
            "stride" => match rest.parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.range_text[2] = n.to_string();
                    self.stride_auto = false;
                }
                _ => self.stride_auto = true,
            },
            "recommended" => self.selected = default_metrics(self.kind).into_iter().collect(),
            "where" => self.show_where(),
            // `pair B|C|D`: the pane of a 3 or 4 image compare measured against A.
            "pair" => self.pair_pick = LETTERS.iter().position(|l| l.eq_ignore_ascii_case(rest)).unwrap_or(1).max(1),
            // `details [close]`: open (or close) the collapsed sections.
            "details" => self.dev_open = Some(rest != "close"),
            "run" => self.request_run(),
            "cancel" => self.cancel(),
            "scroll" => self.dev_scroll = rest.parse().unwrap_or(0.0),
            "map" => match self.map_of(rest) {
                Some(m) => {
                    self.heat.set_map(rest, m);
                    self.heat.visible = true;
                }
                None => self.heat.visible = false,
            },
            "opacity" => self.heat.opacity = rest.parse().unwrap_or(self.heat.opacity),
            "expand" => {
                self.expanded.insert(rest.to_string());
            }
            "seek" => {
                let worst = self.plots.worst_frames(&self.series);
                self.seek = if rest == "worst" { worst.first().copied() } else { rest.parse().ok() };
            }
            "export" => {
                let (what, path) = rest.split_once(' ').map(|(w, p)| (w, Some(PathBuf::from(p.trim())))).unwrap_or((rest, None));
                if path.is_some() {
                    match what {
                        "csv" => self.export_report(true, path),
                        "json" => self.export_report(false, path),
                        "png" => self.export_heatmap(true, path),
                        "tiff" => self.export_heatmap(false, path),
                        "bundle" => self.bundle_request = Some(path),
                        "pairs" => self.export_worst(path),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let values: Vec<String> = self
            .report
            .as_ref()
            .map(|r| rows_of_report(r).iter().filter_map(|row| row.cells.first().map(|c| format!("{}={}", c.key, c.value.map(|v| format!("{v:.4}")).unwrap_or_else(|| "-".into())))).collect())
            .unwrap_or_default();
        let summary = self.summary.as_ref().map(|s| {
            let mut v: Vec<String> = s.head.iter().map(|h| format!("{} ({})", h.title, h.detail)).collect();
            v.extend(s.lines.iter().map(|l| format!("{:?} {} {}", l.level, l.label, l.value)));
            v.join("; ")
        });
        format!(
            "metrics: kind={:?} scope={:?} stride_auto={} running={} stale={} status={:?} frames={} selected=[{}] values=[{}] map={:?} visible={} notes={} history={} worst={:?} pair={} stashed={} side_runs={} summary=[{}]",
            self.kind,
            self.scope,
            self.stride_auto,
            self.run.is_some(),
            self.stale(),
            self.status.as_ref().map(|s| s.0.clone()),
            self.report.as_ref().map(|r| r.frames.len()).unwrap_or(0),
            self.selected.iter().cloned().collect::<Vec<_>>().join(","),
            values.join(" "),
            self.heat.id(),
            self.heat.visible,
            self.report.as_ref().map(|r| r.notes.len()).unwrap_or(0),
            self.history.len(),
            self.plots.worst_frames(&self.series),
            self.letter,
            self.stash.len(),
            self.side_runs.len(),
            summary.unwrap_or_default(),
        )
    }

    /// Headline results of the shown report as (name, value, unit), for
    /// the AI tab's context: only when they were measured on what the AI
    /// tab sends (`subject`, its region `roi` in A's pixels), else none.
    /// Range results are labelled as such.
    pub fn metric_context(&self, subject: &super::Subject, roi: Option<(u32, u32, u32, u32)>) -> Vec<(String, f64, String)> {
        let Some(r) = &self.report else { return Vec::new() };
        if !report_matches(r, subject) || self.run.is_some() {
            return Vec::new();
        }
        let roi = roi.map(|(x, y, w, h)| Roi::new(x, y, w, h));
        match self.result_roi {
            Some(measured) if measured != roi => return Vec::new(),
            // From the history: the region it was measured on is the report's.
            None if r.scope.kind == ScopeKind::Image && r.scope.roi != roi => return Vec::new(),
            _ => {}
        }
        let range = (r.scope.kind == ScopeKind::VideoRange).then(|| r.scope.frames.unwrap_or_default());
        rows_of_report(r)
            .into_iter()
            .filter_map(|row| {
                let c = row.cells.into_iter().next()?;
                let mut name = format!("{}, {}", row.name, key_label(&row.id, short_key(&row.id, &c.key)));
                if let Some(f) = range {
                    let frames = format!("frames {}-{}", f.start, f.end.saturating_sub(1));
                    // Temporal rows and the global PSNR are not per-frame means.
                    let mean = !row.id.starts_with("temporal.") && c.key != "psnr.global";
                    name += &if mean { format!(" (mean over {frames})") } else { format!(" ({frames})") };
                }
                Some((name, c.value?, unit_text(c.unit).to_string()))
            })
            .collect()
    }

    /// A frame (of A) the user asked to see, once.
    pub fn take_seek(&mut self) -> Option<u64> {
        self.seek.take()
    }

    /// The largest minimum size among the selected metrics, with a note for
    /// the region editor's warning (MS-SSIM needs 176 px).
    pub fn min_roi_size(&self) -> Option<((u32, u32), String)> {
        self.runnable()
            .iter()
            .filter_map(|id| metrics::get(id))
            .map(|m| m.info())
            .filter(|i| i.min_size.0 > 8 || i.min_size.1 > 8)
            .max_by_key(|i| i.min_size.0.max(i.min_size.1))
            .map(|i| (i.min_size, format!("{} needs {} px", i.name, i.min_size.0)))
    }

    /// Whether there are graphs for [`MetricsTab::video_plots`].
    pub fn has_plots(&self) -> bool {
        !self.series.is_empty()
    }

    /// Whether [`MetricsTab::paint_heatmap`] draws anything (not while the
    /// results are stale).
    pub fn overlay_active(&self) -> bool {
        self.heat.active() && !self.stale()
    }

    /// The results (and maps) were measured on another region than the panel's.
    fn stale(&self) -> bool {
        self.result_roi.is_some_and(|r| r != self.cur_roi)
    }

    /// The shown report's heatmap of metric `id`.
    fn map_of(&self, id: &str) -> Option<Arc<Heatmap>> {
        self.maps.iter().find(|m| m.0 == id).map(|m| m.2.clone())
    }

    /// A "Report bundle folder" export waiting for the host (see
    /// [`MetricsTab::export_bundle`]); inner `None`: ask for the folder.
    pub fn take_bundle_request(&mut self) -> Option<Option<PathBuf>> {
        self.bundle_request.take()
    }

    /// Follow the host's subject: a new file resets results and loads its history.
    fn sync(&mut self, s: &Subject) {
        let id = s.id();
        if id == self.subject_id {
            return;
        }
        let old_id = std::mem::replace(&mut self.subject_id, id.clone());
        let file_a = s.files().0.map(Path::to_path_buf);
        // Another pair of the same compare: keep this pair's results (or its
        // run) for when it is picked again.
        let same_compare = self.kind == Kind::Compare && s.kind() == Kind::Compare && self.file_a == file_a;
        if same_compare {
            if let Some(run) = self.run.take() {
                self.side_runs.push(SideRun { id: old_id.clone(), letter: self.letter, run });
            }
            if let Some(report) = self.report.take() {
                let st = Stashed { report, maps: std::mem::take(&mut self.maps), summary: self.summary.take(), result_roi: self.result_roi, status: self.status.take() };
                self.stash.insert(old_id, st);
            }
        } else {
            // Dropping the handles cancels the runs.
            drop_later(std::mem::take(&mut self.stash));
            self.side_runs.clear();
            if let Some(r) = self.run.take() {
                r.handle.cancel();
            }
        }
        let shown = self.heat.id().filter(|_| self.heat.visible).map(str::to_string);
        self.kind = s.kind();
        self.letter = s.letter();
        self.file_a = file_a;
        self.report = None;
        self.live.clear();
        self.series = Series::default();
        self.heat.clear();
        self.maps.clear();
        self.result_roi = None;
        self.status = None;
        self.summary = None;
        self.expanded.clear();
        if same_compare {
            if let Some(i) = self.side_runs.iter().position(|r| r.id == id) {
                self.run = Some(self.side_runs.remove(i).run);
            }
            if let Some(st) = self.stash.remove(&id) {
                self.report = Some(st.report);
                self.maps = st.maps;
                self.summary = st.summary;
                self.result_roi = st.result_roi;
                self.status = st.status;
                self.pick_overlay(shown);
            }
        }
        // A new video starts on the frame on screen; a range typed for the
        // last file may not exist in this one.
        self.set_scope(VideoScope::Frame);
        self.range_text[0].clear();
        self.range_text[1].clear();
        let chosen = self.prefs.slot(self.kind).and_then(|s| s.clone()).unwrap_or_else(|| default_metrics(self.kind));
        self.selected = chosen.into_iter().collect();
        self.history = self.file_a.as_ref().and_then(|a| self.cache().get::<Vec<HistoryEntry>>(&history_key(a))).unwrap_or_default();
    }

    fn save_selection(&mut self) {
        let sel: Vec<String> = self.selected.iter().cloned().collect();
        if let Some(slot) = self.prefs.slot(self.kind) {
            *slot = Some(sel);
        }
        self.prefs.stride = Some(self.range_text[2].clone());
        self.prefs.stride_auto = Some(self.stride_auto);
        self.prefs.save();
    }

    fn set_scope(&mut self, scope: VideoScope) {
        self.scope = scope;
        self.temporal = if scope == VideoScope::Frame { Temporal::Frame } else { Temporal::Range };
    }

    /// Range speed for the automatic stride (ms per frame and megapixel).
    fn speed(&self) -> f64 {
        match self.kind {
            Kind::VideoPair => self.prefs.speed_pair.unwrap_or(SPEED_PAIR),
            _ => self.prefs.speed_video.unwrap_or(SPEED_VIDEO),
        }
    }

    /// The frames a range run of `s` would measure: the scope's range with
    /// the typed or the automatic stride.
    fn planned_range(&self, s: &Subject, roi: Option<Roi>) -> Result<RangeSel, String> {
        let total = s.frames();
        let (start, end) = match self.scope {
            VideoScope::Whole | VideoScope::Frame => (String::new(), String::new()),
            VideoScope::Loop => match s.loop_ab() {
                Some((a, b)) => (a.to_string(), (b + 1).to_string()),
                None => return Err("Set an A-B loop first (Ctrl+L)".into()),
            },
            VideoScope::Custom => (self.range_text[0].clone(), self.range_text[1].clone()),
        };
        let stride = if self.stride_auto { "1" } else { self.range_text[2].as_str() };
        let mut r = parse_range(&start, &end, stride, s.fps(), total)?;
        if let (true, Some(n), Some((w, h))) = (self.stride_auto, total, s.size()) {
            let (w, h) = roi.map(|r| (r.w, r.h)).unwrap_or((w, h));
            r.stride = summary::auto_stride(range_frames(r, n), w as f64 * h as f64 / 1e6, self.speed(), RANGE_BUDGET_S);
        }
        Ok(r)
    }

    /// "Every 4th frame, about 25 s" for the scope choice, or why it cannot run.
    fn range_hint(&self, s: &Subject, roi: Option<Roi>) -> Result<String, String> {
        let r = self.planned_range(s, roi)?;
        let Some(n) = s.frames() else { return Ok("Indexing the video".into()) };
        let frames = range_frames(r, n);
        let (w, h) = roi.map(|r| (r.w, r.h)).or(s.size()).unwrap_or((1920, 1080));
        let secs = summary::estimate_secs(frames, 1, w as f64 * h as f64 / 1e6, self.speed());
        let time = if secs < 1.5 { "about 1 s".to_string() } else if secs < 90.0 { format!("about {secs:.0} s") } else { format!("about {:.0} min", secs / 60.0) };
        Ok(format!("{}, {frames} frames, {time}", summary::every(r.stride)))
    }

    /// "Show where": the heatmap the summary points at, on the image.
    fn show_where(&mut self) {
        let Some(summary::Action::ShowMap { id, .. }) = self.summary.as_ref().and_then(|s| s.action.clone()) else { return };
        if let Some(m) = self.map_of(&id) {
            self.heat.set_map(&id, m);
            self.heat.visible = true;
        }
    }

    /// Why metric `id` cannot run on this subject and scope, if it cannot.
    fn blocked(&self, kind: MetricKind) -> Option<String> {
        let has_b = matches!(self.kind, Kind::Compare | Kind::VideoPair);
        let video = matches!(self.kind, Kind::Video | Kind::VideoPair);
        match kind {
            MetricKind::FullReference if !has_b => Some("Needs a reference: compare two images or load a B video".into()),
            MetricKind::FullReference if self.size_mismatch.is_some() => self.size_mismatch.clone(),
            MetricKind::Temporal if !video => Some("Needs a video".into()),
            MetricKind::Temporal if self.temporal != Temporal::Range => Some("Needs a frame range".into()),
            _ => None,
        }
    }

    /// Videos of different sizes cannot be compared pixel by pixel (images
    /// can: B is resampled to A's size).
    fn update_size_mismatch(&mut self, s: &Subject) {
        self.size_mismatch = match s {
            #[cfg(feature = "video")]
            Subject::Video(VideoSubject { a, b: Some(b), .. }) if a.size != b.size => {
                Some(format!("A is {} × {} and B {} × {}: full-reference metrics need videos of the same size", a.size.0, a.size.1, b.size.0, b.size.1))
            }
            _ => None,
        };
    }

    /// The selected metrics that can run now, frame metrics in registry order then temporal ones.
    fn runnable(&self) -> Vec<String> {
        let frame = metrics::all().iter().filter(|m| self.selected.contains(m.id()) && self.blocked(m.info().kind).is_none()).map(|m| m.id().to_string());
        let temporal = metrics::temporal::all().iter().filter(|t| self.selected.contains(t.id) && self.blocked(MetricKind::Temporal).is_none()).map(|t| t.id.to_string());
        frame.chain(temporal).collect()
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some((text.into(), error));
    }

    // ---------------------------------------------------------------- running

    /// Run the selected metric set on the current scope (the Run button, Shift+Q).
    pub fn run(&mut self, ctx: &MetricsCtx, egui_ctx: &egui::Context) {
        self.sync(&ctx.subject);
        self.update_size_mismatch(&ctx.subject);
        let ids = self.runnable();
        if ids.is_empty() {
            let why = self.size_mismatch.clone().unwrap_or_else(|| "Choose at least one metric that applies here (Details and options)".into());
            self.set_status(why, true);
            return;
        }
        let roi = ctx.roi;
        let (fa, fb) = ctx.subject.files();
        let Some(fa) = fa.map(Path::to_path_buf) else {
            self.set_status("Nothing to measure", true);
            return;
        };
        let fb = fb.map(Path::to_path_buf);
        let hist = Some(history_key(&fa));
        match &ctx.subject {
            Subject::None => {}
            Subject::Image { image, source_bits, .. } | Subject::Compare { image_a: image, source_bits, .. } => {
                let b = match &ctx.subject {
                    Subject::Compare { image_b, .. } => Some(image_b),
                    _ => None,
                };
                let Some(pair) = adapters::image_pair(image, b) else {
                    self.set_status("The image is empty", true);
                    return;
                };
                let rescaled = pair.rescaled;
                let spec = adapters::image_spec(ids.clone(), roi, Some(&fa), fb.as_deref(), rescaled, *source_bits);
                let key = run_key("image", &fa, fb.as_deref(), &ids, roi, None, 0);
                let mut jobs = vec![(spec, pair.a, pair.b)];
                // A 3 or 4 image compare: the other panes against A, one after the other.
                let mut sides = Vec::new();
                if let Subject::Compare { others, .. } = &ctx.subject {
                    for (letter, path, img) in others {
                        let Some(pair) = adapters::image_pair(image, Some(img)) else { continue };
                        jobs.push((adapters::image_spec(ids.clone(), roi, Some(&fa), Some(path), pair.rescaled, *source_bits), pair.a, pair.b));
                        sides.push((subject_id(Kind::Compare, Some(&fa), Some(path)), *letter, run_key("image", &fa, Some(path), &ids, roi, None, 0)));
                    }
                }
                let mut handles = self.analyzer(egui_ctx).submit_images(jobs).into_iter();
                let handle = handles.next().expect("the shown pair");
                self.start(Run {
                    handle,
                    key,
                    history: hist,
                    started: Instant::now(),
                    roi,
                    #[cfg(feature = "video")]
                    decode: None,
                });
                self.side_runs.clear();
                for ((id, letter, key), handle) in sides.into_iter().zip(handles) {
                    self.stash.remove(&id);
                    let run = Run {
                        handle,
                        key,
                        history: None,
                        started: Instant::now(),
                        roi,
                        #[cfg(feature = "video")]
                        decode: None,
                    };
                    self.side_runs.push(SideRun { id, letter, run });
                }
                if rescaled {
                    self.set_status(format!("{} is resampled to A's size", self.letter), false);
                }
            }
            #[cfg(feature = "video")]
            Subject::Video(v) => self.run_video(v, ids, roi, fa, fb, hist, egui_ctx),
        }
    }

    #[cfg(feature = "video")]
    #[allow(clippy::too_many_arguments)]
    fn run_video(&mut self, v: &VideoSubject, ids: Vec<String>, panel_roi: Option<Roi>, fa: PathBuf, fb: Option<PathBuf>, hist: Option<CacheKey>, egui_ctx: &egui::Context) {
        let roi = panel_roi.map(|r| native_roi(r, v.a.rotation, v.a.size));
        if self.temporal == Temporal::Frame {
            let Some(cur) = v.current else {
                self.set_status("No frame on screen yet", true);
                return;
            };
            let Some(a) = v.a.native.clone().and_then(|f| adapters::NativeFrame::new(f, v.a.color)) else {
                // Video mode copies the paused frame's planes once the tab is
                // on screen: try again for a moment (Shift+Q with the panel
                // closed), then say what is missing.
                if self.frame_tries < 40 {
                    self.frame_tries += 1;
                    self.run_requested = true;
                    egui_ctx.request_repaint();
                } else {
                    self.frame_tries = 0;
                    self.set_status("Pause the video to analyze the frame on screen", true);
                }
                return;
            };
            self.frame_tries = 0;
            let b = match &v.b {
                Some(sb) => match sb.native.clone().and_then(|f| adapters::NativeFrame::new(f, sb.color)) {
                    Some(f) => Some(f),
                    None => {
                        self.set_status("B's frame is not ready yet", true);
                        return;
                    }
                },
                None => None,
            };
            let frames = Some(lumen_analysis::FrameRange { start: cur as u64, end: cur as u64 + 1, stride: 1 });
            let key = run_key("video_frame", &fa, fb.as_deref(), &ids, roi, frames, v.offset);
            let handle = adapters::submit_video_frame(self.analyzer(egui_ctx), a, b, cur, ids, roi, &fa, fb.as_deref());
            self.start(Run { handle, key, history: hist, started: Instant::now(), roi: panel_roi, decode: None });
            return;
        }
        let total = v.a.frames();
        let r = match self.planned_range(&Subject::Video(v.clone()), panel_roi) {
            Ok(r) => r,
            Err(e) => {
                self.set_status(e, true);
                return;
            }
        };
        if self.stride_auto {
            self.range_text[2] = r.stride.to_string();
        }
        let req = adapters::VideoRangeRequest {
            a: v.a.side(),
            b: v.b.as_ref().map(|b| b.side()),
            offset: v.offset,
            range: crate::video::engine::FrameRange::new(r.start, r.end, r.stride),
            roi,
            metrics: ids.clone(),
            memory_budget: RANGE_MEMORY_BUDGET,
            frames_a: total,
        };
        let end = r.end.or(total).unwrap_or(r.start + 1);
        let frames = Some(lumen_analysis::FrameRange { start: r.start as u64, end: end as u64, stride: r.stride as u64 });
        let key = run_key("video_range", &fa, fb.as_deref(), &ids, roi, frames, v.offset);
        // The panel's region is in displayed pixels, `roi` in native ones.
        let turned = v.a.rotation.rem_euclid(180) == 90;
        let (nw, nh) = (v.a.size.0.max(1) as f32, v.a.size.1.max(1) as f32);
        let (dw, dh) = if turned { (nh, nw) } else { (nw, nh) };
        let frac = |r: Roi, w: f32, h: f32| [r.x as f32 / w, r.y as f32 / h, (r.x + r.w) as f32 / w, (r.y + r.h) as f32 / h];
        let region = panel_roi.zip(roi).map(|(d, n)| adapters::ThumbRegion { display: frac(d, dw, dh), native: frac(n, nw, nh), turned, native_landscape: nw > nh });
        self.video_run = Some(VideoRun { a: req.a.clone(), b: req.b.clone(), offset: v.offset, region });
        let job = adapters::submit_video_range(self.analyzer(egui_ctx), req);
        self.plot_span = (r.start as u64, end.saturating_sub(1).max(r.start) as u64);
        self.start(Run { handle: job.handle, key, history: hist, started: Instant::now(), roi: panel_roi, decode: Some(job.info) });
    }

    fn start(&mut self, run: Run) {
        if let Some(old) = self.run.take() {
            old.handle.cancel();
        }
        drop_later(std::mem::take(&mut self.live));
        self.series = Series::default();
        self.report = None;
        self.summary = None;
        self.status = None;
        self.run = Some(run);
    }

    pub fn cancel(&mut self) {
        for r in self.run.iter().chain(self.side_runs.iter().map(|s| &s.run)) {
            r.handle.cancel();
        }
    }

    /// Take the job's events; call every frame (the tab's `ui` does).
    pub fn poll(&mut self, ctx: &egui::Context) {
        let t0 = Instant::now();
        self.poll_job(ctx);
        crate::devhook::prof("metrics_poll", t0);
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        for rx in std::mem::take(&mut self.bg) {
            match rx.try_recv() {
                Ok(Ok(m)) => self.set_status(m, false),
                Ok(Err(e)) => self.set_status(e, true),
                Err(crossbeam_channel::TryRecvError::Empty) => self.bg.push(rx),
                Err(_) => {}
            }
        }
        self.poll_side_runs();
        let Some(run) = &mut self.run else { return };
        let mut done: Option<Result<AnalysisReport, String>> = None;
        while let Some(ev) = run.handle.try_recv() {
            match ev {
                JobEvent::Partial(r) | JobEvent::Metric(r) => {
                    self.live.retain(|x| x.id != r.id);
                    self.live.push(r);
                }
                JobEvent::Row(row) => self.series.push(&row),
                JobEvent::Done(r) => done = Some(Ok(*r)),
                JobEvent::Failed(e) => done = Some(Err(e)),
                JobEvent::Cancelled => done = Some(Err(String::new())),
            }
        }
        let Some(done) = done else {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        };
        let run = self.run.take().expect("running");
        match done {
            #[cfg_attr(not(feature = "video"), allow(unused_mut))]
            Ok(mut report) => {
                #[cfg(feature = "video")]
                if let Some(d) = &run.decode {
                    d.fix_scope(&mut report);
                }
                let ms = run.started.elapsed().as_secs_f32() * 1000.0;
                self.set_status(format!("Done in {}", fmt_cost(ms.max(1.0))), false);
                #[cfg(feature = "video")]
                if run.decode.is_some() {
                    self.learn_speed(&report, ms as f64);
                }
                self.finish(report, Some((run.key, run.history)));
                self.result_roi = Some(run.roi);
            }
            Err(e) if e.is_empty() => {
                let prog = run.handle.progress();
                #[cfg(feature = "video")]
                let range = run.decode.is_some();
                #[cfg(not(feature = "video"))]
                let range = false;
                match prog.total() {
                    t if t > 1 && range => self.set_status(format!("Cancelled after {} of {t} frames", prog.done()), false),
                    _ => self.set_status("Cancelled", false),
                }
            }
            Err(e) => self.set_status(e, true),
        }
    }

    /// Results of the other pairs of a compare go to the stash, ready for
    /// when their pane is picked.
    fn poll_side_runs(&mut self) {
        let mut i = 0;
        while i < self.side_runs.len() {
            let mut done = None;
            while let Some(ev) = self.side_runs[i].run.handle.try_recv() {
                match ev {
                    JobEvent::Done(r) => done = Some(Some(*r)),
                    JobEvent::Failed(_) | JobEvent::Cancelled => done = Some(None),
                    _ => {}
                }
            }
            let Some(done) = done else {
                i += 1;
                continue;
            };
            let side = self.side_runs.remove(i);
            if let Some(mut report) = done {
                let ms = side.run.started.elapsed().as_secs_f32() * 1000.0;
                let maps = take_maps(&mut report);
                let summary = Some(summary::summarize(&facts_of(&report, &maps, self.fps, side.letter)));
                let status = Some((format!("Done in {}", fmt_cost(ms.max(1.0))), false));
                self.stash.insert(side.id, Stashed { report, maps, summary, result_roi: Some(side.run.roi), status });
            }
        }
    }

    /// Remembers how fast a range run went (for the automatic stride),
    /// smoothed over runs; short runs say little.
    #[cfg(feature = "video")]
    fn learn_speed(&mut self, r: &AnalysisReport, ms: f64) {
        let n = r.frames.len();
        let (w, h) = r.scope.roi.map(|r| (r.w, r.h)).unwrap_or((r.scope.width, r.scope.height));
        let mpix = w as f64 * h as f64 / 1e6;
        if n < 30 || mpix <= 0.0 {
            return;
        }
        let k = ((ms - 500.0).max(ms * 0.5) / (n as f64 * mpix)).clamp(0.5, 500.0);
        let pair = self.kind == Kind::VideoPair;
        let slot = if pair { &mut self.prefs.speed_pair } else { &mut self.prefs.speed_video };
        *slot = Some(slot.map(|old| 0.5 * old + 0.5 * k).unwrap_or(k));
        self.prefs.save();
    }

    /// Show a finished report and store it in the history (`store`).
    fn finish(&mut self, mut report: AnalysisReport, store: Option<(CacheKey, Option<CacheKey>)>) {
        drop_later(std::mem::take(&mut self.live));
        self.result_roi = None;
        let maps = take_maps(&mut report);
        drop_later(std::mem::replace(&mut self.maps, maps));
        if !report.frames.is_empty() {
            self.series = Series::from_report(&report);
            if let Some(f) = report.scope.frames {
                self.plot_span = (f.start, f.end.saturating_sub(1).max(f.start));
            }
        }
        self.summary = None;
        let shown = self.heat.id().filter(|_| self.heat.visible).map(str::to_string);
        self.pick_overlay(shown);
        if let Some((key, Some(hkey))) = store {
            let entry = HistoryEntry {
                key: key.hex(),
                created_unix: report.created_unix,
                scope: scope_label(&report),
                metrics: report.metrics.iter().map(|m| m.id.clone()).chain(report.columns.iter().map(|c| plots::metric_of(&c.key).to_string())).fold(Vec::new(), |mut v, id| {
                    if !v.contains(&id) {
                        v.push(id);
                    }
                    v
                }),
            };
            self.history.retain(|h| h.key != entry.key);
            self.history.insert(0, entry);
            self.history.truncate(HISTORY_MAX);
            let stored = report.clone();
            let (cache, history) = (self.cache(), self.history.clone());
            std::thread::spawn(move || {
                let _ = cache.put(&key, &stored);
                let _ = cache.put(&hkey, &history);
            });
        }
        self.report = Some(report);
    }

    /// Readies the overlay for the shown maps: `keep` (the map on screen)
    /// stays on screen when there is one of that metric; else a difference
    /// map is readied, shown when asked ("Show where", the map buttons).
    /// No-reference tile maps only when asked (they cover the photo).
    fn pick_overlay(&mut self, keep: Option<String>) {
        let same = keep.and_then(|id| self.map_of(&id).map(|m| (id, m)));
        self.heat.visible = same.is_some();
        let pick = same.or_else(|| {
            ["ssim.gauss", "deltae.2000", "diff.abs", "flip", "butteraugli"].iter().find_map(|id| self.map_of(id).map(|m| (id.to_string(), m)))
        });
        match pick {
            Some((id, m)) => self.heat.set_map(&id, m),
            None => self.heat.clear(),
        }
    }

    fn load_history(&mut self, h: &HistoryEntry) {
        match parse_key(&h.key).and_then(|k| self.cache().get::<AnalysisReport>(&k)) {
            Some(r) => {
                self.heat.clear();
                self.series = Series::default();
                self.set_status(format!("From history, {}", fmt_time(h.created_unix)), false);
                self.finish(r, None);
                // Whatever region it had, it is what the user asked to see.
                self.result_roi = None;
            }
            None => {
                self.history.retain(|x| x.key != h.key);
                self.set_status("That result is no longer in the cache", true);
            }
        }
    }

    // ---------------------------------------------------------------- overlay

    /// Draw the heatmap over an image that `xf` maps to the screen (in A's
    /// pixels; B panes use the same size, so the map lines up there too).
    /// With the pointer at `hover`, `pixels(x, y)` gives the A and B pixels
    /// for the readout.
    pub fn paint_heatmap(&mut self, ui: &Ui, painter: &Painter, xf: &ImageTransform, hover: Option<egui::Pos2>, pixels: &dyn Fn(f32, f32) -> (Option<[u8; 4]>, Option<[u8; 4]>)) {
        if self.stale() {
            return;
        }
        // Video frames are measured in native pixels; the picture may be turned.
        #[cfg(feature = "video")]
        let native = match (&self.video, self.kind) {
            (Some(v), Kind::Video | Kind::VideoPair) => Some((v.a.rotation, v.a.size)),
            _ => None,
        };
        #[cfg(not(feature = "video"))]
        let native = None;
        self.heat.set_native(native);
        self.heat.other = self.letter;
        self.heat.paint(ui, painter, xf, hover, pixels);
    }

    /// Graphs of the video range (live while it runs) for the host to draw
    /// under its timeline; `current` is A's frame on screen.
    pub fn video_plots(&mut self, ui: &mut Ui, p: &Palette, current: Option<u64>, plot_height: f32) {
        if self.series.is_empty() {
            return;
        }
        if let Some(a) = self.plots.ui(ui, p, &self.series, self.plot_span, current, plot_height) {
            self.plot_action(a);
        }
    }

    fn plot_action(&mut self, a: PlotAction) {
        match a {
            PlotAction::Seek(f) => self.seek = Some(f),
            PlotAction::Range(s, e) => {
                self.set_scope(VideoScope::Custom);
                self.range_text[0] = s.to_string();
                self.range_text[1] = e.to_string();
                self.set_status(format!("Range set to frames {s}-{}; press Analyze to measure it", e.saturating_sub(1)), false);
            }
        }
    }

    // ---------------------------------------------------------------- ui

    pub fn ui(&mut self, ui: &mut Ui, ctx: &MetricsCtx) {
        let p = *ctx.pal;
        self.shown = Some(Instant::now());
        self.sync(&ctx.subject);
        self.update_size_mismatch(&ctx.subject);
        self.cur_roi = ctx.roi;
        self.fps = ctx.subject.fps();
        self.poll(ui.ctx());
        if std::mem::take(&mut self.run_requested) && self.kind != Kind::None {
            self.run(ctx, ui.ctx());
        }
        if self.kind == Kind::None {
            ui.add_space(10.0);
            ui.label(RichText::new("Nothing to analyze yet").font(theme::text::subtitle()).color(p.text));
            look::note(ui, &p, "Open an image, compare two images (select them in the browser and press P) or open a video.");
            return;
        }
        if self.report.is_some() && self.summary.is_none() {
            self.summary = self.facts().map(|f| summary::summarize(&f));
        }
        ui.spacing_mut().item_spacing.y = 6.0;
        self.scope_bar(ui, &p, &ctx.subject, ctx.roi);
        self.run_row(ui, &p, ctx);
        if self.kind == Kind::Compare && self.compare_panes.len() > 2 {
            self.pairs_card(ui, &p);
        }
        if self.report.is_none() && self.live.is_empty() && self.run.is_none() && self.status.is_none() {
            self.intro(ui, &p);
        }
        self.summary_card(ui, &p);
        self.heatmap_row(ui, &p);
        let force = self.dev_open.take();
        #[cfg(feature = "video")]
        if !self.series.is_empty() {
            self.worst_section(ui, &p, &ctx.subject, force);
        }
        ui.add_space(2.0);
        if self.report.is_some() || !self.live.is_empty() {
            egui::CollapsingHeader::new(RichText::new("All numbers").font(theme::text::subtitle()).color(p.text)).icon(widgets::collapse_caret).id_salt("metrics-all-numbers").default_open(false).open(force).show(ui, |ui| self.results(ui, &p));
        }
        egui::CollapsingHeader::new(RichText::new("Details and options").font(theme::text::subtitle()).color(p.text)).icon(widgets::collapse_caret).id_salt("metrics-details").default_open(false).open(force).show(ui, |ui| {
            self.picker(ui, &p, &ctx.subject, ctx.roi);
            if ctx.subject.is_video() {
                self.range_fields(ui, &p, &ctx.subject, ctx.roi);
            }
            self.history_section(ui, &p);
        });
        if self.dev_scroll != 0.0 {
            ui.scroll_with_delta(Vec2::new(0.0, -std::mem::take(&mut self.dev_scroll)));
        }
    }

    /// Video: the scope choice (frame on screen, A-B loop, whole video) and
    /// what a range run would measure.
    fn scope_bar(&mut self, ui: &mut Ui, p: &Palette, s: &Subject, roi: Option<Roi>) {
        if !s.is_video() {
            return;
        }
        let has_loop = s.loop_ab().is_some();
        // A custom range (set in Details and options) shows as a fourth segment.
        let custom = self.scope == VideoScope::Custom;
        let mut labels = vec!["Current frame", "A-B loop", "Whole video"];
        let mut tips = vec![
            "The frame on screen",
            if has_loop { "The frames of the A-B loop" } else { "Set an A-B loop first (Ctrl+L)" },
            "The whole file; long videos are sampled to keep it under about 30 s",
        ];
        if custom {
            labels.push("Custom");
            tips.push("The range set in Details and options");
        }
        let scopes = [VideoScope::Frame, VideoScope::Loop, VideoScope::Whole, VideoScope::Custom];
        let mut idx = scopes.iter().position(|v| *v == self.scope).unwrap_or(0);
        // Without a loop the A-B loop segment only explains how to set one.
        if widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) && (idx != 1 || has_loop) {
            self.set_scope(scopes[idx]);
            self.status = None;
        }
        // One line whatever the scope, so Analyze below never moves when the
        // scope changes (a click aimed at it would miss).
        let (text, color) = match (self.scope, self.range_hint(s, roi)) {
            (VideoScope::Frame, _) => match (s.current(), s.frames()) {
                (Some(f), Some(n)) => (format!("The frame on screen: frame {f} of {n}"), p.muted),
                _ => ("The frame on screen".to_string(), p.muted),
            },
            (VideoScope::Custom, Ok(t)) => (format!("Custom range: {t}"), p.muted),
            (_, Ok(t)) => (t, p.muted),
            (_, Err(e)) => (e, p.danger),
        };
        let r = ui.add(egui::Label::new(RichText::new(&text).size(11.5).color(color)).truncate());
        if self.stride_auto && self.scope != VideoScope::Frame {
            r.on_hover_text(format!("{text}\nLong videos are sampled so the run takes about 30 s. Set a fixed stride in Details and options."));
        } else {
            r.on_hover_text(text);
        }
    }

    /// Frames a run will measure (1 unless a video range).
    fn frames_to_measure(&self, s: &Subject, roi: Option<Roi>) -> usize {
        if !s.is_video() || self.temporal == Temporal::Frame {
            return 1;
        }
        let total = s.frames().unwrap_or(0);
        self.planned_range(s, roi).map(|r| range_frames(r, total)).unwrap_or(0)
    }

    /// Start, end and stride of a custom range (Details and options).
    fn range_fields(&mut self, ui: &mut Ui, p: &Palette, s: &Subject, roi: Option<Roi>) {
        ui.add_space(4.0);
        ui.label(RichText::new("Frame range").size(theme::text::CAPTION).font(crate::ui::theme::semibold(theme::text::CAPTION)).color(p.muted));
        let mut edited = false;
        ui.horizontal_wrapped(|ui| {
            let field = |ui: &mut Ui, label: &str, text: &mut String, hint: &str, enabled: bool| {
                ui.label(RichText::new(label).size(theme::text::SMALL).color(p.muted));
                ui.add_enabled(enabled, egui::TextEdit::singleline(text).desired_width(56.0).hint_text(hint)).on_hover_text("Frame number, seconds (4.5s) or minutes:seconds (1:02)").changed()
            };
            edited |= field(ui, "Start", &mut self.range_text[0], "0", true);
            edited |= field(ui, "End", &mut self.range_text[1], "end", true);
        });
        if edited {
            self.set_scope(VideoScope::Custom);
            self.status = None;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Stride").size(theme::text::SMALL).color(p.muted));
            if self.stride_auto {
                let auto = self.planned_range(s, roi).map(|r| r.stride.to_string()).unwrap_or_else(|_| "-".into());
                let mut shown = auto;
                ui.add_enabled(false, egui::TextEdit::singleline(&mut shown).desired_width(56.0));
            } else if ui.add(egui::TextEdit::singleline(&mut self.range_text[2]).desired_width(56.0).hint_text("1")).on_hover_text("Measure every Nth frame").changed() {
                self.save_selection();
                self.status = None;
            }
            if widgets::checkbox(ui, &mut self.stride_auto, "Auto").on_hover_text("Pick the stride so a run takes about 30 s").changed() {
                self.save_selection();
            }
        });
        let total = s.frames();
        if let Err(e) = parse_range(&self.range_text[0], &self.range_text[1], if self.stride_auto { "1" } else { &self.range_text[2] }, s.fps(), total) {
            ui.label(RichText::new(e).size(11.5).color(p.danger));
        }
    }

    fn picker(&mut self, ui: &mut Ui, p: &Palette, s: &Subject, roi: Option<Roi>) {
        let mut changed = false;
        let mut presets: Vec<(&str, Vec<String>, &str)> = vec![("Recommended", default_metrics(self.kind), "What the summary uses: quick, and covers the basics")];
        if matches!(self.kind, Kind::Compare | Kind::VideoPair) {
            presets.push(("Quick", metrics::preset("quick").iter().map(|s| s.to_string()).collect(), "PSNR, fast SSIM and the absolute difference"));
        }
        presets.push(("Encode QA", metrics::preset("encode_qa").iter().map(|s| s.to_string()).collect(), "Fidelity, blocking and banding for encodes"));
        presets.push(("Photo technical", metrics::preset("photo_technical").iter().map(|s| s.to_string()).collect(), "Sharpness, noise, exposure and color cast"));
        let is = |ids: &Vec<String>| self.selected == ids.iter().cloned().collect::<BTreeSet<String>>();
        let current = presets.iter().find(|x| is(&x.1)).map(|x| x.0).unwrap_or(if self.selected.is_empty() { "None" } else { "Custom" });
        let mut pick: Option<Vec<String>> = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Metric set").size(theme::text::SMALL).color(p.muted));
            widgets::combo("metrics-preset").selected_text(current).width(ui.available_width()).show_ui(ui, |ui| {
                for (name, ids, tip) in &presets {
                    if ui.selectable_label(*name == current, *name).on_hover_text(*tip).clicked() {
                        pick = Some(ids.clone());
                    }
                }
                ui.separator();
                if ui.selectable_label(current == "None", "None").on_hover_text("Clear the selection, then tick metrics below").clicked() {
                    pick = Some(Vec::new());
                }
            });
        });
        if let Some(ids) = pick {
            self.selected = ids.into_iter().collect();
            changed = true;
        }
        let (w, h) = s.size().unwrap_or((1920, 1080));
        let (w, h) = roi.map(|r| (r.w, r.h)).unwrap_or((w, h));
        let frames = self.frames_to_measure(s, roi).max(1) as f32;
        let threads = analysis_threads() as f32;
        // Frames run in parallel in range jobs.
        let scale = if frames > 1.0 { frames / threads } else { 1.0 };
        let groups = [(MetricKind::FullReference, "Full reference (A vs B)"), (MetricKind::NoReference, "No reference"), (MetricKind::Temporal, "Temporal")];
        for (kind, title) in groups {
            if kind == MetricKind::Temporal && !s.is_video() {
                continue;
            }
            let blocked = self.blocked(kind);
            let n_sel = if kind == MetricKind::Temporal {
                metrics::temporal::all().iter().filter(|t| self.selected.contains(t.id)).count()
            } else {
                metrics::all().iter().filter(|m| m.info().kind == kind && self.selected.contains(m.id())).count()
            };
            let head = RichText::new(format!("{title}{}", if n_sel > 0 { format!("  ({n_sel})") } else { String::new() })).size(theme::text::BODY).color(if blocked.is_some() { p.faint } else { p.text });
            egui::CollapsingHeader::new(head).icon(widgets::collapse_caret).id_salt(("metrics-group", title)).default_open(false).show(ui, |ui| {
                if let Some(why) = &blocked {
                    ui.add(egui::Label::new(RichText::new(why).size(11.0).color(p.faint)).wrap());
                }
                let mut entries: Vec<(&'static str, &'static str, &'static str, Option<f32>, bool)> = Vec::new();
                if kind == MetricKind::Temporal {
                    for t in metrics::temporal::all() {
                        entries.push((t.id, t.name, t.description, Some(2.0 * (w as f32 * h as f32) / (3840.0 * 2160.0)), false));
                    }
                } else {
                    for m in metrics::all().iter().filter(|m| m.info().kind == kind) {
                        let i = m.info();
                        let small = w < i.min_size.0 || h < i.min_size.1;
                        entries.push((i.id, i.name, i.description, (!small).then(|| i.cost_ms(w, h)), i.heavy));
                    }
                }
                for (id, name, desc, cost, heavy) in entries {
                    ui.horizontal(|ui| {
                        let mut on = self.selected.contains(id);
                        let resp = ui.add_enabled_ui(blocked.is_none(), |ui| widgets::checkbox(ui, &mut on, name)).inner.on_hover_text(desc);
                        if resp.changed() {
                            if on {
                                self.selected.insert(id.to_string());
                            } else {
                                self.selected.remove(id);
                            }
                            changed = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (text, color) = match cost {
                                None => ("too small".to_string(), p.warn),
                                Some(ms) => (if ms * scale < 1.0 { fmt_cost(0.0) } else { format!("~{}", fmt_cost(ms * scale)) }, if heavy { p.warn } else { p.faint }),
                            };
                            ui.label(RichText::new(text).size(11.0).color(color)).on_hover_text(if heavy { "Heavy: runs on a quarter-size preview first" } else { "Estimated time" });
                        });
                    });
                }
            });
        }
        if changed {
            self.save_selection();
            self.status = None;
        }
    }

    /// Analyze (or Cancel with progress), the export menu and the status line.
    fn run_row(&mut self, ui: &mut Ui, p: &Palette, ctx: &MetricsCtx) {
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.spacing_mut().interact_size.y = theme::size::CONTROL;
            if let Some(run) = &self.run {
                let prog = run.handle.progress();
                let frac = prog.fraction().unwrap_or(0.0);
                let eta = prog.eta().map(fmt_eta).unwrap_or_default();
                let text = if prog.total() > 1 { format!("{} of {}  {eta}", prog.done(), prog.total()) } else if eta.is_empty() { "Analyzing".to_string() } else { eta };
                // Right to left: the button takes its own width and the bar the rest, so the
                // row never asks for more than the panel has (a wider row grows the panel).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        cancel = true;
                    }
                    let bar_w = ui.available_width().max(40.0);
                    ui.add(egui::ProgressBar::new(frac).fill(p.accent).desired_width(bar_w).desired_height(theme::size::CONTROL).corner_radius(6).text(RichText::new(text).size(11.5)));
                });
            } else {
                let n = self.runnable().len();
                let what = match (self.kind, ctx.roi.is_some(), self.scope) {
                    (Kind::Video | Kind::VideoPair, _, VideoScope::Whole) => "the whole video",
                    (Kind::Video | Kind::VideoPair, _, VideoScope::Loop) => "the A-B loop",
                    (Kind::Video | Kind::VideoPair, _, VideoScope::Custom) => "the range",
                    (_, true, _) => "the region",
                    (Kind::Video | Kind::VideoPair, false, _) => "the frame on screen",
                    (Kind::Compare, false, _) => "",
                    _ => "the whole image",
                };
                let what = match self.compare_panes.len() {
                    _ if !what.is_empty() => what.to_string(),
                    3 => "B and C against A".to_string(),
                    4 => "B, C and D against A".to_string(),
                    _ => "B against A".to_string(),
                };
                let tip = format!("Measure {what} with {n} metric{} (Shift+Q)", if n == 1 { "" } else { "s" });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.export_menu(ui);
                    let bw = ui.available_width().max(80.0);
                    let analyze = ui.allocate_ui_with_layout(Vec2::new(bw, theme::size::CONTROL), egui::Layout::top_down_justified(egui::Align::Center), |ui| widgets::primary_button_enabled(ui, p, n > 0, "Analyze")).inner;
                    if analyze.on_hover_text(tip).on_disabled_hover_text("No metric applies here: choose some in Details and options").clicked() {
                        self.run(ctx, ui.ctx());
                    }
                });
            }
        });
        if cancel {
            self.cancel();
        }
        if self.stale() && self.run.is_none() && self.report.is_some() {
            ui.label(RichText::new(format!("{}  The region changed: Analyze again to update", I::WARNING)).size(11.5).color(p.warn));
        } else if let Some((msg, err)) = &self.status {
            ui.add(egui::Label::new(RichText::new(msg).size(11.5).color(if *err { p.danger } else { p.faint })).wrap());
        } else if let (Some(m), Kind::VideoPair) = (&self.size_mismatch, self.kind) {
            ui.add(egui::Label::new(RichText::new(m).size(11.5).color(p.warn)).wrap());
        }
    }

    /// A 3 or 4 image compare: one row per pane measured against A with its
    /// verdict; the picked row's details are shown below it.
    fn pairs_card(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(a) = self.compare_panes.first().cloned() else { return };
        let mut pick = None;
        ui.label(RichText::new("Each image against A").size(theme::text::CAPTION).font(theme::semibold(theme::text::CAPTION)).color(p.muted));
        look::card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, path) in self.compare_panes.iter().enumerate().skip(1) {
                let id = subject_id(Kind::Compare, Some(&a), Some(path));
                let shown = i == self.pair_pick;
                let running = if shown { self.run.is_some() } else { self.side_runs.iter().any(|r| r.id == id) };
                let sum = if shown { self.summary.as_ref() } else { self.stash.get(&id).and_then(|s| s.summary.as_ref()) };
                let head = sum.and_then(|s| s.head.as_ref());
                let value = sum.and_then(|s| s.closeness).map(|c| c.short().to_string());
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::size::LIST_ROW), Sense::click());
                let painter = ui.painter();
                let bg = rect.expand2(Vec2::new(6.0, 0.0));
                if shown {
                    painter.rect_filled(bg, egui::CornerRadius::same(theme::radius::CONTROL), p.selected_bg);
                } else if resp.hovered() {
                    painter.rect_filled(bg, egui::CornerRadius::same(theme::radius::CONTROL), p.hover);
                }
                let c = egui::pos2(rect.min.x + 8.0, rect.center().y);
                painter.circle_filled(c, 8.0, look::pane_color(p, i));
                painter.text(c, egui::Align2::CENTER_CENTER, LETTERS[i.min(3)], theme::semibold(10.5), egui::Color32::WHITE);
                // Right: verdict dot and number (or what is happening).
                let (dot, right) = match (head, running) {
                    (_, true) => (None, "Analyzing".to_string()),
                    (Some(h), _) => (Some(look::level_color(p, h.level)), value.unwrap_or_default()),
                    (None, _) => (None, "Not analyzed".to_string()),
                };
                let rg = painter.layout_no_wrap(right, theme::text::small(), if head.is_some() && !running { p.text } else { p.faint });
                let rx = rect.max.x - rg.size().x;
                painter.galley(egui::pos2(rx, rect.center().y - rg.size().y / 2.0), rg, p.text);
                if let Some(d) = dot {
                    painter.circle_filled(egui::pos2(rx - 9.0, rect.center().y), 3.5, d);
                }
                let name_w = (rx - 20.0 - (rect.min.x + 22.0)).max(20.0);
                let mut job = egui::text::LayoutJob::simple_singleline(super::file_name(path), theme::text::body(), if shown { p.text } else { p.muted });
                job.wrap = egui::text::TextWrapping::truncate_at_width(name_w);
                let ng = painter.layout_job(job);
                painter.galley(egui::pos2(rect.min.x + 22.0, rect.center().y - ng.size().y / 2.0), ng, p.text);
                let tip = match head {
                    Some(h) => format!("{}\n{}", h.title, h.detail),
                    None if running => format!("Measuring {} against A", LETTERS[i.min(3)]),
                    None => "Press Analyze to measure it".to_string(),
                };
                let resp = resp.on_hover_text(if shown { tip } else { format!("{tip}\nClick to show its details") });
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    pick = Some(i);
                }
            }
        });
        if let Some(i) = pick {
            self.pair_pick = i;
        }
    }

    /// What the tab does, before the first run.
    fn intro(&self, ui: &mut Ui, p: &Palette) {
        let what = match self.kind {
            Kind::Compare if self.compare_panes.len() > 2 => "Measures each image against A: how close it is, and where they differ.",
            Kind::Compare => "Shows how close B is to A, and where they differ.",
            Kind::Video => "Checks the frame on screen, the A-B loop or the whole video.",
            Kind::VideoPair => "Compares B with A on the frame on screen, the A-B loop or the whole video.",
            _ => "Checks sharpness, noise, exposure, color and compression in one click.",
        };
        look::note(ui, p, what);
        ui.add(egui::Label::new(RichText::new("Press R and drag on the image to check only a region.").size(11.5).color(p.faint)).wrap());
    }

    /// The facts the summary is written from (the shown report).
    fn facts(&self) -> Option<summary::Facts> {
        Some(facts_of(self.report.as_ref()?, &self.maps, self.fps, self.letter))
    }

    /// The plain-language result: headline, one line per check, and the
    /// one follow-up (Show where, Show worst frame).
    fn summary_card(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(s) = self.summary.clone() else { return };
        if s.head.is_none() && s.lines.is_empty() {
            return;
        }
        let stale = self.stale();
        let mut toggle_map = false;
        let mut line_map: Option<String> = None;
        let mut seek = None;
        look::card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if let Some(h) = &s.head {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    look::dot(ui, if stale { p.faint } else { look::level_color(p, h.level) }, if stale { "Out of date: analyze again" } else { look::level_text(h.level) });
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add(egui::Label::new(RichText::new(&h.title).font(theme::text::subtitle()).color(if stale { p.faint } else { p.text })).wrap());
                        ui.add(egui::Label::new(RichText::new(&h.detail).size(theme::text::SMALL).color(p.muted)).wrap());
                    });
                });
                if !s.lines.is_empty() {
                    ui.add_space(5.0);
                    let r = ui.available_rect_before_wrap();
                    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border));
                    ui.add_space(5.0);
                }
            }
            let links = if stale { Vec::new() } else { self.line_maps() };
            for l in &s.lines {
                let map = plots::metric_of(&l.key);
                let link = links.iter().any(|m| m == map).then(|| if self.heat.visible && self.heat.id() == Some(map) { "Hide" } else { "Show where" });
                if look::status_row(ui, p, l.level, &l.label, &l.value, &l.tip, stale, link).1 {
                    line_map = Some(map.to_string());
                }
            }
            if !s.cuts.is_empty() {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add_space(18.0);
                    ui.label(RichText::new("Cuts at").size(11.5).color(p.muted));
                    for &c in s.cuts.iter().take(16) {
                        let t = summary::fmt_clock(c as f64 / self.fps.max(1.0));
                        if ui.add(egui::Button::new(RichText::new(t).size(theme::text::SMALL).color(p.accent)).frame(false)).on_hover_text(format!("Show frame {c}")).clicked() {
                            seek = Some(c);
                        }
                    }
                    if s.cuts.len() > 16 {
                        ui.label(RichText::new(format!("and {} more", s.cuts.len() - 16)).size(11.5).color(p.faint));
                    }
                });
            }
            let action = s.action.clone().filter(|_| !stale);
            if action.is_some() || !s.notes.is_empty() {
                ui.add_space(4.0);
            }
            for n in &s.notes {
                ui.label(RichText::new(n).size(11.0).color(p.faint));
            }
            match action {
                Some(summary::Action::ShowMap { id, why }) => {
                    let on = self.heat.visible && self.heat.id() == Some(id.as_str());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Biggest difference: {why}")).size(theme::text::SMALL).color(p.muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let text = if on { "Hide" } else { "Show where" };
                            let tip = format!("Color the image by how much {} differs from A there", self.letter);
                            if widgets::secondary_button(ui, text).on_hover_text(tip).clicked() {
                                toggle_map = true;
                            }
                        });
                    });
                }
                Some(summary::Action::Seek { frame }) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Worst: frame {frame}")).size(theme::text::SMALL).color(p.muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if widgets::secondary_button(ui, "Show worst frame").on_hover_text("Pause on the worst frame").clicked() {
                                seek = Some(frame);
                            }
                        });
                    });
                }
                None => {}
            }
        });
        if toggle_map {
            if self.heat.visible && self.heat.id().is_some() && matches!(&s.action, Some(summary::Action::ShowMap { id, .. }) if self.heat.id() == Some(id.as_str())) {
                self.heat.visible = false;
            } else {
                self.show_where();
            }
        }
        if let Some(id) = line_map {
            if self.heat.visible && self.heat.id() == Some(id.as_str()) {
                self.heat.visible = false;
            } else if let Some(m) = self.map_of(&id) {
                self.heat.set_map(&id, m);
                self.heat.visible = true;
            }
        }
        if seek.is_some() {
            self.seek = seek;
        }
    }

    /// Metrics whose summary line offers "Show where" (lines worth a look
    /// that have a map: clipping, banding, blocking, soft detail). Pairs
    /// have one "Show where" for the biggest difference instead.
    fn line_maps(&self) -> Vec<String> {
        let Some(s) = &self.summary else { return Vec::new() };
        if matches!(s.action, Some(summary::Action::ShowMap { .. })) {
            return Vec::new();
        }
        s.lines
            .iter()
            .filter(|l| matches!(l.level, summary::Level::Fair | summary::Level::Poor))
            .map(|l| plots::metric_of(&l.key).to_string())
            .filter(|id| self.maps.iter().any(|m| &m.0 == id))
            .collect()
    }

    /// While a heatmap is on the image: its name, Hide, and its options.
    fn heatmap_row(&mut self, ui: &mut Ui, p: &Palette) {
        if self.stale() || !self.heat.active() {
            return;
        }
        let name = self.heat.id().and_then(|id| self.maps.iter().find(|m| m.0 == id).map(|m| m.1.clone())).unwrap_or_else(|| "Heatmap".into());
        // The summary's "Show where" already toggles its own map.
        let from_card = matches!(self.summary.as_ref().and_then(|s| s.action.as_ref()), Some(summary::Action::ShowMap { id, .. }) if self.heat.id() == Some(id.as_str()))
            || self.line_maps().iter().any(|m| self.heat.id() == Some(m.as_str()));
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Heatmap on the image: {name}")).size(theme::text::SMALL).color(p.muted));
            if !from_card {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::secondary_button(ui, "Hide").clicked() {
                        self.heat.visible = false;
                    }
                });
            }
        });
        egui::CollapsingHeader::new(RichText::new("Heatmap options").font(theme::text::subtitle()).color(p.text)).icon(widgets::collapse_caret).id_salt("metrics-heatmap").default_open(false).show(ui, |ui| self.heatmap_controls(ui, p));
    }

    /// The value's grade as a dot; what it means (and the limits) on hover.
    /// A value the summary talks about (`verdict`) has the summary's dot,
    /// so the summary and the table never disagree.
    fn chip(ui: &mut Ui, p: &Palette, key: &str, value: Option<f64>, stale: bool, verdict: Option<(summary::Level, String)>) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        if stale || value.is_none() {
            return;
        }
        let limits = threshold(key).map(|t| {
            let (good, poor) = if t.higher_better { ("at or above", "below") } else { ("below", "above") };
            format!("good {good} {}, poor {poor} {}", t.good, t.poor)
        });
        if let Some((level, said)) = verdict {
            ui.painter().circle_filled(rect.center(), 3.5, look::level_color(p, level));
            let limits = limits.filter(|_| level != summary::Level::Info).map(|l| format!("\nLimits: {l}")).unwrap_or_default();
            resp.on_hover_text(format!("{}. {said}{limits}", look::level_text(level)));
            return;
        }
        let (Some(limits), Some(g)) = (limits, value.and_then(|v| grade(key, v))) else { return };
        let (c, what) = match g {
            Grade::Good => (p.ok, "Good"),
            Grade::Fair => (p.warn, "Fair"),
            Grade::Poor => (p.danger, "Poor"),
        };
        ui.painter().circle_filled(rect.center(), 3.5, c);
        resp.on_hover_text(format!("{what}: {limits}"));
    }

    fn arrow(ui: &mut Ui, p: &Palette, d: Direction) {
        let (icon, tip) = match d {
            Direction::HigherIsBetter => (I::ARROW_UP, "Higher is better"),
            Direction::LowerIsBetter => (I::ARROW_DOWN, "Lower is better"),
            Direction::Neutral => ("", ""),
        };
        // A fixed column, so the values line up.
        let (rect, r) = ui.allocate_exact_size(Vec2::new(10.0, 16.0), Sense::hover());
        if !tip.is_empty() {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(11.0), p.faint);
            r.on_hover_text(tip);
        }
    }

    fn results(&mut self, ui: &mut Ui, p: &Palette) {
        let mut rows = match &self.report {
            Some(r) => rows_of_report(r),
            None => rows_of_live(&self.live),
        };
        for row in &mut rows {
            row.has_map |= self.maps.iter().any(|m| m.0 == row.id);
        }
        // Stale results stay readable, dimmed, until the next run.
        let stale = self.stale() && self.report.is_some();
        let (text, muted) = if stale { (p.faint, p.faint) } else { (p.text, p.muted) };
        // Report notes (skipped metrics) are warnings; a metric's own notes explain its value.
        let notes: Vec<(String, bool)> = self
            .report
            .as_ref()
            .map(|r| r.notes.iter().map(|n| (n.clone(), true)).chain(r.metrics.iter().flat_map(|m| m.notes.iter().map(move |n| (format!("{}: {n}", m.name), false)))).collect())
            .unwrap_or_default();
        if rows.is_empty() && notes.is_empty() {
            return;
        }
        let mut toggle: Option<String> = None;
        let mut show_map: Option<String> = None;
        let mut seek: Option<u64> = None;
        // Rows fit the panel: the value and arrow sit at the right edge, the
        // name takes what is left and is cut short when the panel is narrow.
        let name_job = |name: &str, key: Option<&str>, color: egui::Color32, size: f32, width: f32| {
            let mut job = egui::text::LayoutJob::default();
            job.append(name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(size), color));
            if let Some(k) = key {
                job.append(&format!("  {k}"), 0.0, egui::TextFormat::simple(egui::FontId::proportional(11.0), p.faint));
            }
            job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(20.0));
            job
        };
        let right = egui::Layout::right_to_left(egui::Align::Center);
        let left = egui::Layout::left_to_right(egui::Align::Center);
        // Live rows (while running) have no summary yet: thresholds only.
        let summary = self.summary.clone().filter(|_| self.report.is_some());
        let verdict = |key: &str| summary.as_ref().and_then(|s| s.verdict(key));
        ui.spacing_mut().item_spacing.y = 2.0;
        for row in &rows {
            let Some(head) = row.cells.first() else { continue };
            let open = self.expanded.contains(&row.id);
            let many = row.cells.len() > 1;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.spacing_mut().interact_size.y = 20.0;
                Self::chip(ui, p, &head.key, head.value, stale, verdict(&head.key));
                ui.with_layout(right, |ui| {
                    Self::arrow(ui, p, head.direction);
                    let v = ui.label(RichText::new(head.value.map(|v| fmt_value(v, head.unit)).unwrap_or_else(|| "-".into())).color(text).strong());
                    if let Some(t) = &head.tip {
                        if v.on_hover_text(t).clicked() {
                            seek = head.worst;
                        }
                    }
                    if row.has_map {
                        let on = self.heat.visible && self.heat.id() == Some(row.id.as_str());
                        if ui.add(egui::Button::new(RichText::new(I::SELECTION).size(12.0)).selected(on).small()).on_hover_text("Show its heatmap on the image").clicked() {
                            show_map = Some(row.id.clone());
                        }
                    }
                    if row.preview {
                        ui.label(RichText::new("preview").size(10.0).color(p.warn));
                    }
                    ui.with_layout(left, |ui| {
                        let caret = if many { if open { I::CARET_DOWN } else { I::CARET_RIGHT } } else { " " };
                        // Which value the row shows (PSNR: average, Exposure: clipping).
                        let key = many.then(|| key_label(&row.id, short_key(&row.id, &head.key)));
                        let w = ui.available_width();
                        let mut job = name_job(&format!("{caret} {}", row.name), key.as_deref(), text, 13.0, w);
                        // Too long: drop the method in brackets ("Sharpness (Laplacian
                        // variance)"), so the value's name still shows; the full name is on hover.
                        if let Some((short, _)) = row.name.split_once(" (").filter(|_| ui.painter().layout_job(job.clone()).elided) {
                            job = name_job(&format!("{caret} {short}"), key.as_deref(), text, 13.0, w);
                        }
                        // Truncate: in a horizontal layout a label would grow under the map button.
                        let name = ui.add(egui::Label::new(job).truncate().sense(Sense::click())).on_hover_text(&row.name);
                        if many && name.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            toggle = Some(row.id.clone());
                        }
                    });
                });
            });
            if open {
                // Every value with its key, the headline too.
                for c in &row.cells {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.spacing_mut().interact_size.y = 18.0;
                        Self::chip(ui, p, &c.key, c.value, stale, verdict(&c.key));
                        ui.with_layout(right, |ui| {
                            Self::arrow(ui, p, c.direction);
                            let v = ui.label(RichText::new(c.value.map(|v| fmt_value(v, c.unit)).unwrap_or_else(|| "-".into())).color(muted).size(theme::text::SMALL));
                            if let Some(t) = &c.tip {
                                v.on_hover_text(t);
                            }
                            ui.with_layout(left, |ui| {
                                ui.add_space(14.0);
                                let short = key_label(&row.id, short_key(&row.id, &c.key));
                                let job = name_job(&short, None, muted, theme::text::SMALL, ui.available_width());
                                let lab = ui.add(egui::Label::new(job).truncate().sense(Sense::click()));
                                if c.worst.is_some() && lab.on_hover_text("Click to show the worst frame").clicked() {
                                    seek = c.worst;
                                }
                            });
                        });
                    });
                }
            }
        }
        if let Some(id) = toggle {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
        }
        if let Some(id) = show_map {
            let map = self.map_of(&id).or_else(|| self.live.iter().find(|m| m.id == id).and_then(|m| m.map.clone()).map(Arc::new));
            if let Some(m) = map {
                self.heat.set_map(&id, m);
                self.heat.visible = true;
            }
        }
        if seek.is_some() {
            self.seek = seek;
        }
        if !notes.is_empty() {
            ui.add_space(4.0);
            for (n, warn) in notes {
                ui.add(egui::Label::new(RichText::new(format!("{}  {n}", I::INFO)).size(11.0).color(if warn { p.warn } else { p.muted })).wrap());
            }
        }
    }

    /// Which map, colormap, range, opacity and threshold.
    fn heatmap_controls(&mut self, ui: &mut Ui, p: &Palette) {
        let maps: Vec<(String, String)> = self.maps.iter().map(|m| (m.0.clone(), m.1.clone())).collect();
        {
            if !maps.is_empty() {
                let cur = self.heat.id().map(str::to_string).unwrap_or_default();
                let name = maps.iter().find(|m| m.0 == cur).map(|m| m.1.clone()).unwrap_or_else(|| "Choose".into());
                let mut pick = None;
                widgets::combo("heatmap-metric").selected_text(name).show_ui(ui, |ui| {
                    for (id, n) in &maps {
                        if ui.selectable_label(*id == cur, n).clicked() {
                            pick = Some(id.clone());
                        }
                    }
                });
                if let Some(id) = pick {
                    if let Some(m) = self.map_of(&id) {
                        self.heat.set_map(&id, m);
                    }
                }
            }
            self.heat.controls(ui, p);
        }
    }

    #[cfg(feature = "video")]
    fn worst_section(&mut self, ui: &mut Ui, p: &Palette, s: &Subject, force: Option<bool>) {
        let Some(vr) = self.video_run.clone() else { return };
        let Subject::Video(_) = s else { return };
        egui::CollapsingHeader::new(RichText::new("Worst frames").font(theme::text::subtitle()).color(p.text)).icon(widgets::collapse_caret).id_salt("metrics-worst").default_open(false).open(force).show(ui, |ui| {
            let b_path = vr.b.as_ref().map(|b| b.path.as_path());
            if self.thumbs.as_ref().is_none_or(|t| !t.shows(&vr.a.path, b_path, vr.offset, vr.region)) {
                self.thumbs = Some(adapters::VideoThumbs::new(vr.a.clone(), vr.b.clone(), vr.offset, vr.region));
            }
            // Thumbnails once the run is done: while rows arrive the list
            // keeps changing, and each new entry would be decoded.
            let done = self.run.is_none();
            let thumbs = self.thumbs.as_mut().filter(|_| done).map(|t| t as &mut dyn plots::FrameThumbs);
            if let Some(a) = self.plots.worst_ui(ui, p, &self.series, thumbs) {
                self.plot_action(a);
            }
            if !self.plots.worst_frames(&self.series).is_empty() && ui.button(format!("{}  Export worst frames as PNG pairs", I::DOWNLOAD_SIMPLE)).clicked() {
                self.export_worst(None);
            }
        });
    }

    fn history_section(&mut self, ui: &mut Ui, p: &Palette) {
        if self.history.is_empty() {
            return;
        }
        let mut load = None;
        egui::CollapsingHeader::new(RichText::new(format!("History ({})", self.history.len())).size(theme::text::BODY).color(p.text)).icon(widgets::collapse_caret).id_salt("metrics-history").default_open(self.show_history).show(ui, |ui| {
            for h in &self.history {
                let r = ui
                    .vertical(|ui| {
                        ui.label(RichText::new(format!("{}  {}", fmt_time(h.created_unix), h.scope)).size(12.0).color(p.text));
                        ui.label(RichText::new(h.metrics.join(", ")).size(11.0).color(p.faint));
                    })
                    .response
                    .interact(Sense::click());
                if r.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Show this result").clicked() {
                    load = Some(h.clone());
                }
            }
        });
        if let Some(h) = load {
            self.load_history(&h);
        }
    }

    // ---------------------------------------------------------------- export

    fn stem(&self) -> String {
        self.file_a.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "lumen".into())
    }

    fn dialog(&self) -> rfd::FileDialog {
        let d = rfd::FileDialog::new();
        match self.file_a.as_ref().and_then(|p| p.parent()) {
            Some(dir) => d.set_directory(dir),
            None => d,
        }
    }

    fn export_menu(&mut self, ui: &mut Ui) {
        let has_report = self.report.is_some();
        let has_map = self.heat.map().is_some();
        #[cfg(feature = "video")]
        let has_frames = self.video_run.is_some() && !self.series.is_empty();
        #[cfg(not(feature = "video"))]
        let has_frames = false;
        ui.add_enabled_ui(has_report, |ui| {
            let menu = ui.menu_button(RichText::new(I::EXPORT).size(15.0), |ui| {
                if ui.button("CSV").clicked() {
                    ui.close();
                    self.export_report(true, None);
                }
                if ui.button("JSON").clicked() {
                    ui.close();
                    self.export_report(false, None);
                }
                if ui.add_enabled(has_map, egui::Button::new("Heatmap PNG")).clicked() {
                    ui.close();
                    self.export_heatmap(true, None);
                }
                if ui.add_enabled(has_map, egui::Button::new("Heatmap TIFF (values)")).clicked() {
                    ui.close();
                    self.export_heatmap(false, None);
                }
                if has_frames && ui.button("Worst frames as PNG pairs").clicked() {
                    ui.close();
                    self.export_worst(None);
                }
                ui.separator();
                if ui.button("Report bundle folder").clicked() {
                    ui.close();
                    self.bundle_request = Some(None);
                }
            });
            menu.response.on_hover_text("Export: CSV, JSON, heatmaps or a report folder").on_disabled_hover_text("Analyze first, then export the results");
        });
    }

    /// Run `f` off the UI thread and show its result in the status line.
    fn background(&mut self, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        self.bg.push(rx);
        self.set_status("Exporting", false);
    }

    /// The report as CSV or JSON to `path` (asked when `None`).
    fn export_report(&mut self, csv: bool, path: Option<PathBuf>) {
        if self.report.is_none() {
            return;
        }
        let (ext, name) = if csv { ("csv", "CSV") } else { ("json", "JSON") };
        let Some(path) = path.or_else(|| self.dialog().add_filter(name, &[ext]).set_file_name(format!("{}_metrics.{ext}", self.stem())).save_file()) else { return };
        let Some(r) = &self.report else { return };
        let res = if csv { r.write_csv(&path) } else { r.write_json(&path) };
        match res {
            Ok(()) => self.set_status(format!("Saved {}", path_text(&path)), false),
            Err(e) => self.set_status(format!("Could not save: {e}"), true),
        }
    }

    /// The heatmap shown, colorized (PNG) or as values (TIFF), to `path` (asked when `None`).
    fn export_heatmap(&mut self, png: bool, path: Option<PathBuf>) {
        let Some(id) = self.heat.id().map(str::to_string) else { return };
        let (ext, name) = if png { ("png", "PNG") } else { ("tif", "TIFF") };
        let file = format!("{}_{}.{ext}", self.stem(), id.replace('.', "_"));
        let Some(path) = path.or_else(|| self.dialog().add_filter(name, &[ext]).set_file_name(file).save_file()) else { return };
        let Some(map) = self.heat.shared_map() else { return };
        // The overlay's coloring with its opacity; written off the UI thread.
        let o = lumen_analysis::ColorizeOptions { alpha: (self.heat.opacity.clamp(0.0, 1.0) * 255.0).round().max(1.0) as u8, ..self.heat.resolved_options() };
        self.background(move || {
            let res = if png { map.write_png(&path, &o) } else { map.write_tiff(&path) };
            res.map(|()| format!("Saved {}", path_text(&path))).map_err(|e| format!("Could not save: {e}"))
        });
    }

    /// Worst frames of the ranked column as A/B PNG pairs into `dir` (asked when `None`).
    #[allow(unused_variables)]
    fn export_worst(&mut self, dir: Option<PathBuf>) {
        #[cfg(feature = "video")]
        {
            let Some(vr) = self.video_run.clone() else { return };
            let frames = self.plots.worst_frames(&self.series);
            let Some(dir) = dir.or_else(|| self.dialog().pick_folder()) else { return };
            self.background(move || match adapters::export_frame_pairs(&vr.a, vr.b.as_ref(), vr.offset, &frames, &dir) {
                Ok(files) => Ok(format!("Saved {} frames to {}", files.len(), path_text(&dir))),
                Err(e) => Err(format!("Could not export frames: {e:#}")),
            });
        }
    }

    /// A folder with the report as JSON and CSV, every heatmap as PNG and
    /// TIFF, the worst frames of a video range as PNG pairs and the AI
    /// results for the same files (`ai_cards`, Markdown) when there are any.
    /// `parent`: where the folder goes (`None`: ask).
    pub fn export_bundle(&mut self, parent: Option<PathBuf>, ai_cards: Vec<String>) {
        let Some(report) = self.report.clone() else { return };
        let Some(parent) = parent.or_else(|| self.dialog().pick_folder()) else { return };
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let dir = PathBuf::from(path_text(&parent.join(format!("{}_analysis_{stamp}", self.stem()))));
        let maps = self.maps.clone();
        // The map on screen is written as it is shown; the others with their defaults.
        let shown = self.heat.id().map(str::to_string);
        let opts = self.heat.resolved_options();
        let opacity = self.heat.opacity;
        #[cfg(feature = "video")]
        let frames = self.video_run.clone().filter(|_| !self.series.is_empty()).map(|vr| (vr, self.plots.worst_frames(&self.series)));
        self.background(move || {
            let res = (|| -> anyhow::Result<usize> {
                std::fs::create_dir_all(&dir)?;
                report.write_json(&dir.join("report.json"))?;
                report.write_csv(&dir.join("report.csv"))?;
                let mut n = 2;
                for (id, _, map) in &maps {
                    let base = id.replace('.', "_");
                    let o = if shown.as_deref() == Some(id.as_str()) { opts } else { map.default_options() };
                    let o = lumen_analysis::ColorizeOptions { alpha: (opacity.clamp(0.1, 1.0) * 255.0) as u8, threshold: None, ..o };
                    map.write_png(&dir.join(format!("heatmap_{base}.png")), &o)?;
                    map.write_tiff(&dir.join(format!("heatmap_{base}.tif")))?;
                    n += 2;
                }
                if !ai_cards.is_empty() {
                    let md = format!("# AI analysis\n\n{}", ai_cards.join("\n---\n\n"));
                    lumen_analysis::report::write_atomic(&dir.join("ai_cards.md"), md.as_bytes())?;
                    n += 1;
                }
                #[cfg(feature = "video")]
                if let Some((vr, frames)) = frames {
                    n += adapters::export_frame_pairs(&vr.a, vr.b.as_ref(), vr.offset, &frames, &dir.join("worst_frames"))?.len();
                }
                Ok(n)
            })();
            match res {
                Ok(n) => Ok(format!("Saved {n} files to {}", path_text(&dir))),
                Err(e) => Err(format!("Could not save the bundle: {e:#}")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_follow_metric_thresholds() {
        assert_eq!(grade("psnr.y", 41.0), Some(Grade::Good));
        assert_eq!(grade("psnr.y", 40.0), Some(Grade::Good));
        assert_eq!(grade("psnr.y", 35.0), Some(Grade::Fair));
        assert_eq!(grade("psnr.y", 29.9), Some(Grade::Poor));
        assert_eq!(grade("psnr.rgb.avg", 45.0), Some(Grade::Good));
        assert_eq!(grade("psnr.global", 25.0), Some(Grade::Poor));
        assert_eq!(grade("deltae.2000.mean", 0.5), Some(Grade::Good));
        assert_eq!(grade("deltae.2000.mean", 2.0), Some(Grade::Fair));
        assert_eq!(grade("deltae.76.mean", 3.5), Some(Grade::Poor));
        assert_eq!(grade("ssim.gauss.y", 0.99), Some(Grade::Good));
        assert_eq!(grade("ssim.fast.y_b", 0.85), Some(Grade::Poor));
        assert_eq!(grade("butteraugli.score", 1.5), Some(Grade::Fair));
        assert_eq!(grade("ssimulacra2.score", 90.0), Some(Grade::Good));
        // No scale for content-dependent values or MSE.
        assert_eq!(grade("sharpness.laplacian.var", 100.0), None);
        assert_eq!(grade("psnr.mse_y", 3.0), None);
        assert_eq!(grade("nodot", 3.0), None);
    }

    #[test]
    fn frame_numbers_and_times() {
        assert_eq!(parse_frame("120", 25.0), Ok(Some(120)));
        assert_eq!(parse_frame(" 4.5s ", 30.0), Ok(Some(135)));
        assert_eq!(parse_frame("1:02", 25.0), Ok(Some(1550)));
        assert_eq!(parse_frame("", 25.0), Ok(None));
        assert_eq!(parse_frame("End", 25.0), Ok(None));
        assert!(parse_frame("abc", 25.0).is_err());
        assert!(parse_frame("-3", 25.0).is_err());
        assert!(parse_frame("-1s", 25.0).is_err());
    }

    #[test]
    fn ranges_are_checked() {
        assert_eq!(parse_range("", "", "", 25.0, Some(100)), Ok(RangeSel { start: 0, end: None, stride: 1 }));
        assert_eq!(parse_range("10", "50", "4", 25.0, Some(100)), Ok(RangeSel { start: 10, end: Some(50), stride: 4 }));
        // An end past the file means "to the end".
        assert_eq!(parse_range("10", "500", "1", 25.0, Some(100)).unwrap().end, None);
        assert_eq!(parse_range("10", "500", "1", 25.0, None).unwrap().end, Some(500));
        assert!(parse_range("50", "10", "1", 25.0, None).is_err());
        assert!(parse_range("50", "50", "1", 25.0, None).is_err());
        assert!(parse_range("100", "", "1", 25.0, Some(100)).is_err());
        assert!(parse_range("0", "", "0", 25.0, None).is_err());
        assert_eq!(range_frames(RangeSel { start: 10, end: Some(50), stride: 4 }, 100), 10);
        assert_eq!(range_frames(RangeSel { start: 10, end: None, stride: 3 }, 20), 4);
        assert_eq!(range_frames(RangeSel { start: 30, end: None, stride: 1 }, 20), 0);
    }

    #[test]
    fn cache_keys_round_trip_and_differ() {
        let k = run_key("image", Path::new("a.png"), None, &["psnr".into()], None, None, 0);
        assert_eq!(parse_key(&k.hex()), Some(k));
        assert_eq!(parse_key("zz"), None);
        let k2 = run_key("image", Path::new("a.png"), None, &["psnr".into()], Some(Roi::new(0, 0, 8, 8)), None, 0);
        let k3 = run_key("image", Path::new("a.png"), None, &["psnr".into(), "noise".into()], None, None, 0);
        assert!(k != k2 && k != k3 && k2 != k3);
    }

    #[test]
    fn video_summary_rows() {
        use lumen_analysis::metrics::Value;
        let mut b = lumen_analysis::report::ReportBuilder::new(Default::default());
        for (i, y) in [42.0, 31.0, 38.0, 100.0].into_iter().enumerate() {
            let dup = (i == 3) as u8 as f64;
            b.add_row(
                i as u64,
                &[
                    Value::new("psnr.y", y, Unit::Db, Direction::HigherIsBetter),
                    Value::new("psnr.mse_avg", 1.0, Unit::CodeSq, Direction::LowerIsBetter),
                    Value::new("temporal.scene.score", 3.0 * i as f64, Unit::Score, Direction::Neutral),
                    Value::new("temporal.scene.cut", (i == 2) as u8 as f64, Unit::Count, Direction::Neutral),
                    Value::new("temporal.freeze.duplicate", dup, Unit::Count, Direction::LowerIsBetter),
                    Value::new("temporal.freeze.run", dup, Unit::Count, Direction::LowerIsBetter),
                ],
            );
        }
        let r = b.finish(Some(255.0));
        let rows = rows_of_report(&r);
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["psnr", "temporal.freeze", "temporal.scene"]);
        // The headline is the PSNR of the mean MSE, not a mean pulled up by
        // an identical (100 dB) frame.
        assert_eq!(rows[0].cells[0].key, "psnr.global");
        let y = rows[0].cells.iter().find(|c| c.key == "psnr.y").unwrap();
        assert_eq!(y.worst, Some(1));
        assert!((y.value.unwrap() - 52.75).abs() < 1e-9);
        // Temporal rows count instead of averaging flags.
        let dups = &rows[1].cells[0];
        assert_eq!((dups.key.as_str(), dups.value), ("temporal.freeze.duplicates", Some(1.0)));
        assert!(rows[1].cells.iter().any(|c| c.key == "temporal.freeze.longest_freeze" && c.value == Some(2.0)));
        let cuts = &rows[2].cells[0];
        assert_eq!((cuts.key.as_str(), cuts.value, cuts.worst), ("temporal.scene.cuts", Some(1.0), Some(2)));
        assert_eq!(key_label("temporal.freeze", "duplicates_b"), "B: Duplicate frames");
        assert_eq!(key_label("psnr", "mse_avg"), "Average MSE");
        // `_b` is the blue channel outside temporal metrics.
        assert_eq!(key_label("psnr", "mse_b"), "Blue MSE");
        assert_eq!(key_label("psnr", "maxabs_b"), "Blue max difference");
        assert_eq!(key_label("psnr", "mse_y"), "Y MSE");
        assert!(scope_label(&r).starts_with("Image"));
    }

    #[test]
    fn report_bundle_holds_the_ai_cards() {
        let dir = std::env::temp_dir().join(format!("lumen-bundle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut tab = MetricsTab::new();
        tab.report = Some(lumen_analysis::report::ReportBuilder::new(Default::default()).finish(None));
        tab.export_bundle(Some(dir.clone()), vec!["## Quality critique\n\nSharp.".into()]);
        let res = tab.bg.pop().expect("export started").recv_timeout(Duration::from_secs(60)).expect("export finished");
        assert!(res.is_ok(), "{res:?}");
        let bundle = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let md = std::fs::read_to_string(bundle.join("ai_cards.md")).unwrap();
        assert!(md.contains("Quality critique") && md.contains("Sharp."));
        assert!(bundle.join("report.csv").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headline_cells_come_first() {
        use lumen_analysis::metrics::Value;
        // A blue-only shift: R and G identical. The row shows the average.
        let mut b = lumen_analysis::report::ReportBuilder::new(Default::default());
        let mut r = MetricResult { id: "psnr".into(), ..Default::default() };
        for (k, v) in [("r", 100.0), ("g", 100.0), ("b", 18.9), ("avg", 23.6)] {
            r.values.push(Value::new(k, v, Unit::Db, Direction::HigherIsBetter));
        }
        let mut e = MetricResult { id: "exposure".into(), ..Default::default() };
        for (k, v) in [("clip_pct", 12.0), ("clip_lo_pct", 0.0), ("clip_hi_pct", 12.0)] {
            e.values.push(Value::new(k, v, Unit::Percent, Direction::LowerIsBetter));
        }
        b.add_metric(metrics::get("psnr").unwrap().info(), r.clone());
        b.add_metric(metrics::get("exposure").unwrap().info(), e);
        let rows = rows_of_report(&b.finish(None));
        assert_eq!(rows[0].cells[0].key, "psnr.avg");
        assert_eq!(grade(&rows[0].cells[0].key, 23.6), Some(Grade::Poor));
        // Every key is still there, in order after the headline.
        assert_eq!(rows[0].cells.iter().map(|c| short_key("psnr", &c.key)).collect::<Vec<_>>(), ["avg", "r", "g", "b"]);
        assert_eq!(rows[1].cells[0].key, "exposure.clip_pct");
        assert_eq!(grade(&rows[1].cells[0].key, 12.0), Some(Grade::Poor));
        assert_eq!(rows_of_live(&[r])[0].cells[0].key, "psnr.avg");
    }

    #[test]
    fn live_rows_in_registry_order() {
        let mk = |id: &str| MetricResult { id: id.into(), values: vec![lumen_analysis::metrics::Value::new("y", f64::INFINITY, Unit::Db, Direction::HigherIsBetter)], ..Default::default() };
        let rows = rows_of_live(&[mk("noise"), mk("psnr")]);
        assert_eq!(rows[0].id, "psnr");
        assert_eq!(rows[0].cells[0].value, Some(lumen_analysis::PSNR_CAP_DB));
        assert_eq!(fmt_value(41.234, Unit::Db), "41.23 dB");
        assert_eq!(fmt_value(lumen_analysis::PSNR_CAP_DB, Unit::Db), "Identical");
        assert_eq!(path_text(Path::new("C:/a\\b/c.csv")), "C:\\a\\b\\c.csv");
        assert_eq!(fmt_value(0.98765, Unit::Index), "0.9877");
        assert_eq!(fmt_cost(1500.0), "1.5 s");
    }
}
