//! Utility windows: contact sheet, duplicate finder, print. Placeholder.

use crate::app::Core;
use std::path::PathBuf;

pub struct ContactSheet {
    pub files: Vec<PathBuf>,
}

impl ContactSheet {
    pub fn new(files: Vec<PathBuf>, _core: &Core) -> Self {
        ContactSheet { files }
    }
    pub fn show(&mut self, ctx: &egui::Context, _core: &mut Core) -> bool {
        let mut open = true;
        egui::Window::new("Contact sheet").open(&mut open).show(ctx, |ui| {
            ui.label(format!("{} files", self.files.len()));
        });
        open
    }
}

pub struct Duplicates {
    pub folder: PathBuf,
}

/// Events from the duplicates window that the app must handle.
pub enum DupEvent {
    Deleted(Vec<PathBuf>),
    Open(PathBuf),
}

impl Duplicates {
    pub fn new(folder: PathBuf, _files: Vec<PathBuf>, _core: &Core) -> Self {
        Duplicates { folder }
    }
    pub fn show(&mut self, ctx: &egui::Context, _core: &mut Core) -> (bool, Vec<DupEvent>) {
        let mut open = true;
        egui::Window::new("Find duplicates").open(&mut open).show(ctx, |ui| {
            ui.label(self.folder.display().to_string());
        });
        (open, Vec::new())
    }
}

/// Print an image (shows the Windows print dialog).
pub fn print_image(_img: &image::RgbaImage, _title: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("Printing is not implemented yet"))
}
