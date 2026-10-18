//! Side-by-side image comparison (2–4 images, synced zoom/pan). Placeholder.

use crate::app::Core;
use std::path::PathBuf;

pub struct Compare {
    pub paths: Vec<PathBuf>,
}

impl Compare {
    pub fn new(paths: Vec<PathBuf>, _core: &Core) -> Self {
        Compare { paths }
    }

    /// Returns false when the user exits.
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut Core) -> bool {
        ui.label("Compare");
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}
