//! Video mode: playback, frame-accurate inspection and A/B comparison.

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "flv", "mpg", "mpeg", "m2ts", "mts", "ts", "3gp",
    "3g2", "ogv", "vob", "mxf", "y4m", "divx", "f4v", "hevc", "h264", "h265", "264", "265", "ivf", "av1",
];

pub fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXTS.contains(&ext)
}

use crate::app::Core;
use std::path::PathBuf;

/// Thumbnail for a video file (a representative frame).
pub fn thumbnail(_path: &std::path::Path, _max_side: u32) -> anyhow::Result<image::RgbaImage> {
    Err(anyhow::anyhow!("Video thumbnails are not available yet"))
}

pub struct VideoMode {
    pub paths: Vec<PathBuf>,
}

impl VideoMode {
    pub fn new(paths: Vec<PathBuf>, _core: &Core) -> Self {
        VideoMode { paths }
    }
    /// Returns false when the user leaves video mode.
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut Core) -> bool {
        ui.label("Video mode");
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}
