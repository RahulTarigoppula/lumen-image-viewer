//! Video mode: playback, frame-accurate inspection and A/B comparison, and
//! videos played inline in the viewer and the slideshow (`inline`).
//!
//! The UI-free parts (extension list, frame metrics, scopes) live in
//! `lumen_core::video` and are re-exported here so `crate::video::metrics`
//! and friends keep working. The FFmpeg thumbnailer is registered with
//! `lumen_core::video::set_thumbnailer` at startup (see `main.rs`).

pub use lumen_core::video::{is_video_ext, VIDEO_EXTS};
#[cfg(feature = "video")]
pub use lumen_core::video::{metrics, scopes, timealign, timing};

#[cfg(feature = "video")]
mod align;
#[cfg(feature = "video")]
pub mod analyze;
#[cfg(feature = "video")]
mod audio;
#[cfg(feature = "video")]
pub mod engine;
#[cfg(feature = "video")]
pub mod inline;
#[cfg(feature = "video")]
mod mode;
pub mod playback;
#[cfg(feature = "video")]
mod sync;

#[cfg(feature = "video")]
pub use mode::VideoMode;

/// The file is a video (by its extension).
pub fn is_video_path(path: &std::path::Path) -> bool {
    is_video_ext(&lumen_core::formats::ext_of(path))
}

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
    pub fn run_command(&mut self, _cmd: crate::ui::commands::Command) -> bool {
        false
    }
    pub fn take_compare_request(&mut self) -> Option<Vec<std::path::PathBuf>> {
        None
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut crate::app::Core) -> bool {
        ui.centered_and_justified(|ui| ui.label("This build of Lumen was compiled without video support."));
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}

/// Without the `video` feature (no FFmpeg) videos cannot play: the viewer
/// shows a card for them and the slideshow skips them. Same API as the real
/// `inline` module.
#[cfg(not(feature = "video"))]
#[allow(dead_code)]
pub mod inline {
    use crate::settings::VideoSettings;
    use crate::ui::commands::Command;
    use crate::ui::theme::Palette;
    use egui::{Color32, Rect, Ui};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Height of the transport bar.
    pub const BAR_H: f32 = 40.0;

    const TITLE: &str = "Videos need the full build of Lumen";
    const WHY: &str = "This copy of Lumen was built without its video player.";

    /// Where a player's sound goes (nothing in this build).
    #[derive(Clone)]
    pub struct Sound;

    pub struct SoundOut;

    impl SoundOut {
        pub fn open(_ctx: &egui::Context) -> SoundOut {
            SoundOut
        }
        pub fn shared(&mut self) -> Option<Sound> {
            None
        }
        pub fn set_level(&self, _volume: f32, _muted: bool) {}
    }

    /// A video that cannot play in this build.
    pub struct InlinePlayer {
        path: PathBuf,
        born: Instant,
    }

    impl InlinePlayer {
        pub fn open(path: &Path, _ctx: &egui::Context, _audio: Option<Sound>, _autoplay: bool) -> InlinePlayer {
            InlinePlayer { path: path.to_path_buf(), born: Instant::now() }
        }
        pub fn path(&self) -> &Path {
            &self.path
        }
        pub fn error(&self) -> Option<&str> {
            Some(WHY)
        }
        pub fn has_frame(&self) -> bool {
            false
        }
        pub fn age(&self) -> Duration {
            self.born.elapsed()
        }
        pub fn playing(&self) -> bool {
            false
        }
        pub fn ended(&self) -> bool {
            false
        }
        pub fn display_size(&self) -> Option<(u32, u32)> {
            None
        }
        pub fn duration(&self) -> f64 {
            0.0
        }
        pub fn position(&self) -> f64 {
            0.0
        }
        pub fn tick(&mut self, _ctx: &egui::Context) {}
        pub fn play(&mut self) {}
        pub fn pause(&mut self) {}
        pub fn seek(&mut self, _t: f64, _exact: bool) {}
        pub fn set_audio(&mut self, _audio: Option<Sound>) {}
        pub fn fit_decode(&mut self, _zoom: f32, _ctx: &egui::Context) {}
        pub fn paint(&mut self, _ctx: &egui::Context, _painter: &egui::Painter, _dest: Rect, _nearest: bool, _tint: Color32) -> bool {
            false
        }
    }

    /// The viewer's video: in this build only the card.
    #[derive(Default)]
    pub struct InlineVideo {
        shown: bool,
    }

    impl InlineVideo {
        pub fn show(&mut self, path: Option<&Path>, _ctx: &egui::Context, _s: &VideoSettings) {
            self.shown = path.is_some();
        }
        pub fn close(&mut self) {
            self.shown = false;
        }
        pub fn tick(&mut self, _ctx: &egui::Context, _minimized: bool, _s: &VideoSettings) {}
        pub fn size(&self) -> Option<(u32, u32)> {
            None
        }
        pub fn unavailable(&self) -> Option<(&'static str, String)> {
            self.shown.then(|| (TITLE, WHY.to_string()))
        }
        pub fn playing(&self) -> bool {
            false
        }
        pub fn scrubbing(&self) -> bool {
            false
        }
        pub fn toggle_play(&mut self) {}
        pub fn paint(&mut self, _ctx: &egui::Context, _painter: &egui::Painter, _dest: Rect, _zoom: f32, _nearest: bool) -> bool {
            false
        }
        pub fn command(&mut self, _path: &Path, _cmd: Command, _s: &mut VideoSettings) -> bool {
            false
        }
        pub fn state_text(&self) -> String {
            "no video support in this build".into()
        }
        pub fn transport(&mut self, _ui: &mut Ui, _pal: &Palette, _s: &mut VideoSettings, _view: Rect, _bottom: f32) -> Option<Command> {
            None
        }
    }
}
