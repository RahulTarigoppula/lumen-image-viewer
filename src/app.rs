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
use egui::{Key, RichText, Vec2};
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
    /// Rotate / flip non-JPEG files by decoding and re-encoding them in place.
    Reencode { files: Vec<PathBuf>, jpegs: Vec<PathBuf>, t: Transform },
}

/// An action that would discard unsaved edits; held while the user decides.
enum PendingAction {
    OpenFile(PathBuf),
    OpenViewerAt(usize),
    Exit,
    Transfer(Vec<PathBuf>, PathBuf, bool),
    Delete(Vec<PathBuf>),
}

/// One undoable file operation batch: each pair is (original path, current path).
struct FileUndo {
    label: String,
    moves: Vec<(PathBuf, PathBuf)>,
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
    metadata_dialog: Option<(crate::ui::metadata_dialog::MetadataDialog, Vec<PathBuf>)>,
    print_window: Option<crate::ui::tools::PrintWindow>,
    multipage: Option<crate::ui::tools::MultiPageWindow>,
    strip: Option<crate::ui::tools::StripWindow>,
    zip: Option<crate::ui::tools::ZipWindow>,
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
    /// Unsaved-edits guard: the action waiting for Save / Discard / Cancel.
    pending_guard: Option<PendingAction>,
    /// Set once the user agreed to quit with unsaved edits.
    allow_close: bool,
    /// The last click landed in a floating window (tool windows, settings).
    click_in_window: bool,
    /// A tool window has the user's attention: mode shortcuts are suspended.
    tool_window_active: bool,
    /// Undo stack for move / rename batches (Ctrl+Z in the browser).
    file_undo: Vec<FileUndo>,
    /// The running slideshow was started from the viewer (return there when it ends).
    slideshow_from_viewer: bool,
    first_frame_marked: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings, initial: Option<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        crate::devhook::mark("app_new");
        theme::install_fonts(&ctx);
        crate::devhook::mark("fonts");
        let dark = match settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => !matches!(ctx.system_theme(), Some(egui::Theme::Light)),
        };
        let pal = Palette::new(dark, settings.accent);
        theme::apply(&ctx, &pal, settings.ui_scale);
        egui_extras::install_image_loaders(&ctx);
        crate::devhook::mark("theme");

        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let mut loader = Loader::new(ctx.clone(), (cores / 4).clamp(2, 4));
        loader.budget_bytes = settings.cache_budget_mb.max(256) << 20;
        loader.opts.lock().color_manage = settings.color_manage;
        let thumbs = ThumbService::new(ctx.clone(), cores.saturating_sub(2).clamp(2, 12), crate::settings::cache_dir().join("thumbs"));
        thumbs.set_disk_cache(settings.thumb_disk_cache);
        let meta = MetaService::new(ctx.clone());
        crate::devhook::mark("services");

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
            metadata_dialog: None,
            print_window: None,
            multipage: None,
            strip: None,
            zip: None,
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
            pending_guard: None,
            allow_close: false,
            click_in_window: false,
            tool_window_active: false,
            file_undo: Vec::new(),
            slideshow_from_viewer: false,
            first_frame_marked: false,
        };
        crate::ui::keymap::install(&app.core.settings);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        for msg in crate::ui::batch::recover_pending_renames() {
            app.core.toasts.info(msg);
        }
        app.viewer.show_info = app.core.settings.show_info_overlay;
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
        self.load_folder(folder);
    }

    /// Show `folder` in the browser without touching the back / forward history.
    fn load_folder(&mut self, folder: PathBuf) {
        self.core.thumbs.cancel_all();
        self.core.settings.last_folder = Some(folder.clone());
        self.core.settings.push_recent(&folder);
        self.browser.load(folder, &self.core.ctx, self.core.settings.show_hidden_files);
        if self.mode == Mode::Viewer {
            self.mode = Mode::Browser;
        }
    }

    /// Open a single file: show its folder in the browser and the file in the viewer.
    /// Asks first when the viewer holds unsaved edits of another image.
    pub fn open_file(&mut self, path: &Path) {
        let is_video = crate::video::is_video_ext(&crate::formats::ext_of(path));
        if !is_video && self.viewer.is_dirty() {
            if self.viewer.current().map(|c| c == path).unwrap_or(false) {
                self.mode = Mode::Viewer;
            } else {
                self.pending_guard = Some(PendingAction::OpenFile(path.to_path_buf()));
            }
            return;
        }
        self.open_file_now(path);
    }

    fn open_file_now(&mut self, path: &Path) {
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
                if self.viewer.is_dirty() {
                    if self.viewer.current() == Some(&entry.path) {
                        self.mode = Mode::Viewer;
                    } else {
                        self.pending_guard = Some(PendingAction::OpenViewerAt(item));
                    }
                    return;
                }
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
        match self.mode {
            Mode::Viewer => self.targets(),
            _ => self.browser.media_targets(),
        }
    }

    pub fn run(&mut self, cmd: Command) {
        use Command::*;
        let fit = self.core.settings.fit_mode;
        let wrap = self.core.settings.loop_navigation;
        match cmd {
            OpenFile => {
                let exts: Vec<&str> = crate::formats::NATIVE_EXTS.iter().chain(crate::formats::WIC_EXTS.iter()).chain(crate::formats::DOCUMENT_EXTS.iter()).chain(crate::video::VIDEO_EXTS.iter()).copied().collect();
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
            Exit => {
                if self.viewer.is_dirty() {
                    self.pending_guard = Some(PendingAction::Exit);
                } else {
                    self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
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
                    self.load_folder(p);
                }
            }
            GoForward => {
                if let Some(p) = self.browser.forward.pop() {
                    self.browser.back.push(self.browser.folder.clone());
                    self.load_folder(p);
                }
            }
            SelectAll => {
                self.browser.selected = self.browser.items.iter().map(|&e| &self.browser.entries[e]).filter(|e| !e.is_folder()).map(|e| e.path.clone()).collect();
            }
            SelectNone => self.browser.selected.clear(),
            InvertSelection => {
                let all: Vec<PathBuf> = self.browser.items.iter().map(|&e| &self.browser.entries[e]).filter(|e| !e.is_folder()).map(|e| e.path.clone()).collect();
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
                if self.dirty_current_in(&files) {
                    self.pending_guard = Some(PendingAction::Delete(files));
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
                        Ok(()) => self.core.toasts.info(format!("{} file(s) copied - paste in Explorer or another folder", files.len())),
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
                self.slideshow_from_viewer = self.mode == Mode::Viewer;
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
                    self.core.toasts.info("Select 2-4 images (or 2 videos) to compare");
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
                if self.browser.flagged_only || self.core.settings.sort_key == crate::settings::SortKey::Flagged {
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
                if self.core.settings.sort_key == crate::settings::SortKey::ColorLabel {
                    self.browser.mark_resort();
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
            ToggleLockZoom => {
                let on = self.viewer.toggle_lock_zoom();
                self.core.toasts.info(if on { "Zoom ratio locked" } else { "Zoom ratio unlocked" });
            }
            FitWidth => self.viewer.reset_view(FitMode::FitWidth),
            FitHeight => self.viewer.reset_view(FitMode::FitHeight),
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
            Undo if self.mode == Mode::Browser => self.undo_file_op(),
            Undo | Redo if self.viewer.has_tool() => {}
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
                if self.mode == Mode::Viewer {
                    let Some(img) = self.viewer.current_image(&self.core) else {
                        self.core.toasts.info("Image is still loading");
                        return;
                    };
                    let title = self.viewer.current().map(|p| crate::ui::viewer::file_name(p)).unwrap_or_else(|| "Lumen".into());
                    self.print_window = Some(crate::ui::tools::PrintWindow::with_image(img, title, &self.core));
                } else {
                    let files: Vec<PathBuf> = self.media_targets().into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
                    if files.is_empty() {
                        self.core.toasts.info("Select images to print");
                        return;
                    }
                    self.print_window = Some(crate::ui::tools::PrintWindow::new(files, &self.core));
                }
            }
            EditComment | RemoveMetadata | ChangeDates | ExportXmp => {
                use crate::ui::metadata_dialog::{MetadataDialog, Tab};
                let files = self.media_targets();
                if files.is_empty() {
                    self.core.toasts.info("Select files first");
                    return;
                }
                let tab = match cmd {
                    EditComment => Tab::Comment,
                    RemoveMetadata => Tab::Strip,
                    ChangeDates => Tab::Dates,
                    _ => Tab::Sidecar,
                };
                self.metadata_dialog = Some((MetadataDialog::new(files.clone(), &self.core).with_tab(tab), files));
            }
            CreateMultiPage | ImageStrip | CreateZip => {
                let mut files = self.media_targets();
                if files.len() <= 1 && self.mode == Mode::Browser {
                    files = self.browser.media_paths();
                }
                match cmd {
                    CreateMultiPage => self.multipage = Some(crate::ui::tools::MultiPageWindow::new(files, &self.core)),
                    ImageStrip => self.strip = Some(crate::ui::tools::StripWindow::new(files, &self.core)),
                    _ => self.zip = Some(crate::ui::tools::ZipWindow::new(files, &self.core)),
                }
            }
            ToggleInfo => self.viewer.show_info = !self.viewer.show_info,
            ToggleHistogram => self.viewer.show_histogram = !self.viewer.show_histogram,
            ToggleMagnifier => self.viewer.magnifier_locked = !self.viewer.magnifier_locked,
            ToggleFilmstrip => self.viewer.show_filmstrip = !self.viewer.show_filmstrip,
            PlayPause => self.viewer.toggle_animation(),
            NextFrame => self.viewer.step_frame(&self.core, 1),
            PrevFrame => self.viewer.step_frame(&self.core, -1),
            OpenWithEditor | OpenWithEditorN(_) => {
                let n = if let OpenWithEditorN(n) = cmd { n.max(1) as usize } else { 1 };
                if let Some(p) = self.targets().first() {
                    match self.core.settings.external_editors.get(n - 1) {
                        Some(ed) => {
                            if let Err(e) = fileops::open_with(&ed.exe, p) {
                                self.core.toasts.error(format!("Could not start {}: {e}", ed.name));
                            }
                        }
                        None if n == 1 => fileops::open_with_dialog(p),
                        None => self.core.toasts.info(format!("No external editor {n} - add one in Settings > External editors")),
                    }
                }
            }
            OpenGpsMap => {
                let Some(p) = self.media_targets().first().cloned() else { return };
                match crate::meta::read_exif(&p).and_then(|e| e.gps) {
                    Some((lat, lon)) => {
                        let url = format!("https://www.openstreetmap.org/?mlat={lat:.6}&mlon={lon:.6}#map=16/{lat:.6}/{lon:.6}");
                        if let Err(e) = std::process::Command::new("explorer.exe").arg(&url).spawn() {
                            self.core.toasts.error(format!("Could not open the map: {e}"));
                        }
                    }
                    None => self.core.toasts.info("This image has no GPS location"),
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
            self.core.toasts.info("Image is still loading...");
            return false;
        }
        true
    }

    fn transform(&mut self, t: Transform) {
        let fit = self.core.settings.fit_mode;
        if self.mode == Mode::Browser {
            // JPEGs are always rotated losslessly; anything else must be re-encoded,
            // which the user confirms first.
            let targets: Vec<PathBuf> = self.media_targets().into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
            let (jpegs, others): (Vec<PathBuf>, Vec<PathBuf>) = targets.into_iter().partition(|p| crate::formats::is_jpeg_ext(&crate::formats::ext_of(p)));
            if others.is_empty() {
                self.lossless_transform(&jpegs, t);
            } else {
                self.confirm = Some(Confirm::Reencode { files: others, jpegs, t });
            }
            return;
        }
        // Viewer, unedited JPEG: rewrite the EXIF orientation only.
        if self.core.settings.lossless_jpeg_rotate && !self.viewer.is_dirty() {
            if let Some(p) = self.viewer.current().cloned().filter(|p| crate::formats::is_jpeg_ext(&crate::formats::ext_of(p))) {
                if self.lossless_transform(&[p], t) > 0 {
                    self.viewer.edit = None;
                    self.viewer.reset_view(fit);
                }
                return;
            }
        }
        if !self.viewer.apply_simple(&self.core, t.label(), |img| t.apply(img)) {
            self.core.toasts.info("Image is still loading");
        }
        self.viewer.reset_view(fit);
    }

    fn forget_file(&mut self, p: &Path) {
        self.core.loader.invalidate(p);
        self.core.thumbs.forget(p);
        self.viewer.forget_stat(p);
        self.core.meta.forget(p);
    }

    /// Lossless JPEG rotate / flip; returns how many files changed.
    fn lossless_transform(&mut self, jpegs: &[PathBuf], t: Transform) -> usize {
        let mut ok = 0;
        let mut errs = Vec::new();
        for p in jpegs {
            match crate::encode::lossless_jpeg_transform(p, t) {
                Ok(_) => {
                    ok += 1;
                    self.forget_file(p);
                }
                Err(e) => errs.push(format!("{}: {e}", crate::ui::viewer::file_name(p))),
            }
        }
        if !errs.is_empty() {
            self.core.toasts.error(format!("{} file(s) not rotated - {}", errs.len(), errs[0]));
        }
        if ok > 0 {
            self.core.toasts.info(format!("{} - lossless{}", t.label(), if ok > 1 { format!(" ({ok} files)") } else { String::new() }));
        }
        ok
    }

    /// Decode, transform and re-save non-JPEG files in place (after confirmation).
    /// Animations, multi-page files, high bit depth images and icons are refused,
    /// since re-encoding would lose frames or precision.
    fn reencode_transform(&mut self, files: &[PathBuf], t: Transform) {
        let mut ok = 0;
        let mut skipped: Vec<String> = Vec::new();
        let mut errs: Vec<String> = Vec::new();
        for p in files {
            let name = crate::ui::viewer::file_name(p);
            let ext = crate::formats::ext_of(p);
            if ext == "ico" || ext == "cur" {
                skipped.push(format!("{name} (icon)"));
                continue;
            }
            let Some(fmt) = crate::formats::SaveFormat::from_ext(&ext) else {
                skipped.push(format!("{name} (format cannot be saved)"));
                continue;
            };
            let d = match crate::decode::decode_file(p, Default::default()) {
                Ok(d) => d,
                Err(e) => {
                    errs.push(format!("{name}: {e}"));
                    continue;
                }
            };
            if d.frame_count > 1 || d.frames.is_some() {
                skipped.push(format!("{name} (animated or multi-page)"));
                continue;
            }
            let ci = d.color_info.to_ascii_lowercase();
            if ci.contains("16-bit") || ci.contains("32-bit") || ci.contains("float") {
                skipped.push(format!("{name} (more than 8 bits per channel)"));
                continue;
            }
            let out = t.apply(&d.image);
            let opts = SaveOptions { format: fmt, jpeg_quality: self.core.settings.jpeg_quality, exif: d.exif.clone(), ..Default::default() };
            match crate::encode::save(&out, p, &opts) {
                Ok(()) => {
                    ok += 1;
                    self.forget_file(p);
                }
                Err(e) => errs.push(format!("{name}: {e}")),
            }
        }
        if !skipped.is_empty() {
            self.core.toasts.info(format!("Skipped {} file(s): {}", skipped.len(), skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        }
        if !errs.is_empty() {
            self.core.toasts.error(format!("{} file(s) failed: {}", errs.len(), errs[0]));
        }
        if ok > 0 {
            self.core.toasts.info(format!("{} applied to {ok} file(s)", t.label()));
        }
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
                self.viewer.forget_stat(path);
                self.core.meta.forget(path);
                // Always drop the cached pre-edit decode: otherwise returning to this
                // file later shows (and re-edits) the old pixels. The edit session keeps
                // showing the saved image meanwhile.
                self.core.loader.invalidate(path);
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
                    self.viewer.forget_stat(f);
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

    /// True when the viewer has unsaved edits of one of `files`.
    fn dirty_current_in(&self, files: &[PathBuf]) -> bool {
        self.viewer.is_dirty() && self.viewer.current().map(|c| files.iter().any(|f| c == f || c.starts_with(f))).unwrap_or(false)
    }

    fn transfer_files(&mut self, files: Vec<PathBuf>, dest: PathBuf, do_move: bool) {
        if do_move && self.dirty_current_in(&files) {
            self.pending_guard = Some(PendingAction::Transfer(files, dest, do_move));
            return;
        }
        self.transfer_files_now(files, dest, do_move);
    }

    fn transfer_files_now(&mut self, files: Vec<PathBuf>, dest: PathBuf, do_move: bool) {
        let (done, errs) = fileops::transfer(&files, &dest, do_move, fileops::Conflict::Rename);
        if do_move && !done.is_empty() {
            let label = format!("Move {} item(s) to {}", done.len(), dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dest.display().to_string()));
            self.push_file_undo(label, done.clone());
        }
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

    fn push_file_undo(&mut self, label: String, moves: Vec<(PathBuf, PathBuf)>) {
        self.file_undo.push(FileUndo { label, moves });
        if self.file_undo.len() > 20 {
            self.file_undo.remove(0);
        }
    }

    /// Ctrl+Z in the browser: put the last moved / renamed files back.
    fn undo_file_op(&mut self) {
        let Some(op) = self.file_undo.pop() else {
            self.core.toasts.info("Nothing to undo. Deleted files can be restored from the Recycle Bin.");
            return;
        };
        let mut ok = 0;
        let mut errs = Vec::new();
        let mut folders: Vec<PathBuf> = Vec::new();
        for (orig, cur) in op.moves.iter().rev() {
            match fileops::move_path(cur, orig) {
                Ok(()) => {
                    ok += 1;
                    self.core.tags.moved(cur, orig);
                    self.core.loader.invalidate(cur);
                    self.core.thumbs.forget(cur);
                    self.viewer.replace_path(cur, orig);
                    for d in [orig.parent(), cur.parent()].into_iter().flatten() {
                        if !folders.iter().any(|f| f == d) {
                            folders.push(d.to_path_buf());
                        }
                    }
                }
                Err(e) => errs.push(e.to_string()),
            }
        }
        for f in &folders {
            self.browser.tree.invalidate(f);
        }
        if let Some((orig, _)) = op.moves.first() {
            if orig.parent() == Some(self.browser.folder.as_path()) {
                self.browser.pending_select = Some(orig.clone());
            }
        }
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if !errs.is_empty() {
            self.core.toasts.error(format!("Undo: {} item(s) could not be restored - {}", errs.len(), errs[0]));
        }
        if ok > 0 {
            self.core.toasts.success(format!("Undone: {}", op.label));
        }
    }

    /// Carry out an action that was waiting on the unsaved-edits question.
    fn perform_pending(&mut self, a: PendingAction) {
        match a {
            PendingAction::OpenFile(p) => self.open_file_now(&p),
            PendingAction::OpenViewerAt(i) => self.open_viewer_at(i),
            PendingAction::Exit => {
                self.allow_close = true;
                self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PendingAction::Transfer(files, dest, do_move) => self.transfer_files_now(files, dest, do_move),
            PendingAction::Delete(files) => {
                if self.core.settings.confirm_delete {
                    self.confirm = Some(Confirm::Delete(files));
                } else {
                    self.delete_files(files);
                }
            }
        }
    }

    /// Save / Discard / Cancel for unsaved edits (window close, opening another image, moving the file).
    fn guard_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_guard.is_none() {
            return;
        }
        let p = self.core.pal;
        let name = self.viewer.current().map(|c| crate::ui::viewer::file_name(c)).unwrap_or_default();
        let can_save = self.viewer.current().map(|c| crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(c)).is_some()).unwrap_or(false);
        let mut choice: Option<u8> = None; // 0 cancel, 1 save, 2 discard
        egui::Modal::new(egui::Id::unique("unsaved-guard")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Unsaved changes");
            ui.add_space(4.0);
            ui.label(format!("\"{name}\" has edits that are not saved."));
            if !can_save {
                ui.label(RichText::new("This format cannot be saved in place - use Save as from the viewer.").color(p.muted).size(12.0));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can_save, egui::Button::new(format!("{}  Save", I::FLOPPY_DISK))).clicked() {
                    choice = Some(1);
                }
                if ui.button("Discard changes").clicked() {
                    choice = Some(2);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    choice = Some(0);
                }
            });
        });
        match choice {
            Some(0) => self.pending_guard = None,
            Some(1) => {
                let saved = match (self.viewer.current().cloned(), self.viewer.current_image(&self.core)) {
                    (Some(path), Some(img)) => {
                        let fmt = crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(&path));
                        let loaded = self.viewer.loaded(&self.core);
                        let mut opts = SaveOptions { jpeg_quality: self.core.settings.jpeg_quality, keep_metadata: self.core.settings.keep_exif_on_save, ..Default::default() };
                        if let Some(l) = &loaded {
                            opts.exif = l.decoded.exif.clone();
                            if !self.core.settings.color_manage {
                                opts.icc = l.decoded.icc_profile.clone();
                            }
                        }
                        match fmt {
                            Some(f) => {
                                opts.format = f;
                                self.do_save(&path, &img, &opts)
                            }
                            None => false,
                        }
                    }
                    _ => false,
                };
                if saved {
                    if let Some(a) = self.pending_guard.take() {
                        self.perform_pending(a);
                    }
                } else {
                    self.pending_guard = None;
                }
            }
            Some(_) => {
                self.viewer.edit = None;
                if let Some(a) = self.pending_guard.take() {
                    self.perform_pending(a);
                }
            }
            None => {}
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

    /// eframe's clipboard shortcuts are off (so Ctrl+C / V / X reach the keymap);
    /// give text boxes their copy / cut / paste events back.
    fn clipboard_keys(&mut self, ctx: &egui::Context) {
        if !ctx.egui_wants_keyboard_input() {
            return;
        }
        ctx.input_mut(|i| {
            let mut extra = Vec::new();
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } if modifiers.command && !modifiers.alt && !modifiers.shift => match key {
                    Key::C => {
                        extra.push(egui::Event::Copy);
                        false
                    }
                    Key::X => {
                        extra.push(egui::Event::Cut);
                        false
                    }
                    Key::V => {
                        if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                            extra.push(egui::Event::Paste(text));
                        }
                        false
                    }
                    _ => true,
                },
                _ => true,
            });
            i.events.extend(extra);
        });
    }

    /// True while a modal dialog, prompt or confirmation is showing: no shortcuts,
    /// no browser keys.
    fn modal_open(&self) -> bool {
        self.palette.open
            || self.prompt.is_some()
            || self.confirm.is_some()
            || self.transfer.is_some()
            || self.save_dialog.is_some()
            || self.viewer.confirm_discard
            || self.pending_guard.is_some()
    }

    fn tool_window_open(&self) -> bool {
        self.batch_convert.is_some()
            || self.batch_rename.is_some()
            || self.contact_sheet.is_some()
            || self.duplicates.is_some()
            || self.metadata_dialog.is_some()
            || self.print_window.is_some()
            || self.multipage.is_some()
            || self.strip.is_some()
            || self.zip.is_some()
            || self.show_settings
            || self.show_help
            || self.show_about
    }

    /// Remember whether the last click landed in a floating window, so keys typed
    /// in a tool window (Duplicates, Batch rename, ...) do not drive the browser.
    fn track_window_focus(&mut self, ctx: &egui::Context) {
        let over_window = |pos: Option<egui::Pos2>| {
            pos.and_then(|p| ctx.layer_id_at(p)).map(|l| l.order != egui::Order::Background).unwrap_or(false)
        };
        let (pressed, press_pos, hover_pos) = ctx.input(|i| (i.pointer.any_pressed(), i.pointer.press_origin(), i.pointer.hover_pos()));
        if pressed {
            self.click_in_window = over_window(press_pos.or(hover_pos));
        }
        self.tool_window_active = self.tool_window_open() && (self.click_in_window || over_window(hover_pos));
        if !self.tool_window_open() {
            self.click_in_window = false;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use crate::ui::keymap::{self, KeyMode};
        keymap::set_context(if self.mode == Mode::Viewer { KeyMode::Viewer } else { KeyMode::Browser });
        self.track_window_focus(ctx);
        if self.modal_open() || keymap::capturing() {
            return;
        }
        let km = keymap::active();
        let typing = ctx.egui_wants_keyboard_input();
        let mode_map = match self.mode {
            Mode::Browser => Some(KeyMode::Browser),
            // Edit tools own Enter / Escape and their preview base would go stale on undo.
            Mode::Viewer if !self.viewer.has_tool() => Some(KeyMode::Viewer),
            _ => None,
        };
        // Mode shortcuts are off while typing or while a tool window has the user's attention.
        let mode_map = mode_map.filter(|_| !typing && !self.tool_window_active);
        let typeahead = mode_map == Some(KeyMode::Browser) && self.browser.typeahead_active();
        let mut fire = ctx.input_mut(|i| {
            keymap::consume(i, |key, mods| {
                // Mid-word letters belong to the type-ahead search, not to single-letter commands.
                let plain = !mods.ctrl && !mods.command && !mods.alt;
                if typeahead && plain && key.name().len() == 1 {
                    return None;
                }
                if let Some(m) = mode_map {
                    if let Some(c) = km.lookup(m, key, mods) {
                        return Some(c);
                    }
                }
                km.bindings(KeyMode::Global)
                    .iter()
                    .find(|(s, _)| keymap::matches(s, key, mods) && (!typing || keymap::works_while_typing(s)))
                    .map(|(_, c)| *c)
            })
        });
        if self.duplicates.is_some() {
            // Never trash the browser selection while the duplicate finder is open.
            fire.retain(|c| *c != Command::Delete);
        }
        match mode_map {
            Some(KeyMode::Viewer) => {
                let star = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "*")));
                if star {
                    fire.push(Command::CycleFit);
                }
            }
            Some(KeyMode::Browser) => {
                // Letters not bound to a command start (or continue) a type-ahead search.
                let texts: Vec<String> = ctx.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.clone()) } else { None }).collect());
                let mods = ctx.input(|i| i.modifiers);
                if fire.is_empty() && !mods.ctrl && !mods.alt {
                    let bound = km.bound_letters(KeyMode::Browser);
                    for t in texts {
                        let Some(c) = t.chars().next() else { continue };
                        if self.browser.typeahead_active() && c.is_alphanumeric() {
                            self.browser.typeahead_push(&t);
                        } else if c.is_alphabetic() && !bound.contains(&c.to_ascii_lowercase()) {
                            self.browser.typeahead_start(&t);
                        }
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
                                    self.push_file_undo(format!("Rename to {}", crate::ui::viewer::file_name(&new)), vec![(path.clone(), new.clone())]);
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
        if let Some(Confirm::Reencode { files, jpegs, t }) = &self.confirm {
            let (files, jpegs, t) = (files.clone(), jpegs.clone(), *t);
            let mut body = format!(
                "{} will decode and re-encode {} file(s) in place. Some quality or metadata may be lost. Animated, multi-page and 16-bit images and icons are skipped.",
                t.label(),
                files.len()
            );
            if !jpegs.is_empty() {
                body += &format!("\n\n{} JPEG file(s) in the selection are rotated losslessly.", jpegs.len());
            }
            match widgets::show_confirm(ctx, "Re-encode files?", &body, "Re-encode", false, &p) {
                Some(true) => {
                    self.confirm = None;
                    self.lossless_transform(&jpegs, t);
                    self.reencode_transform(&files, t);
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
                if ui.add(egui::Button::new(format!("{}  Browse for a folder...", I::FOLDER_OPEN)).min_size(Vec2::new(ui.available_width(), 30.0))).clicked() {
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
                            d.error = Some("Could not save - see the message below.".into());
                        }
                    }
                }
            }
        }
        if !self.show_settings {
            crate::ui::keymap::set_capturing(false);
        }
        if self.show_settings {
            let mut open = true;
            let color_before = self.core.settings.color_manage;
            if crate::ui::settings_window::show(ctx, &mut self.core, &mut open) {
                self.apply_theme();
                crate::ui::keymap::install(&self.core.settings);
                self.core.loader.budget_bytes = self.core.settings.cache_budget_mb.max(256) << 20;
                self.core.loader.opts.lock().color_manage = self.core.settings.color_manage;
                if color_before != self.core.settings.color_manage {
                    // Re-decode everything so the change is visible (keep an image being edited).
                    let keep: Vec<PathBuf> = if self.viewer.is_dirty() { self.viewer.current().cloned().into_iter().collect() } else { Vec::new() };
                    self.core.loader.clear_except(&keep);
                    self.core.thumbs.clear_textures();
                    self.viewer.reset_view(self.core.settings.fit_mode);
                }
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
                self.core.loader.invalidate(from);
                self.core.thumbs.forget(from);
                self.core.meta.forget(from);
            }
            if !renamed.is_empty() {
                self.push_file_undo(format!("Batch rename of {} file(s)", renamed.len()), renamed.clone());
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
        if let Some((w, files)) = &mut self.metadata_dialog {
            if !w.show(ctx, &mut self.core) {
                let files = std::mem::take(files);
                self.metadata_dialog = None;
                for f in &files {
                    self.forget_file(f);
                }
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
        }
        if let Some(w) = &mut self.print_window {
            if !w.show(ctx, &mut self.core) {
                self.print_window = None;
            }
        }
        let mut reload = false;
        if let Some(w) = &mut self.multipage {
            if !w.show(ctx, &mut self.core) {
                self.multipage = None;
                reload = true;
            }
        }
        if let Some(w) = &mut self.strip {
            if !w.show(ctx, &mut self.core) {
                self.strip = None;
                reload = true;
            }
        }
        if let Some(w) = &mut self.zip {
            if !w.show(ctx, &mut self.core) {
                self.zip = None;
                reload = true;
            }
        }
        if reload {
            self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        }
        if let Some(w) = &mut self.duplicates {
            let (open, events) = w.show(ctx, &mut self.core);
            for e in events {
                match e {
                    crate::ui::tools::DupEvent::Deleted(paths) => {
                        for f in &paths {
                            self.core.tags.removed(f);
                            self.core.thumbs.forget(f);
                            self.viewer.forget_stat(f);
                            self.core.loader.invalidate(f);
                            self.viewer.remove_path(f);
                            self.browser.selected.remove(f);
                        }
                        if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
                            self.mode = Mode::Browser;
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
        use crate::ui::keymap::{self, KeyMode};
        let p = self.core.pal;
        let mut open = true;
        let km = keymap::active();
        egui::Window::new("Keyboard shortcuts").open(&mut open).default_size([640.0, 560.0]).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Preset: {}", keymap::Preset::from_id(&self.core.settings.keymap_preset).label())).color(p.muted));
                if ui.link("Change shortcuts...").clicked() {
                    self.show_settings = true;
                    crate::ui::settings_window::open_page(ctx, "Keyboard");
                }
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Generated from the active keymap, so this list is always correct.
                for mode in KeyMode::ALL {
                    widgets::section_title(ui, &p, mode.label());
                    egui::Grid::new(("help", mode.id())).num_columns(2).striped(true).min_col_width(240.0).show(ui, |ui| {
                        for c in Command::all() {
                            let keys = keymap::mode_text(&km, mode, c);
                            if keys.is_empty() {
                                continue;
                            }
                            ui.label(format!("{}  {}", c.icon(), c.label()));
                            ui.label(RichText::new(keys).monospace().color(p.accent));
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                }
                widgets::section_title(ui, &p, "Mouse");
                for (k, v) in [
                    ("Wheel", "Next / previous image (or zoom - configurable in Settings)"),
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
                ui.separator();
                ui.menu_button(format!("{}   Thumbnail captions", I::TEXTBOX), |ui| {
                    let s = &mut self.core.settings;
                    let cur = if s.show_thumb_captions { s.thumb_caption } else { crate::settings::ThumbCaption::None };
                    for c in crate::settings::ThumbCaption::ALL {
                        if ui.radio(cur == c, c.label()).clicked() {
                            s.set_thumb_caption(c);
                            ui.close();
                        }
                    }
                });
            });
            ui.menu_button(format!("{}   Image", I::IMAGE), |ui| {
                for c in [Command::RotateLeft, Command::RotateRight, Command::FlipHorizontal, Command::FlipVertical, Command::Crop, Command::Resize, Command::CanvasSize, Command::AutoEnhance, Command::AdjustColors, Command::Levels, Command::Curves, Command::Sharpen, Command::Blur, Command::Effects, Command::FreeRotate, Command::AddText, Command::AddBorder, Command::Watermark, Command::SetWallpaper, Command::Print] {
                    item(ui, c, &mut fire);
                }
            });
            ui.menu_button(format!("{}   Tools", I::WRENCH), |ui| {
                for c in [Command::BatchConvert, Command::BatchRename, Command::ContactSheet, Command::FindDuplicates, Command::CreateMultiPage, Command::ImageStrip, Command::CreateZip, Command::Slideshow, Command::Compare, Command::OpenVideoMode] {
                    item(ui, c, &mut fire);
                }
                ui.separator();
                for c in [Command::EditComment, Command::RemoveMetadata, Command::ChangeDates, Command::ExportXmp] {
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
        if self.mode == Mode::Video {
            return; // video mode handles its own drops
        }
        self.open_paths(dropped);
    }

    /// Open paths handed over by Explorer (a second launch) or dropped on the window:
    /// a folder is browsed, several videos open in video mode, several images open
    /// in the viewer as one list, a single file opens normally.
    fn open_paths(&mut self, paths: Vec<PathBuf>) {
        let Some(first) = paths.first().cloned() else { return };
        if paths.len() == 1 || first.is_dir() {
            if first.is_dir() {
                self.navigate(first);
            } else {
                self.open_file(&first);
            }
            return;
        }
        let vids: Vec<PathBuf> = paths.iter().filter(|p| crate::video::is_video_ext(&crate::formats::ext_of(p))).cloned().collect();
        if vids.len() == paths.len() {
            self.video = Some(crate::video::VideoMode::new(vids, &self.core));
            self.mode = Mode::Video;
            return;
        }
        let images: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file() && crate::formats::is_image_path(p)).collect();
        if images.len() < 2 {
            if let Some(p) = images.first() {
                self.open_file(p);
            }
            return;
        }
        if self.viewer.is_dirty() {
            self.pending_guard = Some(PendingAction::OpenFile(images[0].clone()));
            return;
        }
        self.open_file_now(&images[0]);
        // Show exactly the handed-over files, in the given order.
        self.viewer.open(images, 0, self.core.settings.fit_mode);
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.mode {
            Mode::Viewer => match self.viewer.current() {
                Some(p) => format!("{}{} - Lumen", crate::ui::viewer::file_name(p), if self.viewer.is_dirty() { " •" } else { "" }),
                None => "Lumen".into(),
            },
            Mode::Video => "Video - Lumen".into(),
            _ => format!("{} - Lumen", self.browser.folder.display()),
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
        // Single instance: open a file/folder another `lumen.exe` launch forwarded
        // to us (see shell.rs), then bring this window to the front.
        let forwarded = crate::shell::poll_forwarded(ctx);
        if !forwarded.is_empty() {
            self.open_paths(forwarded);
            if ctx.input(|i| i.viewport().minimized == Some(true)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.last_save.elapsed().as_secs() > 5 {
            self.core.tags.save_if_dirty();
            self.last_save = Instant::now();
        }
        let _ = ctx;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let frame_start = std::time::Instant::now();
        if !self.first_frame_marked {
            self.first_frame_marked = true;
            crate::devhook::mark("first_frame");
        }
        // Window close (X, Alt+F4, taskbar) with unsaved edits: ask first.
        if ctx.input(|i| i.viewport().close_requested()) && self.viewer.is_dirty() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.pending_guard.is_none() {
                self.pending_guard = Some(PendingAction::Exit);
            }
        }
        self.dev_actions(&ctx);
        self.handle_dropped(&ctx);
        self.clipboard_keys(&ctx);
        self.shortcuts(&ctx);
        self.browser.suppress_keys = self.modal_open() || self.tool_window_active;
        self.update_title(&ctx);
        self.viewer.edge_panels = self.core.settings.edge_panels;
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
                        let t = std::time::Instant::now();
                        self.core.settings.tree_width = ui.available_width() + 2.0;
                        self.browser.sidebar(ui, &mut self.core, &mut ev);
                        crate::devhook::prof("sidebar", t);
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 6, bottom: 0 }))
                    .show(ui, |ui| {
                        let t = std::time::Instant::now();
                        self.browser.central(ui, &mut self.core, &mut ev);
                        crate::devhook::prof("central", t);
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
                        BrowserEvent::DragOut(files) => match fileops::drag_out(&files) {
                            Ok(fileops::DragOutcome::Moved) => {
                                // Moved by the drop target: forget the old paths.
                                for f in &files {
                                    if !f.exists() {
                                        self.core.loader.invalidate(f);
                                        self.core.thumbs.forget(f);
                                        self.viewer.remove_path(f);
                                        self.browser.selected.remove(f);
                                    }
                                }
                                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                            }
                            Ok(_) => {}
                            Err(e) => self.core.toasts.error(e.to_string()),
                        },
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
                            self.viewer.forget_stat(&p);
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
                    self.mode = if self.viewer.current().is_some() && self.slideshow_from_viewer { Mode::Viewer } else { Mode::Browser };
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
                let deleted = self.compare.as_mut().map(|c| c.take_deleted()).unwrap_or_default();
                if !deleted.is_empty() {
                    for p in &deleted {
                        self.viewer.remove_path(p);
                        self.core.thumbs.forget(p);
                        self.core.loader.invalidate(p);
                        self.core.tags.removed(p);
                        self.browser.selected.remove(p);
                    }
                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                }
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
        self.guard_dialog(&ctx);
        // Leaving video mode by any route stops playback.
        if self.mode != Mode::Video && self.video.is_some() {
            self.video = None;
        }
        let available = |c: Command| -> bool { !matches!(c, Command::Exit) || true };
        if let Some(c) = self.palette.show(&ctx, &p, &available) {
            self.run(c);
        }
        self.core.toasts.show(&ctx, &p);
        crate::devhook::prof("ui_total", frame_start);
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
                    match Command::from_id(&name) {
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
                    self.dev.write_state(&format!("startup: {}", crate::devhook::DevHook::startup_report()));
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
