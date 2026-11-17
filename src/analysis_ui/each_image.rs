//! Each image on its own: the no-reference checks run on every image of a
//! compare (A, B, C, D), shown as one column per image with a verdict word
//! per check, a plain sentence per image, a one-line comparison ("Sharpest:
//! B. Least noise: A."), and the problem areas found on each image
//! (numbered boxes on its pane; click one in the list to zoom). The single
//! image view uses the same problem-area list and boxes.
//!
//! The wording, grades and the report table are pure functions of the
//! measured values, so they are unit tested.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, CornerRadius, CursorIcon, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use lumen_analysis::{find_regions, AnalysisReport, Heatmap, ImageSpec, JobEvent, JobHandle, Region, RegionLevel, Roi, SharedFrame};

use super::canvas::{self, Pane, Slot};
use super::look;
use super::summary::{self, pct};
use crate::ui::theme::{self, Palette};

// ------------------------------------------------------------------ grades

/// The verdict word of one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Good,
    Ok,
    Check,
    Poor,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Good => "Good",
            Verdict::Ok => "OK",
            Verdict::Check => "Check",
            Verdict::Poor => "Poor",
        }
    }

    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            Verdict::Good => p.ok,
            Verdict::Ok => p.ok.gamma_multiply(0.7),
            Verdict::Check => p.warn,
            Verdict::Poor => p.danger,
        }
    }
}

/// Lower is better: below `good` Good, below `poor` OK then Check (split
/// halfway), else Poor.
fn lower_better(v: f64, good: f64, poor: f64) -> Verdict {
    let mid = (good + poor) / 2.0;
    if v < good {
        Verdict::Good
    } else if v < mid {
        Verdict::Ok
    } else if v < poor {
        Verdict::Check
    } else {
        Verdict::Poor
    }
}

/// One row of the table: a check every image gets.
pub struct Check {
    pub name: &'static str,
    /// Report keys, the first one present is used.
    pub keys: &'static [&'static str],
    /// What it means and which way is better, for the hover note.
    pub explain: &'static str,
    pub higher_better: bool,
}

pub const CHECKS: [Check; 7] = [
    Check {
        name: "Sharpness",
        keys: &["sharpness.laplacian.p90", "sharpness.laplacian.var"],
        explain: "Edge contrast in the sharpest areas (Laplacian variance). Higher is sharper: above 500 edges are crisp, below 100 the picture looks soft. Smooth subjects such as sky also score low.",
        higher_better: true,
    },
    Check {
        name: "Noise",
        keys: &["noise.y"],
        explain: "Estimated grain in 8-bit levels. Lower is better: below 2 it is hard to see, above 5 it shows at 100% zoom.",
        higher_better: false,
    },
    Check {
        name: "Visual noise",
        keys: &["visual_noise.index"],
        explain: "Grain and color speckle in the smooth areas such as sky and walls (CIELAB units, weighted like CPIQ visual noise). Lower is better: below 1 it is hard to see at 100% zoom, above 3 it is plain.",
        higher_better: false,
    },
    Check {
        name: "Clipping",
        keys: &["exposure.clip_pct"],
        explain: "Share of pixels that are pure white (blown highlights) or pure black (crushed shadows), the worse of the two. Lower is better: below 0.5% is fine; a few light sources are normal.",
        higher_better: false,
    },
    Check {
        name: "Color cast",
        keys: &["color_cast.delta_e"],
        explain: "How far the near-gray areas lean towards a color (Delta-E). Lower is better: below 2 is hard to see. Warm light or a graded look can be intentional.",
        higher_better: false,
    },
    Check {
        name: "Blocking",
        keys: &["blockiness.score"],
        explain: "Square 8 × 8 blocks left by JPEG or video compression (0 is none). Lower is better: above 0.3 the blocks are visible.",
        higher_better: false,
    },
    Check {
        name: "Banding",
        keys: &["banding.score"],
        explain: "Steps instead of smooth gradients, as a share of the smooth areas. Lower is better: below 1% gradients are clean, above 5% the steps show.",
        higher_better: false,
    },
];

pub type Values = BTreeMap<String, f64>;

fn get(v: &Values, key: &str) -> Option<f64> {
    v.get(key).copied().filter(|x| x.is_finite())
}

/// The value of check `c` and its verdict (`None`: measured but not
/// gradable, such as banding on a picture without smooth areas).
pub fn cell(c: &Check, v: &Values) -> Option<(f64, Option<Verdict>)> {
    let x = c.keys.iter().find_map(|k| get(v, k))?;
    let verdict = match c.name {
        "Sharpness" => Some(if x >= summary::SHARP_GOOD {
            Verdict::Good
        } else if x >= summary::SHARP_SOFT {
            Verdict::Ok
        } else {
            // Content-dependent: soft is worth a look, never "poor".
            Verdict::Check
        }),
        "Noise" => Some(lower_better(x, summary::NOISE_GOOD, summary::NOISE_POOR)),
        "Visual noise" => (get(v, "visual_noise.flat_pct").unwrap_or(100.0) >= summary::VNOISE_MIN_FLAT).then(|| lower_better(x, summary::VNOISE_GOOD, summary::VNOISE_POOR)),
        "Clipping" => Some(lower_better(x, summary::CLIP_GOOD, summary::CLIP_POOR)),
        "Color cast" => (get(v, "color_cast.neutral_pct").unwrap_or(100.0) >= summary::CAST_MIN_NEUTRAL).then(|| lower_better(x, summary::CAST_GOOD, summary::CAST_POOR)),
        "Blocking" => Some(lower_better(x, summary::BLOCK_GOOD, summary::BLOCK_POOR)),
        "Banding" => (get(v, "banding.smooth_pct").unwrap_or(100.0) >= summary::BAND_MIN_SMOOTH).then(|| lower_better(x, summary::BAND_GOOD, summary::BAND_POOR)),
        _ => None,
    };
    Some((x, verdict))
}

/// The value as the table shows it.
pub fn fmt_cell(c: &Check, x: f64) -> String {
    match c.name {
        "Sharpness" => format!("{x:.0}"),
        "Noise" => format!("{x:.2}"),
        "Clipping" | "Banding" => pct(x),
        "Color cast" => format!("dE {x:.1}"),
        _ => format!("{x:.2}"),
    }
}

/// "Sharp, low noise, highlights clipped in 2.4% of the frame." for one
/// image: sharpness and noise always, the other checks only when they
/// found something.
pub fn sentence(v: &Values) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in &CHECKS {
        let Some((x, Some(verdict))) = cell(c, v) else { continue };
        let part = match (c.name, verdict) {
            ("Sharpness", Verdict::Good) => "sharp".to_string(),
            ("Sharpness", Verdict::Ok) => "moderately sharp".to_string(),
            ("Sharpness", _) => "soft, little fine detail".to_string(),
            ("Noise", Verdict::Good) => "low noise".to_string(),
            ("Noise", Verdict::Ok) => "a little noise".to_string(),
            ("Noise", Verdict::Check) => "noticeable noise".to_string(),
            ("Noise", _) => "very noisy".to_string(),
            ("Visual noise", Verdict::Good) => continue,
            ("Visual noise", Verdict::Ok) => "some grain in flat areas".to_string(),
            ("Visual noise", Verdict::Check) => "noticeable grain in flat areas".to_string(),
            ("Visual noise", _) => "strong grain in flat areas".to_string(),
            ("Clipping", Verdict::Good) => continue,
            ("Clipping", _) => {
                let hi = get(v, "exposure.clip_hi_pct").unwrap_or(x);
                let lo = get(v, "exposure.clip_lo_pct").unwrap_or(0.0);
                match (hi >= summary::CLIP_GOOD, lo >= summary::CLIP_GOOD) {
                    (true, true) => format!("highlights clipped in {} and shadows in {} of the frame", pct(hi), pct(lo)),
                    (false, true) => format!("shadows crushed in {} of the frame", pct(lo)),
                    _ => format!("highlights clipped in {} of the frame", pct(hi)),
                }
            }
            ("Color cast", Verdict::Good) => continue,
            ("Color cast", w) => {
                let hue = get(v, "color_cast.hue").map(lumen_analysis::metrics::noref::cast::describe).unwrap_or("color");
                if w == Verdict::Ok { format!("slight {hue} cast") } else { format!("strong {hue} cast") }
            }
            ("Blocking", Verdict::Good) => continue,
            ("Blocking", Verdict::Ok) => "slight compression blocking".to_string(),
            ("Blocking", Verdict::Check) => "visible compression blocks".to_string(),
            ("Blocking", _) => "heavy compression blocks".to_string(),
            ("Banding", Verdict::Good) => continue,
            ("Banding", Verdict::Ok) => "slight banding in gradients".to_string(),
            ("Banding", _) => "visible banding in gradients".to_string(),
            _ => continue,
        };
        parts.push(part);
    }
    if parts.is_empty() {
        return "Not measured.".into();
    }
    let problems = CHECKS.iter().filter(|c| !matches!(c.name, "Sharpness" | "Noise")).any(|c| cell(c, v).is_some_and(|x| x.1.is_some_and(|w| w != Verdict::Good)));
    if !problems {
        parts.push("no other problems found".into());
    }
    let mut s = parts.join(", ");
    if let Some(f) = s.get_mut(0..1) {
        f.make_ascii_uppercase();
    }
    s.push('.');
    s
}

/// The smallest difference between the best and the others that counts as
/// "better" for check `c` (what a viewer could tell apart).
fn tolerance(c: &Check, best: f64) -> f64 {
    match c.name {
        "Sharpness" => best.abs() * 0.25,
        "Noise" => 0.4_f64.max(best.abs() * 0.2),
        // About one just-noticeable difference at the best value.
        "Visual noise" => lumen_analysis::jnd::step_at("visual_noise.index", best).unwrap_or(0.3),
        "Clipping" => 0.5,
        "Color cast" => 1.0,
        "Blocking" => 0.05,
        "Banding" => 1.0,
        _ => f64::INFINITY,
    }
}

/// "Sharpest: B. Least noise: A." over the images (letter, values); only
/// checks where the best image stands out. Ties name every image within
/// reach of the best ("Least noise: A and C").
pub fn comparison(images: &[(&str, &Values)]) -> String {
    if images.len() < 2 {
        return String::new();
    }
    let mut out = Vec::new();
    for c in &CHECKS {
        let vals: Vec<(&str, f64)> = images.iter().filter_map(|(l, v)| cell(c, v).map(|x| (*l, x.0))).collect();
        if vals.len() < 2 {
            continue;
        }
        let better = |a: f64, b: f64| if c.higher_better { a > b } else { a < b };
        let best = vals.iter().map(|x| x.1).fold(vals[0].1, |m, x| if better(x, m) { x } else { m });
        let worst = vals.iter().map(|x| x.1).fold(vals[0].1, |m, x| if better(m, x) { x } else { m });
        let tol = tolerance(c, best);
        if (best - worst).abs() < tol {
            continue;
        }
        let winners: Vec<&str> = vals.iter().filter(|x| (x.1 - best).abs() < tol / 2.0).map(|x| x.0).collect();
        if winners.len() == vals.len() {
            continue;
        }
        let what = match c.name {
            "Sharpness" => "Sharpest",
            "Noise" => "Least noise",
            "Visual noise" => "Least visual noise",
            "Clipping" => "Least clipping",
            "Color cast" => "Most neutral color",
            "Blocking" => "Least blocking",
            _ => "Least banding",
        };
        let names = match winners.as_slice() {
            [one] => one.to_string(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
            [] => continue,
        };
        out.push(format!("{what}: {names}."));
    }
    if out.is_empty() {
        "The images measure about the same.".into()
    } else {
        out.join(" ")
    }
}

/// The per-image results as a report table: one column per image, one row
/// per check, each cell the value and its verdict word.
pub fn table(images: &[(&str, &str, &Values)]) -> Option<lumen_core::report::Table> {
    if images.is_empty() {
        return None;
    }
    let mut header = vec!["Metric".to_string()];
    header.extend(images.iter().map(|i| i.0.to_string()));
    let mut rows = Vec::new();
    for c in &CHECKS {
        let cells: Vec<String> = images
            .iter()
            .map(|(_, _, v)| match cell(c, v) {
                Some((x, Some(w))) => format!("{} {}", fmt_cell(c, x), w.word()),
                Some((x, None)) => fmt_cell(c, x),
                None => "-".into(),
            })
            .collect();
        if cells.iter().all(|s| s == "-") {
            continue;
        }
        let mut row = vec![c.name.to_string()];
        row.extend(cells);
        rows.push(row);
    }
    if rows.is_empty() {
        return None;
    }
    let names = images.iter().map(|i| format!("{}: {}", i.0, i.1)).collect::<Vec<_>>().join(", ");
    let note = format!("Each image measured on its own ({names}). Higher is better for sharpness; lower is better for noise, clipping, color cast, blocking and banding.");
    Some(lumen_core::report::Table { header, rows, note })
}

// ------------------------------------------------------------------ runs

/// One image's run and results.
pub struct Shot {
    pub letter: &'static str,
    pub path: PathBuf,
    /// Pixels measured (the displayed image).
    pub size: (u32, u32),
    handle: Option<JobHandle>,
    pub values: Values,
    pub regions: Vec<Region>,
    pub failed: Option<String>,
}

impl Shot {
    pub fn name(&self) -> String {
        super::file_name(&self.path)
    }

    pub fn slot(&self) -> Slot {
        Slot::from_letter(self.letter).unwrap_or(Slot::A)
    }
}

/// The values of a report by full key (`"noise.y"`).
pub fn values_of(r: &AnalysisReport) -> Values {
    let mut v = Values::new();
    for m in &r.metrics {
        for x in &m.values {
            if let Some(val) = x.value {
                v.insert(x.key.clone(), val);
            }
        }
    }
    v
}

/// The problem areas of a finished report, from its maps (taken out of it).
pub fn regions_of(report: &mut AnalysisReport, size: (u32, u32)) -> Vec<Region> {
    let maps: Vec<(String, Heatmap)> = report.metrics.iter_mut().filter_map(|m| m.map.take().map(|h| (m.id.clone(), h))).collect();
    let refs: Vec<(&str, &Heatmap)> = maps.iter().map(|(id, m)| (id.as_str(), m)).collect();
    let r = find_regions(&refs, size.0, size.1);
    // Full-size maps of a 24 MP image are large: free them off the UI thread.
    let _ = std::thread::Builder::new().name("lumen-free".into()).spawn(move || drop(maps));
    r
}

/// `roi` (A's pixels, A is `a` pixels) on an image of `size`.
fn scale_roi(roi: Roi, a: (u32, u32), size: (u32, u32)) -> Option<Roi> {
    let (kx, ky) = (size.0 as f64 / a.0.max(1) as f64, size.1 as f64 / a.1.max(1) as f64);
    let r = Roi::new((roi.x as f64 * kx) as u32, (roi.y as f64 * ky) as u32, ((roi.w as f64 * kx).round() as u32).max(1), ((roi.h as f64 * ky).round() as u32).max(1));
    r.clamp(size.0, size.1)
}

/// The no-reference checks on every image of a compare.
#[derive(Default)]
pub struct EachImage {
    pub shots: Vec<Shot>,
    /// The panel's region the results are for (`Some(None)`: the whole
    /// picture); `None` before a run.
    pub measured: Option<Option<Roi>>,
    started: Option<Instant>,
    /// Problem area under the pointer in the list: (image, area).
    pub hovered: Option<(usize, usize)>,
}

impl EachImage {
    pub fn clear(&mut self) {
        self.cancel();
        *self = EachImage::default();
    }

    /// The images measured, in pane order.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.shots.iter().map(|s| s.path.clone()).collect()
    }

    pub fn running(&self) -> bool {
        self.shots.iter().any(|s| s.handle.is_some())
    }

    /// Images finished and in the run.
    pub fn progress(&self) -> (usize, usize) {
        (self.shots.iter().filter(|s| s.handle.is_none()).count(), self.shots.len())
    }

    pub fn started(&self) -> Option<Instant> {
        self.started
    }

    pub fn has_results(&self) -> bool {
        self.shots.iter().any(|s| !s.values.is_empty())
    }

    pub fn cancel(&mut self) {
        for s in &mut self.shots {
            if let Some(h) = s.handle.take() {
                h.cancel();
            }
        }
    }

    /// Jobs for `images` (letter, path, pixels; A first) with metrics `ids`
    /// on the panel's region `roi` (A's pixels), scaled to each image. The
    /// caller submits them and hands the handles to [`EachImage::attach`].
    pub fn jobs(&mut self, images: &[(&'static str, PathBuf, Arc<RgbaImage>)], ids: &[String], roi: Option<Roi>, source_bits: u8) -> Vec<(ImageSpec, SharedFrame, Option<SharedFrame>)> {
        self.cancel();
        self.shots.clear();
        self.hovered = None;
        let a = images.first().map(|i| i.2.dimensions()).unwrap_or((1, 1));
        let mut jobs = Vec::new();
        for (letter, path, img) in images {
            let Some(pair) = super::adapters::image_pair(img, None) else { continue };
            let size = img.dimensions();
            let r = roi.and_then(|r| scale_roi(r, a, size));
            jobs.push((super::adapters::image_spec(ids.to_vec(), r, Some(path), None, false, source_bits), pair.a, None));
            self.shots.push(Shot { letter, path: path.clone(), size, handle: None, values: Values::new(), regions: Vec::new(), failed: None });
        }
        self.measured = Some(roi);
        self.started = Some(Instant::now());
        jobs
    }

    /// The handles of the jobs from [`EachImage::jobs`], in the same order.
    pub fn attach(&mut self, handles: impl Iterator<Item = JobHandle>) {
        for (s, h) in self.shots.iter_mut().zip(handles) {
            s.handle = Some(h);
        }
    }

    /// Takes finished results; true when something changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for s in &mut self.shots {
            let Some(h) = &mut s.handle else { continue };
            let mut done = None;
            while let Some(ev) = h.try_recv() {
                match ev {
                    JobEvent::Done(r) => done = Some(Ok(*r)),
                    JobEvent::Failed(e) => done = Some(Err(e)),
                    JobEvent::Cancelled => done = Some(Err(String::new())),
                    _ => {}
                }
            }
            let Some(done) = done else { continue };
            s.handle = None;
            changed = true;
            match done {
                Ok(mut report) => {
                    s.regions = regions_of(&mut report, s.size);
                    s.values = values_of(&report);
                }
                Err(e) if e.is_empty() => s.failed = Some("Cancelled".into()),
                Err(e) => s.failed = Some(e),
            }
        }
        changed
    }

    /// The comparison line of the finished images.
    pub fn comparison(&self) -> String {
        let v: Vec<(&str, &Values)> = self.shots.iter().filter(|s| !s.values.is_empty()).map(|s| (s.letter, &s.values)).collect();
        comparison(&v)
    }

    /// The results as a report table (see [`table`]).
    pub fn table(&self) -> Option<lumen_core::report::Table> {
        let names: Vec<String> = self.shots.iter().map(|s| s.name()).collect();
        let v: Vec<(&str, &str, &Values)> = self.shots.iter().zip(&names).filter(|(s, _)| !s.values.is_empty()).map(|(s, n)| (s.letter, n.as_str(), &s.values)).collect();
        table(&v)
    }

    /// Dev hook state: one sentence and the area count per image.
    pub fn state(&self) -> String {
        let per: Vec<String> = self.shots.iter().map(|s| format!("{}: {} areas={}", s.letter, sentence(&s.values), s.regions.len())).collect();
        format!("each=[{}] compare=[{}]", per.join("; "), self.comparison())
    }

    /// The card: the comparison line, one sentence per image, the table.
    /// `stale`: the panel's region changed since the run.
    pub fn ui(&mut self, ui: &mut Ui, p: &Palette, stale: bool) {
        if self.shots.is_empty() {
            return;
        }
        let dim = |c: Color32| if stale { p.faint } else { c };
        ui.label(RichText::new("Each image on its own").size(theme::text::CAPTION).font(theme::semibold(theme::text::CAPTION)).color(p.muted))
            .on_hover_text("Every image is checked by itself, without comparing it to another: sharpness, noise, exposure, color, compression and banding.");
        look::card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            let line = self.comparison();
            if !line.is_empty() && self.shots.iter().filter(|s| !s.values.is_empty()).count() > 1 {
                ui.add(egui::Label::new(RichText::new(&line).font(theme::text::subtitle()).color(dim(p.text))).wrap());
                ui.add_space(3.0);
            }
            for (i, s) in self.shots.iter().enumerate() {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    badge(ui, p, i, s.letter, &s.path.display().to_string());
                    let (text, color) = match (&s.handle, &s.failed) {
                        (Some(_), _) => ("Analyzing...".to_string(), p.faint),
                        (None, Some(e)) => (e.clone(), p.danger),
                        (None, None) => (sentence(&s.values), p.text),
                    };
                    ui.add(egui::Label::new(RichText::new(text).size(theme::text::SMALL).color(dim(color))).wrap());
                });
            }
            ui.add_space(4.0);
            let r = ui.available_rect_before_wrap();
            ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border));
            ui.add_space(4.0);
            self.grid(ui, p, stale);
        });
    }

    /// One column per image, one row per check.
    fn grid(&self, ui: &mut Ui, p: &Palette, stale: bool) {
        let w = ui.available_width();
        let name_w = (w * 0.3).clamp(64.0, 96.0);
        let col_w = ((w - name_w) / self.shots.len().max(1) as f32).max(36.0);
        // Header: letter and file name.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_exact_size(Vec2::new(name_w, 18.0), Sense::hover());
            for (i, s) in self.shots.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(col_w, 18.0), Sense::hover());
                let c = Pos2::new(rect.min.x + 7.0, rect.center().y);
                ui.painter().circle_filled(c, 7.0, look::pane_color(p, i));
                ui.painter().text(c, Align2::CENTER_CENTER, s.letter, theme::semibold(9.5), Color32::WHITE);
                let mut job = egui::text::LayoutJob::simple_singleline(s.name(), egui::FontId::proportional(10.5), p.muted);
                job.wrap = egui::text::TextWrapping::truncate_at_width((col_w - 20.0).max(8.0));
                let g = ui.painter().layout_job(job);
                ui.painter().galley(Pos2::new(rect.min.x + 17.0, rect.center().y - g.size().y / 2.0), g, p.muted);
                resp.on_hover_text(format!("{} · {}", s.letter, s.path.display()));
            }
        });
        for c in &CHECKS {
            if self.shots.iter().all(|s| cell(c, &s.values).is_none() && s.handle.is_none()) {
                continue;
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let arrow = if c.higher_better { "higher is better" } else { "lower is better" };
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(name_w, 30.0), Sense::hover());
                ui.painter().text(Pos2::new(rect.min.x, rect.center().y), Align2::LEFT_CENTER, c.name, theme::text::small(), if stale { p.faint } else { p.text });
                resp.on_hover_text(format!("{}: {arrow}.\n{}", c.name, c.explain));
                for s in &self.shots {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(col_w, 30.0), Sense::hover());
                    let x = rect.min.x + 2.0;
                    let (value, word) = match (cell(c, &s.values), &s.handle) {
                        (_, Some(_)) => ("...".to_string(), None),
                        (Some((v, w)), None) => (fmt_cell(c, v), w),
                        (None, None) => ("-".to_string(), None),
                    };
                    ui.painter().text(Pos2::new(x, rect.min.y + 8.0), Align2::LEFT_CENTER, &value, egui::FontId::proportional(11.5), if stale { p.faint } else { p.text });
                    let word_text = word.map(|w| w.word()).unwrap_or(if value == "-" || value == "..." { "" } else { "not graded" });
                    let color = if stale { p.faint } else { word.map(|w| w.color(p)).unwrap_or(p.faint) };
                    ui.painter().text(Pos2::new(x, rect.min.y + 22.0), Align2::LEFT_CENTER, word_text, theme::semibold(10.5), color);
                    let verdict = match word {
                        Some(w) => format!("{}: {}", w.word(), level_meaning(w)),
                        None if value == "-" => "Not measured".to_string(),
                        None => "Not graded: the picture has too little of what this check needs (gray areas, smooth gradients)".to_string(),
                    };
                    resp.on_hover_text(format!("{} · {}\n{}: {value}, {arrow}.\n{verdict}\n\n{}", s.letter, s.name(), c.name, c.explain));
                }
            });
        }
    }
}

fn level_meaning(w: Verdict) -> &'static str {
    match w {
        Verdict::Good => "nothing to fix",
        Verdict::Ok => "acceptable, slightly off",
        Verdict::Check => "worth a look",
        Verdict::Poor => "likely a visible problem",
    }
}

/// The pane letter in its compare color.
fn badge(ui: &mut Ui, p: &Palette, i: usize, letter: &str, tip: &str) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 8.0, look::pane_color(p, i));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, letter, theme::semibold(10.0), Color32::WHITE);
    if !tip.is_empty() {
        resp.on_hover_text(tip);
    }
}

// ------------------------------------------------------------------ problem areas

/// The problem areas of one image for the list and the boxes.
pub struct AreaSet<'a> {
    pub slot: Slot,
    /// Pixels the areas are in (the image measured).
    pub size: (u32, u32),
    pub regions: &'a [Region],
}

pub fn level_color(l: RegionLevel) -> Color32 {
    match l {
        RegionLevel::Moderate => Color32::from_rgb(245, 140, 50),
        RegionLevel::Severe => Color32::from_rgb(235, 70, 70),
    }
}

/// The tag of area `i` of an image: "B2", or "2" for a single image.
fn tag(slot: Slot, i: usize, named: bool) -> String {
    if named {
        format!("{}{}", slot.letter(), i + 1)
    } else {
        (i + 1).to_string()
    }
}

/// The list of problem areas: a numbered row per area (hover highlights its
/// box, click zooms to it). Returns the zoom target, as fractions of its image.
pub fn area_list(ui: &mut Ui, p: &Palette, sets: &[AreaSet], named: bool, stale: bool, hovered: &mut Option<(usize, usize)>, show: &mut bool) -> Option<(Slot, [f32; 4])> {
    let total: usize = sets.iter().map(|s| s.regions.len()).sum();
    let mut zoom = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Problem areas").size(theme::text::CAPTION).font(theme::semibold(theme::text::CAPTION)).color(p.muted))
            .on_hover_text("Only prominent problems are marked: large areas that are blurry, blocky, banded or clipped. Small or mild ones are left out.");
        if total > 0 {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                crate::ui::widgets::checkbox(ui, show, "Boxes").on_hover_text("Show the numbered boxes on the image");
            });
        }
    });
    look::card(ui, p, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if total == 0 {
            look::note(ui, p, if named { "No prominent problem areas in any image." } else { "No prominent problem areas found." });
            return;
        }
        let mut hover = None;
        for (si, set) in sets.iter().enumerate() {
            for (ri, r) in set.regions.iter().enumerate() {
                let col = if stale { p.faint } else { level_color(r.level) };
                let inner = ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let t = tag(set.slot, ri, named);
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(if named { 24.0 } else { 18.0 }, 18.0), Sense::hover());
                    ui.painter().rect_filled(rect, CornerRadius::same(3), col);
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, t, theme::semibold(10.5), Color32::BLACK);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(r.kind.title()).size(theme::text::SMALL).font(theme::semibold(theme::text::SMALL)).color(if stale { p.faint } else { p.text }));
                        ui.add(egui::Label::new(RichText::new(r.description()).size(11.0).color(p.muted)).wrap());
                    });
                });
                let rect = inner.response.rect;
                let resp = ui.interact(rect, ui.scope_id().with(("area", si, ri)), Sense::click());
                if resp.hovered() {
                    hover = Some((si, ri));
                    ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(4), Stroke::new(1.0, p.border), egui::StrokeKind::Outside);
                }
                if resp.on_hover_text("Click to zoom to this area").on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    let (w, h) = (set.size.0.max(1) as f32, set.size.1.max(1) as f32);
                    zoom = Some((set.slot, [r.rect[0] / w, r.rect[1] / h, r.rect[2] / w, r.rect[3] / h]));
                }
            }
        }
        *hovered = hover;
    });
    zoom
}

/// Numbered boxes of the areas on every pane of the matching image.
pub fn paint_areas(ui: &Ui, panes: &[Pane], sets: &[AreaSet], named: bool, hovered: Option<(usize, usize)>) {
    for pane in panes {
        let Some((si, set)) = sets.iter().enumerate().find(|(_, s)| s.slot == pane.slot) else { continue };
        let k = Vec2::new(pane.xf.size.x / set.size.0.max(1) as f32, pane.xf.size.y / set.size.1.max(1) as f32);
        let painters: Vec<egui::Painter> = canvas::uncovered(ui.ctx(), pane.clip).into_iter().map(|r| ui.painter().with_clip_rect(r)).collect();
        for (ri, r) in set.regions.iter().enumerate() {
            let rect = pane.xf.rect_to_screen([r.rect[0] * k.x, r.rect[1] * k.y, r.rect[2] * k.x, r.rect[3] * k.y]);
            let col = level_color(r.level);
            let hot = hovered == Some((si, ri));
            let t = tag(set.slot, ri, named);
            let tag_w = if named { 26.0 } else { 18.0 };
            let tag_min = if rect.min.y - 18.0 >= pane.clip.min.y { Pos2::new(rect.min.x, rect.min.y - 18.0) } else { rect.min };
            let tag_r = Rect::from_min_size(tag_min, Vec2::new(tag_w, 18.0));
            for painter in &painters {
                if hot {
                    painter.rect_filled(rect, 0.0, col.gamma_multiply(0.18));
                }
                // Dashed, so they read as measured areas, not the AI's findings.
                let pts = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
                painter.add(egui::Shape::line(pts.to_vec(), Stroke::new(if hot { 4.5 } else { 3.0 }, Color32::from_black_alpha(110))));
                painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(if hot { 2.5 } else { 1.6 }, col), 8.0, 4.0));
                painter.rect_filled(tag_r, CornerRadius::same(3), col);
                painter.text(tag_r.center(), Align2::CENTER_CENTER, &t, theme::semibold(11.0), Color32::BLACK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, f64)]) -> Values {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn clean() -> Values {
        vals(&[
            ("sharpness.laplacian.p90", 900.0),
            ("noise.y", 1.2),
            ("exposure.clip_pct", 0.1),
            ("exposure.clip_hi_pct", 0.1),
            ("exposure.clip_lo_pct", 0.0),
            ("color_cast.delta_e", 0.8),
            ("color_cast.neutral_pct", 20.0),
            ("blockiness.score", 0.02),
            ("banding.score", 0.2),
            ("banding.smooth_pct", 30.0),
        ])
    }

    #[test]
    fn verdict_words_follow_the_limits() {
        let c = &CHECKS[1];
        assert_eq!(cell(c, &vals(&[("noise.y", 1.0)])).unwrap().1, Some(Verdict::Good));
        assert_eq!(cell(c, &vals(&[("noise.y", 3.0)])).unwrap().1, Some(Verdict::Ok));
        assert_eq!(cell(c, &vals(&[("noise.y", 4.0)])).unwrap().1, Some(Verdict::Check));
        assert_eq!(cell(c, &vals(&[("noise.y", 6.0)])).unwrap().1, Some(Verdict::Poor));
        // Soft is never "poor": smooth subjects score low too.
        assert_eq!(cell(&CHECKS[0], &vals(&[("sharpness.laplacian.p90", 20.0)])).unwrap().1, Some(Verdict::Check));
        // Banding without smooth areas is measured but not graded.
        assert_eq!(cell(&CHECKS[6], &vals(&[("banding.score", 9.0), ("banding.smooth_pct", 0.2)])).unwrap().1, None);
        // Visual noise likewise without flat areas.
        assert_eq!(cell(&CHECKS[2], &vals(&[("visual_noise.index", 2.0), ("visual_noise.flat_pct", 0.0)])).unwrap().1, None);
        assert_eq!(cell(&CHECKS[2], &vals(&[("visual_noise.index", 2.0), ("visual_noise.flat_pct", 30.0)])).unwrap().1, Some(Verdict::Check));
        assert!(cell(c, &Values::new()).is_none());
    }

    #[test]
    fn one_sentence_per_image() {
        assert_eq!(sentence(&clean()), "Sharp, low noise, no other problems found.");
        let mut v = clean();
        v.insert("exposure.clip_pct".into(), 2.4);
        v.insert("exposure.clip_hi_pct".into(), 2.4);
        v.insert("blockiness.score".into(), 0.35);
        v.insert("noise.y".into(), 3.0);
        assert_eq!(sentence(&v), "Sharp, a little noise, highlights clipped in 2.4% of the frame, heavy compression blocks.");
        let soft = vals(&[("sharpness.laplacian.var", 50.0)]);
        assert_eq!(sentence(&soft), "Soft, little fine detail, no other problems found.");
        assert_eq!(sentence(&Values::new()), "Not measured.");
    }

    #[test]
    fn comparison_names_the_best_only_when_it_stands_out() {
        let a = clean();
        let mut b = clean();
        b.insert("sharpness.laplacian.p90".into(), 300.0);
        b.insert("noise.y".into(), 4.0);
        let mut c = clean();
        c.insert("noise.y".into(), 1.3);
        assert_eq!(comparison(&[("A", &a), ("B", &b)]), "Sharpest: A. Least noise: A.");
        // C is as clean as A: both named.
        assert_eq!(comparison(&[("A", &a), ("B", &b), ("C", &c)]), "Sharpest: A and C. Least noise: A and C.");
        assert_eq!(comparison(&[("A", &a), ("B", &clean())]), "The images measure about the same.");
        assert_eq!(comparison(&[("A", &a)]), "");
    }

    #[test]
    fn visual_noise_compares_in_noticeable_steps() {
        let mut a = clean();
        a.insert("visual_noise.index".into(), 0.8);
        a.insert("visual_noise.flat_pct".into(), 30.0);
        let mut b = a.clone();
        b.insert("visual_noise.index".into(), 2.0);
        assert_eq!(comparison(&[("A", &a), ("B", &b)]), "Least visual noise: A.");
        assert_eq!(sentence(&b), "Sharp, low noise, noticeable grain in flat areas.");
        // Under one just-noticeable difference apart: not called out.
        b.insert("visual_noise.index".into(), 0.9);
        assert_eq!(comparison(&[("A", &a), ("B", &b)]), "The images measure about the same.");
    }

    #[test]
    fn report_table_has_one_column_per_image() {
        let a = clean();
        let mut b = clean();
        b.insert("blockiness.score".into(), 0.4);
        let t = table(&[("A", "a.jpg", &a), ("B", "b.jpg", &b)]).unwrap();
        assert_eq!(t.header, ["Metric", "A", "B"]);
        assert_eq!(t.rows[0], ["Sharpness", "900 Good", "900 Good"]);
        let blocking = t.rows.iter().find(|r| r[0] == "Blocking").unwrap();
        assert_eq!(blocking[2], "0.40 Poor");
        assert!(t.note.contains("A: a.jpg, B: b.jpg") && t.note.contains("lower is better"));
        assert!(table(&[]).is_none());
    }

    #[test]
    fn roi_scales_to_each_image() {
        let r = scale_roi(Roi::new(100, 50, 200, 100), (1000, 500), (500, 250)).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (50, 25, 100, 50));
        assert!(scale_roi(Roi::new(990, 0, 100, 10), (1000, 500), (500, 250)).is_some_and(|r| r.x + r.w <= 500));
    }
}
