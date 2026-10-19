//! "Save as" dialog with format options and a live file-size estimate.

use crate::encode::{Chroma, SaveOptions};
use crate::formats::SaveFormat;
use crossbeam_channel::Receiver;
use super::icons as I;
use super::theme::{self, size, text};
use super::widgets;
use egui::{RichText, Vec2};
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::Arc;

pub struct SaveDialog {
    pub path: PathBuf,
    pub opts: SaveOptions,
    image: Arc<RgbaImage>,
    estimate: Option<Result<usize, String>>,
    est_rx: Option<Receiver<(u64, Result<usize, String>)>>,
    est_version: u64,
    version: u64,
    pub error: Option<String>,
    /// The file being edited: saving over it needs no "replace?" question.
    original: PathBuf,
    /// The user was told the target exists; the next Save click replaces it.
    confirm_replace: bool,
    /// The name field got its first focus (base name selected).
    focused: bool,
    /// The name field as typed (`path` follows it while it is not empty).
    name: String,
}

pub enum SaveOutcome {
    Open,
    Cancel,
    Save(PathBuf, SaveOptions),
}

impl SaveDialog {
    pub fn new(path: PathBuf, image: Arc<RgbaImage>, mut opts: SaveOptions) -> Self {
        if let Some(f) = SaveFormat::from_ext(&crate::formats::ext_of(&path)) {
            opts.format = f;
        }
        let original = path.clone();
        let path = path.with_extension(opts.format.ext());
        SaveDialog { path, opts, image, estimate: None, est_rx: None, est_version: 0, version: 1, error: None, original, confirm_replace: false, focused: false, name: String::new() }.with_name()
    }

    /// The target is an existing file other than the one being edited.
    fn overwrites_other(&self) -> bool {
        let same = self.path.to_string_lossy().to_lowercase() == self.original.to_string_lossy().to_lowercase();
        !same && self.path.exists()
    }

    /// Show the file name of `path` in the name field.
    fn with_name(mut self) -> Self {
        self.sync_name();
        self
    }

    fn sync_name(&mut self) {
        self.name = self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    }

    fn folder(&self) -> PathBuf {
        self.path.parent().map(|d| d.to_path_buf()).unwrap_or_default()
    }

    /// The system Save dialog, for another folder (or name).
    fn browse(&mut self) {
        let mut dlg = rfd::FileDialog::new().add_filter(self.opts.format.label(), &[self.opts.format.ext()]);
        if let Some(dir) = self.path.parent() {
            dlg = dlg.set_directory(dir);
        }
        if let Some(n) = self.path.file_name() {
            dlg = dlg.set_file_name(n.to_string_lossy());
        }
        if let Some(p) = dlg.save_file() {
            self.path = p;
            self.sync_name();
            // The system dialog already asked about replacing an existing file.
            self.confirm_replace = true;
            if let Some(f) = SaveFormat::from_ext(&crate::formats::ext_of(&self.path)) {
                self.opts.format = f;
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, p: &super::theme::Palette) -> SaveOutcome {
        let mut out = SaveOutcome::Open;
        if let Some(rx) = &self.est_rx {
            if let Ok((v, r)) = rx.try_recv() {
                if v == self.version {
                    self.estimate = Some(r);
                }
                self.est_version = v;
                self.est_rx = None;
            }
        }
        if self.est_version < self.version && self.est_rx.is_none() {
            // Estimate on a downscaled copy for big images, then extrapolate by pixel count.
            let img = self.image.clone();
            let opts = self.opts.clone();
            let v = self.version;
            let (tx, rx) = crossbeam_channel::bounded(1);
            let c = ctx.clone();
            std::thread::spawn(move || {
                let px = img.width() as f64 * img.height() as f64;
                let res = if px > 6e6 {
                    let s = crate::imaging::resize_to_fit(&img, 2400, 2400);
                    let f = px / (s.width() as f64 * s.height() as f64);
                    crate::encode::encode(&s, &opts).map(|b| (b.len() as f64 * f) as usize)
                } else {
                    crate::encode::encode(&img, &opts).map(|b| b.len())
                };
                let _ = tx.send((v, res.map_err(|e| e.to_string())));
                c.request_repaint();
            });
            self.est_rx = Some(rx);
        }

        let mut changed = false;
        let mut save_clicked = false;
        widgets::modal(ctx, egui::Id::unique("save-dialog"), 460.0, |ui| {
            widgets::modal_title(ui, "Save as");
            ui.spacing_mut().item_spacing.y = widgets::FORM_ROW_GAP;
            ui.spacing_mut().combo_width = 200.0;
            let overwrites = self.overwrites_other();
            let ctl_w = ui.available_width() - size::LABEL_W - ui.spacing().item_spacing.x;

            // The name is what is usually changed; the folder comes from the open file.
            widgets::form_row(ui, "Name", |ui| {
                let field = ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(ctl_w).min_size(Vec2::new(0.0, size::CONTROL)).align(egui::Align2::LEFT_CENTER));
                if !self.focused {
                    self.focused = true;
                    field.request_focus();
                    // Select the base name, as Explorer does.
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field.id) {
                        let end = self.name.rfind('.').filter(|&i| i > 0).unwrap_or(self.name.len());
                        let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.name[..end].chars().count()));
                        state.cursor.set_char_range(Some(range));
                        state.store(ui.ctx(), field.id);
                    }
                }
                if field.changed() && !self.name.trim().is_empty() {
                    let typed = PathBuf::from(self.name.trim());
                    // A full path typed or pasted into the name goes anywhere.
                    self.path = if typed.is_absolute() { typed } else { self.folder().join(typed) };
                    self.confirm_replace = false;
                    // A typed extension picks the format.
                    if let Some(f) = SaveFormat::from_ext(&crate::formats::ext_of(&self.path)) {
                        if f != self.opts.format {
                            self.opts.format = f;
                            changed = true;
                        }
                    }
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save_clicked = true;
                }
            });
            widgets::form_row(ui, "Folder", |ui| {
                let folder = self.folder().display().to_string();
                let label = widgets::elide_start(ui, &folder, text::body(), ctl_w - 44.0);
                let b = egui::Button::new(RichText::new(format!("{}  {label}", I::FOLDER)).font(text::body())).min_size(Vec2::new(ctl_w, size::CONTROL));
                let r = ui.add(b).on_hover_text(format!("{folder}\nClick to choose another folder or name"));
                if r.clicked() {
                    self.browse();
                    changed = true;
                }
            });
            ui.add_space(4.0);
            widgets::form_row(ui, "Format", |ui| {
                widgets::combo("save-format").selected_text(self.opts.format.label()).show_ui(ui, |ui| {
                    for f in SaveFormat::ALL {
                        if ui.selectable_value(&mut self.opts.format, f, f.label()).changed() {
                            self.path = self.path.with_extension(f.ext());
                            self.sync_name();
                            self.confirm_replace = false;
                            changed = true;
                        }
                    }
                });
            });
            match self.opts.format {
                SaveFormat::Jpeg => {
                    widgets::form_row(ui, "Quality", |ui| {
                        changed |= ui.add(widgets::Slider::new(&mut self.opts.jpeg_quality, 1..=100).default_value(92)).changed();
                    });
                    widgets::form_row(ui, "Chroma", |ui| {
                        widgets::combo("chroma").selected_text(self.opts.jpeg_chroma.label()).show_ui(ui, |ui| {
                            for c in [Chroma::S444, Chroma::S422, Chroma::S420] {
                                changed |= ui.selectable_value(&mut self.opts.jpeg_chroma, c, c.label()).changed();
                            }
                        });
                    });
                    widgets::form_row(ui, "", |ui| {
                        changed |= widgets::checkbox(ui, &mut self.opts.jpeg_progressive, "Progressive").changed();
                    });
                    widgets::form_row(ui, "", |ui| {
                        widgets::checkbox(ui, &mut self.opts.keep_metadata, "Keep EXIF metadata");
                    });
                }
                SaveFormat::WebP => {
                    if !self.opts.webp_lossless {
                        widgets::form_row(ui, "Quality", |ui| {
                            changed |= ui.add(widgets::Slider::new(&mut self.opts.webp_quality, 1.0..=100.0).fixed_decimals(0).default_value(88.0)).changed();
                        });
                    }
                    widgets::form_row(ui, "", |ui| {
                        changed |= widgets::checkbox(ui, &mut self.opts.webp_lossless, "Lossless").changed();
                    });
                }
                SaveFormat::Png => {
                    widgets::form_row(ui, "", |ui| {
                        changed |= widgets::checkbox(ui, &mut self.opts.png_best_compression, "Best compression (slower)").changed();
                    });
                }
                _ => {}
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING_CIRCLE)).color(p.danger)).wrap());
            }
            if overwrites && self.confirm_replace {
                ui.add_space(4.0);
                let name = self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(format!("{}  {name} already exists. Save again to replace it.", I::WARNING)).color(p.warn)).wrap());
            }
            ui.add_space(theme::space::BUTTONS_GAP - widgets::FORM_ROW_GAP);
            // Footer: the size estimate on the left, Cancel / Save on the right.
            let ok = !self.name.trim().is_empty();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let replace = overwrites && self.confirm_replace;
                    let r = if replace { ui.add_enabled_ui(ok, |ui| widgets::danger_button(ui, p, "Replace")).inner } else { widgets::primary_button_enabled(ui, p, ok, "Save") };
                    if r.clicked() {
                        save_clicked = true;
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        out = SaveOutcome::Cancel;
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new("Estimated size").color(p.muted));
                        match &self.estimate {
                            Some(Ok(n)) if self.est_version >= self.version => {
                                ui.label(RichText::new(crate::catalog::human_size(*n as u64)).color(p.text));
                                ui.add(egui::Label::new(RichText::new(format!("{} × {}", self.image.width(), self.image.height())).color(p.faint)).truncate());
                            }
                            Some(Err(e)) if self.est_version >= self.version => {
                                ui.add(egui::Label::new(RichText::new(e).color(p.danger)).truncate());
                            }
                            _ => {
                                ui.add(egui::Spinner::new().size(14.0));
                            }
                        }
                    });
                });
            });
            if !ok {
                save_clicked = false;
            }
        });
        if changed {
            self.version += 1;
        }
        if save_clicked && !matches!(out, SaveOutcome::Cancel) {
            if self.overwrites_other() && !self.confirm_replace {
                self.confirm_replace = true;
            } else {
                out = SaveOutcome::Save(self.path.clone(), self.opts.clone());
            }
        }
        out
    }
}
