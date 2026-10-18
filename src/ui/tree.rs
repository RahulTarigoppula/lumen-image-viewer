//! Folder navigation sidebar: favorites, quick access and a lazy drive tree.

use super::icons as I;
use super::theme::Palette;
use crate::fileops;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Ui, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Payload used when dragging files out of the grid.
#[derive(Clone)]
pub struct DragFiles(pub Vec<PathBuf>);

pub enum TreeAction {
    Navigate(PathBuf),
    AddFavorite(PathBuf),
    RemoveFavorite(PathBuf),
    NewFolderIn(PathBuf),
    DropFiles { target: PathBuf, files: Vec<PathBuf>, copy: bool },
}

#[derive(Default)]
pub struct FolderTree {
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    revealed_for: Option<PathBuf>,
    scroll_to: Option<PathBuf>,
    drives: Vec<(PathBuf, String)>,
}

fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                let Ok(m) = e.metadata() else { return false };
                if !m.is_dir() {
                    return false;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    // Skip hidden + system folders ($Recycle.Bin, System Volume Information, …)
                    if m.file_attributes() & 0x6 != 0 {
                        return false;
                    }
                }
                !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| {
        crate::catalog::natural_cmp(
            &a.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            &b.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        )
    });
    v
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().trim_end_matches('\\').to_string())
}

impl FolderTree {
    pub fn refresh_drives(&mut self) {
        self.drives = fileops::drives()
            .into_iter()
            .map(|d| {
                let label = fileops::volume_label(&d).unwrap_or_else(|| "Local Disk".into());
                let letter = d.to_string_lossy().trim_end_matches('\\').to_string();
                (d, format!("{label} ({letter})"))
            })
            .collect();
    }

    /// Expand all ancestors of `path` so the current folder is visible.
    pub fn reveal(&mut self, path: &Path) {
        if self.revealed_for.as_deref() == Some(path) {
            return;
        }
        self.revealed_for = Some(path.to_path_buf());
        let mut cur = path.parent();
        while let Some(p) = cur {
            self.expanded.insert(p.to_path_buf());
            cur = p.parent();
        }
        self.scroll_to = Some(path.to_path_buf());
    }

    pub fn invalidate(&mut self, dir: &Path) {
        self.children.remove(dir);
    }

    pub fn ui(&mut self, ui: &mut Ui, current: &Path, favorites: &[PathBuf], p: &Palette) -> Option<TreeAction> {
        if self.drives.is_empty() {
            self.refresh_drives();
        }
        let mut action = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if !favorites.is_empty() {
                header(ui, p, "Favorites");
                for f in favorites {
                    let name = display_name(f);
                    if let Some(a) = self.row(ui, f, &name, I::STAR, 0, current, false, p, true) {
                        action = Some(a);
                    }
                }
                ui.add_space(6.0);
            }
            header(ui, p, "Quick access");
            for (name, path, icon) in fileops::quick_access() {
                if let Some(a) = self.row(ui, &path, &name, icon, 0, current, false, p, false) {
                    action = Some(a);
                }
            }
            ui.add_space(6.0);
            header(ui, p, "This PC");
            let drives = self.drives.clone();
            for (d, label) in drives {
                if let Some(a) = self.node(ui, &d, &label, I::HARD_DRIVES, 0, current, p) {
                    action = Some(a);
                }
            }
            ui.add_space(12.0);
        });
        action
    }

    fn node(&mut self, ui: &mut Ui, path: &Path, name: &str, icon: &str, depth: usize, current: &Path, p: &Palette) -> Option<TreeAction> {
        let mut action = self.row(ui, path, name, icon, depth, current, true, p, false);
        if self.expanded.contains(path) {
            let kids = self.children.entry(path.to_path_buf()).or_insert_with(|| list_subdirs(path)).clone();
            for k in kids {
                let n = display_name(&k);
                let is_open = self.expanded.contains(&k);
                let ic = if is_open { I::FOLDER_OPEN } else { I::FOLDER };
                if let Some(a) = self.node(ui, &k, &n, ic, depth + 1, current, p) {
                    action = Some(a);
                }
            }
        }
        action
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        ui: &mut Ui,
        path: &Path,
        name: &str,
        icon: &str,
        depth: usize,
        current: &Path,
        expandable: bool,
        p: &Palette,
        is_favorite: bool,
    ) -> Option<TreeAction> {
        let mut action = None;
        let h = 26.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click_and_drag());
        let selected = path == current;
        let hovered_drop = resp.dnd_hover_payload::<DragFiles>().is_some();
        if ui.is_rect_visible(rect) {
            let bg = if hovered_drop {
                p.accent.gamma_multiply(0.45)
            } else if selected {
                p.accent_soft
            } else if resp.hovered() {
                p.hover
            } else {
                Color32::TRANSPARENT
            };
            let r = rect.shrink2(Vec2::new(6.0, 1.0));
            ui.painter().rect_filled(r, CornerRadius::same(6), bg);
            if selected {
                ui.painter().rect_filled(
                    Rect::from_min_size(Pos2::new(r.min.x, r.min.y + 5.0), Vec2::new(3.0, r.height() - 10.0)),
                    CornerRadius::same(2),
                    p.accent,
                );
            }
            let x0 = rect.min.x + 12.0 + depth as f32 * 14.0;
            let cy = rect.center().y;
            if expandable {
                let caret = if self.expanded.contains(path) { I::CARET_DOWN } else { I::CARET_RIGHT };
                ui.painter().text(Pos2::new(x0 + 5.0, cy), Align2::CENTER_CENTER, caret, FontId::proportional(11.0), p.faint);
            }
            let icon_color = if selected { p.accent } else if is_favorite { Color32::from_rgb(250, 196, 60) } else { p.muted };
            ui.painter().text(Pos2::new(x0 + 22.0, cy), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), icon_color);
            let text_rect = Rect::from_min_max(Pos2::new(x0 + 34.0, rect.min.y), Pos2::new(rect.max.x - 8.0, rect.max.y));
            let galley = ui.painter().layout_no_wrap(name.to_string(), FontId::proportional(13.0), if selected { p.text } else { p.text.gamma_multiply(0.92) });
            let painter = ui.painter().with_clip_rect(text_rect);
            painter.galley(Pos2::new(text_rect.min.x, cy - galley.size().y / 2.0), galley, p.text);
            if self.scroll_to.as_deref() == Some(path) {
                ui.scroll_to_rect(rect, Some(egui::Align::Center));
                self.scroll_to = None;
            }
        }
        if let Some(files) = resp.dnd_release_payload::<DragFiles>() {
            let copy = ui.input(|i| i.modifiers.ctrl);
            action = Some(TreeAction::DropFiles { target: path.to_path_buf(), files: files.0.clone(), copy });
        }
        if resp.clicked() {
            // Click on the caret toggles, elsewhere navigates (and expands).
            let caret_zone = rect.min.x + 12.0 + depth as f32 * 14.0 + 12.0;
            let on_caret = resp.interact_pointer_pos().map(|pos| pos.x < caret_zone).unwrap_or(false);
            if expandable && on_caret {
                self.toggle(path);
            } else {
                if expandable && !self.expanded.contains(path) {
                    self.toggle(path);
                }
                action = Some(TreeAction::Navigate(path.to_path_buf()));
            }
        }
        if resp.double_clicked() && expandable {
            self.toggle(path);
        }
        resp.on_hover_text(path.display().to_string()).context_menu(|ui| {
            if ui.button(format!("{}  Open", I::FOLDER_OPEN)).clicked() {
                action = Some(TreeAction::Navigate(path.to_path_buf()));
                ui.close();
            }
            if ui.button(format!("{}  Show in Explorer", I::ARROW_SQUARE_OUT)).clicked() {
                fileops::open_with_default(path);
                ui.close();
            }
            if ui.button(format!("{}  New folder here", I::FOLDER_PLUS)).clicked() {
                action = Some(TreeAction::NewFolderIn(path.to_path_buf()));
                ui.close();
            }
            if is_favorite {
                if ui.button(format!("{}  Remove from favorites", I::STAR)).clicked() {
                    action = Some(TreeAction::RemoveFavorite(path.to_path_buf()));
                    ui.close();
                }
            } else if ui.button(format!("{}  Add to favorites", I::STAR)).clicked() {
                action = Some(TreeAction::AddFavorite(path.to_path_buf()));
                ui.close();
            }
            if ui.button(format!("{}  Copy path", I::LINK)).clicked() {
                fileops::copy_text(&path.display().to_string());
                ui.close();
            }
        });
        action
    }

    fn toggle(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
            self.children.remove(path); // re-list on expand so it stays fresh
        }
    }
}

fn header(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(egui::RichText::new(text.to_uppercase()).size(10.5).color(p.faint).strong());
    });
    ui.add_space(3.0);
}
