//! `AnalysisReport` (schema `lumen.analysis/1`): self-describing results of
//! one job with its scope, plus CSV and JSON writers. Values never contain
//! infinity: dB values are capped at 100 dB and undefined values are `null`
//! in JSON and empty cells in CSV.

use crate::frame::Roi;
use crate::heatmap::Heatmap;
use crate::metrics::{psnr_db, Direction, MetricInfo, MetricKind, MetricResult, Unit, Value, PSNR_CAP_DB};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const SCHEMA: &str = "lumen.analysis/1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    #[default]
    Image,
    VideoFrame,
    VideoRange,
}

/// Frame range in frame indices, `end` exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRange {
    pub start: u64,
    pub end: u64,
    pub stride: u64,
}

/// What was measured. The caller fills file names, frame range, `rescaled`
/// and `measured_on`; jobs fill kind, size and ROI.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    pub kind: ScopeKind,
    pub file_a: Option<String>,
    pub file_b: Option<String>,
    pub width: u32,
    pub height: u32,
    /// ROI actually used (snapped to the chroma grid for subsampled YUV).
    pub roi: Option<Roi>,
    pub frames: Option<FrameRange>,
    /// B was rescaled to A's size before measuring.
    pub rescaled: bool,
    /// E.g. "8-bit display data" for 16-bit or HDR sources.
    pub measured_on: Option<String>,
}

/// One reported number; `key` is the full column name (`"psnr.y"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportValue {
    pub key: String,
    pub value: Option<f64>,
    pub unit: Unit,
    pub direction: Direction,
}

/// Image result of one metric. The map's pixels are not serialized.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricEntry {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub kind: MetricKind,
    pub values: Vec<ReportValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub elapsed_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<Heatmap>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub key: String,
    pub unit: Unit,
    pub direction: Direction,
}

/// One video frame; `values[i]` belongs to `columns[i]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameRow {
    pub idx: u64,
    pub values: Vec<Option<f64>>,
}

/// Statistics of one column over all frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSummary {
    pub key: String,
    pub unit: Unit,
    pub count: u64,
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Frame with the worst value by the column's direction.
    pub worst_idx: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub schema: String,
    pub tool: String,
    pub created_unix: u64,
    pub scope: Scope,
    /// Image / single-frame results.
    #[serde(default)]
    pub metrics: Vec<MetricEntry>,
    /// Video: per-frame columns, rows and summaries.
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub frames: Vec<FrameRow>,
    #[serde(default)]
    pub summary: Vec<ColumnSummary>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Report-safe value: NaN to `None`, dB capped at 100, other infinities to `None`.
pub fn sanitize(v: f64, unit: Unit) -> Option<f64> {
    if v.is_nan() {
        None
    } else if unit == Unit::Db {
        if v == f64::NEG_INFINITY {
            None
        } else {
            Some(v.min(PSNR_CAP_DB))
        }
    } else if v.is_infinite() {
        None
    } else {
        Some(v)
    }
}

/// Writes `bytes` to a temporary file next to `path`, then renames it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "out".into());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

impl AnalysisReport {
    pub fn new(scope: Scope) -> Self {
        AnalysisReport {
            schema: SCHEMA.into(),
            tool: format!("lumen-analysis {}", env!("CARGO_PKG_VERSION")),
            created_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            scope,
            metrics: Vec::new(),
            columns: Vec::new(),
            frames: Vec::new(),
            summary: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Image value by full key (`"psnr.y"`).
    pub fn value(&self, key: &str) -> Option<f64> {
        self.metrics.iter().flat_map(|m| &m.values).find(|v| v.key == key).and_then(|v| v.value)
    }

    pub fn metric(&self, id: &str) -> Option<&MetricEntry> {
        self.metrics.iter().find(|m| m.id == id)
    }

    /// Heatmap of an image metric.
    pub fn map(&self, id: &str) -> Option<&Heatmap> {
        self.metric(id).and_then(|m| m.map.as_ref())
    }

    pub fn column_index(&self, key: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.key == key)
    }

    /// Video series of one column: (frame, value).
    pub fn series(&self, key: &str) -> Vec<(u64, Option<f64>)> {
        match self.column_index(key) {
            Some(i) => self.frames.iter().map(|r| (r.idx, r.values.get(i).copied().flatten())).collect(),
            None => Vec::new(),
        }
    }

    /// Video summary of one column.
    pub fn summary_of(&self, key: &str) -> Option<&ColumnSummary> {
        self.summary.iter().find(|s| s.key == key)
    }

    /// The `n` worst frames of a column (lowest when higher is better), worst first.
    pub fn worst_frames(&self, key: &str, n: usize) -> Vec<(u64, f64)> {
        let Some(i) = self.column_index(key) else { return Vec::new() };
        let higher_better = self.columns[i].direction == Direction::HigherIsBetter;
        let mut v: Vec<(u64, f64)> = self.frames.iter().filter_map(|r| r.values.get(i).copied().flatten().map(|x| (r.idx, x))).collect();
        v.sort_by(|a, b| if higher_better { a.1.total_cmp(&b.1) } else { b.1.total_cmp(&a.1) });
        v.truncate(n);
        v
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serializes")
    }

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }

    fn header(&self) -> String {
        let s = &self.scope;
        let mut h = format!("# Lumen analysis report ({}, {})\n", self.schema, self.tool);
        let kind = match s.kind {
            ScopeKind::Image => "image",
            ScopeKind::VideoFrame => "video frame",
            ScopeKind::VideoRange => "video range",
        };
        h += &format!("# scope: {kind}, {}x{}", s.width, s.height);
        if let Some(r) = s.roi {
            h += &format!(", roi x={} y={} w={} h={}", r.x, r.y, r.w, r.h);
        }
        if let Some(f) = s.frames {
            h += &format!(", frames {}..{} step {}", f.start, f.end, f.stride.max(1));
        }
        if s.rescaled {
            h += ", B rescaled to A";
        }
        h += "\n";
        if let Some(a) = &s.file_a {
            h += &format!("# A: {}\n", a.replace('\n', " "));
        }
        if let Some(b) = &s.file_b {
            h += &format!("# B: {}\n", b.replace('\n', " "));
        }
        if let Some(m) = &s.measured_on {
            h += &format!("# measured on {m}\n");
        }
        for n in &self.notes {
            h += &format!("# note: {}\n", n.replace('\n', " "));
        }
        h += &format!("# dB values are capped at {PSNR_CAP_DB:.2} dB (identical input reads {PSNR_CAP_DB:.4}); empty cells are not applicable.\n");
        h
    }

    /// CSV: `#` comment header with the scope, then one row per frame (video)
    /// or one row per value (image: `key,value,unit,direction`).
    pub fn to_csv(&self) -> String {
        let fmt = |v: Option<f64>, u: Unit| match v {
            Some(x) => format!("{:.*}", u.decimals(), x),
            None => String::new(),
        };
        let mut s = self.header();
        if !self.frames.is_empty() {
            s += "frame";
            for c in &self.columns {
                s += ",";
                s += &c.key;
            }
            s += "\n";
            for r in &self.frames {
                s += &r.idx.to_string();
                for (i, c) in self.columns.iter().enumerate() {
                    s += ",";
                    s += &fmt(r.values.get(i).copied().flatten(), c.unit);
                }
                s += "\n";
            }
        } else {
            s += "key,value,unit,direction\n";
            for m in &self.metrics {
                for v in &m.values {
                    let dir = match v.direction {
                        Direction::HigherIsBetter => "higher",
                        Direction::LowerIsBetter => "lower",
                        Direction::Neutral => "",
                    };
                    s += &format!("{},{},{},{}\n", v.key, fmt(v.value, v.unit), v.unit.label(), dir);
                }
            }
        }
        s
    }

    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    pub fn write_csv(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, self.to_csv().as_bytes())
    }
}

/// Incremental report construction used by the jobs.
pub struct ReportBuilder {
    report: AnalysisReport,
    index: HashMap<String, usize>,
}

impl ReportBuilder {
    pub fn new(scope: Scope) -> Self {
        ReportBuilder { report: AnalysisReport::new(scope), index: HashMap::new() }
    }

    pub fn note(&mut self, n: String) {
        self.report.notes.push(n);
    }

    pub fn add_metric(&mut self, info: &MetricInfo, r: MetricResult) {
        let values = r
            .values
            .iter()
            .map(|v| ReportValue { key: format!("{}.{}", r.id, v.key), value: sanitize(v.value, v.unit), unit: v.unit, direction: v.direction })
            .collect();
        self.report.metrics.push(MetricEntry {
            id: r.id,
            name: info.name.into(),
            version: r.version,
            kind: info.kind,
            values,
            notes: r.notes,
            elapsed_ms: r.elapsed_ms,
            map: r.map,
        });
    }

    /// Adds one frame; `values` carry full keys. New keys become new columns.
    pub fn add_row(&mut self, idx: u64, values: &[Value]) {
        let mut row = vec![None; self.report.columns.len()];
        for v in values {
            let i = *self.index.entry(v.key.clone()).or_insert_with(|| {
                self.report.columns.push(Column { key: v.key.clone(), unit: v.unit, direction: v.direction });
                self.report.columns.len() - 1
            });
            if row.len() <= i {
                row.resize(i + 1, None);
            }
            row[i] = sanitize(v.value, v.unit);
        }
        self.report.frames.push(FrameRow { idx, values: row });
    }

    /// Pads rows, sorts metric entries by registry order and computes the
    /// video summaries. `peak` (the PSNR peak value) adds `psnr.global`, the
    /// PSNR of the mean MSE, next to the mean of per-frame PSNR.
    pub fn finish(mut self, peak: Option<f64>) -> AnalysisReport {
        let r = &mut self.report;
        let n = r.columns.len();
        for row in &mut r.frames {
            row.values.resize(n, None);
        }
        let order = |id: &str| crate::metrics::all().iter().position(|m| m.id() == id).unwrap_or(usize::MAX);
        r.metrics.sort_by_key(|m| order(&m.id));
        for (i, c) in r.columns.iter().enumerate() {
            let vals: Vec<(u64, f64)> = r.frames.iter().filter_map(|f| f.values[i].map(|v| (f.idx, v))).collect();
            let count = vals.len() as u64;
            let mean = (count > 0).then(|| vals.iter().map(|v| v.1).sum::<f64>() / count as f64);
            let min = vals.iter().map(|v| v.1).reduce(f64::min);
            let max = vals.iter().map(|v| v.1).reduce(f64::max);
            let worst = match c.direction {
                Direction::HigherIsBetter => vals.iter().min_by(|a, b| a.1.total_cmp(&b.1)),
                Direction::LowerIsBetter => vals.iter().max_by(|a, b| a.1.total_cmp(&b.1)),
                Direction::Neutral => None,
            };
            r.summary.push(ColumnSummary { key: c.key.clone(), unit: c.unit, count, mean, min, max, worst_idx: worst.map(|w| w.0) });
        }
        if let (Some(peak), Some(s)) = (peak, r.summary.iter().find(|s| s.key == "psnr.mse_avg")) {
            if let Some(m) = s.mean {
                let count = s.count;
                r.summary.push(ColumnSummary { key: "psnr.global".into(), unit: Unit::Db, count, mean: Some(psnr_db(m, peak)), min: None, max: None, worst_idx: None });
            }
        }
        self.report
    }
}
