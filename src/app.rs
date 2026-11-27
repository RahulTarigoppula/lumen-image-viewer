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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Services shared by every mode.
pub struct Core {
    pub ctx: egui::Context,
    pub settings: Settings,
    pub pal: Palette,
    pub tags: TagStore,
    /// Custom file orders per folder (drag thumbnails to arrange).
    pub orders: lumen_core::order::OrderStore,
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
    /// A camera comparison project (`ui::workbench`).
    Workbench,
}

enum PendingPrompt {
    Rename(PathBuf),
    NewFolder(PathBuf),
}

enum Confirm {
    /// `permanent`: some files are on a network or removable drive, which has no Recycle Bin.
    Delete { files: Vec<PathBuf>, permanent: bool },
    /// Rotate / flip non-JPEG files by decoding and re-encoding them in place.
    Reencode { files: Vec<PathBuf>, jpegs: Vec<PathBuf>, t: Transform },
    /// Saving in place over a file that changed on disk after it was opened
    /// (Save anyway / Save as copy / Cancel). `then` waited for the save.
    SaveChanged { path: PathBuf, opts: SaveOptions, then: Option<PendingAction> },
}

/// An action that would discard unsaved edits; held while the user decides.
enum PendingAction {
    OpenFile(PathBuf),
    /// The browser item with this path (found again when it runs: the list
    /// may change while a save it waits for runs).
    OpenViewerAt(PathBuf),
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
    /// `t` was applied to files on disk (`None`: a lossless JPEG crop). Each
    /// file is restored from its previous bytes when they were kept; without
    /// them (only for JPEGs turned losslessly) the inverse transform turns it
    /// back.
    Transform { t: Option<Transform>, files: Vec<Transformed> },
    /// A folder was arranged by hand: its previous custom order (none) and sort key.
    Order { folder: PathBuf, previous: Option<Vec<String>>, active: bool },
    /// Moved to the Recycle Bin at `at` (undo takes them out again).
    Delete { files: Vec<DeletedItem>, at: std::time::SystemTime },
    /// Ratings, flags or color labels changed: each file with its tags before.
    /// `id` ties the change to its notice's Undo button.
    Tags { id: u64, before: Vec<(PathBuf, crate::tags::Tag)> },
}

/// One file or folder moved to the Recycle Bin, as kept for undo.
#[derive(Clone)]
struct DeletedItem {
    path: PathBuf,
    /// Its rating, flag and label (dropped with the file, put back with it).
    tag: crate::tags::Tag,
    /// Its place in the viewer's list, when it was there.
    viewer_at: Option<usize>,
}

/// One file rotated / flipped on disk, as kept for undo.
struct Transformed {
    path: PathBuf,
    /// The previous contents, when kept (see `FileChange::Transform`).
    before: Option<Vec<u8>>,
    /// Size and time just before and just after the change: undo leaves a
    /// file alone that no longer matches `after` (changed again since).
    was: Option<fileops::FileStamp>,
    after: Option<fileops::FileStamp>,
}

impl FileUndo {
    /// Memory held by saved file contents.
    fn bytes(&self) -> usize {
        match &self.change {
            FileChange::Transform { files, .. } => files.iter().filter_map(|f| f.before.as_ref()).map(Vec::len).sum(),
            FileChange::Moves(_) | FileChange::Order { .. } | FileChange::Delete { .. } | FileChange::Tags { .. } => 0,
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

/// The main window's HWND, from eframe's window handle.
fn main_window(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// The notice after rating, flagging or labelling `n` files: "Rated 3 stars",
/// "Rated 3 stars · 50 files", "Flagged 4 files", "Label red · 2 files".
fn tag_notice(cmd: Command, flag_on: bool, n: usize) -> String {
    let files = if n > 1 { format!(" \u{00B7} {}", count(n, "file")) } else { String::new() };
    match cmd {
        Command::Rate(0) => format!("Rating cleared{files}"),
        Command::Rate(r) => format!("Rated {}{files}", count(r as usize, "star")),
        Command::ToggleFlag if n > 1 => format!("{} {}", if flag_on { "Flagged" } else { "Flag removed from" }, count(n, "file")),
        Command::ToggleFlag => (if flag_on { "Flagged" } else { "Flag removed" }).to_string(),
        Command::Label(0) => format!("Label cleared{files}"),
        Command::Label(l) => format!("Label {}{files}", crate::tags::LABEL_NAMES[(l as usize).min(5)].to_lowercase()),
        _ => String::new(),
    }
}

/// The first (or `last`) of one folder's `entries` sorted by `key` as the browser sorts.
/// The slow half of `App::folder_end_job`, on a worker: the file dates,
/// capture dates and sizes the sort needs (read in parallel for folders of up
/// to a few thousand pictures; larger ones use what is known), then the end.
fn folder_end_sorted(
    shown: Vec<(PathBuf, crate::tags::Tag)>,
    key: crate::settings::SortKey,
    desc: bool,
    positions: HashMap<String, usize>,
    mut dims: HashMap<PathBuf, (u32, u32)>,
    mut taken: HashMap<PathBuf, i64>,
    last: bool,
) -> Option<PathBuf> {
    use crate::settings::SortKey;
    use rayon::prelude::*;
    const READ_AT_MOST: usize = 3000;
    let stat = matches!(key, SortKey::Size | SortKey::Modified | SortKey::Created | SortKey::DateTaken);
    let entries: Vec<catalog::Entry> = shown
        .par_iter()
        .map(|(p, _)| {
            let e = if stat { catalog::Entry::from_path(p) } else { None };
            e.unwrap_or_else(|| catalog::Entry { path: p.to_path_buf(), name: crate::ui::viewer::file_name(p), ext: crate::formats::ext_of(p), kind: EntryKind::Image, size: 0, modified: None, created: None, hidden: false })
        })
        .collect();
    let read = entries.len() <= READ_AT_MOST;
    if key == SortKey::Dimensions && read {
        let more: Vec<(PathBuf, (u32, u32))> = entries.par_iter().filter(|e| !dims.contains_key(&e.path)).filter_map(|e| crate::decode::probe_dimensions(&e.path).map(|d| (e.path.clone(), d))).collect();
        dims.extend(more);
    }
    if key == SortKey::DateTaken && read {
        let more: Vec<(PathBuf, i64)> = entries.par_iter().filter(|e| !taken.contains_key(&e.path)).filter_map(|e| crate::meta::read_taken(&e.path).map(|t| (e.path.clone(), t))).collect();
        taken.extend(more);
    }
    let tags: HashMap<PathBuf, crate::tags::Tag> = shown.into_iter().collect();
    let tag = |p: &Path| tags.get(p).copied().unwrap_or_default();
    let dims_of = |p: &Path| dims.get(p).copied();
    let taken_of = |p: &Path| taken.get(p).copied();
    let rating = |p: &Path| tag(p).rating;
    let flagged = |p: &Path| tag(p).flagged;
    let label = |p: &Path| tag(p).label;
    let custom = |p: &Path| p.file_name().and_then(|n| positions.get(&n.to_string_lossy().to_lowercase()).copied());
    let cx = catalog::SortContext { dims: &dims_of, taken: &taken_of, rating: &rating, flagged: &flagged, label: &label, custom: &custom };
    end_in_order(entries, key, desc, &cx, last)
}

fn end_in_order(mut entries: Vec<catalog::Entry>, key: crate::settings::SortKey, desc: bool, cx: &catalog::SortContext, last: bool) -> Option<PathBuf> {
    catalog::sort_entries(&mut entries, key, desc, false, cx);
    let end = if last { entries.pop() } else { entries.into_iter().next() };
    end.map(|e| e.path)
}

struct TransferDialog {
    files: Vec<PathBuf>,
    do_move: bool,
    /// The destination field (filled with the most recent target when it opens).
    dest: Option<String>,
    /// Recent and favorite folders that exist, checked once when the dialog
    /// opens (an offline network folder must not stall every frame).
    recents: Option<Vec<PathBuf>>,
    /// The typed destination and whether it is an existing folder (checked when it changes).
    typed_dir: Option<(String, bool)>,
}

/// A save running on the file job thread. The edit session stays unsaved
/// until it is done, so leaving the image meanwhile still asks first.
struct Saving {
    /// The job's label (a job that panics reports it).
    label: String,
    path: PathBuf,
    /// The image being written: once saved, the edit session is clean while
    /// it shows this one (edits made meanwhile stay unsaved).
    img: Arc<image::RgbaImage>,
    /// The open file when the save started.
    source: Option<PathBuf>,
    /// The action that waited for the save (the unsaved-edits question).
    then: Option<PendingAction>,
    /// Save as: its dialog, closed while saving and back if the save fails.
    dialog: Option<SaveDialog>,
}

pub struct App {
    pub core: Core,
    pub mode: Mode,
    pub browser: Browser,
    pub viewer: Viewer,
    slideshow: Option<crate::ui::slideshow::Slideshow>,
    compare: Option<crate::ui::compare::Compare>,
    /// The open camera comparison project.
    workbench: Option<crate::ui::workbench::WorkbenchUi>,
    /// A project being opened (read, or found in its folder) on a worker.
    opening_project: Option<Receiver<anyhow::Result<(crate::ui::workbench::WorkbenchUi, bool)>>>,
    /// The EXIF / Metadata window (viewer, compare and browser).
    meta_window: crate::ui::metadata_view::MetadataWindow,
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
    /// The keyboard shortcuts window's search text, and "focus its field next frame".
    help_query: String,
    help_focus: bool,
    show_about: bool,
    /// The default report branding, opened from the command palette.
    report_branding: Option<crate::ui::branding::BrandingDialog>,
    fullscreen: bool,
    viewer_from_browser: bool,
    last_save: Instant,
    /// When the viewer last said "last image" / "first image" (and which way):
    /// a second press soon after goes on to the next / previous folder.
    end_notice: Option<(Instant, isize)>,
    /// The next / previous folder being looked for on a worker thread: (the
    /// folder it was asked from, the direction, the answer).
    sibling_lookup: Option<(PathBuf, isize, Receiver<Option<(PathBuf, Vec<PathBuf>)>>)>,
    /// Then the picture to open there: asked from, the folder, the picture by
    /// name (if the sort finds none), and the worker's answer.
    sibling_pick: Option<(PathBuf, PathBuf, PathBuf, Receiver<Option<PathBuf>>)>,
    /// "Next image after rating": when to step on (a short pause shows the new tag first).
    advance_at: Option<Instant>,
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
    /// Video mode was opened from the viewer's video (return there when it closes).
    video_from_viewer: bool,
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
    /// The save running as a file job (one at a time).
    saving: Option<Saving>,
    /// The label of the undo running as a file job: the next undo waits for
    /// it (an older change expects the files as this one leaves them).
    undoing: Option<String>,
    /// Ctrl+Alt+M asked to open this map link: waiting for the privacy
    /// confirmation (url, "don't ask again" checkbox).
    gps_confirm: Option<(String, bool)>,
    /// Open floating windows, oldest first (Esc closes the last one).
    window_stack: Vec<Win>,
    /// Full screen was entered from the viewer: leave it when the viewer closes.
    fullscreen_from_viewer: bool,
    /// The window was maximized before going full screen.
    maximized_before_fullscreen: bool,
    /// The main window's handle (HWND), for the Windows share window.
    hwnd: Option<isize>,
    /// Files dragged over the window from another program (see `droptarget`).
    os_drag: Option<crate::droptarget::OsDrag>,
    /// Files from another program dropped this frame, with where and how.
    os_drop: Option<crate::droptarget::OsDrag>,
    /// Where such files would go if dropped now: the folder, and whether it
    /// is the empty area of the folder shown (for the browser's highlight).
    os_target: Option<(PathBuf, bool)>,
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
                orders: lumen_core::order::OrderStore::load(),
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
            workbench: None,
            opening_project: None,
            meta_window: Default::default(),
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
            help_query: String::new(),
            help_focus: false,
            show_about: false,
            report_branding: None,
            fullscreen: false,
            viewer_from_browser: false,
            last_save: Instant::now(),
            end_notice: None,
            sibling_lookup: None,
            sibling_pick: None,
            advance_at: None,
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
            video_from_viewer: false,
            startup_frames: 0,
            last_frame_start: None,
            dev_input: Vec::new(),
            was_editing: false,
            window_focused: true,
            file_jobs: FileJobs::new(ctx.clone()),
            saving: None,
            undoing: None,
            gps_confirm: None,
            window_stack: Vec::new(),
            fullscreen_from_viewer: false,
            maximized_before_fullscreen: false,
            hwnd: main_window(cc),
            os_drag: None,
            os_drop: None,
            os_target: None,
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
        // The main window owns the files Lumen puts on the clipboard, and gets
        // Lumen's own drop target (move or copy into folders, see droptarget.rs).
        {
            use raw_window_handle::HasWindowHandle as _;
            if let Ok(h) = cc.window_handle() {
                if let raw_window_handle::RawWindowHandle::Win32(w) = h.as_raw() {
                    let hwnd = w.hwnd.get();
                    fileops::set_main_window(hwnd);
                    crate::droptarget::install(hwnd, &ctx);
                }
            }
        }
        for msg in crate::ui::batch::recover_pending_renames() {
            app.core.toasts.info(msg);
        }
        crate::devhook::mark("renames");
        app.viewer.show_info = app.core.settings.info_panel_on_start;
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
        // A video that opens in video mode leaves the viewer (and its edits) as they are.
        let to_video_mode = crate::video::is_video_path(path) && self.core.settings.video.open_in_video_mode;
        if !to_video_mode && self.viewer.has_unsaved_work() {
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
        if crate::video::is_video_path(path) && self.core.settings.video.open_in_video_mode {
            self.open_video_mode(vec![path.to_path_buf()]);
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
            // Several videos selected: they open together in video mode to compare.
            EntryKind::Video if self.core.settings.video.open_in_video_mode || self.selected_videos_with(&entry.path).len() >= 2 => {
                let vids = self.selected_videos_with(&entry.path);
                let list = if vids.len() >= 2 { vids } else { vec![entry.path.clone()] };
                self.open_video_mode(list);
            }
            // One video plays in the viewer, between the folder's images.
            EntryKind::Image | EntryKind::Video => {
                if self.viewer.has_unsaved_work() {
                    if self.viewer.current() == Some(&entry.path) {
                        self.mode = Mode::Viewer;
                    } else {
                        self.pending_guard = Some(PendingAction::OpenViewerAt(entry.path));
                    }
                    return;
                }
                let list = self.browser.media_paths();
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

    /// The selected videos, when `path` is one of them (else none).
    fn selected_videos_with(&self, path: &Path) -> Vec<PathBuf> {
        let vids: Vec<PathBuf> = self.browser.selected_paths().into_iter().filter(|p| crate::video::is_video_path(p)).collect();
        if vids.iter().any(|p| p == path) {
            vids
        } else {
            Vec::new()
        }
    }

    /// Show `paths` in video mode (frame by frame, analysis, comparison).
    /// Opened from the viewer, leaving video mode goes back there.
    fn open_video_mode(&mut self, paths: Vec<PathBuf>) {
        self.video_from_viewer = self.mode == Mode::Viewer;
        // The viewer's video stops first: one decoder and one sound at a time.
        self.viewer.stop_video();
        self.video = Some(crate::video::VideoMode::new(paths, &self.core));
        self.mode = Mode::Video;
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
        // Videos play in the viewer too, in the folder's order.
        let list = self.browser.media_paths();
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
        // Playback commands (palette, menus) go to the video when it is on screen.
        if self.mode == Mode::Video {
            if let Some(v) = &mut self.video {
                if v.run_command(cmd) {
                    return;
                }
            }
        }
        // ... and to the viewer's video when one plays there.
        if self.mode == Mode::Viewer && self.viewer.video_command(cmd, &mut self.core.settings.video) {
            return;
        }
        // Compare's commands (palette, menus) go to the comparison on screen.
        if self.mode == Mode::Compare {
            if let Some(c) = &mut self.compare {
                if c.run_command(cmd, &mut self.core) {
                    return;
                }
            }
        }
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
            OpenProject => {
                let start = self.workbench.as_ref().map(|w| w.ws.root.clone()).unwrap_or_else(|| self.browser.folder.clone());
                if let Some(p) = rfd::FileDialog::new().set_title("Folder with the captures of every device").set_directory(&start).pick_folder() {
                    self.open_project(&p);
                }
            }
            BlindTest => match &mut self.workbench {
                Some(w) => {
                    w.start_blind(std::env::var("USERNAME").unwrap_or_default());
                    if w.blind.as_ref().is_some_and(|b| b.trial.is_some()) {
                        self.open_blind_trial();
                    } else {
                        w.blind = None;
                        self.core.toasts.info("A blind test needs scenes with captures from at least two devices");
                    }
                }
                None => self.core.toasts.info("Open a camera comparison project first (Tools, Workbench)"),
            },
            ShowExif | ShowMetadata => {
                use crate::ui::metadata_view::MetaTab;
                // EXIF opens its tab; Metadata the tab used last (Capture at first).
                let tab = if cmd == ShowExif { MetaTab::Exif } else { self.meta_window.tab };
                let blind = self.compare.as_ref().is_some_and(|c| c.link.as_ref().is_some_and(|l| l.blind));
                if self.mode == Mode::Compare && blind {
                    self.core.toasts.info("Metadata is hidden during a blind test");
                    return;
                }
                let accent = self.core.pal.accent;
                let files: Vec<(String, PathBuf, egui::Color32)> = match self.mode {
                    Mode::Compare => self.compare.as_ref().map(|c| c.info_files(&self.core)).unwrap_or_default(),
                    Mode::Viewer => self.viewer.current().map(|p| vec![(String::new(), p.clone(), accent)]).unwrap_or_default(),
                    _ => {
                        let sel: Vec<PathBuf> = self.browser.media_targets().into_iter().filter(|p| crate::formats::is_image_path(p)).take(4).collect();
                        let letters = ["A", "B", "C", "D"];
                        if sel.len() == 1 {
                            vec![(String::new(), sel[0].clone(), accent)]
                        } else {
                            sel.into_iter().enumerate().map(|(i, p)| (letters[i].to_string(), p, accent)).collect()
                        }
                    }
                };
                if files.is_empty() {
                    self.core.toasts.info("Open or select an image first");
                    return;
                }
                let ctx = self.core.ctx.clone();
                self.meta_window.show_for(files, tab, &ctx);
            }
            ExportMetadataCsv => {
                let files: Vec<PathBuf> = match self.mode {
                    Mode::Compare => self.compare.as_ref().map(|c| c.info_files(&self.core).into_iter().map(|f| f.1).collect()).unwrap_or_default(),
                    _ => {
                        let sel: Vec<PathBuf> = self.browser.media_targets().into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
                        if sel.len() > 1 { sel } else { self.browser.image_paths() }
                    }
                };
                if files.is_empty() {
                    self.core.toasts.info("No images to export: open a folder of images first");
                    return;
                }
                let Some(dest) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_directory(&self.browser.folder).set_file_name("metadata.csv").save_file() else { return };
                self.file_jobs.submit(format!("Reading metadata of {}", count(files.len(), "image")), metadata_csv_job(files, dest));
            }
            MeasureProject => match &mut self.workbench {
                Some(w) => {
                    w.start_measure(&mut self.core, false);
                    self.mode = Mode::Workbench;
                }
                None => self.core.toasts.info("Open a camera comparison project first (Tools, Workbench)"),
            },
            Workbench => {
                let folder = self.browser.folder.clone();
                let root = lumen_core::workbench::find_root(&folder, 3).unwrap_or(folder);
                if self.workbench.as_ref().is_some_and(|w| w.ws.root == root) {
                    self.mode = Mode::Workbench;
                } else {
                    self.open_project(&root);
                }
            }
            RoundReport => {
                let blind = self.compare.as_ref().is_some_and(|c| c.link.as_ref().is_some_and(|l| l.blind));
                if self.workbench.is_none() {
                    self.core.toasts.info("Open a camera comparison project first (Tools, Workbench)");
                } else if self.mode == Mode::Compare && blind {
                    self.core.toasts.info("Finish the blind test first");
                } else {
                    if self.mode == Mode::Compare {
                        // Leave the scene as Esc does: its review is written first.
                        self.compare = None;
                    }
                    if let Some(w) = &mut self.workbench {
                        w.save_now(&mut self.core.toasts);
                        w.open_export();
                    }
                    self.mode = Mode::Workbench;
                }
            }
            ReportBranding => self.report_branding = Some(Default::default()),
            // Compare's own commands land here only outside Compare.
            PanLeft | PanRight | PanUp | PanDown | ViewSideBySide | ViewSplit | ViewDifference | JumpDivider | ToggleLink | SwapAB | AlignToA | ToggleCameraInfo | LineProfile | ToggleMarkup | ToggleReview | ExportReport | KeepOnly | NextScene
            | PrevScene | ToggleScorecard | CompareHelp | VoteLeft | VoteRight | VoteSame => self.core.toasts.info("This works in Compare: select two to four images, then Compare"),
            CommandPalette => self.palette.toggle(),
            Settings => self.show_settings = !self.show_settings,
            ShortcutsHelp => {
                self.show_help = !self.show_help;
                if self.show_help {
                    // A fresh search, ready to type in.
                    self.help_query.clear();
                    self.help_focus = true;
                }
            }
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
            CopyImageText => {
                let img = match self.mode {
                    Mode::Viewer => self.viewer.current_image(&self.core),
                    _ => None,
                };
                let Some(img) = img else {
                    self.core.toasts.info("Open an image in the viewer first");
                    return;
                };
                self.file_jobs.submit("Reading the text in the image", Box::new(move |_: &JobProgress| {
                    match lumen_core::ocr::recognize(&img, None) {
                        Ok(o) => {
                            let text = o.text();
                            if text.trim().is_empty() {
                                JobResult::Message { ok: false, text: "No text found in this image".into() }
                            } else {
                                JobResult::CopyText(text)
                            }
                        }
                        Err(e) => JobResult::Message { ok: false, text: format!("Could not read text: {e}") },
                    }
                }));
            }
            FocusAddress => {
                if self.mode == Mode::Browser {
                    self.browser.edit_address();
                }
            }
            SelectFlagged => {
                let n = self.browser.select_flagged(&self.core);
                if n == 0 {
                    self.core.toasts.info("No flagged files here. Flag with Q (or the flag button), then select them all at once.");
                }
            }
            InvertSelection => {
                let all: Vec<PathBuf> = self.browser.items.iter().map(|&e| &self.browser.entries[e]).filter(|e| !e.is_folder()).map(|e| e.path.clone()).collect();
                let cur = std::mem::take(&mut self.browser.selected);
                self.browser.selected = all.into_iter().filter(|p| !cur.contains(p)).collect();
            }
            CopyTo | MoveTo => {
                let files = self.targets();
                if !files.is_empty() {
                    self.transfer = Some(TransferDialog { files, do_move: cmd == MoveTo, dest: None, recents: None, typed_dir: None });
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
                self.ask_delete(files);
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
            Share => {
                // Files only: folders cannot be shared this way.
                let files: Vec<PathBuf> = self.targets().into_iter().filter(|p| p.is_file()).collect();
                if files.is_empty() {
                    self.core.toasts.info("Select the files to share");
                    return;
                }
                if self.mode == Mode::Viewer && self.viewer.is_dirty() {
                    self.core.toasts.info("Sharing the file as saved: your edits are not in it");
                }
                if let Err(e) = crate::share::share_files(self.hwnd, &files) {
                    self.core.toasts.error(e);
                }
            }
            CopyPath => {
                let t: Vec<String> = self.targets().iter().map(|p| p.display().to_string()).collect();
                if !t.is_empty() {
                    fileops::copy_text(&t.join("\r\n"));
                    self.core.toasts.success(if t.len() == 1 { "Path copied".to_string() } else { format!("{} paths copied", t.len()) });
                }
            }
            CutFiles => {
                let files = self.targets();
                if files.is_empty() {
                    return;
                }
                match fileops::cut_files_to_clipboard(&files) {
                    Ok(seq) => {
                        self.core.toasts.success(format!("{} cut - paste in another folder to move", count(files.len(), "file")));
                        // Faded in the browser until pasted (or the clipboard changes).
                        self.browser.set_cut(files, seq);
                    }
                    Err(e) => self.core.toasts.error(e.to_string()),
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
                    Ok(clip) if !clip.files.is_empty() => {
                        let dest = self.browser.folder.clone();
                        if clip.cut {
                            // Cut in Explorer or here: move them (those already here stay).
                            let files = crate::droptarget::movable(&clip.files, &dest);
                            if files.is_empty() {
                                self.core.toasts.info("The cut files are already in this folder");
                            } else {
                                self.transfer_files(files, dest, true);
                            }
                        } else {
                            // A copy into the files' own folder is named "photo - Copy.jpg".
                            self.file_jobs.submit(format!("Pasting {}", count(clip.files.len(), "item")), transfer_job(clip.files, dest, false));
                        }
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
                    // Images and videos (videos play, then the show moves on).
                    let sel = self.browser.selected_paths();
                    let media: Vec<PathBuf> = sel.into_iter().filter(|p| crate::formats::is_image_path(p) || crate::video::is_video_path(p)).collect();
                    if media.len() >= 2 {
                        (media, 0)
                    } else {
                        let all = self.browser.media_paths();
                        let start = self.browser.focused_path().and_then(|f| all.iter().position(|p| p == &f)).unwrap_or(0);
                        (all, start)
                    }
                };
                if list.is_empty() {
                    self.core.toasts.info("No images or videos to show");
                    return;
                }
                self.slideshow_from_viewer = self.mode == Mode::Viewer;
                self.slideshow = Some(crate::ui::slideshow::Slideshow::new(list, start, &self.core));
                self.mode = Mode::Slideshow;
                self.set_fullscreen(true);
            }
            Compare => {
                let sel: Vec<PathBuf> = self.browser.selected_paths().into_iter().filter(|p| !p.is_dir()).collect();
                let vids = sel.iter().filter(|p| crate::video::is_video_path(p)).count();
                if vids >= 2 {
                    self.open_video_mode(sel);
                    return;
                }
                let imgs: Vec<PathBuf> = sel.into_iter().filter(|p| crate::formats::is_image_path(p)).collect();
                // In the viewer: the image on screen and the next one (videos are passed over).
                let next_image = |from: usize, step: isize| {
                    let list = &self.viewer.list;
                    let mut i = from as isize + step;
                    while i >= 0 && (i as usize) < list.len() {
                        if crate::formats::is_image_path(&list[i as usize]) {
                            return Some(i as usize);
                        }
                        i += step;
                    }
                    None
                };
                let cur = self.viewer.index;
                let pair = (self.viewer.current().is_some_and(|p| crate::formats::is_image_path(p)))
                    .then(|| next_image(cur, 1).or_else(|| next_image(cur, -1)))
                    .flatten();
                let imgs = if imgs.len() >= 2 {
                    imgs
                } else if let (Mode::Viewer, Some(j)) = (self.mode, pair) {
                    vec![self.viewer.list[cur].clone(), self.viewer.list[j].clone()]
                } else {
                    self.core.toasts.info("Select 2-4 images (or 2 videos) to compare");
                    return;
                };
                if imgs.len() > 4 {
                    self.core.toasts.info(format!("Comparing the first 4 of {}", imgs.len()));
                }
                self.compare = Some(crate::ui::compare::Compare::new(imgs.into_iter().take(4).collect(), &self.core));
                self.mode = Mode::Compare;
            }
            ToggleFlag => {
                let t = self.media_targets();
                let all_flagged = t.iter().all(|p| self.core.tags.get(p).flagged);
                self.change_tags(&t, cmd, !all_flagged, |tag| tag.flagged = !all_flagged);
                self.schedule_advance();
                if self.browser.flagged_only || self.core.settings.sort_key == crate::settings::SortKey::Flagged {
                    self.browser.mark_resort();
                }
            }
            Rate(n) => {
                let t = self.media_targets();
                self.change_tags(&t, cmd, false, |tag| tag.rating = n);
                self.schedule_advance();
                if self.core.settings.sort_key == crate::settings::SortKey::Rating || self.browser.min_rating > 0 {
                    self.browser.mark_resort();
                }
            }
            Label(n) => {
                let t = self.media_targets();
                self.change_tags(&t, cmd, false, |tag| tag.label = n);
                self.schedule_advance();
                if self.core.settings.sort_key == crate::settings::SortKey::ColorLabel || self.browser.label_filter > 0 {
                    self.browser.mark_resort();
                }
            }
            ShowFlaggedOnly => {
                self.browser.flagged_only = !self.browser.flagged_only;
                self.browser.mark_resort();
            }
            OpenVideoMode => {
                // The viewer's video goes on in video mode; else the selected videos.
                let vids: Vec<PathBuf> = match self.viewer.current() {
                    Some(p) if self.mode == Mode::Viewer && crate::video::is_video_path(p) => vec![p.clone()],
                    _ => self.browser.selected_paths().into_iter().filter(|p| crate::video::is_video_path(p)).collect(),
                };
                self.open_video_mode(vids);
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
            NextImage | PrevImage => {
                let d: isize = if cmd == NextImage { 1 } else { -1 };
                let at_end = if d > 0 { self.viewer.index + 1 >= self.viewer.list.len() } else { self.viewer.index == 0 };
                if wrap || !at_end || self.mode != Mode::Viewer {
                    self.end_notice = None;
                    self.viewer.step(d, wrap, fit);
                } else if self.end_notice.is_some_and(|(t, dir)| dir == d && t.elapsed() < std::time::Duration::from_millis(2000)) {
                    self.end_notice = None;
                    self.open_sibling_folder(d);
                } else {
                    self.end_notice = Some((Instant::now(), d));
                    let (which, key) = if d > 0 { ("Last", "Right") } else { ("First", "Left") };
                    self.core.toasts.info(format!("{which} image in this folder. Press {key} again for the {} folder.", if d > 0 { "next" } else { "previous" }));
                }
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
                // The viewer's image as shown (edits too), else the file's first frame.
                let img = if self.mode == Mode::Viewer {
                    let Some(img) = self.viewer.current_image(&self.core) else {
                        self.image_unavailable();
                        return;
                    };
                    Some(img)
                } else {
                    None
                };
                let Some(p) = self.targets().first().cloned() else { return };
                // Decoding and the clipboard's PNG copy take seconds for a big
                // photo: off the UI thread (the clipboard has no owner window,
                // so any thread may fill it).
                self.file_jobs.submit("Copying the image", copy_image_job(img, p));
            }
            PasteImage => match fileops::paste_image_from_clipboard() {
                Ok(img) => {
                    if self.mode == Mode::Viewer && self.viewer.current().is_some() {
                        // A new file next to the photo, never an edit of it: a later
                        // Ctrl+S must not overwrite the original with the clipboard.
                        let dir = self.viewer.current().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| self.browser.folder.clone());
                        let name = chrono::Local::now().format("Pasted %Y-%m-%d %H%M%S.png").to_string();
                        let path = fileops::unique_path(&dir.join(name));
                        let opts = SaveOptions { format: crate::formats::SaveFormat::Png, ..Default::default() };
                        match crate::encode::save(&img, &path, &opts) {
                            Ok(()) => {
                                self.core.toasts.success(format!("Clipboard image saved as {}", crate::ui::viewer::file_name(&path)));
                                if dir == self.browser.folder {
                                    self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                                }
                                self.open_file(&path);
                            }
                            Err(e) => self.core.toasts.error(e.to_string()),
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
            ToggleInfo => self.viewer.toggle_info(),
            ToggleHistogram => self.viewer.show_histogram = !self.viewer.show_histogram,
            ToggleMagnifier => self.viewer.magnifier_locked = !self.viewer.magnifier_locked,
            TogglePixelValues => self.viewer.show_pixel = !self.viewer.show_pixel,
            MoveEarlier | MoveLater => {
                if self.mode == Mode::Browser {
                    let step = if cmd == MoveEarlier { -1 } else { 1 };
                    self.browser.nudge(&mut self.core, step);
                }
            }
            NextPane | PrevPane => {
                if self.mode == Mode::Browser {
                    let ctx = self.core.ctx.clone();
                    self.browser.cycle_focus(&ctx, cmd == NextPane);
                }
            }
            CycleInspect => {
                self.viewer.cycle_inspect();
                self.core.toasts.info(format!("Inspect view: {}. {}", self.viewer.inspect.label(), self.viewer.inspect.tip()));
            }
            ToggleFilmstrip => self.viewer.show_filmstrip = !self.viewer.show_filmstrip,
            PlayPause => self.viewer.toggle_animation(),
            NextFrame => self.viewer.step_frame(&self.core, 1),
            PrevFrame => self.viewer.step_frame(&self.core, -1),
            // Without a video on screen (a video takes these above).
            SeekBack | SeekForward => {}
            ToggleMute => {
                let v = &mut self.core.settings.video;
                v.muted = !v.muted;
                self.core.toasts.info(if v.muted { "Video sound off" } else { "Video sound on" });
            }
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
                if self.mode == Mode::Compare && self.compare.as_ref().is_some_and(|c| c.link.as_ref().is_some_and(|l| l.blind)) {
                    self.core.toasts.info("Analysis is off during a blind test (it would show the file names)");
                    return;
                }
                // In the browser: open the focused image or video, with the panel.
                let from_browser = self.mode == Mode::Browser;
                if let Some(i) = self.browser.focus.filter(|&i| from_browser && self.browser.item(i).is_some_and(|e| e.kind != EntryKind::Folder)) {
                    match self.browser.item(i).filter(|e| e.kind == EntryKind::Video).map(|e| e.path.clone()) {
                        // A video is analyzed in video mode (frame metrics, timing).
                        Some(p) => {
                            let vids = self.selected_videos_with(&p);
                            self.open_video_mode(if vids.len() >= 2 { vids } else { vec![p] });
                        }
                        None => self.open_viewer_at(i),
                    }
                }
                // The viewer's video is analyzed in video mode too.
                let viewer_video = self.mode == Mode::Viewer && self.viewer.showing_video();
                if let Some(p) = self.viewer.current().cloned().filter(|_| viewer_video) {
                    self.open_video_mode(vec![p]);
                }
                if !matches!(self.mode, Mode::Viewer | Mode::Compare | Mode::Video) {
                    self.core.toasts.info(if from_browser { "Select an image or a video to analyze" } else { "The analysis panel works on an open image, a comparison or a video" });
                    return;
                }
                match cmd {
                    ToggleAnalysis if from_browser || viewer_video => self.analysis.open = true,
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
                                c.zoom_to_region(slot.index(), rect, ppp);
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
                PanelAction::AddMarks(marks) => {
                    let n = match (&mut self.compare, self.mode) {
                        (Some(c), Mode::Compare) => c.add_marks(marks),
                        _ => 0,
                    };
                    if n > 0 {
                        self.core.toasts.success(format!("Added {n} mark{} (Review panel lists them)", if n == 1 { "" } else { "s" }));
                    } else {
                        self.core.toasts.info("No marks added: they are there already, or marks are off here");
                    }
                }
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
        if self.mode == Mode::Viewer && self.viewer.showing_video() {
            self.core.toasts.info("This works on images, not on videos");
            return;
        }
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
        // Viewer, unedited JPEG: turned losslessly on disk (the EXIF
        // orientation, or the picture's blocks when it has no orientation
        // tag), off the UI thread; the viewer shows the file again when done.
        if self.core.settings.lossless_jpeg_rotate && !self.viewer.is_dirty() {
            if let Some(p) = self.viewer.current().cloned().filter(|p| crate::formats::is_jpeg_ext(&crate::formats::ext_of(p))) {
                self.file_jobs.submit(t.label(), rotate_job(vec![p], Vec::new(), t, self.core.settings.jpeg_quality));
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

    /// Crop the JPEG `path` on disk without re-encoding it (undoable from
    /// its previous bytes); `rect` is in its displayed pixels.
    fn lossless_crop(&mut self, path: PathBuf, rect: (u32, u32, u32, u32)) {
        self.file_jobs.submit("Cropping losslessly", lossless_crop_job(path, rect));
    }

    /// The viewer shows one of `files`, changed on disk: drop its edit
    /// session (it has no unsaved edits) so the new file shows, fitted.
    fn show_changed(&mut self, files: &[&Path]) {
        let shown = self.viewer.current().is_some_and(|c| files.contains(&c.as_path()));
        if shown && !self.viewer.is_dirty() {
            self.viewer.edit = None;
            self.viewer.reset_view(self.core.settings.fit_mode);
        }
    }

    fn save(&mut self, save_as: bool) {
        let Some(path) = self.viewer.current().cloned().filter(|_| self.mode == Mode::Viewer) else {
            self.core.toasts.info("Open an image in the viewer to save it");
            return;
        };
        if let Some(s) = &self.saving {
            self.core.toasts.info(format!("Still saving {}", crate::ui::viewer::file_name(&s.path)));
            return;
        }
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
        self.do_save(&path, img, &opts, None, None);
    }

    /// Save `img` to `path` (see `write_save`), first asking when that
    /// replaces a change made on disk to the open file.
    fn do_save(&mut self, path: &Path, img: Arc<image::RgbaImage>, opts: &SaveOptions, then: Option<PendingAction>, dialog: Option<SaveDialog>) {
        // Saving over the open file: when it changed on disk after it was loaded
        // (edited in another program), ask before replacing that change.
        if self.viewer.current().is_some_and(|c| fileops::same_path(c, path)) {
            // (Only when the load recorded the file's time: otherwise nothing to compare.)
            let loaded = self.viewer.loaded(&self.core).filter(|l| l.modified.is_some());
            let changed = loaded.is_some_and(|l| fileops::FileStamp::of(path).is_some_and(|now| !now.matches(l.file_size, l.modified)));
            if changed {
                // From Save as: the question replaces the dialog. From the
                // unsaved-edits question: its action waits for the answer.
                drop(dialog);
                self.confirm = Some(Confirm::SaveChanged { path: path.to_path_buf(), opts: opts.clone(), then });
                return;
            }
        }
        self.write_save(path, img, opts, then, dialog);
    }

    /// Save `img` to `path` (no questions asked) on the file job thread:
    /// encoding a big PNG or AVIF takes seconds. `finish_save` refreshes what
    /// shows the file and then runs `then`; Save as's `dialog` comes back if
    /// the save fails.
    fn write_save(&mut self, path: &Path, img: Arc<image::RgbaImage>, opts: &SaveOptions, then: Option<PendingAction>, dialog: Option<SaveDialog>) {
        if let Some(s) = &self.saving {
            self.core.toasts.info(format!("Still saving {} - save again when it is done", crate::ui::viewer::file_name(&s.path)));
            // Nothing is lost: the question or the dialog stays.
            if self.pending_guard.is_none() {
                self.pending_guard = then;
            }
            if self.save_dialog.is_none() {
                self.save_dialog = dialog;
            }
            return;
        }
        // The open file is the source of the comment / XMP / IPTC carried into a JPEG.
        let source = self.viewer.current().cloned();
        let label = format!("Saving {}", crate::ui::viewer::file_name(path));
        if self.file_jobs.submit(label.clone(), save_job(img.clone(), path.to_path_buf(), opts.clone(), source.clone())) {
            self.saving = Some(Saving { label, path: path.to_path_buf(), img, source, then, dialog });
        } else {
            self.core.toasts.error("Save failed: the background worker is not running");
        }
    }

    /// The save started by `write_save` is done (`res`): the edit session is
    /// saved as far as it was then, the views show the new file, and what
    /// waited for it goes on (or, after a failure, asks again).
    fn finish_save(&mut self, res: Result<(), String>) {
        let Some(s) = self.saving.take() else { return };
        let path = s.path.as_path();
        let shows_source = self.viewer.current().is_some_and(|c| s.source.as_ref() == Some(c));
        if let Err(e) = res {
            self.core.toasts.error(format!("Save failed: {e}"));
            // Save as: the dialog comes back as it was, while the viewer still shows that image.
            if let Some(mut d) = s.dialog.filter(|_| shows_source && !self.modal_open()) {
                d.error = Some("Could not save - see the message below.".into());
                self.save_dialog = Some(d);
            }
            // The edits are not saved: what waited asks again.
            if self.pending_guard.is_none() {
                self.pending_guard = s.then;
            }
            return;
        }
        // The image written, not edits made while it was written.
        if let Some(e) = self.viewer.edit.as_mut().filter(|e| fileops::same_path(&e.path, path)) {
            e.mark_saved_image(&s.img);
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
        if let Some(d) = &s.dialog {
            self.core.settings.jpeg_quality = d.opts.jpeg_quality;
            // Saved to a new file: the viewer goes on with the saved copy (as
            // saved, no edits); the original stays as it is on disk. Not when
            // the image was changed or left while saving (nothing is dropped).
            let unchanged = shows_source && !self.viewer.has_tool() && self.viewer.current_image(&self.core).is_some_and(|i| Arc::ptr_eq(&i, &s.img));
            if self.mode == Mode::Viewer && unchanged && !self.viewer.current().is_some_and(|c| fileops::same_path(c, path)) {
                self.viewer.show_saved_copy(path);
            }
        }
        if let Some(a) = s.then {
            // Edits made while saving are not saved: asked about first.
            if self.viewer.has_unsaved_work() {
                self.pending_guard = Some(a);
            } else {
                self.perform_pending(a);
            }
        }
    }

    /// A running save writes the open file as it is shown now, with no tool
    /// work on top: once it is done, nothing would be lost by leaving it.
    fn save_covers_current(&self) -> bool {
        let Some(s) = &self.saving else { return false };
        self.viewer.current().is_some_and(|c| fileops::same_path(c, &s.path)) && !self.viewer.tool_has_work() && self.viewer.current_image(&self.core).is_some_and(|i| Arc::ptr_eq(&i, &s.img))
    }

    /// "Wait until photo.jpg is saved" when a running save writes one of
    /// `files` (or a file inside one of them).
    fn wait_for_save(&self, files: &[PathBuf]) -> Option<String> {
        let s = self.saving.as_ref()?;
        files.iter().any(|f| fileops::is_within(&s.path, f)).then(|| format!("Wait until {} is saved", crate::ui::viewer::file_name(&s.path)))
    }

    fn set_wallpaper(&mut self) {
        let img = if self.mode == Mode::Viewer { self.viewer.current_image(&self.core) } else { None };
        let Some(path) = self.targets().first().cloned() else { return };
        // A JPEG, BMP or PNG file as saved is used as it is; anything else
        // (or the viewer's edited image) is written as a JPEG first.
        let edited = self.mode == Mode::Viewer && self.viewer.is_dirty();
        let as_is = !edited && matches!(crate::formats::ext_of(&path).as_str(), "jpg" | "jpeg" | "bmp" | "png");
        // Decoding, encoding and Windows taking the picture take a while: off the UI thread.
        self.file_jobs.submit("Setting the wallpaper", wallpaper_job(path, as_is, img));
    }

    /// Delete, asking first when the setting says so, and always when the files
    /// cannot go to the Recycle Bin (network and removable drives).
    fn ask_delete(&mut self, files: Vec<PathBuf>) {
        let permanent = fileops::is_permanent_delete(&files);
        if self.core.settings.confirm_delete || permanent {
            self.confirm = Some(Confirm::Delete { files, permanent });
        } else {
            self.delete_files(files);
        }
    }

    fn delete_files(&mut self, files: Vec<PathBuf>) {
        // The save would bring the file back.
        if let Some(msg) = self.wait_for_save(&files) {
            self.core.toasts.info(msg);
            return;
        }
        // For undo: when they went, their tags and their places in the viewer.
        let at = std::time::SystemTime::now();
        let kept: Vec<DeletedItem> = files.iter().map(|f| DeletedItem { path: f.clone(), tag: self.core.tags.get(f), viewer_at: self.viewer.list.iter().position(|p| p == f) }).collect();
        match fileops::recycle(&files) {
            Ok(outcome) => {
                for f in &files {
                    self.core.tags.removed(f);
                    self.core.loader.invalidate(f);
                    self.core.thumbs.forget(f);
                    self.viewer.forget_stat(f);
                    self.viewer.remove_path(f);
                    self.browser.selected.remove(f);
                }
                match outcome {
                    fileops::TrashOutcome::Recycled => {
                        let key = Command::Undo.shortcut();
                        let undo = if key.is_empty() { "Undo".to_string() } else { key };
                        self.core.toasts.success(format!("Moved {} to the Recycle Bin - {undo} to undo", count(files.len(), "item")));
                        self.push_undo(FileUndo { label: format!("Delete {}", count(files.len(), "item")), change: FileChange::Delete { files: kept, at } });
                    }
                    fileops::TrashOutcome::DeletedPermanently => {
                        self.core.toasts.success(format!("Deleted {} (network or removable drive: no Recycle Bin)", count(files.len(), "item")));
                    }
                }
                if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
                    self.mode = Mode::Browser;
                }
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
            }
            Err(e) => self.core.toasts.error(e.to_string()),
        }
    }

    /// "Moved 3 files to Holiday (Ctrl+Z to undo)", and the first error.
    fn report_transfer(&mut self, n: usize, folders: usize, errs: Vec<String>, dest: &Path, moved: bool) {
        if !errs.is_empty() {
            self.core.toasts.error(format!("{} failed: {}", errs.len(), errs.first().cloned().unwrap_or_default()));
        }
        if n > 0 {
            let what = match (n.saturating_sub(folders), folders) {
                (files, 0) => count(files, "file"),
                (0, dirs) => count(dirs, "folder"),
                _ => count(n, "item"),
            };
            let to = dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| dest.display().to_string());
            let undo = Command::Undo.shortcut();
            self.core.toasts.success(match (moved, undo.is_empty()) {
                (true, false) => format!("Moved {what} to {to} ({undo} to undo)"),
                (true, true) => format!("Moved {what} to {to}"),
                (false, _) => format!("Copied {what} to {to}"),
            });
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
            JobResult::Transfer { done, errs, dest, do_move, folders } => self.finish_transfer(done, errs, dest, do_move, folders),
            JobResult::Rotate { t, lossless, reencoded, skipped, errs, trimmed, gain_maps } => {
                for f in lossless.iter().chain(&reencoded) {
                    self.forget_file(&f.path);
                }
                let changed: Vec<&Path> = lossless.iter().chain(&reencoded).map(|f| f.path.as_path()).collect();
                self.show_changed(&changed);
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
                    let undoable = reencoded.iter().all(|f| f.before.is_some());
                    let note = if undoable { "" } else { " (too large to undo)" };
                    // Turning the picture itself keeps whole blocks only.
                    let cut = match trimmed.as_slice() {
                        [] => String::new(),
                        [(w, h)] if ok == 1 => format!(", {} trimmed off the edge to keep it lossless", px_size(*w, *h)),
                        many => format!(", edges trimmed by a few pixels on {} to keep them lossless", count(many.len(), "file")),
                    };
                    self.core.toasts.info(format!("{}{how}{}{note}{cut}", t.label(), if ok > 1 { format!(" ({ok} files)") } else { String::new() }));
                    if gain_maps > 0 {
                        self.core.toasts.info(gain_map_note(gain_maps));
                    }
                    if undoable {
                        let files: Vec<Transformed> = lossless.into_iter().chain(reencoded).collect();
                        self.push_undo(FileUndo { label: t.label().to_string(), change: FileChange::Transform { t: Some(t), files } });
                    } else {
                        // Ctrl+Z must not skip this change and undo an older one instead.
                        self.file_undo.clear();
                    }
                }
            }
            JobResult::Restore { files, res } => {
                self.undoing = None;
                self.finish_restore(files, res);
            }
            JobResult::UndoMoves { label, total, done, errs } => {
                self.undoing = None;
                self.finish_undo_moves(label, total, done, errs);
            }
            JobResult::UndoTransform { label, files, ok, restored, errs } => {
                self.undoing = None;
                self.finish_undo_transform(label, files, ok, restored, errs);
            }
            JobResult::Cropped { file, size, gain_map_removed } => {
                self.forget_file(&file.path);
                self.show_changed(&[file.path.as_path()]);
                let text = format!("Cropped losslessly to {} × {}", size.0, size.1);
                if gain_map_removed {
                    self.core.toasts.info(gain_map_note(1));
                }
                if file.before.is_some() {
                    self.core.toasts.success(text);
                    self.push_undo(FileUndo { label: "Lossless crop".to_string(), change: FileChange::Transform { t: None, files: vec![file] } });
                } else {
                    self.core.toasts.info(format!("{text} (too large to undo)"));
                    // Ctrl+Z must not skip this change and undo an older one instead.
                    self.file_undo.clear();
                }
            }
            JobResult::Saved(res) => self.finish_save(res),
            JobResult::Panicked(label) => {
                if self.saving.as_ref().is_some_and(|s| s.label == label) {
                    self.finish_save(Err("it stopped unexpectedly".into()));
                    return;
                }
                if self.undoing.as_ref() == Some(&label) {
                    self.undoing = None;
                }
                self.core.toasts.error(format!("{label} stopped unexpectedly; check the folder"));
            }
            JobResult::Message { ok: true, text } => self.core.toasts.success(text),
            JobResult::Message { ok: false, text } => self.core.toasts.error(text),
            JobResult::CopyText(text) => {
                let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
                self.core.ctx.copy_text(text);
                self.core.toasts.success(format!("Copied {} of text", count(lines, "line")));
            }
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

    fn finish_transfer(&mut self, done: Vec<(PathBuf, PathBuf)>, errs: Vec<String>, dest: PathBuf, do_move: bool, folders: usize) {
        if do_move && !done.is_empty() {
            let label = format!("Move {} to {}", count(done.len(), "item"), dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dest.display().to_string()));
            self.push_file_undo(label, done.clone());
            // Cut files that were pasted (or moved some other way) leave the
            // clipboard, as in Explorer.
            if errs.is_empty() {
                let moved: Vec<PathBuf> = done.iter().map(|(from, _)| from.clone()).collect();
                fileops::clear_cut_after_move(&moved);
            }
        }
        // Pasted or dropped into the folder shown: select what arrived, as Explorer does.
        if !done.is_empty() && fileops::same_path(&dest, &self.browser.folder) {
            self.browser.pending_select = done.first().map(|(_, to)| to.clone());
            self.browser.pending_selection = Some(done.iter().map(|(_, to)| to.clone()).collect());
        }
        if do_move {
            self.core.tags.moved_many(&done);
        }
        // What `forget_file` does for each file, with one pass over the
        // thumbnail cache and the viewer list for all of them (a move can be
        // thousands of files).
        let mut touched: Vec<&Path> = Vec::with_capacity(done.len() * 2);
        for (from, to) in &done {
            if do_move {
                touched.push(from);
            } else {
                self.core.tags.copied(from, to);
            }
            touched.push(to);
        }
        for p in &touched {
            self.core.loader.invalidate(p);
            self.viewer.forget_stat(p);
            self.core.meta.forget(p);
        }
        self.core.thumbs.forget_many(&touched.iter().copied().collect());
        if do_move {
            let gone: std::collections::HashSet<&Path> = done.iter().map(|(from, _)| from.as_path()).collect();
            if let Some(last) = self.viewer.list.iter().find(|p| gone.contains(p.as_path())).cloned() {
                // All but one in one pass; `remove_path` for the last also closes
                // the edit session and resets the view, as it did per file.
                let mut rest = gone.clone();
                rest.remove(last.as_path());
                self.viewer.index = fileops::remove_listed(&mut self.viewer.list, self.viewer.index, &rest);
                self.viewer.remove_path(&last);
            }
        }
        self.report_transfer(done.len(), folders, errs, &dest, do_move);
        self.browser.tree.invalidate(&dest);
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if self.mode == Mode::Viewer && self.viewer.list.is_empty() {
            self.mode = Mode::Browser;
        }
    }

    /// Deleted files are back from the Recycle Bin: show them again (selected
    /// in the browser, at their old places in the viewer) with their tags.
    fn finish_restore(&mut self, files: Vec<DeletedItem>, res: crate::recycle::Restored) {
        let mut select: Vec<PathBuf> = Vec::new();
        let mut folders: Vec<PathBuf> = Vec::new();
        // (place in the viewer's list, path now)
        let mut back: Vec<(usize, PathBuf)> = Vec::new();
        for (orig, now) in &res.done {
            self.forget_file(orig);
            self.forget_file(now);
            if let Some(f) = files.iter().find(|f| &f.path == orig) {
                let tag = f.tag;
                if !tag.is_empty() {
                    self.core.tags.update(now, |t| *t = tag);
                }
                if let Some(at) = f.viewer_at {
                    back.push((at, now.clone()));
                }
            }
            if let Some(dir) = now.parent() {
                if !folders.iter().any(|f| f == dir) {
                    folders.push(dir.to_path_buf());
                }
                if dir == self.browser.folder.as_path() {
                    select.push(now.clone());
                }
            }
        }
        // Back at their places in the viewer, when it still shows that folder.
        back.sort_by_key(|(at, _)| *at);
        let in_viewer = !back.is_empty() && back.iter().all(|(_, p)| self.viewer.list.iter().any(|q| q.parent() == p.parent()));
        if in_viewer {
            for (at, p) in &back {
                if self.viewer.list.contains(p) {
                    continue;
                }
                let at = (*at).min(self.viewer.list.len());
                if at <= self.viewer.index {
                    self.viewer.index += 1;
                }
                self.viewer.list.insert(at, p.clone());
            }
            if self.mode == Mode::Viewer && !self.viewer.has_unsaved_work() {
                if let Some(i) = self.viewer.list.iter().position(|q| q == &back[0].1) {
                    self.viewer.go_to(i, self.core.settings.fit_mode);
                }
            }
        }
        for f in &folders {
            self.browser.tree.invalidate(f);
        }
        if !select.is_empty() {
            self.browser.selected = select.iter().cloned().collect();
            self.browser.pending_select = select.first().cloned();
        }
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        let name = crate::ui::viewer::file_name;
        let n = res.done.len();
        if n > 0 {
            self.core.toasts.success(format!("Restored {} from the Recycle Bin", count(n, "item")));
        }
        let renamed: Vec<&(PathBuf, PathBuf)> = res.done.iter().filter(|(orig, now)| orig != now).collect();
        match renamed.as_slice() {
            [] => {}
            [(orig, now)] => self.core.toasts.info(format!("The name \"{}\" is taken, so it came back as \"{}\"", name(orig), name(now))),
            many => self.core.toasts.info(format!("{} names are taken, so those items came back with a number added", many.len())),
        }
        match res.failed.as_slice() {
            [] => {}
            [(p, why)] => self.core.toasts.error(format!("Could not restore \"{}\": {why}", name(p))),
            all => {
                let names: Vec<String> = all.iter().take(3).map(|(p, _)| name(p)).collect();
                let more = if all.len() > 3 { ", ..." } else { "" };
                self.core.toasts.error(format!("Could not restore {} of {}: {}{more} ({})", all.len(), count(all.len() + n, "item"), names.join(", "), all[0].1));
            }
        }
    }

    /// Moved or renamed items are back where they were (undo of `label`;
    /// `done`: each as (where it was, where it is again)): tags, arranged
    /// orders and the views follow, and the ones back in the folder shown
    /// are selected.
    fn finish_undo_moves(&mut self, label: String, total: usize, done: Vec<(PathBuf, PathBuf)>, errs: Vec<(String, String)>) {
        self.core.tags.moved_many(&done);
        self.core.orders.rename_many(&done);
        // What `forget_file` does for each item, with one pass over the
        // thumbnail cache for all of them (an undo can be thousands of files).
        let touched: std::collections::HashSet<&Path> = done.iter().flat_map(|(from, to)| [from.as_path(), to.as_path()]).collect();
        for p in &touched {
            self.core.loader.invalidate(p);
            self.viewer.forget_stat(p);
            self.core.meta.forget(p);
        }
        self.core.thumbs.forget_many(&touched);
        self.viewer.replace_paths(&done);
        let mut folders: Vec<&Path> = Vec::new();
        for d in done.iter().flat_map(|(from, to)| [to.parent(), from.parent()]).flatten() {
            if !folders.contains(&d) {
                folders.push(d);
            }
        }
        for f in folders {
            self.browser.tree.invalidate(f);
        }
        let here: Vec<PathBuf> = done.iter().map(|(_, to)| to).filter(|to| to.parent().is_some_and(|d| fileops::same_path(d, &self.browser.folder))).cloned().collect();
        if !here.is_empty() {
            self.browser.pending_select = here.first().cloned();
            self.browser.pending_selection = Some(here.into_iter().collect());
        }
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if let Some(msg) = undo_failures(&errs, total) {
            self.core.toasts.error(msg);
        }
        if !done.is_empty() {
            self.core.toasts.success(format!("Undone: {label}"));
        }
    }

    /// Rotated / flipped files are turned back (undo of `label`; `ok` of
    /// `files` restored, `restored`: those with their stamp before the
    /// change and their stamp now).
    fn finish_undo_transform(&mut self, label: String, files: Vec<PathBuf>, ok: usize, restored: Vec<(PathBuf, fileops::FileStamp, Option<fileops::FileStamp>)>, errs: Vec<(String, String)>) {
        for p in &files {
            self.forget_file(p);
        }
        // An older rotation of the same file expects it as this change
        // found it: it is that again, with a new time.
        for older in self.file_undo.iter_mut() {
            if let FileChange::Transform { files: older_files, .. } = &mut older.change {
                for f in older_files.iter_mut() {
                    if let Some((_, _, now)) = restored.iter().find(|(p, was, _)| *p == f.path && f.after == Some(*was)) {
                        f.after = *now;
                    }
                }
            }
        }
        // The viewer shows the restored file (unless it holds edits of its own).
        let changed: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        self.show_changed(&changed);
        self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
        if let Some(msg) = undo_failures(&errs, files.len()) {
            self.core.toasts.error(msg);
        }
        if ok > 0 {
            self.core.toasts.success(format!("Undone: {label}"));
        }
    }

    fn push_file_undo(&mut self, label: String, moves: Vec<(PathBuf, PathBuf)>) {
        self.push_undo(FileUndo { label, change: FileChange::Moves(moves) });
    }

    /// Rate, flag or label `files` (the `cmd` the keys, menus and tag rows run;
    /// `flag_on`: what ToggleFlag sets), keeping their tags before for Ctrl+Z.
    fn change_tags(&mut self, files: &[PathBuf], cmd: Command, flag_on: bool, f: impl Fn(&mut crate::tags::Tag)) {
        if files.is_empty() {
            return;
        }
        let before: Vec<(PathBuf, crate::tags::Tag)> = files.iter().map(|p| (p.clone(), self.core.tags.get(p))).collect();
        for p in files {
            self.core.tags.update(p, &f);
        }
        self.record_tags(before, tag_notice(cmd, flag_on, files.len()));
    }

    /// Put a tag change on the undo history (only the files it changed) and
    /// show `notice` with an Undo button.
    fn record_tags(&mut self, before: Vec<(PathBuf, crate::tags::Tag)>, notice: String) {
        let before: Vec<(PathBuf, crate::tags::Tag)> = before.into_iter().filter(|(p, t)| self.core.tags.get(p) != *t).collect();
        if before.is_empty() {
            // Nothing changed (already rated so): say so without an Undo.
            self.core.toasts.info(notice);
            return;
        }
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.push_undo(FileUndo { label: notice.clone(), change: FileChange::Tags { id, before } });
        let tip = crate::ui::tips::with_shortcut("Put the tags back as they were", &Command::Undo.shortcut());
        self.core.toasts.with_action(notice, "Undo", &tip, id);
    }

    /// A notice's Undo button: undo that tag change when it is still the last
    /// thing done (an older one would undo something newer first).
    fn undo_tags_by_id(&mut self, id: u64) {
        match self.file_undo.last() {
            Some(FileUndo { change: FileChange::Tags { id: last, .. }, .. }) if *last == id => self.undo_file_op(),
            _ => self.core.toasts.info(format!("Something else was done since. Press {} to undo step by step.", Command::Undo.shortcut())),
        }
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

    /// First start: the few choices people coming from FastStone look for first.
    fn welcome(&mut self, ctx: &egui::Context) {
        use crate::settings::WheelAction;
        use crate::ui::keymap::Preset;
        let p = self.core.pal;
        let mut done = false;
        let s = &mut self.core.settings;
        crate::ui::widgets::modal(ctx, egui::Id::new("welcome"), 460.0, |ui| {
            crate::ui::widgets::modal_title(ui, "Welcome to Lumen");
            ui.label(RichText::new("Two choices to start with; both can be changed any time in Settings (Ctrl+,).").color(p.muted));
            ui.add_space(10.0);
            ui.label(RichText::new("Keyboard shortcuts").strong());
            let preset = Preset::from_id(&s.keymap_preset);
            for pr in Preset::ALL {
                let tip = match pr {
                    Preset::Lumen => "Lumen's own keys (F1 lists them all)",
                    Preset::FastStone => "The keys you know from FastStone Image Viewer",
                };
                if crate::ui::widgets::radio(ui, preset == pr, pr.label()).on_hover_text(tip).clicked() {
                    s.keymap_preset = pr.id().to_string();
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Mouse wheel in the viewer").strong());
            crate::ui::widgets::radio_value(ui, &mut s.wheel_action, WheelAction::Navigate, "Next / previous image (Ctrl + wheel zooms)");
            crate::ui::widgets::radio_value(ui, &mut s.wheel_action, WheelAction::Zoom, "Zoom");
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if crate::ui::widgets::primary_button(ui, &p, "Start").clicked() {
                    done = true;
                }
                ui.label(RichText::new("Tip: Ctrl+K finds any command by name.").color(p.muted).size(theme::text::SMALL));
            });
        });
        if done {
            self.core.settings.first_run_done = true;
            crate::ui::keymap::install(&self.core.settings);
            self.core.settings.save();
        }
    }

    /// "Next image after rating" (viewer only): step on after a short pause.
    fn schedule_advance(&mut self) {
        if self.core.settings.advance_after_tag && self.mode == Mode::Viewer {
            self.advance_at = Some(Instant::now() + std::time::Duration::from_millis(350));
            self.core.ctx.request_repaint_after(std::time::Duration::from_millis(360));
        }
    }

    /// From the viewer at the end of a folder: open the first (or, going back,
    /// the last) picture of the next (`d` > 0) or previous sibling folder that
    /// has pictures (folders in natural name order, the picture in the order
    /// the browser shows that folder in). The folders are listed on a worker
    /// thread (a network drive answers slowly), then the picture is picked on
    /// another (file dates and capture dates may have to be read);
    /// `poll_sibling_folder` opens it.
    fn open_sibling_folder(&mut self, d: isize) {
        let Some(cur) = self.viewer.current().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (from, ctx) = (cur.clone(), self.core.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(sibling_folder_pictures(&from, d));
            ctx.request_repaint();
        });
        self.sibling_pick = None;
        self.sibling_lookup = Some((cur, d, rx));
    }

    /// True while the viewer still shows a picture of `folder`.
    fn viewer_in(&self, folder: &Path) -> bool {
        self.mode == Mode::Viewer && self.viewer.current().and_then(|p| p.parent()).is_some_and(|p| fileops::same_path(p, folder))
    }

    /// Carry on what `open_sibling_folder` started, if the viewer is still on
    /// the folder it was asked from.
    fn poll_sibling_folder(&mut self) {
        if let Some((from, dir, by_name, rx)) = &self.sibling_pick {
            let pick = match rx.try_recv() {
                Ok(pick) => pick,
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => None,
            };
            let (from, dir) = (from.clone(), dir.clone());
            let pic = pick.unwrap_or_else(|| by_name.clone());
            self.sibling_pick = None;
            if self.viewer_in(&from) {
                self.core.toasts.info(format!("Folder: {}", crate::ui::viewer::file_name(&dir)));
                self.open_file(&pic);
            }
            return;
        }
        let Some((from, d, rx)) = &self.sibling_lookup else { return };
        let found = match rx.try_recv() {
            Ok(found) => found,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => None,
        };
        let (from, d) = (from.clone(), *d);
        self.sibling_lookup = None;
        if !self.viewer_in(&from) {
            return;
        }
        let Some((dir, pics)) = found else {
            self.core.toasts.info(if d > 0 { "No later folder with pictures" } else { "No earlier folder with pictures" });
            return;
        };
        let Some(by_name) = (if d > 0 { pics.first() } else { pics.last() }).cloned() else { return };
        match self.folder_end_job(&dir, &pics, d < 0) {
            Some(job) => {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let ctx = self.core.ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(job());
                    ctx.request_repaint();
                });
                self.sibling_pick = Some((from, dir, by_name, rx));
            }
            // The browser's filters hide every picture there: by name.
            None => {
                self.core.toasts.info(format!("Folder: {}", crate::ui::viewer::file_name(&dir)));
                self.open_file(&by_name);
            }
        }
    }

    /// How to find the first (or with `last`, the last) of `pics`, the
    /// pictures of `dir`, in the order the browser shows that folder in: its
    /// own arranged order when it has one, else the toolbar sort, and the
    /// browser's flag, rating and label filters. The returned job (run on a
    /// worker) reads the file dates, capture dates and sizes that sort needs.
    /// None when the filters hide every picture.
    fn folder_end_job(&self, dir: &Path, pics: &[PathBuf], last: bool) -> Option<Box<dyn FnOnce() -> Option<PathBuf> + Send>> {
        use crate::settings::SortKey;
        let s = &self.core.settings;
        let key = if self.core.orders.is_active(dir) {
            SortKey::Custom
        } else if s.sort_key == SortKey::Custom {
            SortKey::Name
        } else {
            s.sort_key
        };
        let desc = s.sort_desc;
        let b = &self.browser;
        let shown: Vec<(PathBuf, crate::tags::Tag)> = pics
            .iter()
            .map(|p| (p.clone(), self.core.tags.get(p)))
            .filter(|(_, t)| (!b.flagged_only || t.flagged) && t.rating >= b.min_rating && (b.label_filter == 0 || t.label == b.label_filter))
            .collect();
        if shown.is_empty() {
            return None;
        }
        let positions = if key == SortKey::Custom { self.core.orders.positions(dir) } else { Default::default() };
        // What the metadata service already knows; the job reads the rest.
        let meta = &self.core.meta;
        let dims: HashMap<PathBuf, (u32, u32)> = if key == SortKey::Dimensions { shown.iter().filter_map(|(p, _)| meta.get(p).and_then(|m| m.dims).map(|d| (p.clone(), d))).collect() } else { Default::default() };
        let taken: HashMap<PathBuf, i64> = if key == SortKey::DateTaken { shown.iter().filter_map(|(p, _)| meta.get(p).and_then(|m| m.taken).map(|t| (p.clone(), t))).collect() } else { Default::default() };
        Some(Box::new(move || folder_end_sorted(shown, key, desc, positions, dims, taken, last)))
    }

    /// Ctrl+Z with no edit to undo: put the last moved / renamed files back,
    /// turn the last rotated / flipped files back, take deleted files out of
    /// the Recycle Bin (those as file jobs, off the UI thread: moving back
    /// to another drive copies every file), or put tags or an arranged order
    /// back (in memory, at once).
    fn undo_file_op(&mut self) {
        // One at a time: an older change expects the files as the newer
        // undo leaves them.
        if self.undoing.is_some() {
            self.core.toasts.info("Still undoing the last change");
            return;
        }
        let Some(op) = self.file_undo.pop() else {
            self.core.toasts.info("Nothing to undo. Deleted files can be restored from the Recycle Bin.");
            return;
        };
        match op.change {
            FileChange::Moves(moves) => {
                let job = format!("Moving back {}", count(moves.len(), "item"));
                self.submit_undo(job, undo_moves_job(op.label, moves));
            }
            FileChange::Order { folder, previous, active } => {
                match previous {
                    Some(v) => {
                        self.core.orders.set(&folder, v);
                        self.core.orders.set_active(&folder, active);
                    }
                    None => self.core.orders.clear(&folder),
                }
                self.browser.mark_resort();
                self.browser.reload(&self.core.ctx, self.core.settings.show_hidden_files);
                self.core.toasts.success(format!("Undone: {}", op.label));
            }
            FileChange::Delete { files, at } => {
                // The Recycle Bin can be large. The job refreshes the views
                // and reports when it is done.
                self.submit_undo(format!("Restoring {}", count(files.len(), "item")), restore_job(files, at));
            }
            FileChange::Tags { id, before } => {
                for (p, t) in &before {
                    self.core.tags.update(p, |x| *x = *t);
                }
                // Its notice's Undo button has nothing left to do.
                self.core.toasts.dismiss(id);
                // The sort or a flag / rating / label filter may depend on them.
                // (Tags live in Lumen's own store: the folder on disk did not change.)
                self.browser.mark_resort();
                if !before.is_empty() {
                    self.core.toasts.success(format!("Undone: {}", op.label));
                }
            }
            FileChange::Transform { t, files } => {
                let job = format!("Restoring {}", count(files.len(), "file"));
                self.submit_undo(job, undo_transform_job(op.label, t, files));
            }
        }
    }

    /// Run an undo as a file job (with the progress card); the next undo
    /// waits for it.
    fn submit_undo(&mut self, label: String, job: FileJob) {
        if self.file_jobs.submit(label.clone(), job) {
            self.undoing = Some(label);
        }
    }

    /// Carry out an action that was waiting on the unsaved-edits question.
    fn perform_pending(&mut self, a: PendingAction) {
        match a {
            PendingAction::OpenFile(p) => self.open_file_now(&p),
            PendingAction::OpenViewerAt(p) => {
                if let Some(i) = self.browser.index_of(&p) {
                    self.open_viewer_at(i);
                }
            }
            PendingAction::Exit => {
                self.allow_close = true;
                self.core.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PendingAction::Transfer(files, dest, do_move) => self.transfer_files_now(files, dest, do_move),
            PendingAction::Delete(files) => {
                self.ask_delete(files);
            }
        }
    }

    /// Save / Discard / Cancel for unsaved edits (window close, opening another image, moving the file).
    fn guard_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_guard.is_none() {
            return;
        }
        // The image is being saved as it is shown: the action waits for that
        // save instead of asking (and asks if the save fails).
        if self.save_covers_current() {
            if let Some(s) = &mut self.saving {
                s.then = self.pending_guard.take();
                return;
            }
        }
        let p = self.core.pal;
        let name = self.viewer.current().map(|c| crate::ui::viewer::file_name(c)).unwrap_or_default();
        let lossy = self.viewer.current().and_then(|c| lossy_resave_reason(self.viewer.loaded(&self.core).as_ref().map(|l| &l.decoded), c));
        // Two questions, one at a time: first the open tool's unapplied work
        // (Discard closes the tool), then, only for real unsaved edits,
        // Save / Discard / Cancel.
        let tool_work = self.viewer.tool_work_title();
        let dirty = self.viewer.is_dirty();
        let can_save = lossy.is_none() && self.viewer.current().map(|c| crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(c)).is_some()).unwrap_or(false);
        let mut choice: Option<u8> = None; // 0 cancel, 1 save, 2 discard, 3 discard the tool's work
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
            if let Some(title) = &tool_work {
                // Destructive: red, and Enter does not discard.
                widgets::modal_title(ui, "Discard changes?");
                widgets::modal_body(ui, &format!("{title}: the changes are not applied to \"{name}\" yet."));
                if let Some(r) = widgets::dialog_buttons(ui, &p, "Keep editing", "Discard", widgets::ButtonKind::Danger, true) {
                    choice = Some(if r { 3 } else { 0 });
                }
                return;
            }
            widgets::modal_title(ui, "Save changes?");
            widgets::modal_body(ui, &format!("\"{name}\" has unsaved edits."));
            if let Some(why) = &lossy {
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
                if widgets::secondary_button(ui, "Cancel").clicked() {
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
                // The action runs once the save is done (on the file job thread).
                let then = self.pending_guard.take();
                if let (Some(path), Some(img)) = (self.viewer.current().cloned(), self.viewer.current_image(&self.core)) {
                    let fmt = crate::formats::SaveFormat::from_ext(&crate::formats::ext_of(&path));
                    let loaded = self.viewer.loaded(&self.core);
                    let mut opts = SaveOptions { jpeg_quality: self.core.settings.jpeg_quality, keep_metadata: self.core.settings.keep_exif_on_save, ..Default::default() };
                    if let Some(l) = &loaded {
                        opts.exif = l.decoded.exif.clone();
                        if !self.core.settings.color_manage {
                            opts.icc = l.decoded.icc_profile.clone();
                        }
                    }
                    if let Some(f) = fmt {
                        opts.format = f;
                        self.do_save(&path, img, &opts, then, None);
                    }
                }
            }
            Some(3) => {
                // The tool's work is gone; unsaved edits are asked about next.
                self.viewer.close_tool();
                if !dirty {
                    if let Some(a) = self.pending_guard.take() {
                        self.perform_pending(a);
                    }
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
            || self.report_branding.is_some()
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
        keymap::set_context(match self.mode {
            Mode::Viewer => KeyMode::Viewer,
            Mode::Compare => KeyMode::Compare,
            _ => KeyMode::Browser,
        });
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
        // The folder tree (F6) owns the plain keys: arrows, Enter, letters, Del ...
        // Only shortcuts with Ctrl or Alt and the F-keys still reach the browser.
        let tree_keys = mode_map == Some(KeyMode::Browser) && self.browser.tree_focused();
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
                let tree_owns = tree_keys && !keymap::works_while_typing(&egui::KeyboardShortcut::new(mods, key));
                if let Some(m) = mode_map.filter(|_| !tree_owns) {
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
                // Letters not bound to a command start (or continue) a type-ahead search
                // in the files (the folder tree has its own when it has the keyboard).
                let texts: Vec<String> = ctx.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.clone()) } else { None }).collect());
                let mods = ctx.input(|i| i.modifiers);
                if fire.is_empty() && !mods.ctrl && !mods.alt && !tree_keys {
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
        if t.recents.is_none() {
            let mut recents: Vec<PathBuf> = Vec::new();
            for d in self.core.settings.copy_move_targets.iter().chain(self.core.settings.favorites.iter()) {
                if recents.len() < 9 && !recents.iter().any(|r| fileops::same_path(r, d)) && d.is_dir() {
                    recents.push(d.clone());
                }
            }
            t.recents = Some(recents);
        }
        let recents = t.recents.clone().unwrap_or_default();
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
            let typed_exists = match &t.typed_dir {
                Some((s, ok)) if s == dest.trim() => *ok,
                _ => {
                    let ok = PathBuf::from(dest.trim()).is_dir();
                    t.typed_dir = Some((dest.trim().to_string(), ok));
                    ok
                }
            };
            let typed = PathBuf::from(dest.trim());
            if !dest.trim().is_empty() && !typed_exists {
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
                            let renamed = match self.wait_for_save(std::slice::from_ref(&path)) {
                                // A running save writes the old name.
                                Some(msg) => Err(anyhow::anyhow!(msg)),
                                None => fileops::rename(&path, &value),
                            };
                            match renamed {
                                Ok(new) => {
                                    self.push_file_undo(format!("Rename to {}", crate::ui::viewer::file_name(&new)), vec![(path.clone(), new.clone())]);
                                    self.core.tags.moved(&path, &new);
                                    self.core.orders.rename(&path, &new);
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
        if let Some(Confirm::Delete { files, permanent }) = &self.confirm {
            let (files, permanent) = (files.clone(), *permanent);
            let what = if files.len() == 1 { format!("\"{}\"", crate::ui::viewer::file_name(&files[0])) } else { format!("{} items", files.len()) };
            let body = if permanent {
                format!("Delete {what} permanently? This drive has no Recycle Bin, so this cannot be undone.")
            } else {
                format!("Move {what} to the Recycle Bin?")
            };
            // Red, but Enter still confirms when the Recycle Bin can undo it (as in Explorer).
            match widgets::show_confirm_with(ctx, if permanent { "Delete permanently" } else { "Delete" }, &body, "Delete", widgets::ButtonKind::Danger, !permanent, &p) {
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
        if let Some(Confirm::SaveChanged { path, .. }) = &self.confirm {
            let name = crate::ui::viewer::file_name(path);
            let mut choice: Option<u8> = None; // 0 cancel, 1 save anyway, 2 save as copy
            widgets::modal(ctx, egui::Id::unique("save-changed"), 420.0, |ui| {
                widgets::modal_title(ui, "File changed on disk");
                widgets::modal_body(ui, &format!("\"{name}\" was changed by another program after it was opened here. Saving replaces that version with your edits."));
                ui.add_space(theme::space::BUTTONS_GAP);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // [Save anyway] [Cancel] [Save as copy]: the safe choice is the primary.
                    if widgets::primary_button_enabled(ui, &p, true, "Save as copy").clicked() {
                        choice = Some(2);
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        choice = Some(0);
                    }
                    if widgets::danger_button(ui, &p, "Save anyway").clicked() {
                        choice = Some(1);
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    choice = Some(0);
                }
            });
            if let Some(c) = choice {
                if let Some(Confirm::SaveChanged { path, opts, then }) = self.confirm.take() {
                    match (c, self.viewer.current_image(&self.core)) {
                        (1, Some(img)) => self.write_save(&path, img, &opts, then, None),
                        (2, Some(img)) => self.save_dialog = Some(SaveDialog::new(edited_copy_name(&path), img, opts)),
                        _ => {}
                    }
                }
            }
        }
        self.transfer_dialog(ctx);
        match self.save_dialog.as_mut().map(|d| d.show(ctx, &p)) {
            Some(SaveOutcome::Cancel) => self.save_dialog = None,
            Some(SaveOutcome::Save(path, opts)) => {
                if let Some(img) = self.viewer.current_image(&self.core) {
                    // The dialog closes while saving (the viewer stays usable)
                    // and comes back if the save fails; once saved, the viewer
                    // goes on with the copy (see `finish_save`).
                    let dlg = self.save_dialog.take();
                    self.do_save(&path, img, &opts, None, dlg);
                }
            }
            Some(SaveOutcome::Open) | None => {}
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
        if !self.core.settings.first_run_done && self.mode == Mode::Browser {
            // The UI smoke test drives a fresh app: no welcome card over its screenshots.
            if std::env::var_os("LUMEN_DEV_DIR").is_some() {
                self.core.settings.first_run_done = true;
            } else {
                self.welcome(ctx);
            }
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
            self.core.orders.rename_many(&renamed);
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
        use crate::ui::keymap;
        let p = self.core.pal;
        let mut open = true;
        let mut close = false;
        let km = keymap::active();
        // Generated from the active keymap and the mouse settings, so this list is always correct.
        let sections = crate::ui::help::filtered(crate::ui::help::sections(&km, self.core.settings.wheel_action), &self.help_query);
        crate::ui::widgets::app_window("shortcuts-help", "Keyboard shortcuts").open(&mut open).default_size(640.0, 560.0).min_size(360.0, 240.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Preset: {}", keymap::Preset::from_id(&self.core.settings.keymap_preset).label())).color(p.muted));
                if ui.link("Change shortcuts...").clicked() {
                    self.show_settings = true;
                    crate::ui::settings_window::open_page(ctx, "Keyboard");
                }
            });
            ui.add_space(6.0);
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.help_query)
                    .id_salt("help-search")
                    .hint_text(format!("{}  Search shortcuts and mouse actions", I::MAGNIFYING_GLASS))
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.help_focus) {
                search.request_focus();
            }
            // Esc in the search: clears it, or closes the window when it is empty.
            if search.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                if self.help_query.is_empty() {
                    close = true;
                } else {
                    self.help_query.clear();
                    search.request_focus();
                }
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if sections.is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new("No shortcuts or mouse actions match").color(p.muted));
                    return;
                }
                // One grid for every section, so the key column lines up at the same x.
                let no_wrap = |t: RichText| egui::Label::new(t).wrap_mode(egui::TextWrapMode::Extend);
                // One grid cell: the gap above the title lives inside it
                // (add_space directly in a grid row is not allowed).
                let title = |ui: &mut egui::Ui, t: &str| {
                    ui.vertical(|ui| {
                        ui.add_space(10.0);
                        ui.add(no_wrap(RichText::new(t).font(theme::text::subtitle()).color(p.text)));
                    });
                    ui.end_row();
                };
                egui::Grid::new("help-shortcuts").num_columns(2).spacing([28.0, 4.0]).min_row_height(24.0).striped(false).show(ui, |ui| {
                    for s in &sections {
                        title(ui, &s.title);
                        for r in &s.rows {
                            if s.mouse {
                                ui.add(no_wrap(RichText::new(&r.label)));
                                ui.add(no_wrap(RichText::new(&r.keys).color(p.muted)));
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    ui.add_sized([18.0, 20.0], egui::Label::new(RichText::new(r.icon.unwrap_or_default()).size(theme::size::ICON).color(p.muted)));
                                    ui.add(no_wrap(RichText::new(&r.label)));
                                });
                                crate::ui::widgets::shortcut_keys(ui, &p, &r.keys);
                            }
                            ui.end_row();
                        }
                    }
                });
            });
        });
        self.show_help = open && !close;
    }

    // ------------------------------------------------------------ chrome

    fn menu(&mut self, ui: &mut egui::Ui) {
        let mut fire = None;
        use crate::ui::widgets::{menu_item, menu_toggle, submenu};
        let item = |ui: &mut egui::Ui, c: Command, fire: &mut Option<Command>| {
            if crate::ui::tips::menu_tip(menu_item(ui, c.icon(), &c.label(), &c.shortcut()), c).clicked() {
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
                for c in [Command::SelectAll, Command::SelectNone, Command::InvertSelection, Command::CutFiles, Command::CopyFiles, Command::PasteFiles, Command::CopyTo, Command::MoveTo, Command::Rename, Command::Delete, Command::NewFolder] {
                    item(ui, c, &mut fire);
                }
            });
            let (details, preview, flagged, full) = (self.core.settings.browser_view == crate::settings::BrowserView::Details, self.core.settings.show_preview_pane, self.browser.flagged_only, self.fullscreen);
            submenu(ui, I::EYE, "View", |ui| {
                // On / off settings show a check mark instead of "Toggle ...".
                for (c, label, on) in [(Command::ToggleDetails, "Details view", details), (Command::TogglePreviewPane, "Preview pane", preview), (Command::ShowFlaggedOnly, "Flagged only", flagged), (Command::ToggleFullscreen, "Full screen", full)] {
                    if crate::ui::tips::menu_tip(menu_toggle(ui, on, label, &c.shortcut()), c).clicked() {
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
                for c in [Command::Workbench, Command::OpenProject] {
                    item(ui, c, &mut fire);
                }
                ui.separator();
                for c in [Command::BatchConvert, Command::BatchRename, Command::ContactSheet, Command::FindDuplicates, Command::CreateMultiPage, Command::ImageStrip, Command::CreateZip, Command::Slideshow, Command::Compare, Command::OpenVideoMode] {
                    item(ui, c, &mut fire);
                }
                ui.separator();
                for c in [Command::EditComment, Command::RemoveMetadata, Command::ChangeDates, Command::ExportXmp, Command::ExportMetadataCsv] {
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
        })
        .response
        .on_hover_text("Menu: open, edit, view, tools, settings and help");
        if let Some(c) = fire {
            self.run(c);
        }
    }

    fn handle_dropped(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        // Where and how they were dropped (Lumen's drop target, see droptarget.rs).
        let how = self.os_drop.take();
        if dropped.is_empty() {
            return;
        }
        if self.mode == Mode::Video {
            return; // video mode handles its own drops
        }
        // Onto a folder in the browser: copy or move them there, as Explorer
        // does. The zones are where the folders were in the last frame.
        if let Some(how) = how.filter(|_| self.mode == Mode::Browser && !self.modal_open()) {
            let drag = crate::droptarget::OsDrag { files: dropped.clone(), ..how };
            match crate::droptarget::plan(&self.browser.drop_zones, drag.pos(ctx.pixels_per_point()), &drag) {
                crate::droptarget::DropPlan::Transfer { files, dest, op, .. } => {
                    self.transfer_files(files, dest, op == crate::droptarget::DropOp::Move);
                    return;
                }
                crate::droptarget::DropPlan::Nothing => return,
                crate::droptarget::DropPlan::Open => {}
            }
        }
        self.open_paths(dropped);
    }

    /// While files from another program are dragged over the window: what a
    /// drop at the pointer would do (the cursor, a "Move to Holiday" badge,
    /// and the folder the browser highlights in the next frame).
    fn os_drag_feedback(&mut self, ctx: &egui::Context) {
        use crate::droptarget::{DropOp, DropPlan};
        let Some(drag) = self.os_drag.clone() else {
            self.os_target = None;
            return;
        };
        let pos = drag.pos(ctx.pixels_per_point());
        let plan = if self.mode == Mode::Browser && !self.modal_open() { crate::droptarget::plan(&self.browser.drop_zones, pos, &drag) } else { DropPlan::Open };
        crate::droptarget::show_effect(&plan);
        let target = match &plan {
            DropPlan::Transfer { dest, background, .. } => Some((dest.clone(), *background)),
            _ => None,
        };
        if target != self.os_target {
            self.os_target = target;
            ctx.request_repaint();
        }
        if let (Some(pos), DropPlan::Transfer { dest, op, .. }) = (pos, &plan) {
            let name = dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| dest.display().to_string());
            let verb = if *op == DropOp::Move { "Move" } else { "Copy" };
            crate::ui::browser::pointer_badge(ctx, pos, &format!("{verb} to {name}"), &self.core.pal);
        }
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
        let vids: Vec<PathBuf> = paths.iter().filter(|p| crate::video::is_video_path(p)).cloned().collect();
        if vids.len() == paths.len() {
            self.open_video_mode(vids);
            return;
        }
        // Images and videos together: the viewer, where videos play in place.
        let media: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file() && (crate::formats::is_image_path(p) || crate::video::is_video_path(p))).collect();
        if media.len() < 2 {
            if let Some(p) = media.first() {
                self.open_file(p);
            }
            return;
        }
        if self.viewer.has_unsaved_work() {
            self.pending_guard = Some(PendingAction::OpenFile(media[0].clone()));
            return;
        }
        self.open_file_now(&media[0]);
        if self.mode == Mode::Viewer {
            // Show exactly the handed-over files, in the given order.
            self.viewer.open(media, 0, self.core.settings.fit_mode);
        }
    }

    /// Open (or create) the project in `root` and show the workbench. The
    /// files are read (and, for a new project, scanned) on a worker thread:
    /// the folder can be large or on a slow share.
    fn open_project(&mut self, root: &Path) {
        if let Some(w) = &mut self.workbench {
            w.save_soon();
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (root, ctx) = (root.to_path_buf(), self.core.ctx.clone());
        let spawned = std::thread::Builder::new().name("lumen-open-project".into()).spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::ui::workbench::WorkbenchUi::open(&root, &ctx)));
            let _ = tx.send(res.unwrap_or_else(|_| Err(anyhow::anyhow!("the project could not be read"))));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => self.opening_project = Some(rx),
            Err(e) => self.core.toasts.error(format!("Could not open the project: {e}")),
        }
    }

    /// The project `open_project` started is read.
    fn project_opened(&mut self, res: anyhow::Result<(crate::ui::workbench::WorkbenchUi, bool)>) {
        match res {
            Ok((w, created)) => {
                if created {
                    let p = &w.ws.project;
                    self.core.toasts.success(format!("Found {} devices and {} scenes ({}). Saved as {}.", p.devices.len(), p.scenes.len(), p.detected_by, lumen_core::workbench::PROJECT_FILE));
                }
                // The old project saves what is left on a worker (dropping it saves).
                if let Some(old) = self.workbench.replace(w) {
                    let _ = std::thread::Builder::new().name("lumen-wb-close".into()).spawn(move || drop(old));
                }
                self.compare = None;
                self.mode = Mode::Workbench;
            }
            Err(e) => self.core.toasts.error(format!("Could not open the project: {e:#}")),
        }
    }

    /// Show scene `k` of the project in Compare.
    fn open_scene(&mut self, k: usize) {
        let Some(w) = &self.workbench else { return };
        let (files, mut link) = w.link_for(k);
        if files.len() < 2 {
            self.core.toasts.info("This scene has a capture from one device only");
            return;
        }
        // Video captures open in video compare (up to four, from their start).
        let videos = files.iter().filter(|f| crate::video::is_video_ext(&crate::formats::ext_of(f))).count();
        if videos == files.len() {
            self.open_video_mode(files);
            return;
        }
        if videos > 0 {
            self.core.toasts.info("This scene mixes photos and videos: only the photos are compared");
        }
        let keep: Vec<bool> = files.iter().map(|f| !crate::video::is_video_ext(&crate::formats::ext_of(f))).collect();
        fn pick<T>(v: Vec<T>, keep: &[bool]) -> Vec<T> {
            v.into_iter().zip(keep).filter(|(_, k)| **k).map(|(x, _)| x).collect()
        }
        link.labels = pick(std::mem::take(&mut link.labels), &keep);
        link.colors = pick(std::mem::take(&mut link.colors), &keep);
        let files: Vec<PathBuf> = pick(files, &keep);
        if files.len() < 2 {
            return;
        }
        let scorecard = self.compare.as_ref().map(|c| c.scorecard).unwrap_or(true);
        let mut c = crate::ui::compare::Compare::new_scene(files, &self.core, link, &w.ws.root);
        c.scorecard = scorecard;
        self.compare = Some(c);
        if let Some(w) = &mut self.workbench {
            w.selected = k;
        }
        self.mode = Mode::Compare;
    }

    /// Show the current blind trial in Compare (or end the test).
    fn open_blind_trial(&mut self) {
        let Some(w) = &self.workbench else { return };
        match w.blind_link() {
            Some((files, link)) => {
                let c = crate::ui::compare::Compare::new_scene(files, &self.core, link, &w.ws.root);
                self.compare = Some(c);
                // The Analysis panel would show file names.
                self.analysis.open = false;
                self.mode = Mode::Compare;
            }
            None => self.end_blind(),
        }
    }

    fn end_blind(&mut self) {
        if let Some(w) = &mut self.workbench {
            let n = w.blind.as_ref().map(|b| b.count).unwrap_or(0);
            w.blind = None;
            w.save_soon();
            if n > 0 {
                self.core.toasts.success(format!("Blind test: {n} vote{} recorded. The ranking is in the project's side panel.", if n == 1 { "" } else { "s" }));
            }
        }
        self.compare = None;
        self.mode = if self.workbench.is_some() { Mode::Workbench } else { Mode::Browser };
    }

    /// Requests from a compare opened from the project.
    fn compare_link_requests(&mut self) {
        let Some(c) = &mut self.compare else { return };
        let reqs = std::mem::take(&mut c.link_requests);
        let Some(link) = c.link.clone() else { return };
        for r in reqs {
            use crate::ui::compare::LinkRequest as L;
            match r {
                L::Step(d) => {
                    let next = self.workbench.as_ref().and_then(|w| w.step_scene(link.scene, d));
                    match next {
                        Some(k) => {
                            if let Some(w) = &mut self.workbench {
                                w.save_soon();
                            }
                            self.open_scene(k);
                        }
                        None => self.core.toasts.info(if d > 0 { "This is the last scene" } else { "This is the first scene" }),
                    }
                    return;
                }
                L::ToggleScorecard => {
                    if let Some(c) = &mut self.compare {
                        c.scorecard = !c.scorecard;
                    }
                }
                L::SetDone(done) => {
                    // By name: the scene list may have been reloaded since.
                    if let Some(w) = &mut self.workbench {
                        if let Some(k) = w.ws.project.scene_index(&link.name) {
                            w.set_done(k, done);
                        }
                    }
                }
                L::Vote(v) => {
                    if let Some(w) = &mut self.workbench {
                        w.vote(v);
                    }
                    self.open_blind_trial();
                    return;
                }
                L::EndBlind => {
                    self.end_blind();
                    return;
                }
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.mode {
            Mode::Viewer => match self.viewer.current() {
                Some(p) => format!("{}{} - Lumen", crate::ui::viewer::file_name(p), if self.viewer.is_dirty() { " •" } else { "" }),
                None => "Lumen".into(),
            },
            Mode::Video => "Video - Lumen".into(),
            Mode::Workbench => match &self.workbench {
                Some(w) => format!("{} - Workbench - Lumen", w.ws.project.name),
                None => "Workbench - Lumen".into(),
            },
            Mode::Compare if self.compare.as_ref().is_some_and(|c| c.link.as_ref().is_some_and(|l| l.blind)) => "Blind test - Lumen".into(),
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
        // Files dragged in from another program arrive through Lumen's own drop
        // target (droptarget.rs): hand them to egui as winit would.
        let hover = crate::droptarget::hovering();
        if hover.is_some() || crate::droptarget::installed() {
            raw.hovered_files = hover.iter().flat_map(|d| d.files.iter()).map(|p| egui::HoveredFile { path: Some(p.clone()), ..Default::default() }).collect();
        }
        if let Some(d) = crate::droptarget::take_dropped() {
            raw.dropped_files.extend(d.files.iter().map(|p| std::sync::Arc::new(crate::droptarget::DroppedPath(p.clone())) as egui::DroppedFileHandle));
            self.os_drop = Some(d);
        }
        self.os_drag = hover;
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
        self.poll_sibling_folder();
        if let Some(rx) = &self.opening_project {
            match rx.try_recv() {
                Ok(res) => {
                    self.opening_project = None;
                    self.project_opened(res);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.opening_project = None,
                Err(_) => {}
            }
        }
        // The project saves (blind votes too) and notices saves by others in
        // every mode, not only while its overview or scorecard is shown.
        if let Some(w) = &mut self.workbench {
            w.tick(&mut self.core);
        }
        if let Some(t) = self.advance_at {
            if Instant::now() >= t {
                self.advance_at = None;
                if self.mode == Mode::Viewer {
                    self.run(Command::NextImage);
                }
            } else {
                self.core.ctx.request_repaint_after(t - Instant::now());
            }
        }
        if let Some((moved, onto, after)) = self.viewer.film_arranged.take() {
            // Saved like a drag in the browser (same folder only; other lists are just shown arranged).
            if moved.parent() == Some(self.browser.folder.as_path()) {
                if let Some(t) = self.browser.index_of(&onto) {
                    self.browser.arrange(&mut self.core, &[moved], Some(t), after);
                }
            }
        }
        if let Some((folder, previous, active)) = self.browser.arranged.take() {
            self.push_undo(FileUndo { label: "Arrange".into(), change: FileChange::Order { folder, previous, active } });
        }
        while let Ok(res) = self.file_jobs.rx.try_recv() {
            self.file_jobs.queued = self.file_jobs.queued.saturating_sub(1);
            self.finish_file_job(res);
        }
        if self.last_save.elapsed().as_secs() > 5 {
            // Serialized here, written to disk on the background writer.
            self.core.tags.save_in_background();
            self.core.orders.save_in_background();
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
        // Files from another program over the window: the browser notes where
        // its folders are this frame and highlights the one a drop would go to.
        self.browser.drop_zones.clear();
        self.browser.os_drop = self.os_drag.as_ref().and_then(|d| d.pos(ctx.pixels_per_point())).map(|pos| crate::droptarget::Hover { pos, target: self.os_target.clone() });
        self.clipboard_keys(&ctx);
        self.shortcuts(&ctx);
        self.browser.suppress_keys = self.modal_open() || self.tool_window_active;
        self.update_title(&ctx);
        self.viewer.edge_panels = self.core.settings.edge_panels();
        let p = self.core.pal;

        // The Analysis panel sits right of the mode's whole view, so it is shown
        // before the mode's central panel; its overlays are drawn after it.
        #[cfg(feature = "video")]
        if self.mode != Mode::Video {
            // Video mode publishes its state every frame; drop it (and the
            // frames it holds) elsewhere.
            self.core.metrics.set_video(None);
        }
        // Project scenes: the Analysis panel names each file's device.
        crate::analysis_ui::set_display_labels(match (&self.compare, self.mode) {
            (Some(c), Mode::Compare) => match &c.link {
                Some(l) if !l.blind => c.paths.iter().cloned().zip(l.labels.iter().cloned()).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        });
        // A 3 or 4 image compare measures C and D against A too.
        self.core.metrics.set_compare_panes(match (&self.compare, self.mode) {
            (Some(c), Mode::Compare) => c.paths.clone(),
            _ => Vec::new(),
        });
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
                let tree_w = crate::ui::safe::safe_clamp(self.core.settings.tree_width, *range.start(), *range.end());
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
                            // Files already in the folder stay where they are.
                            let files = crate::droptarget::movable(&files, &target);
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
                    let t = std::time::Instant::now();
                    events = self.viewer.ui(ui, &mut self.core, fullscreen);
                    crate::devhook::prof("viewer", t);
                });
                for e in events {
                    match e {
                        ViewerEvent::Command(c) => self.run(c),
                        ViewerEvent::Close => self.run(Command::CloseViewer),
                        ViewerEvent::LosslessCrop { path, rect } => self.lossless_crop(path, rect),
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
                // A project scene: the scorecard panel beside the images.
                let card = match (&self.compare, &self.workbench) {
                    (Some(c), Some(w)) if c.scorecard => c.link.as_ref().filter(|l| !l.blind).and_then(|l| {
                        // By name: scores must land in the scene shown, also
                        // after the project was reloaded (scenes renumbered).
                        let k = w.ws.project.scene_index(&l.name)?;
                        let files = w.ws.scene_files(k);
                        let mut devs: Vec<String> = Vec::new();
                        for (d, _) in files.into_iter().take(4) {
                            if !devs.contains(&d) {
                                devs.push(d);
                            }
                        }
                        Some((k, devs))
                    }),
                    _ => None,
                };
                if let (Some((scene, devs)), Some(w)) = (card, self.workbench.as_mut()) {
                    let max = (ctx.content_rect().width() * 0.4).clamp(280.0, 520.0);
                    let close = egui::Panel::right("wb-scorecard")
                        .resizable(true)
                        .default_size(320.0_f32.min(max))
                        .size_range(260.0..=max)
                        .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin { left: 12, right: 4, top: 10, bottom: 8 }))
                        .show(ui, |ui| w.scorecard(ui, &p, scene, &devs))
                        .inner;
                    if close {
                        if let Some(c) = &mut self.compare {
                            c.scorecard = false;
                        }
                    }
                }
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    if let Some(c) = &mut self.compare {
                        // The report's measurements page: the Analysis panel's
                        // per-image checks and text checks, when they have run.
                        c.set_metrics_table(merge_tables(self.core.metrics.report_table(), self.analysis.text.report_table()));
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
                // Ratings, flags and Keep A / reject others: on the undo history,
                // with the notice and its Undo button; Ctrl+Z there undoes them.
                let (changes, undo) = self.compare.as_mut().map(|c| c.take_tag_changes()).unwrap_or_default();
                for (notice, before) in changes {
                    self.record_tags(before, notice);
                }
                if undo {
                    match self.file_undo.last() {
                        Some(FileUndo { change: FileChange::Tags { .. }, .. }) => self.undo_file_op(),
                        _ => self.core.toasts.info("Nothing to undo here"),
                    }
                }
                self.compare_link_requests();
                if let Some(r) = self.compare.as_mut().and_then(|c| c.info_request.take()) {
                    self.run(match r {
                        crate::ui::compare::InfoRequest::Metadata => Command::ShowMetadata,
                    });
                }
                if !keep {
                    let linked = self.compare.as_ref().and_then(|c| c.link.clone());
                    match linked {
                        Some(l) if l.blind => self.end_blind(),
                        Some(_) if self.workbench.is_some() => {
                            self.compare = None;
                            if let Some(w) = &mut self.workbench {
                                w.save_soon();
                            }
                            self.mode = Mode::Workbench;
                        }
                        _ => {
                            self.compare = None;
                            self.mode = Mode::Browser;
                        }
                    }
                }
            }
            Mode::Workbench => {
                let mut reqs = Vec::new();
                match &mut self.workbench {
                    Some(w) => {
                        let t = Instant::now();
                        w.ui(ui, &mut self.core);
                        crate::devhook::prof("workbench", t);
                        reqs = std::mem::take(&mut w.requests);
                    }
                    None => self.mode = Mode::Browser,
                }
                for r in reqs {
                    use crate::ui::workbench::WbRequest as R;
                    match r {
                        R::OpenScene(k) => self.open_scene(k),
                        R::BlindTrial => self.open_blind_trial(),
                        R::Back => {
                            if let Some(w) = &mut self.workbench {
                                w.save_soon();
                            }
                            self.mode = Mode::Browser;
                        }
                    }
                }
            }
            Mode::Video => {
                let mut keep = true;
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    if let Some(v) = &mut self.video {
                        keep = v.ui(ui, &mut self.core);
                    }
                });
                // "Open frames in Compare": the saved frames open in image Compare.
                if let Some(frames) = self.video.as_mut().and_then(|v| v.take_compare_request()) {
                    self.video = None;
                    self.compare = Some(crate::ui::compare::Compare::new(frames, &self.core));
                    self.mode = Mode::Compare;
                } else if !keep {
                    self.video = None;
                    // Opened from the viewer's video: back to it.
                    let back = self.video_from_viewer && self.viewer.current().is_some();
                    self.video_from_viewer = false;
                    self.mode = if back { Mode::Viewer } else { Mode::Browser };
                    if self.fullscreen && !back {
                        self.set_fullscreen(false);
                    }
                }
            }
        }
        self.os_drag_feedback(&ctx);

        let t_overlay = Instant::now();
        analysis_actions.extend(self.analysis.overlay(ui, &mut self.core, subject.as_ref()));
        crate::devhook::prof("analysis_overlay", t_overlay);
        self.analysis_actions(&ctx, analysis_actions);
        match self.core.analysis_request.take() {
            Some(crate::analysis_ui::PanelRequest::Toggle) => self.run(Command::ToggleAnalysis),
            Some(crate::analysis_ui::PanelRequest::Open) if !self.analysis.open => self.run(Command::ToggleAnalysis),
            _ => {}
        }
        // The metadata window follows the viewer to the next image.
        if self.meta_window.open && self.mode == Mode::Viewer {
            if let Some(cur) = self.viewer.current().cloned() {
                if self.meta_window.paths() != [cur.clone()] {
                    let tab = self.meta_window.tab;
                    self.meta_window.show_for(vec![(String::new(), cur, p.accent)], tab, &ctx);
                }
            }
        }
        self.meta_window.ui(&ctx, &p);
        self.dialogs(&ctx);
        self.guard_dialog(&ctx);
        if let Some(w) = &mut self.workbench {
            w.conflict_dialog(&ctx, &mut self.core);
        }
        if let Some(d) = &mut self.report_branding {
            let out = d.show(&ctx, &p, crate::ui::branding::Target::Defaults(&mut self.core.settings.report_branding));
            if let Some(e) = out.error {
                self.core.toasts.error(e);
            }
            if !out.open {
                self.report_branding = None;
            }
        }
        // Leaving video mode by any route stops playback.
        if self.mode != Mode::Video && self.video.is_some() {
            self.video = None;
        }
        if self.mode != Mode::Video {
            self.video_from_viewer = false;
        }
        // So does leaving (or covering) the viewer: its video decoder is freed.
        if self.mode != Mode::Viewer {
            self.viewer.stop_video();
        }
        // Compare's own commands are offered in Compare only.
        let in_compare = self.mode == Mode::Compare;
        let available = |c: Command| -> bool { in_compare || !Command::COMPARE.contains(&c) };
        if let Some(c) = self.palette.show(&ctx, &p, &available) {
            self.run(c);
        }
        self.file_job_progress(&ctx, &p);
        // Above the status bar in the browser; above the floating bottom bar (and
        // its 12 pt gap) of the viewer, compare and video modes.
        self.core.toasts.set_bottom(if self.mode == Mode::Browser { 44.0 } else { 84.0 });
        if let Some(id) = self.core.toasts.show(&ctx, &p) {
            self.undo_tags_by_id(id);
        }
        self.was_editing = ctx.text_edit_focused();
        crate::devhook::prof("ui_total", frame_start);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Let running copy / move jobs finish (bounded) so their tags and undo
        // records are not lost.
        let mut deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut stopped = false;
        while self.file_jobs.queued > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.file_jobs.rx.recv_timeout(left) {
                Ok(res) => {
                    self.file_jobs.queued -= 1;
                    self.finish_file_job(res);
                }
                // Still copying: stop. The file being copied is dropped (copies are
                // made under a hidden temporary name and only renamed when complete)
                // and the job still reports what it finished.
                Err(_) if !stopped => {
                    fileops::cancel_copies();
                    stopped = true;
                    deadline = Instant::now() + std::time::Duration::from_secs(3);
                }
                Err(_) => break,
            }
        }
        if stopped {
            let left = fileops::remove_partial_copies();
            if left > 0 {
                log::warn!("{left} unfinished copies could not be removed (hidden .lumen-copy folders)");
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
                crate::devhook::DevAction::Events(mut ev) => {
                    self.dev_input.append(&mut ev);
                    ctx.request_repaint();
                }
                crate::devhook::DevAction::Hover(pos) => {
                    self.dev_input.push(egui::Event::PointerMoved(pos));
                    ctx.request_repaint();
                }
                crate::devhook::DevAction::Scroll(dy) => {
                    self.dev_input.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::Vec2::new(0.0, dy),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: egui::Modifiers::NONE,
                    });
                    ctx.request_repaint();
                }
                crate::devhook::DevAction::Ppp(s) => {
                    // Same effect on layout and textures as a display with another scale.
                    ctx.set_zoom_factor(s * self.core.settings.ui_scale.clamp(0.6, 2.5));
                    ctx.request_repaint();
                }
                crate::devhook::DevAction::OsDrag { pos, ctrl, shift, files } => {
                    let ppp = ctx.pixels_per_point();
                    let allowed = fileops::EFFECT_COPY | fileops::EFFECT_MOVE | fileops::EFFECT_LINK;
                    crate::droptarget::simulate(Some(crate::droptarget::OsDrag { files, px: Some((pos.x * ppp, pos.y * ppp)), ctrl, shift, allowed }), false);
                    ctx.request_repaint();
                }
                crate::devhook::DevAction::OsDrop(drop) => {
                    crate::droptarget::simulate(None, drop);
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
                    if self.viewer.showing_video() {
                        self.dev.write_state(&format!("viewer video: {}", self.viewer.video_state()));
                    }
                    if let Some(s) = &self.slideshow {
                        self.dev.write_state(&format!("slideshow: {}", s.state_text()));
                    }
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
        let (maximized, full, inner) = self.core.ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false), i.viewport().inner_rect));
        self.core.settings.window_maximized = maximized;
        if let Some(r) = inner.filter(|_| !maximized && !full) {
            if r.width() >= 640.0 && r.height() >= 420.0 {
                self.core.settings.window_size = Some([r.width().round(), r.height().round()]);
            }
        }
        // On exit: written on this thread, after any background save, so nothing is lost.
        self.core.settings.save_now();
        self.core.tags.save_if_dirty();
        self.core.orders.save_if_dirty();
        // The project too (blind votes, scorecard edits not saved yet).
        if let Some(w) = &mut self.workbench {
            w.save_blocking();
        }
    }
}

/// The first (`d` > 0) or last picture of the next or previous sibling folder
/// of `cur` that has pictures, in natural name order, with that folder.
fn sibling_folder_pictures(cur: &Path, d: isize) -> Option<(PathBuf, Vec<PathBuf>)> {
    let parent = cur.parent()?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort_by(|a, b| crate::catalog::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    let at = dirs.iter().position(|p| fileops::same_path(p, cur))?;
    let candidates: Vec<&PathBuf> = if d > 0 { dirs[at + 1..].iter().take(50).collect() } else { dirs[..at].iter().rev().take(50).collect() };
    for dir in candidates {
        let mut pics: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| crate::formats::is_image_path(p)).take(20000).collect())
            .unwrap_or_default();
        if pics.is_empty() {
            continue;
        }
        pics.sort_by(|a, b| crate::catalog::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        return Some((dir.clone(), pics));
    }
    None
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
    /// `folders`: how many of `done` are folders (for the message).
    Transfer { done: Vec<(PathBuf, PathBuf)>, errs: Vec<String>, dest: PathBuf, do_move: bool, folders: usize },
    /// Changed files with their previous bytes for undo (`None` once the
    /// budget for keeping them is used up). `trimmed`: pixels cut off the
    /// width and height of JPEGs whose picture was turned (per file, only
    /// when any were); `gain_maps`: HDR gain maps removed that way.
    Rotate { t: Transform, lossless: Vec<Transformed>, reencoded: Vec<Transformed>, skipped: Vec<String>, errs: Vec<String>, trimmed: Vec<(u32, u32)>, gain_maps: usize },
    /// A lossless JPEG crop: the file as it was, its new displayed size.
    Cropped { file: Transformed, size: (u32, u32), gain_map_removed: bool },
    /// Deleted files taken out of the Recycle Bin again (undo of a delete).
    Restore { files: Vec<DeletedItem>, res: crate::recycle::Restored },
    /// Moved or renamed items put back (undo of `label`, `total` items):
    /// `done` as (where each was, where it is again), `errs` as (name, why).
    UndoMoves { label: String, total: usize, done: Vec<(PathBuf, PathBuf)>, errs: Vec<(String, String)> },
    /// Rotated / flipped `files` turned back (undo of `label`): how many,
    /// those restored with their stamp before the change and now, and the
    /// ones left as they were (name, why).
    UndoTransform { label: String, files: Vec<PathBuf>, ok: usize, restored: Vec<(PathBuf, fileops::FileStamp, Option<fileops::FileStamp>)>, errs: Vec<(String, String)> },
    /// The save `App::saving` describes is done.
    Saved(Result<(), String>),
    Panicked(String),
    /// A job that only reports a line to the user.
    Message { ok: bool, text: String },
    /// Text for the clipboard (OCR).
    CopyText(String),
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

    /// Queue `job`; false when the worker is not running.
    fn submit(&mut self, label: impl Into<String>, job: FileJob) -> bool {
        let sent = self.tx.send((label.into(), job)).is_ok();
        if sent {
            self.queued += 1;
        }
        sent
    }
}

/// Inspect `files` one by one and write one CSV row per file to `dest`.
fn metadata_csv_job(files: Vec<PathBuf>, dest: PathBuf) -> FileJob {
    Box::new(move |pr| {
        let total = files.len();
        let mut found = Vec::with_capacity(total);
        for (i, f) in files.iter().enumerate() {
            pr.set(i, total);
            let name = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            found.push((name, lumen_core::inspect::inspect_file(f)));
        }
        pr.set(total, total);
        let rows: Vec<(String, &lumen_core::inspect::Inspection)> = found.iter().map(|(n, i)| (n.clone(), i)).collect();
        let csv = lumen_core::inspect::table_csv(&rows);
        let shown = dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::write(&dest, csv) {
            Ok(()) => JobResult::Message { ok: true, text: format!("Metadata of {} saved to {shown}", count(total, "image")) },
            Err(e) => JobResult::Message { ok: false, text: format!("Could not save {shown}: {e}") },
        }
    })
}

/// Copy or move `files` into `dest` one by one, reporting progress.
fn transfer_job(files: Vec<PathBuf>, dest: PathBuf, do_move: bool) -> FileJob {
    Box::new(move |pr| {
        let total = files.len();
        let (mut done, mut errs) = (Vec::new(), Vec::new());
        for (i, f) in files.iter().enumerate() {
            // Lumen is closing: stop between files (the one being copied is dropped).
            if fileops::copies_cancelled() {
                break;
            }
            pr.set(i, total);
            let (d, e) = fileops::transfer(std::slice::from_ref(f), &dest, do_move);
            done.extend(d);
            errs.extend(e);
        }
        let folders = done.iter().filter(|(_, to)| to.is_dir()).count();
        JobResult::Transfer { done, errs, dest, do_move, folders }
    })
}

/// Put deleted files back from the Recycle Bin where they were.
fn restore_job(files: Vec<DeletedItem>, at: std::time::SystemTime) -> FileJob {
    Box::new(move |pr| {
        let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let res = crate::recycle::restore(&paths, at, &|done: usize, total: usize| pr.set(done, total));
        JobResult::Restore { files, res }
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
        let (mut trimmed, mut gain_maps) = (Vec::new(), 0);
        for (i, p) in jpegs.iter().enumerate() {
            pr.set(i, total);
            let before = keep(p);
            let was = fileops::FileStamp::of(p);
            match crate::encode::lossless_jpeg_transform(p, t) {
                Ok(turn) => {
                    if turn.trimmed != (0, 0) {
                        trimmed.push(turn.trimmed);
                    }
                    gain_maps += usize::from(turn.gain_map_removed);
                    lossless.push(Transformed { path: p.clone(), before, was, after: fileops::FileStamp::of(p) });
                }
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
            let was = fileops::FileStamp::of(p);
            let out = t.apply(&d.image);
            // A lossless WebP stays lossless (re-encoding it lossy would lose quality).
            let webp_lossless = fmt == crate::formats::SaveFormat::WebP && crate::encode::webp_file_is_lossless(p);
            let opts = SaveOptions { format: fmt, jpeg_quality, webp_lossless, exif: d.exif.clone(), ..Default::default() };
            match crate::encode::save_from(&out, p, &opts, Some(p)) {
                Ok(()) => reencoded.push(Transformed { path: p.clone(), before, was, after: fileops::FileStamp::of(p) }),
                Err(e) => errs.push(format!("{name}: {e}")),
            }
        }
        JobResult::Rotate { t, lossless, reencoded, skipped, errs, trimmed, gain_maps }
    })
}

/// Lossless crop of the JPEG `path` to `rect` (its displayed pixels), in place.
fn lossless_crop_job(path: PathBuf, rect: (u32, u32, u32, u32)) -> FileJob {
    Box::new(move |_| {
        let name = crate::ui::viewer::file_name(&path);
        let was = fileops::FileStamp::of(&path);
        let res = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|before| {
            let e = lumen_core::jpeg_lossless::crop(&before, rect)?;
            crate::encode::write_atomic(&path, &e.bytes)?;
            Ok((before, e))
        });
        match res {
            Ok((before, e)) => {
                // The file as it was, for an exact undo.
                let before = Some(before).filter(|b| b.len() <= UNDO_BYTES_BUDGET);
                let file = Transformed { path: path.clone(), before, was, after: fileops::FileStamp::of(&path) };
                JobResult::Cropped { file, size: (e.kept.2, e.kept.3), gain_map_removed: e.gain_map_removed }
            }
            Err(e) => JobResult::Message { ok: false, text: format!("Could not crop {name} losslessly: {e}") },
        }
    })
}

/// "8 px" or "8 x 16 px" (width x height) of a lossless turn's trim.
fn px_size(w: u32, h: u32) -> String {
    match (w, h) {
        (w, 0) => format!("{w} px"),
        (0, h) => format!("{h} px"),
        (w, h) => format!("{w} × {h} px"),
    }
}

fn gain_map_note(n: usize) -> String {
    if n == 1 {
        "The HDR gain map was removed: it no longer matched the picture".to_string()
    } else {
        format!("The HDR gain maps of {} were removed: they no longer matched the pictures", count(n, "file"))
    }
}

/// Undo of moves and renames (`label`): each item goes back where it was,
/// the last moved first. Across drives that copies it back.
fn undo_moves_job(label: String, moves: Vec<(PathBuf, PathBuf)>) -> FileJob {
    Box::new(move |pr| {
        let total = moves.len();
        let (mut done, mut errs) = (Vec::new(), Vec::new());
        for (i, (orig, cur)) in moves.iter().rev().enumerate() {
            // Lumen is closing: stop between items (the one being copied is dropped).
            if fileops::copies_cancelled() {
                break;
            }
            pr.set(i, total);
            match fileops::move_path(cur, orig) {
                Ok(()) => done.push((cur.clone(), orig.clone())),
                Err(e) => errs.push((crate::ui::viewer::file_name(cur), e.root_cause().to_string())),
            }
        }
        // In the order they were moved (the first one gets the focus).
        done.reverse();
        JobResult::UndoMoves { label, total, done, errs }
    })
}

/// Undo of a rotation / flip `t` (`None`: a lossless crop) written to
/// `files` (`label`).
fn undo_transform_job(label: String, t: Option<Transform>, files: Vec<Transformed>) -> FileJob {
    Box::new(move |pr| {
        let total = files.len();
        let (mut ok, mut restored, mut errs) = (0, Vec::new(), Vec::new());
        for (i, f) in files.iter().enumerate() {
            pr.set(i, total);
            match restore_transformed(f, t) {
                Ok(()) => {
                    ok += 1;
                    if let Some(was) = f.was {
                        restored.push((f.path.clone(), was, fileops::FileStamp::of(&f.path)));
                    }
                }
                Err(e) => errs.push((crate::ui::viewer::file_name(&f.path), e.to_string())),
            }
        }
        // (The kept contents, up to the undo budget, are freed here.)
        let files = files.into_iter().map(|f| f.path).collect();
        JobResult::UndoTransform { label, files, ok, restored, errs }
    })
}

/// Put one file rotated / flipped by `t` (or cropped) back: its previous
/// bytes when they were kept, else (a JPEG turned losslessly) the inverse turn.
fn restore_transformed(f: &Transformed, t: Option<Transform>) -> anyhow::Result<()> {
    let p = &f.path;
    // Changed again since (saved from another program, edited here):
    // writing the old contents back would lose that.
    let changed = f.after.is_some() && fileops::FileStamp::of(p) != f.after;
    match &f.before {
        _ if !p.is_file() => Err(anyhow::anyhow!("the file no longer exists")),
        _ if changed => Err(anyhow::anyhow!("the file was modified since, so it was left as it is")),
        Some(b) => crate::encode::write_atomic(p, b),
        None => match t {
            Some(t) => crate::encode::lossless_jpeg_transform(p, inverse(t)).map(|_| ()),
            None => Err(anyhow::anyhow!("the file before the change was not kept")),
        },
    }
}

/// The message for the items an undo left as they were (`errs`: name and
/// why) out of `total`: by name, the first few when there are several.
fn undo_failures(errs: &[(String, String)], total: usize) -> Option<String> {
    match errs {
        [] => None,
        [(name, why)] => Some(format!("Could not undo \"{name}\": {why}")),
        all => {
            let names: Vec<&str> = all.iter().take(3).map(|(n, _)| n.as_str()).collect();
            let more = if all.len() > 3 { ", ..." } else { "" };
            Some(format!("Could not undo {} of {}: {}{more} ({})", all.len(), count(total, "item"), names.join(", "), all[0].1))
        }
    }
}

/// Encode `img` and write it to `path` (atomically, see `write_atomic`);
/// `source`, the open file, gives a JPEG its comment, XMP and IPTC.
fn save_job(img: Arc<image::RgbaImage>, path: PathBuf, opts: SaveOptions, source: Option<PathBuf>) -> FileJob {
    Box::new(move |_| JobResult::Saved(crate::encode::save_from(&img, &path, &opts, source.as_deref()).map_err(|e| e.to_string())))
}

/// The picture a job works on: `img` (the viewer's, as shown), else the
/// first frame of the file at `path`, decoded here.
fn job_image(img: Option<Arc<image::RgbaImage>>, path: &Path) -> Result<Arc<image::RgbaImage>, String> {
    if let Some(img) = img {
        return Ok(img);
    }
    let first_frame = crate::decode::DecodeOptions { animations: false, ..Default::default() };
    crate::decode::decode_file(path, first_frame).map(|d| d.image).map_err(|e| format!("Could not open {}: {e}", crate::ui::viewer::file_name(path)))
}

/// Copy image: `img`, or the file at `path`, onto the clipboard.
fn copy_image_job(img: Option<Arc<image::RgbaImage>>, path: PathBuf) -> FileJob {
    Box::new(move |_| {
        let res = job_image(img, &path).and_then(|img| fileops::copy_image_to_clipboard(&img).map_err(|e| format!("Copy failed: {e}")));
        match res {
            Ok(()) => JobResult::Message { ok: true, text: "Image copied to clipboard".into() },
            Err(text) => JobResult::Message { ok: false, text },
        }
    })
}

/// Set as wallpaper: the file at `path` as it is (`as_is`: a JPEG, PNG or
/// BMP), else `img` or the decoded file, written as a JPEG in Lumen's data
/// folder first.
fn wallpaper_job(path: PathBuf, as_is: bool, img: Option<Arc<image::RgbaImage>>) -> FileJob {
    Box::new(move |_| {
        let target = if as_is {
            Ok(path)
        } else {
            job_image(img, &path).and_then(|img| {
                let dir = crate::settings::data_dir();
                let _ = std::fs::create_dir_all(&dir);
                let out = dir.join("wallpaper.jpg");
                let opts = SaveOptions { jpeg_quality: 95, ..Default::default() };
                crate::encode::save(&img, &out, &opts).map(|()| out).map_err(|e| e.to_string())
            })
        };
        match target.and_then(|t| fileops::set_wallpaper(&t, 0).map_err(|e| e.to_string())) {
            Ok(()) => JobResult::Message { ok: true, text: "Desktop wallpaper set".into() },
            Err(text) => JobResult::Message { ok: false, text },
        }
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
    fn undo_failures_name_the_files() {
        let e = |n: &str| (n.to_string(), "Access is denied".to_string());
        assert_eq!(undo_failures(&[], 5), None);
        assert_eq!(undo_failures(&[e("a.jpg")], 5).as_deref(), Some("Could not undo \"a.jpg\": Access is denied"));
        assert_eq!(undo_failures(&[e("a.jpg"), e("b.jpg")], 2000).as_deref(), Some("Could not undo 2 of 2000 items: a.jpg, b.jpg (Access is denied)"));
        let many = [e("a.jpg"), e("b.jpg"), e("c.jpg"), e("d.jpg")];
        assert_eq!(undo_failures(&many, 4).as_deref(), Some("Could not undo 4 of 4 items: a.jpg, b.jpg, c.jpg, ... (Access is denied)"));
    }

    /// Undo of moves runs on a worker: every item goes back, the last moved
    /// first (so a chain of renames comes undone), and one that cannot is
    /// named.
    #[test]
    fn undoing_moves_puts_items_back_and_names_failures() {
        let d = std::env::temp_dir().join(format!("lumen-undo-moves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("to")).unwrap();
        // a.jpg was renamed to b.jpg, then b.jpg moved into to/; c.jpg was
        // moved too but is gone now.
        std::fs::write(d.join("to").join("b.jpg"), b"a").unwrap();
        let moves = vec![(d.join("a.jpg"), d.join("b.jpg")), (d.join("b.jpg"), d.join("to").join("b.jpg")), (d.join("c.jpg"), d.join("to").join("c.jpg"))];
        let progress = JobProgress { status: Arc::new(Mutex::new(JobStatus::default())), ctx: egui::Context::default() };
        match undo_moves_job("Move".into(), moves)(&progress) {
            JobResult::UndoMoves { label, total, done, errs } => {
                assert_eq!((label.as_str(), total), ("Move", 3));
                assert_eq!(done, vec![(d.join("b.jpg"), d.join("a.jpg")), (d.join("to").join("b.jpg"), d.join("b.jpg"))]);
                assert_eq!(errs.len(), 1);
                assert_eq!(errs[0].0, "c.jpg");
            }
            _ => panic!("not an undo of moves"),
        }
        assert_eq!(std::fs::read(d.join("a.jpg")).unwrap(), b"a");
        assert!(!d.join("b.jpg").exists() && !d.join("to").join("b.jpg").exists());
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

    #[test]
    fn tag_notices_say_what_changed_and_how_many() {
        assert_eq!(tag_notice(Command::Rate(3), false, 50), "Rated 3 stars \u{00B7} 50 files");
        assert_eq!(tag_notice(Command::Rate(1), false, 1), "Rated 1 star");
        assert_eq!(tag_notice(Command::Rate(0), false, 500), "Rating cleared \u{00B7} 500 files");
        assert_eq!(tag_notice(Command::ToggleFlag, true, 4), "Flagged 4 files");
        assert_eq!(tag_notice(Command::ToggleFlag, false, 4), "Flag removed from 4 files");
        assert_eq!(tag_notice(Command::ToggleFlag, true, 1), "Flagged");
        assert_eq!(tag_notice(Command::Label(1), false, 2), "Label red \u{00B7} 2 files");
        assert_eq!(tag_notice(Command::Label(0), false, 1), "Label cleared");
    }

    #[test]
    fn next_folder_starts_where_its_sort_starts() {
        use crate::settings::SortKey;
        let entry = |name: &str, secs: u64| catalog::Entry {
            path: PathBuf::from(format!(r"C:\f\{name}")),
            name: name.into(),
            ext: "jpg".into(),
            kind: EntryKind::Image,
            size: secs,
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            created: None,
            hidden: false,
        };
        let list = || vec![entry("a.jpg", 30), entry("b.jpg", 10), entry("c.jpg", 20)];
        // Taken dates: c before a; b has none and sorts by its file date (10 s), first.
        let taken = |p: &Path| -> Option<i64> {
            match p.file_name().and_then(|n| n.to_str()) {
                Some("a.jpg") => Some(200),
                Some("c.jpg") => Some(100),
                _ => None,
            }
        };
        // Arranged by hand: b, c, a.
        let custom = |p: &Path| -> Option<usize> {
            match p.file_name().and_then(|n| n.to_str()) {
                Some("b.jpg") => Some(0),
                Some("c.jpg") => Some(1),
                Some("a.jpg") => Some(2),
                _ => None,
            }
        };
        let none_dims = |_: &Path| -> Option<(u32, u32)> { None };
        let zero = |_: &Path| 0u8;
        let no = |_: &Path| false;
        let cx = catalog::SortContext { dims: &none_dims, taken: &taken, rating: &zero, flagged: &no, label: &zero, custom: &custom };
        let name = |p: Option<PathBuf>| p.and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
        assert_eq!(name(end_in_order(list(), SortKey::Name, false, &cx, false)), "a.jpg");
        assert_eq!(name(end_in_order(list(), SortKey::Name, true, &cx, false)), "c.jpg");
        assert_eq!(name(end_in_order(list(), SortKey::Modified, false, &cx, false)), "b.jpg");
        assert_eq!(name(end_in_order(list(), SortKey::DateTaken, false, &cx, false)), "b.jpg");
        assert_eq!(name(end_in_order(list(), SortKey::DateTaken, false, &cx, true)), "a.jpg");
        assert_eq!(name(end_in_order(list(), SortKey::Custom, false, &cx, false)), "b.jpg");
        // An arranged order has no direction.
        assert_eq!(name(end_in_order(list(), SortKey::Custom, true, &cx, true)), "a.jpg");
        assert_eq!(end_in_order(Vec::new(), SortKey::Name, false, &cx, false), None);
    }
}

/// Join the image checks and the text checks into one report table; text
/// rows are added under their own heading row when both exist.
fn merge_tables(a: Option<lumen_core::report::Table>, b: Option<lumen_core::report::Table>) -> Option<lumen_core::report::Table> {
    match (a, b) {
        (Some(mut a), Some(b)) => {
            let cols = a.header.len().max(b.header.len());
            if !b.rows.is_empty() {
                let mut head = vec!["Text".to_string()];
                head.resize(cols, String::new());
                a.rows.push(head);
                a.rows.extend(b.rows);
            }
            if !b.note.is_empty() {
                a.note = if a.note.is_empty() { b.note } else { format!("{} {}", a.note, b.note) };
            }
            Some(a)
        }
        (a, b) => a.or(b),
    }
}
