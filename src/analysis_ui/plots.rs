//! Video metric graphs: one plot per metric family (PSNR, SSIM, freeze, ...)
//! over frame numbers, drawn with the painter (egui_plot 0.37 targets egui
//! 0.36). Click to seek, drag to select a new frame range, a horizontal
//! threshold line per family, and a worst-frames list with thumbnails.

use super::metrics_view::{key_label, threshold, unit_text};
use crate::ui::theme::Palette;
use egui::{Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use lumen_analysis::metrics::{Direction, Unit};
use lumen_analysis::{AnalysisReport, FrameResult};
use std::collections::HashMap;

/// One plotted column (`"psnr.y"`).
#[derive(Clone, Debug, PartialEq)]
pub struct Col {
    pub key: String,
    pub unit: Unit,
    pub direction: Direction,
}

/// Per-frame values by column, filled live from job rows or from a report.
#[derive(Clone, Debug, Default)]
pub struct Series {
    pub cols: Vec<Col>,
    /// `data[i]` belongs to `cols[i]`: (frame, value), in frame order.
    pub data: Vec<Vec<(u64, f64)>>,
}

/// Columns of one metric, plotted together.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    /// Metric id (`"psnr"`, `"temporal.freeze"`).
    pub id: String,
    pub name: String,
    /// Indices into `Series::cols` with the family's headline unit.
    pub cols: Vec<usize>,
    pub unit: Unit,
}

/// Metric id of a column key: everything before the last dot.
pub fn metric_of(key: &str) -> &str {
    key.rsplit_once('.').map(|(m, _)| m).unwrap_or(key)
}

/// Display name of a metric id.
pub fn metric_name(id: &str) -> String {
    lumen_analysis::metrics::get(id)
        .map(|m| m.info().name.to_string())
        .or_else(|| lumen_analysis::metrics::temporal::get(id).map(|t| t.name.to_string()))
        .unwrap_or_else(|| id.to_string())
}

/// Plot-safe value: dB capped like the reports, other non-finite values dropped.
fn finite(v: f64, unit: Unit) -> Option<f64> {
    lumen_analysis::report::sanitize(v, unit)
}

impl Series {
    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|d| d.is_empty())
    }

    fn col(&mut self, key: &str, unit: Unit, direction: Direction) -> usize {
        if let Some(i) = self.cols.iter().position(|c| c.key == key) {
            return i;
        }
        self.cols.push(Col { key: key.to_string(), unit, direction });
        self.data.push(Vec::new());
        self.cols.len() - 1
    }

    /// Add one frame of a running job.
    pub fn push(&mut self, row: &FrameResult) {
        for v in &row.values {
            let i = self.col(&v.key, v.unit, v.direction);
            if let Some(x) = finite(v.value, v.unit) {
                self.data[i].push((row.idx, x));
            }
        }
    }

    pub fn from_report(r: &AnalysisReport) -> Series {
        let mut s = Series::default();
        for c in &r.columns {
            s.col(&c.key, c.unit, c.direction);
        }
        for f in &r.frames {
            for (i, v) in f.values.iter().enumerate() {
                if let (Some(v), Some(c)) = (v, r.columns.get(i)) {
                    if let Some(x) = finite(*v, c.unit) {
                        s.data[i].push((f.idx, x));
                    }
                }
            }
        }
        s
    }

    /// Metric families in column order. A family plots the columns that share
    /// its headline (first) column's unit, so PSNR shows Y, U, V and the
    /// average but not the MSE columns.
    pub fn families(&self) -> Vec<Family> {
        let mut out: Vec<Family> = Vec::new();
        for (i, c) in self.cols.iter().enumerate() {
            let id = metric_of(&c.key);
            match out.iter_mut().find(|f| f.id == id) {
                Some(f) => {
                    if c.unit == f.unit {
                        f.cols.push(i);
                    }
                }
                None => out.push(Family { id: id.to_string(), name: metric_name(id), cols: vec![i], unit: c.unit }),
            }
        }
        out
    }

    /// The `n` worst frames of `key` by its direction (lowest first when
    /// higher is better), worst first.
    pub fn worst(&self, key: &str, n: usize) -> Vec<(u64, f64)> {
        let Some(i) = self.cols.iter().position(|c| c.key == key) else { return Vec::new() };
        let mut v = self.data[i].clone();
        match self.cols[i].direction {
            Direction::LowerIsBetter => v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0))),
            _ => v.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0))),
        }
        v.truncate(n);
        v
    }

    /// The `n` worst frames of `key` at least `gap` frames apart, worst
    /// first: a bad stretch is listed once, not as its 8 neighbours.
    pub fn worst_spaced(&self, key: &str, n: usize, gap: u64) -> Vec<(u64, f64)> {
        let mut out: Vec<(u64, f64)> = Vec::new();
        for w in self.worst(key, usize::MAX) {
            if out.len() >= n {
                break;
            }
            if out.iter().all(|o| o.0.abs_diff(w.0) >= gap) {
                out.push(w);
            }
        }
        out
    }

    /// The value every frame of `key` has, when they all have the same one.
    pub fn all_equal(&self, key: &str) -> Option<f64> {
        let d = &self.data[self.cols.iter().position(|c| c.key == key)?];
        let (lo, hi) = d.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(p.1), b.max(p.1)));
        (!d.is_empty() && hi - lo <= 1e-9 * hi.abs().max(1.0)).then_some(lo)
    }

    /// Spacing of the worst-frames list: 1/(4 n) of the frames covered, at
    /// least 3 frames.
    pub fn worst_gap(&self, n: usize) -> u64 {
        let (lo, hi) = self.data.iter().flatten().fold((u64::MAX, 0), |(a, b), p| (a.min(p.0), b.max(p.0)));
        (hi.saturating_sub(lo) / (4 * n.max(1) as u64)).max(3)
    }

    /// Columns that make sense for the worst-frames list (each family's
    /// headline columns with a direction).
    pub fn rankable(&self) -> Vec<&str> {
        self.families()
            .iter()
            .flat_map(|f| f.cols.iter().map(|&i| &self.cols[i]))
            .filter(|c| c.direction != Direction::Neutral && !c.key.ends_with("_b"))
            .map(|c| c.key.as_str())
            .collect()
    }

    /// Default ranking column: PSNR-Y, else the first full-reference
    /// headline, else the first rankable column.
    pub fn default_rank_key(&self) -> Option<String> {
        let keys = self.rankable();
        ["psnr.y", "psnr.avg", "ssim.fast.y", "ssim.gauss.y"]
            .iter()
            .find(|k| keys.contains(k))
            .map(|k| k.to_string())
            .or_else(|| keys.first().map(|k| k.to_string()))
    }
}

/// What the user asked for in a plot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlotAction {
    /// Show frame (of A).
    Seek(u64),
    /// Measure frames `start..end` next.
    Range(u64, u64),
}

/// Frame thumbnails for the worst-frames list (A, or A and B side by side).
pub trait FrameThumbs {
    /// The thumbnail of frame `idx`, once loaded (loading starts on first ask).
    fn get(&mut self, ctx: &egui::Context, idx: u64) -> Option<(egui::TextureId, Vec2)>;
}

/// Length of a legend's line swatch.
const SWATCH_W: f32 = 12.0;

/// Line color of series `n` of a graph; B's sequence (`_b`) is dimmer.
fn series_color(key: &str, n: usize) -> Color32 {
    let c = SERIES_COLORS[n % SERIES_COLORS.len()];
    if key.ends_with("_b") {
        c.gamma_multiply(0.55)
    } else {
        c
    }
}

const SERIES_COLORS: [Color32; 6] = [
    Color32::from_rgb(86, 156, 240),
    Color32::from_rgb(240, 160, 60),
    Color32::from_rgb(90, 200, 130),
    Color32::from_rgb(220, 100, 200),
    Color32::from_rgb(230, 90, 90),
    Color32::from_rgb(170, 170, 90),
];

/// Plot state kept between frames.
#[derive(Default)]
pub struct Plots {
    /// Drag selection in progress: plot family and start frame.
    drag: Option<(String, f64)>,
    /// Per-family threshold overrides (`None` = no line).
    thresholds: HashMap<String, Option<f64>>,
    /// Column ranked in the worst-frames list.
    pub rank_key: Option<String>,
    pub worst_n: usize,
}

/// Frame at screen x in a plot spanning `span`.
fn frame_at_x(rect: Rect, span: (u64, u64), x: f32) -> f64 {
    let t = ((x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0) as f64;
    span.0 as f64 + t * (span.1.saturating_sub(span.0)) as f64
}

fn x_of_frame(rect: Rect, span: (u64, u64), f: f64) -> f32 {
    let w = (span.1.saturating_sub(span.0)).max(1) as f64;
    rect.min.x + ((f - span.0 as f64) / w) as f32 * rect.width()
}

/// Value range of the plotted columns, padded; flat data gets a small band.
/// The padding never goes past what the unit can reach (PSNR is capped at
/// 100 dB, SSIM at 1, percentages at 0 and 100).
fn y_range(s: &Series, cols: &[usize], unit: Unit, extra: Option<f64>) -> Option<(f64, f64)> {
    let mut it = cols.iter().flat_map(|&i| s.data[i].iter().map(|p| p.1)).chain(extra);
    let first = it.next()?;
    let (lo, hi) = it.fold((first, first), |(a, b), v| (a.min(v), b.max(v)));
    let pad = if hi > lo { (hi - lo) * 0.08 } else { lo.abs().max(1.0) * 0.01 };
    let (min, max) = match unit {
        Unit::Db => (f64::NEG_INFINITY, lumen_analysis::PSNR_CAP_DB),
        Unit::Index => (f64::NEG_INFINITY, 1.0),
        Unit::Percent => (0.0, 100.0),
        Unit::Count | Unit::Pixels | Unit::CodeSq | Unit::Levels8Sq => (0.0, f64::INFINITY),
        _ => (f64::NEG_INFINITY, f64::INFINITY),
    };
    let (a, b) = ((lo - pad).max(min.min(lo)), (hi + pad).min(max.max(hi)));
    Some(if b > a { (a, b) } else { (lo - pad, hi + pad) })
}

/// Min/max of each pixel column, so long series draw as an envelope.
fn decimate(points: &[(u64, f64)], rect: Rect, span: (u64, u64), to_y: &dyn Fn(f64) -> f32) -> Vec<Pos2> {
    if points.len() as f32 <= rect.width() * 1.5 {
        return points.iter().map(|&(f, v)| Pos2::new(x_of_frame(rect, span, f as f64), to_y(v))).collect();
    }
    let mut out = Vec::new();
    let mut col: Option<(i32, f64, f64, f32)> = None;
    for &(f, v) in points {
        let x = x_of_frame(rect, span, f as f64);
        let c = x.floor() as i32;
        match &mut col {
            Some((cc, lo, hi, _)) if *cc == c => {
                *lo = lo.min(v);
                *hi = hi.max(v);
            }
            _ => {
                if let Some((_, lo, hi, cx)) = col {
                    out.push(Pos2::new(cx, to_y(lo)));
                    out.push(Pos2::new(cx, to_y(hi)));
                }
                col = Some((c, v, v, x));
            }
        }
    }
    if let Some((_, lo, hi, cx)) = col {
        out.push(Pos2::new(cx, to_y(lo)));
        out.push(Pos2::new(cx, to_y(hi)));
    }
    out
}

fn fmt_axis(v: f64, unit: Unit) -> String {
    let d = if unit == Unit::Index {
        4
    } else if v.abs() >= 100.0 {
        0
    } else {
        2
    };
    format!("{v:.d$}")
}

impl Plots {
    fn threshold_for(&mut self, fam: &Family, s: &Series) -> Option<f64> {
        let key = s.cols[fam.cols[0]].key.clone();
        *self.thresholds.entry(fam.id.clone()).or_insert_with(|| threshold(&key).map(|t| t.poor))
    }

    /// All family plots, `span` frames wide, with `current` marked. Returns
    /// a seek or a new range when the user clicked or dragged.
    pub fn ui(&mut self, ui: &mut Ui, p: &Palette, s: &Series, span: (u64, u64), current: Option<u64>, plot_height: f32) -> Option<PlotAction> {
        let mut action = None;
        let span = (span.0, span.1.max(span.0 + 1));
        // Compact: two graphs fit the space video mode gives them.
        ui.spacing_mut().item_spacing.y = 2.0;
        let fams = s.families();
        // One value column for every graph, wide enough for the widest label.
        let gutter = fams
            .iter()
            .filter_map(|f| y_range(s, &f.cols, f.unit, None).map(|(lo, hi)| [lo, hi].map(|v| ui.painter().layout_no_wrap(fmt_axis(v, f.unit), FontId::proportional(10.0), p.faint).size().x)))
            .flatten()
            .fold(0.0f32, f32::max)
            .max(18.0)
            + 6.0;
        let n = fams.len();
        for (i, fam) in fams.iter().enumerate() {
            if i > 0 {
                ui.add_space(4.0);
            }
            if let Some(a) = self.family_plot(ui, p, s, fam, span, current, plot_height, gutter, i + 1 == n) {
                action = Some(a);
            }
        }
        action
    }

    /// One family's graph: a header with the series legend and threshold,
    /// values in a left column `gutter` wide, frame numbers under the last
    /// graph (`axis`) with the measured range.
    #[allow(clippy::too_many_arguments)]
    fn family_plot(&mut self, ui: &mut Ui, p: &Palette, s: &Series, fam: &Family, span: (u64, u64), current: Option<u64>, height: f32, gutter: f32, axis: bool) -> Option<PlotAction> {
        let thr = self.threshold_for(fam, s);
        // Header: name, series legend, threshold field, in exactly the
        // width of the graph (nested layouts here would grow the row in a
        // scroll area and push the video transport bar off screen).
        let width = ui.available_width();
        let left = ui.cursor().min.x;
        ui.allocate_ui_with_layout(Vec2::new(width, crate::ui::theme::size::COMPACT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_max_width(width);
            let mut on = thr.is_some();
            let sp = ui.spacing();
            let check_w = sp.icon_width + sp.icon_spacing + ui.painter().layout_no_wrap("Threshold".into(), FontId::proportional(11.0), p.text).size().x;
            let need = check_w + if on { sp.item_spacing.x + crate::ui::theme::size::VALUE_W } else { 0.0 } + 2.0;
            ui.add_space(gutter);
            ui.label(RichText::new(&fam.name).size(12.0).strong().color(p.text));
            ui.add_space(6.0);
            // As many series names as fit, each with its line color; the rest
            // are counted (Exposure has 20).
            let names: Vec<String> = fam
                .cols
                .iter()
                .map(|&i| {
                    let c = &s.cols[i];
                    key_label(&fam.id, c.key.strip_prefix(&fam.id).map(|k| k.trim_start_matches('.')).unwrap_or(&c.key))
                })
                .collect();
            for (n, name) in names.iter().enumerate() {
                let g = ui.painter().layout_no_wrap(name.clone(), FontId::proportional(11.0), p.muted);
                let w = SWATCH_W + 4.0 + g.size().x + 10.0;
                let room = width - (ui.cursor().min.x - left) - need - 40.0;
                if w > room && n + 1 < names.len() {
                    ui.label(RichText::new(format!("+{}", names.len() - n)).size(11.0).color(p.muted)).on_hover_text(names[n..].join(", "));
                    break;
                }
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::hover());
                let y = r.center().y;
                ui.painter().line_segment([Pos2::new(r.min.x, y), Pos2::new(r.min.x + SWATCH_W, y)], Stroke::new(2.0, series_color(&s.cols[fam.cols[n]].key, n)));
                ui.painter().galley(Pos2::new(r.min.x + SWATCH_W + 4.0, y - g.size().y / 2.0), g, p.muted);
            }
            // Threshold controls at the right edge.
            let mut v = thr.unwrap_or_else(|| s.data[fam.cols[0]].iter().map(|p| p.1).sum::<f64>() / s.data[fam.cols[0]].len().max(1) as f64);
            ui.add_space((width - (ui.cursor().min.x - left) - need).max(0.0));
            let mut changed = crate::ui::widgets::checkbox(ui, &mut on, RichText::new("Threshold").size(crate::ui::theme::text::SMALL)).changed();
            if on {
                changed |= crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut v).speed(0.01).max_decimals(4)).changed();
            }
            if changed {
                self.thresholds.insert(fam.id.clone(), on.then_some(v));
            }
        });
        let (full, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
        // The values sit in the column left of the graph, never over the lines.
        let rect = Rect::from_min_max(Pos2::new(full.min.x + gutter, full.min.y), full.max);
        let painter = ui.painter_at(full.expand(1.0));
        painter.rect_filled(rect, 3.0, p.raised);
        painter.rect_stroke(rect, 3.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let Some((lo, hi)) = y_range(s, &fam.cols, fam.unit, thr) else {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No values", FontId::proportional(11.0), p.faint);
            return None;
        };
        let inner = rect.shrink2(Vec2::new(4.0, 6.0));
        let to_y = |v: f64| inner.max.y - ((v - lo) / (hi - lo).max(1e-12)) as f32 * inner.height();
        // Grid: a middle line; the top and bottom values beside the graph
        // (the middle one too when it is tall enough).
        let mid = (lo + hi) / 2.0;
        painter.line_segment([Pos2::new(inner.min.x, to_y(mid)), Pos2::new(inner.max.x, to_y(mid))], Stroke::new(1.0, p.border));
        let font = FontId::proportional(10.0);
        let lx = rect.min.x - 5.0;
        painter.text(Pos2::new(lx, inner.min.y), egui::Align2::RIGHT_CENTER, fmt_axis(hi, fam.unit), font.clone(), p.faint);
        painter.text(Pos2::new(lx, inner.max.y), egui::Align2::RIGHT_CENTER, fmt_axis(lo, fam.unit), font.clone(), p.faint);
        if inner.height() >= 60.0 {
            painter.text(Pos2::new(lx, to_y(mid)), egui::Align2::RIGHT_CENTER, fmt_axis(mid, fam.unit), font, p.faint);
        }
        // Selection being dragged.
        if let Some((f, start)) = &self.drag {
            if *f == fam.id {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let (a, b) = (x_of_frame(inner, span, *start), pos.x.clamp(inner.min.x, inner.max.x));
                    painter.rect_filled(Rect::from_x_y_ranges(a.min(b)..=a.max(b), rect.y_range()), 0.0, p.accent_soft);
                }
            }
        }
        // Threshold line, with the frames beyond it marked.
        if let Some(t) = thr {
            let y = to_y(t);
            let mut x = inner.min.x;
            while x < inner.max.x {
                painter.line_segment([Pos2::new(x, y), Pos2::new((x + 5.0).min(inner.max.x), y)], Stroke::new(1.0, p.danger));
                x += 9.0;
            }
            let head = &s.cols[fam.cols[0]];
            let bad = |v: f64| if head.direction == Direction::LowerIsBetter { v > t } else { v < t };
            for &(f, _) in s.data[fam.cols[0]].iter().filter(|q| bad(q.1)) {
                let x = x_of_frame(inner, span, f as f64);
                painter.line_segment([Pos2::new(x, rect.max.y - 3.0), Pos2::new(x, rect.max.y - 1.0)], Stroke::new(1.0, p.danger));
            }
        }
        for (n, &i) in fam.cols.iter().enumerate() {
            let color = series_color(&s.cols[i].key, n);
            let pts = decimate(&s.data[i], inner, span, &to_y);
            if pts.len() == 1 {
                painter.circle_filled(pts[0], 2.0, color);
            } else if pts.len() > 1 {
                painter.add(egui::Shape::line(pts, Stroke::new(1.25, color)));
            }
        }
        if let Some(c) = current.filter(|c| (span.0..=span.1).contains(c)) {
            let x = x_of_frame(inner, span, c as f64);
            painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(1.0, p.text));
        }
        // Frame numbers under the last graph: the ends and the middle, and
        // that the graphs cover the measured range, not the whole timeline.
        if axis {
            let (row, _) = ui.allocate_exact_size(Vec2::new(width, 13.0), Sense::hover());
            let ap = ui.painter_at(row);
            let font = FontId::proportional(10.0);
            ap.text(Pos2::new(inner.min.x, row.min.y), egui::Align2::LEFT_TOP, span.0.to_string(), font.clone(), p.faint);
            ap.text(Pos2::new(inner.center().x, row.min.y), egui::Align2::CENTER_TOP, format!("Measured range, frames {}-{}", span.0, span.1), font.clone(), p.faint);
            ap.text(Pos2::new(inner.max.x, row.min.y), egui::Align2::RIGHT_TOP, span.1.to_string(), font, p.faint);
        }
        // Hover readout.
        if let Some(pos) = resp.hover_pos() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            let f = frame_at_x(inner, span, pos.x).round() as u64;
            let x = x_of_frame(inner, span, f as f64);
            painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(1.0, p.muted.gamma_multiply(0.6)));
            let mut lines = vec![format!("Frame {f}")];
            for &i in &fam.cols {
                let d = &s.data[i];
                let k = d.partition_point(|q| q.0 < f);
                let near = [k.checked_sub(1), Some(k)].into_iter().flatten().filter_map(|k| d.get(k)).min_by_key(|q| q.0.abs_diff(f));
                if let Some(&(nf, v)) = near {
                    let at = if nf == f { String::new() } else { format!(" (frame {nf})") };
                    let c = &s.cols[i];
                    let short = c.key.strip_prefix(&fam.id).map(|k| k.trim_start_matches('.')).unwrap_or(&c.key);
                    lines.push(format!("{} {} {}{at}", key_label(&fam.id, short), fmt_axis(v, c.unit), unit_text(c.unit)));
                }
            }
            super::heatmap_view::readout(ui, pos, &lines.join("\n"));
        }
        // Click seeks; a drag selects a new range.
        if resp.drag_started() {
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()) {
                self.drag = Some((fam.id.clone(), frame_at_x(inner, span, pos.x)));
            }
        }
        if resp.drag_stopped() {
            if let (Some((f, start)), Some(pos)) = (self.drag.take(), resp.interact_pointer_pos()) {
                if f == fam.id {
                    let end = frame_at_x(inner, span, pos.x);
                    let (a, b) = (start.min(end).round() as u64, start.max(end).round() as u64);
                    if b > a {
                        return Some(PlotAction::Range(a, b + 1));
                    }
                }
            }
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                return Some(PlotAction::Seek(frame_at_x(inner, span, pos.x).round() as u64));
            }
        }
        None
    }

    /// The worst frames of the ranked column with thumbnails; a click seeks.
    pub fn worst_ui(&mut self, ui: &mut Ui, p: &Palette, s: &Series, thumbs: Option<&mut dyn FrameThumbs>) -> Option<PlotAction> {
        let keys: Vec<String> = s.rankable().iter().map(|k| k.to_string()).collect();
        if keys.is_empty() {
            return None;
        }
        if self.rank_key.as_ref().is_none_or(|k| !keys.contains(k)) {
            self.rank_key = s.default_rank_key();
        }
        if self.worst_n == 0 {
            self.worst_n = 8;
        }
        let mut key = self.rank_key.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Rank by").size(12.0).color(p.muted));
            let label = |k: &str| format!("{}, {}", metric_name(metric_of(k)), key_label(metric_of(k), k.rsplit_once('.').map(|x| x.1).unwrap_or(k)));
            // Fits the panel: long metric names are cut short.
            crate::ui::widgets::combo("worst-key").selected_text(label(&key)).width(ui.available_width().max(80.0)).truncate().show_ui(ui, |ui| {
                for k in &keys {
                    ui.selectable_value(&mut key, k.clone(), label(k));
                }
            });
        });
        self.rank_key = Some(key.clone());
        let mut action = None;
        let mut thumbs = thumbs;
        let unit = s.cols.iter().find(|c| c.key == key).map(|c| c.unit).unwrap_or(Unit::Score);
        // Every frame tied (identical files): no frame is worse than another,
        // and listing (and decoding) arbitrary ones would only mislead.
        if let Some(lo) = s.all_equal(&key) {
            let text = if unit == Unit::Db && lo >= lumen_analysis::PSNR_CAP_DB { "All frames identical".to_string() } else { format!("All frames score the same ({} {})", fmt_axis(lo, unit), unit_text(unit)) };
            ui.label(RichText::new(text).size(12.0).color(p.muted));
            return None;
        }
        for (f, v) in s.worst_spaced(&key, self.worst_n, s.worst_gap(self.worst_n)) {
            let r = ui
                .horizontal(|ui| {
                    if let Some(t) = thumbs.as_deref_mut() {
                        match t.get(ui.ctx(), f) {
                            Some((id, size)) => {
                                let h = 40.0;
                                let w = size.x / size.y.max(1.0) * h;
                                ui.add(egui::Image::new((id, Vec2::new(w, h))));
                            }
                            None => {
                                ui.add_sized(Vec2::new(71.0, 40.0), egui::Spinner::new().size(14.0));
                            }
                        }
                    }
                    ui.label(RichText::new(format!("Frame {f}")).color(p.text));
                    ui.label(RichText::new(format!("{} {}", fmt_axis(v, unit), unit_text(unit))).color(p.muted));
                })
                .response
                .interact(Sense::click());
            if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                action = Some(PlotAction::Seek(f));
            }
        }
        action
    }

    /// Frames listed by [`Plots::worst_ui`].
    pub fn worst_frames(&self, s: &Series) -> Vec<u64> {
        let key = self.rank_key.clone().or_else(|| s.default_rank_key()).unwrap_or_default();
        let n = if self.worst_n == 0 { 8 } else { self.worst_n };
        if s.all_equal(&key).is_some() {
            return Vec::new();
        }
        s.worst_spaced(&key, n, s.worst_gap(n)).into_iter().map(|w| w.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_analysis::Value;

    fn row(idx: u64, vals: &[(&str, f64, Unit, Direction)]) -> FrameResult {
        FrameResult { idx, values: vals.iter().map(|(k, v, u, d)| Value::new(*k, *v, *u, *d)).collect() }
    }

    fn series() -> Series {
        let mut s = Series::default();
        for (i, (y, ss)) in [(42.0, 0.99), (31.5, 0.95), (f64::INFINITY, 1.0), (28.0, 0.90)].into_iter().enumerate() {
            s.push(&row(
                i as u64 * 2,
                &[
                    ("psnr.y", y, Unit::Db, Direction::HigherIsBetter),
                    ("psnr.mse_y", 1.0, Unit::CodeSq, Direction::LowerIsBetter),
                    ("psnr.avg", y, Unit::Db, Direction::HigherIsBetter),
                    ("ssim.fast.y", ss, Unit::Index, Direction::HigherIsBetter),
                    ("temporal.scene.score", i as f64, Unit::Score, Direction::Neutral),
                ],
            ));
        }
        s
    }

    #[test]
    fn families_group_by_metric_and_unit() {
        let s = series();
        let f = s.families();
        assert_eq!(f.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), vec!["psnr", "ssim.fast", "temporal.scene"]);
        // MSE is a different unit and is left out of the PSNR plot.
        assert_eq!(f[0].cols.iter().map(|&i| s.cols[i].key.as_str()).collect::<Vec<_>>(), vec!["psnr.y", "psnr.avg"]);
        assert_eq!(metric_of("temporal.freeze.duplicate_b"), "temporal.freeze");
    }

    #[test]
    fn infinite_db_is_capped() {
        let s = series();
        assert_eq!(s.data[0][2], (4, lumen_analysis::PSNR_CAP_DB));
    }

    #[test]
    fn worst_frames_follow_direction() {
        let s = series();
        assert_eq!(s.worst("psnr.y", 2), vec![(6, 28.0), (2, 31.5)]);
        assert_eq!(s.worst("psnr.mse_y", 1).len(), 1);
        assert!(s.worst("nope", 3).is_empty());
        assert_eq!(s.default_rank_key().as_deref(), Some("psnr.y"));
        // Neutral columns are not ranked.
        assert!(!s.rankable().contains(&"temporal.scene.score"));
    }

    #[test]
    fn worst_frames_are_spread_out() {
        let mut s = Series::default();
        // A bad stretch at 100-110 and a smaller dip at 300.
        for i in 0..400u64 {
            let y = if (100..=110).contains(&i) { 20.0 + (i - 100) as f64 * 0.1 } else if i == 300 { 30.0 } else { 45.0 };
            s.push(&row(i, &[("psnr.y", y, Unit::Db, Direction::HigherIsBetter)]));
        }
        assert_eq!(s.worst_gap(4), 24);
        let w: Vec<u64> = s.worst_spaced("psnr.y", 3, 24).iter().map(|w| w.0).collect();
        assert_eq!(w[0], 100);
        assert!(w.contains(&300), "{w:?}");
        assert!(w.windows(2).all(|p| p[0].abs_diff(p[1]) >= 24));
    }

    #[test]
    fn identical_files_list_no_worst_frames() {
        let mut s = Series::default();
        for i in 0..300u64 {
            s.push(&row(i, &[("psnr.y", f64::INFINITY, Unit::Db, Direction::HigherIsBetter), ("ssim.fast.y", 0.9 + (i % 3) as f64 * 0.01, Unit::Index, Direction::HigherIsBetter)]));
        }
        assert_eq!(s.all_equal("psnr.y"), Some(lumen_analysis::PSNR_CAP_DB));
        assert_eq!(s.all_equal("ssim.fast.y"), None);
        assert_eq!(s.all_equal("nope"), None);
        let p = Plots { rank_key: Some("psnr.y".into()), ..Default::default() };
        assert!(p.worst_frames(&s).is_empty());
    }

    #[test]
    fn report_round_trip() {
        let s = series();
        let mut b = lumen_analysis::report::ReportBuilder::new(Default::default());
        for (i, (f, v)) in s.data[0].iter().enumerate() {
            b.add_row(*f, &[Value::new("psnr.y", *v, Unit::Db, Direction::HigherIsBetter), Value::new("ssim.fast.y", s.data[3][i].1, Unit::Index, Direction::HigherIsBetter)]);
        }
        let r = b.finish(None);
        let back = Series::from_report(&r);
        assert_eq!(back.data[0], s.data[0]);
        assert_eq!(back.families().len(), 2);
    }

    #[test]
    fn plot_geometry() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 0.0), Vec2::new(100.0, 50.0));
        assert_eq!(frame_at_x(rect, (0, 200), 60.0), 100.0);
        assert_eq!(x_of_frame(rect, (100, 300), 200.0), 60.0);
        assert_eq!(frame_at_x(rect, (0, 200), -50.0), 0.0);
        // Long series collapse to two points per pixel column.
        let pts: Vec<(u64, f64)> = (0..10_000).map(|i| (i, (i % 7) as f64)).collect();
        let d = decimate(&pts, rect, (0, 9_999), &|v| v as f32);
        assert!(d.len() <= 202, "{}", d.len());
        let (lo, hi) = y_range(&series(), &[0], Unit::Db, None).unwrap();
        assert!(lo < 28.0 && hi == lumen_analysis::PSNR_CAP_DB);
        let (_, hi) = y_range(&series(), &[3], Unit::Index, None).unwrap();
        assert_eq!(hi, 1.0);
    }
}
