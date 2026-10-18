pub mod browser;
pub mod commands;
pub mod keymap;
#[allow(dead_code)]
pub mod icons;
pub mod palette;
pub mod theme;
pub mod tree;
pub mod viewer;
pub mod widgets;
pub mod batch;
pub mod compare;
pub mod metadata_dialog;
pub mod save_dialog;
pub mod settings_window;
pub mod slideshow;
pub mod tools;

/// Raw (unsmoothed) vertical mouse-wheel movement this frame, in points.
/// Positive = wheel moved up / away from the user.
pub fn wheel_delta(i: &egui::InputState) -> f32 {
    i.raw
        .events
        .iter()
        .map(|e| match e {
            egui::Event::MouseWheel { unit, delta, .. } => match unit {
                egui::MouseWheelUnit::Point => delta.y,
                egui::MouseWheelUnit::Line => delta.y * 40.0,
                egui::MouseWheelUnit::Page => delta.y * 400.0,
            },
            _ => 0.0,
        })
        .sum()
}
