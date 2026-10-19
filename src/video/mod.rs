//! Video mode: playback, frame-accurate inspection and A/B comparison.
//!
//! The UI-free parts (extension list, frame metrics, scopes) live in
//! `lumen_core::video` and are re-exported here so `crate::video::metrics`
//! and friends keep working. The FFmpeg thumbnailer is registered with
//! `lumen_core::video::set_thumbnailer` at startup (see `main.rs`).

pub use lumen_core::video::{is_video_ext, VIDEO_EXTS};
#[cfg(feature = "video")]
pub use lumen_core::video::{metrics, scopes};

#[cfg(feature = "video")]
pub mod analyze;
#[cfg(feature = "video")]
mod audio;
#[cfg(feature = "video")]
pub mod engine;
#[cfg(feature = "video")]
mod mode;

#[cfg(feature = "video")]
pub use mode::VideoMode;

#[cfg(not(feature = "video"))]
pub struct VideoMode;

#[cfg(not(feature = "video"))]
impl VideoMode {
    pub fn new(_paths: Vec<std::path::PathBuf>, _core: &crate::app::Core) -> Self {
        VideoMode
    }
    pub fn analysis_subject(&self) -> crate::analysis_ui::Subject {
        crate::analysis_ui::Subject { kind: crate::analysis_ui::SubjectKind::Video, a: None, b: None }
    }
    pub fn zoom_to_region(&mut self, _r: [f32; 4], _pane: egui::Vec2, _ppp: f32) {}
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut crate::app::Core) -> bool {
        ui.centered_and_justified(|ui| ui.label("This build of Lumen was compiled without video support."));
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}
