//! Windows for the output tools: Create multi-page file (PDF / TIFF / GIF /
//! WebP), Image strip and Create ZIP. Work runs on background threads with
//! progress and cancel; existing files are only replaced after confirmation.

use super::multipage::{self as mp, MultiFormat, MultiSettings, OutProgress, PdfPage, StripDir, StripSettings, StripSize, ZipSettings};
use crate::app::Core;
use crate::catalog::human_size;
use crate::formats::SaveFormat;
use crate::ui::batch::advanced::color_rgba;
use crate::ui::batch::{card, danger_button, empty_state, file_name, hint, primary_button, window_size};
use crate::ui::icons as I;
use crate::ui::theme::Palette;
use crossbeam_channel::{Receiver, TryRecvError};
use egui::{CornerRadius, RichText, Ui, Vec2};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ------------------------------------------------------------------ shared

struct Job {
    progress: Arc<OutProgress>,
    rx: Receiver<Result<String, String>>,
    out: PathBuf,
}

fn spawn_job(ctx: &egui::Context, out: PathBuf, work: impl FnOnce(&OutProgress) -> Result<String, String> + Send + 'static) -> Job {
    let progress = Arc::new(OutProgress::default());
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (prog, c) = (progress.clone(), ctx.clone());
    std::thread::spawn(move || {
        let r = super::guarded("The job", || work(&prog));
        let _ = tx.send(r);
        c.request_repaint();
    });
    Job { progress, rx, out }
}

/// Result of polling a job.
enum Polled {
    Running,
    Done(Result<String, String>, PathBuf),
}

fn poll(job: &Option<Job>) -> Polled {
    let Some(j) = job else { return Polled::Running };
    match j.rx.try_recv() {
        Ok(r) => Polled::Done(r, j.out.clone()),
        Err(TryRecvError::Disconnected) => Polled::Done(Err("The job stopped unexpectedly".into()), j.out.clone()),
        Err(TryRecvError::Empty) => Polled::Running,
    }
}

/// Output folder + file name editor. Returns the full path when valid.
fn target_ui(ui: &mut Ui, p: &Palette, dir: &mut Option<PathBuf>, name: &mut String, ext: &str) -> Option<PathBuf> {
    egui::Grid::new(ui.next_auto_id()).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("File name");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(name).desired_width(180.0));
            ui.label(RichText::new(format!(".{ext}")).color(p.muted));
        });
        ui.end_row();
        ui.label("Folder");
        let label = dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "Choose...".into());
        if ui.add(egui::Button::new(format!("{}  {label}", I::FOLDER)).truncate()).on_hover_text(&label).clicked() {
            let mut dlg = rfd::FileDialog::new();
            if let Some(d) = dir.as_ref() {
                dlg = dlg.set_directory(d);
            }
            if let Some(d) = dlg.pick_folder() {
                *dir = Some(d);
            }
        }
        ui.end_row();
    });
    let stem = crate::ui::batch::logic::sanitize_file_stem(name.trim());
    dir.as_ref().filter(|_| !name.trim().is_empty()).map(|d| d.join(format!("{stem}.{ext}")))
}

/// Footer with status, progress, Cancel / Create. Returns (close, create).
#[allow(clippy::too_many_arguments)]
fn footer(ui: &mut Ui, p: &Palette, id: &'static str, job: &Option<Job>, result: &Option<(Result<String, String>, PathBuf)>, idle_text: &str, create_label: &str, can_create: bool) -> (bool, bool) {
    let (mut close, mut create) = (false, false);
    egui::Panel::bottom(id).frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
        if let Some(j) = job {
            ui.add(egui::ProgressBar::new(j.progress.fraction()).fill(p.accent).desired_height(6.0));
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            match (job, result) {
                (Some(j), _) => {
                    ui.spinner();
                    let phase = j.progress.phase.lock().clone();
                    ui.label(RichText::new(format!("{phase}... {:.0}%", j.progress.fraction() * 100.0)).color(p.muted));
                }
                (None, Some((Ok(msg), _))) => {
                    ui.label(RichText::new(I::CHECK_CIRCLE).color(p.ok));
                    ui.add(egui::Label::new(RichText::new(msg).color(p.ok)).truncate());
                }
                (None, Some((Err(e), _))) if e == mp::CANCELLED => {
                    ui.label(RichText::new("Cancelled").color(p.warn));
                }
                (None, Some((Err(e), _))) => {
                    ui.label(RichText::new(I::WARNING_CIRCLE).color(p.danger));
                    ui.add(egui::Label::new(RichText::new(e).color(p.danger)).truncate()).on_hover_text(e);
                }
                (None, None) => {
                    ui.add(egui::Label::new(RichText::new(idle_text).color(p.muted)).truncate());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(j) = job {
                    if danger_button(ui, p, &format!("{}  Cancel", I::STOP)).clicked() {
                        j.progress.cancel.store(true, Ordering::SeqCst);
                    }
                } else {
                    if primary_button(ui, p, can_create, create_label).clicked() {
                        create = true;
                    }
                    if ui.add(egui::Button::new("Close").min_size(Vec2::new(80.0, 30.0))).clicked() {
                        close = true;
                    }
                    if let Some((Ok(_), out)) = result {
                        if ui.add(egui::Button::new(format!("{}  Show in Explorer", I::ARROW_SQUARE_OUT)).min_size(Vec2::new(0.0, 30.0))).clicked() {
                            crate::fileops::show_in_explorer(out);
                        }
                        if ui.add(egui::Button::new(format!("{}  Open", I::EYE)).min_size(Vec2::new(0.0, 30.0))).clicked() {
                            crate::fileops::open_with_default(out);
                        }
                    }
                }
            });
        });
    });
    (close, create)
}

/// Confirm replacing an existing file. Some(true) = go ahead.
fn confirm_replace(ctx: &egui::Context, p: &Palette, path: &Path) -> Option<bool> {
    crate::ui::widgets::show_confirm(ctx, "Replace existing file?", &format!("{} already exists. Replace it?", path.display()), "Replace", true, p)
}

fn file_list(ui: &mut Ui, p: &Palette, files: &mut Vec<PathBuf>, id: &str) -> bool {
    let mut changed = false;
    let mut action: Option<(usize, i32)> = None;
    egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if files.is_empty() {
            empty_state(ui, p, I::IMAGES, "No images", "Select images in the browser first.");
            return;
        }
        egui::ScrollArea::vertical().id_salt(id).max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
            let n = files.len();
            for (i, f) in files.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).color(p.faint));
                    ui.add(egui::Label::new(RichText::new(file_name(f)).size(12.5)).truncate()).on_hover_text(f.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(I::X).on_hover_text("Remove").clicked() {
                            action = Some((i, 0));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new(I::ARROW_DOWN).small()).clicked() {
                            action = Some((i, 1));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new(I::ARROW_UP).small()).clicked() {
                            action = Some((i, -1));
                        }
                    });
                });
            }
        });
    });
    if let Some((i, d)) = action {
        changed = true;
        if d == 0 {
            files.remove(i);
        } else {
            let j = (i as i32 + d) as usize;
            files.swap(i, j);
        }
    }
    hint(ui, p, &format!("{} file{} in this order", files.len(), if files.len() == 1 { "" } else { "s" }));
    changed
}

fn default_dir(files: &[PathBuf]) -> Option<PathBuf> {
    files.first().and_then(|f| f.parent()).map(Path::to_path_buf)
}

fn default_name(files: &[PathBuf], fallback: &str) -> String {
    files.first().and_then(|f| f.parent()).and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| fallback.to_string())
}

fn window(title: String, ctx: &egui::Context, w: f32, h: f32) -> egui::Window<'static> {
    egui::Window::new(title).default_size(window_size(ctx, w, h)).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).min_size([520.0, 380.0]).resizable(true).collapsible(false)
}

// ============================================================ multi-page

pub struct MultiPageWindow {
    files: Vec<PathBuf>,
    s: MultiSettings,
    dir: Option<PathBuf>,
    name: String,
    job: Option<Job>,
    result: Option<(Result<String, String>, PathBuf)>,
    confirm: Option<PathBuf>,
}

impl MultiPageWindow {
    pub fn new(files: Vec<PathBuf>, _core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
        MultiPageWindow { dir: default_dir(&files), name: default_name(&files, "Images"), files, s: crate::ui::batch::convert::load_prefs().multi, job: None, result: None, confirm: None }
    }

    fn start(&mut self, ctx: &egui::Context, out: PathBuf) {
        let s = self.s.clone();
        crate::ui::batch::convert::save_prefs(|p| p.multi = s.clone());
        let files = self.files.clone();
        let o = out.clone();
        self.result = None;
        self.job = Some(spawn_job(ctx, out, move |prog| {
            mp::write_multi(&files, &s, &o, prog)?;
            let size = std::fs::metadata(&o).map(|m| m.len()).unwrap_or(0);
            Ok(format!("Created {} ({}, {} page{})", file_name(&o), human_size(size), files.len(), if files.len() == 1 { "" } else { "s" }))
        }));
    }

    /// Returns false when the window was closed.
    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        if let Polled::Done(r, out) = poll(&self.job) {
            self.job = None;
            if let Ok(m) = &r {
                core.toasts.success(m.clone());
            }
            self.result = Some((r, out));
        }
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut open = true;
        let mut close = false;
        let mut create = None;
        window(format!("{}  Create multi-page file", I::FILES), ctx, 760.0, 600.0).open(&mut open).show(ctx, |ui| {
            let target = {
                let ext = self.s.format.ext();
                // Computed inside the card below; peek with the current values here.
                self.dir.as_ref().filter(|_| !self.name.trim().is_empty()).map(|d| d.join(format!("{}.{ext}", crate::ui::batch::logic::sanitize_file_stem(self.name.trim()))))
            };
            let n = self.files.len();
            let idle = format!("{n} image{} -> one {}", if n == 1 { "" } else { "s" }, self.s.format.label());
            let (c, go) = footer(ui, &p, "mp-footer", &self.job, &self.result, &idle, &format!("{}  Create", I::FLOPPY_DISK), n > 0 && target.is_some());
            close = c;
            if go {
                create = target.clone();
            }
            egui::CentralPanel::no_frame().show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("mp-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_enabled_ui(self.job.is_none(), |ui| {
                        ui.columns(2, |cols| {
                            card(&mut cols[0], &p, I::IMAGES, "Images", |ui| {
                                file_list(ui, &p, &mut self.files, "mp-files");
                            });
                            self.settings(&mut cols[1], &p);
                        });
                    });
                });
            });
        });
        if let Some(out) = create {
            if out.exists() {
                self.confirm = Some(out);
            } else {
                self.start(ctx, out);
            }
        }
        if let Some(out) = self.confirm.clone() {
            match confirm_replace(ctx, &p, &out) {
                Some(true) => {
                    self.confirm = None;
                    self.start(ctx, out);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        let still = open && !close;
        if !still {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::SeqCst);
            }
            let s = self.s.clone();
            crate::ui::batch::convert::save_prefs(|p| p.multi = s);
        }
        still
    }

    fn settings(&mut self, ui: &mut Ui, p: &Palette) {
        let s = &mut self.s;
        card(ui, p, I::FILE_PDF, "Format", |ui| {
            for f in MultiFormat::ALL {
                ui.radio_value(&mut s.format, f, f.label());
            }
            ui.add_space(4.0);
            egui::Grid::new("mp-opts").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| match s.format {
                MultiFormat::Pdf => {
                    ui.label("Page size");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut s.pdf_page, PdfPage::A4, "A4");
                        ui.selectable_value(&mut s.pdf_page, PdfPage::Letter, "Letter");
                        ui.selectable_value(&mut s.pdf_page, PdfPage::Image, "Image size");
                    });
                    ui.end_row();
                    if s.pdf_page == PdfPage::Image {
                        ui.label("Resolution");
                        ui.add(egui::DragValue::new(&mut s.dpi).range(10..=1200).suffix(" dpi"));
                    } else {
                        ui.label("Margins");
                        ui.add(egui::DragValue::new(&mut s.margin_mm).range(0.0..=60.0).suffix(" mm"));
                    }
                    ui.end_row();
                    ui.label("JPEG quality");
                    ui.add(egui::Slider::new(&mut s.jpeg_quality, 30..=100));
                    ui.end_row();
                    ui.label("Max image side");
                    ui.add(egui::DragValue::new(&mut s.pdf_max_side).range(0..=20000).suffix(" px"))
                        .on_hover_text("Large photos are reduced to this size to keep the PDF small (0 = keep the full size)");
                    ui.end_row();
                }
                MultiFormat::Tiff => {
                    ui.label("Resolution");
                    ui.add(egui::DragValue::new(&mut s.dpi).range(10..=1200).suffix(" dpi"));
                    ui.end_row();
                    ui.label("");
                    hint(ui, p, "Lossless (LZW), one page per image, full size.");
                    ui.end_row();
                }
                MultiFormat::Gif | MultiFormat::WebP => {
                    ui.label("Frame time");
                    ui.add(egui::DragValue::new(&mut s.frame_ms).range(20..=60000).speed(10.0).suffix(" ms"));
                    ui.end_row();
                    ui.label("Max size");
                    ui.add(egui::DragValue::new(&mut s.anim_max_side).range(16..=4000).suffix(" px"));
                    ui.end_row();
                    if s.format == MultiFormat::WebP {
                        ui.label("Quality");
                        ui.add(egui::Slider::new(&mut s.webp_quality, 1.0..=100.0).fixed_decimals(0));
                        ui.end_row();
                    }
                    ui.label("Background");
                    ui.color_edit_button_srgb(&mut s.bg);
                    ui.end_row();
                    ui.label("");
                    hint(ui, p, "Loops forever. Every frame is fitted to the first image's shape.");
                    ui.end_row();
                }
            });
        });
        ui.add_space(8.0);
        let ext = self.s.format.ext();
        card(ui, p, I::FLOPPY_DISK, "Save as", |ui| {
            target_ui(ui, p, &mut self.dir, &mut self.name, ext);
        });
    }
}

// =================================================================== strip

pub struct StripWindow {
    files: Vec<PathBuf>,
    dims: Option<Vec<(u32, u32)>>,
    dims_rx: Option<Receiver<Vec<(u32, u32)>>>,
    s: StripSettings,
    last_s: StripSettings,
    version: u64,
    preview_version: u64,
    preview_rx: Option<Receiver<(u64, egui::ColorImage)>>,
    preview: Option<egui::TextureHandle>,
    job: Option<Job>,
    result: Option<(Result<String, String>, PathBuf)>,
    confirm: Option<PathBuf>,
}

fn probe_all(files: Vec<PathBuf>, ctx: egui::Context) -> Receiver<Vec<(u32, u32)>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        use rayon::prelude::*;
        let d: Vec<(u32, u32)> = files.par_iter().map(|f| std::panic::catch_unwind(|| crate::decode::probe_dimensions(f)).ok().flatten().unwrap_or((1, 1))).collect();
        let _ = tx.send(d);
        ctx.request_repaint();
    });
    rx
}

impl StripWindow {
    pub fn new(files: Vec<PathBuf>, core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
        let mut s = crate::ui::batch::convert::load_prefs().strip;
        if s.out_dir.as_ref().is_none_or(|d| !d.is_dir()) || !files.is_empty() {
            s.out_dir = default_dir(&files).or(s.out_dir);
        }
        StripWindow {
            dims: None,
            dims_rx: Some(probe_all(files.clone(), core.ctx.clone())),
            files,
            last_s: s.clone(),
            s,
            version: 1,
            preview_version: 0,
            preview_rx: None,
            preview: None,
            job: None,
            result: None,
            confirm: None,
        }
    }

    fn target(&self) -> Option<PathBuf> {
        let stem = crate::ui::batch::logic::sanitize_file_stem(self.s.name.trim());
        self.s.out_dir.as_ref().filter(|_| !self.s.name.trim().is_empty()).map(|d| d.join(format!("{stem}.{}", self.s.format.ext())))
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.dims_rx {
            match rx.try_recv() {
                Ok(d) => {
                    self.dims = Some(d);
                    self.dims_rx = None;
                    self.version += 1;
                }
                Err(TryRecvError::Disconnected) => {
                    self.dims = Some(vec![(1, 1); self.files.len()]);
                    self.dims_rx = None;
                }
                Err(_) => {}
            }
        }
        if self.s != self.last_s {
            self.last_s = self.s.clone();
            self.version += 1;
        }
        if let Some(rx) = &self.preview_rx {
            match rx.try_recv() {
                Ok((v, img)) => {
                    if v == self.version {
                        self.preview = Some(ctx.load_texture("strip-preview", img, egui::TextureOptions::LINEAR));
                    }
                    self.preview_version = v;
                    self.preview_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.preview_version = self.version;
                    self.preview_rx = None;
                }
                Err(_) => {}
            }
        }
        if let (Some(dims), None) = (&self.dims, &self.preview_rx) {
            if self.preview_version < self.version && !self.files.is_empty() && !ctx.egui_is_using_pointer() {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (files, dims, s, v, c) = (self.files.clone(), dims.clone(), self.s.clone(), self.version, ctx.clone());
                std::thread::spawn(move || {
                    let full = mp::strip_layout(&dims, &s);
                    // Preview at reduced scale from thumbnails.
                    let f = (1200.0 / full.width.max(full.height) as f64).min(1.0);
                    let small = StripSettings { custom: ((s.custom as f64 * f).round() as u32).max(1), spacing: (s.spacing as f64 * f).round() as u32, margin: (s.margin as f64 * f).round() as u32, ..s.clone() };
                    let sdims: Vec<(u32, u32)> = dims.iter().map(|&(w, h)| (((w as f64 * f).round() as u32).max(1), ((h as f64 * f).round() as u32).max(1))).collect();
                    let l = mp::strip_layout(&sdims, &small);
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        mp::render_strip(&l, s.bg, &OutProgress::default(), |i| {
                            let (_, _, w, h) = l.cells[i];
                            Ok(crate::decode::decode_thumbnail(&files[i], w.max(h).max(16)).unwrap_or_else(|_| image::RgbaImage::from_pixel(1, 1, image::Rgba([200, 200, 200, 255]))))
                        })
                    }));
                    if let Ok(Ok(img)) = r {
                        let _ = tx.send((v, egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())));
                    }
                    c.request_repaint();
                });
                self.preview_rx = Some(rx);
            }
        }
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn start(&mut self, ctx: &egui::Context, out: PathBuf) {
        let Some(dims) = self.dims.clone() else { return };
        let s = self.s.clone();
        crate::ui::batch::convert::save_prefs(|p| p.strip = s.clone());
        let files = self.files.clone();
        let o = out.clone();
        self.result = None;
        self.job = Some(spawn_job(ctx, out, move |prog| {
            mp::write_strip(&files, &dims, &s, &o, prog)?;
            let l = mp::strip_layout(&dims, &s);
            Ok(format!("Created {} ({} x {} px)", file_name(&o), l.width, l.height))
        }));
    }

    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        self.poll(ctx);
        if let Polled::Done(r, out) = poll(&self.job) {
            self.job = None;
            if let Ok(m) = &r {
                core.toasts.success(m.clone());
            }
            self.result = Some((r, out));
        }
        let layout = self.dims.as_ref().map(|d| mp::strip_layout(d, &self.s));
        let limit = layout.as_ref().and_then(|l| mp::strip_limit_error(l, self.s.format));
        let target = self.target();
        let mut open = true;
        let mut close = false;
        let mut create = None;
        window(format!("{}  Image strip", I::COLUMNS), ctx, 960.0, 640.0).open(&mut open).show(ctx, |ui| {
            let idle = match &layout {
                Some(l) => format!("{} images  -  {} x {} px", self.files.len(), l.width, l.height),
                None => "Reading image sizes...".into(),
            };
            let ok = !self.files.is_empty() && layout.is_some() && limit.is_none() && target.is_some();
            let (c, go) = footer(ui, &p, "st-footer", &self.job, &self.result, &idle, &format!("{}  Create strip", I::FLOPPY_DISK), ok);
            close = c;
            if go {
                create = target.clone();
            }
            egui::Panel::left("st-settings").resizable(false).exact_size(330.0).frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 10, top: 0, bottom: 0 })).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("st-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_enabled_ui(self.job.is_none(), |ui| {
                        card(ui, &p, I::IMAGES, "Images", |ui| {
                            if file_list(ui, &p, &mut self.files, "st-files") {
                                self.dims = None;
                                self.dims_rx = Some(probe_all(self.files.clone(), ui.ctx().clone()));
                            }
                        });
                        ui.add_space(8.0);
                        self.settings(ui, &p, limit.as_deref());
                    });
                });
            });
            egui::CentralPanel::no_frame().show(ui, |ui| {
                egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("PREVIEW").size(11.0).color(p.faint).strong());
                        if self.preview_version < self.version {
                            ui.spinner();
                        }
                    });
                    if let Some(tex) = &self.preview {
                        let size = tex.size_vec2();
                        let room = ui.available_size() - Vec2::new(0.0, 4.0);
                        let s = (room.x / size.x).min(room.y / size.y).min(1.0);
                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Image::new((tex.id(), size * s)));
                        });
                    }
                });
            });
        });
        if let Some(out) = create {
            if out.exists() {
                self.confirm = Some(out);
            } else {
                self.start(ctx, out);
            }
        }
        if let Some(out) = self.confirm.clone() {
            match confirm_replace(ctx, &p, &out) {
                Some(true) => {
                    self.confirm = None;
                    self.start(ctx, out);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        let still = open && !close;
        if !still {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::SeqCst);
            }
            let s = self.s.clone();
            crate::ui::batch::convert::save_prefs(|p| p.strip = s);
        }
        still
    }

    fn settings(&mut self, ui: &mut Ui, p: &Palette, limit: Option<&str>) {
        let s = &mut self.s;
        card(ui, p, I::COLUMNS, "Layout", |ui| {
            egui::Grid::new("st-layout").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Direction");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.dir, StripDir::Horizontal, "Horizontal");
                    ui.selectable_value(&mut s.dir, StripDir::Vertical, "Vertical");
                });
                ui.end_row();
                ui.label(if s.dir == StripDir::Horizontal { "Height" } else { "Width" });
                egui::ComboBox::from_id_salt("st-size")
                    .selected_text(match s.size {
                        StripSize::Original => "Keep original sizes",
                        StripSize::Smallest => "Same as smallest",
                        StripSize::Largest => "Same as largest",
                        StripSize::Custom => "Custom",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.size, StripSize::Smallest, "Same as smallest");
                        ui.selectable_value(&mut s.size, StripSize::Largest, "Same as largest");
                        ui.selectable_value(&mut s.size, StripSize::Custom, "Custom");
                        ui.selectable_value(&mut s.size, StripSize::Original, "Keep original sizes");
                    });
                ui.end_row();
                if s.size == StripSize::Custom {
                    ui.label("");
                    ui.add(egui::DragValue::new(&mut s.custom).range(8..=30000).suffix(" px"));
                    ui.end_row();
                }
                ui.label("Spacing");
                ui.add(egui::DragValue::new(&mut s.spacing).range(0..=2000).suffix(" px"));
                ui.end_row();
                ui.label("Outer margin");
                ui.add(egui::DragValue::new(&mut s.margin).range(0..=2000).suffix(" px"));
                ui.end_row();
                ui.label("Background");
                color_rgba(ui, &mut s.bg, true);
                ui.end_row();
            });
            if let Some(e) = limit {
                ui.label(RichText::new(format!("{}  {e}", I::WARNING)).size(11.5).color(p.danger));
            }
        });
        ui.add_space(8.0);
        card(ui, p, I::FLOPPY_DISK, "Save as", |ui| {
            egui::Grid::new("st-fmt").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Format");
                egui::ComboBox::from_id_salt("st-format").selected_text(if s.format == SaveFormat::WebP { "WebP" } else { s.format.label() }).show_ui(ui, |ui| {
                    for f in [SaveFormat::Jpeg, SaveFormat::Png, SaveFormat::WebP, SaveFormat::Tiff, SaveFormat::Bmp] {
                        ui.selectable_value(&mut s.format, f, if f == SaveFormat::WebP { "WebP" } else { f.label() });
                    }
                });
                ui.end_row();
                if s.format == SaveFormat::Jpeg {
                    ui.label("Quality");
                    ui.add(egui::Slider::new(&mut s.jpeg_quality, 40..=100));
                    ui.end_row();
                }
            });
            let ext = s.format.ext();
            target_ui(ui, p, &mut s.out_dir, &mut s.name, ext);
        });
    }
}

// ===================================================================== zip

pub struct ZipWindow {
    files: Vec<PathBuf>,
    total: Option<u64>,
    total_rx: Option<Receiver<u64>>,
    s: ZipSettings,
    dir: Option<PathBuf>,
    name: String,
    job: Option<Job>,
    result: Option<(Result<String, String>, PathBuf)>,
    confirm: Option<PathBuf>,
}

impl ZipWindow {
    /// Any files (not only images) can be archived.
    pub fn new(files: Vec<PathBuf>, core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| p.is_file()).collect();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (list, c) = (files.clone(), core.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(list.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum());
            c.request_repaint();
        });
        ZipWindow { dir: default_dir(&files), name: default_name(&files, "Archive"), files, total: None, total_rx: Some(rx), s: crate::ui::batch::convert::load_prefs().zip, job: None, result: None, confirm: None }
    }

    fn start(&mut self, ctx: &egui::Context, out: PathBuf) {
        let s = self.s.clone();
        crate::ui::batch::convert::save_prefs(|p| p.zip = s.clone());
        // Never archive the archive itself.
        let files: Vec<PathBuf> = self.files.iter().filter(|f| !crate::ui::batch::convert::same_dir(f, &out)).cloned().collect();
        let o = out.clone();
        self.result = None;
        self.job = Some(spawn_job(ctx, out, move |prog| {
            let size = mp::write_zip(&files, &s, &o, prog)?;
            Ok(format!("Created {} ({} files, {})", file_name(&o), files.len(), human_size(size)))
        }));
    }

    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        if let Some(rx) = &self.total_rx {
            match rx.try_recv() {
                Ok(t) => {
                    self.total = Some(t);
                    self.total_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.total_rx = None,
                Err(_) => {}
            }
        }
        if let Polled::Done(r, out) = poll(&self.job) {
            self.job = None;
            if let Ok(m) = &r {
                core.toasts.success(m.clone());
            }
            self.result = Some((r, out));
        }
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut open = true;
        let mut close = false;
        let mut create = None;
        window(format!("{}  Create ZIP archive", I::FILE_ZIP), ctx, 640.0, 520.0).open(&mut open).show(ctx, |ui| {
            let target = self.dir.as_ref().filter(|_| !self.name.trim().is_empty()).map(|d| d.join(format!("{}.zip", crate::ui::batch::logic::sanitize_file_stem(self.name.trim()))));
            let n = self.files.len();
            let idle = format!("{n} file{}{}", if n == 1 { "" } else { "s" }, self.total.map(|t| format!(", {}", human_size(t))).unwrap_or_default());
            let (c, go) = footer(ui, &p, "zip-footer", &self.job, &self.result, &idle, &format!("{}  Create ZIP", I::FILE_ZIP), n > 0 && target.is_some());
            close = c;
            if go {
                create = target;
            }
            egui::CentralPanel::no_frame().show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("zip-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_enabled_ui(self.job.is_none(), |ui| {
                        card(ui, &p, I::FILES, "Files", |ui| {
                            if file_list(ui, &p, &mut self.files, "zip-files") {
                                self.total = None;
                            }
                        });
                        ui.add_space(8.0);
                        card(ui, &p, I::FILE_ZIP, "Archive", |ui| {
                            ui.checkbox(&mut self.s.store_compressed, "Store JPEG, PNG, video... without recompressing (much faster)");
                            ui.checkbox(&mut self.s.keep_folders, "Keep the folder structure");
                            ui.add_space(4.0);
                            target_ui(ui, &p, &mut self.dir, &mut self.name, "zip");
                        });
                    });
                });
            });
        });
        if let Some(out) = create {
            if out.exists() {
                self.confirm = Some(out);
            } else {
                self.start(ctx, out);
            }
        }
        if let Some(out) = self.confirm.clone() {
            match confirm_replace(ctx, &p, &out) {
                Some(true) => {
                    self.confirm = None;
                    self.start(ctx, out);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        let still = open && !close;
        if !still {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::SeqCst);
            }
            let s = self.s.clone();
            crate::ui::batch::convert::save_prefs(|p| p.zip = s);
        }
        still
    }
}
