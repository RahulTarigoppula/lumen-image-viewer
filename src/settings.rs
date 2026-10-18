//! Persistent user settings. Stored as JSON in `%APPDATA%\Lumen\settings.json`,
//! or next to the executable when a `portable.txt` file is present there.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WheelAction {
    Navigate,
    Zoom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FitMode {
    /// Shrink large images to fit, show small images at 100%.
    FitLarge,
    /// Always scale to fit the window (enlarges small images too).
    FitAlways,
    /// Fill the window (crop overflow).
    Fill,
    FitWidth,
    FitHeight,
    Actual,
}

impl FitMode {
    pub fn label(self) -> &'static str {
        match self {
            FitMode::FitLarge => "Fit large images",
            FitMode::FitAlways => "Fit all images",
            FitMode::Fill => "Fill window",
            FitMode::FitWidth => "Fit width",
            FitMode::FitHeight => "Fit height",
            FitMode::Actual => "Actual size (100%)",
        }
    }
    pub const ALL: [FitMode; 6] = [
        FitMode::FitLarge,
        FitMode::FitAlways,
        FitMode::Fill,
        FitMode::FitWidth,
        FitMode::FitHeight,
        FitMode::Actual,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Created,
    Type,
    Dimensions,
    DateTaken,
    Rating,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Size => "File size",
            SortKey::Modified => "Date modified",
            SortKey::Created => "Date created",
            SortKey::Type => "Type",
            SortKey::Dimensions => "Dimensions",
            SortKey::DateTaken => "Date taken (EXIF)",
            SortKey::Rating => "Rating",
        }
    }
    pub const ALL: [SortKey; 8] = [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Created,
        SortKey::Type,
        SortKey::Dimensions,
        SortKey::DateTaken,
        SortKey::Rating,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserView {
    Thumbnails,
    Details,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SlideshowSettings {
    pub interval_secs: f32,
    pub transition: String,
    pub transition_secs: f32,
    pub random_order: bool,
    pub loop_show: bool,
    pub include_subfolders: bool,
    pub show_caption: bool,
    pub ken_burns: bool,
    pub fit: FitMode,
}

impl Default for SlideshowSettings {
    fn default() -> Self {
        Self {
            interval_secs: 4.0,
            transition: "Crossfade".into(),
            transition_secs: 0.8,
            random_order: false,
            loop_show: true,
            include_subfolders: false,
            show_caption: true,
            ken_burns: false,
            fit: FitMode::FitAlways,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub accent: [u8; 3],
    pub ui_scale: f32,
    pub wheel_action: WheelAction,
    pub fit_mode: FitMode,
    pub open_viewer_fullscreen: bool,
    pub viewer_background: [u8; 3],
    pub checkerboard_alpha: bool,
    pub smooth_zoom_in: bool,
    pub animate_zoom: bool,
    pub loop_navigation: bool,
    pub preload_neighbors: bool,
    pub cache_budget_mb: u64,
    pub thumb_size: f32,
    pub thumb_disk_cache: bool,
    pub show_thumb_captions: bool,
    pub show_hidden_files: bool,
    pub sort_key: SortKey,
    pub sort_desc: bool,
    pub folders_first: bool,
    pub browser_view: BrowserView,
    pub show_folders_in_grid: bool,
    pub confirm_delete: bool,
    pub jpeg_quality: u8,
    pub keep_exif_on_save: bool,
    pub lossless_jpeg_rotate: bool,
    pub color_manage: bool,
    pub magnifier_zoom: f32,
    pub magnifier_size: f32,
    pub last_folder: Option<PathBuf>,
    pub favorites: Vec<PathBuf>,
    pub recent_folders: Vec<PathBuf>,
    pub tree_width: f32,
    pub preview_height: f32,
    pub show_preview_pane: bool,
    pub show_info_overlay: bool,
    pub slideshow: SlideshowSettings,
    pub window_maximized: bool,
    pub first_run_done: bool,
    pub copy_move_targets: Vec<PathBuf>,
    pub external_editors: Vec<ExternalEditor>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ExternalEditor {
    pub name: String,
    pub exe: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            accent: [86, 156, 255],
            ui_scale: 1.0,
            wheel_action: WheelAction::Navigate,
            fit_mode: FitMode::FitLarge,
            open_viewer_fullscreen: false,
            viewer_background: [14, 14, 16],
            checkerboard_alpha: true,
            smooth_zoom_in: true,
            animate_zoom: true,
            loop_navigation: false,
            preload_neighbors: true,
            cache_budget_mb: 1536,
            thumb_size: 168.0,
            thumb_disk_cache: true,
            show_thumb_captions: true,
            show_hidden_files: false,
            sort_key: SortKey::Name,
            sort_desc: false,
            folders_first: true,
            browser_view: BrowserView::Thumbnails,
            show_folders_in_grid: true,
            confirm_delete: true,
            jpeg_quality: 92,
            keep_exif_on_save: true,
            lossless_jpeg_rotate: true,
            color_manage: true,
            magnifier_zoom: 3.0,
            magnifier_size: 260.0,
            last_folder: None,
            favorites: Vec::new(),
            recent_folders: Vec::new(),
            tree_width: 260.0,
            preview_height: 260.0,
            show_preview_pane: true,
            show_info_overlay: false,
            slideshow: SlideshowSettings::default(),
            window_maximized: true,
            first_run_done: false,
            copy_move_targets: Vec::new(),
            external_editors: Vec::new(),
        }
    }
}

/// Directory holding settings, tags database and caches.
pub fn data_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("portable.txt").exists() {
                return dir.join("LumenData");
            }
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lumen")
}

pub fn cache_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("portable.txt").exists() {
                return dir.join("LumenData").join("cache");
            }
        }
    }
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lumen")
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("settings.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // Write atomically so a crash mid-write never corrupts settings.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn push_recent(&mut self, folder: &std::path::Path) {
        self.recent_folders.retain(|p| p != folder);
        self.recent_folders.insert(0, folder.to_path_buf());
        self.recent_folders.truncate(15);
    }
}
