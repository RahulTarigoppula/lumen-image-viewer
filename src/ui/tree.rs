//! Folder navigation sidebar: favorites, quick access and a lazy drive tree.

use super::icons as I;
use super::theme::{self, Palette};
use super::widgets;
use crate::fileops;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, Sense, Stroke, Ui, Vec2};
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
    /// Tree height last frame: after a big change (window resized, panel
    /// moved) the current folder is scrolled into view again.
    last_view_h: f32,
    drives: Vec<(PathBuf, String)>,
    drives_rx: Option<crossbeam_channel::Receiver<Vec<(PathBuf, String)>>>,
    drives_requested: bool,
    quick: Option<Vec<(String, PathBuf, &'static str)>>,
    /// Flattened visible rows, rebuilt only when the tree changes.
    rows: Vec<Row>,
    rows_dirty: bool,
    rows_favs: Vec<PathBuf>,
    /// Deepest row in `rows`, for the indent step.
    max_depth: usize,
    /// Indent per level this frame (narrower than `INDENT` when deep folders
    /// would otherwise push names off the right edge).
    indent: f32,
    /// Path tooltips this frame: off while a window is open (they would draw on top of it).
    tips: bool,
    /// Settings > Show hidden files: list hidden folders, dot-folders and AppData.
    show_hidden: bool,
    /// The tree has the keyboard (F6, or a click in it): the arrows move the
    /// cursor row, Enter opens its folder.
    pub focused: bool,
    /// The keyboard cursor: its row and that row's path (the row is found
    /// again by path when the rows are rebuilt).
    cursor: Option<(usize, PathBuf)>,
    /// Scroll the cursor row into view on the next frame.
    scroll_cursor: bool,
    /// Type-ahead: the letters typed so far and when the last one came (egui time).
    typed: (String, f64),
}

/// What a key does in the tree (see [`tree_key`]).
#[derive(Debug, PartialEq)]
enum TreeMove {
    None,
    /// Put the cursor on this row.
    To(usize),
    /// Expand or collapse this row's folder.
    Toggle(usize),
    /// Open this row's folder.
    Open(usize),
    /// Give the keyboard back to the files.
    Leave,
}

/// Seconds between letters for type-ahead to go on with the same word.
const TYPEAHEAD_SECS: f64 = 0.9;

/// The move for `key` with the cursor on row `cur`: Up / Down, Page Up / Down
/// (`page` rows), Home / End over the folder rows; Right expands a closed
/// folder or steps into an open one; Left collapses an open folder or goes to
/// its parent (Backspace too); Enter / Space open; Esc leaves the tree.
fn tree_key(rows: &[Row], expanded: &HashSet<PathBuf>, cur: usize, key: Key, page: usize) -> TreeMove {
    let items: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::Item { .. })).map(|(i, _)| i).collect();
    let (Some(pos), Some(Row::Item { path, depth, expandable, .. })) = (items.iter().position(|&i| i == cur), rows.get(cur)) else {
        return items.first().map_or(TreeMove::None, |&i| TreeMove::To(i));
    };
    let last = items.len() - 1;
    let open = *expandable && expanded.contains(path);
    match key {
        Key::ArrowUp => TreeMove::To(items[pos.saturating_sub(1)]),
        Key::ArrowDown => TreeMove::To(items[(pos + 1).min(last)]),
        Key::PageUp => TreeMove::To(items[pos.saturating_sub(page)]),
        Key::PageDown => TreeMove::To(items[(pos + page).min(last)]),
        Key::Home => TreeMove::To(items[0]),
        Key::End => TreeMove::To(items[last]),
        Key::ArrowRight if *expandable && !open => TreeMove::Toggle(cur),
        Key::ArrowRight if open => match rows.get(cur + 1) {
            Some(Row::Item { depth: d, .. }) if *d == depth + 1 => TreeMove::To(cur + 1),
            _ => TreeMove::None,
        },
        Key::ArrowLeft if open => TreeMove::Toggle(cur),
        Key::ArrowLeft | Key::Backspace if *depth > 0 => {
            (0..cur).rev().find(|&i| matches!(&rows[i], Row::Item { depth: d, .. } if d + 1 == *depth)).map_or(TreeMove::None, TreeMove::To)
        }
        Key::Enter | Key::Space => TreeMove::Open(cur),
        Key::Escape => TreeMove::Leave,
        _ => TreeMove::None,
    }
}

/// Type-ahead in the tree: the row of the next folder name (lowercase in
/// `names`, `None` for rows that are not folders) that starts with `query`,
/// from the cursor on and wrapping around. A new search (`fresh`) starts
/// after the cursor, so typing the same letter again steps to the next
/// folder with it; so does a run of one letter ("ddd") that no name starts with.
fn typeahead_row(names: &[Option<String>], cur: Option<usize>, query: &str, fresh: bool) -> Option<usize> {
    let n = names.len();
    if n == 0 || query.is_empty() {
        return None;
    }
    let find = |q: &str, start: usize| (0..n).map(|k| (start + k) % n).find(|&i| names[i].as_deref().is_some_and(|s| s.starts_with(q)));
    let after = cur.map_or(0, |c| c + 1) % n;
    let start = if fresh { after } else { cur.unwrap_or(0) % n };
    find(query, start).or_else(|| {
        let first = query.chars().next()?;
        if query.chars().all(|c| c == first) {
            find(&first.to_string(), after)
        } else {
            None
        }
    })
}

const ROW_H: f32 = 26.0;
/// Caption of the drive tree (also where `reveal` looks for the current folder).
const THIS_PC: &str = "This PC";
/// Indent per tree level when there is room.
const INDENT: f32 = 14.0;

/// Indent per level so the deepest row keeps at least ~60% of `width` for its name.
fn indent_step(width: f32, max_depth: usize) -> f32 {
    if max_depth == 0 {
        return INDENT;
    }
    (((width - 46.0) * 0.4).max(0.0) / max_depth as f32).clamp(4.0, INDENT)
}

enum Row {
    Header(&'static str),
    Loading { depth: usize },
    Item { path: PathBuf, name: String, icon: &'static str, depth: usize, expandable: bool, favorite: bool },
}

/// Hidden (0x2) and system (0x4) attributes: Show hidden files lists hidden
/// folders, never protected ones that are both, as Explorer does.
#[cfg(windows)]
fn skip_attrs(attrs: u32, show_hidden: bool) -> bool {
    if show_hidden {
        attrs & 0x6 == 0x6
    } else {
        attrs & 0x6 != 0
    }
}

fn list_subdirs(dir: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                let Ok(mut m) = e.metadata() else { return false };
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    // Check the link itself first: legacy junctions such as
                    // "Application Data" or "My Documents" are hidden+system.
                    if skip_attrs(m.file_attributes(), show_hidden) {
                        return false;
                    }
                }
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
                    if skip_attrs(m.file_attributes(), show_hidden) {
                        return false;
                    }
                }
                // Dot-folders (.cargo, .vscode) and the profile's AppData are
                // hidden too, as in Explorer, unless Show hidden files is on.
                show_hidden || !(e.file_name().to_string_lossy().starts_with('.') || crate::catalog::is_profile_noise(&e.path()))
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
        let show_hidden = self.show_hidden;
        std::thread::spawn(move || {
            let kids = list_subdirs(&d, show_hidden);
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

    /// Expand all ancestors of `path` so the current folder is visible, and
    /// re-list any ancestor whose cached listing does not have the next folder
    /// on the way (it was created after that listing was read).
    pub fn reveal(&mut self, path: &Path) {
        let mut child = path;
        while let Some(parent) = child.parent() {
            self.expanded.insert(parent.to_path_buf());
            if self.children.get(parent).is_some_and(|kids| !kids.iter().any(|k| k == child)) {
                self.invalidate(parent);
            }
            child = parent;
        }
        self.rows_dirty = true;
        if self.revealed_for.as_deref() != Some(path) {
            self.revealed_for = Some(path.to_path_buf());
            self.scroll_to = Some(path.to_path_buf());
        }
    }

    /// Give the tree the keyboard (F6) with the cursor on `current`, the open
    /// folder's row in the drive tree, scrolled into view.
    pub fn focus(&mut self, current: &Path) {
        self.focused = true;
        self.cursor = Some((usize::MAX, current.to_path_buf()));
        self.scroll_cursor = true;
    }

    /// The cursor's row, found again by path after the rows changed: the
    /// nearest row with that path (a row of `usize::MAX` prefers the drive
    /// tree, which comes last). A folder no longer shown falls back to its
    /// nearest shown parent, then to the first folder.
    fn resolve_cursor(&mut self) -> Option<usize> {
        let (i, path) = self.cursor.clone()?;
        if matches!(self.rows.get(i), Some(Row::Item { path: p, .. }) if *p == path) {
            return Some(i);
        }
        let rows = &self.rows;
        let nearest = |want: &Path| {
            rows.iter()
                .enumerate()
                .filter(|(_, r)| matches!(r, Row::Item { path: p, .. } if p == want))
                .min_by_key(|(k, _)| k.abs_diff(i))
                .map(|(k, _)| k)
        };
        let found = nearest(path.as_path()).or_else(|| path.ancestors().skip(1).find_map(nearest)).or_else(|| rows.iter().position(|r| matches!(r, Row::Item { .. })));
        self.cursor = found.and_then(|k| match &self.rows[k] {
            Row::Item { path, .. } => Some((k, path.clone())),
            _ => None,
        });
        found
    }

    fn set_cursor(&mut self, i: usize) {
        if let Some(Row::Item { path, .. }) = self.rows.get(i) {
            self.cursor = Some((i, path.clone()));
            self.scroll_cursor = true;
        }
    }

    fn typeahead_on(&self, now: f64) -> bool {
        !self.typed.0.is_empty() && now - self.typed.1 < TYPEAHEAD_SECS
    }

    /// The tree's keys this frame (see [`tree_key`]; letters jump by name).
    /// Enter on the folder that is already open hands the keyboard to its files.
    fn keys(&mut self, ui: &Ui, current: &Path, favorites: &[PathBuf]) -> Option<TreeAction> {
        let (events, now) = ui.input(|i| (i.events.clone(), i.time));
        let ctx = ui.ctx().clone();
        let mut action = None;
        for e in &events {
            if !self.focused {
                break;
            }
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } if !(modifiers.ctrl || modifiers.command || modifiers.alt) => {
                    // A space inside a type-ahead word belongs to the name (the Text event).
                    if *key == Key::Space && self.typeahead_on(now) {
                        continue;
                    }
                    if self.cursor.is_none() {
                        self.cursor = Some((usize::MAX, current.to_path_buf()));
                    }
                    let Some(cur) = self.resolve_cursor() else { continue };
                    let page = ((self.last_view_h / ROW_H) as usize).saturating_sub(1).max(1);
                    match tree_key(&self.rows, &self.expanded, cur, *key, page) {
                        TreeMove::None => {}
                        TreeMove::To(i) => self.set_cursor(i),
                        TreeMove::Toggle(i) => {
                            if let Some(Row::Item { path, .. }) = self.rows.get(i) {
                                let path = path.clone();
                                self.toggle(&path);
                                // Rows now, so the next key in this frame sees them.
                                self.rebuild_rows(favorites, &ctx);
                                self.scroll_cursor = true;
                            }
                        }
                        TreeMove::Open(i) => {
                            if let Some(Row::Item { path, expandable, .. }) = self.rows.get(i) {
                                let (path, expandable) = (path.clone(), *expandable);
                                if path == current && *key == Key::Enter {
                                    self.focused = false;
                                } else {
                                    if expandable && !self.expanded.contains(&path) {
                                        self.toggle(&path);
                                        self.rebuild_rows(favorites, &ctx);
                                    }
                                    action = Some(TreeAction::Navigate(path));
                                }
                            }
                        }
                        TreeMove::Leave => self.focused = false,
                    }
                }
                egui::Event::Text(t) => {
                    let fresh = !self.typeahead_on(now);
                    // A lone space opens (the Space key above); it starts no search.
                    if fresh && t.trim().is_empty() {
                        continue;
                    }
                    if fresh {
                        self.typed.0.clear();
                    }
                    self.typed.0.push_str(&t.to_lowercase());
                    self.typed.1 = now;
                    if self.cursor.is_none() {
                        self.cursor = Some((usize::MAX, current.to_path_buf()));
                    }
                    let cur = self.resolve_cursor();
                    let names: Vec<Option<String>> = self
                        .rows
                        .iter()
                        .map(|r| match r {
                            Row::Item { name, .. } => Some(name.to_lowercase()),
                            _ => None,
                        })
                        .collect();
                    if let Some(i) = typeahead_row(&names, cur, &self.typed.0, fresh) {
                        self.set_cursor(i);
                    }
                }
                _ => {}
            }
        }
        // The tree owns the plain keys: the files must not see them too (the
        // Enter that hands the keyboard over would also open the focused file).
        let plain = |e: &egui::Event| match e {
            egui::Event::Text(_) => true,
            egui::Event::Key { pressed: true, modifiers, .. } => !(modifiers.ctrl || modifiers.command || modifiers.alt),
            _ => false,
        };
        ctx.input_mut(|i| i.events.retain(|e| !plain(e)));
        action
    }

    /// Re-read the listing of `dir` the next time it is shown (the old one stays
    /// on screen until the new one arrives, so nothing flickers).
    pub fn invalidate(&mut self, dir: &Path) {
        if self.children.contains_key(dir) {
            self.stale.insert(dir.to_path_buf());
            self.rows_dirty = true;
        }
    }

    /// Follow Settings > Show hidden files: a change lists the folders again.
    pub fn set_show_hidden(&mut self, on: bool) {
        if self.show_hidden != on {
            self.show_hidden = on;
            self.refresh();
        }
    }

    /// Re-read every listing and the drive list (F5, and when the window gets
    /// the focus back, since folders may have been created or removed in
    /// Explorer meanwhile). Only expanded folders are listed again right away;
    /// collapsed ones are forgotten and listed when expanded.
    pub fn refresh(&mut self) {
        let expanded = &self.expanded;
        self.children.retain(|dir, _| expanded.contains(dir));
        self.stale.extend(self.children.keys().cloned());
        self.drives_requested = false;
        self.rows_dirty = true;
    }

    /// Rebuild the flat row list (only when something changed).
    fn rebuild_rows(&mut self, favorites: &[PathBuf], ctx: &egui::Context) {
        let mut rows = Vec::new();
        if !favorites.is_empty() {
            rows.push(Row::Header("Favorites"));
            for f in favorites {
                rows.push(Row::Item { path: f.clone(), name: display_name(f), icon: I::STAR, depth: 0, expandable: false, favorite: true });
            }
        }
        rows.push(Row::Header("Quick access"));
        for (name, path, icon) in self.quick.clone().unwrap_or_default() {
            rows.push(Row::Item { path, name, icon, depth: 0, expandable: false, favorite: false });
        }
        rows.push(Row::Header(THIS_PC));
        if self.drives.is_empty() && self.drives_rx.is_some() {
            rows.push(Row::Loading { depth: 0 });
        }
        for (d, label) in self.drives.clone() {
            self.flatten(&mut rows, &d, label, I::HARD_DRIVES, 0, ctx);
        }
        self.max_depth = rows
            .iter()
            .map(|r| match r {
                Row::Item { depth, .. } | Row::Loading { depth } => *depth,
                Row::Header(_) => 0,
            })
            .max()
            .unwrap_or(0);
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
    /// `show_focus`: the tree has the keyboard (its cursor row gets a focus ring);
    /// `keys`: it may read this frame's keys.
    pub fn ui(&mut self, ui: &mut Ui, current: &Path, favorites: &[PathBuf], p: &Palette, show_focus: bool, keys: bool) -> Option<TreeAction> {
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
        let mut action = if keys && self.focused { self.keys(ui, current, favorites) } else { None };
        let cursor_row = if show_focus && self.focused { self.resolve_cursor() } else { None };
        let row_h = ROW_H;
        self.indent = indent_step(ui.available_width(), self.max_depth);
        self.tips = !super::overlay_open(&ctx);
        let view_h = ui.available_height();
        if self.last_view_h > 0.0 && (view_h - self.last_view_h).abs() > 3.0 * row_h && self.scroll_to.is_none() {
            self.scroll_to = self.revealed_for.clone();
        }
        self.last_view_h = view_h;
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("folder-tree");
        if let Some(target) = self.scroll_to.clone() {
            // Look in the drive tree (after the "This PC" header), not in favorites / quick access.
            let start = self.rows.iter().position(|r| matches!(r, Row::Header(THIS_PC))).unwrap_or(0);
            let find = |t: &Path| self.rows[start..].iter().position(|r| matches!(r, Row::Item { path, .. } if path == t)).map(|i| i + start);
            let settled = self.loading.is_empty() && self.drives_rx.is_none() && !self.rows.iter().any(|r| matches!(r, Row::Loading { .. }));
            // Once loading settles, fall back to the deepest visible ancestor (a hidden
            // folder like `.git` on the way is not listed in the tree).
            let found = find(&target).or_else(|| if settled { target.ancestors().skip(1).find_map(find) } else { None });
            match found {
                Some(i) => {
                    // About a third of the way down, on a whole row so the top
                    // row is never cut in half.
                    let view_h = ui.available_height();
                    let rows_above = ((view_h / 3.0) / row_h).floor();
                    scroll = scroll.vertical_scroll_offset(((i as f32 - rows_above).max(0.0) * row_h).floor());
                    self.scroll_to = None;
                }
                // Give up once nothing is loading any more (the target is hidden or gone).
                None if settled => self.scroll_to = None,
                None => {}
            }
        }
        // The keyboard cursor moved: scroll just enough to show its row.
        if self.scroll_cursor {
            if let Some(i) = cursor_row {
                self.scroll_cursor = false;
                let offset = egui::scroll_area::State::load(&ctx, ui.make_persistent_id("folder-tree")).map(|s| s.clamped_offset().y).unwrap_or(0.0);
                let y = i as f32 * row_h;
                if y < offset {
                    scroll = scroll.vertical_scroll_offset(y);
                } else if y + row_h > offset + view_h {
                    scroll = scroll.vertical_scroll_offset(y + row_h - view_h);
                }
            }
        }
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
                        let x = rect.min.x + 12.0 + depth as f32 * self.indent + 22.0;
                        ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, "Loading...", FontId::proportional(12.0), p.faint);
                    }
                    Row::Item { path, name, icon, depth, expandable, favorite } => {
                        if let Some(a) = self.row(ui, i, cursor_row == Some(i), path, name, icon, *depth, current, *expandable, p, *favorite) {
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
        index: usize,
        cursor_here: bool,
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
        let hovered = resp.hovered() && super::pointer_on_layer(ui);
        // The full path shows as a tooltip only when the name does not fit.
        let mut truncated = false;
        if ui.is_rect_visible(rect) {
            let bg = if hovered_drop {
                p.accent.gamma_multiply(0.45)
            } else if selected {
                p.accent_soft
            } else if hovered {
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
            // The keyboard cursor (the tree has the keyboard).
            if cursor_here {
                ui.painter().rect_stroke(r, CornerRadius::same(6), Stroke::new(1.5, p.accent), egui::StrokeKind::Inside);
            }
            let x0 = rect.min.x + 12.0 + depth as f32 * self.indent;
            let cy = rect.center().y;
            if expandable {
                let caret = if self.expanded.contains(path) { I::CARET_DOWN } else { I::CARET_RIGHT };
                ui.painter().text(Pos2::new(x0 + 5.0, cy), Align2::CENTER_CENTER, caret, FontId::proportional(11.0), p.faint);
            }
            let icon_color = if selected { p.accent_text } else if is_favorite { Color32::from_rgb(250, 196, 60) } else { p.muted };
            ui.painter().text(Pos2::new(x0 + 22.0, cy), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), icon_color);
            let text_rect = Rect::from_min_max(Pos2::new(x0 + 34.0, rect.min.y), Pos2::new(rect.max.x - 8.0, rect.max.y));
            let color = if selected { p.text } else { p.text.gamma_multiply(0.92) };
            let font = FontId::proportional(13.0);
            let mut galley = ui.painter().layout_no_wrap(name.to_string(), font.clone(), color);
            if galley.size().x > text_rect.width() && text_rect.width() > 20.0 {
                // Truncate with a plain "..." so long names stay readable (full path in the tooltip).
                truncated = true;
                let chars: Vec<char> = name.chars().collect();
                let (mut lo, mut hi) = (0usize, chars.len());
                while lo < hi {
                    let mid = (lo + hi + 1) / 2;
                    let t: String = chars[..mid].iter().collect::<String>() + "...";
                    if ui.painter().layout_no_wrap(t, font.clone(), color).size().x <= text_rect.width() {
                        lo = mid;
                    } else {
                        hi = mid - 1;
                    }
                }
                galley = ui.painter().layout_no_wrap(chars[..lo].iter().collect::<String>() + "...", font, color);
            }
            let painter = ui.painter().with_clip_rect(text_rect);
            painter.galley(Pos2::new(text_rect.min.x, cy - galley.size().y / 2.0), galley, p.text);
        }
        if let Some(files) = resp.dnd_release_payload::<DragFiles>() {
            let copy = ui.input(|i| i.modifiers.ctrl);
            action = Some(TreeAction::DropFiles { target: path.to_path_buf(), files: files.0.clone(), copy });
        }
        if resp.clicked() || resp.secondary_clicked() {
            // The keyboard follows the click (Up / Down go on from this row).
            self.cursor = Some((index, path.to_path_buf()));
        }
        if resp.clicked() {
            // Click on the caret toggles, elsewhere navigates (and expands).
            let caret_zone = rect.min.x + 12.0 + depth as f32 * self.indent + 12.0;
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
        let resp = if hovered && self.tips && truncated { resp.on_hover_text(path.display().to_string()) } else { resp };
        resp.context_menu(|ui| {
            ui.set_min_width(200.0);
            if widgets::menu_item(ui, I::FOLDER_OPEN, "Open", "").clicked() {
                action = Some(TreeAction::Navigate(path.to_path_buf()));
                ui.close();
            }
            if widgets::menu_item(ui, I::ARROW_SQUARE_OUT, "Show in Explorer", "").clicked() {
                fileops::open_with_default(path);
                ui.close();
            }
            if widgets::menu_item(ui, I::FOLDER_PLUS, "New folder here", "").clicked() {
                action = Some(TreeAction::NewFolderIn(path.to_path_buf()));
                ui.close();
            }
            if is_favorite {
                if widgets::menu_item(ui, I::STAR, "Remove from favorites", "").clicked() {
                    action = Some(TreeAction::RemoveFavorite(path.to_path_buf()));
                    ui.close();
                }
            } else if widgets::menu_item(ui, I::STAR, "Add to favorites", "").clicked() {
                action = Some(TreeAction::AddFavorite(path.to_path_buf()));
                ui.close();
            }
            if widgets::menu_item(ui, I::LINK, "Copy path", "").clicked() {
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

/// A section caption ("Favorites", "This PC"): 11 pt semibold muted, sentence
/// case, sitting low in its row so it reads with the rows below it.
fn header(ui: &mut Ui, p: &Palette, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    ui.painter().text(Pos2::new(rect.min.x + 14.0, rect.max.y - 8.0), Align2::LEFT_CENTER, text, theme::text::caption(), p.muted);
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    fn item(path: &str, depth: usize) -> Row {
        Row::Item { path: PathBuf::from(path), name: display_name(Path::new(path)), icon: I::FOLDER, depth, expandable: true, favorite: false }
    }

    /// Quick access, then C:\ with Photos (open: 2024, 2025) and Work (open, listing), then D:\.
    fn rows() -> (Vec<Row>, HashSet<PathBuf>) {
        let rows = vec![
            Row::Header("Quick access"),
            Row::Item { path: PathBuf::from(r"C:\Users\me\Pictures"), name: "Pictures".into(), icon: I::FOLDER, depth: 0, expandable: false, favorite: false },
            Row::Header(THIS_PC),
            item(r"C:\", 0),
            item(r"C:\Photos", 1),
            item(r"C:\Photos\2024", 2),
            item(r"C:\Photos\2025", 2),
            item(r"C:\Work", 1),
            Row::Loading { depth: 2 },
            item(r"D:\", 0),
        ];
        let open: HashSet<PathBuf> = [r"C:\", r"C:\Photos", r"C:\Work"].into_iter().map(PathBuf::from).collect();
        (rows, open)
    }

    #[test]
    fn arrows_move_over_folder_rows_only() {
        let (r, open) = rows();
        let k = |cur, key| tree_key(&r, &open, cur, key, 3);
        assert_eq!(k(1, Key::ArrowDown), TreeMove::To(3), "headers are skipped");
        assert_eq!(k(3, Key::ArrowUp), TreeMove::To(1));
        assert_eq!(k(1, Key::ArrowUp), TreeMove::To(1), "stays on the first row");
        assert_eq!(k(7, Key::ArrowDown), TreeMove::To(9), "the loading row is skipped");
        assert_eq!(k(9, Key::ArrowDown), TreeMove::To(9));
        assert_eq!(k(4, Key::Home), TreeMove::To(1));
        assert_eq!(k(4, Key::End), TreeMove::To(9));
        assert_eq!(k(1, Key::PageDown), TreeMove::To(5));
        assert_eq!(k(9, Key::PageUp), TreeMove::To(5));
        // A cursor that is not on a folder row starts over at the first one.
        assert_eq!(k(8, Key::ArrowDown), TreeMove::To(1));
    }

    #[test]
    fn left_and_right_fold_and_walk_the_tree() {
        let (r, open) = rows();
        let k = |cur, key| tree_key(&r, &open, cur, key, 3);
        assert_eq!(k(5, Key::ArrowRight), TreeMove::Toggle(5), "a closed folder opens");
        assert_eq!(k(4, Key::ArrowRight), TreeMove::To(5), "an open folder: to its first sub-folder");
        assert_eq!(k(7, Key::ArrowRight), TreeMove::None, "sub-folders still being listed");
        assert_eq!(k(1, Key::ArrowRight), TreeMove::None, "quick access rows do not expand");
        assert_eq!(k(4, Key::ArrowLeft), TreeMove::Toggle(4), "an open folder closes");
        assert_eq!(k(6, Key::ArrowLeft), TreeMove::To(4), "a closed folder: to its parent");
        assert_eq!(k(4, Key::Backspace), TreeMove::To(3), "Backspace goes to the parent even when open");
        assert_eq!(k(9, Key::ArrowLeft), TreeMove::None, "a closed drive has no parent row");
        assert_eq!(k(6, Key::Enter), TreeMove::Open(6));
        assert_eq!(k(6, Key::Space), TreeMove::Open(6));
        assert_eq!(k(6, Key::Escape), TreeMove::Leave);
    }

    #[test]
    fn typeahead_finds_folders_by_their_first_letters() {
        let (r, _) = rows();
        let names: Vec<Option<String>> = r
            .iter()
            .map(|r| match r {
                Row::Item { name, .. } => Some(name.to_lowercase()),
                _ => None,
            })
            .collect();
        assert_eq!(typeahead_row(&names, Some(1), "p", true), Some(4));
        assert_eq!(typeahead_row(&names, Some(4), "p", true), Some(1), "the same letter again steps on, wrapping around");
        assert_eq!(typeahead_row(&names, Some(4), "pi", false), Some(1), "more letters narrow the search");
        assert_eq!(typeahead_row(&names, Some(4), "ph", false), Some(4), "the cursor row still matches");
        assert_eq!(typeahead_row(&names, Some(4), "pp", false), Some(1), "a repeated letter cycles");
        assert_eq!(typeahead_row(&names, Some(3), "2025", true), Some(6));
        assert_eq!(typeahead_row(&names, None, "w", true), Some(7));
        assert_eq!(typeahead_row(&names, Some(3), "x", true), None);
        assert_eq!(typeahead_row(&[], None, "a", true), None);
    }
}
