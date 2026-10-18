//! Batch convert/resize and batch rename windows. Placeholder.

use crate::app::Core;
use std::path::PathBuf;

pub struct BatchConvert {
    pub files: Vec<PathBuf>,
}

impl BatchConvert {
    pub fn new(files: Vec<PathBuf>, _folder: PathBuf, _core: &Core) -> Self {
        BatchConvert { files }
    }
    /// Returns false when the window was closed.
    pub fn show(&mut self, ctx: &egui::Context, _core: &mut Core) -> bool {
        let mut open = true;
        egui::Window::new("Batch convert").open(&mut open).show(ctx, |ui| {
            ui.label(format!("{} files", self.files.len()));
        });
        open
    }
}

pub struct BatchRename {
    pub files: Vec<PathBuf>,
}

impl BatchRename {
    pub fn new(files: Vec<PathBuf>, _core: &Core) -> Self {
        BatchRename { files }
    }
    /// Returns Some(renamed pairs) when finished, None while open. `open` false = closed.
    pub fn show(&mut self, ctx: &egui::Context, _core: &mut Core) -> (bool, Vec<(PathBuf, PathBuf)>) {
        let mut open = true;
        egui::Window::new("Batch rename").open(&mut open).show(ctx, |ui| {
            ui.label(format!("{} files", self.files.len()));
        });
        (open, Vec::new())
    }
}
