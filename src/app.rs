//! Application shell: shared services (`Core`), mode switching, command
//! dispatch, keyboard shortcuts and app-level dialogs.

use crate::catalog::{self, EntryKind};
use crate::edit::{tools as edit_tools, Transform};
use crate::encode::SaveOptions;
use crate::fileops;
use crate::loader::Loader;
use crate::meta::MetaService;
use crate::settings::{BrowserView, FitMode, Settings, ThemeChoice};
use crate::tags::TagStore;
use crate::thumbs::ThumbService;
use crate::ui::browser::{Browser, BrowserEvent};
use crate::ui::commands::Command;
use crate::ui::icons as I;
use crate::ui::palette::CommandPalette;
use crate::ui::save_dialog::{SaveDialog, SaveOutcome};
use crate::ui::theme::{self, Palette};
use crate::ui::viewer::{Viewer, ViewerEvent};
use crate::ui::widgets::{self, Prompt, Toasts};
use egui::{Key, KeyboardShortcut, Modifiers, RichText, Vec2};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Services shared by every mode.
pub struct Core {
    pub ctx: egui::Context,
    pub settings: Settings,
    pub pal: Palette,
    pub tags: TagStore,
    pub loader: Loader,
    pub thumbs: ThumbService,
    pub meta: MetaService,
    pub toasts: Toasts,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Browser,
    Viewer,
    Slideshow,
    Compare,
    Video,
}

enum PendingPrompt {
    Rename(PathBuf),
    NewFolder(PathBuf),
}

enum Confirm {
    Delete(Vec<PathBuf>),
}

struct TransferDialog {
    files: Vec<PathBuf>,
    do_move: bool,
}

pub struct App {
    pub core: Core,
    pub mode: Mode,
    pub browser: Browser,
    pub viewer: Viewer,
    slideshow: Option<crate::ui::slideshow::Slideshow>,
    compare: Option<crate::ui::compare::Compare>,
    video: Option<crate::video::VideoMode>,
    batch_convert: Option<crate::ui::batch::BatchConvert>,
    batch_rename: Option<crate::ui::batch::BatchRename>,
    contact_sheet: Option<crate::ui::tools::ContactSheet>,
    duplicates: Option<crate::ui::tools::Duplicates>,
    palette: CommandPalette,
    prompt: Option<(Prompt, PendingPrompt)>,
    confirm: Option<Confirm>,
    transfer: Option<TransferDialog>,
    save_dialog: Option<SaveDialog>,
    show_settings: bool,
    show_help: bool,
    show_about: bool,
    fullscreen: bool,
    viewer_from_browser: bool,
    last_save: Instant,
    last_title: String,
    effective_dark: bool,
    dev: crate::devhook::DevHook,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings, initial: Option<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install_fonts(&ctx);
        let dark = match settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => !matches!(ctx.system_theme(), Some(egui::Theme::Light)),
        };
        let pal = Palette::new(dark, settings.accent);
        theme::apply(&ctx, &pal, settings.ui_scale);
        egui_extras::install_image_loaders(&ctx);

        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let mut loader = Loader::new(ctx.clone(), (cores / 4).clamp(2, 4));
        loader.budget_bytes = settings.cache_budget_mb.max(256) << 20;
        loader.opts.lock().color_manage = settings.color_manage;
        let thumbs = ThumbService::new(ctx.clone(), cores.saturating_sub(2).clamp(2, 12), crate::settings::cache_dir().join("thumbs"));
        thumbs.set_disk_cache(settings.thumb_disk_cache);
        let meta = MetaService::new(ctx.clone());

        let start_folder = settings
            .last_folder
            .clone()
            .filter(|p| p.is_dir())
            .or_else(dirs::picture_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("C:\\"));

        let mut app = App {
            core: Core { ctx: ctx.clone(), settings, pal, tags: TagStore::load(), loader, thumbs, meta, toasts: Toasts::default() },
            mode: Mode::Browser,
            browser: Browser::new(start_folder.clone()),
            viewer: Viewer::new(),
            slideshow: None,
            compare: None,
            video: None,
            batch_convert: None,
            batch_rename: None,
            contact_sheet: None,
            duplicates: None,
            palette: CommandPalette::default(),
            prompt: None,
            confirm: None,
            transfer: None,
            save_dialog: None,
            show_settings: false,
            show_help: false,
            show_about: false,
            fullscreen: false,
            viewer_from_browser: false,
            last_save: Instant::now(),
            last_title: String::new(),
            effective_dark: dark,
            dev: crate::devhook::DevHook::new(),
        };
        app.viewer.show_info = app.core.settings.show_info_overlay && false;
        match initial {
            Some(p) if p.is_file() => app.open_file(&p),
            Some(p) if p.is_dir() => app.navigate(p),
            _ => app.navigate(start_folder),
        }
        app
    }

    // ------------------------------------------------------------ navigation

    pub fn navigate(&mut self, folder: PathBuf) {
        if folder != self.browser.folder && self.browser.folder.exists() && !self.browser.entries.is_empty() {
            self.browser.back.push(self.browser.folder.clone());
            self.browser.forward.clear();
        }
        self.core.thumbs.cancel_all();
        self.core.settings.last_folder = Some(folder.clone());
        self.core.settings.push_recent(&folder);
        self.browser.load(folder, &self.core.ctx, self.core.settings.show_hidden_files);
        if self.mode == Mode::Viewer {
            self.mode = Mode::Browser;
        }
    }

    /// Open a single file: show its folder in the browser and the file in the viewer.
    pub fn open_file(&mut self, path: &Path) {
        let ext = crate::formats::ext_of(path);
        if crate::video::is_video_ext(&ext) {
            self.video = Some(crate::video::VideoMode::new(vec![path.to_path_buf()], &self.core));
            self.mode = Mode::Video;
            if let Some(parent) = path.parent() {
                self.browser.pending_select = Some(path.to_path_buf());
                self.browser.load(parent.to_path_buf(), &self.core.ctx, self.core.settings.show_hidden_files);
            }
            return;
        }
        let Some(parent) = path.parent() else { return };
        self.browser.pending_select = Some(path.to_path_buf());
        self.browser.load(parent.to_path_buf(), &self.core.ctx, self.core.settings.show_hidden_files);
        self.core.settings.last_folder = Some(parent.to_path_buf());
        // Open the viewer right away with just this file; the list fills in when the listing arrives.
        self.viewer.open(vec![path.to_path_buf()], 0, self.core.settings.fit_mode);
        self.viewer_from_browser = false;
        self.mode = Mode::Viewer;
        if self.core.settings.open_viewer_fullscreen {
            self.set_fullscreen(true);
        }
    }

    fn open_viewer_at(&mut self, item: usize) {
        let Some(entry) = self.browser.item(item).cloned() else { return };
        match entry.kind {
            EntryKind::Folder => self.navigate(entry.path),
            EntryKind::Video => {
                let vids: Vec<PathBuf> = self.browser.selected_paths().into_iter().filter(|p| crate::video::is_video_ext(&crate::formats::ext_of(p))).collect();
                let list = if vids.len() >= 2 && vids.contains(&entry.path) { vids } else { vec![entry.path.clone()] };
                self.video = Some(crate::video::VideoMode::new(list, &self.core));
                self.mode = Mode::Video;
            }
            EntryKind::Image => {
                let list = self.browser.image_paths();
                let idx = list.iter().position(|p| p == &entry.path).unwrap_or(0);
                self.viewer.open(list, idx, self.core.settings.fit_mode);
                self.viewer_from_browser = true;
                self.mode = Mode::Viewer;
                if self.core.settings.open_viewer_fullscreen {
                    self.set_fullscreen(true);
                }
            }
        }
    }

    fn close_viewer(&mut self) {
        if self.viewer.is_dirty() {
            self.viewer.confirm_discard = true;
            self.viewer.pending_nav = None;
            return;
        }
        if let Some(p) = self.viewer.current().cloned() {
            self.browser.set_focus_path(&p);
        }
        self.viewer.edit = None;
        self.mode = Mode::Browser;
        if self.fullscreen && self.core.settings.open_viewer_fullscreen {
            self.set_fullscreen(false);
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// Keep the viewer's file list in sync with the browser after a listing refresh.
    fn sync_viewer_list(&mut self) {
        if self.mode != Mode::Viewer || self.browser.loading {
            return;
        }
        let Some(cur) = self.viewer.current().cloned() else { return };
        if cur.parent() != Some(self.browser.folder.as_path()) {
            return;
        }
        let list = self.browser.image_paths();
        if list != self.viewer.list && !list.is_empty() {
            if let Some(i) = list.iter().position(|p| p == &cur) {
                self.viewer.list = list;
                self.viewer.index = i;
            }
        }
    }

    // ------------------------------------------------------------ commands

    fn targets(&self) -> Vec<PathBuf> {
        match self.mode {
            Mode::Viewer => self.viewer.current().cloned().into_iter().collect(),
            _ => self.browser.targets(),
        }
    }

    fn media_targets(&self) -> Vec<PathBuf> {
        self.targets().into_iter().filter(|p| !p.is_dir()).collect()
    }

    pub fn run(&mut self, cmd: Command) {
        use Command::*;
        let fit = self.core.settings.fit_mode;
        let wrap = self.core.settings.loop_navigation;
        match cmd {
            OpenFile => {
                let exts: Vec<&str> = crate::formats::NATIVE_EXTS.iter().chain(crate::formats::WIC_EXTS.iter()).chain(crate::video::VIDEO_EXTS.iter()).copied().collect();
                let mut dlg = rfd::FileDialog::new().add_filter("Images and videos", &exts).add_filter("All files", &["*"]);
                dlg = dlg.set_directory(&self.browser.folder);
                if let Some(p) = dlg.pick_file() {
                    self.open_file(&p);
                }
            }
            OpenFolder => {
                if let Some(p) = rfd::FileDialog::new().set_directory(&self.browser.folder).pick_folder() {
                    self.navigate(p);
                }
            }
            CommandPalette => self.palette.toggle(),
            Settings => self.show_settings = !self.show_settings,
            ShortcutsHelp => self.show_help = !self.show_help,
            About => self.show_about = true,
            ToggleFullscreen => self.set_fullscreen(!self.fullscreen),
            ToggleTheme => {
                self.core.settings.theme = if self.effective_dark { ThemeChoice::Light } else { ThemeChoice::Dark };
                self.apply_theme();
            }
            Exit => self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Refresh => {
                for e in &self.browser.entries {
                    self.core.meta.forget(&e.path);
                }
                self.browser.tree.invalidate(&self.browser.folder.clone());
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
            GoUp => {
                if let Some(parent) = self.browser.folder.parent().map(|p| p.to_path_buf()) {
                    let from = self.browser.folder.clone();
                    self.navigate(parent);
                    self.browser.pending_select = Some(from);
                }
            }
            GoBack => {
                if let Some(p) = self.browser.back.pop() {
                    self.browser.forward.push(self.browser.folder.clone());
                    self.browser.load(p, &self.core.ctx, self.core.settings.show_hidden_files);
                }
            }
            GoForward => {
                if let Some(p) = self.browser.forward.pop() {
                    self.browser.back.push(self.browser.folder.clone());
                    self.browser.load(p, &self.core.ctx, self.core.settings.show_hidden_files);
                }
            }
            SelectAll => {
                self.browser.selected = self.browser.items.iter().map(|&e| self.browser.entries[e].path.clone()).filter(|p| !p.is_dir()).collect();
            }
            SelectNone => self.browser.selected.clear(),
            InvertSelection => {
                let all: Vec<PathBuf> = self.browser.items.iter().map(|&e| self.browser.entries[e].path.clone()).filter(|p| !p.is_dir()).collect();
                let cur = std::mem::take(&mut self.browser.selected);
                self.browser.selected = all.into_iter().filter(|p| !cur.contains(p)).collect();
            }
            CopyTo | MoveTo => {
                let files = self.targets();
                if !files.is_empty() {
                    self.transfer = Some(TransferDialog { files, do_move: cmd == MoveTo });
                }
            }
            Delete => {
                let files = self.targets();
                if files.is_empty() {
                    return;
                }
                if self.core.settings.confirm_delete {
                    self.confirm = Some(Confirm::Delete(files));
                } else {
                    self.delete_files(files);
                }
            }
            Rename => {
                if let Some(p) = self.targets().first().cloned() {
                    if self.browser.selected.len() > 1 && self.mode == Mode::Browser {
                        self.run(BatchRename);
                        return;
                    }
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    self.prompt = Some((Prompt::new("Rename", &name, "New name"), PendingPrompt::Rename(p)));
                }
            }
            NewFolder => {
                let parent = self.browser.folder.clone();
                self.prompt = Some((Prompt::new("New folder", "New folder", "Folder name"), PendingPrompt::NewFolder(parent)));
            }
            AddFavorite => {
                let f = self.browser.folder.clone();
                if !self.core.settings.favorites.contains(&f) {
                    self.core.settings.favorites.push(f);
                    self.core.toasts.success("Added to favorites");
                }
            }
            ShowInExplorer => {
                if let Some(p) = self.targets().first() {
                    fileops::show_in_explorer(p);
                } else {
                    fileops::open_with_default(&self.browser.folder);
                }
            }
            CopyPath => {
                let t: Vec<String> = self.targets().iter().map(|p| p.display().to_string()).collect();
                if !t.is_empty() {
                    fileops::copy_text(&t.join("\r\n"));
                    self.core.toasts.info(if t.len() == 1 { "Path copied".to_string() } else { format!("{} paths copied", t.len()) });
                }
            }
            CopyFiles => {
                if self.mode == Mode::Viewer {
                    self.run(CopyImage);
                    return;
                }
                let files = self.targets();
                if !files.is_empty() {
                    match fileops::copy_files_to_clipboard(&files) {
                        Ok(()) => self.core.toasts.info(format!("{} file(s) copied — paste in Explorer or another folder", files.len())),
                        Err(e) => self.core.toasts.error(e.to_string()),
                    }
                }
            }
            PasteFiles => {
                if self.mode == Mode::Viewer {
                    self.run(PasteImage);
                    return;
                }
                match fileops::paste_files_from_clipboard() {
                    Ok(files) if !files.is_empty() => {
                        let (done, errs) = fileops::transfer(&files, &self.browser.folder.clone(), false, fileops::Conflict::Rename);
                        self.report_transfer(done.len(), errs, false);
                        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                    }
                    _ => {
                        // Maybe an image bitmap: save it as a new PNG.
                        if let Ok(img) = fileops::paste_image_from_clipboard() {
                            let ts = chrono::Local::now().format("Pasted %Y-%m-%d %H%M%S.png").to_string();
                            let path = fileops::unique_path(&self.browser.folder.join(ts));
                            let opts = SaveOptions { format: crate::formats::SaveFormat::Png, ..Default::default() };
                            match crate::encode::save(&img, &path, &opts) {
                                Ok(()) => {
                                    self.core.toasts.success(format!("Saved clipboard image as {}", path.file_name().unwrap().to_string_lossy()));
                                    self.browser.pending_select = Some(path);
                                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                                }
                                Err(e) => self.core.toasts.error(e.to_string()),
                            }
                        } else {
                            self.core.toasts.info("Nothing to paste");
                        }
                    }
                }
            }
            ThumbBigger => self.core.settings.thumb_size = (self.core.settings.thumb_size * 1.15).min(384.0),
            ThumbSmaller => self.core.settings.thumb_size = (self.core.settings.thumb_size / 1.15).max(80.0),
            ToggleDetails => {
                self.core.settings.browser_view = match self.core.settings.browser_view {
                    BrowserView::Thumbnails => BrowserView::Details,
                    BrowserView::Details => BrowserView::Thumbnails,
                };
                if self.core.settings.browser_view == BrowserView::Details {
                    for e in self.browser.entries.iter().filter(|e| e.kind == EntryKind::Image) {
                        self.core.meta.request(&e.path);
                    }
                }
            }
            TogglePreviewPane => self.core.settings.show_preview_pane = !self.core.settings.show_preview_pane,
            FocusFilter => {
                self.mode = Mode::Browser;
                self.browser.focus_filter = true;
            }
            BatchConvert => {
                let mut files = self.media_targets();
                if files.len() <= 1 && self.mode == Mode::Browser && self.browser.selected.len() <= 1 {
                    files = self.browser.image_paths();
                }
                self.batch_convert = Some(crate::ui::batch::BatchConvert::new(files, self.browser.folder.clone(), &self.core));
            }
            BatchRename => {
                let mut files = self.media_targets();
                if files.len() <= 1 {
                    files = self.browser.media_paths();
                }
                self.batch_rename = Some(crate::ui::batch::BatchRename::new(files, &self.core));
            }
            ContactSheet => {
                let mut files = self.media_targets();
                if files.len() <= 1 {
                    files = self.browser.image_paths();
                }
                self.contact_sheet = Some(crate::ui::tools::ContactSheet::new(files, &self.core));
            }
            FindDuplicates => {
                let files = self.browser.image_paths();
                self.duplicates = Some(crate::ui::tools::Duplicates::new(self.browser.folder.clone(), files, &self.core));
            }
            Slideshow => {
                let (list, start) = if self.mode == Mode::Viewer {
                    (self.viewer.list.clone(), self.viewer.index)
                } else {
                    let sel = self.browser.selected_paths();
                    let images: Vec<PathBuf> = sel.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
                    if images.len() >= 2 {
                        (images, 0)
                    } else {
                        let all = self.browser.image_paths();
                        let start = self.browser.focused_path().and_then(|f| all.iter().position(|p| p == &f)).unwrap_or(0);
                        (all, start)
                    }
                };
                if list.is_empty() {
                    self.core.toasts.info("No images to show");
                    return;
                }
                self.slideshow = Some(crate::ui::slideshow::Slideshow::new(list, start, &self.core));
                self.mode = Mode::Slideshow;
                self.set_fullscreen(true);
            }
            Compare => {
                let sel: Vec<PathBuf> = self.browser.selected_paths().into_iter().filter(|p| !p.is_dir()).collect();
                let vids = sel.iter().filter(|p| crate::video::is_video_ext(&crate::formats::ext_of(p))).count();
                if vids >= 2 {
                    self.video = Some(crate::video::VideoMode::new(sel, &self.core));
                    self.mode = Mode::Video;
                    return;
                }
                let imgs: Vec<PathBuf> = sel.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
                let imgs = if imgs.len() >= 2 {
                    imgs
                } else if self.mode == Mode::Viewer && self.viewer.list.len() >= 2 {
                    let i = self.viewer.index;
                    let j = if i + 1 < self.viewer.list.len() { i + 1 } else { i - 1 };
                    vec![self.viewer.list[i].clone(), self.viewer.list[j].clone()]
                } else {
                    self.core.toasts.info("Select 2–4 images (or 2 videos) to compare");
                    return;
                };
                self.compare = Some(crate::ui::compare::Compare::new(imgs.into_iter().take(4).collect(), &self.core));
                self.mode = Mode::Compare;
            }
            ToggleFlag => {
                let t = self.media_targets();
                let all_flagged = t.iter().all(|p| self.core.tags.get(p).flagged);
                for p in &t {
                    self.core.tags.update(p, |tag| tag.flagged = !all_flagged);
                }
                if self.browser.flagged_only {
                    self.browser.mark_resort();
                }
            }
            Rate(n) => {
                let t = self.media_targets();
                for p in &t {
                    self.core.tags.update(p, |tag| tag.rating = n);
                }
                if !t.is_empty() {
                    self.core.toasts.info(if n == 0 { "Rating cleared".to_string() } else { format!("Rated {}", "★".repeat(n as usize)) });
                }
                if self.core.settings.sort_key == crate::settings::SortKey::Rating || self.browser.min_rating > 0 {
                    self.browser.mark_resort();
                }
            }
            Label(n) => {
                for p in &self.media_targets() {
                    self.core.tags.update(p, |tag| tag.label = n);
                }
            }
            ShowFlaggedOnly => {
                self.browser.flagged_only = !self.browser.flagged_only;
                self.browser.mark_resort();
            }
            OpenVideoMode => {
                let vids: Vec<PathBuf> = self.browser.selected_paths().into_iter().filter(|p| crate::video::is_video_ext(&crate::formats::ext_of(p))).collect();
                self.video = Some(crate::video::VideoMode::new(vids, &self.core));
                self.mode = Mode::Video;
            }
            OpenViewer => {
                if let Some(f) = self.browser.focus {
                    self.open_viewer_at(f);
                }
            }
            CloseViewer => match self.mode {
                Mode::Viewer => self.close_viewer(),
                Mode::Browser => {}
                _ => self.mode = Mode::Browser,
            },
            NextImage => {
                self.viewer.step(1, wrap, fit);
            }
            PrevImage => {
                self.viewer.step(-1, wrap, fit);
            }
            FirstImage => {
                let d = -(self.viewer.index as isize);
                self.viewer.step(d, false, fit);
            }
            LastImage => {
                let d = self.viewer.list.len() as isize - 1 - self.viewer.index as isize;
                self.viewer.step(d, false, fit);
            }
            ZoomIn => self.viewer.zoom_by(1.25, None),
            ZoomOut => self.viewer.zoom_by(0.8, None),
            ZoomActual => {
                if (self.viewer.zoom - 1.0).abs() < 0.001 && self.viewer.auto_fit.is_none() {
                    self.viewer.reset_view(fit);
                } else {
                    self.viewer.set_zoom(1.0, None);
                }
            }
            ZoomFit => self.viewer.reset_view(if self.viewer.auto_fit == Some(fit) && fit == FitMode::FitLarge { FitMode::FitAlways } else { fit }),
            CycleFit => {
                let cur = self.viewer.auto_fit.unwrap_or(fit);
                let i = FitMode::ALL.iter().position(|m| *m == cur).unwrap_or(0);
                let next = FitMode::ALL[(i + 1) % FitMode::ALL.len()];
                self.viewer.reset_view(next);
                self.core.toasts.info(next.label());
            }
            RotateLeft | RotateRight | FlipHorizontal | FlipVertical => {
                let t = match cmd {
                    RotateLeft => Transform::Rotate270,
                    RotateRight => Transform::Rotate90,
                    FlipHorizontal => Transform::FlipH,
                    _ => Transform::FlipV,
                };
                self.transform(t);
            }
            Crop => {
                if self.ensure_viewer() {
                    self.viewer.start_crop(&self.core);
                }
            }
            AutoEnhance => {
                if self.ensure_viewer() && !self.viewer.apply_simple(&self.core, "Auto enhance", crate::edit::ops::auto_levels) {
                    self.core.toasts.info("Image is still loading");
                }
            }
            Resize | CanvasSize | AdjustColors | Levels | Curves | Sharpen | Blur | Effects | AddText | AddBorder | Watermark | FreeRotate => {
                if !self.ensure_viewer() {
                    return;
                }
                match edit_tools::make_tool(cmd) {
                    Some(tool) => self.viewer.open_tool(tool, &self.core),
                    None => self.core.toasts.info(format!("{} is coming soon", cmd.label())),
                }
            }
            Undo => {
                if let Some(e) = &mut self.viewer.edit {
                    if let Some(w) = e.undo() {
                        self.core.toasts.info(format!("Undo: {w}"));
                        self.viewer.reset_view(fit);
                    }
                }
            }
            Redo => {
                if let Some(e) = &mut self.viewer.edit {
                    if let Some(w) = e.redo() {
                        self.core.toasts.info(format!("Redo: {w}"));
                        self.viewer.reset_view(fit);
                    }
                }
            }
            Save => self.save(false),
            SaveAs => self.save(true),
            CopyImage => {
                let img = if self.mode == Mode::Viewer { self.viewer.current_image(&self.core) } else { None };
                let img = img.or_else(|| self.targets().first().and_then(|p| crate::decode::decode_file(p, Default::default()).ok()).map(|d| d.image));
                match img {
                    Some(img) => match fileops::copy_image_to_clipboard(&img) {
                        Ok(()) => self.core.toasts.info("Image copied to clipboard"),
                        Err(e) => self.core.toasts.error(format!("Copy failed: {e}")),
                    },
                    None => self.core.toasts.info("Image is still loading"),
                }
            }
            PasteImage => match fileops::paste_image_from_clipboard() {
                Ok(img) => {
                    if self.mode == Mode::Viewer && self.viewer.current().is_some() {
                        if let Some(sess) = self.viewer.ensure_edit(&self.core) {
                            sess.push(img, "Paste");
                            self.viewer.reset_view(fit);
                        }
                    } else {
                        self.run(PasteFiles);
                    }
                }
                Err(e) => self.core.toasts.info(e.to_string()),
            },
            SetWallpaper => self.set_wallpaper(),
            Print => {
                let img = self.viewer.current_image(&self.core).or_else(|| self.targets().first().and_then(|p| crate::decode::decode_file(p, Default::default()).ok()).map(|d| d.image));
                if let Some(img) = img {
                    let title = self.targets().first().map(|p| crate::ui::viewer::file_name(p)).unwrap_or_else(|| "Lumen".into());
                    if let Err(e) = crate::ui::tools::print_image(&img, &title) {
                        self.core.toasts.error(e.to_string());
                    }
                }
            }
            ToggleInfo => self.viewer.show_info = !self.viewer.show_info,
            ToggleHistogram => self.viewer.show_info = !self.viewer.show_info,
            ToggleMagnifier => self.viewer.magnifier_locked = !self.viewer.magnifier_locked,
            ToggleFilmstrip => self.viewer.show_filmstrip = !self.viewer.show_filmstrip,
            PlayPause => self.viewer.toggle_animation(),
            NextFrame => self.viewer.step_frame(&self.core, 1),
            PrevFrame => self.viewer.step_frame(&self.core, -1),
            OpenWithEditor => {
                if let Some(p) = self.targets().first() {
                    match self.core.settings.external_editors.first() {
                        Some(ed) => {
                            if let Err(e) = fileops::open_with(&ed.exe, p) {
                                self.core.toasts.error(format!("Could not start {}: {e}", ed.name));
                            }
                        }
                        None => fileops::open_with_dialog(p),
                    }
                }
            }
            Properties => {
                if let Some(p) = self.targets().first() {
                    fileops::properties_dialog(p);
                }
            }
        }
    }

    /// Make sure the viewer shows the focused image (for edit commands issued from the browser).
    fn ensure_viewer(&mut self) -> bool {
        if self.mode != Mode::Viewer {
            let Some(f) = self.browser.focus else { return false };
            if self.browser.item(f).map(|e| e.kind != EntryKind::Image).unwrap_or(true) {
                return false;
            }
            self.open_viewer_at(f);
        }
        if self.viewer.current_image(&self.core).is_none() {
            self.core.toasts.info("Image is still loading…");
            return false;
        }
        true
    }

    fn transform(&mut self, t: Transform) {
        let fit = self.core.settings.fit_mode;
        // Lossless path: unedited JPEG → rewrite EXIF orientation only.
        if self.core.settings.lossless_jpeg_rotate && !self.viewer.is_dirty() {
            let targets = self.media_targets();
            let jpegs: Vec<PathBuf> = targets.iter().filter(|p| crate::formats::is_jpeg_ext(&crate::formats::ext_of(p))).cloned().collect();
            if !jpegs.is_empty() && jpegs.len() == targets.len() {
                let mut ok = 0;
                for p in &jpegs {
                    match crate::encode::lossless_jpeg_transform(p, t) {
                        Ok(_) => {
                            ok += 1;
                            self.core.loader.invalidate(p);
                            self.core.thumbs.forget(p);
                            self.core.meta.forget(p);
                        }
                        Err(e) => self.core.toasts.error(format!("{}: {e}", p.display())),
                    }
                }
                if ok > 0 {
                    self.viewer.edit = None;
                    self.viewer.reset_view(fit);
                    self.core.toasts.info(format!("{} — lossless{}", t.label(), if ok > 1 { format!(" ({ok} files)") } else { String::new() }));
                }
                return;
            }
        }
        if self.mode == Mode::Browser {
            // Batch rotate non-JPEG files: decode, transform, re-save in place.
            let targets = self.media_targets();
            let mut ok = 0;
            for p in &targets {
                let Some(fmt) = crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(p)) else { continue };
                if let Ok(d) = crate::decode::decode_file(p, Default::default()) {
                    let out = t.apply(&d.image);
                    let opts = SaveOptions { format: fmt, jpeg_quality: self.core.settings.jpeg_quality, exif: d.exif.clone(), ..Default::default() };
                    if crate::encode::save(&out, p, &opts).is_ok() {
                        ok += 1;
                        self.core.loader.invalidate(p);
                        self.core.thumbs.forget(p);
                    }
                }
            }
            if ok > 0 {
                self.core.toasts.info(format!("{} applied to {ok} file(s)", t.label()));
            }
            return;
        }
        if !self.viewer.apply_simple(&self.core, t.label(), |img| t.apply(img)) {
            self.core.toasts.info("Image is still loading");
        }
        self.viewer.reset_view(fit);
    }

    fn save(&mut self, save_as: bool) {
        let Some(path) = self.viewer.current().cloned().filter(|_| self.mode == Mode::Viewer) else {
            self.core.toasts.info("Open an image in the viewer to save it");
            return;
        };
        let Some(img) = self.viewer.current_image(&self.core) else { return };
        let loaded = self.viewer.loaded(&self.core);
        let mut opts = SaveOptions { jpeg_quality: self.core.settings.jpeg_quality, keep_metadata: self.core.settings.keep_exif_on_save, ..Default::default() };
        if let Some(l) = &loaded {
            opts.exif = l.decoded.exif.clone();
            if !self.core.settings.color_manage {
                opts.icc = l.decoded.icc_profile.clone();
            }
        }
        let fmt = crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(&path));
        if save_as || fmt.is_none() {
            let target = if fmt.is_none() { path.with_extension("jpg") } else { path.clone() };
            self.save_dialog = Some(SaveDialog::new(target, img, opts));
            return;
        }
        if !self.viewer.is_dirty() {
            self.core.toasts.info("No changes to save");
            return;
        }
        opts.format = fmt.unwrap();
        self.do_save(&path, &img, &opts);
    }

    fn do_save(&mut self, path: &Path, img: &image::RgbaImage, opts: &SaveOptions) -> bool {
        match crate::encode::save(img, path, opts) {
            Ok(()) => {
                let same = self.viewer.current().map(|c| c == path).unwrap_or(false);
                if same {
                    if let Some(e) = &mut self.viewer.edit {
                        e.saved_version = e.version;
                    }
                }
                self.core.thumbs.forget(path);
                self.core.meta.forget(path);
                if !same {
                    self.core.loader.invalidate(path);
                }
                self.core.toasts.success(format!("Saved {}", crate::ui::viewer::file_name(path)));
                if path.parent() == Some(self.browser.folder.as_path()) {
                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                }
                true
            }
            Err(e) => {
                self.core.toasts.error(format!("Save failed: {e}"));
                false
            }
        }
    }

    fn set_wallpaper(&mut self) {
        let img = if self.mode == Mode::Viewer { self.viewer.current_image(&self.core) } else { None };
        let path = self.targets().first().cloned();
        let target = if let (Some(p), false) = (&path, self.viewer.is_dirty()) {
            let ext = crate::formats::ext_of(p);
            if matches!(ext.as_str(), "jpg" | "jpeg" | "bmp" | "png") {
                Some(p.clone())
            } else {
                None
            }
        } else {
            None
        };
        let target = match target {
            Some(t) => t,
            None => {
                let img = img.or_else(|| path.as_ref().and_then(|p| crate::decode::decode_file(p, Default::default()).ok()).map(|d| d.image));
                let Some(img) = img else { return };
                let out = crate::settings::data_dir().join("wallpaper.jpg");
                let _ = std::fs::create_dir_all(crate::settings::data_dir());
                let opts = SaveOptions { jpeg_quality: 95, ..Default::default() };
                if let Err(e) = crate::encode::save(&img, &out, &opts) {
                    self.core.toasts.error(e.to_string());
                    return;
                }
                out
            }
        };
        match fileops::set_wallpaper(&target, 0) {
            Ok(()) => self.core.toasts.success("Desktop wallpaper set"),
            Err(e) => self.core.toasts.error(e.to_string()),
        }
    }

    fn delete_files(&mut self, files: Vec<PathBuf>) {
        match fileops::delete_to_trash(&files) {
            Ok(()) => {
                for f in &files {
                    self.core.tags.removed(f);
                    self.core.loader.invalidate(f);
                    self.core.thumbs.forget(f);
                    self.viewer.remove_path(f);
                    self.browser.selected.remove(f);
                }
                self.core.toasts.info(format!("Moved {} item(s) to the Recycle Bin", files.len()));
                if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
                    self.mode = Mode::Browser;
                }
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
            Err(e) => self.core.toasts.error(e.to_string()),
        }
    }

    fn report_transfer(&mut self, n: usize, errs: Vec<String>, moved: bool) {
        if !errs.is_empty() {
            self.core.toasts.error(format!("{} failed: {}", errs.len(), errs.first().cloned().unwrap_or_default()));
        }
        if n > 0 {
            self.core.toasts.success(format!("{} {n} item(s)", if moved { "Moved" } else { "Copied" }));
        }
    }

    fn transfer_files(&mut self, files: Vec<PathBuf>, dest: PathBuf, do_move: bool) {
        let (done, errs) = fileops::transfer(&files, &dest, do_move, fileops::Conflict::Rename);
        for (from, to) in &done {
            if do_move {
                self.core.tags.moved(from, to);
                self.core.loader.invalidate(from);
                self.viewer.remove_path(from);
            } else {
                self.core.tags.copied(from, to);
            }
        }
        self.report_transfer(done.len(), errs, do_move);
        self.core.settings.copy_move_targets.retain(|p| p != &dest);
        self.core.settings.copy_move_targets.insert(0, dest.clone());
        self.core.settings.copy_move_targets.truncate(10);
        self.browser.tree.invalidate(&dest);
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
            self.mode = Mode::Browser;
        }
    }

    fn apply_theme(&mut self) {
        let dark = match self.core.settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => !matches!(self.core.ctx.system_theme(), Some(egui::Theme::Light)),
        };
        self.effective_dark = dark;
        self.core.pal = Palette::new(dark, self.core.settings.accent);
        theme::apply(&self.core.ctx, &self.core.pal, self.core.settings.ui_scale);
    }

    // ------------------------------------------------------------ shortcuts

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.palette.open || self.prompt.is_some() || self.confirm.is_some() || self.save_dialog.is_some() || self.viewer.confirm_discard {
            return;
        }
        let typing = ctx.egui_wants_keyboard_input();
        let mut fire: Vec<Command> = Vec::new();
        let ctrl = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        let none = Modifiers::NONE;
        let mut take = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));

        // Global (work even while typing)
        if take(ctrl, Key::K) {
            fire.push(Command::CommandPalette);
        }
        if take(none, Key::F11) {
            fire.push(Command::ToggleFullscreen);
        }
        if take(ctrl, Key::Comma) {
            fire.push(Command::Settings);
        }
        if take(none, Key::F1) {
            fire.push(Command::ShortcutsHelp);
        }
        if take(cs, Key::O) {
            fire.push(Command::OpenFolder);
        }
        if take(ctrl, Key::O) {
            fire.push(Command::OpenFile);
        }
        if take(cs, Key::T) {
            fire.push(Command::ToggleTheme);
        }
        if take(cs, Key::V) {
            fire.push(Command::OpenVideoMode);
        }
        if typing {
            for c in fire {
                self.run(c);
            }
            return;
        }
        let mode = self.mode;
        let viewer_tool = self.viewer.has_tool();
        match mode {
            Mode::Browser => {
                let table: &[(Modifiers, Key, Command)] = &[
                    (none, Key::F5, Command::Refresh),
                    (none, Key::Backspace, Command::GoUp),
                    (alt, Key::ArrowUp, Command::GoUp),
                    (alt, Key::ArrowLeft, Command::GoBack),
                    (alt, Key::ArrowRight, Command::GoForward),
                    (ctrl, Key::A, Command::SelectAll),
                    (ctrl, Key::D, Command::SelectNone),
                    (ctrl, Key::I, Command::InvertSelection),
                    (none, Key::F7, Command::CopyTo),
                    (none, Key::F8, Command::MoveTo),
                    (none, Key::Delete, Command::Delete),
                    (ctrl, Key::F2, Command::BatchRename),
                    (none, Key::F2, Command::Rename),
                    (cs, Key::N, Command::NewFolder),
                    (cs, Key::C, Command::CopyPath),
                    (ctrl, Key::C, Command::CopyFiles),
                    (ctrl, Key::V, Command::PasteFiles),
                    (ctrl, Key::Plus, Command::ThumbBigger),
                    (ctrl, Key::Equals, Command::ThumbBigger),
                    (ctrl, Key::Minus, Command::ThumbSmaller),
                    (ctrl, Key::F, Command::FocusFilter),
                    (none, Key::F3, Command::BatchConvert),
                    (cs, Key::D, Command::ToggleDetails),
                    (cs, Key::P, Command::TogglePreviewPane),
                    (none, Key::S, Command::Slideshow),
                    (none, Key::P, Command::Compare),
                    (none, Key::Space, Command::ToggleFlag),
                    (none, Key::Backtick, Command::ToggleFlag),
                    (none, Key::L, Command::RotateLeft),
                    (none, Key::R, Command::RotateRight),
                    (none, Key::E, Command::OpenWithEditor),
                    (alt, Key::Enter, Command::Properties),
                    (ctrl, Key::W, Command::SetWallpaper),
                ];
                for (m, k, c) in table {
                    if take(*m, *k) {
                        fire.push(*c);
                    }
                }
                for (i, k) in [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].iter().enumerate() {
                    if take(none, *k) {
                        fire.push(Command::Rate(i as u8));
                    }
                }
                for (i, k) in [Key::Num6, Key::Num7, Key::Num8, Key::Num9].iter().enumerate() {
                    if take(none, *k) {
                        fire.push(Command::Label(i as u8 + 1));
                    }
                }
                // Remaining letter presses start a type-ahead search.
                let texts: Vec<String> = ctx.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.clone()) } else { None }).collect());
                if fire.is_empty() {
                    if let Some(t) = texts.first() {
                        let c = t.chars().next().unwrap_or(' ');
                        if c.is_alphabetic() && !"SPLRE".contains(c.to_ascii_uppercase()) {
                            self.browser.typeahead_start(t);
                        }
                    }
                }
            }
            Mode::Viewer => {
                if viewer_tool {
                    // Tools own Enter/Escape; keep undo available.
                    if take(ctrl, Key::Z) {
                        fire.push(Command::Undo);
                    }
                } else {
                    let table: &[(Modifiers, Key, Command)] = &[
                        (none, Key::Escape, Command::CloseViewer),
                        (none, Key::ArrowRight, Command::NextImage),
                        (none, Key::PageDown, Command::NextImage),
                        (none, Key::Space, Command::NextImage),
                        (none, Key::ArrowLeft, Command::PrevImage),
                        (none, Key::PageUp, Command::PrevImage),
                        (none, Key::Backspace, Command::PrevImage),
                        (shift, Key::Space, Command::PrevImage),
                        (none, Key::Home, Command::FirstImage),
                        (none, Key::End, Command::LastImage),
                        (none, Key::Plus, Command::ZoomIn),
                        (none, Key::Equals, Command::ZoomIn),
                        (ctrl, Key::Plus, Command::ZoomIn),
                        (ctrl, Key::Equals, Command::ZoomIn),
                        (none, Key::Minus, Command::ZoomOut),
                        (ctrl, Key::Minus, Command::ZoomOut),
                        (ctrl, Key::Num0, Command::ZoomActual),
                        (none, Key::Z, Command::ZoomActual),
                        (none, Key::Slash, Command::ZoomFit),
                        (none, Key::L, Command::RotateLeft),
                        (none, Key::R, Command::RotateRight),
                        (none, Key::H, Command::FlipHorizontal),
                        (none, Key::V, Command::FlipVertical),
                        (none, Key::X, Command::Crop),
                        (none, Key::C, Command::Crop),
                        (cs, Key::R, Command::CanvasSize),
                        (ctrl, Key::R, Command::Resize),
                        (ctrl, Key::E, Command::AdjustColors),
                        (ctrl, Key::L, Command::Levels),
                        (ctrl, Key::M, Command::Curves),
                        (ctrl, Key::J, Command::Sharpen),
                        (ctrl, Key::B, Command::Blur),
                        (cs, Key::E, Command::Effects),
                        (cs, Key::A, Command::AutoEnhance),
                        (none, Key::T, Command::AddText),
                        (cs, Key::Z, Command::Redo),
                        (ctrl, Key::Z, Command::Undo),
                        (ctrl, Key::Y, Command::Redo),
                        (cs, Key::S, Command::SaveAs),
                        (ctrl, Key::S, Command::Save),
                        (ctrl, Key::C, Command::CopyImage),
                        (ctrl, Key::V, Command::PasteImage),
                        (ctrl, Key::W, Command::SetWallpaper),
                        (ctrl, Key::P, Command::Print),
                        (none, Key::I, Command::ToggleInfo),
                        (ctrl, Key::H, Command::ToggleHistogram),
                        (none, Key::M, Command::ToggleMagnifier),
                        (none, Key::F, Command::ToggleFilmstrip),
                        (none, Key::K, Command::PlayPause),
                        (none, Key::Period, Command::NextFrame),
                        (none, Key::Comma, Command::PrevFrame),
                        (none, Key::E, Command::OpenWithEditor),
                        (none, Key::Delete, Command::Delete),
                        (none, Key::F2, Command::Rename),
                        (none, Key::F7, Command::CopyTo),
                        (none, Key::F8, Command::MoveTo),
                        (none, Key::S, Command::Slideshow),
                        (none, Key::P, Command::Compare),
                        (none, Key::Backtick, Command::ToggleFlag),
                        (alt, Key::Enter, Command::Properties),
                        (none, Key::Enter, Command::ToggleFullscreen),
                    ];
                    for (m, k, c) in table {
                        if take(*m, *k) {
                            fire.push(*c);
                        }
                    }
                    for (i, k) in [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].iter().enumerate() {
                        if take(none, *k) {
                            fire.push(Command::Rate(i as u8));
                        }
                    }
                    for (i, k) in [Key::Num6, Key::Num7, Key::Num8, Key::Num9].iter().enumerate() {
                        if take(none, *k) {
                            fire.push(Command::Label(i as u8 + 1));
                        }
                    }
                    let star = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "*")));
                    if star {
                        fire.push(Command::CycleFit);
                    }
                }
            }
            _ => {}
        }
        for c in fire {
            self.run(c);
        }
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let p = self.core.pal;
        if let Some((prompt, kind)) = &mut self.prompt {
            match widgets::show_prompt(ctx, prompt) {
                Some(true) => {
                    let value = prompt.value.clone();
                    match kind {
                        PendingPrompt::Rename(path) => {
                            let path = path.clone();
                            match fileops::rename(&path, &value) {
                                Ok(new) => {
                                    self.core.tags.moved(&path, &new);
                                    self.core.loader.invalidate(&path);
                                    self.viewer.replace_path(&path, &new);
                                    self.browser.pending_select = Some(new);
                                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                                }
                                Err(e) => self.core.toasts.error(e.to_string()),
                            }
                        }
                        PendingPrompt::NewFolder(parent) => {
                            let parent = parent.clone();
                            match fileops::new_folder(&parent, &value) {
                                Ok(p) => {
                                    self.browser.tree.invalidate(&parent);
                                    if parent == self.browser.folder {
                                        self.browser.pending_select = Some(p);
                                        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                                    }
                                }
                                Err(e) => self.core.toasts.error(e.to_string()),
                            }
                        }
                    }
                    self.prompt = None;
                }
                Some(false) => self.prompt = None,
                None => {}
            }
        }
        if let Some(Confirm::Delete(files)) = &self.confirm {
            let files = files.clone();
            let body = if files.len() == 1 {
                format!("Move \"{}\" to the Recycle Bin?", crate::ui::viewer::file_name(&files[0]))
            } else {
                format!("Move {} items to the Recycle Bin?", files.len())
            };
            match widgets::show_confirm(ctx, "Delete", &body, "Delete", true, &p) {
                Some(true) => {
                    self.confirm = None;
                    self.delete_files(files);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        if let Some(t) = &self.transfer {
            let mut close = false;
            let mut go: Option<PathBuf> = None;
            let title = if t.do_move { "Move to folder" } else { "Copy to folder" };
            let n = t.files.len();
            egui::Modal::new(egui::Id::new("transfer")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading(title);
                ui.label(RichText::new(format!("{n} item(s)")).color(p.muted));
                ui.add_space(8.0);
                if ui.add(egui::Button::new(format!("{}  Browse for a folder…", I::FOLDER_OPEN)).min_size(Vec2::new(ui.available_width(), 30.0))).clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_directory(&self.browser.folder).pick_folder() {
                        go = Some(d);
                    }
                }
                let recents: Vec<PathBuf> = self.core.settings.copy_move_targets.iter().chain(self.core.settings.favorites.iter()).filter(|p| p.is_dir()).cloned().collect();
                if !recents.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Recent & favorite folders").color(p.faint).size(11.5));
                    for (i, d) in recents.iter().enumerate().take(12) {
                        let label = format!("{}  {}", if i < 9 { format!("{}", i + 1) } else { " ".into() }, d.display());
                        if ui.add(egui::Button::new(label).frame(false)).clicked() {
                            go = Some(d.clone());
                        }
                    }
                    for (i, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9].iter().enumerate() {
                        if ui.input(|inp| inp.key_pressed(*k)) {
                            if let Some(d) = recents.get(i) {
                                go = Some(d.clone());
                            }
                        }
                    }
                }
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
            if let Some(dest) = go {
                let t = self.transfer.take().unwrap();
                self.transfer_files(t.files, dest, t.do_move);
            } else if close {
                self.transfer = None;
            }
        }
        if let Some(dlg) = &mut self.save_dialog {
            match dlg.show(ctx, &p) {
                SaveOutcome::Open => {}
                SaveOutcome::Cancel => self.save_dialog = None,
                SaveOutcome::Save(path, opts) => {
                    if let Some(img) = self.viewer.current_image(&self.core) {
                        if self.do_save(&path, &img, &opts) {
                            self.save_dialog = None;
                            self.core.settings.jpeg_quality = opts.jpeg_quality;
                            // Saving to a new file: show it.
                            if self.viewer.current() != Some(&path) && path.parent() == Some(self.browser.folder.as_path()) {
                                if let Some(e) = &mut self.viewer.edit {
                                    e.saved_version = e.version;
                                }
                            }
                        } else if let Some(d) = &mut self.save_dialog {
                            d.error = Some("Could not save — see the message below.".into());
                        }
                    }
                }
            }
        }
        if self.show_settings {
            let mut open = true;
            if crate::ui::settings_window::show(ctx, &mut self.core, &mut open) {
                self.apply_theme();
                self.core.loader.budget_bytes = self.core.settings.cache_budget_mb.max(256) << 20;
                self.core.loader.opts.lock().color_manage = self.core.settings.color_manage;
                self.core.thumbs.set_disk_cache(self.core.settings.thumb_disk_cache);
            }
            self.show_settings = open;
        }
        if self.show_help {
            self.help_window(ctx);
        }
        if self.show_about {
            let mut open = true;
            egui::Window::new("About Lumen").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::include_image!("../assets/lumen-256.png")).fit_to_exact_size(Vec2::splat(56.0)));
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Lumen").font(theme::semibold(22.0)));
                        ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(p.muted));
                    });
                });
                ui.add_space(8.0);
                ui.label("A fast, private image & video viewer, browser, editor and converter for Windows.");
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{}  100% offline. No telemetry, no accounts, no network access.", I::SHIELD_CHECK)).color(p.ok));
                ui.add_space(8.0);
                ui.hyperlink_to("github.com/RahulTarigoppula/lumen-image-viewer", "https://github.com/RahulTarigoppula/lumen-image-viewer");
                ui.label(RichText::new("MIT licensed. Icons: Phosphor (MIT). Video decoding: FFmpeg (LGPL).").size(11.5).color(p.faint));
            });
            self.show_about = open;
        }
        // Tool windows
        if let Some(w) = &mut self.batch_convert {
            if !w.show(ctx, &mut self.core) {
                self.batch_convert = None;
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
        }
        if let Some(w) = &mut self.batch_rename {
            let (open, renamed) = w.show(ctx, &mut self.core);
            for (from, to) in &renamed {
                self.core.tags.moved(from, to);
                self.viewer.replace_path(from, to);
            }
            if !renamed.is_empty() {
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
            if !open {
                self.batch_rename = None;
            }
        }
        if let Some(w) = &mut self.contact_sheet {
            if !w.show(ctx, &mut self.core) {
                self.contact_sheet = None;
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
        }
        if let Some(w) = &mut self.duplicates {
            let (open, events) = w.show(ctx, &mut self.core);
            for e in events {
                match e {
                    crate::ui::tools::DupEvent::Deleted(paths) => {
                        for f in &paths {
                            self.core.tags.removed(f);
                            self.core.thumbs.forget(f);
                            self.core.loader.invalidate(f);
                        }
                        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                    }
                    crate::ui::tools::DupEvent::Open(p) => {
                        self.open_file(&p);
                    }
                }
            }
            if !open {
                self.duplicates = None;
            }
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let p = self.core.pal;
        let mut open = true;
        egui::Window::new("Keyboard shortcuts").open(&mut open).default_size([640.0, 560.0]).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let groups: [(&str, Vec<Command>); 4] = [
                    ("General", vec![Command::CommandPalette, Command::OpenFile, Command::OpenFolder, Command::Settings, Command::ToggleFullscreen, Command::ToggleTheme, Command::ShortcutsHelp, Command::OpenVideoMode]),
                    ("Browser", vec![Command::OpenViewer, Command::Refresh, Command::GoUp, Command::GoBack, Command::GoForward, Command::SelectAll, Command::InvertSelection, Command::FocusFilter, Command::CopyFiles, Command::PasteFiles, Command::CopyTo, Command::MoveTo, Command::Rename, Command::BatchRename, Command::Delete, Command::NewFolder, Command::BatchConvert, Command::Slideshow, Command::Compare, Command::ToggleFlag, Command::Rate(3), Command::Label(1), Command::ThumbBigger, Command::ThumbSmaller, Command::ToggleDetails, Command::TogglePreviewPane]),
                    ("Viewer", vec![Command::CloseViewer, Command::NextImage, Command::PrevImage, Command::FirstImage, Command::LastImage, Command::ZoomIn, Command::ZoomOut, Command::ZoomActual, Command::ZoomFit, Command::CycleFit, Command::ToggleMagnifier, Command::ToggleInfo, Command::ToggleFilmstrip, Command::PlayPause, Command::NextFrame, Command::PrevFrame]),
                    ("Editing", vec![Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::FlipVertical, Command::Crop, Command::Resize, Command::CanvasSize, Command::AdjustColors, Command::Levels, Command::Curves, Command::Sharpen, Command::Blur, Command::Effects, Command::AutoEnhance, Command::AddText, Command::Undo, Command::Redo, Command::Save, Command::SaveAs, Command::CopyImage, Command::SetWallpaper, Command::Print]),
                ];
                for (title, cmds) in groups {
                    widgets::section_title(ui, &p, title);
                    egui::Grid::new(title).num_columns(2).striped(true).min_col_width(240.0).show(ui, |ui| {
                        for c in cmds {
                            ui.label(format!("{}  {}", c.icon(), c.label()));
                            ui.label(RichText::new(c.shortcut()).monospace().color(p.accent));
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                }
                widgets::section_title(ui, &p, "Mouse");
                for (k, v) in [
                    ("Wheel", "Next / previous image (or zoom — configurable in Settings)"),
                    ("Ctrl + wheel", "Zoom at cursor (thumbnail size in the browser)"),
                    ("Drag", "Pan a zoomed image"),
                    ("Press & hold", "Magnifier loupe (when the image fits the window)"),
                    ("Middle click", "Toggle fit / 100%"),
                    ("Double click", "Back to the browser"),
                    ("Back / forward buttons", "Previous / next image"),
                ] {
                    widgets::kv_row(ui, &p, k, v);
                }
            });
        });
        self.show_help = open;
    }

    // ------------------------------------------------------------ chrome

    fn menu(&mut self, ui: &mut egui::Ui) {
        let mut fire = None;
        let p = self.core.pal;
        let item = |ui: &mut egui::Ui, c: Command, fire: &mut Option<Command>| {
            if ui.add(egui::Button::new(format!("{}   {}", c.icon(), c.label())).shortcut_text(RichText::new(c.shortcut()).color(p.faint))).clicked() {
                *fire = Some(c);
                ui.close();
            }
        };
        ui.menu_button(RichText::new(I::LIST).size(18.0), |ui| {
            ui.set_min_width(270.0);
            item(ui, Command::OpenFile, &mut fire);
            item(ui, Command::OpenFolder, &mut fire);
            ui.menu_button(format!("{}   Recent folders", I::CLOCK_COUNTER_CLOCKWISE), |ui| {
                for r in self.core.settings.recent_folders.clone() {
                    if ui.button(r.display().to_string()).clicked() {
                        self.navigate(r);
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.menu_button(format!("{}   Edit", I::PENCIL_SIMPLE), |ui| {
                for c in [Command::SelectAll, Command::SelectNone, Command::InvertSelection, Command::CopyFiles, Command::PasteFiles, Command::CopyTo, Command::MoveTo, Command::Rename, Command::Delete, Command::NewFolder] {
                    item(ui, c, &mut fire);
                }
            });
            ui.menu_button(format!("{}   View", I::EYE), |ui| {
                for c in [Command::ToggleDetails, Command::TogglePreviewPane, Command::ThumbBigger, Command::ThumbSmaller, Command::ShowFlaggedOnly, Command::ToggleFullscreen, Command::ToggleTheme] {
                    item(ui, c, &mut fire);
                }
            });
            ui.menu_button(format!("{}   Image", I::IMAGE), |ui| {
                for c in [Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::FlipVertical, Command::Crop, Command::Resize, Command::CanvasSize, Command::AutoEnhance, Command::AdjustColors, Command::Levels, Command::Curves, Command::Sharpen, Command::Blur, Command::Effects, Command::FreeRotate, Command::AddText, Command::AddBorder, Command::Watermark, Command::SetWallpaper, Command::Print] {
                    item(ui, c, &mut fire);
                }
            });
            ui.menu_button(format!("{}   Tools", I::WRENCH), |ui| {
                for c in [Command::BatchConvert, Command::BatchRename, Command::ContactSheet, Command::FindDuplicates, Command::Slideshow, Command::Compare, Command::OpenVideoMode] {
                    item(ui, c, &mut fire);
                }
            });
            ui.separator();
            item(ui, Command::CommandPalette, &mut fire);
            item(ui, Command::Settings, &mut fire);
            item(ui, Command::ShortcutsHelp, &mut fire);
            item(ui, Command::About, &mut fire);
            ui.separator();
            item(ui, Command::Exit, &mut fire);
        });
        if let Some(c) = fire {
            self.run(c);
        }
    }

    fn handle_dropped(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if dropped.is_empty() {
            return;
        }
        let first = dropped[0].clone();
        if self.mode == Mode::Video {
            return; // video mode handles its own drops
        }
        if first.is_dir() {
            self.navigate(first);
        } else if dropped.len() >= 2 && dropped.iter().all(|p| crate::video::is_video_ext(&crate::formats::ext_of(p))) {
            self.video = Some(crate::video::VideoMode::new(dropped, &self.core));
            self.mode = Mode::Video;
        } else {
            self.open_file(&first);
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.mode {
            Mode::Viewer => match self.viewer.current() {
                Some(p) => format!("{}{} — Lumen", crate::ui::viewer::file_name(p), if self.viewer.is_dirty() { " •" } else { "" }),
                None => "Lumen".into(),
            },
            Mode::Video => "Video — Lumen".into(),
            _ => format!("{} — Lumen", self.browser.folder.display()),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let protect: Vec<PathBuf> = self.viewer.current().cloned().into_iter().collect();
        let changed = self.core.loader.poll(&protect);
        let _ = changed;
        if self.core.meta.poll() && matches!(self.core.settings.sort_key, crate::settings::SortKey::Dimensions | crate::settings::SortKey::DateTaken) && self.core.meta.pending() == 0 {
            self.browser.mark_resort();
        }
        self.core.thumbs.begin_frame();
        let was_loading = self.browser.loading;
        self.browser.tick(&mut self.core);
        if was_loading && !self.browser.loading {
            self.sync_viewer_list();
        }
        if self.last_save.elapsed().as_secs() > 5 {
            self.core.tags.save_if_dirty();
            self.last_save = Instant::now();
        }
        let _ = ctx;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.dev_actions(&ctx);
        self.handle_dropped(&ctx);
        self.shortcuts(&ctx);
        self.update_title(&ctx);
        let p = self.core.pal;

        match self.mode {
            Mode::Browser => {
                let mut ev = Vec::new();
                egui::Panel::top("toolbar")
                    .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(8, 6)))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            self.menu(ui);
                            self.browser.toolbar(ui, &mut self.core, &mut ev);
                        });
                    });
                egui::Panel::bottom("status")
                    .exact_size(26.0)
                    .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(12, 4)))
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(RichText::new(self.browser.status_text(&self.core)).size(12.0).color(p.muted));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} Offline & private", I::SHIELD_CHECK)).size(11.5).color(p.faint))
                                    .on_hover_text("Lumen never connects to the internet. Ratings and tags are stored locally.");
                                if self.browser.loading {
                                    ui.spinner();
                                }
                            });
                        });
                    });
                let tree_w = self.core.settings.tree_width.clamp(180.0, 520.0);
                egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(tree_w)
                    .size_range(180.0..=560.0)
                    .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin { left: 0, right: 2, top: 4, bottom: 0 }))
                    .show(ui, |ui| {
                        self.core.settings.tree_width = ui.available_width() + 2.0;
                        self.browser.sidebar(ui, &mut self.core, &mut ev);
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 6, bottom: 0 }))
                    .show(ui, |ui| {
                        self.browser.central(ui, &mut self.core, &mut ev);
                    });
                for e in ev {
                    match e {
                        BrowserEvent::Navigate(path) => self.navigate(path),
                        BrowserEvent::Open(i) => self.open_viewer_at(i),
                        BrowserEvent::Command(c) => self.run(c),
                        BrowserEvent::DropFiles { target, files, copy } => {
                            let files: Vec<PathBuf> = files.into_iter().filter(|f| f != &target && !target.starts_with(f)).collect();
                            if !files.is_empty() {
                                self.transfer_files(files, target, !copy);
                            }
                        }
                        BrowserEvent::NewFolderIn(parent) => {
                            self.prompt = Some((Prompt::new("New folder", "New folder", "Folder name"), PendingPrompt::NewFolder(parent)));
                        }
                        BrowserEvent::AddFavorite(path) => {
                            if !self.core.settings.favorites.contains(&path) {
                                self.core.settings.favorites.push(path);
                            }
                        }
                        BrowserEvent::RemoveFavorite(path) => self.core.settings.favorites.retain(|f| f != &path),
                    }
                }
            }
            Mode::Viewer => {
                let fullscreen = self.fullscreen;
                let mut events = Vec::new();
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    events = self.viewer.ui(ui, &mut self.core, fullscreen);
                });
                for e in events {
                    match e {
                        ViewerEvent::Command(c) => self.run(c),
                        ViewerEvent::Close => self.run(Command::CloseViewer),
                        ViewerEvent::FileChanged(p) => {
                            self.core.loader.invalidate(&p);
                            self.core.thumbs.forget(&p);
                        }
                        ViewerEvent::IndexChanged(_) => {}
                    }
                }
            }
            Mode::Slideshow => {
                let mut keep = true;
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    if let Some(s) = &mut self.slideshow {
                        keep = s.ui(ui, &mut self.core);
                    }
                });
                if !keep {
                    let last = self.slideshow.as_ref().and_then(|s| s.list.get(s.index).cloned());
                    self.slideshow = None;
                    self.mode = if self.viewer.current().is_some() && self.viewer_from_browser { Mode::Viewer } else { Mode::Browser };
                    if let Some(l) = last {
                        if self.mode == Mode::Viewer {
                            if let Some(i) = self.viewer.list.iter().position(|p| p == &l) {
                                self.viewer.go_to(i, self.core.settings.fit_mode);
                            }
                        } else {
                            self.browser.set_focus_path(&l);
                        }
                    }
                    self.set_fullscreen(false);
                }
            }
            Mode::Compare => {
                let mut keep = true;
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    if let Some(c) = &mut self.compare {
                        keep = c.ui(ui, &mut self.core);
                    }
                });
                if !keep {
                    self.compare = None;
                    self.mode = Mode::Browser;
                }
            }
            Mode::Video => {
                let mut keep = true;
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    if let Some(v) = &mut self.video {
                        keep = v.ui(ui, &mut self.core);
                    }
                });
                if !keep {
                    self.video = None;
                    self.mode = Mode::Browser;
                    if self.fullscreen {
                        self.set_fullscreen(false);
                    }
                }
            }
        }

        self.dialogs(&ctx);
        let available = |c: Command| -> bool { !matches!(c, Command::Exit) || true };
        if let Some(c) = self.palette.show(&ctx, &p, &available) {
            self.run(c);
        }
        self.core.toasts.show(&ctx, &p);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist();
    }
}

impl App {
    fn dev_actions(&mut self, ctx: &egui::Context) {
        for a in self.dev.tick(ctx) {
            match a {
                crate::devhook::DevAction::Run(name) => {
                    let mut all = Command::palette();
                    all.extend([Command::CopyFiles, Command::PasteFiles]);
                    match all.into_iter().find(|c| format!("{c:?}") == name) {
                        Some(c) => self.run(c),
                        None => self.dev.write_state(&format!("unknown command {name}")),
                    }
                }
                crate::devhook::DevAction::Open(p) => {
                    if p.is_dir() {
                        self.navigate(p)
                    } else {
                        self.open_file(&p)
                    }
                }
                crate::devhook::DevAction::State => {
                    let cur = self.viewer.current().map(|p| p.display().to_string()).unwrap_or_default();
                    let st = match self.viewer.current().and_then(|p| self.core.loader.state(p)) {
                        Some(crate::loader::LoadState::Ready(l)) => format!("ready {}x{} {}ms", l.decoded.width(), l.decoded.height(), l.load_ms),
                        Some(crate::loader::LoadState::Failed(e)) => format!("failed {e}"),
                        Some(crate::loader::LoadState::Preview(_)) => "preview".into(),
                        Some(crate::loader::LoadState::Loading) => "loading".into(),
                        None => "none".into(),
                    };
                    let s = format!(
                        "mode={:?} folder={} items={} focus={:?} selected={} viewer={} [{}/{}] zoom={:.3} load={} dirty={}",
                        self.mode,
                        self.browser.folder.display(),
                        self.browser.items.len(),
                        self.browser.focus,
                        self.browser.selected.len(),
                        cur,
                        self.viewer.index + 1,
                        self.viewer.list.len(),
                        self.viewer.zoom,
                        st,
                        self.viewer.is_dirty()
                    );
                    self.dev.write_state(&s);
                }
            }
        }
    }

    fn persist(&mut self) {
        self.core.settings.window_maximized = self.core.ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        self.core.settings.save();
        self.core.tags.save_if_dirty();
    }
}

#[allow(dead_code)]
fn _unused(_: &catalog::Entry) {}
