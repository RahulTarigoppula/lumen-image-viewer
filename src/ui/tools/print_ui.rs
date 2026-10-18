//! The Print options window: sizing, margins, copies and images per page,
//! with a live page preview. The system print dialog opens only when the user
//! presses Print; pages are then rendered and spooled on a background thread.

use super::print::{self, Orientation, PageInfo, PrintOptions, PrintProgress, PrintSource, Sizing, Unit, PER_PAGE_CHOICES};
use crate::app::Core;
use crate::ui::batch::{card, danger_button, hint, primary_button, window_size};
use crate::ui::icons as I;
use crate::ui::theme::Palette;
use crossbeam_channel::Receiver;
use egui::{Color32, CornerRadius, Rect, RichText, Ui, Vec2};
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

struct Job {
    progress: Arc<PrintProgress>,
    rx: Receiver<Result<(), String>>,
}

struct SourceInfo {
    dims: (u32, u32),
    dpi: Option<f32>,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

pub struct PrintWindow {
    sources: Vec<PrintSource>,
    infos: Option<Vec<SourceInfo>>,
    infos_rx: Option<Receiver<Vec<SourceInfo>>>,
    opts: PrintOptions,
    page: usize,
    job: Option<Job>,
    error: Option<String>,
    title: String,
    /// Texture for an in-memory image source.
    mem_tex: Option<egui::TextureHandle>,
}

impl PrintWindow {
    /// Print one or more image files.
    pub fn new(files: Vec<PathBuf>, core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
        let title = match files.len() {
            1 => crate::ui::batch::file_name(&files[0]),
            n => format!("{n} images"),
        };
        Self::build(files.into_iter().map(PrintSource::File).collect(), title, core)
    }

    /// Print an image that is already in memory (e.g. the edited image in the viewer).
    pub fn with_image(img: Arc<RgbaImage>, name: String, core: &Core) -> Self {
        Self::build(vec![PrintSource::Image { img, name: name.clone() }], name, core)
    }

    fn build(sources: Vec<PrintSource>, title: String, core: &Core) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (list, ctx) = (sources.clone(), core.ctx.clone());
        std::thread::spawn(move || {
            let infos: Vec<SourceInfo> = list
                .iter()
                .map(|s| {
                    let meta = if let PrintSource::File(p) = s { std::fs::metadata(p).ok() } else { None };
                    let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (s.dims(), s.dpi())));
                    let (dims, dpi) = probe.unwrap_or((None, None));
                    SourceInfo { dims: dims.unwrap_or((1, 1)), dpi, size: meta.as_ref().map(|m| m.len()).unwrap_or(0), modified: meta.and_then(|m| m.modified().ok()) }
                })
                .collect();
            let _ = tx.send(infos);
            ctx.request_repaint();
        });
        PrintWindow {
            sources,
            infos: None,
            infos_rx: Some(rx),
            opts: crate::ui::batch::convert::load_prefs().print,
            page: 0,
            job: None,
            error: None,
            title,
            mem_tex: None,
        }
    }

    fn landscape(&self) -> bool {
        let dims: Vec<(u32, u32)> = self.infos.as_ref().map(|v| v.iter().map(|i| i.dims).collect()).unwrap_or_default();
        print::choose_landscape(&self.opts, &dims)
    }

    fn start(&mut self, ctx: &egui::Context) {
        let o = self.opts.clone();
        crate::ui::batch::convert::save_prefs(|p| p.print = o);
        self.error = None;
        // The system dialog is modal by nature; everything after it runs on a worker.
        match print::choose_printer(self.landscape(), self.opts.copies) {
            Ok(None) => {}
            Err(e) => self.error = Some(e.to_string()),
            Ok(Some(printer)) => {
                let progress = Arc::new(PrintProgress::default());
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (sources, opts, title, prog, c) = (self.sources.clone(), self.opts.clone(), self.title.clone(), progress.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| print::run_job(printer, &sources, &opts, &title, &prog).map_err(|e| e.to_string())));
                    let r = r.unwrap_or_else(|e| Err(format!("Printing failed unexpectedly: {}", crate::ui::batch::convert::panic_message(&*e))));
                    prog.finished.store(true, Ordering::SeqCst);
                    let _ = tx.send(r);
                    c.request_repaint();
                });
                self.job = Some(Job { progress, rx });
            }
        }
    }

    /// Returns false when the window should close.
    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        if let Some(rx) = &self.infos_rx {
            match rx.try_recv() {
                Ok(v) => {
                    self.infos = Some(v);
                    self.infos_rx = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.infos = Some(self.sources.iter().map(|_| SourceInfo { dims: (1, 1), dpi: None, size: 0, modified: None }).collect());
                    self.infos_rx = None;
                }
                Err(_) => {}
            }
        }
        let mut finished_ok = false;
        if let Some(job) = &self.job {
            let r = match job.rx.try_recv() {
                Ok(r) => Some(r),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err("The print job stopped unexpectedly".into())),
                Err(_) => None,
            };
            if let Some(r) = r {
                let cancelled = job.progress.cancel.load(Ordering::Relaxed);
                self.job = None;
                match r {
                    Ok(()) if cancelled => core.toasts.info("Printing cancelled".to_string()),
                    Ok(()) => {
                        core.toasts.success(format!("Sent {} to the printer", self.title));
                        finished_ok = true;
                    }
                    Err(e) => {
                        core.toasts.error(e.clone());
                        self.error = Some(e);
                    }
                }
            }
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new(format!("{}  Print", I::PRINTER))
            .open(&mut open)
            .default_size(window_size(ctx, 820.0, 600.0)).max_size(window_size(ctx, 4000.0, 4000.0)).constrain(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .min_size(crate::ui::batch::window_size(ctx, 560.0, 420.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                close = self.ui(ui, &p, core);
            });
        let still_open = open && !close && !finished_ok;
        if !still_open {
            if let Some(job) = &self.job {
                job.progress.cancel.store(true, Ordering::SeqCst);
            }
            let o = self.opts.clone();
            crate::ui::batch::convert::save_prefs(|p| p.print = o);
        }
        still_open
    }

    fn ui(&mut self, ui: &mut Ui, p: &Palette, core: &mut Core) -> bool {
        let mut close = false;
        let pages = print::page_count(self.sources.len(), self.opts.per_page);
        let mut start = false;
        egui::Panel::bottom("pr-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            if let Some(job) = &self.job {
                let (d, t) = (job.progress.pages_done.load(Ordering::Relaxed), job.progress.pages_total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t == 0 { 0.0 } else { d as f32 / t as f32 }).fill(p.accent).desired_height(6.0));
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                if let Some(job) = &self.job {
                    ui.spinner();
                    let (d, t) = (job.progress.pages_done.load(Ordering::Relaxed), job.progress.pages_total.load(Ordering::Relaxed));
                    ui.label(RichText::new(format!("Printing page {} of {}...", (d + 1).min(t.max(1)), t.max(1))).color(p.muted));
                } else if let Some(e) = &self.error {
                    ui.label(RichText::new(I::WARNING_CIRCLE).color(p.danger));
                    ui.add(egui::Label::new(RichText::new(e).color(p.danger)).truncate());
                } else {
                    let n = self.sources.len();
                    ui.label(RichText::new(format!("{n} image{}  -  {pages} page{}", if n == 1 { "" } else { "s" }, if pages == 1 { "" } else { "s" })).color(p.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(job) = &self.job {
                        if danger_button(ui, p, &format!("{}  Cancel", I::STOP)).clicked() {
                            job.progress.cancel.store(true, Ordering::SeqCst);
                        }
                    } else {
                        if primary_button(ui, p, !self.sources.is_empty() && self.infos.is_some(), &format!("{}  Print...", I::PRINTER)).clicked() {
                            start = true;
                        }
                        if ui.add(egui::Button::new("Close").min_size(Vec2::new(80.0, 30.0))).clicked() {
                            close = true;
                        }
                    }
                });
            });
        });
        let busy = self.job.is_some();
        egui::Panel::left("pr-settings").resizable(false).exact_size(320.0).frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 10, top: 0, bottom: 0 })).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("pr-scroll").auto_shrink([false, false]).show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| self.settings(ui, p));
            });
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("PREVIEW (A4) - PAGE {} OF {pages}", self.page.min(pages - 1) + 1)).size(11.0).color(p.faint).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(self.page + 1 < pages, egui::Button::new(I::CARET_RIGHT).small()).clicked() {
                            self.page += 1;
                        }
                        if ui.add_enabled(self.page > 0, egui::Button::new(I::CARET_LEFT).small()).clicked() {
                            self.page -= 1;
                        }
                    });
                });
                self.page = self.page.min(pages - 1);
                self.preview(ui, p, core);
            });
        });
        if start {
            self.start(ui.ctx());
        }
        close
    }

    fn settings(&mut self, ui: &mut Ui, p: &Palette) {
        let o = &mut self.opts;
        card(ui, p, I::ARROWS_OUT_SIMPLE, "Size", |ui| {
            for s in Sizing::ALL {
                ui.radio_value(&mut o.sizing, s, s.label());
            }
            match o.sizing {
                Sizing::Custom => {
                    ui.horizontal(|ui| {
                        let speed = if o.unit == Unit::Cm { 0.1 } else { 0.05 };
                        ui.add(egui::DragValue::new(&mut o.custom_w).range(0.5..=200.0).speed(speed).max_decimals(2));
                        ui.label("x");
                        ui.add(egui::DragValue::new(&mut o.custom_h).range(0.5..=200.0).speed(speed).max_decimals(2));
                        egui::ComboBox::from_id_salt("pr-unit").width(60.0).selected_text(o.unit.label()).show_ui(ui, |ui| {
                            for u in [Unit::Cm, Unit::Inch] {
                                ui.selectable_value(&mut o.unit, u, u.label());
                            }
                        });
                    });
                    hint(ui, p, "The image is fitted inside this box, keeping its shape.");
                }
                Sizing::Actual => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("When a file has no DPI, use").color(p.muted));
                        ui.add(egui::DragValue::new(&mut o.assume_dpi).range(10.0..=2400.0).suffix(" dpi"));
                    });
                }
                _ => {}
            }
            ui.checkbox(&mut o.center, "Center on the page");
        });
        ui.add_space(8.0);
        card(ui, p, I::GRID_NINE, "Layout", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Images per page").color(p.muted));
                for n in PER_PAGE_CHOICES {
                    ui.selectable_value(&mut o.per_page, n, n.to_string());
                }
            });
            egui::Grid::new("pr-layout").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Orientation");
                egui::ComboBox::from_id_salt("pr-orient")
                    .selected_text(match o.orientation {
                        Orientation::Auto => "Automatic",
                        Orientation::Portrait => "Portrait",
                        Orientation::Landscape => "Landscape",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut o.orientation, Orientation::Auto, "Automatic");
                        ui.selectable_value(&mut o.orientation, Orientation::Portrait, "Portrait");
                        ui.selectable_value(&mut o.orientation, Orientation::Landscape, "Landscape");
                    });
                ui.end_row();
                ui.label("Margins");
                ui.add(egui::DragValue::new(&mut o.margin_mm).range(0.0..=60.0).speed(0.5).suffix(" mm"));
                ui.end_row();
                if o.per_page > 1 {
                    ui.label("Spacing");
                    ui.add(egui::DragValue::new(&mut o.gap_mm).range(0.0..=60.0).speed(0.5).suffix(" mm"));
                    ui.end_row();
                }
                ui.label("Copies");
                ui.add(egui::DragValue::new(&mut o.copies).range(1..=999));
                ui.end_row();
            });
            hint(ui, p, "Printer, paper size and quality are chosen in the next dialog.");
        });
    }

    fn preview(&mut self, ui: &mut Ui, p: &Palette, core: &mut Core) {
        let Some(infos) = &self.infos else {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        };
        let landscape = self.landscape();
        let (pw, ph) = if landscape { (11.69, 8.27) } else { (8.27, 11.69) };
        let page = PageInfo::paper(pw, ph, 100.0);
        let per = self.opts.per_page.max(1) as usize;
        let start = self.page * per;
        let end = (start + per).min(self.sources.len());
        let dims: Vec<((u32, u32), Option<f32>)> = infos[start..end].iter().map(|i| (i.dims, i.dpi)).collect();
        let placed = print::layout_page(&page, &self.opts, &dims, landscape);
        let room = ui.available_size() - Vec2::new(0.0, 6.0);
        let s = (room.x / page.paper_w as f32).min(room.y / page.paper_h as f32).max(0.01);
        let size = Vec2::new(page.paper_w as f32 * s, page.paper_h as f32 * s);
        let (outer, _) = ui.allocate_exact_size(room, egui::Sense::hover());
        let rect = Rect::from_center_size(outer.center(), size);
        let painter = ui.painter_at(outer);
        painter.rect_filled(rect.translate(Vec2::new(3.0, 4.0)), CornerRadius::same(2), Color32::from_black_alpha(50));
        painter.rect_filled(rect, CornerRadius::same(2), Color32::WHITE);
        // Margin guide.
        let m = self.opts.margin_mm / 25.4 * 100.0 * s;
        painter.rect_stroke(rect.shrink(m), CornerRadius::ZERO, egui::Stroke::new(1.0, Color32::from_gray(225)), egui::StrokeKind::Inside);
        if self.mem_tex.is_none() {
            if let Some(PrintSource::Image { img, .. }) = self.sources.first() {
                let small = crate::imaging::resize_to_fit(img, 1024, 1024);
                let ci = egui::ColorImage::from_rgba_unmultiplied([small.width() as usize, small.height() as usize], small.as_raw());
                self.mem_tex = Some(ui.ctx().load_texture("print-preview", ci, egui::TextureOptions::LINEAR));
            }
        }
        for (k, pl) in placed.iter().enumerate() {
            let i = start + k;
            let r = Rect::from_min_size(rect.min + Vec2::new(pl.dst.x as f32 * s, pl.dst.y as f32 * s), Vec2::new(pl.dst.w as f32 * s, pl.dst.h as f32 * s));
            let (iw, ih) = infos[i].dims;
            let (sx, sy, sw, sh) = pl.src;
            let uv = Rect::from_min_max(egui::pos2(sx as f32 / iw as f32, sy as f32 / ih as f32), egui::pos2((sx + sw) as f32 / iw as f32, (sy + sh) as f32 / ih as f32));
            let tex = match &self.sources[i] {
                PrintSource::Image { .. } => self.mem_tex.as_ref().map(|t| t.id()),
                PrintSource::File(path) => {
                    let key = crate::thumbs::ThumbKey::new(path, infos[i].size, infos[i].modified);
                    match core.thumbs.get(&key) {
                        crate::thumbs::Thumb::Ready(info) => Some(info.tex.id()),
                        _ => None,
                    }
                }
            };
            match tex {
                Some(id) => {
                    painter.image(id, r, uv, Color32::WHITE);
                }
                None => {
                    painter.rect_filled(r, CornerRadius::ZERO, Color32::from_gray(228));
                    painter.text(r.center(), egui::Align2::CENTER_CENTER, I::IMAGE, egui::FontId::proportional(18.0), Color32::from_gray(160));
                }
            }
        }
        if placed.is_empty() {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Nothing to print", egui::FontId::proportional(14.0), p.muted);
        }
    }
}
