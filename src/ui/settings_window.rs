//! Settings window. Placeholder.

use crate::app::Core;

/// Returns true if visual settings changed (theme/scale/accent) so the app re-applies the theme.
pub fn show(ctx: &egui::Context, core: &mut Core, open: &mut bool) -> bool {
    let mut changed = false;
    egui::Window::new("Settings").open(open).show(ctx, |ui| {
        changed |= ui.checkbox(&mut core.settings.checkerboard_alpha, "Checkerboard behind transparent images").changed();
    });
    changed
}
