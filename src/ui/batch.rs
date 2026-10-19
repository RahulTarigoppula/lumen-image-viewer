//! Batch convert/resize and batch rename windows.
//!
//! All heavy lifting happens on background threads (see `convert.rs`); the
//! windows only poll channels and draw progress.

pub mod advanced;
pub mod convert;
pub mod logic;
pub mod pipeline;

use self::convert::{ConflictPolicy, ConvertSettings, DestMode, Estimate, FileStatus};
use self::logic::{CaseChange, ExtCase, NameInfo, RenameIssue, RenameSettings, ResizeMode};
use crate::app::Core;
use crate::catalog::human_size;
use crate::encode::Chroma;
use crate::formats::SaveFormat;
use crate::imaging::Filter;
use crate::ui::icons as I;
use crate::ui::theme::{radius, size, text, Palette};
use crate::ui::widgets;
use crossbeam_channel::Receiver;
use egui::{Color32, CornerRadius, RichText, Ui, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------- UI helpers

/// A titled card used to group settings: 13 pt semibold title with a 14 pt accent icon.
pub(crate) fn card<R>(ui: &mut Ui, p: &Palette, icon: &str, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(if p.dark { p.panel } else { p.bg })
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).color(p.accent_text).size(size::ICON));
                ui.label(RichText::new(title).font(text::subtitle()).color(p.text));
            });
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

/// The window's primary action (see [`widgets::primary_button_enabled`]).
pub(crate) fn primary_button(ui: &mut Ui, p: &Palette, enabled: bool, text: &str) -> egui::Response {
    widgets::primary_button_enabled(ui, p, enabled, text)
}

pub(crate) fn danger_button(ui: &mut Ui, p: &Palette, text: &str) -> egui::Response {
    widgets::danger_button(ui, p, text)
}

/// Small muted note. Ragged right: in the justified layout of `ui.columns` egui
/// would otherwise stretch the word gaps of every wrapped line.
pub(crate) fn hint(ui: &mut Ui, p: &Palette, t: &str) {
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(egui::Label::new(RichText::new(t).size(text::SMALL).color(p.muted)).wrap());
    });
}

/// Centered empty-state message.
pub(crate) fn empty_state(ui: &mut Ui, p: &Palette, icon: &str, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(18.0);
        ui.label(RichText::new(icon).size(34.0).color(p.faint));
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(text::subtitle()).color(p.text));
        ui.label(RichText::new(body).color(p.muted));
        ui.add_space(18.0);
    });
}

/// Tool window contents laid out in exactly the window's current size: whatever does
/// not fit is clipped instead of growing the window past the screen edge.
pub(crate) fn window_body<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let rect = ui.available_rect_before_wrap();
    let mut body = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    body.set_clip_rect(rect.expand(4.0).intersect(ui.clip_rect()));
    let r = add(&mut body);
    ui.advance_cursor_after_rect(rect);
    r
}

/// Width below which tool windows stack their two columns and footer rows.
pub(crate) const NARROW: f32 = 600.0;

/// Two side-by-side columns, or one above the other when the window is narrow.
/// `add` is called for column 0, then column 1.
pub(crate) fn columns_or_stack(ui: &mut Ui, mut add: impl FnMut(&mut Ui, usize)) {
    if ui.available_width() >= NARROW {
        ui.columns(2, |cols| {
            add(&mut cols[0], 0);
            add(&mut cols[1], 1);
        });
    } else {
        add(ui, 0);
        ui.add_space(8.0);
        add(ui, 1);
    }
}

/// Footer: `right` holds the buttons (laid out right to left, so add them in
/// reverse) and `left` a status line in the room that is left (use truncating
/// labels). When narrow, the status goes on its own line and the buttons wrap.
pub(crate) fn footer_row(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    use egui::{Align, Layout};
    if ui.available_width() >= NARROW {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                right(ui);
                ui.with_layout(Layout::left_to_right(Align::Center), left);
            });
        });
    } else {
        ui.horizontal(left);
        ui.with_layout(Layout::right_to_left(Align::Min).with_main_wrap(true), right);
    }
}

pub(crate) fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn format_label(f: SaveFormat) -> &'static str {
    match f {
        SaveFormat::WebP => "WebP",
        other => other.label(),
    }
}

fn image_exts() -> Vec<&'static str> {
    crate::formats::NATIVE_EXTS.iter().chain(crate::formats::WIC_EXTS.iter()).copied().collect()
}

/// Scan a folder for images on a background thread.
fn scan_folder(folder: PathBuf, recursive: bool, ctx: egui::Context) -> Receiver<Vec<PathBuf>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let mut files: Vec<PathBuf> = if recursive {
            crate::catalog::list_recursive(&folder, 200_000).into_iter().map(|e| e.path).collect()
        } else {
            crate::catalog::list_folder(&folder, false)
                .map(|v| v.into_iter().filter(|e| e.kind == crate::catalog::EntryKind::Image).map(|e| e.path).collect())
                .unwrap_or_default()
        };
        files.sort_by(|a, b| crate::catalog::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        let _ = tx.send(files);
        ctx.request_repaint();
    });
    rx
}

fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 {
        format!("{s:.1} s")
    } else {
        format!("{}:{:02} min", (s / 60.0) as u32, (s % 60.0) as u32)
    }
}

// ============================================================ batch convert

pub struct BatchConvert {
    pub files: Vec<PathBuf>,
    /// The browser selection and every image of the folder, while the list is one
    /// of them (the Selected / All switch); `None` once the list was edited.
    scopes: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
    folder: PathBuf,
    selected: HashSet<usize>,
    s: ConvertSettings,
    last_s: ConvertSettings,
    version: u64,
    est_version: u64,
    estimate: Option<Result<Estimate, String>>,
    est_rx: Option<Receiver<(u64, Result<Estimate, String>)>>,
    cache: convert::DecodeCache,
    adding: Option<Receiver<Vec<PathBuf>>>,
    total_bytes: Option<u64>,
    total_rx: Option<Receiver<(u64, u64)>>,
    /// Bumped when the file list changes.
    files_version: u64,
    /// The files version the running/last total computation is for.
    total_for: u64,
    /// Settings/files version the overwrite check below was computed for.
    overwrite_checked: u64,
    /// Converting can replace originals (Overwrite policy into a source folder).
    overwrites: bool,
    job: Option<convert::Job>,
    job_files: Vec<PathBuf>,
    statuses: Vec<FileStatus>,
    only_problems: bool,
    confirm_overwrite: bool,
    toasted: bool,
    adv_selected: Option<usize>,
    /// 0 = basic settings, 1 = advanced steps.
    tab: u8,
    presets: advanced::PresetUi,
    /// Index of the file shown in the before/after preview.
    preview_index: usize,
    preview_tex: Option<(egui::TextureHandle, egui::TextureHandle)>,
}

impl BatchConvert {
    /// `selected` are the files picked in the browser (or the viewer's image) and `all`
    /// every image in the folder. With a selection the list starts with it, as in
    /// FastStone, and a switch in the Source card goes to the whole folder.
    pub fn new(selected: Vec<PathBuf>, all: Vec<PathBuf>, folder: PathBuf, _core: &Core) -> Self {
        let s = convert::load_prefs().convert;
        let images = |v: Vec<PathBuf>| -> Vec<PathBuf> { v.into_iter().filter(|p| crate::formats::is_image_path(p)).collect() };
        let (selected, all) = (images(selected), images(all));
        let (files, scopes) = if selected.is_empty() {
            (all, None)
        } else if all.len() <= selected.len() {
            (selected, None)
        } else {
            (selected.clone(), Some((selected, all)))
        };
        BatchConvert {
            files,
            scopes,
            folder,
            selected: HashSet::new(),
            last_s: s.clone(),
            s,
            version: 1,
            est_version: 0,
            estimate: None,
            est_rx: None,
            cache: Default::default(),
            adding: None,
            total_bytes: None,
            total_rx: None,
            files_version: 1,
            total_for: 0,
            overwrite_checked: 0,
            overwrites: false,
            job: None,
            job_files: Vec::new(),
            statuses: Vec::new(),
            only_problems: false,
            confirm_overwrite: false,
            toasted: false,
            adv_selected: None,
            tab: convert::load_prefs().convert_tab.min(1),
            presets: advanced::PresetUi::new(),
            preview_index: 0,
            preview_tex: None,
        }
    }

    /// The list was edited by hand: it is no longer "the selection" or "the folder".
    fn files_edited(&mut self) {
        self.scopes = None;
        self.files_changed();
    }

    fn files_changed(&mut self) {
        self.selected.clear();
        self.total_bytes = None;
        self.files_version += 1;
        self.version += 1;
    }

    fn add_files(&mut self, new: Vec<PathBuf>) {
        let have: HashSet<String> = self.files.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
        let mut added = 0;
        for f in new {
            if crate::formats::is_image_path(&f) && !have.contains(&f.to_string_lossy().to_lowercase()) {
                self.files.push(f);
                added += 1;
            }
        }
        if added > 0 {
            self.files_edited();
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        use crossbeam_channel::TryRecvError;
        if let Some(rx) = &self.adding {
            match rx.try_recv() {
                Ok(v) => {
                    self.adding = None;
                    self.add_files(v);
                }
                Err(TryRecvError::Disconnected) => self.adding = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.s != self.last_s {
            self.last_s = self.s.clone();
            self.version += 1;
        }
        if self.overwrite_checked != self.version {
            self.overwrite_checked = self.version;
            self.overwrites = self.s.overwrites_originals(&self.files);
        }
        if let Some(rx) = &self.est_rx {
            match rx.try_recv() {
                Ok((v, mut r)) => {
                    if v == self.version {
                        if let Ok(e) = &mut r {
                            if let (Some(b), Some(a)) = (e.before.take(), e.after.take()) {
                                let tex = |name: &str, img: &image::RgbaImage| ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()), egui::TextureOptions::LINEAR);
                                self.preview_tex = Some((tex("bc-before", &b), tex("bc-after", &a)));
                            }
                        } else {
                            self.preview_tex = None;
                        }
                        self.estimate = Some(r);
                    }
                    self.est_version = v;
                    self.est_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.estimate = Some(Err("The preview could not be rendered".into()));
                    self.est_version = self.version;
                    self.est_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        // The preview follows the selected file (the first one when nothing is selected).
        let want_preview = self.selected.iter().min().copied().unwrap_or(0).min(self.files.len().saturating_sub(1));
        if want_preview != self.preview_index {
            self.preview_index = want_preview;
            self.version += 1;
        }
        if self.job.is_none() && self.est_rx.is_none() && self.est_version < self.version && !ctx.egui_is_using_pointer() {
            match self.files.get(self.preview_index).cloned() {
                None => {
                    self.estimate = None;
                    self.preview_tex = None;
                    self.est_version = self.version;
                }
                Some(first) => {
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    let (s, v, cache, c, idx) = (self.s.clone(), self.version, self.cache.clone(), ctx.clone(), self.preview_index);
                    std::thread::spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| convert::estimate(&first, idx, &s, &cache)))
                            .unwrap_or_else(|e| Err(format!("Preview failed: {}", convert::panic_message(&*e))));
                        let _ = tx.send((v, r));
                        c.request_repaint();
                    });
                    self.est_rx = Some(rx);
                }
            }
        }
        // Total size of the list, summed on a worker (20k stat calls take a while).
        if let Some(rx) = &self.total_rx {
            match rx.try_recv() {
                Ok((v, bytes)) => {
                    if v == self.files_version {
                        self.total_bytes = Some(bytes);
                    }
                    self.total_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.total_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.total_bytes.is_none() && self.total_rx.is_none() && self.total_for != self.files_version {
            self.total_for = self.files_version;
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (files, v, c) = (self.files.clone(), self.files_version, ctx.clone());
            std::thread::spawn(move || {
                let bytes = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
                let _ = tx.send((v, bytes));
                c.request_repaint();
            });
            self.total_rx = Some(rx);
        }
        if let Some(job) = &mut self.job {
            while let Ok((i, st)) = job.rx.try_recv() {
                if let Some(slot) = self.statuses.get_mut(i) {
                    *slot = st;
                }
            }
            if job.elapsed.is_none() && job.progress.finished.load(Ordering::SeqCst) {
                job.elapsed = Some(job.started.elapsed());
            }
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let s = self.s.clone();
        convert::save_prefs(|p| p.convert = s.clone());
        self.job_files = self.files.clone();
        self.statuses = vec![FileStatus::Pending; self.job_files.len()];
        self.job = Some(convert::start(self.job_files.clone(), s, ctx.clone()));
        self.only_problems = false;
        self.toasted = false;
    }

    /// Returns false when the window was closed.
    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        self.poll(ctx);
        if let Some(job) = &self.job {
            if job.elapsed.is_some() && !self.toasted {
                self.toasted = true;
                let ok = self.statuses.iter().filter(|s| matches!(s, FileStatus::Done { .. })).count();
                let renamed = self.statuses.iter().filter(|s| matches!(s, FileStatus::Done { note: Some(_), .. })).count();
                let extra = if renamed > 0 { format!(" ({renamed} given a numbered name so no other file of the batch was replaced)") } else { String::new() };
                core.toasts.success(format!("Batch convert finished: {ok} of {} files{extra}", self.statuses.len()));
            }
        }
        let mut open = true;
        let mut close = false;
        widgets::app_window("batch-convert", "Batch convert & resize")
            .open(&mut open)
            .default_size(780.0, 680.0)
            .min_size(520.0, 360.0)
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    if self.job.is_some() {
                        close = self.job_ui(ui, &p);
                    } else {
                        close = self.settings_ui(ui, &p);
                    }
                })
            });
        if self.confirm_overwrite {
            match crate::ui::widgets::show_confirm(
                ctx,
                "Replace original files?",
                "With these settings, converted files can overwrite originals that have the same name and format. Each file is written to a temporary file first and only then replaces the original, but the original pixels and metadata are lost.",
                "Overwrite originals",
                true,
                &p,
            ) {
                Some(true) => {
                    self.confirm_overwrite = false;
                    self.start(ctx);
                }
                Some(false) => self.confirm_overwrite = false,
                None => {}
            }
        }
        let still_open = open && !close;
        if !still_open {
            if let Some(job) = &self.job {
                job.progress.cancel.store(true, Ordering::SeqCst);
            }
            let (s, tab) = (self.s.clone(), self.tab);
            convert::save_prefs(|p| {
                p.convert = s;
                p.convert_tab = tab;
            });
        }
        still_open
    }

    // ------------------------------------------------------------ settings UI

    fn settings_ui(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        let mut close = false;
        let n = self.files.len();
        egui::Panel::bottom("bc-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            footer_row(
                ui,
                |ui| {
                    ui.add(egui::Label::new(RichText::new(format!("{}  {} worker threads", I::LIGHTNING, convert::worker_threads())).size(11.5).color(p.faint)).truncate());
                },
                |ui| {
                    let label = format!("{}  Convert {} file{}", I::PLAY, n, if n == 1 { "" } else { "s" });
                    let ok = n > 0 && !(self.s.dest == DestMode::Folder && self.s.out_folder.is_none());
                    if primary_button(ui, p, ok, &label).clicked() {
                        if self.overwrites {
                            self.confirm_overwrite = true;
                        } else {
                            self.start(ui.ctx());
                        }
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                },
            );
        });
        egui::Panel::top("bc-tabs").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 0, bottom: 10 })).show(ui, |ui| {
            let active = self.s.advanced.steps.iter().filter(|e| e.enabled).count();
            let adv = if self.s.use_advanced && active > 0 { format!("Advanced ({active})") } else { "Advanced".to_string() };
            widgets::segmented_value(ui, &mut self.tab, &[(0, "Basic"), (1, adv.as_str())]);
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.tab == 1 {
                if ui.available_width() < NARROW {
                    // Narrow: steps, presets and preview in one scrolling column.
                    egui::ScrollArea::vertical().id_salt("bc-adv-scroll").auto_shrink([false, false]).show(ui, |ui| {
                        self.advanced_card(ui, p);
                        ui.add_space(8.0);
                        self.presets_card(ui, p);
                        ui.add_space(8.0);
                        self.estimate_card(ui, p);
                    });
                    return;
                }
                // Steps on the left, live before/after preview on the right.
                ui.columns(2, |cols| {
                    egui::ScrollArea::vertical().id_salt("bc-adv-scroll").auto_shrink([false, false]).show(&mut cols[0], |ui| {
                        self.advanced_card(ui, p);
                        ui.add_space(8.0);
                        self.presets_card(ui, p);
                    });
                    egui::ScrollArea::vertical().id_salt("bc-adv-preview").auto_shrink([false, false]).show(&mut cols[1], |ui| {
                        self.estimate_card(ui, p);
                    });
                });
                return;
            }
            egui::ScrollArea::vertical().id_salt("bc-scroll").auto_shrink([false, false]).show(ui, |ui| {
                columns_or_stack(ui, |ui, col| {
                    if col == 0 {
                        self.sources_card(ui, p);
                        ui.add_space(8.0);
                        self.destination_card(ui, p);
                        ui.add_space(8.0);
                        self.presets_card(ui, p);
                    } else {
                        self.format_card(ui, p);
                        ui.add_space(8.0);
                        self.resize_card(ui, p);
                        ui.add_space(8.0);
                        self.adjust_card(ui, p);
                    }
                });
                ui.add_space(8.0);
                self.estimate_card(ui, p);
            });
        });
        close
    }

    fn sources_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::IMAGES, "Source files", |ui| {
            if let Some((sel, all)) = &self.scopes {
                let (sel_label, all_label) = (format!("Selected ({})", sel.len()), format!("All in folder ({})", all.len()));
                let mut i = if self.files.len() == sel.len() { 0 } else { 1 };
                if widgets::segmented(ui, &mut i, &[sel_label.as_str(), all_label.as_str()]) {
                    self.files = if i == 0 { sel.clone() } else { all.clone() };
                    self.files_changed();
                }
                ui.add_space(4.0);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(format!("{}  Add files", I::FILE_PLUS)).clicked() {
                    if let Some(v) = rfd::FileDialog::new().add_filter("Images", &image_exts()).set_directory(&self.folder).pick_files() {
                        self.add_files(v);
                    }
                }
                if ui.button(format!("{}  Add folder", I::FOLDER_PLUS)).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&self.folder).pick_folder() {
                        self.adding = Some(scan_folder(dir, self.s.include_subfolders, ui.ctx().clone()));
                    }
                }
                let can_remove = !self.selected.is_empty();
                if ui.add_enabled(can_remove, egui::Button::new(format!("{}  Remove", I::MINUS_CIRCLE))).on_hover_text("Remove the selected files from the list").clicked() {
                    let sel = std::mem::take(&mut self.selected);
                    let mut i = 0;
                    self.files.retain(|_| {
                        let keep = !sel.contains(&i);
                        i += 1;
                        keep
                    });
                    self.files_edited();
                }
                if ui.add_enabled(!self.files.is_empty(), egui::Button::new(format!("{}  Clear", I::BROOM))).on_hover_text("Remove every file from the list").clicked() {
                    self.files.clear();
                    self.files_edited();
                }
            });
            crate::ui::widgets::checkbox(ui, &mut self.s.include_subfolders, "Include subfolders when adding a folder");
            ui.add_space(4.0);
            egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if self.adding.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Scanning folder...");
                    });
                }
                if self.files.is_empty() {
                    empty_state(ui, p, I::IMAGES, "No files yet", "Add files or a folder to get started.");
                    return;
                }
                let row_h = 22.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical().id_salt("bc-files").max_height(190.0).auto_shrink([false, true]).show_rows(ui, row_h, self.files.len(), |ui, range| {
                    for i in range {
                        let f = &self.files[i];
                        let sel = self.selected.contains(&i);
                        let r = list_row(ui, p, sel, &file_name(f), row_h);
                        if r.clicked() {
                            let ctrl = ui.input(|i| i.modifiers.command);
                            if !ctrl {
                                self.selected.clear();
                            }
                            if sel && ctrl {
                                self.selected.remove(&i);
                            } else {
                                self.selected.insert(i);
                            }
                        }
                        r.on_hover_text(f.display().to_string());
                    }
                });
            });
            let size = self.total_bytes.map(|b| format!(" · {}", human_size(b))).unwrap_or_default();
            hint(ui, p, &format!("{} file{}{}   ·   Ctrl+click to select several", self.files.len(), if self.files.len() == 1 { "" } else { "s" }, size));
        });
    }

    fn destination_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::FOLDER_OPEN, "Destination", |ui| {
            crate::ui::widgets::radio_value(ui, &mut self.s.dest, DestMode::Same, "Same folder as the original");
            ui.horizontal(|ui| {
                crate::ui::widgets::radio_value(ui, &mut self.s.dest, DestMode::Subfolder, "Subfolder");
                ui.add_enabled(self.s.dest == DestMode::Subfolder, egui::TextEdit::singleline(&mut self.s.subfolder).desired_width(140.0).hint_text("Converted"));
            });
            ui.horizontal(|ui| {
                crate::ui::widgets::radio_value(ui, &mut self.s.dest, DestMode::Folder, "Folder");
                let label = self.s.out_folder.as_ref().map(|f| f.display().to_string()).unwrap_or_else(|| "Choose...".into());
                let r = ui.add(egui::Button::new(format!("{}  {}", I::FOLDER, label)).truncate());
                if r.clicked() {
                    let mut dlg = rfd::FileDialog::new();
                    if let Some(f) = self.s.out_folder.as_ref().or(Some(&self.folder)) {
                        dlg = dlg.set_directory(f);
                    }
                    if let Some(dir) = dlg.pick_folder() {
                        self.s.out_folder = Some(dir);
                        self.s.dest = DestMode::Folder;
                    }
                }
            });
            if self.s.dest == DestMode::Folder && self.s.out_folder.is_none() {
                ui.label(RichText::new("Choose an output folder").size(11.5).color(p.warn));
            }
            ui.add_space(6.0);
            ui.label(RichText::new("File name").color(p.muted));
            ui.add(egui::TextEdit::singleline(&mut self.s.template).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace).hint_text("{name}"));
            token_chips(ui, &mut self.s.template, &["{name}", "{n}", "{n:3}", "{date}", "{width}", "{height}", "{ext}"]);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Counter starts at").color(p.muted));
                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut self.s.start).range(0..=1_000_000));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("If the file exists").color(p.muted));
                crate::ui::widgets::combo("bc-conflict").width(200.0).selected_text(self.s.conflict.label()).show_ui(ui, |ui| {
                    for c in ConflictPolicy::ALL {
                        ui.selectable_value(&mut self.s.conflict, c, c.label());
                    }
                });
            });
            if self.overwrites {
                ui.label(RichText::new(format!("{}  Originals with the same name and format will be replaced.", I::WARNING)).size(12.0).color(p.danger));
            }
        });
    }

    fn format_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::FILE_IMAGE, "Output format", |ui| {
            let current = if self.s.keep_format { "Keep original format" } else { format_label(self.s.format) };
            crate::ui::widgets::combo("bc-format").width(200.0).selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.s.keep_format, true, "Keep original format");
                for f in SaveFormat::ALL {
                    if ui.selectable_label(!self.s.keep_format && self.s.format == f, format_label(f)).clicked() {
                        self.s.keep_format = false;
                        self.s.format = f;
                    }
                }
            });
            let fmt = if self.s.keep_format { None } else { Some(self.s.format) };
            if self.s.keep_format {
                hint(ui, p, "Formats Lumen can't write (HEIC, RAW, ...) are saved as JPEG.");
            }
            // Say up front what the chosen format cannot keep.
            let has_ext = |exts: &[&str]| self.files.iter().any(|f| exts.contains(&crate::formats::ext_of(f).as_str()));
            if let Some(f) = fmt {
                if has_ext(&["gif", "apng", "webp", "tif", "tiff"]) {
                    hint(ui, p, "Animated and multi-page files are converted using their first frame only.");
                }
                if matches!(f, SaveFormat::Jpeg | SaveFormat::Bmp) && has_ext(&["png", "gif", "webp", "tif", "tiff", "ico", "tga", "qoi", "psd", "svg"]) {
                    hint(ui, p, "This format has no transparency: transparent areas are filled with white.");
                }
            }
            egui::Grid::new("bc-format-grid").num_columns(2).spacing([10.0, 6.0]).striped(false).show(ui, |ui| {
                if fmt.is_none() || fmt == Some(SaveFormat::Jpeg) {
                    ui.label("JPEG quality");
                    ui.add(crate::ui::widgets::Slider::new(&mut self.s.jpeg_quality, 1..=100));
                    ui.end_row();
                    ui.label("Chroma");
                    crate::ui::widgets::combo("bc-chroma").selected_text(self.s.jpeg_chroma.label()).show_ui(ui, |ui| {
                        for c in [Chroma::S444, Chroma::S422, Chroma::S420] {
                            ui.selectable_value(&mut self.s.jpeg_chroma, c, c.label());
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    crate::ui::widgets::checkbox(ui, &mut self.s.jpeg_progressive, "Progressive");
                    ui.end_row();
                }
                if fmt.is_none() || fmt == Some(SaveFormat::WebP) {
                    ui.label("WebP");
                    crate::ui::widgets::checkbox(ui, &mut self.s.webp_lossless, "Lossless");
                    ui.end_row();
                    if !self.s.webp_lossless {
                        ui.label("WebP quality");
                        ui.add(crate::ui::widgets::Slider::new(&mut self.s.webp_quality, 1.0..=100.0).fixed_decimals(0));
                        ui.end_row();
                    }
                }
                if fmt.is_none() || fmt == Some(SaveFormat::Png) {
                    ui.label("PNG");
                    crate::ui::widgets::checkbox(ui, &mut self.s.png_best, "Best compression (slower)");
                    ui.end_row();
                }
                if fmt == Some(SaveFormat::Ico) {
                    ui.label("");
                    hint(ui, p, "Icons are limited to 256 × 256.");
                    ui.end_row();
                }
            });
        });
    }

    fn resize_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::ARROWS_OUT_SIMPLE, "Resize", |ui| {
            let r = &mut self.s.resize;
            egui::Grid::new("bc-resize-grid").num_columns(2).spacing([10.0, 6.0]).striped(false).show(ui, |ui| {
                ui.label("Mode");
                crate::ui::widgets::combo("bc-resize-mode").width(160.0).selected_text(r.mode.label()).show_ui(ui, |ui| {
                    for m in ResizeMode::ALL {
                        ui.selectable_value(&mut r.mode, m, m.label());
                    }
                });
                ui.end_row();
                match r.mode {
                    ResizeMode::None => {}
                    ResizeMode::Percent => {
                        ui.label("Scale");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.percent).range(1.0..=1000.0).speed(0.5).suffix(" %"));
                        ui.end_row();
                    }
                    ResizeMode::Fit => {
                        ui.label("Box");
                        ui.horizontal(|ui| {
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.a).range(1..=60000).suffix(" px"));
                            ui.label("×");
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.b).range(1..=60000).suffix(" px"));
                        });
                        ui.end_row();
                    }
                    _ => {
                        ui.label("Size");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.a).range(1..=60000).suffix(" px"));
                        ui.end_row();
                    }
                }
                if r.mode != ResizeMode::None {
                    ui.label("");
                    crate::ui::widgets::checkbox(ui, &mut r.no_upscale, "Don't enlarge smaller images");
                    ui.end_row();
                    ui.label("Filter");
                    crate::ui::widgets::combo("bc-filter").width(160.0).selected_text(self.s.filter.label()).show_ui(ui, |ui| {
                        for f in Filter::ALL {
                            ui.selectable_value(&mut self.s.filter, f, f.label());
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    crate::ui::widgets::checkbox(ui, &mut self.s.sharpen, "Light sharpening after downscale");
                    ui.end_row();
                }
            });
            if r.mode == ResizeMode::None {
                hint(ui, p, "Images keep their original pixel size.");
            }
        });
    }

    fn adjust_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::MAGIC_WAND, "Adjustments & metadata", |ui| {
            crate::ui::widgets::checkbox(ui, &mut self.s.auto_enhance, "Auto enhance (levels)");
            crate::ui::widgets::checkbox(ui, &mut self.s.grayscale, "Convert to grayscale");
            crate::ui::widgets::checkbox(ui, &mut self.s.keep_exif, "Keep EXIF metadata").on_hover_text("Camera data, date taken, GPS... (written to JPEG output)");
            crate::ui::widgets::checkbox(ui, &mut self.s.preserve_date, "Keep the original modified date");
            hint(ui, p, "Photos are rotated upright using their EXIF orientation.");
        });
    }

    fn presets_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::BOOKMARK_SIMPLE, "Presets", |ui| {
            self.presets.ui(ui, p, &mut self.s);
            hint(ui, p, "A preset stores the format, resize, adjustments, destination and advanced steps.");
        });
    }

    fn advanced_card(&mut self, ui: &mut Ui, p: &Palette) {
        card(ui, p, I::SLIDERS_HORIZONTAL, "Advanced steps", |ui| {
            crate::ui::widgets::checkbox(ui, &mut self.s.use_advanced, "Apply these steps to every image (after the basic resize and adjustments)");
            ui.add_enabled_ui(self.s.use_advanced, |ui| {
                advanced::pipeline_ui(ui, p, &mut self.s.advanced, &mut self.adv_selected);
            });
        });
    }

    fn preview_images(&mut self, ui: &mut Ui, p: &Palette) {
        let Some((before, after)) = &self.preview_tex else { return };
        let avail = ui.available_width();
        // Side by side when there is room, otherwise one above the other.
        let side = avail > 520.0;
        let h_max = if side { 300.0f32 } else { 240.0 };
        let slot_w = if side { (avail - 12.0) / 2.0 } else { avail };
        let fit = |t: &egui::TextureHandle| {
            let sz = t.size_vec2();
            let s = (slot_w / sz.x).min(h_max / sz.y).min(1.5);
            sz * s
        };
        let show = |ui: &mut Ui, t: &egui::TextureHandle, label: &str| {
            ui.vertical(|ui| {
                ui.label(RichText::new(label).font(text::caption()).color(p.muted));
                ui.add(egui::Image::new((t.id(), fit(t))).corner_radius(CornerRadius::same(4)));
            });
        };
        if side {
            ui.horizontal_top(|ui| {
                show(ui, before, "Before");
                ui.add_space(8.0);
                show(ui, after, "After");
            });
        } else {
            show(ui, before, "Before");
            show(ui, after, "After");
        }
    }

    fn estimate_card(&mut self, ui: &mut Ui, p: &Palette) {
        let title = if self.selected.is_empty() { "Preview (first file)" } else { "Preview (selected file)" };
        card(ui, p, I::EYE, title, |ui| {
            let Some(first) = self.files.get(self.preview_index).cloned() else {
                hint(ui, p, "Add files to see the expected result.");
                return;
            };
            let first = &first;
            let pending = self.est_version < self.version;
            ui.horizontal_wrapped(|ui| {
                if self.files.len() > 1 && self.files.len() <= 5000 {
                    // Pick the preview file without going back to the source list.
                    let mut idx = self.preview_index;
                    crate::ui::widgets::combo("bc-preview-file").width(180.0).truncate().selected_text(file_name(first)).show_ui(ui, |ui| {
                        for (i, f) in self.files.iter().enumerate() {
                            ui.selectable_value(&mut idx, i, file_name(f));
                        }
                    });
                    if idx != self.preview_index {
                        self.selected.clear();
                        self.selected.insert(idx);
                    }
                } else {
                    ui.label(RichText::new(file_name(first)).strong());
                }
                match &self.estimate {
                    Some(Ok(e)) => {
                        ui.label(RichText::new(format!("{} × {} · {}", e.in_dims.0, e.in_dims.1, human_size(e.in_bytes))).color(p.muted));
                        ui.label(RichText::new(I::ARROW_RIGHT).color(p.accent));
                        ui.label(RichText::new(&e.name).strong());
                        ui.label(RichText::new(format!("{} × {} · ~{}", e.out_dims.0, e.out_dims.1, human_size(e.out_bytes as u64))).color(p.muted));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(format!("{}  {e}", I::WARNING_CIRCLE)).color(p.danger));
                    }
                    None => {}
                }
                if pending {
                    ui.spinner();
                }
            });
            if matches!(self.estimate, Some(Ok(_))) {
                ui.add_space(4.0);
                self.preview_images(ui, p);
            }
            if let (Some(Ok(e)), n) = (&self.estimate, self.files.len()) {
                if n > 1 && e.in_bytes > 0 {
                    if let Some(total) = self.total_bytes {
                        let ratio = e.out_bytes as f64 / e.in_bytes as f64;
                        hint(ui, p, &format!("All {n} files: about {} -> {} (estimated from the first file)", human_size(total), human_size((total as f64 * ratio) as u64)));
                    }
                }
            }
        });
    }

    // ----------------------------------------------------------------- job UI

    fn job_ui(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        let mut close = false;
        let Some(job) = &self.job else { return false };
        let total = job.progress.total;
        let done = job.progress.done.load(Ordering::Relaxed);
        let finished = job.progress.finished.load(Ordering::SeqCst);
        let cancelled = job.progress.cancel.load(Ordering::Relaxed);
        let (mut ok, mut skipped, mut failed, mut in_b, mut out_b) = (0usize, 0usize, 0usize, 0u64, 0u64);
        for s in &self.statuses {
            match s {
                FileStatus::Done { in_bytes, out_bytes, .. } => {
                    ok += 1;
                    in_b += in_bytes;
                    out_b += out_bytes;
                }
                FileStatus::Skipped(_) => skipped += 1,
                FileStatus::Failed(_) => failed += 1,
                _ => {}
            }
        }
        let elapsed = job.elapsed.unwrap_or_else(|| job.started.elapsed());
        let mut back = false;
        let mut open_folder = None;
        egui::Panel::bottom("bc-job-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            footer_row(
                ui,
                |_| {},
                |ui| {
                    if finished {
                        if primary_button(ui, p, true, "Done").clicked() {
                            close = true;
                        }
                        if ui.add(egui::Button::new(format!("{}  Back to settings", I::ARROW_U_UP_LEFT)).min_size(Vec2::new(0.0, size::CONTROL))).clicked() {
                            back = true;
                        }
                        let first_out = self.statuses.iter().find_map(|s| if let FileStatus::Done { out, .. } = s { Some(out.clone()) } else { None });
                        if let Some(out) = first_out {
                            if ui.add(egui::Button::new(format!("{}  Show in Explorer", I::ARROW_SQUARE_OUT)).min_size(Vec2::new(0.0, size::CONTROL))).clicked() {
                                open_folder = Some(out);
                            }
                        }
                    } else if !cancelled {
                        if danger_button(ui, p, &format!("{}  Cancel", I::STOP)).clicked() {
                            job.progress.cancel.store(true, Ordering::SeqCst);
                        }
                    } else {
                        ui.spinner();
                        ui.label("Stopping after the files in progress...");
                    }
                },
            );
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let title = if !finished {
                format!("Converting... {done} of {total}")
            } else if cancelled {
                format!("Cancelled - {ok} of {total} converted")
            } else if failed == 0 {
                format!("Finished - {ok} file{} converted", if ok == 1 { "" } else { "s" })
            } else {
                format!("Finished with {failed} error{}", if failed == 1 { "" } else { "s" })
            };
            let (icon, color) = if !finished {
                (I::ARROWS_CLOCKWISE, p.accent)
            } else if failed > 0 || cancelled {
                (I::WARNING_CIRCLE, p.warn)
            } else {
                (I::CHECK_CIRCLE, p.ok)
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(22.0).color(color));
                ui.add(egui::Label::new(RichText::new(title).font(text::title())).wrap());
            });
            ui.add_space(6.0);
            let frac = if total == 0 { 1.0 } else { done as f32 / total as f32 };
            ui.add(egui::ProgressBar::new(frac).fill(p.accent).desired_height(8.0).corner_radius(CornerRadius::same(4)));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let stat = |ui: &mut Ui, icon: &str, c: Color32, text: String| {
                    ui.label(RichText::new(icon).color(c));
                    ui.label(RichText::new(text).color(p.muted));
                    ui.add_space(10.0);
                };
                stat(ui, I::CHECK_CIRCLE, p.ok, format!("{ok} converted"));
                if skipped > 0 {
                    stat(ui, I::MINUS_CIRCLE, p.warn, format!("{skipped} skipped"));
                }
                if failed > 0 {
                    stat(ui, I::X_CIRCLE, p.danger, format!("{failed} failed"));
                }
                if ok > 0 {
                    let pct = if in_b > 0 { (out_b as f64 / in_b as f64 * 100.0).round() as i64 } else { 100 };
                    stat(ui, I::HARD_DRIVES, p.accent, format!("{} -> {} ({pct}%)", human_size(in_b), human_size(out_b)));
                }
                stat(ui, I::TIMER, p.faint, fmt_duration(elapsed));
                if skipped + failed > 0 {
                    widgets::checkbox(ui, &mut self.only_problems, "Show only problems");
                }
            });
            ui.add_space(6.0);
            let rows: Vec<usize> = (0..self.statuses.len()).filter(|&i| !self.only_problems || matches!(self.statuses[i], FileStatus::Skipped(_) | FileStatus::Failed(_))).collect();
            egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical().id_salt("bc-job-list").auto_shrink([false, false]).show_rows(ui, 26.0, rows.len(), |ui, range| {
                    for &i in &rows[range] {
                        let f = &self.job_files[i];
                        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 26.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_height(26.0);
                            ui.spacing_mut().item_spacing.x = 8.0;
                            match &self.statuses[i] {
                                FileStatus::Pending => {
                                    ui.label(RichText::new(I::CIRCLE).color(p.faint));
                                }
                                FileStatus::Working => {
                                    ui.add(egui::Spinner::new().size(14.0));
                                }
                                FileStatus::Done { .. } => {
                                    ui.label(RichText::new(I::CHECK_CIRCLE).color(p.ok));
                                }
                                FileStatus::Skipped(_) => {
                                    ui.label(RichText::new(I::MINUS_CIRCLE).color(p.warn));
                                }
                                FileStatus::Failed(_) => {
                                    ui.label(RichText::new(I::X_CIRCLE).color(p.danger));
                                }
                            }
                            ui.add(egui::Label::new(RichText::new(file_name(f)).size(12.5)).truncate()).on_hover_text(f.display().to_string());
                            match &self.statuses[i] {
                                FileStatus::Done { out, out_bytes, dims, note, .. } => {
                                    ui.label(RichText::new(I::ARROW_RIGHT).color(p.faint));
                                    if let Some(note) = note {
                                        ui.label(RichText::new(I::WARNING_CIRCLE).color(p.warn)).on_hover_text(note);
                                    }
                                    ui.add(egui::Label::new(RichText::new(file_name(out)).size(12.5)).truncate()).on_hover_text(out.display().to_string());
                                    ui.label(RichText::new(format!("{} × {} · {}", dims.0, dims.1, human_size(*out_bytes))).size(11.5).color(p.muted));
                                }
                                FileStatus::Skipped(why) => {
                                    ui.add(egui::Label::new(RichText::new(why).size(12.0).color(p.warn)).truncate());
                                }
                                FileStatus::Failed(why) => {
                                    ui.add(egui::Label::new(RichText::new(why).size(12.0).color(p.danger)).truncate()).on_hover_text(why);
                                }
                                FileStatus::Working => {
                                    ui.label(RichText::new("Working...").size(11.5).color(p.muted));
                                }
                                FileStatus::Pending => {}
                            }
                        });
                    }
                });
            });
        });
        if let Some(out) = open_folder {
            crate::fileops::show_in_explorer(&out);
        }
        if back {
            self.job = None;
            self.version += 1;
        }
        close
    }
}

/// A fixed-size, left-aligned table cell.
fn cell(ui: &mut Ui, size: [f32; 2], w: impl egui::Widget) -> egui::Response {
    ui.allocate_ui_with_layout(Vec2::from(size), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(size[0]);
        ui.add(w)
    })
    .inner
}

/// Compact, left-aligned selectable list row.
fn list_row(ui: &mut Ui, p: &Palette, selected: bool, text: &str, h: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            p.selected_bg
        } else if resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(4), bg);
        let galley = ui.painter().layout(text.to_string(), egui::FontId::proportional(12.5), p.text, f32::INFINITY);
        let clip = rect.shrink2(Vec2::new(6.0, 0.0));
        ui.painter().with_clip_rect(clip).galley(egui::pos2(clip.min.x, rect.center().y - galley.size().y / 2.0), galley, p.text);
    }
    resp
}

/// Clickable token chips that append a token to a template.
fn token_chips(ui: &mut Ui, template: &mut String, tokens: &[&str]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
        for t in tokens {
            let desc = logic::TOKENS.iter().find(|(k, _)| k == t).map(|(_, d)| *d).unwrap_or("");
            if ui.add(egui::Button::new(RichText::new(*t).monospace().size(11.0)).small().corner_radius(CornerRadius::same(10))).on_hover_text(desc).clicked() {
                template.push_str(t);
            }
        }
    });
}

// ============================================================= batch rename

struct PreviewRow {
    new_name: String,
    target: PathBuf,
    issue: Option<RenameIssue>,
    changed: bool,
}

/// Messages from the metadata reader.
struct InfoUpdate {
    index: usize,
    modified: Option<chrono::NaiveDateTime>,
    dims: Option<Option<(u32, u32)>>,
    taken: Option<Option<chrono::NaiveDateTime>>,
}

struct MetaJob {
    rx: Receiver<InfoUpdate>,
    done: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
    full: bool,
}

struct RenameJob {
    rx: Receiver<logic::RenameOutcome>,
    progress: Arc<AtomicUsize>,
    steps: usize,
    undo: bool,
}

pub struct BatchRename {
    pub files: Vec<PathBuf>,
    infos: Vec<NameInfo>,
    s: RenameSettings,
    last_s: Option<RenameSettings>,
    rows: Vec<PreviewRow>,
    dirty: bool,
    meta: Option<MetaJob>,
    have_full: bool,
    job: Option<RenameJob>,
    last_batch: Vec<(PathBuf, PathBuf)>,
    status: Option<(bool, String)>,
    only_changed: bool,
    /// Lower-case names present in each folder, read once per file list
    /// (refreshed after a rename) instead of one stat per target per keystroke.
    dir_names: HashMap<PathBuf, HashSet<String>>,
}

/// Finish any batch rename that was interrupted by a crash or power cut (from
/// the journal in the data folder). Returns messages for the user; call once
/// at startup. The batch rename window also calls it when it opens.
pub fn recover_pending_renames() -> Vec<String> {
    logic::recover_journal(&logic::journal_path())
}

fn list_dir_names(dirs: impl Iterator<Item = PathBuf>) -> HashMap<PathBuf, HashSet<String>> {
    let mut out = HashMap::new();
    for d in dirs {
        if out.contains_key(&d) {
            continue;
        }
        let names: HashSet<String> = std::fs::read_dir(&d)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
            .unwrap_or_default();
        out.insert(d, names);
    }
    out
}

fn start_meta(files: Vec<PathBuf>, full: bool, ctx: egui::Context) -> MetaJob {
    let (tx, rx) = crossbeam_channel::unbounded();
    let done = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let total = files.len();
    let (d, c) = (done.clone(), cancel.clone());
    std::thread::spawn(move || {
        use rayon::prelude::*;
        let threads = convert::worker_threads();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(if full { threads } else { 1 }).build();
        let work = |i: usize| {
            if c.load(Ordering::Relaxed) {
                return;
            }
            let p = &files[i];
            // A panic while reading one file's metadata must still count it as done.
            let r = std::panic::catch_unwind(|| {
                let modified = std::fs::metadata(p).ok().and_then(|m| m.modified().ok()).map(logic::systime_to_naive);
                let (dims, taken) = if full {
                    (Some(crate::decode::probe_dimensions(p)), Some(crate::meta::read_exif(p).and_then(|e| e.taken_ts).and_then(logic::ts_to_naive)))
                } else {
                    (None, None)
                };
                InfoUpdate { index: i, modified, dims, taken }
            });
            if let Ok(u) = r {
                let _ = tx.send(u);
            }
            let n = d.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 64 == 0 || n == files.len() {
                ctx.request_repaint();
            }
        };
        match pool {
            Ok(pool) => pool.install(|| (0..files.len()).into_par_iter().for_each(work)),
            Err(_) => (0..files.len()).for_each(work),
        }
        // Cancelled runs skip files; mark everything done so the UI never waits forever.
        d.store(files.len(), Ordering::SeqCst);
        ctx.request_repaint();
    });
    MetaJob { rx, done, total, cancel, full }
}

impl BatchRename {
    pub fn new(files: Vec<PathBuf>, core: &Core) -> Self {
        let recovered = recover_pending_renames();
        let infos = files.iter().map(|p| NameInfo::from_path(p)).collect();
        let meta = Some(start_meta(files.clone(), false, core.ctx.clone()));
        let dir_names = list_dir_names(files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)));
        BatchRename {
            files,
            infos,
            s: convert::load_prefs().rename,
            last_s: None,
            rows: Vec::new(),
            dirty: true,
            meta,
            have_full: false,
            job: None,
            last_batch: Vec::new(),
            status: if recovered.is_empty() { None } else { Some((!recovered.iter().any(|m| m.starts_with("Could not")), recovered.join("; "))) },
            only_changed: false,
            dir_names,
        }
    }

    fn poll(&mut self, ctx: &egui::Context) -> Vec<(PathBuf, PathBuf)> {
        let mut renamed = Vec::new();
        if self.last_s.as_ref() != Some(&self.s) {
            self.last_s = Some(self.s.clone());
            self.dirty = true;
        }
        let needs_full = logic::needs_dims(&self.s.template) || logic::needs_taken(&self.s.template);
        if let Some(m) = &self.meta {
            // Check completion before draining so no update is lost.
            let finished = m.done.load(Ordering::SeqCst) >= m.total;
            while let Ok(u) = m.rx.try_recv() {
                if let Some(info) = self.infos.get_mut(u.index) {
                    info.modified = u.modified.or(info.modified);
                    if let Some(d) = u.dims {
                        info.dims = d;
                    }
                    if let Some(t) = u.taken {
                        info.taken = t;
                    }
                }
                self.dirty = true;
            }
            if finished {
                if m.full {
                    self.have_full = true;
                }
                self.meta = None;
                self.dirty = true;
            }
        }
        if needs_full && !self.have_full && !self.meta.as_ref().is_some_and(|m| m.full) {
            // The full pass also reads modified dates, so it replaces a quick pass.
            if let Some(m) = &self.meta {
                m.cancel.store(true, Ordering::Relaxed);
            }
            self.meta = Some(start_meta(self.files.clone(), true, ctx.clone()));
        }
        if let Some(job) = &self.job {
            let out = match job.rx.try_recv() {
                Ok(out) => Some(out),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(logic::RenameOutcome { done: Vec::new(), errors: vec!["The rename stopped unexpectedly; check the folder (it is repaired the next time this window opens)".into()] })
                }
                Err(_) => None,
            };
            if let Some(out) = out {
                let undo = job.undo;
                self.job = None;
                self.dir_names = list_dir_names(self.files.iter().chain(out.done.iter().map(|(_, t)| t)).filter_map(|f| f.parent().map(Path::to_path_buf)));
                // Update our own list to the new paths. Look up by the paths as they
                // were before the batch, so chains (1->2, 2->3) map each file once.
                let index: HashMap<PathBuf, usize> = self.files.iter().cloned().enumerate().map(|(i, f)| (f, i)).collect();
                for (from, to) in &out.done {
                    if let Some(&i) = index.get(from) {
                        self.files[i] = to.clone();
                        let mut info = NameInfo::from_path(to);
                        let old = &self.infos[i];
                        info.modified = old.modified;
                        info.dims = old.dims;
                        info.taken = old.taken;
                        self.infos[i] = info;
                    }
                }
                let n = out.done.len();
                self.status = Some(if out.errors.is_empty() {
                    (true, format!("{} {n} file{}", if undo { "Restored" } else { "Renamed" }, if n == 1 { "" } else { "s" }))
                } else {
                    (false, format!("{n} renamed, {} failed: {}", out.errors.len(), out.errors.join("; ")))
                });
                self.last_batch = if undo { Vec::new() } else { out.done.clone() };
                renamed = out.done;
                self.dirty = true;
            }
        }
        if self.dirty {
            self.recompute();
        }
        renamed
    }

    fn recompute(&mut self) {
        self.dirty = false;
        let pad = logic::auto_pad(&self.s, self.files.len());
        let names: Vec<String> = self.infos.iter().enumerate().map(|(i, info)| logic::new_file_name(&self.s, info, i, pad)).collect();
        let targets: Vec<PathBuf> = self.files.iter().zip(&names).map(|(f, n)| f.with_file_name(n)).collect();
        let dir_names = &self.dir_names;
        let issues = logic::check_targets(&self.files, &targets, |p| {
            let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return p.exists() };
            match dir_names.get(dir) {
                Some(names) => names.contains(&name.to_string_lossy().to_lowercase()),
                None => p.exists(),
            }
        });
        self.rows = names
            .into_iter()
            .zip(targets)
            .zip(issues)
            .zip(&self.files)
            .map(|(((new_name, target), issue), f)| {
                let changed = target != *f;
                PreviewRow { new_name, target, issue, changed }
            })
            .collect();
    }

    fn start_rename(&mut self, pairs: Vec<(PathBuf, PathBuf)>, undo: bool, ctx: &egui::Context) {
        let tag = format!("{}-{}", std::process::id(), chrono::Local::now().timestamp_millis());
        let plan = logic::plan_two_phase(&pairs, &tag);
        let steps = plan.len() * 2;
        let progress = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (pr, c) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let journal = logic::journal_path();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                logic::execute_plan_journaled(&plan, Some(&journal), |n| {
                    pr.store(n, Ordering::Relaxed);
                    if n % 50 == 0 {
                        c.request_repaint();
                    }
                })
            }));
            let out = r.unwrap_or_else(|e| {
                // Repair right away from the journal so no file keeps a temporary name.
                let mut errors = vec![format!("The rename failed unexpectedly: {}", convert::panic_message(&*e))];
                errors.extend(logic::recover_journal(&journal));
                logic::RenameOutcome { done: Vec::new(), errors }
            });
            let _ = tx.send(out);
            c.request_repaint();
        });
        self.status = None;
        self.job = Some(RenameJob { rx, progress, steps, undo });
    }

    /// Returns (still open, renamed (from, to) pairs completed this frame).
    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> (bool, Vec<(PathBuf, PathBuf)>) {
        let p = core.pal;
        let renamed = self.poll(ctx);
        if let Some((true, msg)) = &self.status {
            if !renamed.is_empty() {
                core.toasts.success(msg.clone());
            }
        }
        let mut open = true;
        let mut close = false;
        widgets::app_window("batch-rename", "Batch rename")
            .open(&mut open)
            .default_size(820.0, 640.0)
            .min_size(520.0, 380.0)
            .show(ctx, |ui| {
                close = window_body(ui, |ui| self.ui(ui, &p));
            });
        let still_open = open && !close;
        if !still_open {
            if let Some(m) = &self.meta {
                m.cancel.store(true, Ordering::Relaxed);
            }
            let s = self.s.clone();
            convert::save_prefs(|p| p.rename = s);
        }
        (still_open, renamed)
    }

    fn ui(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        let mut close = false;
        let conflicts = self.rows.iter().filter(|r| r.issue.is_some()).count();
        let changes = self.rows.iter().filter(|r| r.changed).count();
        let busy = self.job.is_some();
        let mut do_rename = false;
        let mut do_undo = false;
        egui::Panel::bottom("br-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            if let Some(job) = &self.job {
                let n = job.progress.load(Ordering::Relaxed);
                ui.add(egui::ProgressBar::new(n as f32 / job.steps.max(1) as f32).fill(p.accent).desired_height(6.0));
                ui.add_space(4.0);
            }
            let n_files = self.files.len();
            let last_batch = !self.last_batch.is_empty();
            footer_row(
                ui,
                |ui| match &self.status {
                    Some((ok, msg)) => {
                        let (icon, c) = if *ok { (I::CHECK_CIRCLE, p.ok) } else { (I::WARNING_CIRCLE, p.danger) };
                        ui.label(RichText::new(icon).color(c));
                        ui.add(egui::Label::new(RichText::new(msg).color(c)).truncate());
                    }
                    None if conflicts > 0 => {
                        ui.add(egui::Label::new(RichText::new(format!("{}  {conflicts} conflict{} - fix them to rename", I::WARNING, if conflicts == 1 { "" } else { "s" })).color(p.danger)).truncate());
                    }
                    None => {
                        ui.add(egui::Label::new(RichText::new(format!("{changes} of {n_files} file{} will be renamed", if n_files == 1 { "" } else { "s" })).color(p.muted)).truncate());
                    }
                },
                |ui| {
                    let label = format!("{}  Rename {changes} file{}", I::CHECK, if changes == 1 { "" } else { "s" });
                    if primary_button(ui, p, !busy && conflicts == 0 && changes > 0, &label).clicked() {
                        do_rename = true;
                    }
                    if widgets::secondary_button(ui, "Close").clicked() {
                        close = true;
                    }
                    if last_batch && ui.add_enabled(!busy, egui::Button::new(format!("{}  Undo", I::ARROW_COUNTER_CLOCKWISE)).min_size(Vec2::new(0.0, size::CONTROL))).on_hover_text("Restore the names from the last rename").clicked() {
                        do_undo = true;
                    }
                },
            );
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            // Small window (large interface size): everything scrolls, the preview list
            // gets a fixed height.
            if ui.available_width() < NARROW || ui.available_height() < 420.0 {
                egui::ScrollArea::vertical().id_salt("br-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| self.settings_cards(ui, p));
                    ui.add_space(8.0);
                    self.preview_header(ui, p);
                    self.preview_table(ui, p, 240.0);
                });
            } else {
                ui.add_enabled_ui(!busy, |ui| self.settings_cards(ui, p));
                ui.add_space(8.0);
                self.preview_header(ui, p);
                self.preview_table(ui, p, f32::INFINITY);
            }
        });
        if do_rename {
            let pairs: Vec<(PathBuf, PathBuf)> = self.files.iter().zip(&self.rows).filter(|(_, r)| r.changed).map(|(f, r)| (f.clone(), r.target.clone())).collect();
            self.start_rename(pairs, false, ui.ctx());
        }
        if do_undo {
            let pairs: Vec<(PathBuf, PathBuf)> = self.last_batch.iter().map(|(f, t)| (t.clone(), f.clone())).collect();
            self.start_rename(pairs, true, ui.ctx());
        }
        close
    }

    fn settings_cards(&mut self, ui: &mut Ui, p: &Palette) {
        columns_or_stack(ui, |ui, col| {
            if col == 0 {
                card(ui, p, I::TEXTBOX, "New name", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.s.template).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace).hint_text("{name}"));
                    token_chips(ui, &mut self.s.template, &["{name}", "{n}", "{n:3}", "{date}", "{taken}", "{width}", "{height}", "{folder}"]);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Start").color(p.muted));
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut self.s.start).range(-1_000_000..=1_000_000));
                        ui.label(RichText::new("Step").color(p.muted));
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut self.s.step).range(-1000..=1000));
                        ui.label(RichText::new("Digits").color(p.muted));
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut self.s.pad).range(0..=10).custom_formatter(|v, _| if v == 0.0 { "auto".into() } else { format!("{v}") }));
                    });
                    hint(ui, p, "The extension is kept. {date:%Y%m%d} accepts a custom date format.");
                });
            } else {
                card(ui, p, I::MAGNIFYING_GLASS, "Find, replace & case", |ui| {
                    egui::Grid::new("br-grid").num_columns(2).spacing([10.0, 6.0]).striped(false).show(ui, |ui| {
                        ui.label("Find");
                        ui.add(egui::TextEdit::singleline(&mut self.s.find).desired_width(f32::INFINITY));
                        ui.end_row();
                        ui.label("Replace");
                        ui.add(egui::TextEdit::singleline(&mut self.s.replace).desired_width(f32::INFINITY));
                        ui.end_row();
                        ui.label("");
                        crate::ui::widgets::checkbox(ui, &mut self.s.ignore_case, "Ignore case");
                        ui.end_row();
                        ui.label("Name case");
                        crate::ui::widgets::combo("br-case").selected_text(self.s.case.label()).show_ui(ui, |ui| {
                            for c in CaseChange::ALL {
                                ui.selectable_value(&mut self.s.case, c, c.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Extension");
                        crate::ui::widgets::combo("br-ext").selected_text(self.s.ext_case.label()).show_ui(ui, |ui| {
                            for c in ExtCase::ALL {
                                ui.selectable_value(&mut self.s.ext_case, c, c.label());
                            }
                        });
                        ui.end_row();
                    });
                });
            }
        });
    }

    fn preview_header(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::checkbox(ui, &mut self.only_changed, "Only changed or conflicting");
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Preview").font(text::caption()).color(p.muted));
                    if let Some(m) = &self.meta {
                        if m.full {
                            ui.spinner();
                            ui.add(egui::Label::new(RichText::new(format!("Reading image details... {} / {}", m.done.load(Ordering::Relaxed), m.total)).size(11.5).color(p.muted)).truncate());
                        }
                    }
                });
            });
        });
    }

    fn preview_table(&mut self, ui: &mut Ui, p: &Palette, max_h: f32) {
        egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if self.files.is_empty() {
                empty_state(ui, p, I::TEXTBOX, "Nothing to rename", "Select files in the browser, then open Batch rename.");
                return;
            }
            let w = ui.available_width();
            let col_w = ((w - 60.0) / 2.0).max(120.0);
            ui.horizontal(|ui| {
                cell(ui, [30.0, 18.0], egui::Label::new(RichText::new("#").size(11.5).color(p.faint)));
                cell(ui, [col_w, 18.0], egui::Label::new(RichText::new("Current name").size(11.5).color(p.faint)));
                cell(ui, [col_w, 18.0], egui::Label::new(RichText::new("New name").size(11.5).color(p.faint)));
            });
            ui.separator();
            let rows: Vec<usize> = (0..self.rows.len()).filter(|&i| !self.only_changed || self.rows[i].changed || self.rows[i].issue.is_some()).collect();
            if rows.is_empty() {
                hint(ui, p, "No file names change with the current settings.");
                return;
            }
            let row_h = 24.0;
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().id_salt("br-preview").max_height(max_h).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
                for &i in &rows[range] {
                    let r = &self.rows[i];
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_height(row_h);
                        ui.spacing_mut().item_spacing.x = 8.0;
                        cell(ui, [30.0, row_h], egui::Label::new(RichText::new(format!("{}", i + 1)).size(11.5).color(p.faint)));
                        let old = file_name(&self.files[i]);
                        cell(ui, [col_w, row_h], egui::Label::new(RichText::new(old).size(12.5).color(if r.changed { p.muted } else { p.faint })).truncate());
                        let (color, icon) = match &r.issue {
                            Some(_) => (p.danger, Some(I::WARNING)),
                            None if r.changed => (p.text, Some(I::ARROW_RIGHT)),
                            None => (p.faint, None),
                        };
                        let resp = cell(ui, [col_w - 24.0, row_h], egui::Label::new(RichText::new(&r.new_name).size(12.5).color(color).strong()).truncate());
                        if let Some(issue) = &r.issue {
                            resp.on_hover_text(issue.message());
                            ui.label(RichText::new(icon.unwrap_or("")).color(p.danger)).on_hover_text(issue.message());
                        }
                    });
                }
            });
        });
    }
}
