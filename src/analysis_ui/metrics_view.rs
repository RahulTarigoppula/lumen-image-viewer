//! The Metrics tab of the analysis panel: scope bar, metric picker with
//! presets and cost hints, Run / Cancel with progress, results with quality
//! chips, skipped-metric notes, heatmap controls, per-file history and
//! exports.
//!
//! Hosts describe what is on screen with a [`MetricsCtx`] every frame and
//! call [`MetricsTab::ui`]. Over the image they call
//! [`MetricsTab::paint_overlay`] (heatmap and region of interest); video
//! mode also draws [`MetricsTab::video_plots`] under its timeline and seeks
//! to [`MetricsTab::take_seek`].

use super::adapters;
use super::heatmap_view::HeatmapView;
use super::plots::{self, PlotAction, Plots, Series};
use crate::ui::icons as I;
use crate::ui::roi::{ImageTransform, RoiEditor, RoiOptions};
use crate::ui::theme::Palette;
use egui::{Color32, CursorIcon, Painter, Response, RichText, Sense, Ui, Vec2};
use image::RgbaImage;
use lumen_analysis::cache::{CacheKey, DiskCache, DEFAULT_MAX_BYTES};
use lumen_analysis::metrics::{self, Direction, MetricKind, Unit};
use lumen_analysis::{AnalysisReport, Analyzer, JobEvent, JobHandle, MetricResult, Roi, ScopeKind};
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
        ("exposure", "clip_lo_pct" | "clip_hi_pct") => Threshold::lo(0.5, 2.0),
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
pub struct VideoSideCtx<'a> {
    pub path: &'a Path,
    pub info: &'a MediaInfo,
    pub index: &'a SharedIndex,
    /// Native planes of the frame on screen (when paused and enabled).
    pub native: Option<Arc<YuvFrame>>,
}

#[cfg(feature = "video")]
impl VideoSideCtx<'_> {
    fn fps(&self) -> f64 {
        [self.info.avg_fps, self.info.fps].into_iter().find(|f| *f > 0.0).unwrap_or(25.0)
    }

    fn side(&self) -> adapters::VideoSide {
        adapters::VideoSide { path: self.path.to_path_buf(), index: self.index.lock().as_ref().map(|v| Arc::new(v.clone())), fps: self.fps() }
    }

    fn frames(&self) -> Option<usize> {
        self.index.lock().as_ref().map(|v| v.len())
    }
}

/// Video mode's state: A, optional B shifted by `offset` frames.
#[cfg(feature = "video")]
pub struct VideoSubject<'a> {
    pub a: VideoSideCtx<'a>,
    pub b: Option<VideoSideCtx<'a>>,
    /// B frame = A frame + offset.
    pub offset: i64,
    /// A's frame on screen.
    pub current: Option<usize>,
    /// The A-B loop in A frames (inclusive).
    pub loop_ab: Option<(usize, usize)>,
}

/// What the host shows.
pub enum Subject<'a> {
    None,
    /// The viewer's image: display pixels and the file's bits per channel.
    Image { path: &'a Path, image: Arc<RgbaImage>, source_bits: u8 },
    /// Compare A and B (other panes are not measured).
    Compare { a: &'a Path, image_a: Arc<RgbaImage>, b: &'a Path, image_b: Arc<RgbaImage>, source_bits: u8 },
    #[cfg(feature = "video")]
    Video(VideoSubject<'a>),
}

pub struct MetricsCtx<'a> {
    pub pal: &'a Palette,
    pub subject: Subject<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    None,
    Image,
    Compare,
    Video,
    VideoPair,
}

impl Subject<'_> {
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
            Subject::Image { path, .. } => (Some(path), None),
            Subject::Compare { a, b, .. } => (Some(a), Some(b)),
            #[cfg(feature = "video")]
            Subject::Video(v) => (Some(v.a.path), v.b.as_ref().map(|b| b.path)),
        }
    }

    /// Size of A in pixels (native size for video).
    fn size(&self) -> Option<(u32, u32)> {
        match self {
            Subject::None => None,
            Subject::Image { image, .. } => Some(image.dimensions()),
            Subject::Compare { image_a, .. } => Some(image_a.dimensions()),
            #[cfg(feature = "video")]
            Subject::Video(v) => Some((v.a.info.width, v.a.info.height)),
        }
    }

    fn id(&self) -> String {
        let (a, b) = self.files();
        format!("{:?}|{}|{}", self.kind(), a.map(|p| p.display().to_string()).unwrap_or_default(), b.map(|p| p.display().to_string()).unwrap_or_default())
    }

    fn is_video(&self) -> bool {
        matches!(self.kind(), Kind::Video | Kind::VideoPair)
    }

    fn has_b(&self) -> bool {
        matches!(self.kind(), Kind::Compare | Kind::VideoPair)
    }

    fn fps(&self) -> f64 {
        match self {
            #[cfg(feature = "video")]
            Subject::Video(v) => v.a.fps(),
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
}

// ------------------------------------------------------------------ prefs and history

/// Remembered metric choices per kind of subject, and the last stride.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    image: Option<Vec<String>>,
    compare: Option<Vec<String>>,
    video: Option<Vec<String>>,
    video_pair: Option<Vec<String>>,
    stride: Option<String>,
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

/// Metric set before the user chose one.
fn default_metrics(k: Kind) -> Vec<String> {
    let ids: &[&str] = match k {
        Kind::None => &[],
        Kind::Image => metrics::preset("photo_technical"),
        Kind::Compare => &["psnr", "ssim.gauss", "deltae.2000", "diff.abs"],
        Kind::Video => &["sharpness.laplacian", "noise", "blockiness", "banding", "temporal.freeze", "temporal.scene"],
        Kind::VideoPair => &["psnr", "ssim.fast", "temporal.freeze", "temporal.scene"],
    };
    ids.iter().map(|s| s.to_string()).collect()
}

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
const RANGE_MEMORY_BUDGET: u64 = 1 << 30;

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
        parts.push(format!("ROI {}x{}", r.w, r.h));
    }
    parts.join(", ")
}

fn fmt_time(unix: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// Decimals shown in the results table.
fn ui_decimals(u: Unit) -> usize {
    match u {
        Unit::Index => 4,
        Unit::DeltaE | Unit::Score => 3,
        Unit::Count | Unit::Pixels => 0,
        _ => 2,
    }
}

pub fn fmt_value(v: f64, u: Unit) -> String {
    let d = ui_decimals(u);
    match u.label() {
        "" => format!("{v:.d$}"),
        l => format!("{v:.d$} {l}"),
    }
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
            .map(|e| Row {
                id: e.id.clone(),
                name: e.name.clone(),
                cells: e.values.iter().map(|v| Cell { key: v.key.clone(), value: v.value, unit: v.unit, direction: v.direction, tip: None, worst: None }).collect(),
                preview: false,
                has_map: e.map.as_ref().is_some_and(|m| !m.data.is_empty()),
            })
            .collect();
    }
    let mut rows: Vec<Row> = Vec::new();
    for s in &r.summary {
        let id = plots::metric_of(&s.key).to_string();
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
    rows
}

// ------------------------------------------------------------------ the tab

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spatial {
    Whole,
    Roi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Temporal {
    Frame,
    Range,
}

struct Run {
    handle: JobHandle,
    key: CacheKey,
    history: Option<CacheKey>,
    started: Instant,
    #[cfg(feature = "video")]
    decode: Option<Arc<adapters::DecodeInfo>>,
}

/// The video a range was measured on (for thumbnails and frame exports).
#[cfg(feature = "video")]
#[derive(Clone)]
struct VideoRun {
    a: adapters::VideoSide,
    b: Option<adapters::VideoSide>,
    offset: i64,
}

pub struct MetricsTab {
    analyzer: Option<Analyzer>,
    cache_dir: PathBuf,
    prefs: Prefs,
    subject_id: String,
    kind: Kind,
    file_a: Option<PathBuf>,
    spatial: Spatial,
    temporal: Temporal,
    range_text: [String; 3],
    selected: BTreeSet<String>,
    run: Option<Run>,
    report: Option<AnalysisReport>,
    live: Vec<MetricResult>,
    series: Series,
    plot_span: (u64, u64),
    plots: Plots,
    heat: HeatmapView,
    expanded: HashSet<String>,
    history: Vec<HistoryEntry>,
    show_history: bool,
    status: Option<(String, bool)>,
    roi: RoiEditor,
    seek: Option<u64>,
    bg: Vec<crossbeam_channel::Receiver<Result<String, String>>>,
    #[cfg(feature = "video")]
    video_run: Option<VideoRun>,
    #[cfg(feature = "video")]
    thumbs: Option<adapters::VideoThumbs>,
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
        MetricsTab {
            analyzer: None,
            cache_dir: crate::settings::cache_dir().join("analysis"),
            prefs,
            subject_id: String::new(),
            kind: Kind::None,
            file_a: None,
            spatial: Spatial::Whole,
            temporal: Temporal::Frame,
            range_text: [String::new(), String::new(), stride],
            selected: BTreeSet::new(),
            run: None,
            report: None,
            live: Vec::new(),
            series: Series::default(),
            plot_span: (0, 1),
            plots: Plots::default(),
            heat: HeatmapView::default(),
            expanded: HashSet::new(),
            history: Vec::new(),
            show_history: false,
            status: None,
            roi: RoiEditor::new((1.0, 1.0), RoiOptions::region(Color32::from_rgb(80, 200, 255))),
            seek: None,
            bg: Vec::new(),
            #[cfg(feature = "video")]
            video_run: None,
            #[cfg(feature = "video")]
            thumbs: None,
        }
    }

    fn cache(&self) -> DiskCache {
        DiskCache::new(self.cache_dir.clone(), DEFAULT_MAX_BYTES)
    }

    fn analyzer(&mut self, ctx: &egui::Context) -> &Analyzer {
        self.analyzer.get_or_insert_with(|| {
            let ctx = ctx.clone();
            Analyzer::new(None, Some(Arc::new(move || ctx.request_repaint())))
        })
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// A frame (of A) the user asked to see, once.
    pub fn take_seek(&mut self) -> Option<u64> {
        self.seek.take()
    }

    /// The region scope is selected: drags on the image draw the region.
    pub fn roi_mode(&self) -> bool {
        self.spatial == Spatial::Roi
    }

    /// `R`: switch between the whole picture and the region.
    pub fn toggle_roi(&mut self) {
        self.spatial = if self.spatial == Spatial::Roi { Spatial::Whole } else { Spatial::Roi };
    }

    /// The region in source pixels, when the region scope is selected.
    pub fn roi(&self) -> Option<Roi> {
        (self.spatial == Spatial::Roi).then(|| self.roi.pixel_rect()).flatten().map(|(x, y, w, h)| Roi::new(x, y, w, h))
    }

    /// Whether there are graphs for [`MetricsTab::video_plots`].
    pub fn has_plots(&self) -> bool {
        !self.series.is_empty()
    }

    pub fn heatmap(&self) -> &HeatmapView {
        &self.heat
    }

    /// Follow the host's subject: a new file resets results and loads its history.
    fn sync(&mut self, s: &Subject) {
        let id = s.id();
        if id == self.subject_id {
            return;
        }
        self.subject_id = id;
        if let Some(r) = self.run.take() {
            r.handle.cancel();
        }
        self.kind = s.kind();
        self.file_a = s.files().0.map(Path::to_path_buf);
        self.report = None;
        self.live.clear();
        self.series = Series::default();
        self.heat.clear();
        self.status = None;
        self.expanded.clear();
        if let Some(size) = s.size() {
            let size = (size.0 as f32, size.1 as f32);
            if self.roi.image_size() != size {
                self.roi = RoiEditor::new(size, self.roi.opts);
            }
        }
        if !s.is_video() {
            self.temporal = Temporal::Frame;
        }
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
        self.prefs.save();
    }

    /// Why metric `id` cannot run on this subject and scope, if it cannot.
    fn blocked(&self, kind: MetricKind) -> Option<&'static str> {
        let has_b = matches!(self.kind, Kind::Compare | Kind::VideoPair);
        let video = matches!(self.kind, Kind::Video | Kind::VideoPair);
        match kind {
            MetricKind::FullReference if !has_b => Some("Needs a reference: compare two images or load a B video"),
            MetricKind::Temporal if !video => Some("Needs a video"),
            MetricKind::Temporal if self.temporal != Temporal::Range => Some("Needs a frame range"),
            _ => None,
        }
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
        let ids = self.runnable();
        if ids.is_empty() {
            self.set_status("Choose at least one metric that applies here", true);
            return;
        }
        let roi = self.roi();
        if self.spatial == Spatial::Roi && roi.is_none() {
            self.set_status("Draw a region on the image first (or choose Whole)", true);
            return;
        }
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
                let spec = adapters::image_spec(ids.clone(), roi, Some(&fa), fb.as_deref(), pair.rescaled, *source_bits);
                let key = run_key("image", &fa, fb.as_deref(), &ids, roi, None, 0);
                let handle = self.analyzer(egui_ctx).submit_image(spec, pair.a, pair.b);
                self.start(Run {
                    handle,
                    key,
                    history: hist,
                    started: Instant::now(),
                    #[cfg(feature = "video")]
                    decode: None,
                });
                if pair.rescaled {
                    self.set_status("B is resampled to A's size", false);
                }
            }
            #[cfg(feature = "video")]
            Subject::Video(v) => self.run_video(v, ids, roi, fa, fb, hist, egui_ctx),
        }
    }

    #[cfg(feature = "video")]
    #[allow(clippy::too_many_arguments)]
    fn run_video(&mut self, v: &VideoSubject, ids: Vec<String>, roi: Option<Roi>, fa: PathBuf, fb: Option<PathBuf>, hist: Option<CacheKey>, egui_ctx: &egui::Context) {
        if self.temporal == Temporal::Frame {
            let Some(cur) = v.current else {
                self.set_status("No frame on screen yet", true);
                return;
            };
            let color = |s: &VideoSideCtx| adapters::VideoColor::of_info(s.info);
            let Some(a) = v.a.native.clone().and_then(|f| adapters::NativeFrame::new(f, color(&v.a))) else {
                self.set_status("Pause on a frame first (native planes are not ready)", true);
                return;
            };
            let b = match &v.b {
                Some(sb) => match sb.native.clone().and_then(|f| adapters::NativeFrame::new(f, color(sb))) {
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
            self.start(Run { handle, key, history: hist, started: Instant::now(), decode: None });
            return;
        }
        let total = v.a.frames();
        let r = match parse_range(&self.range_text[0], &self.range_text[1], &self.range_text[2], v.a.fps(), total) {
            Ok(r) => r,
            Err(e) => {
                self.set_status(e, true);
                return;
            }
        };
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
        self.video_run = Some(VideoRun { a: req.a.clone(), b: req.b.clone(), offset: v.offset });
        let job = adapters::submit_video_range(self.analyzer(egui_ctx), req);
        self.plot_span = (r.start as u64, end.saturating_sub(1).max(r.start) as u64);
        self.start(Run { handle: job.handle, key, history: hist, started: Instant::now(), decode: Some(job.info) });
    }

    fn start(&mut self, run: Run) {
        if let Some(old) = self.run.take() {
            old.handle.cancel();
        }
        self.live.clear();
        self.series = Series::default();
        self.report = None;
        self.status = None;
        self.run = Some(run);
    }

    pub fn cancel(&mut self) {
        if let Some(r) = &self.run {
            r.handle.cancel();
        }
    }

    /// Take the job's events; call every frame (the tab's `ui` does).
    pub fn poll(&mut self, ctx: &egui::Context) {
        for rx in std::mem::take(&mut self.bg) {
            match rx.try_recv() {
                Ok(Ok(m)) => self.set_status(m, false),
                Ok(Err(e)) => self.set_status(e, true),
                Err(crossbeam_channel::TryRecvError::Empty) => self.bg.push(rx),
                Err(_) => {}
            }
        }
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
            Ok(mut report) => {
                #[cfg(feature = "video")]
                if let Some(d) = &run.decode {
                    d.fix_scope(&mut report);
                }
                let secs = run.started.elapsed().as_secs_f64();
                self.set_status(format!("Done in {secs:.1} s"), false);
                self.finish(report, Some((run.key, run.history)));
            }
            Err(e) if e.is_empty() => self.set_status("Cancelled", false),
            Err(e) => self.set_status(e, true),
        }
    }

    /// Show a finished report and store it in the history (`store`).
    fn finish(&mut self, report: AnalysisReport, store: Option<(CacheKey, Option<CacheKey>)>) {
        self.live.clear();
        if !report.frames.is_empty() {
            self.series = Series::from_report(&report);
            if let Some(f) = report.scope.frames {
                self.plot_span = (f.start, f.end.saturating_sub(1).max(f.start));
            }
        }
        // Keep the overlay of the same metric, else show the first map.
        let pick = self.heat.id().map(str::to_string).filter(|id| report.map(id).is_some_and(|m| !m.data.is_empty())).or_else(|| {
            ["ssim.gauss", "deltae.2000", "diff.abs", "flip", "butteraugli"]
                .iter()
                .map(|s| s.to_string())
                .chain(report.metrics.iter().map(|m| m.id.clone()))
                .find(|id| report.map(id).is_some_and(|m| !m.data.is_empty()))
        });
        match pick {
            Some(id) => self.heat.set_map(&id, report.map(&id).cloned().expect("picked")),
            None => self.heat.clear(),
        }
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
            let mut stored = report.clone();
            for m in &mut stored.metrics {
                m.map = None;
            }
            let (cache, history) = (self.cache(), self.history.clone());
            std::thread::spawn(move || {
                let _ = cache.put(&key, &stored);
                let _ = cache.put(&hkey, &history);
            });
        }
        self.report = Some(report);
    }

    fn load_history(&mut self, h: &HistoryEntry) {
        match parse_key(&h.key).and_then(|k| self.cache().get::<AnalysisReport>(&k)) {
            Some(r) => {
                self.heat.clear();
                self.series = Series::default();
                self.set_status(format!("From history, {}", fmt_time(h.created_unix)), false);
                self.finish(r, None);
            }
            None => {
                self.history.retain(|x| x.key != h.key);
                self.set_status("That result is no longer in the cache", true);
            }
        }
    }

    // ---------------------------------------------------------------- overlay

    /// Draw the heatmap and the region over an image that `xf` maps to the
    /// screen (A's pixels; in compare mode call it for every pane with that
    /// pane's transform and response). `pixels(x, y)` gives the A and B
    /// pixels for the hover readout. Returns true when this frame's drag on
    /// `resp` belongs to the region editor (the host must not pan).
    pub fn paint_overlay(&mut self, ui: &Ui, painter: &Painter, xf: &ImageTransform, resp: &Response, pixels: &dyn Fn(f32, f32) -> (Option<[u8; 4]>, Option<[u8; 4]>)) -> bool {
        let hover = resp.hover_pos().filter(|_| !self.roi.is_dragging());
        self.heat.paint(ui, painter, xf, hover, pixels);
        if self.spatial != Spatial::Roi {
            return false;
        }
        let min = self
            .runnable()
            .iter()
            .filter_map(|id| metrics::get(id))
            .map(|m| m.info())
            .filter(|i| i.min_size.0 > 8 || i.min_size.1 > 8)
            .max_by_key(|i| i.min_size.0.max(i.min_size.1));
        match min {
            Some(i) => self.roi.set_min_size(Some(i.min_size), format!("{} needs {} px", i.name, i.min_size.0)),
            None => self.roi.set_min_size(None, ""),
        }
        let r = self.roi.show(ui, painter, xf, resp);
        r.dragging || self.roi.is_dragging() || resp.dragged() || resp.drag_started()
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
                self.temporal = Temporal::Range;
                self.range_text[0] = s.to_string();
                self.range_text[1] = e.to_string();
                self.set_status(format!("Range set to frames {s}-{}; press Run to measure it", e.saturating_sub(1)), false);
            }
        }
    }

    // ---------------------------------------------------------------- ui

    pub fn ui(&mut self, ui: &mut Ui, ctx: &MetricsCtx) {
        let p = *ctx.pal;
        self.sync(&ctx.subject);
        self.poll(ui.ctx());
        if self.kind == Kind::None {
            ui.add_space(8.0);
            ui.label(RichText::new("Open an image, compare two images or open a video to measure it.").color(p.muted));
            return;
        }
        self.scope_bar(ui, &p, &ctx.subject);
        ui.add_space(6.0);
        self.picker(ui, &p, &ctx.subject);
        ui.add_space(6.0);
        self.run_row(ui, &p, ctx);
        ui.add_space(6.0);
        self.results(ui, &p);
        if self.heat.map().is_some() || self.report.as_ref().is_some_and(|r| r.metrics.iter().any(|m| m.map.as_ref().is_some_and(|m| !m.data.is_empty()))) {
            ui.add_space(6.0);
            self.heatmap_section(ui, &p);
        }
        #[cfg(feature = "video")]
        if !self.series.is_empty() {
            ui.add_space(6.0);
            self.worst_section(ui, &p, &ctx.subject);
        }
        ui.add_space(6.0);
        self.history_section(ui, &p);
    }

    fn scope_bar(&mut self, ui: &mut Ui, p: &Palette, s: &Subject) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.selectable_value(&mut self.spatial, Spatial::Whole, "Whole").on_hover_text("Measure the whole picture");
            ui.selectable_value(&mut self.spatial, Spatial::Roi, "ROI").on_hover_text("Measure a region: drag on the image (R)");
            if s.is_video() {
                ui.label(RichText::new("|").color(p.faint));
                ui.selectable_value(&mut self.temporal, Temporal::Frame, "Current frame").on_hover_text("The paused frame");
                ui.selectable_value(&mut self.temporal, Temporal::Range, "Range").on_hover_text("A range of frames");
            }
        });
        if self.spatial == Spatial::Roi {
            ui.horizontal(|ui| {
                match self.roi.pixel_rect() {
                    Some((x, y, w, h)) => {
                        let warn = self.roi.too_small();
                        ui.label(RichText::new(format!("{w} x {h} px at {x}, {y}")).color(if warn { p.warn } else { p.muted }));
                        if ui.small_button("Clear").clicked() {
                            self.roi.clear();
                        }
                    }
                    None => {
                        ui.label(RichText::new("Drag on the image to draw a region").color(p.muted));
                    }
                }
            });
        }
        if s.is_video() && self.temporal == Temporal::Range {
            let total = s.frames();
            ui.horizontal(|ui| {
                let field = |ui: &mut Ui, label: &str, text: &mut String, hint: &str| {
                    ui.label(RichText::new(label).color(p.muted));
                    ui.add(egui::TextEdit::singleline(text).desired_width(52.0).hint_text(hint)).on_hover_text("Frame number, seconds (4.5s) or minutes:seconds (1:02)").changed()
                };
                field(ui, "Start", &mut self.range_text[0], "0");
                field(ui, "End", &mut self.range_text[1], "end");
                if field(ui, "Stride", &mut self.range_text[2], "1") {
                    self.save_selection();
                }
            });
            ui.horizontal(|ui| {
                #[cfg(feature = "video")]
                let loop_ab = match s {
                    Subject::Video(v) => v.loop_ab,
                    _ => None,
                };
                #[cfg(not(feature = "video"))]
                let loop_ab: Option<(usize, usize)> = None;
                if ui.add_enabled(loop_ab.is_some(), egui::Button::new("Use A-B loop").small()).on_disabled_hover_text("Set an A-B loop first (Ctrl+L)").clicked() {
                    if let Some((a, b)) = loop_ab {
                        self.range_text[0] = a.to_string();
                        self.range_text[1] = (b + 1).to_string();
                    }
                }
                if ui.small_button("Whole file").clicked() {
                    self.range_text[0].clear();
                    self.range_text[1].clear();
                }
                match parse_range(&self.range_text[0], &self.range_text[1], &self.range_text[2], s.fps(), total) {
                    Ok(r) => {
                        if let Some(n) = total {
                            ui.label(RichText::new(format!("{} frames", range_frames(r, n))).color(p.faint));
                        }
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(p.danger));
                    }
                }
            });
        }
    }

    /// Frames a run will measure (1 unless a video range).
    fn frames_to_measure(&self, s: &Subject) -> usize {
        if !s.is_video() || self.temporal == Temporal::Frame {
            return 1;
        }
        let total = s.frames().unwrap_or(0);
        parse_range(&self.range_text[0], &self.range_text[1], &self.range_text[2], s.fps(), s.frames()).map(|r| range_frames(r, total)).unwrap_or(0)
    }

    fn picker(&mut self, ui: &mut Ui, p: &Palette, s: &Subject) {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("Presets").color(p.muted));
            for (name, id) in [("Quick", "quick"), ("Encode QA", "encode_qa"), ("Photo technical", "photo_technical")] {
                if ui.small_button(name).clicked() {
                    self.selected = metrics::preset(id).iter().map(|s| s.to_string()).collect();
                    changed = true;
                }
            }
            if ui.small_button("None").clicked() {
                self.selected.clear();
                changed = true;
            }
        });
        let (w, h) = s.size().unwrap_or((1920, 1080));
        let (w, h) = self.roi().map(|r| (r.w, r.h)).unwrap_or((w, h));
        let frames = self.frames_to_measure(s).max(1) as f32;
        let threads = std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1) as f32;
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
            let head = RichText::new(format!("{title}{}", if n_sel > 0 { format!("  ({n_sel})") } else { String::new() })).color(if blocked.is_some() { p.faint } else { p.text });
            egui::CollapsingHeader::new(head).id_salt(("metrics-group", title)).default_open(blocked.is_none() && kind != MetricKind::Temporal).show(ui, |ui| {
                if let Some(why) = blocked {
                    ui.label(RichText::new(why).size(11.0).color(p.faint));
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
                        let resp = ui.add_enabled(blocked.is_none(), egui::Checkbox::new(&mut on, name)).on_hover_text(desc);
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
                                Some(ms) => (format!("~{}", fmt_cost(ms * scale)), if heavy { p.warn } else { p.faint }),
                            };
                            ui.label(RichText::new(text).size(11.0).color(color)).on_hover_text(if heavy { "Heavy: runs on a quarter-size preview first" } else { "Estimated time" });
                        });
                    });
                }
            });
        }
        if changed {
            self.save_selection();
        }
    }

    fn run_row(&mut self, ui: &mut Ui, p: &Palette, ctx: &MetricsCtx) {
        ui.horizontal(|ui| {
            if let Some(run) = &self.run {
                if ui.button(format!("{}  Cancel", I::STOP)).clicked() {
                    run.handle.cancel();
                }
                let prog = run.handle.progress();
                let frac = prog.fraction().unwrap_or(0.0);
                let eta = prog.eta().map(fmt_eta).unwrap_or_default();
                let text = if prog.total() > 1 { format!("{} / {}  {eta}", prog.done(), prog.total()) } else { eta };
                ui.add(egui::ProgressBar::new(frac).fill(p.accent).desired_height(14.0).text(RichText::new(text).size(11.0)));
            } else {
                let n = self.runnable().len();
                let run = ui.add_enabled(n > 0, egui::Button::new(RichText::new(format!("{}  Run", I::PLAY)).strong())).on_hover_text("Shift+Q");
                if run.clicked() {
                    self.run(ctx, ui.ctx());
                }
                ui.label(RichText::new(format!("{n} metric{}", if n == 1 { "" } else { "s" })).color(p.faint));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.export_menu(ui);
                });
            }
        });
        if let Some((msg, err)) = &self.status {
            ui.label(RichText::new(msg).size(12.0).color(if *err { p.danger } else { p.muted }));
        }
    }

    fn chip(ui: &mut Ui, p: &Palette, g: Option<Grade>) {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        let c = match g {
            Some(Grade::Good) => p.ok,
            Some(Grade::Fair) => p.warn,
            Some(Grade::Poor) => p.danger,
            None => return,
        };
        ui.painter().circle_filled(rect.center(), 4.5, c);
    }

    fn arrow(ui: &mut Ui, p: &Palette, d: Direction) {
        let (icon, tip) = match d {
            Direction::HigherIsBetter => (I::ARROW_UP, "Higher is better"),
            Direction::LowerIsBetter => (I::ARROW_DOWN, "Lower is better"),
            Direction::Neutral => ("", ""),
        };
        let r = ui.label(RichText::new(icon).size(11.0).color(p.faint));
        if !tip.is_empty() {
            r.on_hover_text(tip);
        }
    }

    fn results(&mut self, ui: &mut Ui, p: &Palette) {
        let rows = match &self.report {
            Some(r) => rows_of_report(r),
            None => rows_of_live(&self.live),
        };
        let notes: Vec<String> = self.report.as_ref().map(|r| r.notes.iter().cloned().chain(r.metrics.iter().flat_map(|m| m.notes.iter().map(move |n| format!("{}: {n}", m.name)))).collect()).unwrap_or_default();
        if rows.is_empty() && notes.is_empty() {
            return;
        }
        let mut toggle: Option<String> = None;
        let mut show_map: Option<String> = None;
        let mut seek: Option<u64> = None;
        egui::Grid::new("metrics-results").num_columns(4).spacing(Vec2::new(6.0, 3.0)).show(ui, |ui| {
            for row in &rows {
                let Some(head) = row.cells.first() else { continue };
                let open = self.expanded.contains(&row.id);
                Self::chip(ui, p, head.value.and_then(|v| grade(&head.key, v)));
                ui.horizontal(|ui| {
                    let caret = if row.cells.len() > 1 { if open { I::CARET_DOWN } else { I::CARET_RIGHT } } else { " " };
                    let name = ui.add(egui::Label::new(RichText::new(format!("{caret} {}", row.name)).color(p.text)).sense(Sense::click()));
                    if name.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        toggle = Some(row.id.clone());
                    }
                    if row.preview {
                        ui.label(RichText::new("preview").size(10.0).color(p.warn));
                    }
                    if row.has_map {
                        let on = self.heat.id() == Some(row.id.as_str());
                        if ui.add(egui::Button::new(RichText::new(I::SELECTION).size(12.0)).selected(on).small()).on_hover_text("Show its heatmap").clicked() {
                            show_map = Some(row.id.clone());
                        }
                    }
                });
                let v = ui.label(RichText::new(head.value.map(|v| fmt_value(v, head.unit)).unwrap_or_else(|| "-".into())).color(p.text).strong());
                if let Some(t) = &head.tip {
                    if v.on_hover_text(t).clicked() {
                        seek = head.worst;
                    }
                }
                Self::arrow(ui, p, head.direction);
                ui.end_row();
                if open {
                    for c in &row.cells[1..] {
                        Self::chip(ui, p, c.value.and_then(|v| grade(&c.key, v)));
                        let short = c.key.strip_prefix(&row.id).map(|k| k.trim_start_matches('.')).unwrap_or(&c.key);
                        let lab = ui.add(egui::Label::new(RichText::new(format!("    {short}")).color(p.muted).size(12.0)).sense(Sense::click()));
                        if c.worst.is_some() && lab.on_hover_text("Click to show the worst frame").clicked() {
                            seek = c.worst;
                        }
                        let v = ui.label(RichText::new(c.value.map(|v| fmt_value(v, c.unit)).unwrap_or_else(|| "-".into())).color(p.muted).size(12.0));
                        if let Some(t) = &c.tip {
                            v.on_hover_text(t);
                        }
                        Self::arrow(ui, p, c.direction);
                        ui.end_row();
                    }
                }
            }
        });
        if let Some(id) = toggle {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
        }
        if let Some(id) = show_map {
            let map = self.report.as_ref().and_then(|r| r.map(&id).cloned()).or_else(|| self.live.iter().find(|m| m.id == id).and_then(|m| m.map.clone()));
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
            for n in notes {
                ui.label(RichText::new(format!("{}  {n}", I::INFO)).size(11.0).color(p.warn));
            }
        }
    }

    fn heatmap_section(&mut self, ui: &mut Ui, p: &Palette) {
        let maps: Vec<(String, String)> = self
            .report
            .as_ref()
            .map(|r| r.metrics.iter().filter(|m| m.map.as_ref().is_some_and(|m| !m.data.is_empty())).map(|m| (m.id.clone(), m.name.clone())).collect())
            .unwrap_or_default();
        egui::CollapsingHeader::new(RichText::new("Heatmap").color(p.text)).id_salt("metrics-heatmap").default_open(true).show(ui, |ui| {
            if !maps.is_empty() {
                let cur = self.heat.id().map(str::to_string).unwrap_or_default();
                let name = maps.iter().find(|m| m.0 == cur).map(|m| m.1.clone()).unwrap_or_else(|| "Choose".into());
                let mut pick = None;
                egui::ComboBox::from_id_salt("heatmap-metric").selected_text(name).show_ui(ui, |ui| {
                    for (id, n) in &maps {
                        if ui.selectable_label(*id == cur, n).clicked() {
                            pick = Some(id.clone());
                        }
                    }
                });
                if let Some(id) = pick {
                    if let Some(m) = self.report.as_ref().and_then(|r| r.map(&id).cloned()) {
                        self.heat.set_map(&id, m);
                    }
                }
            }
            self.heat.controls(ui, p);
        });
    }

    #[cfg(feature = "video")]
    fn worst_section(&mut self, ui: &mut Ui, p: &Palette, s: &Subject) {
        let Some(vr) = self.video_run.clone() else { return };
        let Subject::Video(_) = s else { return };
        egui::CollapsingHeader::new(RichText::new("Worst frames").color(p.text)).id_salt("metrics-worst").default_open(true).show(ui, |ui| {
            let b_path = vr.b.as_ref().map(|b| b.path.as_path());
            if self.thumbs.as_ref().is_none_or(|t| !t.shows(&vr.a.path, b_path, vr.offset)) {
                self.thumbs = Some(adapters::VideoThumbs::new(vr.a.clone(), vr.b.clone(), vr.offset));
            }
            let thumbs = self.thumbs.as_mut().map(|t| t as &mut dyn plots::FrameThumbs);
            if let Some(a) = self.plots.worst_ui(ui, p, &self.series, thumbs) {
                self.plot_action(a);
            }
            if ui.button(format!("{}  Export worst frames as PNG pairs", I::DOWNLOAD_SIMPLE)).clicked() {
                self.export_worst(None);
            }
        });
    }

    fn history_section(&mut self, ui: &mut Ui, p: &Palette) {
        if self.history.is_empty() {
            return;
        }
        let mut load = None;
        egui::CollapsingHeader::new(RichText::new(format!("{}  History ({})", I::CLOCK_COUNTER_CLOCKWISE, self.history.len())).color(p.text)).id_salt("metrics-history").default_open(self.show_history).show(ui, |ui| {
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
            ui.menu_button(format!("{}  Export", I::EXPORT), |ui| {
                if ui.button("CSV").clicked() {
                    ui.close();
                    self.export_report(true);
                }
                if ui.button("JSON").clicked() {
                    ui.close();
                    self.export_report(false);
                }
                if ui.add_enabled(has_map, egui::Button::new("Heatmap PNG")).clicked() {
                    ui.close();
                    self.export_heatmap(true);
                }
                if ui.add_enabled(has_map, egui::Button::new("Heatmap TIFF (values)")).clicked() {
                    ui.close();
                    self.export_heatmap(false);
                }
                if has_frames && ui.button("Worst frames as PNG pairs").clicked() {
                    ui.close();
                    self.export_worst(None);
                }
                ui.separator();
                if ui.button("Report bundle folder").clicked() {
                    ui.close();
                    self.export_bundle();
                }
            });
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

    fn export_report(&mut self, csv: bool) {
        let Some(r) = &self.report else { return };
        let (ext, name) = if csv { ("csv", "CSV") } else { ("json", "JSON") };
        let Some(path) = self.dialog().add_filter(name, &[ext]).set_file_name(format!("{}_metrics.{ext}", self.stem())).save_file() else { return };
        let res = if csv { r.write_csv(&path) } else { r.write_json(&path) };
        match res {
            Ok(()) => self.set_status(format!("Saved {}", path.display()), false),
            Err(e) => self.set_status(format!("Could not save: {e}"), true),
        }
    }

    fn export_heatmap(&mut self, png: bool) {
        let Some(id) = self.heat.id().map(str::to_string) else { return };
        let (ext, name) = if png { ("png", "PNG") } else { ("tif", "TIFF") };
        let file = format!("{}_{}.{ext}", self.stem(), id.replace('.', "_"));
        let Some(path) = self.dialog().add_filter(name, &[ext]).set_file_name(file).save_file() else { return };
        let res = if png { self.heat.export_png(&path) } else { self.heat.map().map(|m| m.write_tiff(&path)).unwrap_or(Ok(())) };
        match res {
            Ok(()) => self.set_status(format!("Saved {}", path.display()), false),
            Err(e) => self.set_status(format!("Could not save: {e}"), true),
        }
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
                Ok(files) => Ok(format!("Saved {} frames to {}", files.len(), dir.display())),
                Err(e) => Err(format!("Could not export frames: {e:#}")),
            });
        }
    }

    /// A folder with the report as JSON and CSV, every heatmap as PNG and
    /// TIFF, and the worst frames of a video range as PNG pairs.
    fn export_bundle(&mut self) {
        let Some(report) = self.report.clone() else { return };
        let Some(parent) = self.dialog().pick_folder() else { return };
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let dir = parent.join(format!("{}_analysis_{stamp}", self.stem()));
        let opts = self.heat.options();
        let opacity = self.heat.opacity;
        #[cfg(feature = "video")]
        let frames = self.video_run.clone().filter(|_| !self.series.is_empty()).map(|vr| (vr, self.plots.worst_frames(&self.series)));
        self.background(move || {
            let res = (|| -> anyhow::Result<usize> {
                std::fs::create_dir_all(&dir)?;
                report.write_json(&dir.join("report.json"))?;
                report.write_csv(&dir.join("report.csv"))?;
                let mut n = 2;
                for m in &report.metrics {
                    let Some(map) = m.map.as_ref().filter(|m| !m.data.is_empty()) else { continue };
                    let base = m.id.replace('.', "_");
                    let o = if m.map.as_ref().is_some_and(|x| x.colormap == opts.colormap) { opts } else { map.default_options() };
                    let o = lumen_analysis::ColorizeOptions { alpha: (opacity.clamp(0.1, 1.0) * 255.0) as u8, threshold: None, ..o };
                    map.write_png(&dir.join(format!("heatmap_{base}.png")), &o)?;
                    map.write_tiff(&dir.join(format!("heatmap_{base}.tif")))?;
                    n += 2;
                }
                #[cfg(feature = "video")]
                if let Some((vr, frames)) = frames {
                    n += adapters::export_frame_pairs(&vr.a, vr.b.as_ref(), vr.offset, &frames, &dir.join("worst_frames"))?.len();
                }
                Ok(n)
            })();
            match res {
                Ok(n) => Ok(format!("Saved {n} files to {}", dir.display())),
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
        for (i, y) in [42.0, 31.0, 38.0].into_iter().enumerate() {
            b.add_row(i as u64, &[Value::new("psnr.y", y, Unit::Db, Direction::HigherIsBetter), Value::new("psnr.mse_avg", 1.0, Unit::CodeSq, Direction::LowerIsBetter), Value::new("temporal.scene.score", 3.0, Unit::Score, Direction::Neutral)]);
        }
        let r = b.finish(Some(255.0));
        let rows = rows_of_report(&r);
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["psnr", "temporal.scene"]);
        let y = &rows[0].cells[0];
        assert_eq!(y.key, "psnr.y");
        assert_eq!(y.worst, Some(1));
        assert!((y.value.unwrap() - 37.0).abs() < 1e-9);
        assert!(rows[0].cells.iter().any(|c| c.key == "psnr.global"));
        assert!(scope_label(&r).starts_with("Image"));
    }

    #[test]
    fn live_rows_in_registry_order() {
        let mk = |id: &str| MetricResult { id: id.into(), values: vec![lumen_analysis::metrics::Value::new("y", f64::INFINITY, Unit::Db, Direction::HigherIsBetter)], ..Default::default() };
        let rows = rows_of_live(&[mk("noise"), mk("psnr")]);
        assert_eq!(rows[0].id, "psnr");
        assert_eq!(rows[0].cells[0].value, Some(lumen_analysis::PSNR_CAP_DB));
        assert_eq!(fmt_value(41.234, Unit::Db), "41.23 dB");
        assert_eq!(fmt_value(0.98765, Unit::Index), "0.9877");
        assert_eq!(fmt_cost(1500.0), "1.5 s");
    }
}
