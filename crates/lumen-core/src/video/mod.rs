//! UI-free video pieces: the list of video extensions, the pure frame metrics
//! (PSNR/SSIM) and scopes, and a hook for the FFmpeg thumbnailer that the app
//! registers when it is built with the `video` feature.

pub mod metrics;
pub mod scopes;

use std::path::Path;
use std::sync::OnceLock;

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "flv", "mpg", "mpeg", "m2ts", "mts", "ts", "3gp",
    "3g2", "ogv", "vob", "mxf", "y4m", "divx", "f4v", "hevc", "h264", "h265", "264", "265", "ivf", "av1",
];

pub fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXTS.contains(&ext)
}

/// Renders a representative frame of a video file, longest side at most `max_side`.
pub type Thumbnailer = fn(&Path, u32) -> anyhow::Result<image::RgbaImage>;

static THUMBNAILER: OnceLock<Thumbnailer> = OnceLock::new();

/// Install the video thumbnailer (the app does this at startup when it is
/// built with a video engine). Only the first call takes effect.
pub fn set_thumbnailer(f: Thumbnailer) {
    let _ = THUMBNAILER.set(f);
}

/// Thumbnail for a video file (a representative frame).
pub fn thumbnail(path: &Path, max_side: u32) -> anyhow::Result<image::RgbaImage> {
    match THUMBNAILER.get() {
        Some(f) => f(path, max_side),
        None => Err(anyhow::anyhow!("This build of Lumen has no video support")),
    }
}
