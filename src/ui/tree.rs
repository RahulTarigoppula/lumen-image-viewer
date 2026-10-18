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

type Listing = (PathBuf, Vec<PathBuf>);

/// Sub-folder listings and drive labels are read on background threads: an offline
/// network drive or a slow USB disk must never freeze the UI.
#[derive(Default)]
pub struct FolderTree {
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    /// Listings being read right now.
    loading: HashSet<PathBuf>,
    /// Listings shown but due for a refresh (kept until the new one arrives).
    stale: HashSet<PathBuf>,
    chan: Option<(crossbeam_channel::Sender<Listing>, crossbeam_channel::Receiver<Listing>)>,
    revealed_for: Option<PathBuf>,
    scroll_to: Option<PathBuf>,
    drives: Vec<(PathBuf, String)>,
    drives_rx: Option<crossbeam_channel::Receiver<Vec<(PathBuf, String)>>>,
    drives_requested: bool,
    quick: Option<Vec<(String, PathBuf, &'static str)>>,
    /// Flattened visible rows, rebuilt only when the tree changes.
    rows: Vec<Row>,
    rows_dirty: bool,
    rows_favs: Vec<PathBuf>,
}

const ROW_H: f32 = 26.0;

enum Row {
    Header(&'static str),
    Loading { depth: usize },
    Item { path: PathBuf, name: String, icon: &'static str, depth: usize, expandable: bool, favorite: bool },
}

fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                let Ok(mut m) = e.metadata() else { return false };
                // Follow folder symlinks / junctions (DirEntry::metadata does not on Windows).
                if m.file_type().is_symlink() {
                    match std::fs::metadata(e.path()) {
                        Ok(t) => m = t,
                        Err(_) => return false,
                    }
                }
                if !m.is_dir() {
                    return false;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    // Skip hidden + system folders ($Recycle.Bin, System Volume Information, ...)
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

fn read_drives() -> Vec<(PathBuf, String)> {
    fileops::drives()
        .into_iter()
        .map(|d| {
            let label = fileops::volume_label(&d).unwrap_or_else(|| "Local Disk".into());
            let letter = d.to_string_lossy().trim_end_matches('\\').to_string();
            (d, format!("{label} ({letter})"))
        })
        .collect()
}

impl FolderTree {
    fn start_drives(&mut self, ctx: &egui::Context) {
        if self.drives_requested {
            return;
        }
        self.drives_requested = true;
        let (tx, rx) = crossbeam_channel::bounded(1);
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_drives());
            c.request_repaint();
        });
        self.drives_rx = Some(rx);
    }

    fn start_listing(&mut self, dir: &Path, ctx: &egui::Context) {
        if self.loading.contains(dir) {
            return;
        }
        self.loading.insert(dir.to_path_buf());
        self.stale.remove(dir);
        let tx = self.chan.get_or_insert_with(crossbeam_channel::unbounded).0.clone();
        let d = dir.to_path_buf();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let kids = list_subdirs(&d);
            let _ = tx.send((d, kids));
            c.request_repaint();
        });
    }

    /// Take finished background listings; true when something arrived.
    fn poll(&mut self) -> bool {
        let mut got = false;
        if let Some((_, rx)) = &self.chan {
            while let Ok((dir, kids)) = rx.try_recv() {
                self.loading.remove(&dir);
                self.children.insert(dir, kids);
                got = true;
            }
        }
        if let Some(rx) = &self.drives_rx {
            if let Ok(d) = rx.try_recv() {
                self.drives = d;
                self.drives_rx = None;
                got = true;
            }
        }
        got
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
        self.rows_dirty = true;
    }

    pub fn invalidate(&mut self, dir: &Path) {
        if self.children.contains_key(dir) {
            self.stale.insert(dir.to_path_buf());
            self.rows_dirty = true;
        }
    }

    /// Rebuild the flat row list (only when something changed).
    fn rebuild_rows(&mut self, favorites: &[PathBuf], ctx: &egui::Context) {
        let mut rows = Vec::new();
        if !favorites.is_empty() {
            rows.push(Row::Header("FAVORITES"));
            for f in favorites {
                rows.push(Row::Item { path: f.clone(), name: display_name(f), icon: I::STAR, depth: 0, expandable: false, favorite: true });
            }
        }
        rows.push(Row::Header("QUICK ACCESS"));
        for (name, path, icon) in self.quick.clone().unwrap_or_default() {
            rows.push(Row::Item { path, name, icon, depth: 0, expandable: false, favorite: false });
        }
        rows.push(Row::Header("THIS PC"));
        if self.drives.is_empty() && self.drives_rx.is_some() {
            rows.push(Row::Loading { depth: 0 });
        }
        for (d, label) in self.drives.clone() {
            self.flatten(&mut rows, &d, label, I::HARD_DRIVES, 0, ctx);
        }
        self.rows = rows;
        self.rows_favs = favorites.to_vec();
        self.rows_dirty = false;
    }

    fn flatten(&mut self, rows: &mut Vec<Row>, path: &Path, name: String, icon: &'static str, depth: usize, ctx: &egui::Context) {
        rows.push(Row::Item { path: path.to_path_buf(), name, icon, depth, expandable: true, favorite: false });
        if !self.expanded.contains(path) {
            return;
        }
        let kids = match self.children.get(path).cloned() {
            Some(k) => {
                if self.stale.contains(path) {
                    self.start_listing(path, ctx);
                }
                k
            }
            None => {
                self.start_listing(path, ctx);
                rows.push(Row::Loading { depth: depth + 1 });
                return;
            }
        };
        for k in kids {
            let ic = if self.expanded.contains(&k) { I::FOLDER_OPEN } else { I::FOLDER };
            let n = display_name(&k);
            self.flatten(rows, &k, n, ic, depth + 1, ctx);
        }
    }

    /// Rows are virtualized: the tree is flattened once per change and only the
    /// rows on screen are laid out, so folders with thousands of sub-folders stay cheap.
    pub fn ui(&mut self, ui: &mut Ui, current: &Path, favorites: &[PathBuf], p: &Palette) -> Option<TreeAction> {
        let ctx = ui.ctx().clone();
        if self.poll() {
            self.rows_dirty = true;
        }
        self.start_drives(&ctx);
        if self.quick.is_none() {
            self.quick = Some(fileops::quick_access());
            self.rows_dirty = true;
        }
        if self.rows_dirty || self.rows_favs.as_slice() != favorites {
            self.rebuild_rows(favorites, &ctx);
        }
        let row_h = ROW_H;
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("folder-tree");
        if let Some(target) = self.scroll_to.clone() {
            // Look in the drive tree (after the "This PC" header), not in favorites / quick access.
            let start = self.rows.iter().position(|r| matches!(r, Row::Header("THIS PC"))).unwrap_or(0);
            let find = |t: &Path| self.rows[start..].iter().position(|r| matches!(r, Row::Item { path, .. } if path == t)).map(|i| i + start);
            let settled = self.loading.is_empty() && self.drives_rx.is_none() && !self.rows.iter().any(|r| matches!(r, Row::Loading { .. }));
            // Once loading settles, fall back to the deepest visible ancestor (a hidden
            // folder like `.git` on the way is not listed in the tree).
            let found = find(&target).or_else(|| if settled { target.ancestors().skip(1).find_map(find) } else { None });
            match found {
                Some(i) => {
                    let view_h = ui.available_height();
                    scroll = scroll.vertical_scroll_offset((i as f32 * row_h - view_h / 2.0 + row_h / 2.0).max(0.0));
                    self.scroll_to = None;
                }
                // Give up once nothing is loading any more (the target is hidden or gone).
                None if settled => self.scroll_to = None,
                None => {}
            }
        }
        let mut action = None;
        let rows = std::mem::take(&mut self.rows);
        // show_rows measures rows with the outer item spacing: make it zero there too.
        let prev_spacing = ui.spacing().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let Some(r) = rows.get(i) else { break };
                match r {
                    Row::Header(text) => header(ui, p, text),
                    Row::Loading { depth } => {
                        let depth = *depth;
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
                        let x = rect.min.x + 12.0 + depth as f32 * 14.0 + 22.0;
                        ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, "Loading...", FontId::proportional(12.0), p.faint);
                    }
                    Row::Item { path, name, icon, depth, expandable, favorite } => {
                        if let Some(a) = self.row(ui, path, name, icon, *depth, current, *expandable, p, *favorite) {
                            action = Some(a);
                        }
                    }
                }
            }
        });
        ui.spacing_mut().item_spacing.y = prev_spacing;
        if !self.rows_dirty {
            self.rows = rows;
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
        let h = ROW_H;
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
        let resp = if resp.hovered() { resp.on_hover_text(path.display().to_string()) } else { resp };
        resp.context_menu(|ui| {
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
        self.rows_dirty = true;
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
            self.invalidate(path); // re-list on expand so it stays fresh
        }
    }
}

fn header(ui: &mut Ui, p: &Palette, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    ui.painter().text(Pos2::new(rect.min.x + 14.0, rect.max.y - 8.0), Align2::LEFT_CENTER, text, FontId::proportional(10.5), p.faint);
}
