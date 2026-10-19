//! Utility windows: contact sheet, duplicate finder and printing.

pub mod dupes;
pub mod multipage;
pub mod output_ui;
pub mod pdf;
pub mod print;
pub mod print_ui;
pub mod sheet;

pub use output_ui::{MultiPageWindow, StripWindow, ZipWindow};
pub use print_ui::PrintWindow;

use self::dupes::{Group, GroupKind, KeepRule, ScanItem};
use self::sheet::{SheetItem, SheetSettings};
use crate::app::Core;
use crate::catalog::human_size;
use crate::formats::SaveFormat;
use crate::ui::batch::{card, danger_button, empty_state, file_name, footer_row, hint, primary_button, window_body, window_size};
use crate::ui::icons as I;
use crate::ui::theme::Palette;
use crossbeam_channel::Receiver;
use egui::{Color32, CornerRadius, Rect, RichText, Sense, Ui, Vec2};
use image::RgbaImage;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Shared progress counters for background jobs.
#[derive(Default)]
struct Progress {
    phase: Mutex<String>,
    done: AtomicUsize,
    total: AtomicUsize,
    cancel: AtomicBool,
}

impl Progress {
    fn set_phase(&self, phase: &str, total: usize) {
        *self.phase.lock() = phase.to_string();
        self.done.store(0, Ordering::SeqCst);
        self.total.store(total, Ordering::SeqCst);
    }
    fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 {
            0.0
        } else {
            self.done.load(Ordering::Relaxed) as f32 / t as f32
        }
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

fn job_pool() -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new().num_threads(crate::ui::batch::convert::worker_threads()).build().ok()
}

// ============================================================ contact sheet

type ThumbCache = Arc<Mutex<HashMap<(PathBuf, u32, u32, bool), Arc<RgbaImage>>>>;

/// Longest side of the contact sheet preview image.
const PREVIEW_MAX: f32 = 1400.0;

/// Run `f`, turning a panic into an error message so a background job always reports back.
pub(crate) fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into());
            Err(format!("{what} failed unexpectedly: {msg}"))
        }
    }
}

struct SheetJob {
    progress: Arc<Progress>,
    rx: Receiver<Result<Vec<PathBuf>, String>>,
    result: Option<Result<Vec<PathBuf>, String>>,
}

pub struct ContactSheet {
    pub files: Vec<PathBuf>,
    s: SheetSettings,
    last_s: SheetSettings,
    items: Option<Arc<Vec<SheetItem>>>,
    items_rx: Option<Receiver<Vec<SheetItem>>>,
    cache: ThumbCache,
    version: u64,
    preview_version: u64,
    preview_rx: Option<Receiver<(u64, egui::ColorImage, (u32, u32))>>,
    preview: Option<(egui::TextureHandle, (u32, u32))>,
    job: Option<SheetJob>,
}

fn cached_thumb(cache: &ThumbCache, path: &Path, s: &SheetSettings) -> Option<Arc<RgbaImage>> {
    let key = (path.to_path_buf(), s.cell_w, s.cell_h, s.crop_fill);
    if let Some(t) = cache.lock().get(&key) {
        return Some(t.clone());
    }
    // A decoder panic on one file leaves an empty cell instead of failing the sheet.
    let t = Arc::new(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sheet::cell_thumb(path, s))).ok().flatten()?);
    let mut c = cache.lock();
    if c.len() > 400 {
        c.clear();
    }
    c.insert(key, t.clone());
    Some(t)
}

impl ContactSheet {
    pub fn new(files: Vec<PathBuf>, core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
        let mut s = crate::ui::batch::convert::load_prefs().sheet;
        let folder = files.first().and_then(|f| f.parent()).map(Path::to_path_buf);
        s.title = folder.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if s.out_dir.as_ref().is_none_or(|d| !d.is_dir()) || folder.is_some() {
            s.out_dir = folder.or(s.out_dir);
        }
        // Gather dimensions/dates on a background thread.
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (list, ctx) = (files.clone(), core.ctx.clone());
        std::thread::spawn(move || {
            use rayon::prelude::*;
            let items: Vec<SheetItem> = list
                .par_iter()
                .map(|p| std::panic::catch_unwind(|| SheetItem::from_path(p)).unwrap_or_else(|_| SheetItem { path: p.clone(), dims: None, size: 0, modified: None }))
                .collect();
            let _ = tx.send(items);
            ctx.request_repaint();
        });
        ContactSheet {
            files,
            last_s: s.clone(),
            s,
            items: None,
            items_rx: Some(rx),
            cache: Default::default(),
            version: 1,
            preview_version: 0,
            preview_rx: None,
            preview: None,
            job: None,
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.items_rx {
            match rx.try_recv() {
                Ok(v) => {
                    self.items = Some(Arc::new(v));
                    self.items_rx = None;
                    self.version += 1;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // The info thread died: fall back to bare paths.
                    self.items = Some(Arc::new(self.files.iter().map(|p| SheetItem { path: p.clone(), dims: None, size: 0, modified: None }).collect()));
                    self.items_rx = None;
                    self.version += 1;
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
                Ok((v, img, dims)) => {
                    if v == self.version {
                        let tex = ctx.load_texture("contact-sheet-preview", img, egui::TextureOptions::LINEAR);
                        self.preview = Some((tex, dims));
                    }
                    self.preview_version = v;
                    self.preview_rx = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.preview_version = self.version;
                    self.preview_rx = None;
                }
                Err(_) => {}
            }
        }
        if let (Some(items), None) = (&self.items, &self.preview_rx) {
            if self.preview_version < self.version && !items.is_empty() && !ctx.egui_is_using_pointer() {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (s, v, items, cache, c) = (self.s.clone(), self.version, items.clone(), self.cache.clone(), ctx.clone());
                std::thread::spawn(move || {
                    use rayon::prelude::*;
                    let full = sheet::layout(&s, items.len());
                    // Render the preview at a reduced scale: a full-size page can be gigabytes.
                    let f = (PREVIEW_MAX / full.width.max(full.height).max(1) as f32).min(1.0);
                    let ps = s.scaled(f);
                    let l = sheet::layout(&ps, items.len());
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let page: Vec<SheetItem> = items.iter().take(l.per_page).cloned().collect();
                        let thumbs: Vec<Option<RgbaImage>> = page.par_iter().map(|it| cached_thumb(&cache, &it.path, &ps).map(|t| (*t).clone())).collect();
                        let img = sheet::render_page(&ps, &l, &page, &thumbs, 0);
                        egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
                    }));
                    let ci = r.unwrap_or_else(|_| egui::ColorImage::filled([1, 1], Color32::TRANSPARENT));
                    let _ = tx.send((v, ci, (full.width, full.height)));
                    c.request_repaint();
                });
                self.preview_rx = Some(rx);
            }
        }
        if let Some(job) = &mut self.job {
            if job.result.is_none() {
                match job.rx.try_recv() {
                    Ok(r) => job.result = Some(r),
                    Err(crossbeam_channel::TryRecvError::Disconnected) => job.result = Some(Err("The contact sheet job stopped unexpectedly".into())),
                    Err(_) => {}
                }
            }
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(items) = self.items.clone() else { return };
        let Some(dir) = self.s.out_dir.clone() else { return };
        let s = self.s.clone();
        crate::ui::batch::convert::save_prefs(|p| p.sheet = s.clone());
        let progress = Arc::new(Progress::default());
        progress.set_phase("Creating contact sheet", items.len());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (prog, cache, c) = (progress.clone(), self.cache.clone(), ctx.clone());
        std::thread::spawn(move || {
            use rayon::prelude::*;
            let run = || -> Result<Vec<PathBuf>, String> {
                let l = sheet::layout(&s, items.len());
                if let Some(e) = sheet::page_limit_error(s.format, &l) {
                    return Err(e);
                }
                std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
                let paths = sheet::page_paths(&s, &dir, l.pages);
                let pool = job_pool();
                let mut written = Vec::new();
                for (page, target) in paths.into_iter().enumerate() {
                    if prog.cancelled() {
                        break;
                    }
                    let start = page * l.per_page;
                    let page_items: Vec<SheetItem> = items.iter().skip(start).take(l.per_page).cloned().collect();
                    let decode = || -> Vec<Option<RgbaImage>> {
                        page_items
                            .par_iter()
                            .map(|it| {
                                if prog.cancelled() {
                                    return None;
                                }
                                let t = cached_thumb(&cache, &it.path, &s).map(|t| (*t).clone());
                                prog.done.fetch_add(1, Ordering::Relaxed);
                                c.request_repaint();
                                t
                            })
                            .collect()
                    };
                    let thumbs = match &pool {
                        Some(p) => p.install(decode),
                        None => decode(),
                    };
                    if prog.cancelled() {
                        break;
                    }
                    let img = sheet::render_page(&s, &l, &page_items, &thumbs, page);
                    let opts = crate::encode::SaveOptions { format: s.format, jpeg_quality: s.jpeg_quality, keep_metadata: false, ..Default::default() };
                    let bytes = crate::encode::encode(&img, &opts).map_err(|e| e.to_string())?;
                    let out = crate::fileops::unique_path(&target);
                    crate::encode::write_atomic(&out, &bytes).map_err(|e| e.to_string())?;
                    written.push(out);
                }
                Ok(written)
            };
            let _ = tx.send(guarded("Contact sheet", run));
            c.request_repaint();
        });
        self.job = Some(SheetJob { progress, rx, result: None });
    }

    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> bool {
        let p = core.pal;
        self.poll(ctx);
        let mut open = true;
        let mut close = false;
        egui::Window::new(format!("{}  Contact sheet", I::GRID_NINE))
            .open(&mut open)
            .default_size(window_size(ctx, 980.0, 680.0)).max_size(window_size(ctx, 4000.0, 4000.0)).constrain(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .min_size(crate::ui::batch::window_size(ctx, 600.0, 400.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                close = window_body(ui, |ui| self.ui(ui, &p));
            });
        let still_open = open && !close;
        if !still_open {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::SeqCst);
            }
            let s = self.s.clone();
            crate::ui::batch::convert::save_prefs(|p| p.sheet = s);
        }
        still_open
    }

    fn ui(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        let mut close = false;
        let n = self.files.len();
        let l = sheet::layout(&self.s, n);
        let running = self.job.as_ref().is_some_and(|j| j.result.is_none());
        let mut start = false;
        egui::Panel::bottom("cs-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            if let Some(job) = &self.job {
                if job.result.is_none() {
                    ui.add(egui::ProgressBar::new(job.progress.fraction()).fill(p.accent).desired_height(6.0));
                    ui.add_space(4.0);
                }
            }
            footer_row(
                ui,
                |ui| match self.job.as_ref().and_then(|j| j.result.as_ref()) {
                    Some(Ok(paths)) if !paths.is_empty() => {
                        ui.label(RichText::new(I::CHECK_CIRCLE).color(p.ok));
                        let dir = paths[0].parent().map(|d| d.display().to_string()).unwrap_or_default();
                        ui.add(egui::Label::new(RichText::new(format!("Saved {} page{} to {dir}", paths.len(), if paths.len() == 1 { "" } else { "s" })).color(p.ok)).truncate());
                    }
                    Some(Ok(_)) => {
                        ui.label(RichText::new("Cancelled").color(p.warn));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(I::WARNING_CIRCLE).color(p.danger));
                        ui.add(egui::Label::new(RichText::new(e).color(p.danger)).truncate());
                    }
                    None if running => {
                        let job = self.job.as_ref().unwrap();
                        ui.spinner();
                        ui.add(egui::Label::new(RichText::new(format!("Rendering... {} / {}", job.progress.done.load(Ordering::Relaxed), n)).color(p.muted)).truncate());
                    }
                    None => {
                        ui.add(egui::Label::new(RichText::new(format!("{n} images  ·  {} page{}  ·  {} × {} px", l.pages, if l.pages == 1 { "" } else { "s" }, l.width, l.height)).color(p.muted)).truncate());
                    }
                },
                |ui| {
                    if running {
                        if danger_button(ui, p, &format!("{}  Cancel", I::STOP)).clicked() {
                            if let Some(j) = &self.job {
                                j.progress.cancel.store(true, Ordering::SeqCst);
                            }
                        }
                    } else {
                        let label = format!("{}  Create {} page{}", I::FLOPPY_DISK, l.pages, if l.pages == 1 { "" } else { "s" });
                        let ok = n > 0 && self.items.is_some() && self.s.out_dir.is_some() && sheet::page_limit_error(self.s.format, &l).is_none();
                        if primary_button(ui, p, ok, &label).clicked() {
                            start = true;
                        }
                        if ui.add(egui::Button::new("Close").min_size(Vec2::new(80.0, 30.0))).clicked() {
                            close = true;
                        }
                        if let Some(Ok(paths)) = self.job.as_ref().and_then(|j| j.result.as_ref()) {
                            if let Some(first) = paths.first().cloned() {
                                if ui.add(egui::Button::new(format!("{}  Show in Explorer", I::ARROW_SQUARE_OUT)).min_size(Vec2::new(0.0, 30.0))).clicked() {
                                    crate::fileops::show_in_explorer(&first);
                                }
                                if ui.add(egui::Button::new(format!("{}  Open", I::EYE)).min_size(Vec2::new(0.0, 30.0))).clicked() {
                                    crate::fileops::open_with_default(&first);
                                }
                            }
                        }
                    }
                },
            );
        });
        // Narrow window: settings only (the preview needs room to be useful).
        if ui.available_width() < 340.0 + 260.0 {
            egui::CentralPanel::no_frame().show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("cs-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_enabled_ui(!running, |ui| self.settings(ui, p, &l));
                });
            });
            if start {
                self.start(ui.ctx());
            }
            return close;
        }
        egui::Panel::left("cs-settings").resizable(false).exact_size(340.0).frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 10, top: 0, bottom: 0 })).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("cs-scroll").auto_shrink([false, false]).show(ui, |ui| {
                ui.add_enabled_ui(!running, |ui| self.settings(ui, p, &l));
            });
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                let avail = ui.available_size();
                ui.set_min_size(avail);
                if n == 0 {
                    empty_state(ui, p, I::GRID_NINE, "No images selected", "Select images in the browser, then create a contact sheet.");
                    return;
                }
                let pending = self.preview_version < self.version || self.items.is_none();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PREVIEW - PAGE 1").size(11.0).color(p.faint).strong());
                    if pending {
                        ui.spinner();
                    }
                });
                match &self.preview {
                    Some((tex, _)) => {
                        let size = tex.size_vec2();
                        let room = ui.available_size() - Vec2::new(0.0, 4.0);
                        let s = (room.x / size.x).min(room.y / size.y).min(1.0);
                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Image::new((tex.id(), size * s)).corner_radius(CornerRadius::same(2)));
                        });
                    }
                    None => {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("Preparing preview...").color(p.muted));
                        });
                    }
                }
            });
        });
        if start {
            self.start(ui.ctx());
        }
        close
    }

    fn settings(&mut self, ui: &mut Ui, p: &Palette, l: &sheet::Layout) {
        let s = &mut self.s;
        ui.spacing_mut().slider_width = 110.0;
        card(ui, p, I::GRID_NINE, "Layout", |ui| {
            egui::Grid::new("cs-layout").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Grid");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.cols).range(1..=20).suffix(" cols"));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut s.rows).range(1..=30).suffix(" rows"));
                });
                ui.end_row();
                ui.label("Thumbnail");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.cell_w).range(32..=2000).suffix(" px"));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut s.cell_h).range(32..=2000).suffix(" px"));
                });
                ui.end_row();
                ui.label("Spacing");
                ui.add(egui::DragValue::new(&mut s.spacing).range(0..=200).suffix(" px"));
                ui.end_row();
                ui.label("Margin");
                ui.add(egui::DragValue::new(&mut s.margin).range(0..=400).suffix(" px"));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut s.crop_fill, "Crop thumbnails to fill");
                ui.end_row();
            });
            hint(ui, p, &format!("{} per page · {} page{} · {} × {} px", l.per_page, l.pages, if l.pages == 1 { "" } else { "s" }, l.width, l.height));
            if let Some(e) = sheet::page_limit_error(s.format, l) {
                ui.label(RichText::new(format!("{}  {e}", I::WARNING)).size(11.5).color(p.danger));
            }
        });
        ui.add_space(8.0);
        card(ui, p, I::TEXT_T, "Captions & colors", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut s.cap_name, "File name");
                ui.checkbox(&mut s.cap_dims, "Dimensions");
                ui.checkbox(&mut s.cap_date, "Date");
                ui.checkbox(&mut s.cap_size, "File size");
            });
            egui::Grid::new("cs-captions").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Text size");
                ui.add(egui::Slider::new(&mut s.caption_px, 9.0..=40.0).fixed_decimals(0).suffix(" px"));
                ui.end_row();
                ui.label("Background");
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgb(&mut s.bg);
                    for (name, bg, fg) in [("White", [255, 255, 255], [40, 42, 48]), ("Gray", [128, 128, 128], [250, 250, 250]), ("Black", [18, 18, 20], [225, 225, 230])] {
                        if ui.small_button(name).clicked() {
                            s.bg = bg;
                            s.fg = fg;
                        }
                    }
                });
                ui.end_row();
                ui.label("Text");
                ui.color_edit_button_srgb(&mut s.fg);
                ui.end_row();
            });
            if sheet::fonts().regular.is_none() {
                ui.label(RichText::new("The Segoe UI font was not found; captions are disabled.").size(11.5).color(p.warn));
            }
        });
        ui.add_space(8.0);
        card(ui, p, I::TEXT_AA, "Header", |ui| {
            ui.add(egui::TextEdit::singleline(&mut s.title).hint_text("Title (optional)").desired_width(f32::INFINITY));
            egui::Grid::new("cs-header").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Title size");
                ui.add(egui::Slider::new(&mut s.title_px, 12.0..=96.0).fixed_decimals(0).suffix(" px"));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut s.page_numbers, "Page numbers");
                ui.end_row();
            });
        });
        ui.add_space(8.0);
        card(ui, p, I::FLOPPY_DISK, "Output", |ui| {
            egui::Grid::new("cs-out").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Format");
                egui::ComboBox::from_id_salt("cs-format").selected_text(if s.format == SaveFormat::WebP { "WebP" } else { s.format.label() }).show_ui(ui, |ui| {
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
                ui.label("File name");
                ui.add(egui::TextEdit::singleline(&mut s.base_name).desired_width(170.0));
                ui.end_row();
                ui.label("Folder");
                let label = s.out_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "Choose...".into());
                if ui.add(egui::Button::new(format!("{}  {label}", I::FOLDER)).truncate()).on_hover_text(&label).clicked() {
                    let mut dlg = rfd::FileDialog::new();
                    if let Some(d) = &s.out_dir {
                        dlg = dlg.set_directory(d);
                    }
                    if let Some(d) = dlg.pick_folder() {
                        s.out_dir = Some(d);
                    }
                }
                ui.end_row();
            });
            hint(ui, p, "Existing files are never overwritten.");
        });
    }
}

// =============================================================== duplicates

/// Events from the duplicates window that the app must handle.
pub enum DupEvent {
    Deleted(Vec<PathBuf>),
    Open(PathBuf),
}

struct ScanJob {
    progress: Arc<Progress>,
    rx: Receiver<Result<Vec<ScanItem>, String>>,
    similar: bool,
}

pub struct Duplicates {
    pub folder: PathBuf,
    files: Vec<PathBuf>,
    include_subfolders: bool,
    find_similar: bool,
    similarity: u32,
    scan: Option<ScanJob>,
    items: Arc<Vec<ScanItem>>,
    scanned: Option<(bool, bool)>,
    /// Groups hold indices into `items`; they are only valid while
    /// `groups_gen == items_gen` (see `replace_items` / `accept_groups`).
    groups: Vec<Group>,
    groups_gen: u64,
    grouped_for: Option<(bool, u32)>,
    /// Grouping runs on a worker: (parameters, scan generation, result).
    group_rx: Option<Receiver<((bool, u32), u64, Vec<Group>)>>,
    /// Bumped whenever `items` changes so stale grouping results are dropped.
    items_gen: u64,
    /// When the grouping parameters last changed (for debouncing the slider).
    want_changed: Option<((bool, u32), std::time::Instant)>,
    /// Let "Mark all but the best" also mark similar (not identical) groups.
    bulk_similar: bool,
    marked: HashSet<PathBuf>,
    keep_rule: KeepRule,
    confirm_delete: bool,
    deleting: Option<Receiver<Result<Vec<PathBuf>, String>>>,
    error: Option<String>,
}

fn scan(files: Vec<PathBuf>, folder: PathBuf, recursive: bool, similar: bool, prog: Arc<Progress>, ctx: egui::Context) -> Result<Vec<ScanItem>, String> {
    use rayon::prelude::*;
    prog.set_phase("Listing files", 0);
    let paths: Vec<PathBuf> = if recursive {
        crate::catalog::list_recursive(&folder, 500_000).into_iter().map(|e| e.path).collect()
    } else if files.is_empty() {
        crate::catalog::list_folder(&folder, false).map_err(|e| e.to_string())?.into_iter().filter(|e| e.kind == crate::catalog::EntryKind::Image).map(|e| e.path).collect()
    } else {
        files
    };
    let pool = job_pool().ok_or("Could not start worker threads")?;
    pool.install(|| {
        let mut items: Vec<ScanItem> = paths
            .par_iter()
            .filter_map(|p| {
                let m = std::fs::metadata(p).ok()?;
                Some(ScanItem { path: p.clone(), size: m.len(), modified: m.modified().ok(), dims: None, content: None, dhash: None, detail: 0.0 })
            })
            .filter(|it| it.size > 0)
            .collect();
        // Exact duplicates: only files sharing a size can be identical.
        let mut by_size: HashMap<u64, usize> = HashMap::new();
        for it in &items {
            *by_size.entry(it.size).or_default() += 1;
        }
        let candidates: Vec<usize> = (0..items.len()).filter(|&i| by_size[&items[i].size] > 1).collect();
        prog.set_phase("Comparing file contents", candidates.len());
        let hashes: Vec<(usize, Option<u64>)> = candidates
            .par_iter()
            .map(|&i| {
                if prog.cancelled() {
                    return (i, None);
                }
                let h = std::panic::catch_unwind(|| dupes::file_hash(&items[i].path).ok()).ok().flatten();
                prog.done.fetch_add(1, Ordering::Relaxed);
                ctx.request_repaint();
                (i, h)
            })
            .collect();
        for (i, h) in hashes {
            items[i].content = h;
        }
        if prog.cancelled() {
            return Err("Cancelled".to_string());
        }
        // Visual fingerprints (and dimensions for picking the best copy).
        let need: Vec<usize> = if similar {
            (0..items.len()).collect()
        } else {
            let mut seen: HashMap<(u64, u64), usize> = HashMap::new();
            for it in &items {
                if let Some(c) = it.content {
                    *seen.entry((it.size, c)).or_default() += 1;
                }
            }
            (0..items.len()).filter(|&i| items[i].content.is_some_and(|c| seen[&(items[i].size, c)] > 1)).collect()
        };
        prog.set_phase(if similar { "Analyzing images" } else { "Reading image sizes" }, need.len());
        let info: Vec<(usize, Option<(u32, u32)>, Option<(u64, f32)>)> = need
            .par_iter()
            .map(|&i| {
                if prog.cancelled() {
                    return (i, None, None);
                }
                let p = &items[i].path;
                // A decoder panic on one broken file must not take the whole scan down.
                let r = std::panic::catch_unwind(|| {
                    let dims = crate::decode::probe_dimensions(p);
                    let h = if similar { crate::decode::decode_thumbnail(p, 256).ok().map(|t| dupes::dhash_detail(&t)) } else { None };
                    (dims, h)
                });
                let (dims, h) = r.unwrap_or((None, None));
                prog.done.fetch_add(1, Ordering::Relaxed);
                ctx.request_repaint();
                (i, dims, h)
            })
            .collect();
        for (i, d, h) in info {
            items[i].dims = d;
            if let Some((h, detail)) = h {
                items[i].dhash = Some(h);
                items[i].detail = detail;
            }
        }
        if prog.cancelled() {
            return Err("Cancelled".to_string());
        }
        Ok(items)
    })
}

impl Duplicates {
    pub fn new(folder: PathBuf, files: Vec<PathBuf>, core: &Core) -> Self {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
        let mut d = Self::blank(folder, files);
        d.start_scan(&core.ctx);
        d
    }

    fn blank(folder: PathBuf, files: Vec<PathBuf>) -> Self {
        Duplicates {
            folder,
            files,
            include_subfolders: false,
            find_similar: true,
            similarity: 90,
            scan: None,
            items: Arc::new(Vec::new()),
            scanned: None,
            groups: Vec::new(),
            groups_gen: 0,
            grouped_for: None,
            group_rx: None,
            items_gen: 0,
            want_changed: None,
            bulk_similar: false,
            marked: HashSet::new(),
            keep_rule: KeepRule::Resolution,
            confirm_delete: false,
            deleting: None,
            error: None,
        }
    }

    /// Swap in a new item list. Groups and marks from the old list are
    /// dropped at once: group members are indices into `items`, so keeping
    /// them would draw (or mark for deletion) the wrong files.
    fn replace_items(&mut self, items: Vec<ScanItem>) {
        self.items = Arc::new(items);
        self.items_gen += 1;
        self.groups.clear();
        self.groups_gen = self.items_gen;
        self.marked.clear();
        self.grouped_for = None;
        // A grouping still running for the old list is stale; let a new one start.
        self.group_rx = None;
    }

    /// Accept a grouping result computed for `generation` of the item list.
    /// Returns false (and changes nothing) when it belongs to another list.
    fn accept_groups(&mut self, want: (bool, u32), generation: u64, mut groups: Vec<Group>) -> bool {
        if generation != self.items_gen {
            return false;
        }
        let n = self.items.len();
        groups.retain(|g| g.members.len() > 1 && g.members.iter().all(|&i| i < n));
        self.groups = groups;
        self.groups_gen = generation;
        self.grouped_for = Some(want);
        let present: HashSet<&PathBuf> = self.groups.iter().flat_map(|g| g.members.iter().map(|&i| &self.items[i].path)).collect();
        self.marked.retain(|m| present.contains(m));
        true
    }

    /// Defensive: never keep groups that index into another item list.
    fn ensure_groups_current(&mut self) {
        let n = self.items.len();
        if self.groups_gen != self.items_gen || self.groups.iter().any(|g| g.members.iter().any(|&i| i >= n)) {
            self.groups.clear();
            self.groups_gen = self.items_gen;
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if let Some(s) = &self.scan {
            s.progress.cancel.store(true, Ordering::SeqCst);
        }
        let progress = Arc::new(Progress::default());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (files, folder, rec, sim, prog, c) = (self.files.clone(), self.folder.clone(), self.include_subfolders, self.find_similar, progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = guarded("Scanning", || scan(files, folder, rec, sim, prog, c.clone()));
            let _ = tx.send(r);
            c.request_repaint();
        });
        self.error = None;
        self.scan = Some(ScanJob { progress, rx, similar: self.find_similar });
    }

    fn poll(&mut self, events: &mut Vec<DupEvent>, core: &mut Core) {
        if let Some(job) = &self.scan {
            let r = match job.rx.try_recv() {
                Ok(r) => Some(r),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err("The scan stopped unexpectedly".to_string())),
                Err(_) => None,
            };
            if let Some(r) = r {
                let sim = job.similar;
                self.scan = None;
                match r {
                    Ok(items) => {
                        self.replace_items(items);
                        self.scanned = Some((sim, self.include_subfolders));
                    }
                    Err(e) if e == "Cancelled" => {}
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if let Some(rx) = &self.group_rx {
            match rx.try_recv() {
                Ok((want, generation, groups)) => {
                    self.group_rx = None;
                    self.accept_groups(want, generation, groups);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.group_rx = None;
                    self.error = Some("Grouping failed unexpectedly".into());
                    self.grouped_for = Some((self.find_similar, dupes::threshold_for_pct(self.similarity)));
                }
                Err(_) => {}
            }
        }
        let want = (self.find_similar && self.scanned.is_some_and(|s| s.0), dupes::threshold_for_pct(self.similarity));
        if self.scanned.is_some() && self.grouped_for != Some(want) && self.group_rx.is_none() {
            // Debounce: wait until the slider has rested for a moment.
            let now = std::time::Instant::now();
            let since = match self.want_changed {
                Some((w, t)) if w == want => t,
                _ => {
                    self.want_changed = Some((want, now));
                    now
                }
            };
            let first = self.grouped_for.is_none();
            if first || now.duration_since(since) >= std::time::Duration::from_millis(250) {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (items, generation, c) = (self.items.clone(), self.items_gen, core.ctx.clone());
                std::thread::spawn(move || {
                    if let Ok(groups) = std::panic::catch_unwind(|| dupes::group(&items, want.0, want.1)) {
                        let _ = tx.send((want, generation, groups));
                    }
                    c.request_repaint();
                });
                self.group_rx = Some(rx);
            } else {
                core.ctx.request_repaint_after(std::time::Duration::from_millis(260));
            }
        }
        if let Some(rx) = &self.deleting {
            let r = match rx.try_recv() {
                Ok(r) => Some(r),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err("Deleting stopped unexpectedly".to_string())),
                Err(_) => None,
            };
            if let Some(r) = r {
                self.deleting = None;
                // Whatever happened, files that no longer exist leave the list
                // (a partial failure can still have removed some of them).
                let gone: Vec<PathBuf> = self.items.iter().filter(|it| self.marked.contains(&it.path) && !it.path.exists()).map(|it| it.path.clone()).collect();
                if !gone.is_empty() {
                    let set: HashSet<&PathBuf> = gone.iter().collect();
                    let items: Vec<ScanItem> = self.items.iter().filter(|it| !set.contains(&it.path)).cloned().collect();
                    self.replace_items(items);
                }
                match r {
                    Ok(_) => core.toasts.success(format!("Moved {} file{} to the Recycle Bin", gone.len(), if gone.len() == 1 { "" } else { "s" })),
                    Err(e) => {
                        let msg = if gone.is_empty() { e } else { format!("{e} ({} file{} were moved to the Recycle Bin)", gone.len(), if gone.len() == 1 { "" } else { "s" }) };
                        core.toasts.error(msg.clone());
                        self.error = Some(msg);
                    }
                }
                if !gone.is_empty() {
                    events.push(DupEvent::Deleted(gone));
                }
            }
        }
    }

    fn mark_best(&mut self, gi: usize) {
        self.ensure_groups_current();
        let Some(g) = self.groups.get(gi) else { return };
        let best = dupes::best_in_group(&self.items, &g.members, self.keep_rule);
        for (k, &i) in g.members.iter().enumerate() {
            let path = self.items[i].path.clone();
            if k == best {
                self.marked.remove(&path);
            } else {
                self.marked.insert(path);
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, core: &mut Core) -> (bool, Vec<DupEvent>) {
        let p = core.pal;
        let mut events = Vec::new();
        self.poll(&mut events, core);
        let mut open = true;
        let mut close = false;
        egui::Window::new(format!("{}  Find duplicate & similar images", I::COPY_SIMPLE))
            .open(&mut open)
            .default_size(window_size(ctx, 900.0, 680.0)).max_size(window_size(ctx, 4000.0, 4000.0)).constrain(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .min_size(crate::ui::batch::window_size(ctx, 560.0, 380.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                close = window_body(ui, |ui| self.ui(ui, &p, core, &mut events));
            });
        if self.confirm_delete && (self.marked.is_empty() || self.deleting.is_some()) {
            self.confirm_delete = false;
        }
        if self.confirm_delete {
            let paths: Vec<PathBuf> = self.marked.iter().cloned().collect();
            let bytes: u64 = self.items.iter().filter(|it| self.marked.contains(&it.path)).map(|it| it.size).sum();
            let all_gone = self.groups.iter().filter(|g| g.members.iter().all(|&i| self.marked.contains(&self.items[i].path))).count();
            let mut body = format!("{} file{} ({}) will be moved to the Recycle Bin. You can restore them from there.", paths.len(), if paths.len() == 1 { "" } else { "s" }, human_size(bytes));
            if all_gone > 0 {
                body.push_str(&format!("\n\nWarning: in {all_gone} group{} every copy is marked, so no copy will be kept.", if all_gone == 1 { "" } else { "s" }));
            }
            match crate::ui::widgets::show_confirm(ctx, "Move to Recycle Bin?", &body, "Move to Recycle Bin", true, &p) {
                Some(true) => {
                    self.confirm_delete = false;
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    let c = ctx.clone();
                    std::thread::spawn(move || {
                        let r = crate::fileops::delete_to_trash(&paths).map(|_| paths).map_err(|e| e.to_string());
                        let _ = tx.send(r);
                        c.request_repaint();
                    });
                    self.deleting = Some(rx);
                }
                Some(false) => self.confirm_delete = false,
                None => {}
            }
        }
        let still_open = open && !close;
        if !still_open {
            if let Some(s) = &self.scan {
                s.progress.cancel.store(true, Ordering::SeqCst);
            }
        }
        (still_open, events)
    }

    fn ui(&mut self, ui: &mut Ui, p: &Palette, core: &mut Core, events: &mut Vec<DupEvent>) -> bool {
        self.ensure_groups_current();
        let mut close = false;
        let scanning = self.scan.is_some();
        let busy = scanning || self.deleting.is_some();
        let marked_bytes: u64 = self.items.iter().filter(|it| self.marked.contains(&it.path)).map(|it| it.size).sum();
        egui::Panel::bottom("dup-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 })).show(ui, |ui| {
            let n = self.marked.len();
            let mut confirm = false;
            footer_row(
                ui,
                |ui| {
                    if self.deleting.is_some() {
                        ui.spinner();
                        ui.add(egui::Label::new(RichText::new("Moving files to the Recycle Bin...").color(p.muted)).truncate());
                    } else if n > 0 {
                        ui.add(egui::Label::new(RichText::new(format!("{n} marked · {}", human_size(marked_bytes))).color(p.muted)).truncate());
                    } else if let Some(e) = &self.error {
                        ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING_CIRCLE)).color(p.danger)).truncate());
                    }
                },
                |ui| {
                    let enabled = n > 0 && !busy;
                    let fill = if enabled { p.danger } else { p.danger.gamma_multiply(0.4) };
                    if ui.add_enabled(enabled, egui::Button::new(RichText::new(format!("{}  Move {n} marked to Recycle Bin", I::TRASH)).color(Color32::WHITE)).fill(fill).min_size(Vec2::new(0.0, 30.0))).clicked() {
                        confirm = true;
                    }
                    if ui.add(egui::Button::new("Close").min_size(Vec2::new(80.0, 30.0))).clicked() {
                        close = true;
                    }
                },
            );
            if confirm {
                self.confirm_delete = true;
            }
        });
        egui::Panel::top("dup-options").frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 0, bottom: 8 })).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    ui.set_max_width(ui.available_width());
                    ui.label(RichText::new(I::FOLDER).color(p.accent));
                    ui.add(egui::Label::new(RichText::new(short_path(&self.folder, 72)).strong()).truncate()).on_hover_text(self.folder.display().to_string());
                });
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.include_subfolders, "Include subfolders");
                    ui.checkbox(&mut self.find_similar, "Find similar images");
                    ui.add_enabled_ui(self.find_similar, |ui| {
                        ui.label(RichText::new("Similarity").color(p.muted));
                        ui.add(egui::Slider::new(&mut self.similarity, 70..=100).suffix(" %")).on_hover_text("Lower values find more loosely similar pictures (crops, edits, re-encodes)");
                    });
                    let stale = self.scanned.is_some_and(|(sim, rec)| (self.find_similar && !sim) || rec != self.include_subfolders);
                    let label = if self.scanned.is_none() { "Scan" } else { "Scan again" };
                    if stale {
                        if primary_button(ui, p, true, &format!("{}  {label}", I::MAGNIFYING_GLASS)).clicked() {
                            self.start_scan(ui.ctx());
                        }
                    } else if ui.button(format!("{}  {label}", I::ARROWS_CLOCKWISE)).clicked() {
                        self.start_scan(ui.ctx());
                    }
                });
            });
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(job) = &self.scan {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    let phase = job.progress.phase.lock().clone();
                    let (d, t) = (job.progress.done.load(Ordering::Relaxed), job.progress.total.load(Ordering::Relaxed));
                    ui.label(RichText::new(if t > 0 { format!("{phase}...  {d} / {t}") } else { format!("{phase}...") }).font(crate::ui::theme::semibold(14.0)));
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(job.progress.fraction()).fill(p.accent).desired_width(360.0f32.min(ui.available_width())).desired_height(6.0));
                    ui.add_space(8.0);
                    if ui.button(format!("{}  Cancel", I::STOP)).clicked() {
                        job.progress.cancel.store(true, Ordering::SeqCst);
                    }
                });
                return;
            }
            if self.scanned.is_none() {
                if self.error.is_none() {
                    empty_state(ui, p, I::COPY_SIMPLE, "Ready to scan", "Choose options above and press Scan.");
                }
                return;
            }
            if self.group_rx.is_some() || self.grouped_for.is_none() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Grouping...").color(p.muted));
                });
            }
            if self.groups.is_empty() {
                if self.grouped_for.is_none() {
                    return;
                }
                empty_state(ui, p, I::CHECK_CIRCLE, "No duplicates found", &format!("{} images checked. Try a lower similarity or include subfolders.", self.items.len()));
                return;
            }
            // Summary + auto-mark
            let files_in_groups: usize = self.groups.iter().map(|g| g.members.len()).sum();
            let reclaim: u64 = self
                .groups
                .iter()
                .map(|g| {
                    let best = g.members[dupes::best_in_group(&self.items, &g.members, self.keep_rule)];
                    g.members.iter().filter(|&&i| i != best).map(|&i| self.items[i].size).sum::<u64>()
                })
                .sum();
            let mut mark_all = false;
            let ng = self.groups.len();
            let summary = |ui: &mut Ui, keep_rule: &mut KeepRule, bulk_similar: &mut bool, marked: &mut HashSet<PathBuf>, mark_all: &mut bool| {
                ui.add(egui::Label::new(RichText::new(format!("{ng} group{} · {files_in_groups} files · up to {} reclaimable", if ng == 1 { "" } else { "s" }, human_size(reclaim))).strong()).wrap());
                ui.add_space(2.0);
                // Wraps onto a second line when narrow.
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Keep").color(p.muted));
                    egui::ComboBox::from_id_salt("dup-keep").selected_text(keep_rule.label()).show_ui(ui, |ui| {
                        for r in KeepRule::ALL {
                            ui.selectable_value(keep_rule, r, r.label());
                        }
                    });
                    ui.checkbox(bulk_similar, "Include similar groups")
                        .on_hover_text("Similar pictures are not copies: review them one group at a time unless you are sure");
                    if ui.button(format!("{}  Mark all but the best", I::LIST_CHECKS)).on_hover_text("In every identical group (and, if allowed, every similar group), mark all copies except the one to keep").clicked() {
                        *mark_all = true;
                    }
                    if ui.add_enabled(!marked.is_empty(), egui::Button::new("Clear marks")).clicked() {
                        marked.clear();
                    }
                });
                ui.add_space(4.0);
            };
            // Short window (large interface size): the summary scrolls with the groups.
            let summary_in_list = ui.available_height() < 260.0;
            if !summary_in_list {
                summary(ui, &mut self.keep_rule, &mut self.bulk_similar, &mut self.marked, &mut mark_all);
            }
            let mut mark_group = None;
            let mut unmark_group = None;
            egui::ScrollArea::vertical().id_salt("dup-groups").auto_shrink([false, false]).show(ui, |ui| {
                if summary_in_list {
                    summary(ui, &mut self.keep_rule, &mut self.bulk_similar, &mut self.marked, &mut mark_all);
                }
                for (gi, g) in self.groups.iter().enumerate() {
                    let best = dupes::best_in_group(&self.items, &g.members, self.keep_rule);
                    egui::Frame::new()
                        .fill(if p.dark { p.panel } else { p.bg })
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            // Buttons at the right; badge and size in the room left of them.
                            ui.horizontal(|ui| {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("Unmark").clicked() {
                                        unmark_group = Some(gi);
                                    }
                                    if ui.small_button(format!("{}  Keep best", I::CROWN)).on_hover_text(format!("Mark all except the {} copy", self.keep_rule.label().to_lowercase())).clicked() {
                                        mark_group = Some(gi);
                                    }
                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                        let (badge, color) = match g.kind {
                                            GroupKind::Identical => ("Identical".to_string(), p.accent),
                                            GroupKind::Similar => (format!("Similar · {}%", g.min_similarity), p.warn),
                                        };
                                        egui::Frame::new().fill(color.gamma_multiply(0.18)).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
                                            ui.label(RichText::new(badge).size(11.5).color(color).strong());
                                        });
                                        let total: u64 = g.members.iter().map(|&i| self.items[i].size).sum();
                                        ui.add(egui::Label::new(RichText::new(format!("{} files · {}", g.members.len(), human_size(total))).color(p.muted)).truncate());
                                    });
                                });
                            });
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
                                for (k, &i) in g.members.iter().enumerate() {
                                    let it = &self.items[i];
                                    let mut marked = self.marked.contains(&it.path);
                                    if dup_tile(ui, p, core, it, &mut marked, k == best, events) {
                                        if marked {
                                            self.marked.insert(it.path.clone());
                                        } else {
                                            self.marked.remove(&it.path);
                                        }
                                    }
                                }
                            });
                        });
                    ui.add_space(8.0);
                }
            });
            if mark_all {
                for gi in 0..self.groups.len() {
                    // Similar groups are only bulk-marked on explicit opt-in.
                    if self.groups[gi].kind == GroupKind::Identical || self.bulk_similar {
                        self.mark_best(gi);
                    }
                }
            }
            if let Some(gi) = mark_group {
                self.mark_best(gi);
            }
            if let Some(g) = unmark_group.and_then(|gi| self.groups.get(gi)) {
                for &i in &g.members {
                    self.marked.remove(&self.items[i].path);
                }
            }
        });
        close
    }
}

/// Shorten a long path to "...\parent\folder" so it fits in a header.
fn short_path(p: &Path, max_chars: usize) -> String {
    let full = p.display().to_string();
    if full.chars().count() <= max_chars {
        return full;
    }
    let parts: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    let mut tail = String::new();
    for part in parts.iter().rev() {
        let candidate = if tail.is_empty() { part.clone() } else { format!("{part}\\{tail}") };
        if candidate.chars().count() + 2 > max_chars && !tail.is_empty() {
            break;
        }
        tail = candidate;
    }
    format!("...\\{tail}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_paths() {
        let p = std::path::Path::new("C:\\Users\\someone\\Pictures\\2024\\Holiday");
        assert_eq!(super::short_path(p, 80), "C:\\Users\\someone\\Pictures\\2024\\Holiday");
        assert_eq!(super::short_path(p, 20), "...\\2024\\Holiday");
    }

    fn item(name: &str) -> super::ScanItem {
        super::ScanItem { path: std::path::PathBuf::from(name), size: 10, modified: None, dims: None, content: Some(1), dhash: None, detail: 0.0 }
    }

    fn group(members: Vec<usize>) -> super::Group {
        super::Group { kind: super::GroupKind::Identical, members, min_similarity: 100 }
    }

    #[test]
    fn duplicate_groups_never_outlive_their_item_list() {
        let mut d = super::Duplicates::blank(std::path::PathBuf::from("C:\\x"), Vec::new());
        d.replace_items(vec![item("a"), item("b"), item("c"), item("d")]);
        let old_gen = d.items_gen;
        assert!(d.accept_groups((false, 0), old_gen, vec![group(vec![2, 3])]));
        d.marked.insert("d".into());
        // The list shrinks (files were moved to the Recycle Bin): old groups and marks go.
        d.replace_items(vec![item("a"), item("c")]);
        assert!(d.groups.is_empty());
        assert!(d.marked.is_empty());
        assert!(d.grouped_for.is_none());
        // A late result for the old list is ignored and cannot index the new one.
        assert!(!d.accept_groups((false, 0), old_gen, vec![group(vec![2, 3])]));
        assert!(d.groups.is_empty());
        d.mark_best(0);
        assert!(d.marked.is_empty());
        // Out-of-range members are dropped even for the current generation.
        assert!(d.accept_groups((false, 0), d.items_gen, vec![group(vec![0, 1]), group(vec![1, 5])]));
        assert_eq!(d.groups.len(), 1);
        d.mark_best(0);
        assert_eq!(d.marked.len(), 1);
        // Stale groups left behind by any other path are cleared before use.
        d.items_gen += 1;
        d.ensure_groups_current();
        assert!(d.groups.is_empty());
    }
}

/// One file inside a duplicate group. Returns true when the mark changed.
fn dup_tile(ui: &mut Ui, p: &Palette, core: &mut Core, it: &ScanItem, marked: &mut bool, best: bool, events: &mut Vec<DupEvent>) -> bool {
    let w = 168.0;
    let mut changed = false;
    ui.allocate_ui_with_layout(Vec2::new(w, 190.0), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(w);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 2.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 126.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(6), ui.visuals().extreme_bg_color);
            let key = crate::thumbs::ThumbKey::new(&it.path, it.size, it.modified);
            match core.thumbs.get(&key) {
                crate::thumbs::Thumb::Ready(info) => {
                    let ts = info.tex.size_vec2();
                    let s = ((rect.width() - 8.0) / ts.x).min((rect.height() - 8.0) / ts.y);
                    let r = Rect::from_center_size(rect.center(), ts * s);
                    painter.image(info.tex.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), if *marked { Color32::from_white_alpha(110) } else { Color32::WHITE });
                }
                crate::thumbs::Thumb::Pending => {
                    painter.text(rect.center(), egui::Align2::CENTER_CENTER, I::IMAGE, egui::FontId::proportional(22.0), p.faint);
                }
                crate::thumbs::Thumb::Failed => {
                    painter.text(rect.center(), egui::Align2::CENTER_CENTER, I::WARNING, egui::FontId::proportional(22.0), p.danger);
                }
            }
            let stroke = if *marked {
                egui::Stroke::new(2.0, p.danger)
            } else if resp.hovered() {
                egui::Stroke::new(1.5, p.accent)
            } else {
                egui::Stroke::new(1.0, p.border)
            };
            painter.rect_stroke(rect, CornerRadius::same(6), stroke, egui::StrokeKind::Inside);
            if best {
                let badge = Rect::from_min_size(rect.left_top() + Vec2::new(6.0, 6.0), Vec2::new(24.0, 22.0));
                painter.rect_filled(badge, CornerRadius::same(5), p.ok);
                painter.text(badge.center(), egui::Align2::CENTER_CENTER, I::CROWN, egui::FontId::proportional(13.0), Color32::WHITE);
            }
            if *marked {
                let badge = Rect::from_min_size(rect.right_top() + Vec2::new(-30.0, 6.0), Vec2::new(24.0, 22.0));
                painter.rect_filled(badge, CornerRadius::same(5), p.danger);
                painter.text(badge.center(), egui::Align2::CENTER_CENTER, I::TRASH, egui::FontId::proportional(13.0), Color32::WHITE);
            }
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\nClick to open", it.path.display()));
        if resp.clicked() {
            events.push(DupEvent::Open(it.path.clone()));
        }
        if ui.checkbox(marked, RichText::new(file_name(&it.path)).size(12.5)).changed() {
            changed = true;
        }
        let dims = it.dims.map(|(w, h)| format!("{w} × {h} · ")).unwrap_or_default();
        ui.add(egui::Label::new(RichText::new(format!("{dims}{}", human_size(it.size))).size(11.5).color(p.muted)).truncate());
        let folder = it.path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        ui.add(egui::Label::new(RichText::new(format!("{} · {folder}", crate::catalog::format_time(it.modified))).size(11.0).color(p.faint)).truncate());
    });
    changed
}

// ================================================================== print

/// Print an image (shows the Windows print dialog). Returns Ok(()) when the
/// user cancels.
pub fn print_image(img: &RgbaImage, title: &str) -> anyhow::Result<()> {
    print::print_image(img, title)
}
