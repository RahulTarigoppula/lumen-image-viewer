//! "Save as" dialog with format options and a live file-size estimate.

use crate::encode::{Chroma, SaveOptions};
use crate::formats::SaveFormat;
use crossbeam_channel::Receiver;
use egui::{Color32, RichText};
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
        let path = path.with_extension(opts.format.ext());
        SaveDialog { path, opts, image, estimate: None, est_rx: None, est_version: 0, version: 1, error: None }
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

        egui::Modal::new(egui::Id::new("save-dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Save as");
            ui.add_space(8.0);
            let mut changed = false;
            ui.horizontal(|ui| {
                let mut s = self.path.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut s).desired_width(360.0)).changed() {
                    self.path = PathBuf::from(s);
                }
                if ui.button("Browse...").clicked() {
                    let mut dlg = rfd::FileDialog::new().add_filter(self.opts.format.label(), &[self.opts.format.ext()]);
                    if let Some(dir) = self.path.parent() {
                        dlg = dlg.set_directory(dir);
                    }
                    if let Some(n) = self.path.file_name() {
                        dlg = dlg.set_file_name(n.to_string_lossy());
                    }
                    if let Some(p) = dlg.save_file() {
                        self.path = p;
                        if let Some(f) = SaveFormat::from_ext(&crate::formats::ext_of(&self.path)) {
                            self.opts.format = f;
                            changed = true;
                        }
                    }
                }
            });
            ui.add_space(6.0);
            egui::Grid::new("save-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Format");
                egui::ComboBox::from_id_salt("save-format").selected_text(self.opts.format.label()).show_ui(ui, |ui| {
                    for f in SaveFormat::ALL {
                        if ui.selectable_value(&mut self.opts.format, f, f.label()).changed() {
                            self.path = self.path.with_extension(f.ext());
                            changed = true;
                        }
                    }
                });
                ui.end_row();
                match self.opts.format {
                    SaveFormat::Jpeg => {
                        ui.label("Quality");
                        changed |= ui.add(egui::Slider::new(&mut self.opts.jpeg_quality, 1..=100)).changed();
                        ui.end_row();
                        ui.label("Chroma");
                        egui::ComboBox::from_id_salt("chroma").selected_text(self.opts.jpeg_chroma.label()).show_ui(ui, |ui| {
                            for c in [Chroma::S444, Chroma::S422, Chroma::S420] {
                                changed |= ui.selectable_value(&mut self.opts.jpeg_chroma, c, c.label()).changed();
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        changed |= ui.checkbox(&mut self.opts.jpeg_progressive, "Progressive").changed();
                        ui.end_row();
                    }
                    SaveFormat::WebP => {
                        ui.label("");
                        changed |= ui.checkbox(&mut self.opts.webp_lossless, "Lossless").changed();
                        ui.end_row();
                        if !self.opts.webp_lossless {
                            ui.label("Quality");
                            changed |= ui.add(egui::Slider::new(&mut self.opts.webp_quality, 1.0..=100.0)).changed();
                            ui.end_row();
                        }
                    }
                    SaveFormat::Png => {
                        ui.label("");
                        changed |= ui.checkbox(&mut self.opts.png_best_compression, "Best compression (slower)").changed();
                        ui.end_row();
                    }
                    _ => {}
                }
                if self.opts.format == SaveFormat::Jpeg {
                    ui.label("");
                    ui.checkbox(&mut self.opts.keep_metadata, "Keep EXIF metadata");
                    ui.end_row();
                }
                ui.label("Estimated size");
                match &self.estimate {
                    _ if self.est_version < self.version => {
                        ui.spinner();
                    }
                    Some(Ok(n)) => {
                        ui.label(RichText::new(format!("{}  ({} × {})", crate::catalog::human_size(*n as u64), self.image.width(), self.image.height())).strong());
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(p.danger));
                    }
                    None => {
                        ui.spinner();
                    }
                }
                ui.end_row();
            });
            if changed {
                self.version += 1;
            }
            if let Some(e) = &self.error {
                ui.add_space(6.0);
                ui.colored_label(p.danger, e);
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    out = SaveOutcome::Cancel;
                }
                if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(p.accent).min_size(egui::vec2(80.0, 28.0))).clicked() {
                    out = SaveOutcome::Save(self.path.clone(), self.opts.clone());
                }
            });
        });
        out
    }
}
