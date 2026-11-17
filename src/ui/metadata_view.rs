//! The Metadata and EXIF window: everything stored in one image, or in 2-4
//! images side by side (Compare), with the values that differ highlighted.
//!
//! Three tabs:
//! - Capture: what the camera did and which features fired (HDR, night mode,
//!   multi-frame merges, portrait depth, zoom, lens), interpreted in plain
//!   words by `lumen_core::inspect`.
//! - EXIF: every EXIF tag by group.
//! - Metadata: file and encoding details (JPEG quality estimate, chroma
//!   subsampling), color profile, XMP, maker trailers, embedded images,
//!   PNG text (AI generator settings), comments.
//!
//! Files are read on background threads; results are kept per path.

use super::icons as I;
use super::theme::{text, Palette};
use super::widgets;
use crossbeam_channel::Receiver;
use egui::{Color32, RichText, Vec2};
use lumen_core::inspect::{Field, Group, Inspection};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaTab {
    Capture,
    Exif,
    Other,
}

impl MetaTab {
    fn label(self) -> &'static str {
        match self {
            MetaTab::Capture => "Capture",
            MetaTab::Exif => "EXIF",
            MetaTab::Other => "Metadata",
        }
    }
    fn tip(self) -> &'static str {
        match self {
            MetaTab::Capture => "What the camera did and which features fired: exposure, lens, zoom, HDR or night mode, multi-frame merges, portrait depth, motion photo",
            MetaTab::Exif => "Every EXIF tag stored in the file, by group",
            MetaTab::Other => "File and encoding details (JPEG quality estimate, chroma subsampling), color profile, XMP, maker data, embedded images, text chunks and comments",
        }
    }
}

/// One shown file.
struct Item {
    label: String,
    path: PathBuf,
    color: Color32,
}

pub struct MetadataWindow {
    pub open: bool,
    pub tab: MetaTab,
    items: Vec<Item>,
    cache: HashMap<PathBuf, Arc<Inspection>>,
    pending: HashMap<PathBuf, Receiver<Inspection>>,
    filter: String,
    only_diff: bool,
}

impl Default for MetadataWindow {
    fn default() -> Self {
        MetadataWindow { open: false, tab: MetaTab::Capture, items: Vec::new(), cache: HashMap::new(), pending: HashMap::new(), filter: String::new(), only_diff: false }
    }
}

/// A row of the table: the field name, its explanation and one value per file.
struct Row {
    name: String,
    note: String,
    values: Vec<Option<String>>,
}

impl Row {
    fn differs(&self) -> bool {
        let first = &self.values[0];
        self.values.iter().any(|v| v != first)
    }
}

/// A table cell of a fixed width with its label aligned left (`add_sized`
/// centers, which reads badly in a column of names and values).
fn cell(ui: &mut egui::Ui, w: f32, h: f32, label: egui::Label) -> egui::Response {
    ui.allocate_ui_with_layout(Vec2::new(w, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(w);
        ui.add(label)
    })
    .inner
}

/// Rows of one group across files, in the order fields first appear.
fn merge_rows(groups: &[Option<&Vec<Field>>]) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let n = groups.len();
    for (k, g) in groups.iter().enumerate() {
        let Some(fields) = g else { continue };
        // Repeated names in one group (lists) stay separate rows.
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for f in fields.iter() {
            let nth = seen.entry(f.name.as_str()).or_insert(0);
            let mut count = 0;
            let pos = rows.iter().position(|r| {
                if r.name == f.name {
                    count += 1;
                    count > *nth
                } else {
                    false
                }
            });
            *nth += 1;
            match pos {
                Some(i) => {
                    rows[i].values[k] = Some(f.value.clone());
                    if rows[i].note.is_empty() {
                        rows[i].note = f.note.clone();
                    }
                }
                None => {
                    let mut values = vec![None; n];
                    values[k] = Some(f.value.clone());
                    rows.push(Row { name: f.name.clone(), note: f.note.clone(), values });
                }
            }
        }
    }
    rows
}

/// Group names across files in order of first appearance.
fn group_names<'a>(lists: impl Iterator<Item = &'a Vec<Group>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in lists {
        for g in l {
            if !out.contains(&g.name) {
                out.push(g.name.clone());
            }
        }
    }
    out
}

impl MetadataWindow {
    /// Show `files` (label, path, badge color) on `tab`. The same files and
    /// tab again close the window (the toolbar button toggles).
    pub fn show_for(&mut self, files: Vec<(String, PathBuf, Color32)>, tab: MetaTab, ctx: &egui::Context) {
        let same = self.open && self.tab == tab && self.items.len() == files.len() && self.items.iter().zip(&files).all(|(a, b)| a.path == b.1);
        if same {
            self.open = false;
            return;
        }
        self.items = files.into_iter().map(|(label, path, color)| Item { label, path, color }).collect();
        self.tab = tab;
        self.open = true;
        if self.items.len() < 2 {
            self.only_diff = false;
        }
        // Keep the cache small: only what is shown now.
        let keep: Vec<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        self.cache.retain(|k, _| keep.contains(k));
        for it in &self.items {
            if self.cache.contains_key(&it.path) || self.pending.contains_key(&it.path) {
                continue;
            }
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (path, ctx) = (it.path.clone(), ctx.clone());
            std::thread::spawn(move || {
                crate::bg::background_priority();
                let r = std::panic::catch_unwind(|| lumen_core::inspect::inspect_file(&path)).unwrap_or_else(|_| Inspection { warnings: vec!["Could not read the metadata".into()], ..Default::default() });
                let _ = tx.send(r);
                ctx.request_repaint();
            });
            self.pending.insert(it.path.clone(), rx);
        }
    }

    /// The files shown now (to refresh when the viewer moves on).
    pub fn paths(&self) -> Vec<PathBuf> {
        self.items.iter().map(|i| i.path.clone()).collect()
    }

    fn poll(&mut self) {
        let mut done = Vec::new();
        for (p, rx) in &self.pending {
            if let Ok(r) = rx.try_recv() {
                done.push((p.clone(), r));
            }
        }
        for (p, r) in done {
            self.pending.remove(&p);
            self.cache.insert(p, Arc::new(r));
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.open {
            return;
        }
        self.poll();
        let mut open = true;
        let title = if self.items.len() > 1 { format!("Metadata of {} images", self.items.len()) } else { "Metadata".to_string() };
        let w = (420.0 + 220.0 * self.items.len().max(1) as f32).min(1100.0);
        widgets::app_window("metadata-window", title).open(&mut open).default_size(w, 620.0).min_size(380.0, 300.0).show(ctx, |ui| self.body(ui, p));
        if !open {
            self.open = false;
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let mut idx = [MetaTab::Capture, MetaTab::Exif, MetaTab::Other].iter().position(|t| *t == self.tab).unwrap_or(0);
            let labels = [MetaTab::Capture.label(), MetaTab::Exif.label(), MetaTab::Other.label()];
            let tips = [MetaTab::Capture.tip(), MetaTab::Exif.tip(), MetaTab::Other.tip()];
            if widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) {
                self.tab = [MetaTab::Capture, MetaTab::Exif, MetaTab::Other][idx.min(2)];
            }
            ui.add_space(8.0);
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(format!("{}  Find a tag or value", I::MAGNIFYING_GLASS)).desired_width(180.0)).on_hover_text("Show only rows whose name or value contains this text");
            if self.items.len() > 1 {
                widgets::checkbox(ui, &mut self.only_diff, "Only differences").on_hover_text("Hide the rows where every image has the same value");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, I::COPY, "Copy this tab as text (tab separated, pastes into a spreadsheet)", false).clicked() {
                    ui.ctx().copy_text(self.as_text());
                }
            });
        });
        ui.add_space(6.0);
        let n = self.items.len().max(1);
        let name_w = 190.0f32;
        let avail = ui.available_width();
        let col_w = ((avail - name_w - 16.0) / n as f32).max(120.0);
        // Column headers.
        ui.horizontal(|ui| {
            cell(ui, name_w, 22.0, egui::Label::new(RichText::new("").size(text::SMALL)));
            for it in &self.items {
                let name = it.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let head = if it.label.is_empty() { name.clone() } else { format!("{}  {}", it.label, name) };
                cell(ui, col_w, 22.0, egui::Label::new(RichText::new(head).font(text::body_strong()).color(it.color)).truncate()).on_hover_text(it.path.display().to_string());
            }
        });
        ui.separator();
        let loading = self.items.iter().any(|i| !self.cache.contains_key(&i.path));
        let warnings: Vec<String> = self.items.iter().filter_map(|i| self.cache.get(&i.path)).flat_map(|r| r.warnings.clone()).collect();
        let sections = self.sections();
        let filter = self.filter.trim().to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Reading the files...").color(p.muted));
                });
            }
            let mut shown_any = false;
            for (title, rows) in &sections {
                let rows: Vec<&Row> = rows
                    .iter()
                    .filter(|r| !self.only_diff || r.differs())
                    .filter(|r| filter.is_empty() || r.name.to_lowercase().contains(&filter) || r.values.iter().flatten().any(|v| v.to_lowercase().contains(&filter)))
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                shown_any = true;
                egui::CollapsingHeader::new(RichText::new(format!("{title}  ({})", rows.len())).font(text::body_strong()).color(p.text)).id_salt(("meta-group", title)).default_open(true).icon(widgets::collapse_caret).show(ui, |ui| {
                    for (k, r) in rows.iter().enumerate() {
                        let differs = n > 1 && r.differs();
                        let bg = if k % 2 == 1 { p.faint_bg } else { Color32::TRANSPARENT };
                        egui::Frame::new().fill(bg).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let name = RichText::new(&r.name).size(text::SMALL).color(if differs { p.accent_text } else { p.muted });
                                let resp = cell(ui, name_w, 20.0, egui::Label::new(if differs { name.strong() } else { name }).truncate());
                                let tip = match (r.note.is_empty(), differs) {
                                    (true, false) => String::new(),
                                    (true, true) => "The images differ here".to_string(),
                                    (false, false) => r.note.clone(),
                                    (false, true) => format!("{}\nThe images differ here.", r.note),
                                };
                                if !tip.is_empty() {
                                    resp.on_hover_text(tip);
                                }
                                for v in &r.values {
                                    let (t, c) = match v {
                                        Some(v) => (v.clone(), p.text),
                                        None => ("-".to_string(), p.faint),
                                    };
                                    let short: String = if t.chars().count() > 160 { t.chars().take(160).collect::<String>() + "..." } else { t.clone() };
                                    let resp = cell(ui, col_w, 20.0, egui::Label::new(RichText::new(short).size(text::SMALL).color(c)).truncate().selectable(true));
                                    if t.chars().count() > 30 {
                                        resp.on_hover_text(t);
                                    }
                                }
                            });
                        });
                    }
                });
            }
            if !shown_any && !loading {
                ui.add_space(12.0);
                let msg = if !filter.is_empty() || self.only_diff { "Nothing matches the filter." } else { "No metadata of this kind in the file." };
                ui.label(RichText::new(msg).color(p.muted));
            }
            if !warnings.is_empty() {
                ui.add_space(10.0);
                for w in warnings {
                    ui.label(RichText::new(format!("{}  {w}", I::WARNING)).size(text::SMALL).color(p.warn));
                }
            }
        });
    }

    /// (group title, rows) of the current tab.
    fn sections(&self) -> Vec<(String, Vec<Row>)> {
        let results: Vec<Option<&Arc<Inspection>>> = self.items.iter().map(|i| self.cache.get(&i.path)).collect();
        match self.tab {
            MetaTab::Capture => {
                let groups: Vec<Option<&Vec<Field>>> = results.iter().map(|r| r.map(|r| &r.capture)).collect();
                vec![("Capture".to_string(), merge_rows(&groups))]
            }
            MetaTab::Exif | MetaTab::Other => {
                let pick = |r: &Inspection| -> Vec<Group> { if self.tab == MetaTab::Exif { r.exif.clone() } else { r.other.clone() } };
                let lists: Vec<Option<Vec<Group>>> = results.iter().map(|r| r.map(|r| pick(r))).collect();
                let names = group_names(lists.iter().flatten());
                names
                    .into_iter()
                    .map(|name| {
                        let groups: Vec<Option<&Vec<Field>>> = lists.iter().map(|l| l.as_ref().and_then(|l| l.iter().find(|g| g.name == name)).map(|g| &g.fields)).collect();
                        let rows = merge_rows(&groups);
                        (name, rows)
                    })
                    .collect()
            }
        }
    }

    /// The current tab as tab-separated text.
    fn as_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Field");
        for it in &self.items {
            out.push('\t');
            out.push_str(&it.path.display().to_string());
        }
        out.push('\n');
        for (title, rows) in self.sections() {
            for r in rows {
                out.push_str(&format!("{title}: {}", r.name));
                for v in &r.values {
                    out.push('\t');
                    out.push_str(&v.clone().unwrap_or_default().replace(['\t', '\n'], " "));
                }
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, value: &str) -> Field {
        Field { name: name.into(), value: value.into(), note: String::new() }
    }

    #[test]
    fn rows_merge_across_files_and_flag_differences() {
        let a = vec![f("ISO", "100"), f("Flash", "No"), f("Item", "x"), f("Item", "y")];
        let b = vec![f("ISO", "100"), f("Lens", "Wide"), f("Item", "x")];
        let rows = merge_rows(&[Some(&a), Some(&b), None]);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["ISO", "Flash", "Item", "Item", "Lens"]);
        assert!(!rows[0].values[..2].iter().any(|v| v.as_deref() != Some("100")));
        assert!(rows[0].differs(), "the third file has no value");
        assert_eq!(rows[3].values, vec![Some("y".to_string()), None, None]);
        let same = merge_rows(&[Some(&a), Some(&a)]);
        assert!(same.iter().all(|r| !r.differs()));
    }

    #[test]
    fn group_order_is_first_seen() {
        let g = |n: &str| Group { name: n.into(), fields: vec![] };
        let a = vec![g("File"), g("XMP")];
        let b = vec![g("File"), g("Encoding"), g("XMP")];
        assert_eq!(group_names([&a, &b].into_iter()), vec!["File", "XMP", "Encoding"]);
    }
}
