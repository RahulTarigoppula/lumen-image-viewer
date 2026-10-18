//! Slideshow mode with transitions. (Placeholder — full implementation pending.)

use crate::app::Core;
use std::path::PathBuf;

pub struct Slideshow {
    pub list: Vec<PathBuf>,
    pub index: usize,
}

impl Slideshow {
    pub fn new(list: Vec<PathBuf>, start: usize, _core: &Core) -> Self {
        Slideshow { list, index: start }
    }

    /// Draw the slideshow. Returns false when the user exits.
    pub fn ui(&mut self, ui: &mut egui::Ui, _core: &mut Core) -> bool {
        ui.label("Slideshow");
        !ui.input(|i| i.key_pressed(egui::Key::Escape))
    }
}
