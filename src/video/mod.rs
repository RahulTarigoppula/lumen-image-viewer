//! Video mode: playback, frame-accurate inspection and A/B comparison.

pub mod metrics;
pub mod scopes;

#[cfg(feature = "video")]
mod analyze;
#[cfg(feature = "video")]
mod audio;
#[cfg(feature = "video")]
pub mod engine;
#[cfg(feature = "video")]
mod mode;

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "flv", "mpg", "mpeg", "m2ts", "mts", "ts", "3gp",
    "3g2", "ogv", "vob", "mxf", "y4m", "divx", "f4v", "hevc", "h264", "h265", "264", "265", "ivf", "av1",
];

pub fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXTS.contains(&ext)
}

#[cfg(feature = "video")]
pub use mode::VideoMode;

/// Thumbnail for a video file (a representative frame).
#[cfg(feature = "video")]
pub fn thumbnail(path: &std::path::Path, max_side: u32) -> anyhow::Result<image::RgbaImage> {
    engine::thumbnail(path, max_side)
}

#[cfg(not(feature = "video"))]
pub fn thumbnail(_path: &std::path::Path, _max_side: u32) -> anyhow::Result<image::RgbaImage> {
    Err(anyhow::anyhow!("This build of Lumen has no video support"))
}

#[cfg(not(feature = "video"))]
pub struct VideoMode;

#[cfg(not(feature = "video"))]
impl VideoMode {
    pub fn new(_paths: Vec<std::path::PathBuf>, _core: &crate::app::Core) -> Self {
        VideoMode
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut crate::app::Core) -> bool {
        ui.centered_and_justified(|ui| ui.label("This build of Lumen was compiled without video support."));
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}
