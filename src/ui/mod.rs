pub mod browser;
pub mod commands;
pub mod keymap;
#[allow(dead_code)]
pub mod icons;
pub mod palette;
pub mod roi;
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

/// Tooltip delay set by the theme (seconds).
pub const TOOLTIP_DELAY: f32 = 0.35;

/// True while a floating window (Settings, a tool window, Help) is on screen.
/// Modal dialogs need no check: their backdrop already covers everything behind.
pub fn overlay_open(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.areas().visible_layer_ids().iter().any(|l| l.order == egui::Order::Middle))
}

/// True when the pointer is over a window, popup or modal rather than the
/// panels underneath.
pub fn pointer_over_overlay(ctx: &egui::Context) -> bool {
    let pos = ctx.input(|i| i.pointer.hover_pos());
    pos.and_then(|p| ctx.layer_id_at(p)).is_some_and(|l| l.order != egui::Order::Background)
}

/// True when the pointer is over `ui`'s own layer (not over a window or popup
/// above it). Widgets near a window edge can still report hover from egui's
/// hit test; use this to keep their highlight and tooltip away from the window.
pub fn pointer_on_layer(ui: &egui::Ui) -> bool {
    let pos = ui.ctx().input(|i| i.pointer.hover_pos());
    match pos.and_then(|p| ui.ctx().layer_id_at(p)) {
        Some(l) => l == ui.layer_id(),
        None => ui.layer_id().order == egui::Order::Background,
    }
}

/// Call once per frame before any UI: while a floating window is open and the
/// pointer is over the panels behind it, tooltips are off, so tooltips of the
/// browser, viewer or toolbar never draw on top of the window.
pub fn gate_background_tooltips(ctx: &egui::Context) {
    let delay = if overlay_open(ctx) && !pointer_over_overlay(ctx) { f32::INFINITY } else { TOOLTIP_DELAY };
    if ctx.global_style().interaction.tooltip_delay != delay {
        ctx.all_styles_mut(|s| s.interaction.tooltip_delay = delay);
    }
}

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
