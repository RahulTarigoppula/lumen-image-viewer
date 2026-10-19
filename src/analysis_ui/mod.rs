//! The Analysis panel shared by the viewer, compare and video modes: a
//! right-side panel with a Metrics tab (local objective metrics) and an AI
//! tab (remote critique against the user's own vLLM server, feature `ai`),
//! the region of interest drawn on the image, and the overlays (ROI, AI
//! finding boxes) drawn over whatever the active mode displays.
//!
//! Integration, kept out of the modes as far as possible:
//! - each mode calls [`canvas::publish`] where it draws an image, so overlays
//!   and the ROI editor know the image-to-screen transform of every pane;
//! - the app builds a [`Subject`] (what is on screen), shows
//!   [`AnalysisPanel::side_panel`] before the mode's central panel and
//!   [`AnalysisPanel::overlay`] after it, then applies the returned
//!   [`PanelAction`]s (zoom the mode's view to a region).

// Without the `ai` feature parts of the subject plumbing (frame info,
// labels) are only used by the AI tab.
#![cfg_attr(not(feature = "ai"), allow(dead_code))]

pub mod adapters;
pub mod canvas;
pub mod heatmap_view;
pub mod metrics_view;
pub mod panel;
pub mod plots;

#[cfg(feature = "ai")]
pub mod ai;
#[cfg(feature = "ai")]
pub mod ai_view;
#[cfg(feature = "ai")]
pub mod send_preview;

pub use panel::{AnalysisPanel, PanelAction};

use image::RgbaImage;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the pixels of a subject image come from.
#[derive(Clone)]
pub enum Pixels {
    /// Decoded, sRGB, as displayed.
    Ready(Arc<RgbaImage>),
    /// A video frame, decoded at native resolution when it is needed.
    VideoFrame { path: PathBuf, pts: f64 },
    /// Still loading (or failed to load).
    Pending,
}

/// A video frame's position, for prompts, history and overlays.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameInfo {
    pub timestamp_s: f64,
    pub index: u64,
}

/// One image the analysis works on.
#[derive(Clone)]
pub struct SubjectImage {
    pub path: PathBuf,
    pub pixels: Pixels,
    /// Pixel size of the image as sent (native size for video frames).
    pub size: Option<(u32, u32)>,
    pub frame: Option<FrameInfo>,
}

impl SubjectImage {
    pub fn file_name(&self) -> String {
        file_name(&self.path)
    }
}

/// Which kind of view the subject comes from; picks the preset list.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SubjectKind {
    Image,
    Pair,
    Video,
    VideoPair,
}

impl SubjectKind {
    pub fn is_video(self) -> bool {
        matches!(self, SubjectKind::Video | SubjectKind::VideoPair)
    }
    pub fn is_pair(self) -> bool {
        matches!(self, SubjectKind::Pair | SubjectKind::VideoPair)
    }
}

/// What the active mode shows: image A, and B in compare views.
#[derive(Clone)]
pub struct Subject {
    pub kind: SubjectKind,
    pub a: Option<SubjectImage>,
    pub b: Option<SubjectImage>,
}

impl Subject {
    /// The viewer's current image.
    pub fn image(path: Option<&Path>, image: Option<Arc<RgbaImage>>) -> Subject {
        let a = path.map(|p| SubjectImage {
            path: p.to_path_buf(),
            size: image.as_ref().map(|i| i.dimensions()),
            pixels: image.map(Pixels::Ready).unwrap_or(Pixels::Pending),
            frame: None,
        });
        Subject { kind: SubjectKind::Image, a, b: None }
    }

    /// Identifies the files (not the frame): results are kept per file key.
    /// Paths are normalized (separators, case), since the same file reaches
    /// the viewer as `C:/x\y.jpg` or `C:\x\y.jpg` depending on how it opened.
    pub fn file_key(&self) -> Option<String> {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
        let a = self.a.as_ref()?;
        Some(match &self.b {
            Some(b) => format!("{}|{}", norm(&a.path), norm(&b.path)),
            None => norm(&a.path),
        })
    }

    /// The frame shown, for video subjects.
    pub fn frame(&self) -> Option<FrameInfo> {
        self.a.as_ref().and_then(|a| a.frame)
    }

    /// Short description for history entries.
    pub fn label(&self) -> String {
        let Some(a) = &self.a else { return String::new() };
        let mut s = match &self.b {
            Some(b) => format!("{} vs {}", a.file_name(), b.file_name()),
            None => a.file_name(),
        };
        if let Some(f) = a.frame {
            s.push_str(&format!(", frame {}", f.index));
        }
        s
    }
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

/// Per-frame upkeep that must run whatever is on screen (AI tasks finish
/// while the panel or the settings window is closed).
pub fn tick(core: &mut crate::app::Core) {
    #[cfg(feature = "ai")]
    ai::tick(core);
    #[cfg(not(feature = "ai"))]
    let _ = core;
}

/// "Sending to 192.168.1.20:8000" while a request is in flight (status bars).
pub fn network_badge(ui: &mut egui::Ui, core: &crate::app::Core) -> bool {
    #[cfg(feature = "ai")]
    {
        if let Some(server) = core.ai.sending_to() {
            let text = format!("{}  Sending to {server}", crate::ui::icons::WIFI_HIGH);
            ui.label(egui::RichText::new(text).size(11.5).color(core.pal.accent))
                .on_hover_text("An AI analysis request is in flight to the server set in Settings > AI. Nothing else leaves this computer.");
            return true;
        }
    }
    #[cfg(not(feature = "ai"))]
    let _ = (ui, core);
    false
}

/// Hover text of the status bar privacy label.
pub fn privacy_note(core: &crate::app::Core) -> &'static str {
    #[cfg(feature = "ai")]
    if core.ai.settings.enabled {
        return "Lumen connects only to the AI server set in Settings > AI, and only when you run an analysis. Map links open in your browser only when you ask. Ratings and tags are stored locally.";
    }
    let _ = core;
    "Lumen itself never connects to the internet; map links open in your browser only when you ask. Ratings and tags are stored locally."
}

/// Dev hook `ai <verb> [args]` for automated UI tests (LUMEN_DEV_DIR only).
/// Returns a line for state.txt and requests for the app.
pub fn dev(panel: &mut AnalysisPanel, core: &mut crate::app::Core, subject: Option<&Subject>, args: &str) -> (String, Vec<PanelAction>) {
    let args = args.trim();
    let (verb, rest) = args.split_once(' ').map(|(v, r)| (v, r.trim())).unwrap_or((args, ""));
    let mut actions = Vec::new();
    match verb {
        "open" => panel.open = true,
        // `ai metrics <verb> [args]`: the Metrics tab (see `MetricsTab::dev`).
        "metrics" => {
            panel.open = true;
            panel.tab = panel::Tab::Metrics;
            return (core.metrics.dev(rest), actions);
        }
        "close" => panel.open = false,
        "settings" => actions.push(PanelAction::OpenSettings("Ai")),
        "roi" => match (rest, subject.and_then(|s| s.a.as_ref()).and_then(|a| a.size)) {
            ("clear", Some(size)) => panel.set_roi(size, None),
            (r, Some(size)) => {
                let v: Vec<u32> = r.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if let [x, y, w, h] = v[..] {
                    panel.set_roi(size, Some((x, y, w, h)));
                }
            }
            _ => {}
        },
        #[cfg(feature = "ai")]
        _ => ai_dev(panel, core, subject, verb, rest, &mut actions),
        #[cfg(not(feature = "ai"))]
        _ => {}
    }
    #[cfg(feature = "ai")]
    let state = {
        let ai = &mut core.ai;
        let phase = match &ai.phase {
            ai::Phase::Idle => "idle",
            ai::Phase::Preparing { .. } => "preparing",
            ai::Phase::Preview { .. } => "preview",
            ai::Phase::Running { .. } => "running",
        };
        let steps: Vec<String> = ai.steps.iter().map(|s| format!("{}:{:?}", s.step.label(), s.status)).collect();
        let (host, port, model, enabled, models, live, error) =
            (ai.settings.host.clone(), ai.settings.port, ai.settings.model.clone(), ai.settings.enabled, ai.models.len(), ai.live.text.len(), ai.error.clone());
        let shown = subject.and_then(|s| s.file_key()).and_then(|k| {
            ai.shown_record(&k).map(|r| match &r.outcome {
                ai::Outcome::Done(res) => {
                    let findings = res.report.as_ref().and_then(|r| r.as_ref().ok()).map(|r| r.findings.len()).unwrap_or(0);
                    format!("done({}, {findings} findings)", r.preset)
                }
                ai::Outcome::Failed { message, .. } => format!("failed({message})"),
                ai::Outcome::Cancelled { partial } => format!("cancelled({} chars)", partial.len()),
            })
        });
        format!(
            "ai: open={} enabled={enabled} host={host:?} port={port} model={model:?} models={models} phase={phase} steps=[{}] live={live} chars error={error:?} shown={} roi={:?}",
            panel.open,
            steps.join(" "),
            shown.unwrap_or_else(|| "none".into()),
            panel.roi_pixels(),
        )
    };
    #[cfg(not(feature = "ai"))]
    let state = {
        let _ = core;
        format!("ai: open={} roi={:?} (no ai feature)", panel.open, panel.roi_pixels())
    };
    (state, actions)
}

#[cfg(feature = "ai")]
fn ai_dev(panel: &mut AnalysisPanel, core: &mut crate::app::Core, subject: Option<&Subject>, verb: &str, rest: &str, actions: &mut Vec<PanelAction>) {
    let ai = &mut core.ai;
    match verb {
        "enable" | "disable" => {
            ai.settings.enabled = verb == "enable";
            ai.dirty = true;
        }
        "host" => {
            ai.host_edit = rest.to_string();
            ai.apply_host();
        }
        "test" => ai.test_connection(),
        "model" => ai.set_model(rest),
        "preset" => {
            if let Some(s) = subject {
                panel.ai.set_preset(s.kind, rest);
            }
        }
        "question" => panel.ai.question = rest.to_string(),
        "run" => panel.request_ai_run(),
        "send" => ai.send_previewed(false),
        "trust" => ai.send_previewed(true),
        "cancel" => ai.cancel(),
        "confirm" => {
            ai.settings.always_confirm = rest != "off";
            ai.dirty = true;
        }
        // Like clicking finding n (1-based) in the list: zoom to its box.
        "finding" => {
            let n: usize = rest.parse().unwrap_or(1);
            let Some(key) = subject.and_then(|s| s.file_key()) else { return };
            let Some(rec) = ai.shown_record(&key) else { return };
            let Some(Ok(report)) = rec.result().and_then(|r| r.report.as_ref()) else { return };
            if let Some(s) = report.findings.get(n.saturating_sub(1)).and_then(|f| f.source) {
                let b = s.label == lumen_ai::ImageLabel::B;
                if let Some((w, h)) = rec.dims[b as usize] {
                    let (w, h) = (w as f32, h as f32);
                    let slot = if b { canvas::Slot::B } else { canvas::Slot::A };
                    actions.push(PanelAction::ZoomTo { slot, rect: [s.x0 as f32 / w, s.y0 as f32 / h, s.x1 as f32 / w, s.y1 as f32 / h] });
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_keys_ignore_separators_and_case() {
        let key = |p: &str| Subject::image(Some(Path::new(p)), None).file_key();
        assert_eq!(key("C:/Pics\\A.jpg"), key("c:\\pics\\a.JPG"));
        assert_ne!(key("C:/Pics/a.jpg"), key("C:/Pics/b.jpg"));
        assert_eq!(Subject::image(None, None).file_key(), None);
    }
}
