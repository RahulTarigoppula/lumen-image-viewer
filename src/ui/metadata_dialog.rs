//! "Edit metadata" dialog: JPEG comment, lossless metadata removal, file /
//! EXIF dates and XMP sidecar export, for one or many files. All edits are
//! lossless (no re-encoding) and run on a background thread.

#![allow(dead_code)]

use super::icons as I;
use super::theme::Palette;
use crate::app::Core;
use crate::metaedit::{self, DateTags, StripOptions};
use crossbeam_channel::Receiver;
use egui::{RichText, Ui};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Comment,
    Strip,
    Dates,
    Sidecar,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileDateMode {
    Keep,
    FromExif,
    Specific,
}

struct Job {
    rx: Receiver<Vec<(PathBuf, Result<String, String>)>>,
    label: String,
}

pub struct MetadataDialog {
    paths: Vec<PathBuf>,
    pub tab: Tab,
    comment: String,
    original_comment: Option<String>,
    strip: StripOptions,
    keep_times: bool,
    file_mode: FileDateMode,
    file_date: String,
    set_created: bool,
    set_modified: bool,
    shift_exif: bool,
    shift_sign: i8,
    shift: [i64; 4], // days, hours, minutes, seconds
    exif_tags: DateTags,
    job: Option<Job>,
    results: Vec<(PathBuf, Result<String, String>)>,
    last_label: String,
}

impl MetadataDialog {
    /// `paths`: the files to edit (the viewer's current image, or the
    /// browser selection).
    pub fn new(paths: Vec<PathBuf>, _core: &Core) -> Self {
        let original_comment = if paths.len() == 1 && metaedit::is_jpeg(&paths[0]) {
            metaedit::read_jpeg_comment(&paths[0]).ok().flatten()
        } else {
            None
        };
        let file_date = paths
            .first()
            .and_then(|p| metaedit::exif_date_taken(p))
            .unwrap_or_else(|| chrono::Local::now().naive_local())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        MetadataDialog {
            comment: original_comment.clone().unwrap_or_default(),
            original_comment,
            paths,
            tab: Tab::Comment,
            strip: StripOptions::default(),
            keep_times: true,
            file_mode: FileDateMode::FromExif,
            file_date,
            set_created: true,
            set_modified: true,
            shift_exif: false,
            shift_sign: 1,
            shift: [0, 0, 0, 0],
            exif_tags: DateTags::default(),
            job: None,
            results: Vec::new(),
            last_label: String::new(),
        }
    }

    /// Open directly on a tab (e.g. Tab::Comment for the "D" key).
    pub fn with_tab(mut self, tab: Tab) -> Self {
        self.tab = tab;
        self
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Returns false once the dialog is closed.
    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        self.poll(core);
        let mut open = true;
        let mut close = false;
        // A text field focused at the start of the frame consumes Esc (it only
        // drops focus), so typing in the comment field never closes the dialog.
        let typing = ctx.egui_wants_keyboard_input();
        let title = if self.paths.len() == 1 {
            format!("{}  Edit metadata - {}", I::NOTE_PENCIL, self.paths[0].file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
        } else {
            format!("{}  Edit metadata - {} files", I::NOTE_PENCIL, self.paths.len())
        };
        egui::Window::new(title)
            .open(&mut open)
            .default_width(520.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                close = self.ui(ui, &p, core);
            });
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.job.is_none() {
            close = true;
        }
        open && !close
    }

    fn poll(&mut self, core: &mut Core) {
        let Some(job) = &self.job else { return };
        let Ok(results) = job.rx.try_recv() else { return };
        let label = job.label.clone();
        self.job = None;
        let failed = results.iter().filter(|r| r.1.is_err()).count();
        for (path, r) in &results {
            if r.is_ok() {
                core.loader.invalidate(path);
                core.meta.forget(path);
                core.thumbs.forget(path);
            }
        }
        let ok = results.len() - failed;
        if failed == 0 {
            core.toasts.success(format!("{label}: {ok} file(s) updated"));
        } else {
            core.toasts.error(format!("{label}: {ok} updated, {failed} failed"));
        }
        if self.tab == Tab::Comment && failed == 0 && self.paths.len() == 1 {
            self.original_comment = if self.comment.is_empty() { None } else { Some(self.comment.clone()) };
        }
        self.last_label = label;
        self.results = results;
    }

    fn run(&mut self, ctx: &egui::Context, label: &str, op: impl Fn(&std::path::Path) -> anyhow::Result<String> + Send + 'static) {
        let paths = self.paths.clone();
        self.run_on(ctx, label, paths, op);
    }

    fn run_on(&mut self, ctx: &egui::Context, label: &str, paths: Vec<PathBuf>, op: impl Fn(&std::path::Path) -> anyhow::Result<String> + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let c = ctx.clone();
        std::thread::spawn(move || {
            let res: Vec<_> = paths
                .into_iter()
                .map(|p| {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| op(&p)))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error")))
                        .map_err(|e| format!("{e:#}"));
                    (p, r)
                })
                .collect();
            let _ = tx.send(res);
            c.request_repaint();
        });
        self.results.clear();
        self.job = Some(Job { rx, label: label.to_string() });
    }

    fn ui(&mut self, ui: &mut Ui, p: &Palette, core: &mut Core) -> bool {
        let mut close = false;
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            for (t, name) in [(Tab::Comment, "Comment"), (Tab::Strip, "Remove metadata"), (Tab::Dates, "Dates"), (Tab::Sidecar, "XMP sidecar")] {
                if ui.selectable_label(self.tab == t, name).clicked() {
                    self.tab = t;
                }
            }
        });
        ui.separator();
        ui.add_enabled_ui(!busy, |ui| match self.tab {
            Tab::Comment => self.comment_ui(ui, p),
            Tab::Strip => self.strip_ui(ui, p),
            Tab::Dates => self.dates_ui(ui, p),
            Tab::Sidecar => self.sidecar_ui(ui, p, core),
        });
        ui.add_space(6.0);
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working...");
            });
        } else if !self.results.is_empty() {
            let failed: Vec<_> = self.results.iter().filter_map(|(path, r)| r.as_ref().err().map(|e| (path, e))).collect();
            if failed.is_empty() {
                let note = self.results.iter().find_map(|r| r.1.as_ref().ok()).cloned().unwrap_or_default();
                ui.label(RichText::new(format!("{}: done. {note}", self.last_label)).color(p.ok));
            } else {
                egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    for (path, e) in failed {
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        ui.label(RichText::new(format!("{name}: {e}")).color(p.danger));
                    }
                });
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.keep_times, "Keep file dates when rewriting")
                .on_hover_text("Restore the created / modified times after a comment or metadata change");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new("Close")).clicked() {
                    close = true;
                }
            });
        });
        close
    }

    fn comment_ui(&mut self, ui: &mut Ui, p: &Palette) {
        let jpegs = self.paths.iter().filter(|p| metaedit::is_jpeg(p)).count();
        if jpegs == 0 {
            ui.label(RichText::new("Comments can only be stored in JPEG files.").color(p.muted));
            return;
        }
        if self.paths.len() > 1 {
            ui.label(RichText::new(format!("The comment will be written to {jpegs} JPEG file(s); other files are skipped.")).color(p.muted));
        }
        ui.add(egui::TextEdit::multiline(&mut self.comment).desired_rows(5).desired_width(f32::INFINITY).hint_text("JPEG comment (empty removes it)"));
        let changed = self.paths.len() > 1 || self.original_comment.as_deref().unwrap_or("") != self.comment;
        ui.horizontal(|ui| {
            if ui.add_enabled(changed, egui::Button::new("Save comment")).clicked() {
                let text = self.comment.clone();
                let keep = self.keep_times;
                let ctx = ui.ctx().clone();
                self.run(&ctx, "Comment", move |path| {
                    if !metaedit::is_jpeg(path) {
                        return Ok("skipped".into());
                    }
                    metaedit::write_jpeg_comment(path, &text, keep)?;
                    Ok(String::new())
                });
            }
            if self.original_comment.is_some() && ui.button("Remove comment").clicked() {
                self.comment.clear();
            }
        });
    }

    fn strip_ui(&mut self, ui: &mut Ui, p: &Palette) {
        let n = self.paths.iter().filter(|p| metaedit::can_strip(p)).count();
        ui.label("Removes camera data (EXIF, GPS location, embedded thumbnail), XMP, IPTC and other application data without re-encoding the image.");
        if n < self.paths.len() {
            ui.label(RichText::new(format!("{} file(s) are not JPEG or PNG and will be skipped.", self.paths.len() - n)).color(p.warn));
        }
        ui.add_space(4.0);
        ui.checkbox(&mut self.strip.keep_icc, "Keep the color profile (ICC)");
        ui.checkbox(&mut self.strip.keep_orientation, "Keep the orientation (photo stays upright)");
        ui.checkbox(&mut self.strip.keep_comment, "Keep the JPEG comment");
        ui.add_space(4.0);
        if ui.add_enabled(n > 0, egui::Button::new(format!("Remove metadata from {n} file(s)"))).clicked() {
            let opts = self.strip;
            let keep = self.keep_times;
            let ctx = ui.ctx().clone();
            self.run(&ctx, "Remove metadata", move |path| {
                if !metaedit::can_strip(path) {
                    return Ok("skipped".into());
                }
                let r = metaedit::strip_metadata(path, opts, keep)?;
                Ok(format!("Saved {} KB.", r.bytes_before.saturating_sub(r.bytes_after) / 1024))
            });
        }
    }

    fn dates_ui(&mut self, ui: &mut Ui, p: &Palette) {
        ui.label(RichText::new("File dates").strong());
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.file_mode, FileDateMode::Keep, "Don't change");
            ui.radio_value(&mut self.file_mode, FileDateMode::FromExif, "Set to EXIF date taken");
            ui.radio_value(&mut self.file_mode, FileDateMode::Specific, "Set to:");
            ui.add_enabled(self.file_mode == FileDateMode::Specific, egui::TextEdit::singleline(&mut self.file_date).desired_width(150.0));
        });
        let specific = metaedit::parse_datetime(&self.file_date);
        if self.file_mode == FileDateMode::Specific && specific.is_none() {
            ui.label(RichText::new("Use the form YYYY-MM-DD HH:MM:SS").color(p.danger));
        }
        ui.add_enabled_ui(self.file_mode != FileDateMode::Keep, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.set_modified, "Modified");
                ui.checkbox(&mut self.set_created, "Created");
            });
        });
        ui.add_space(6.0);
        ui.label(RichText::new("EXIF dates (JPEG, PNG, WebP)").strong());
        ui.checkbox(&mut self.shift_exif, "Shift EXIF dates by:");
        ui.add_enabled_ui(self.shift_exif, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.shift_sign, 1, "+");
                ui.selectable_value(&mut self.shift_sign, -1, "-");
                for (i, unit) in ["days", "h", "min", "s"].iter().enumerate() {
                    ui.add(egui::DragValue::new(&mut self.shift[i]).range(0..=if i == 0 { 36500 } else { 59 }));
                    ui.label(*unit);
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.exif_tags.original, "Date taken");
                ui.checkbox(&mut self.exif_tags.digitized, "Digitized");
                ui.checkbox(&mut self.exif_tags.modified, "Modified");
            });
            ui.label(RichText::new("Useful when the camera clock was wrong or set to another time zone.").color(p.muted));
        });
        ui.add_space(4.0);
        let offset = self.shift_sign as i64 * (self.shift[0] * 86400 + self.shift[1] * 3600 + self.shift[2] * 60 + self.shift[3]);
        let do_shift = self.shift_exif && offset != 0;
        let do_files = self.file_mode != FileDateMode::Keep && (self.set_created || self.set_modified) && (self.file_mode != FileDateMode::Specific || specific.is_some());
        if ui.add_enabled(do_shift || do_files, egui::Button::new(format!("Apply to {} file(s)", self.paths.len()))).clicked() {
            let (mode, created, modified, tags) = (self.file_mode, self.set_created, self.set_modified, self.exif_tags);
            let keep = self.keep_times;
            let ctx = ui.ctx().clone();
            self.run(&ctx, "Change dates", move |path| {
                let mut notes = Vec::new();
                if do_shift {
                    // Keep file times through the EXIF rewrite unless they are set below.
                    let n = metaedit::shift_exif_dates(path, offset, tags, keep || do_files)?;
                    notes.push(format!("{n} EXIF date(s) shifted."));
                }
                if do_files {
                    let t = match mode {
                        FileDateMode::FromExif => {
                            let dt = metaedit::exif_date_taken(path).ok_or_else(|| anyhow::anyhow!("no EXIF date taken"))?;
                            metaedit::local_to_system(dt)
                        }
                        _ => specific.and_then(metaedit::local_to_system),
                    }
                    .ok_or_else(|| anyhow::anyhow!("invalid date"))?;
                    metaedit::set_file_times(path, created.then_some(t), modified.then_some(t))?;
                    notes.push("File dates set.".into());
                }
                Ok(notes.join(" "))
            });
        }
    }

    fn sidecar_ui(&mut self, ui: &mut Ui, p: &Palette, core: &mut Core) {
        ui.label("Writes the Lumen rating, color label and flag to an XMP sidecar (name.xmp) next to each file, so Lightroom, Bridge and darktable can read them. Existing sidecars are updated in place; files without a rating, label or flag are skipped so existing ratings are not cleared.");
        ui.add_space(4.0);
        let tagged = self.paths.iter().filter(|path| !core.tags.get(path).is_empty()).count();
        ui.label(RichText::new(format!("{tagged} of {} file(s) have a rating, label or flag.", self.paths.len())).color(p.muted));
        ui.horizontal(|ui| {
            if ui.add_enabled(tagged > 0, egui::Button::new("Export sidecars")).clicked() {
                // Untagged files are left alone: writing Rating=0 / Label=""
                // would wipe ratings another program stored in the sidecar.
                let tags: Vec<(PathBuf, crate::tags::Tag)> = self.paths.iter().map(|path| (path.clone(), core.tags.get(path))).filter(|(_, t)| !t.is_empty()).collect();
                let paths: Vec<PathBuf> = tags.iter().map(|t| t.0.clone()).collect();
                let ctx = ui.ctx().clone();
                self.run_on(&ctx, "XMP sidecar", paths, move |path| {
                    let tag = tags.iter().find(|(q, _)| q == path).map(|t| t.1).unwrap_or_default();
                    metaedit::export_xmp_sidecar(path, tag)?;
                    Ok(String::new())
                });
            }
            if ui.button("Import from sidecars").on_hover_text("Read xmp:Rating / xmp:Label back into Lumen").clicked() {
                let mut n = 0;
                for path in &self.paths {
                    if let Some(t) = metaedit::read_xmp_sidecar(path) {
                        core.tags.update(path, |tag| *tag = t);
                        n += 1;
                    }
                }
                core.toasts.info(format!("Imported tags from {n} sidecar(s)"));
            }
        });
    }
}
