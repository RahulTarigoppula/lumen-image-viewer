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
use crossbeam_channel::{Receiver, Sender};
use egui::{Key, RichText, Vec2};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Remote AI analysis: settings, connection, runs and results.
    #[cfg(feature = "ai")]
    pub ai: crate::analysis_ui::ai::AiState,
    /// The Analysis panel's Metrics tab (video mode draws its graphs).
    pub metrics: crate::analysis_ui::metrics_view::MetricsTab,
    /// The Analysis panel is open (set by the app each frame, for the modes'
    /// toolbar buttons).
    pub analysis_open: bool,
    /// A mode's toolbar button or "Analyze..." menu item was used this frame.
    pub analysis_request: Option<crate::analysis_ui::PanelRequest>,
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

/// One entry of the file undo history (Ctrl+Z when no edit is open): moves,
/// renames, and rotations / flips written to files.
struct FileUndo {
    label: String,
    change: FileChange,
}

enum FileChange {
    /// Moved or renamed: each pair is (original path, current path).
    Moves(Vec<(PathBuf, PathBuf)>),
    /// `t` was applied to files on disk. Each file is restored from its
    /// previous bytes when they were kept; without them (only for JPEGs whose
    /// EXIF orientation alone changed) the inverse transform turns it back.
    Transform { t: Transform, files: Vec<(PathBuf, Option<Vec<u8>>)> },
}

impl FileUndo {
    /// Memory held by saved file contents.
    fn bytes(&self) -> usize {
        match &self.change {
            FileChange::Transform { files, .. } => files.iter().filter_map(|(_, b)| b.as_ref()).map(Vec::len).sum(),
            FileChange::Moves(_) => 0,
        }
    }
}

/// At most this much previous file content is kept for undoing rotations;
/// older history is dropped beyond it.
const UNDO_BYTES_BUDGET: usize = 512 << 20;

/// Decoded size up to which the last viewed image stays cached after the
/// viewer closes (a 24 MP photo with its mip levels).
const KEEP_AFTER_VIEWER: u64 = 128 << 20;

/// The transform that undoes `t`.
fn inverse(t: Transform) -> Transform {
    match t {
        Transform::Rotate90 => Transform::Rotate270,
        Transform::Rotate270 => Transform::Rotate90,
        other => other,
    }
}

/// "1 file", "3 files".
fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

struct TransferDialog {
    files: Vec<PathBuf>,
    do_move: bool,
    /// The destination field (filled with the most recent target when it opens).
    dest: Option<String>,
}

pub struct App {
    pub core: Core,
    pub mode: Mode,
    pub browser: Browser,
    pub viewer: Viewer,
    slideshow: Option<crate::ui::slideshow::Slideshow>,
    compare: Option<crate::ui::compare::Compare>,
    video: Option<crate::video::VideoMode>,
    /// The Analysis panel of the viewer, compare and video modes.
    analysis: crate::analysis_ui::AnalysisPanel,
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
    /// Debounced settings autosave: the JSON last written (or loaded), the
    /// JSON seen at the previous check, and when that check ran.
    settings_saved: String,
    settings_seen: String,
    settings_check: Instant,
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
    /// Frames started so far, up to 2 (startup milestones).
    startup_frames: u8,
    /// When the previous frame started (dev hook `bench`: frame intervals).
    last_frame_start: Option<Instant>,
    /// Dev hook pointer events for the next frame's raw input.
    dev_input: Vec<egui::Event>,
    /// A text field had keyboard focus at the end of the last frame.
    was_editing: bool,
    /// The window had the keyboard focus last frame (folders are re-listed
    /// when it comes back, since Explorer may have changed them).
    window_focused: bool,
    /// Copy / move / paste / rotate jobs running off the UI thread.
    file_jobs: FileJobs,
    /// Ctrl+Alt+M asked to open this map link: waiting for the privacy
    /// confirmation (url, "don't ask again" checkbox).
    gps_confirm: Option<(String, bool)>,
    /// Open floating windows, oldest first (Esc closes the last one).
    window_stack: Vec<Win>,
    /// Full screen was entered from the viewer: leave it when the viewer closes.
    fullscreen_from_viewer: bool,
    /// The window was maximized before going full screen.
    maximized_before_fullscreen: bool,
}

/// Floating windows that Esc can close, most recently opened first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Win {
    Help,
    About,
    Settings,
    BatchConvert,
    BatchRename,
    ContactSheet,
    Duplicates,
    Metadata,
    Print,
    MultiPage,
    Strip,
    Zip,
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
        let meta = MetaService::new({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        crate::devhook::mark("services");

        let start_folder = settings
            .last_folder
            .clone()
            .filter(|p| p.is_dir())
            .or_else(dirs::picture_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("C:\\"));

        let tags = TagStore::load();
        crate::devhook::mark("tags");
        let browser = Browser::new(start_folder.clone());
        crate::devhook::mark("browser");
        #[cfg(feature = "ai")]
        let ai = crate::analysis_ui::ai::AiState::new(&ctx, &settings);
        let mut app = App {
            core: Core {
                ctx: ctx.clone(),
                settings,
                pal,
                tags,
                loader,
                thumbs,
                meta,
                toasts: Toasts::default(),
                #[cfg(feature = "ai")]
                ai,
                metrics: Default::default(),
                analysis_open: false,
                analysis_request: None,
            },
            mode: Mode::Browser,
            browser,
            viewer: Viewer::new(),
            slideshow: None,
            compare: None,
            video: None,
            analysis: Default::default(),
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
            settings_saved: String::new(),
            settings_seen: String::new(),
            settings_check: Instant::now(),
            last_title: String::new(),
            effective_dark: dark,
            dev: crate::devhook::DevHook::new(),
            pending_guard: None,
            allow_close: false,
            click_in_window: false,
            tool_window_active: false,
            file_undo: Vec::new(),
            slideshow_from_viewer: false,
            startup_frames: 0,
            last_frame_start: None,
            dev_input: Vec::new(),
            was_editing: false,
            window_focused: true,
            file_jobs: FileJobs::new(ctx.clone()),
            gps_confirm: None,
            window_stack: Vec::new(),
            fullscreen_from_viewer: false,
            maximized_before_fullscreen: false,
        };
        crate::devhook::mark("app_struct");
        crate::ui::keymap::install(&app.core.settings);
        // Ctrl+Q is the Analysis panel, not egui's "quit" (its default quit
        // shortcut also matches Ctrl+Shift+Q, which is Run AI analysis).
        ctx.options_mut(|o| {
            o.zoom_with_keyboard = false;
            o.quit_shortcuts.clear();
        });
        crate::devhook::mark("keymap");
        for msg in crate::ui::batch::recover_pending_renames() {
            app.core.toasts.info(msg);
        }
        crate::devhook::mark("renames");
        app.viewer.show_info = app.core.settings.show_info_overlay;
        match initial {
            Some(p) if p.is_file() => app.open_file(&p),
            Some(p) if p.is_dir() => app.navigate(p),
            _ => app.navigate(start_folder),
        }
        crate::devhook::mark("navigate");
        app
    }

    // ------------------------------------------------------------ navigation

    pub fn navigate(&mut self, folder: PathBuf) {
        // A typed "c:\photos" must match the tree's "C:\Photos".
        let folder = fileops::true_case(&folder);
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
        if !is_video && self.viewer.has_unsaved_work() {
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
                if self.viewer.has_unsaved_work() {
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
        if self.viewer.has_unsaved_work() {
            self.viewer.confirm_close();
            return;
        }
        if let Some(p) = self.viewer.current().cloned() {
            self.browser.set_focus_path(&p);
        }
        self.viewer.edit = None;
        // Decoded images are only needed in the viewer (a 100 MP photo holds about
        // 500 MB): release them, keeping only a normal-sized last image so going
        // back to it is instant.
        let keep: Vec<PathBuf> = self.viewer.current().filter(|p| self.core.loader.bytes_of(p).is_some_and(|b| b <= KEEP_AFTER_VIEWER)).cloned().into_iter().collect();
        self.core.loader.clear_except(&keep);
        self.viewer.release_gpu_over(KEEP_AFTER_VIEWER);
        self.mode = Mode::Browser;
        if self.fullscreen && (self.core.settings.open_viewer_fullscreen || self.fullscreen_from_viewer) {
            self.set_fullscreen(false);
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        let was = self.fullscreen;
        if on && !was {
            self.maximized_before_fullscreen = self.core.ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            self.fullscreen_from_viewer = self.mode == Mode::Viewer;
        }
        self.fullscreen = on;
        self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        if !on && was {
            self.fullscreen_from_viewer = false;
            if self.maximized_before_fullscreen {
                self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            }
        }
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
                if self.viewer.has_unsaved_work() {
                    self.pending_guard = Some(PendingAction::Exit);
                } else {
                    self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Refresh => {
                for e in &self.browser.entries {
                    self.core.meta.forget(&e.path);
                }
                // F5 retries thumbnails that failed (a file still being written, or
                // one that was briefly missing during a move).
                self.core.thumbs.retry_failed_in(&self.browser.folder);
                self.browser.tree.refresh();
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
                    self.transfer = Some(TransferDialog { files, do_move: cmd == MoveTo, dest: None });
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
                    self.core.toasts.success(if t.len() == 1 { "Path copied".to_string() } else { format!("{} paths copied", t.len()) });
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
                        Ok(()) => self.core.toasts.success(format!("{} copied - paste in Explorer or another folder", count(files.len(), "file"))),
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
                        let dest = self.browser.folder.clone();
                        self.file_jobs.submit(format!("Pasting {}", count(files.len(), "item")), transfer_job(files, dest, false));
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
                let selected = if self.mode == Mode::Browser && self.browser.selected.is_empty() { Vec::new() } else { self.media_targets() };
                self.batch_convert = Some(crate::ui::batch::BatchConvert::new(selected, self.browser.image_paths(), self.browser.folder.clone(), &self.core));
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
                    self.image_unavailable();
                }
            }
            DrawBoard | CloneStamp | HealingBrush | RedEye | BlurRegion | Selection => {
                if !self.ensure_viewer() {
                    return;
                }
                let name = match cmd {
                    DrawBoard => "draw_board",
                    CloneStamp => "clone_stamp",
                    HealingBrush => "heal",
                    RedEye => "red_eye",
                    BlurRegion => "blur_region",
                    _ => "selection",
                };
                match edit_tools::make_tool_by_name(name) {
                    Some(tool) => self.viewer.open_tool(tool, &self.core),
                    None => self.core.toasts.info(format!("{} is not available", cmd.label())),
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
            // With an edit tool open, Ctrl+Z / Ctrl+Y step through the tool's own strokes.
            Undo | Redo if self.viewer.has_tool() => {
                let _ = if cmd == Undo { self.viewer.tool_undo() } else { self.viewer.tool_redo() };
            }
            // One history in the viewer: the image's edits first, then the file
            // changes (lossless rotate, rename, move) in the order they were made.
            Undo => {
                if let Some(w) = self.viewer.edit.as_mut().and_then(|e| e.undo()) {
                    self.core.toasts.success(format!("Undone: {w}"));
                    self.viewer.reset_view(fit);
                } else if self.mode == Mode::Viewer {
                    self.undo_file_op();
                }
            }
            Redo => {
                if let Some(w) = self.viewer.edit.as_mut().and_then(|e| e.redo()) {
                    self.core.toasts.success(format!("Redone: {w}"));
                    self.viewer.reset_view(fit);
                }
            }
            Save => self.save(false),
            SaveAs => self.save(true),
            CopyImage => {
                let img = if self.mode == Mode::Viewer {
                    let Some(img) = self.viewer.current_image(&self.core) else {
                        self.image_unavailable();
                        return;
                    };
                    img
                } else {
                    let Some(p) = self.targets().first().cloned() else { return };
                    let first_frame = crate::decode::DecodeOptions { animations: false, ..Default::default() };
                    match crate::decode::decode_file(&p, first_frame) {
                        Ok(d) => d.image,
                        Err(e) => {
                            self.core.toasts.error(format!("Could not open {}: {e}", crate::ui::viewer::file_name(&p)));
                            return;
                        }
                    }
                };
                match fileops::copy_image_to_clipboard(&img) {
                    Ok(()) => self.core.toasts.success("Image copied to clipboard"),
                    Err(e) => self.core.toasts.error(format!("Copy failed: {e}")),
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
                        self.image_unavailable();
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
                        if self.core.settings.gps_map_no_confirm {
                            self.open_map_url(&url);
                        } else {
                            // The coordinates leave this PC: ask first.
                            self.gps_confirm = Some((url, false));
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
            ToggleAnalysis | RunMetrics | RunAi => {
                if !matches!(self.mode, Mode::Viewer | Mode::Compare | Mode::Video) {
                    self.core.toasts.info("The analysis panel works on an open image, a comparison or a video");
                    return;
                }
                match cmd {
                    ToggleAnalysis => self.analysis.toggle(),
                    RunMetrics => self.analysis.request_metrics_run(),
                    _ => self.analysis.request_ai_run(),
                }
            }
        }
    }

    /// What the Analysis panel works on in the current mode.
    fn analysis_subject(&self) -> Option<crate::analysis_ui::Subject> {
        match self.mode {
            Mode::Viewer => Some(crate::analysis_ui::Subject::image(self.viewer.current().map(|p| p.as_path()), self.viewer.current_image(&self.core))),
            Mode::Compare => self.compare.as_ref().map(|c| c.analysis_subject()),
            Mode::Video => self.video.as_ref().map(|v| v.analysis_subject()),
            _ => None,
        }
    }

    /// Requests from the Analysis panel.
    fn analysis_actions(&mut self, ctx: &egui::Context, actions: Vec<crate::analysis_ui::PanelAction>) {
        use crate::analysis_ui::{canvas, PanelAction};
        for a in actions {
            match a {
                PanelAction::ZoomTo { slot, rect } => {
                    let ppp = ctx.pixels_per_point();
                    let pane = canvas::panes(ctx).into_iter().find(|p| p.slot == slot).map(|p| p.clip.size());
                    match self.mode {
                        Mode::Viewer => self.viewer.zoom_to_region(rect, ppp),
                        Mode::Compare => {
                            if let Some(c) = &mut self.compare {
                                c.zoom_to_region(if slot == canvas::Slot::B { 1 } else { 0 }, rect, ppp);
                            }
                        }
                        Mode::Video => {
                            if let (Some(v), Some(pane)) = (&mut self.video, pane) {
                                v.zoom_to_region(rect, pane, ppp);
                            }
                        }
                        _ => {}
                    }
                }
                PanelAction::OpenSettings(page) => {
                    crate::ui::settings_window::open_page(ctx, page);
                    self.show_settings = true;
                }
                PanelAction::Toast(t) => self.core.toasts.success(t),
                PanelAction::ExportJson(text) => {
                    if let Some(p) = rfd::FileDialog::new().add_filter("JSON", &["json"]).set_file_name("lumen_ai_result.json").save_file() {
                        match std::fs::write(&p, text) {
                            Ok(()) => self.core.toasts.success(format!("Exported {}", p.display())),
                            Err(e) => self.core.toasts.error(e.to_string()),
                        }
                    }
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
            self.image_unavailable();
            return false;
        }
        true
    }

    /// Tell the user why the viewer has no image to work on: it is still
    /// loading, or the file could not be opened (and why).
    fn image_unavailable(&mut self) {
        let failed = self.viewer.current().and_then(|p| match self.core.loader.state(p) {
            Some(crate::loader::LoadState::Failed(e)) => Some(format!("Could not open {}: {e}", crate::ui::viewer::file_name(p))),
            _ => None,
        });
        match failed {
            Some(msg) => self.core.toasts.error(msg),
            None => self.core.toasts.info("Image is still loading"),
        }
    }

    fn transform(&mut self, t: Transform) {
        let fit = self.core.settings.fit_mode;
        if self.mode == Mode::Browser {
            // JPEGs are always rotated losslessly; anything else must be re-encoded,
            // which the user confirms first.
            let targets: Vec<PathBuf> = self.media_targets().into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
            let (jpegs, others): (Vec<PathBuf>, Vec<PathBuf>) = targets.into_iter().partition(|p| crate::formats::is_jpeg_ext(&crate::formats::ext_of(p)));
            if others.is_empty() {
                if !jpegs.is_empty() {
                    self.file_jobs.submit(format!("{} ({} files)", t.label(), jpegs.len()), rotate_job(jpegs, Vec::new(), t, self.core.settings.jpeg_quality));
                }
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
            self.image_unavailable();
        }
        self.viewer.reset_view(fit);
    }

    /// Drop everything cached about the file at `p` (decode, thumbnail, failure
    /// marks, size / date, metadata). File operations call it for every path they
    /// touch, the source and the destination, so a file that comes back (undo, a
    /// move into an open folder) is read again instead of showing stale state.
    fn forget_file(&mut self, p: &Path) {
        self.core.loader.invalidate(p);
        self.core.thumbs.forget(p);
        self.viewer.forget_stat(p);
        self.core.meta.forget(p);
    }

    /// Lossless JPEG rotate / flip (undoable); returns how many files changed.
    fn lossless_transform(&mut self, jpegs: &[PathBuf], t: Transform) -> usize {
        let mut done = Vec::new();
        let mut errs = Vec::new();
        for p in jpegs {
            // The file as it was, for an exact undo.
            let before = std::fs::read(p).ok().filter(|b| b.len() <= UNDO_BYTES_BUDGET);
            match crate::encode::lossless_jpeg_transform(p, t) {
                Ok(_) => {
                    done.push((p.clone(), before));
                    self.forget_file(p);
                }
                Err(e) => errs.push(format!("{}: {e}", crate::ui::viewer::file_name(p))),
            }
        }
        if !errs.is_empty() {
            self.core.toasts.error(format!("{} not rotated - {}", count(errs.len(), "file"), errs[0]));
        }
        let ok = done.len();
        if ok > 0 {
            self.core.toasts.info(format!("{} - lossless{}", t.label(), if ok > 1 { format!(" ({ok} files)") } else { String::new() }));
            self.push_undo(FileUndo { label: t.label().to_string(), change: FileChange::Transform { t, files: done } });
        }
        ok
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
        // Saving in place writes one 8-bit frame: never do that silently over an
        // animation, a multi-page file or a high bit depth source.
        if let Some(why) = lossy_resave_reason(loaded.as_ref().map(|l| &l.decoded), &path) {
            let mut dlg = SaveDialog::new(edited_copy_name(&path), img, opts);
            dlg.error = Some(format!("{why} Save the edited image under a new name to keep the original."));
            self.save_dialog = Some(dlg);
            return;
        }
        opts.format = fmt.unwrap();
        self.do_save(&path, &img, &opts);
    }

    fn do_save(&mut self, path: &Path, img: &image::RgbaImage, opts: &SaveOptions) -> bool {
        // The open file is the source of the comment / XMP / IPTC carried into a JPEG.
        let source = self.viewer.current().cloned();
        match crate::encode::save_from(img, path, opts, source.as_deref()) {
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
                let first_frame = crate::decode::DecodeOptions { animations: false, ..Default::default() };
                let img = img.or_else(|| path.as_ref().and_then(|p| crate::decode::decode_file(p, first_frame).ok()).map(|d| d.image));
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
                self.core.toasts.success(format!("Moved {} to the Recycle Bin", count(files.len(), "item")));
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
            self.core.toasts.success(format!("{} {}", if moved { "Moved" } else { "Copied" }, count(n, "item")));
        }
    }

    /// True when the viewer has unsaved edits of one of `files`.
    fn dirty_current_in(&self, files: &[PathBuf]) -> bool {
        self.viewer.has_unsaved_work() && self.viewer.current().map(|c| files.iter().any(|f| c == f || c.starts_with(f))).unwrap_or(false)
    }

    fn transfer_files(&mut self, files: Vec<PathBuf>, dest: PathBuf, do_move: bool) {
        if do_move && self.dirty_current_in(&files) {
            self.pending_guard = Some(PendingAction::Transfer(files, dest, do_move));
            return;
        }
        self.transfer_files_now(files, dest, do_move);
    }

    fn transfer_files_now(&mut self, files: Vec<PathBuf>, dest: PathBuf, do_move: bool) {
        self.core.settings.copy_move_targets.retain(|p| p != &dest);
        self.core.settings.copy_move_targets.insert(0, dest.clone());
        self.core.settings.copy_move_targets.truncate(10);
        let label = format!("{} {}", if do_move { "Moving" } else { "Copying" }, count(files.len(), "item"));
        self.file_jobs.submit(label, transfer_job(files, dest, do_move));
    }

    /// A background file job finished: update caches, tags, undo and the views.
    fn finish_file_job(&mut self, res: JobResult) {
        match res {
            JobResult::Transfer { done, errs, dest, do_move } => self.finish_transfer(done, errs, dest, do_move),
            JobResult::Rotate { t, lossless, reencoded, skipped, errs } => {
                for (p, _) in lossless.iter().chain(&reencoded) {
                    self.forget_file(p);
                }
                if !skipped.is_empty() {
                    self.core.toasts.info(format!("Skipped {}: {}", count(skipped.len(), "file"), skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
                }
                if !errs.is_empty() {
                    self.core.toasts.error(format!("{} not changed - {}", count(errs.len(), "file"), errs[0]));
                }
                let ok = lossless.len() + reencoded.len();
                if ok > 0 {
                    let how = if reencoded.is_empty() { " - lossless" } else { "" };
                    // A re-encoded file is undone from its previous bytes only: when
                    // those were too large to keep, the change cannot be undone.
                    let undoable = reencoded.iter().all(|(_, b)| b.is_some());
                    let note = if undoable { "" } else { " (too large to undo)" };
                    self.core.toasts.info(format!("{}{how}{}{note}", t.label(), if ok > 1 { format!(" ({ok} files)") } else { String::new() }));
                    if undoable {
                        let files = lossless.into_iter().chain(reencoded).collect();
                        self.push_undo(FileUndo { label: t.label().to_string(), change: FileChange::Transform { t, files } });
                    } else {
                        // Ctrl+Z must not skip this change and undo an older one instead.
                        self.file_undo.clear();
                    }
                }
            }
            JobResult::Panicked(label) => self.core.toasts.error(format!("{label} stopped unexpectedly; check the folder")),
        }
    }

    /// Small "Copying 3 of 10" card while background file jobs run.
    fn file_job_progress(&mut self, ctx: &egui::Context, p: &Palette) {
        if self.file_jobs.queued == 0 {
            return;
        }
        let (label, done, total) = {
            let s = self.file_jobs.status.lock();
            (s.label.clone(), s.done, s.total)
        };
        let more = self.file_jobs.queued.saturating_sub(1);
        egui::Area::new(egui::Id::unique("file_job_progress"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_BOTTOM, Vec2::new(16.0, -16.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        let mut text = if total > 1 { format!("{label}: {} of {total}", (done + 1).min(total)) } else { format!("{label}...") };
                        if more > 0 {
                            text += &format!(" ({more} more queued)");
                        }
                        ui.label(RichText::new(text).color(p.text));
                    });
                    if total > 1 {
                        ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(220.0));
                    }
                });
            });
    }

    fn finish_transfer(&mut self, done: Vec<(PathBuf, PathBuf)>, errs: Vec<String>, dest: PathBuf, do_move: bool) {
        if do_move && !done.is_empty() {
            let label = format!("Move {} to {}", count(done.len(), "item"), dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dest.display().to_string()));
            self.push_file_undo(label, done.clone());
        }
        if do_move {
            self.core.tags.moved_many(&done);
        }
        for (from, to) in &done {
            if do_move {
                self.forget_file(from);
                self.viewer.remove_path(from);
            } else {
                self.core.tags.copied(from, to);
            }
            self.forget_file(to);
        }
        self.report_transfer(done.len(), errs, do_move);
        self.browser.tree.invalidate(&dest);
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
            self.mode = Mode::Browser;
        }
    }

    fn push_file_undo(&mut self, label: String, moves: Vec<(PathBuf, PathBuf)>) {
        self.push_undo(FileUndo { label, change: FileChange::Moves(moves) });
    }

    fn push_undo(&mut self, op: FileUndo) {
        self.file_undo.push(op);
        if self.file_undo.len() > 20 {
            self.file_undo.remove(0);
        }
        // Saved file contents are bounded: forget the oldest history first.
        while self.file_undo.len() > 1 && self.file_undo.iter().map(FileUndo::bytes).sum::<usize>() > UNDO_BYTES_BUDGET {
            self.file_undo.remove(0);
        }
    }

    /// Ctrl+Z with no edit to undo: put the last moved / renamed files back, or
    /// turn the last rotated / flipped files back.
    fn undo_file_op(&mut self) {
        let Some(op) = self.file_undo.pop() else {
            self.core.toasts.info("Nothing to undo. Deleted files can be restored from the Recycle Bin.");
            return;
        };
        let mut ok = 0;
        let mut errs = Vec::new();
        match &op.change {
            FileChange::Moves(moves) => {
                let mut folders: Vec<PathBuf> = Vec::new();
                for (orig, cur) in moves.iter().rev() {
                    match fileops::move_path(cur, orig) {
                        Ok(()) => {
                            ok += 1;
                            self.core.tags.moved(cur, orig);
                            self.forget_file(cur);
                            self.forget_file(orig);
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
                if let Some((orig, _)) = moves.first() {
                    if orig.parent() == Some(self.browser.folder.as_path()) {
                        self.browser.pending_select = Some(orig.clone());
                    }
                }
            }
            FileChange::Transform { t, files } => {
                for (p, bytes) in files {
                    let res = match bytes {
                        _ if !p.is_file() => Err(anyhow::anyhow!("the file no longer exists")),
                        Some(b) => crate::encode::write_atomic(p, b),
                        None => crate::encode::lossless_jpeg_transform(p, inverse(*t)).map(|_| ()),
                    };
                    match res {
                        Ok(()) => ok += 1,
                        Err(e) => errs.push(format!("{}: {e}", crate::ui::viewer::file_name(p))),
                    }
                    self.forget_file(p);
                }
                // The viewer shows the restored file (unless it holds edits of its own).
                let shown = self.viewer.current().is_some_and(|c| files.iter().any(|(p, _)| p == c));
                if shown && !self.viewer.is_dirty() {
                    self.viewer.edit = None;
                    self.viewer.reset_view(self.core.settings.fit_mode);
                }
            }
        }
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if !errs.is_empty() {
            self.core.toasts.error(format!("Undo: {} could not be restored - {}", count(errs.len(), "item"), errs[0]));
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
        let lossy = self.viewer.current().and_then(|c| lossy_resave_reason(self.viewer.loaded(&self.core).as_ref().map(|l| &l.decoded), c));
        // Saving would leave out an open tool's unapplied work: apply it first.
        let tool_work = self.viewer.tool_has_work();
        let dirty = self.viewer.is_dirty();
        let can_save = !tool_work && lossy.is_none() && self.viewer.current().map(|c| crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(c)).is_some()).unwrap_or(false);
        let mut choice: Option<u8> = None; // 0 cancel, 1 save, 2 discard
        // Same wording and buttons as the viewer's own prompt.
        crate::ui::widgets::modal(ctx, egui::Id::unique("unsaved-guard"), 400.0, |ui| {
            use crate::ui::widgets;
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                choice = Some(0);
            }
            let note = |ui: &mut egui::Ui, text: &str| {
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new(text).color(p.muted).size(crate::ui::theme::text::SMALL)).wrap());
            };
            if !dirty {
                // Only an open tool's unapplied work would be lost. Destructive:
                // red, and Enter does not discard.
                widgets::modal_title(ui, "Discard changes?");
                widgets::modal_body(ui, &format!("The changes in the open tool are not applied to \"{name}\" yet."));
                if let Some(r) = widgets::dialog_buttons(ui, &p, "Keep editing", "Discard", widgets::ButtonKind::Danger, true) {
                    choice = Some(if r { 2 } else { 0 });
                }
                return;
            }
            widgets::modal_title(ui, "Save changes?");
            widgets::modal_body(ui, &format!("\"{name}\" has unsaved edits."));
            if tool_work {
                note(ui, "The open tool also has changes that are not applied. Keep editing to apply them before saving.");
            } else if let Some(why) = &lossy {
                note(ui, &format!("{why} Use Save as from the viewer."));
            } else if !can_save {
                note(ui, "This format cannot be saved in place. Use Save as from the viewer.");
            }
            ui.add_space(crate::ui::theme::space::BUTTONS_GAP);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // [Discard] [Cancel] [Save]: the primary last, Discard away from it.
                if widgets::primary_button_enabled(ui, &p, can_save, "Save").clicked() {
                    choice = Some(1);
                }
                if widgets::secondary_button(ui, if tool_work { "Keep editing" } else { "Cancel" }).clicked() {
                    choice = Some(0);
                }
                if widgets::secondary_button(ui, "Discard").clicked() {
                    choice = Some(2);
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
                self.viewer.close_tool();
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
            || self.gps_confirm.is_some()
            || self.analysis.modal_open(&self.core)
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

    fn open_windows(&self) -> Vec<Win> {
        let mut v = Vec::new();
        for (open, w) in [
            (self.show_help, Win::Help),
            (self.show_about, Win::About),
            (self.show_settings, Win::Settings),
            (self.batch_convert.is_some(), Win::BatchConvert),
            (self.batch_rename.is_some(), Win::BatchRename),
            (self.contact_sheet.is_some(), Win::ContactSheet),
            (self.duplicates.is_some(), Win::Duplicates),
            (self.metadata_dialog.is_some(), Win::Metadata),
            (self.print_window.is_some(), Win::Print),
            (self.multipage.is_some(), Win::MultiPage),
            (self.strip.is_some(), Win::Strip),
            (self.zip.is_some(), Win::Zip),
        ] {
            if open {
                v.push(w);
            }
        }
        v
    }

    /// Keep `window_stack` in opening order: drop closed windows, append new ones.
    fn update_window_stack(&mut self) {
        let open = self.open_windows();
        self.window_stack.retain(|w| open.contains(w));
        for w in open {
            if !self.window_stack.contains(&w) {
                self.window_stack.push(w);
            }
        }
    }

    /// Close one floating window the way its own close button does.
    fn close_window(&mut self, w: Win) {
        let reload = match w {
            Win::Help => {
                self.show_help = false;
                false
            }
            Win::About => {
                self.show_about = false;
                false
            }
            Win::Settings => {
                self.show_settings = false;
                false
            }
            Win::BatchConvert => self.batch_convert.take().is_some(),
            Win::BatchRename => {
                self.batch_rename = None;
                false
            }
            Win::ContactSheet => self.contact_sheet.take().is_some(),
            Win::Duplicates => {
                self.duplicates = None;
                false
            }
            Win::Metadata => {
                if let Some((_, files)) = self.metadata_dialog.take() {
                    for f in &files {
                        self.forget_file(f);
                    }
                }
                true
            }
            Win::Print => {
                self.print_window = None;
                false
            }
            Win::MultiPage => self.multipage.take().is_some(),
            Win::Strip => self.strip.take().is_some(),
            Win::Zip => self.zip.take().is_some(),
        };
        if reload {
            self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        }
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
        // Help and About show text only: they never take keys from the viewer.
        let capturing = self.open_windows().iter().any(|w| !matches!(w, Win::Help | Win::About));
        self.tool_window_active = capturing && (self.click_in_window || over_window(hover_pos));
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
        self.update_window_stack();
        // egui drops the keyboard focus on Esc before this frame's widgets run:
        // Esc that only leaves a text field does nothing else.
        let left_field = self.was_editing && ctx.input(|i| i.key_pressed(Key::Escape));
        let typing = ctx.egui_wants_keyboard_input() || left_field;
        // Esc closes the most recently opened floating window first.
        if !typing {
            if let Some(&w) = self.window_stack.last() {
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                    self.close_window(w);
                    self.window_stack.pop();
                }
            }
        }
        // The Analysis panel's R (draw a region) and Esc (clear it) win over the mode's keys.
        if !typing && !self.tool_window_active && matches!(self.mode, Mode::Viewer | Mode::Compare | Mode::Video) {
            self.analysis.pre_shortcuts(ctx);
        }
        let km = keymap::active();
        // Edit tools own Enter / Escape and most keys; only undo / redo (of the tool) pass.
        let tool_only = self.mode == Mode::Viewer && self.viewer.has_tool();
        let mode_map = match self.mode {
            Mode::Browser => Some(KeyMode::Browser),
            Mode::Viewer => Some(KeyMode::Viewer),
            _ => None,
        };
        // Mode shortcuts are off while typing or while a tool window has the user's attention.
        let mode_map = mode_map.filter(|_| !typing && !self.tool_window_active);
        let typeahead = mode_map == Some(KeyMode::Browser) && self.browser.typeahead_active();
        // Esc with a menu or a list open only closes it (egui does), it does
        // not also leave the viewer or the mode.
        let popup = egui::Popup::is_any_open(ctx);
        let mut fire = ctx.input_mut(|i| {
            keymap::consume(i, |key, mods| {
                if popup && key == Key::Escape {
                    return None;
                }
                // Mid-word letters belong to the type-ahead search, not to single-letter commands.
                let plain = !mods.ctrl && !mods.command && !mods.alt;
                if typeahead && plain && key.name().len() == 1 {
                    return None;
                }
                if let Some(m) = mode_map {
                    if let Some(c) = km.lookup(m, key, mods) {
                        // Image navigation works like the toolbar arrows: it closes an
                        // untouched tool, or asks first when the tool has changes.
                        // Space (pan) and Backspace (delete an object) stay with the tool.
                        let nav = matches!(c, Command::NextImage | Command::PrevImage | Command::FirstImage | Command::LastImage) && !matches!(key, Key::Space | Key::Backspace);
                        if !tool_only || nav || matches!(c, Command::Undo | Command::Redo) {
                            return Some(c);
                        }
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
                if star && !tool_only {
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

    fn open_map_url(&mut self, url: &str) {
        if let Err(e) = std::process::Command::new("explorer.exe").arg(url).spawn() {
            self.core.toasts.error(format!("Could not open the map: {e}"));
        }
    }

    /// "Move to folder" / "Copy to folder": a destination field (type, paste or
    /// Browse) above the recent and favorite folders (click to pick, double-click or
    /// 1-9 to go at once). A folder that does not exist yet is created.
    fn transfer_dialog(&mut self, ctx: &egui::Context) {
        let Some(t) = &mut self.transfer else { return };
        let p = self.core.pal;
        let mut close = false;
        let mut go: Option<PathBuf> = None;
        let n = t.files.len();
        let what = if n == 1 { format!("\"{}\"", crate::ui::viewer::file_name(&t.files[0])) } else { format!("{n} items") };
        let title = format!("{} {what} to folder", if t.do_move { "Move" } else { "Copy" });
        let mut recents: Vec<PathBuf> = Vec::new();
        for d in self.core.settings.copy_move_targets.iter().chain(self.core.settings.favorites.iter()) {
            if d.is_dir() && !recents.iter().any(|r| fileops::same_path(r, d)) {
                recents.push(d.clone());
            }
        }
        recents.truncate(9);
        // Start from the most recent target; with none, an empty field to type into.
        let first_frame = t.dest.is_none();
        if first_frame {
            t.dest = Some(recents.first().map(|d| d.display().to_string()).unwrap_or_default());
        }
        widgets::modal(ctx, egui::Id::unique("transfer"), 480.0, |ui| {
            widgets::modal_title(ui, &title);
            let dest = t.dest.get_or_insert_with(String::new);
            ui.horizontal(|ui| {
                let browse_w = 36.0;
                let field = ui.add(
                    egui::TextEdit::singleline(dest)
                        .hint_text("Folder path")
                        .desired_width(ui.available_width() - browse_w - ui.spacing().item_spacing.x)
                        .min_size(Vec2::new(0.0, theme::size::CONTROL))
                        .align(egui::Align2::LEFT_CENTER),
                );
                if first_frame && dest.is_empty() {
                    field.request_focus();
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !dest.trim().is_empty() {
                    go = Some(PathBuf::from(dest.trim()));
                }
                let b = ui.add_sized([browse_w, theme::size::CONTROL], egui::Button::new(RichText::new(I::FOLDER_OPEN).size(theme::size::ICON_TOOLBAR))).on_hover_text("Browse for a folder");
                if b.clicked() {
                    let start = Some(PathBuf::from(dest.trim())).filter(|d| d.is_dir()).unwrap_or_else(|| self.browser.folder.clone());
                    if let Some(d) = rfd::FileDialog::new().set_directory(&start).pick_folder() {
                        *dest = d.display().to_string();
                    }
                }
            });
            let typed = PathBuf::from(dest.trim());
            if !dest.trim().is_empty() && !typed.is_dir() {
                ui.add_space(2.0);
                ui.label(RichText::new(format!("{}  This folder does not exist yet; it will be created.", I::INFO)).size(theme::text::SMALL).color(p.muted));
            }
            if !recents.is_empty() {
                widgets::section_title(ui, &p, "Recent and favorite folders");
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, d) in recents.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::click());
                    let selected = fileops::same_path(&typed, d);
                    if ui.is_rect_visible(rect) {
                        let painter = ui.painter();
                        let cr = egui::CornerRadius::same(theme::radius::CONTROL);
                        if selected {
                            painter.rect_filled(rect, cr, p.selected_bg);
                        } else if resp.hovered() {
                            painter.rect_filled(rect, cr, p.hover);
                        }
                        let icon_c = if selected { p.accent_text } else { p.muted };
                        painter.text(egui::pos2(rect.left() + 18.0, rect.center().y), egui::Align2::CENTER_CENTER, I::FOLDER, egui::FontId::proportional(theme::size::ICON), icon_c);
                        let key_w = widgets::paint_shortcut_keys(painter, &p, egui::pos2(rect.right() - 8.0, rect.center().y), &format!("{}", i + 1));
                        let name = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| d.display().to_string());
                        let x = rect.left() + 36.0;
                        let room = rect.right() - 8.0 - key_w - 12.0 - x;
                        let name_g = painter.layout_no_wrap(name, theme::text::body(), if selected { p.accent_text } else { p.text });
                        let name_w = name_g.size().x.min(room);
                        let clip = egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(x + room, rect.bottom()));
                        painter.with_clip_rect(clip.intersect(ui.clip_rect())).galley(egui::pos2(x, rect.center().y - name_g.size().y / 2.0), name_g, p.text);
                        if let Some(parent) = d.parent() {
                            let left = room - name_w - 10.0;
                            if left > 40.0 {
                                let pt = widgets::elide_start(ui, &parent.display().to_string(), theme::text::small(), left);
                                let g = painter.layout_no_wrap(pt, theme::text::small(), p.faint);
                                painter.galley(egui::pos2(x + name_w + 10.0, rect.center().y - g.size().y / 2.0), g, p.faint);
                            }
                        }
                    }
                    let resp = resp.on_hover_text(d.display().to_string());
                    if resp.double_clicked() {
                        go = Some(d.clone());
                    } else if resp.clicked() {
                        *dest = d.display().to_string();
                    }
                }
                // 1-9 go straight to a listed folder (while not typing a path).
                if !ui.ctx().text_edit_focused() {
                    for (i, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9].iter().enumerate() {
                        if ui.input(|inp| inp.key_pressed(*k)) {
                            if let Some(d) = recents.get(i) {
                                go = Some(d.clone());
                            }
                        }
                    }
                }
                ui.spacing_mut().item_spacing.y = 4.0;
            }
            let ok = !dest.trim().is_empty();
            match widgets::dialog_buttons(ui, &p, "Cancel", if t.do_move { "Move" } else { "Copy" }, widgets::ButtonKind::Primary, ok) {
                Some(true) => go = Some(PathBuf::from(dest.trim())),
                Some(false) => close = true,
                None => {}
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if let Some(dest) = go {
            if !dest.is_dir() {
                if let Err(e) = std::fs::create_dir_all(&dest) {
                    self.core.toasts.error(format!("Could not create {}: {e}", dest.display()));
                    return;
                }
                if let Some(parent) = dest.parent() {
                    self.browser.tree.invalidate(parent);
                }
            }
            let t = self.transfer.take().unwrap();
            self.transfer_files(t.files, dest, t.do_move);
        } else if close {
            self.transfer = None;
        }
    }

    /// "Open this location on openstreetmap.org?" (Ctrl+Alt+M), with "Don't ask again".
    fn gps_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some((_, dont_ask)) = &mut self.gps_confirm else { return };
        let p = self.core.pal;
        let mut choice: Option<bool> = None;
        widgets::modal(ctx, egui::Id::unique("gps-confirm"), 420.0, |ui| {
            widgets::modal_title(ui, "Open map in browser?");
            widgets::modal_body(ui, "Open this location on openstreetmap.org in your browser? This sends the coordinates to that website.");
            ui.add_space(10.0);
            widgets::checkbox(ui, dont_ask, "Don't ask again");
            if let Some(r) = widgets::dialog_buttons(ui, &p, "Cancel", "Open map", widgets::ButtonKind::Primary, true) {
                choice = Some(r);
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                choice = Some(false);
            } else if ui.input(|i| i.key_pressed(Key::Enter)) {
                choice = Some(true);
            }
        });
        match choice {
            Some(true) => {
                let (url, dont_ask) = self.gps_confirm.take().unwrap_or_default();
                if dont_ask {
                    self.core.settings.gps_map_no_confirm = true;
                    self.core.settings.save();
                }
                self.open_map_url(&url);
            }
            Some(false) => self.gps_confirm = None,
            None => {}
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.gps_confirm_dialog(ctx);
        let p = self.core.pal;
        if let Some((prompt, kind)) = &mut self.prompt {
            match widgets::show_prompt(ctx, prompt) {
                Some(true) => {
                    let value = prompt.value.clone();
                    // An error keeps the prompt open with the typed text.
                    let mut error: Option<String> = None;
                    match kind {
                        PendingPrompt::Rename(path) => {
                            let path = path.clone();
                            match fileops::rename(&path, &value) {
                                Ok(new) => {
                                    self.push_file_undo(format!("Rename to {}", crate::ui::viewer::file_name(&new)), vec![(path.clone(), new.clone())]);
                                    self.core.tags.moved(&path, &new);
                                    self.forget_file(&path);
                                    self.forget_file(&new);
                                    self.viewer.replace_path(&path, &new);
                                    self.browser.pending_select = Some(new);
                                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                                }
                                Err(e) => error = Some(e.to_string()),
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
                                Err(e) => error = Some(e.to_string()),
                            }
                        }
                    }
                    match error {
                        Some(e) => {
                            if let Some((prompt, _)) = &mut self.prompt {
                                prompt.error = Some(e);
                            }
                        }
                        None => self.prompt = None,
                    }
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
            // Red, but Enter still confirms: the Recycle Bin can undo it (as in Explorer).
            match widgets::show_confirm_with(ctx, "Delete", &body, "Delete", widgets::ButtonKind::Danger, true, &p) {
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
                "{} will decode and re-encode {} in place. Some quality or metadata may be lost. Animated, multi-page and 16-bit images and icons are skipped.",
                t.label(),
                count(files.len(), "file")
            );
            if !jpegs.is_empty() {
                body += &format!("\n\n{} in the selection {} rotated losslessly.", count(jpegs.len(), "JPEG file"), if jpegs.len() == 1 { "is" } else { "are" });
            }
            match widgets::show_confirm(ctx, "Re-encode files?", &body, "Re-encode", false, &p) {
                Some(true) => {
                    self.confirm = None;
                    let n = jpegs.len() + files.len();
                    self.file_jobs.submit(format!("{} ({n} files)", t.label()), rotate_job(jpegs, files, t, self.core.settings.jpeg_quality));
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        self.transfer_dialog(ctx);
        if let Some(dlg) = &mut self.save_dialog {
            match dlg.show(ctx, &p) {
                SaveOutcome::Open => {}
                SaveOutcome::Cancel => self.save_dialog = None,
                SaveOutcome::Save(path, opts) => {
                    if let Some(img) = self.viewer.current_image(&self.core) {
                        if self.do_save(&path, &img, &opts) {
                            self.save_dialog = None;
                            self.core.settings.jpeg_quality = opts.jpeg_quality;
                            // Saved to a new file: the viewer goes on with the
                            // saved copy (as saved, no edits); the original stays
                            // as it is on disk.
                            if self.mode == Mode::Viewer && self.viewer.current() != Some(&path) {
                                self.viewer.show_saved_copy(&path);
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
            crate::ui::widgets::app_window("about", "About Lumen").open(&mut open).width(420.0).anchored().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::include_image!("../assets/lumen-256.png")).fit_to_exact_size(Vec2::splat(56.0)));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new("Lumen").font(theme::text::display()));
                        ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(p.muted));
                    });
                });
                ui.add_space(12.0);
                ui.add(egui::Label::new("A fast, private image & video viewer, browser, editor and converter for Windows.").wrap());
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(I::SHIELD_CHECK).size(theme::size::ICON_TOOLBAR).color(p.ok));
                    ui.add(egui::Label::new(RichText::new("No telemetry, no accounts. Lumen itself never connects to the internet; map links open in your browser only when you ask.").color(p.muted)).wrap());
                });
                ui.add_space(12.0);
                ui.hyperlink_to("github.com/RahulTarigoppula/lumen-image-viewer", "https://github.com/RahulTarigoppula/lumen-image-viewer");
                ui.label(RichText::new("MIT licensed. Icons: Phosphor (MIT). Video decoding: FFmpeg (LGPL).").size(theme::text::SMALL).color(p.faint));
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
            self.core.tags.moved_many(&renamed);
            for (from, to) in &renamed {
                self.viewer.replace_path(from, to);
            }
            for (from, to) in &renamed {
                self.forget_file(from);
                self.forget_file(to);
            }
            if !renamed.is_empty() {
                self.push_file_undo(format!("Batch rename of {}", count(renamed.len(), "file")), renamed.clone());
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
        crate::ui::widgets::app_window("shortcuts-help", "Keyboard shortcuts").open(&mut open).default_size(640.0, 560.0).min_size(360.0, 240.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Preset: {}", keymap::Preset::from_id(&self.core.settings.keymap_preset).label())).color(p.muted));
                if ui.link("Change shortcuts...").clicked() {
                    self.show_settings = true;
                    crate::ui::settings_window::open_page(ctx, "Keyboard");
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // One grid for every section, so the key column lines up at the same x.
                // Generated from the active keymap, so this list is always correct.
                let no_wrap = |t: RichText| egui::Label::new(t).wrap_mode(egui::TextWrapMode::Extend);
                let title = |ui: &mut egui::Ui, t: &str| {
                    ui.add_space(10.0);
                    ui.add(no_wrap(RichText::new(t).font(theme::text::subtitle()).color(p.text)));
                    ui.end_row();
                };
                egui::Grid::new("help-shortcuts").num_columns(2).spacing([28.0, 4.0]).min_row_height(24.0).striped(false).show(ui, |ui| {
                    for mode in KeyMode::ALL {
                        title(ui, mode.label());
                        for c in Command::all() {
                            let keys = keymap::mode_text(&km, mode, c);
                            if keys.is_empty() {
                                continue;
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                ui.add_sized([18.0, 20.0], egui::Label::new(RichText::new(c.icon()).size(theme::size::ICON).color(p.muted)));
                                ui.add(no_wrap(RichText::new(c.label())));
                            });
                            crate::ui::widgets::shortcut_keys(ui, &p, &keys);
                            ui.end_row();
                        }
                    }
                    title(ui, "Mouse");
                    for (k, v) in [
                        ("Wheel", "Next / previous image (or zoom - configurable in Settings)"),
                        ("Ctrl + wheel", "Zoom at cursor (thumbnail size in the browser)"),
                        ("Drag", "Pan a zoomed image"),
                        ("Press & hold", "Magnifier loupe (when the image fits the window)"),
                        ("Middle click", "Toggle fit / 100%"),
                        ("Double click", "Back to the browser"),
                        ("Back / forward buttons", "Previous / next image"),
                    ] {
                        ui.add(no_wrap(RichText::new(v)));
                        ui.add(no_wrap(RichText::new(k).color(p.muted)));
                        ui.end_row();
                    }
                });
            });
        });
        self.show_help = open;
    }

    // ------------------------------------------------------------ chrome

    fn menu(&mut self, ui: &mut egui::Ui) {
        let mut fire = None;
        use crate::ui::widgets::{menu_item, menu_toggle, submenu};
        let item = |ui: &mut egui::Ui, c: Command, fire: &mut Option<Command>| {
            if menu_item(ui, c.icon(), &c.label(), &c.shortcut()).clicked() {
                *fire = Some(c);
                ui.close();
            }
        };
        ui.menu_button(RichText::new(I::LIST).size(18.0), |ui| {
            ui.set_min_width(280.0);
            item(ui, Command::OpenFile, &mut fire);
            item(ui, Command::OpenFolder, &mut fire);
            submenu(ui, I::CLOCK_COUNTER_CLOCKWISE, "Recent folders", |ui| {
                ui.set_min_width(240.0);
                for r in self.core.settings.recent_folders.clone() {
                    if menu_item(ui, I::FOLDER, &r.display().to_string(), "").clicked() {
                        self.navigate(r);
                        ui.close();
                    }
                }
            });
            ui.separator();
            submenu(ui, I::PENCIL_SIMPLE, "Edit", |ui| {
                for c in [Command::SelectAll, Command::SelectNone, Command::InvertSelection, Command::CopyFiles, Command::PasteFiles, Command::CopyTo, Command::MoveTo, Command::Rename, Command::Delete, Command::NewFolder] {
                    item(ui, c, &mut fire);
                }
            });
            let (details, preview, flagged, full) = (self.core.settings.browser_view == crate::settings::BrowserView::Details, self.core.settings.show_preview_pane, self.browser.flagged_only, self.fullscreen);
            submenu(ui, I::EYE, "View", |ui| {
                // On / off settings show a check mark instead of "Toggle ...".
                for (c, label, on) in [(Command::ToggleDetails, "Details view", details), (Command::TogglePreviewPane, "Preview pane", preview), (Command::ShowFlaggedOnly, "Flagged only", flagged), (Command::ToggleFullscreen, "Full screen", full)] {
                    if menu_toggle(ui, on, label, &c.shortcut()).clicked() {
                        fire = Some(c);
                        ui.close();
                    }
                }
                ui.separator();
                for c in [Command::ThumbBigger, Command::ThumbSmaller, Command::ToggleTheme] {
                    item(ui, c, &mut fire);
                }
                ui.separator();
                submenu(ui, I::TEXTBOX, "Thumbnail captions", |ui| {
                    let s = &mut self.core.settings;
                    let cur = if s.show_thumb_captions { s.thumb_caption } else { crate::settings::ThumbCaption::None };
                    for c in crate::settings::ThumbCaption::ALL {
                        if menu_toggle(ui, cur == c, c.label(), "").clicked() {
                            s.set_thumb_caption(c);
                            ui.close();
                        }
                    }
                });
            });
            submenu(ui, I::IMAGE, "Image", |ui| {
                // The edit tools by group (as in the viewer's Edit menu): one short
                // list instead of 25 rows that would not fit the window.
                for (g, name) in Command::EDIT_GROUPS.iter().zip(crate::ui::viewer::EDIT_GROUP_NAMES) {
                    submenu(ui, g[0].icon(), name, |ui| {
                        for c in g.iter() {
                            item(ui, *c, &mut fire);
                        }
                    });
                }
                ui.separator();
                for c in [Command::SetWallpaper, Command::Print] {
                    item(ui, c, &mut fire);
                }
            });
            submenu(ui, I::WRENCH, "Tools", |ui| {
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
        if self.viewer.has_unsaved_work() {
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
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        raw.events.append(&mut self.dev_input);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let protect = self.viewer.protected_paths();
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
        if self.fullscreen && self.fullscreen_from_viewer && self.mode == Mode::Browser {
            self.set_fullscreen(false);
        }
        crate::analysis_ui::tick(&mut self.core);
        while let Ok(res) = self.file_jobs.rx.try_recv() {
            self.file_jobs.queued = self.file_jobs.queued.saturating_sub(1);
            self.finish_file_job(res);
        }
        if self.last_save.elapsed().as_secs() > 5 {
            self.core.tags.save_if_dirty();
            self.last_save = Instant::now();
        }
        if self.settings_check.elapsed() >= std::time::Duration::from_secs(1) {
            self.settings_check = Instant::now();
            self.autosave_settings();
        }
        let _ = ctx;
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let frame_start = std::time::Instant::now();
        // `bench`: time between frames and eframe's CPU time of the last
        // frame (logic, layout, tessellation, texture uploads and painting).
        if let Some(t) = self.last_frame_start.replace(frame_start) {
            crate::devhook::prof_ms("frame_interval", frame_start.duration_since(t).as_secs_f64() * 1000.0);
        }
        if let Some(cpu) = frame.info().cpu_usage {
            crate::devhook::prof_ms("frame_cpu", cpu as f64 * 1000.0);
        }
        if self.startup_frames < 2 {
            self.startup_frames += 1;
            // The second frame starts once the first one has been painted and presented.
            crate::devhook::mark(if self.startup_frames == 1 { "first_frame" } else { "first_frame_shown" });
        }
        // Window close (X, Alt+F4, taskbar) with unsaved edits: ask first.
        if ctx.input(|i| i.viewport().close_requested()) && self.viewer.has_unsaved_work() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.pending_guard.is_none() {
                self.pending_guard = Some(PendingAction::Exit);
            }
        }
        // Back from another program: folders may have been created, renamed or
        // removed there, so the tree lists its open folders again.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.window_focused {
            self.browser.tree.refresh();
        }
        self.window_focused = focused;
        self.dev_actions(&ctx);
        crate::ui::gate_background_tooltips(&ctx);
        self.handle_dropped(&ctx);
        self.clipboard_keys(&ctx);
        self.shortcuts(&ctx);
        self.browser.suppress_keys = self.modal_open() || self.tool_window_active;
        self.update_title(&ctx);
        self.viewer.edge_panels = self.core.settings.edge_panels;
        let p = self.core.pal;

        // The Analysis panel sits right of the mode's whole view, so it is shown
        // before the mode's central panel; its overlays are drawn after it.
        #[cfg(feature = "video")]
        if self.mode != Mode::Video {
            // Video mode publishes its state every frame; drop it (and the
            // frames it holds) elsewhere.
            self.core.metrics.set_video(None);
        }
        self.core.analysis_open = self.analysis.open;
        let subject = self.analysis_subject();
        let t_panel = Instant::now();
        let mut analysis_actions = match (&subject, self.analysis.open) {
            (Some(s), true) => self.analysis.side_panel(ui, &mut self.core, s),
            _ => Vec::new(),
        };
        crate::devhook::prof("analysis_panel", t_panel);

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
                    .exact_size(crate::ui::theme::size::STATUS_BAR)
                    .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(12, 0)))
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(RichText::new(self.browser.status_text(&self.core)).size(crate::ui::theme::text::SMALL).color(p.muted));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Thumbnail size at the far right (Explorer and Lightroom put it there).
                                self.browser.status_size_slider(ui, &mut self.core);
                                if !crate::analysis_ui::network_badge(ui, &self.core) {
                                    ui.label(RichText::new(format!("{} Offline & private", I::SHIELD_CHECK)).size(11.5).color(p.faint))
                                        .on_hover_text(crate::analysis_ui::privacy_note(&self.core));
                                }
                                if self.browser.loading {
                                    ui.spinner();
                                }
                            });
                        });
                    });
                let range = crate::ui::browser::sidebar_range(&ctx);
                let tree_w = self.core.settings.tree_width.clamp(*range.start(), *range.end());
                egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(tree_w)
                    .size_range(range)
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
                                        self.forget_file(f);
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
                        let t = Instant::now();
                        keep = c.ui(ui, &mut self.core);
                        crate::devhook::prof("compare", t);
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

        let t_overlay = Instant::now();
        analysis_actions.extend(self.analysis.overlay(ui, &mut self.core, subject.as_ref()));
        crate::devhook::prof("analysis_overlay", t_overlay);
        self.analysis_actions(&ctx, analysis_actions);
        match self.core.analysis_request.take() {
            Some(crate::analysis_ui::PanelRequest::Toggle) => self.run(Command::ToggleAnalysis),
            Some(crate::analysis_ui::PanelRequest::Open) if !self.analysis.open => self.run(Command::ToggleAnalysis),
            _ => {}
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
        self.file_job_progress(&ctx, &p);
        // Above the status bar in the browser; above the floating bottom bar (and
        // its 12 pt gap) of the viewer, compare and video modes.
        self.core.toasts.set_bottom(if self.mode == Mode::Browser { 44.0 } else { 84.0 });
        self.core.toasts.show(&ctx, &p);
        self.was_editing = ctx.text_edit_focused();
        crate::devhook::prof("ui_total", frame_start);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Let running copy / move jobs finish (bounded) so their tags and undo
        // records are not lost.
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while self.file_jobs.queued > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.file_jobs.rx.recv_timeout(left) {
                Ok(res) => {
                    self.file_jobs.queued -= 1;
                    self.finish_file_job(res);
                }
                Err(_) => break,
            }
        }
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
                crate::devhook::DevAction::Analysis(args) => {
                    let subject = self.analysis_subject();
                    let (reply, actions) = crate::analysis_ui::dev(&mut self.analysis, &mut self.core, subject.as_ref(), &args);
                    self.analysis_actions(ctx, actions);
                    self.dev.write_state(&reply);
                }
                crate::devhook::DevAction::Key(name) => match crate::ui::keymap::parse_shortcut(&name) {
                    Some(sc) => ctx.input_mut(|i| {
                        // Windows reports Ctrl as both ctrl and command.
                        let m = egui::Modifiers { ctrl: sc.modifiers.ctrl || sc.modifiers.command, command: sc.modifiers.ctrl || sc.modifiers.command, ..sc.modifiers };
                        i.modifiers = m;
                        for pressed in [true, false] {
                            i.events.push(egui::Event::Key { key: sc.logical_key, physical_key: Some(sc.logical_key), pressed, repeat: false, modifiers: m });
                        }
                    }),
                    None => self.dev.write_state(&format!("unknown key {name}")),
                },
                crate::devhook::DevAction::Pointer { pos, secondary, pressed } => {
                    // Pointer state is built from raw input: see `raw_input_hook`.
                    let button = if secondary { egui::PointerButton::Secondary } else { egui::PointerButton::Primary };
                    self.dev_input.push(egui::Event::PointerMoved(pos));
                    self.dev_input.push(egui::Event::PointerButton { pos, button, pressed, modifiers: egui::Modifiers::NONE });
                    ctx.request_repaint();
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
                        "mode={:?} folder={} items={} focus={:?} selected={} viewer={} [{}/{}] zoom={:.3} load={} dirty={} window_focused={} file_undo={}",
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
                        self.viewer.is_dirty(),
                        self.window_focused,
                        self.file_undo.len()
                    );
                    self.dev.write_state(&s);
                }
            }
        }
    }

    /// Writes settings.json shortly after a change (once it has been stable
    /// for a check interval, so a slider drag is one write), so a crash or
    /// a killed process does not lose what was set.
    fn autosave_settings(&mut self) {
        let Ok(now) = serde_json::to_string(&self.core.settings) else { return };
        if self.settings_saved.is_empty() {
            // First check: this is what was loaded.
            self.settings_saved = now.clone();
        } else if now != self.settings_saved && now == self.settings_seen {
            self.core.settings.save();
            self.settings_saved = now.clone();
        }
        self.settings_seen = now;
    }

    fn persist(&mut self) {
        self.core.settings.window_maximized = self.core.ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        self.core.settings.save();
        self.core.tags.save_if_dirty();
    }
}

#[allow(dead_code)]
fn _unused(_: &catalog::Entry) {}

/// Why saving an edit in place over `path` would lose data (frames, pages or
/// bit depth), or `None` when an in-place save is safe. Without decode info
/// (evicted from the cache) formats that can hold more than one frame are
/// treated as unsafe.
fn lossy_resave_reason(decoded: Option<&crate::decode::Decoded>, path: &Path) -> Option<String> {
    match decoded {
        Some(d) if d.frames.is_some() => Some("This file is animated: saving it in place would keep only one frame.".into()),
        Some(d) if d.frame_count > 1 => Some(format!("This file has {} pages: saving it in place would keep only one page.", d.frame_count)),
        Some(d) if d.bit_depth > 8 => Some(format!("This image has {} bits per channel: saving it in place would reduce it to 8 bits.", d.bit_depth)),
        Some(_) => None,
        None => {
            let ext = crate::formats::ext_of(path);
            matches!(ext.as_str(), "gif" | "tif" | "tiff" | "webp" | "apng").then(|| "This file may be animated or have several pages: saving it in place could keep only one frame.".into())
        }
    }
}

/// "photo.gif" -> "photo edited.gif" (or "photo edited (2).gif" when taken).
fn edited_copy_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "image".into());
    let name = match path.extension() {
        Some(e) => format!("{stem} edited.{}", e.to_string_lossy()),
        None => format!("{stem} edited"),
    };
    fileops::unique_path(&path.with_file_name(name))
}

// ------------------------------------------------------------ background file jobs

/// Progress of the background file job that is running now.
#[derive(Default)]
struct JobStatus {
    label: String,
    done: usize,
    total: usize,
}

/// What a finished background file job hands back to the UI thread.
enum JobResult {
    Transfer { done: Vec<(PathBuf, PathBuf)>, errs: Vec<String>, dest: PathBuf, do_move: bool },
    /// Changed files with their previous bytes for undo (`None` once the
    /// budget for keeping them is used up).
    Rotate { t: Transform, lossless: Vec<(PathBuf, Option<Vec<u8>>)>, reencoded: Vec<(PathBuf, Option<Vec<u8>>)>, skipped: Vec<String>, errs: Vec<String> },
    Panicked(String),
}

struct JobProgress {
    status: Arc<Mutex<JobStatus>>,
    ctx: egui::Context,
}

impl JobProgress {
    fn set(&self, done: usize, total: usize) {
        {
            let mut s = self.status.lock();
            s.done = done;
            s.total = total;
        }
        self.ctx.request_repaint();
    }
}

type FileJob = Box<dyn FnOnce(&JobProgress) -> JobResult + Send>;

/// Copy / move / paste / rotate run on one worker thread, one job after the
/// other: the UI never blocks and two jobs never touch the same file at once.
struct FileJobs {
    tx: Sender<(String, FileJob)>,
    rx: Receiver<JobResult>,
    status: Arc<Mutex<JobStatus>>,
    /// Submitted jobs whose result has not been handled yet.
    queued: usize,
}

impl FileJobs {
    fn new(ctx: egui::Context) -> Self {
        let (tx, jobs) = crossbeam_channel::unbounded::<(String, FileJob)>();
        let (res_tx, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(JobStatus::default()));
        let st = status.clone();
        let spawned = std::thread::Builder::new().name("lumen-file-jobs".into()).spawn(move || {
            for (label, job) in jobs {
                *st.lock() = JobStatus { label: label.clone(), done: 0, total: 0 };
                let progress = JobProgress { status: st.clone(), ctx: ctx.clone() };
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&progress))).unwrap_or(JobResult::Panicked(label));
                if res_tx.send(res).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        if let Err(e) = spawned {
            log::error!("cannot start the file job thread: {e}");
        }
        FileJobs { tx, rx, status, queued: 0 }
    }

    fn submit(&mut self, label: impl Into<String>, job: FileJob) {
        if self.tx.send((label.into(), job)).is_ok() {
            self.queued += 1;
        }
    }
}

/// Copy or move `files` into `dest` one by one, reporting progress.
fn transfer_job(files: Vec<PathBuf>, dest: PathBuf, do_move: bool) -> FileJob {
    Box::new(move |pr| {
        let total = files.len();
        let (mut done, mut errs) = (Vec::new(), Vec::new());
        for (i, f) in files.iter().enumerate() {
            pr.set(i, total);
            let (d, e) = fileops::transfer(std::slice::from_ref(f), &dest, do_move);
            done.extend(d);
            errs.extend(e);
        }
        JobResult::Transfer { done, errs, dest, do_move }
    })
}

/// Lossless JPEG rotation of `jpegs`, then decode / transform / re-encode of
/// `others` in place. Animations, multi-page files, high bit depth images and
/// icons are skipped, since re-encoding would lose frames or precision.
fn rotate_job(jpegs: Vec<PathBuf>, others: Vec<PathBuf>, t: Transform, jpeg_quality: u8) -> FileJob {
    Box::new(move |pr| {
        let total = jpegs.len() + others.len();
        let (mut lossless, mut reencoded, mut skipped, mut errs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // The files as they were, for undo, within a memory budget.
        let mut kept = 0usize;
        let mut keep = |p: &Path| {
            let b = std::fs::read(p).ok().filter(|b| kept + b.len() <= UNDO_BYTES_BUDGET)?;
            kept += b.len();
            Some(b)
        };
        for (i, p) in jpegs.iter().enumerate() {
            pr.set(i, total);
            let before = keep(p);
            match crate::encode::lossless_jpeg_transform(p, t) {
                Ok(_) => lossless.push((p.clone(), before)),
                Err(e) => errs.push(format!("{}: {e}", crate::ui::viewer::file_name(p))),
            }
        }
        for (i, p) in others.iter().enumerate() {
            pr.set(jpegs.len() + i, total);
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
            if d.bit_depth > 8 || ci.contains("16-bit") || ci.contains("32-bit") || ci.contains("float") {
                skipped.push(format!("{name} (more than 8 bits per channel)"));
                continue;
            }
            let before = keep(p);
            let out = t.apply(&d.image);
            let opts = SaveOptions { format: fmt, jpeg_quality, exif: d.exif.clone(), ..Default::default() };
            match crate::encode::save_from(&out, p, &opts, Some(p)) {
                Ok(()) => reencoded.push((p.clone(), before)),
                Err(e) => errs.push(format!("{name}: {e}")),
            }
        }
        JobResult::Rotate { t, lossless, reencoded, skipped, errs }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_copy_names() {
        let d = std::env::temp_dir().join(format!("lumen-app-names-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        assert_eq!(edited_copy_name(&d.join("a.gif")), d.join("a edited.gif"));
        assert_eq!(lossy_resave_reason(None, Path::new("x.png")), None);
        assert!(lossy_resave_reason(None, Path::new("x.GIF")).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inverse_transform_restores_the_orientation() {
        use crate::encode::compose_orientation;
        for t in [Transform::Rotate90, Transform::Rotate180, Transform::Rotate270, Transform::FlipH, Transform::FlipV] {
            for o in 1..=8u8 {
                assert_eq!(compose_orientation(compose_orientation(o, t), inverse(t)), o, "{t:?} from {o}");
            }
        }
    }
}
