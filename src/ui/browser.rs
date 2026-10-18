//! Browser mode: folder tree, thumbnail grid / details list, preview pane.

use super::commands::Command;
use super::icons as I;
use super::theme::Palette;
use super::tree::{DragFiles, FolderTree, TreeAction};
use super::widgets;
use crate::app::Core;
use crate::catalog::{self, Entry, EntryKind, MediaFilter};
use crate::settings::{BrowserView, SortKey, ThumbCaption};
use crate::thumbs::{Thumb, ThumbKey};
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub enum BrowserEvent {
    Navigate(PathBuf),
    Open(usize),
    Command(Command),
    DropFiles { target: PathBuf, files: Vec<PathBuf>, copy: bool },
    /// Thumbnails were dragged past the window edge: hand them to Windows drag and drop.
    DragOut(Vec<PathBuf>),
    NewFolderIn(PathBuf),
    AddFavorite(PathBuf),
    RemoveFavorite(PathBuf),
}

pub struct Browser {
    pub folder: PathBuf,
    pub entries: Vec<Entry>,
    /// Display order: indices into `entries` after filtering and sorting.
    pub items: Vec<usize>,
    pub selected: HashSet<PathBuf>,
    pub focus: Option<usize>,
    anchor: Option<usize>,
    pub filter: String,
    pub media_filter: MediaFilter,
    pub flagged_only: bool,
    pub min_rating: u8,
    pub loading: bool,
    pub error: Option<String>,
    list_rx: Option<Receiver<(PathBuf, std::io::Result<Vec<Entry>>)>>,
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
    address: Option<String>,
    pub tree: FolderTree,
    pub scroll_to_focus: bool,
    pub focus_filter: bool,
    dir_mtime: Option<SystemTime>,
    last_poll: Instant,
    typeahead: (String, Instant),
    /// Path to select once the listing arrives (e.g. after opening a file).
    pub pending_select: Option<PathBuf>,
    resort_needed: bool,
    right_w: f32,
    /// Set by the app while a modal dialog or tool window owns the keyboard.
    pub suppress_keys: bool,
}

impl Browser {
    pub fn new(folder: PathBuf) -> Self {
        Browser {
            folder,
            entries: Vec::new(),
            items: Vec::new(),
            selected: HashSet::new(),
            focus: None,
            anchor: None,
            filter: String::new(),
            media_filter: MediaFilter::All,
            flagged_only: false,
            min_rating: 0,
            loading: false,
            error: None,
            list_rx: None,
            back: Vec::new(),
            forward: Vec::new(),
            address: None,
            tree: FolderTree::default(),
            scroll_to_focus: false,
            focus_filter: false,
            dir_mtime: None,
            last_poll: Instant::now(),
            typeahead: (String::new(), Instant::now()),
            pending_select: None,
            resort_needed: false,
            right_w: 600.0,
            suppress_keys: false,
        }
    }

    /// Start listing `folder` in the background.
    pub fn load(&mut self, folder: PathBuf, ctx: &egui::Context, show_hidden: bool) {
        if folder != self.folder {
            self.selected.clear();
            self.focus = None;
            self.anchor = None;
            self.filter.clear();
        }
        self.folder = folder.clone();
        self.loading = true;
        self.error = None;
        self.dir_mtime = std::fs::metadata(&folder).and_then(|m| m.modified()).ok();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let c = ctx.clone();
        std::thread::spawn(move || {
            let res = catalog::list_folder(&folder, show_hidden);
            let _ = tx.send((folder, res));
            c.request_repaint();
        });
        self.list_rx = Some(rx);
        self.tree.reveal(&self.folder.clone());
    }

    pub fn reload(&mut self, ctx: &egui::Context, show_hidden: bool) {
        let keep_focus = self.focused_path();
        self.pending_select = self.pending_select.take().or(keep_focus);
        let f = self.folder.clone();
        let sel = self.selected.clone();
        self.load(f, ctx, show_hidden);
        self.selected = sel;
    }

    pub fn focused_path(&self) -> Option<PathBuf> {
        self.focus.and_then(|i| self.items.get(i)).and_then(|&e| self.entries.get(e)).map(|e| e.path.clone())
    }

    fn anchor_path(&self) -> Option<PathBuf> {
        self.anchor.and_then(|i| self.items.get(i)).and_then(|&e| self.entries.get(e)).map(|e| e.path.clone())
    }

    pub fn item(&self, i: usize) -> Option<&Entry> {
        self.items.get(i).map(|&e| &self.entries[e])
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        // Keep display order.
        self.items
            .iter()
            .map(|&e| &self.entries[e])
            .filter(|e| self.selected.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Selected paths, or the focused one when nothing is selected.
    pub fn targets(&self) -> Vec<PathBuf> {
        let s = self.selected_paths();
        if s.is_empty() {
            self.focused_path().into_iter().collect()
        } else {
            s
        }
    }

    /// Media (non-folder) entries in display order.
    pub fn media_paths(&self) -> Vec<PathBuf> {
        self.items.iter().map(|&e| &self.entries[e]).filter(|e| e.is_media()).map(|e| e.path.clone()).collect()
    }

    /// Like [`Self::targets`] but without folders (uses the listing, no disk access).
    pub fn media_targets(&self) -> Vec<PathBuf> {
        let sel: Vec<&Entry> = self.items.iter().map(|&e| &self.entries[e]).filter(|e| self.selected.contains(&e.path)).collect();
        if sel.is_empty() {
            return self.focus.and_then(|i| self.item(i)).filter(|e| !e.is_folder()).map(|e| e.path.clone()).into_iter().collect();
        }
        sel.into_iter().filter(|e| !e.is_folder()).map(|e| e.path.clone()).collect()
    }

    pub fn image_paths(&self) -> Vec<PathBuf> {
        self.items.iter().map(|&e| &self.entries[e]).filter(|e| e.kind == EntryKind::Image).map(|e| e.path.clone()).collect()
    }

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.items.iter().position(|&e| self.entries[e].path == path)
    }

    pub fn set_focus_path(&mut self, path: &Path) {
        if let Some(i) = self.index_of(path) {
            self.focus = Some(i);
            self.anchor = Some(i);
            self.selected.clear();
            self.selected.insert(path.to_path_buf());
            self.scroll_to_focus = true;
        }
    }

    pub fn mark_resort(&mut self) {
        self.resort_needed = true;
    }

    /// Re-apply sorting and filtering (keeps focus on the same file).
    pub fn rebuild(&mut self, core: &Core) {
        let focus_path = self.focused_path();
        let anchor_path = self.anchor_path();
        let s = &core.settings;
        let dims = |p: &Path| core.meta.get(p).and_then(|m| m.dims);
        let taken = |p: &Path| core.meta.get(p).and_then(|m| m.taken);
        let rating = |p: &Path| core.tags.get(p).rating;
        let flagged = |p: &Path| core.tags.get(p).flagged;
        let label = |p: &Path| core.tags.get(p).label;
        let cx = catalog::SortContext { dims: &dims, taken: &taken, rating: &rating, flagged: &flagged, label: &label };
        catalog::sort_entries(&mut self.entries, s.sort_key, s.sort_desc, s.folders_first, &cx);
        self.items = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                if e.is_folder() {
                    return s.show_folders_in_grid && self.filter.is_empty() && !self.flagged_only && self.min_rating == 0;
                }
                let kind_ok = match self.media_filter {
                    MediaFilter::All => true,
                    MediaFilter::Images => e.kind == EntryKind::Image,
                    MediaFilter::Videos => e.kind == EntryKind::Video,
                };
                if !kind_ok || !catalog::matches_filter(&e.name, &self.filter) {
                    return false;
                }
                let t = core.tags.get(&e.path);
                (!self.flagged_only || t.flagged) && t.rating >= self.min_rating
            })
            .collect();
        self.focus = focus_path.and_then(|p| self.index_of(&p)).or(if self.items.is_empty() { None } else { self.focus.map(|f| f.min(self.items.len() - 1)) });
        // Re-resolve the Shift-selection anchor by path: indices are stale after
        // a re-sort, filter change or reload. Fall back to the focus.
        self.anchor = anchor_path.and_then(|p| self.index_of(&p)).or(self.focus);
        if !self.selected.is_empty() {
            let present: HashSet<&Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
            self.selected.retain(|p| present.contains(p.as_path()));
        }
    }

    /// Called every frame: receive listings, poll for folder changes.
    pub fn tick(&mut self, core: &mut Core) {
        if let Some(rx) = &self.list_rx {
            if let Ok((folder, res)) = rx.try_recv() {
                self.list_rx = None;
                if folder == self.folder {
                    self.loading = false;
                    match res {
                        Ok(entries) => {
                            // Old item indices point into the old entry list:
                            // remember the anchor by path and drop them first.
                            let anchor_path = self.anchor_path();
                            self.items.clear();
                            self.entries = entries;
                            let needs_meta = matches!(core.settings.sort_key, SortKey::Dimensions | SortKey::DateTaken)
                                || core.settings.browser_view == BrowserView::Details;
                            if needs_meta {
                                for e in self.entries.iter().filter(|e| e.kind == EntryKind::Image) {
                                    core.meta.request(&e.path);
                                }
                            }
                            self.rebuild(core);
                            if let Some(p) = self.pending_select.take() {
                                self.set_focus_path(&p);
                            } else if self.focus.is_none() && !self.items.is_empty() {
                                // Focus the first media item so arrow keys work immediately.
                                self.focus = self.items.iter().position(|&e| self.entries[e].is_media()).or(Some(0));
                            }
                            if let Some(i) = anchor_path.and_then(|p| self.index_of(&p)) {
                                self.anchor = Some(i);
                            }
                        }
                        Err(e) => {
                            self.entries.clear();
                            self.items.clear();
                            self.error = Some(format!("Can't open this folder: {e}"));
                        }
                    }
                }
            }
        }
        if self.resort_needed {
            self.resort_needed = false;
            self.rebuild(core);
        }
        // Cheap change detection: a folder's mtime changes when files are added/removed/renamed.
        if !self.loading && self.last_poll.elapsed().as_millis() > 1500 {
            self.last_poll = Instant::now();
            let m = std::fs::metadata(&self.folder).and_then(|m| m.modified()).ok();
            if m.is_some() && m != self.dir_mtime {
                self.reload(&core.ctx, core.settings.show_hidden_files);
            }
        }
    }

    // ------------------------------------------------------------------ UI

    pub fn toolbar(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        let p = core.pal;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back (Alt+←)", false).clicked() {
                ev.push(BrowserEvent::Command(Command::GoBack));
            }
            if widgets::icon_button(ui, I::ARROW_RIGHT, "Forward (Alt+→)", false).clicked() {
                ev.push(BrowserEvent::Command(Command::GoForward));
            }
            if widgets::icon_button(ui, I::ARROW_UP, "Parent folder (Backspace)", false).clicked() {
                ev.push(BrowserEvent::Command(Command::GoUp));
            }
            if widgets::icon_button(ui, I::ARROW_CLOCKWISE, "Refresh (F5)", false).clicked() {
                ev.push(BrowserEvent::Command(Command::Refresh));
            }
            ui.add_space(6.0);

            // Adapt to the room left (points, so UI scale is included): first drop
            // the size slider, then shrink the filter box, then fold the mode
            // buttons into a "..." menu and the sort label into its icon.
            let room = ui.available_width();
            let show_slider = room >= 1050.0;
            let filter_w = if room >= 860.0 { 150.0 } else if room >= 700.0 { 110.0 } else { 80.0 };
            let overflow = room < 800.0;
            let sort_icon_only = room < 640.0;

            // Address bar: breadcrumbs, click to edit.
            let addr_w = (ui.available_width() - self.right_w - 14.0).max(120.0);
            self.address_bar(ui, addr_w, &p, ev);
            ui.add_space(6.0);
            let right_start = ui.cursor().min.x;

            // Filter
            let filter_resp = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Filter (Ctrl+F)", I::FUNNEL))
                    .desired_width(filter_w),
            );
            if self.focus_filter {
                filter_resp.request_focus();
                self.focus_filter = false;
            }
            if filter_resp.changed() {
                self.resort_needed = true;
            }
            if filter_resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                self.filter.clear();
                self.resort_needed = true;
                filter_resp.surrender_focus();
            }

            // Sort menu
            let sort_icon = if core.settings.sort_desc { I::SORT_DESCENDING } else { I::SORT_ASCENDING };
            let sort_label = if sort_icon_only { sort_icon.to_string() } else { format!("{sort_icon} {}", core.settings.sort_key.label()) };
            ui.menu_button(sort_label, |ui| {
                for k in SortKey::ALL {
                    if ui.radio(core.settings.sort_key == k, k.label()).clicked() {
                        core.settings.sort_key = k;
                        if matches!(k, SortKey::Dimensions | SortKey::DateTaken) {
                            for e in self.entries.iter().filter(|e| e.kind == EntryKind::Image) {
                                core.meta.request(&e.path);
                            }
                        }
                        self.resort_needed = true;
                    }
                }
                ui.separator();
                if ui.checkbox(&mut core.settings.sort_desc, "Descending").changed() {
                    self.resort_needed = true;
                }
                if ui.checkbox(&mut core.settings.folders_first, "Folders first").changed() {
                    self.resort_needed = true;
                }
                if ui.checkbox(&mut core.settings.show_folders_in_grid, "Show folders").changed() {
                    self.resort_needed = true;
                }
            });

            // Media type + flag/rating filters
            let filt_icon = if self.flagged_only || self.min_rating > 0 || self.media_filter != MediaFilter::All { I::FUNNEL_SIMPLE } else { I::FUNNEL_SIMPLE_X };
            ui.menu_button(filt_icon, |ui| {
                ui.label(RichText::new("Show").color(p.muted));
                for (f, label) in [(MediaFilter::All, "Images and videos"), (MediaFilter::Images, "Images only"), (MediaFilter::Videos, "Videos only")] {
                    if ui.radio(self.media_filter == f, label).clicked() {
                        self.media_filter = f;
                        self.resort_needed = true;
                    }
                }
                ui.separator();
                if ui.checkbox(&mut self.flagged_only, format!("{} Flagged only", I::FLAG)).changed() {
                    self.resort_needed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Minimum rating");
                    if let Some(r) = widgets::star_rating(ui, self.min_rating, 13.0, &p) {
                        self.min_rating = r;
                        self.resort_needed = true;
                    }
                });
            });

            // Thumbnail size
            if show_slider {
                let mut ts = core.settings.thumb_size;
                ui.add_space(4.0);
                ui.label(RichText::new(I::IMAGE).color(p.muted).size(12.0));
                ui.spacing_mut().slider_width = 90.0;
                if ui.add(egui::Slider::new(&mut ts, 80.0..=384.0).show_value(false)).on_hover_text("Thumbnail size (Ctrl+wheel)").changed() {
                    core.settings.thumb_size = ts;
                }
                ui.label(RichText::new(I::IMAGE).color(p.muted).size(17.0));
            }

            let details = core.settings.browser_view == BrowserView::Details;
            if widgets::icon_button(ui, if details { I::SQUARES_FOUR } else { I::LIST_DASHES }, if details { "Thumbnails view" } else { "Details view" }, false).clicked() {
                ev.push(BrowserEvent::Command(Command::ToggleDetails));
            }
            widgets::separator_v(ui);
            let modes = [
                (I::PLAY_CIRCLE, "Slideshow (S)", Command::Slideshow),
                (I::COLUMNS, "Compare selected (P)", Command::Compare),
                (I::FILM_STRIP, "Video mode", Command::OpenVideoMode),
                (I::STACK, "Batch convert / resize (F3)", Command::BatchConvert),
            ];
            if overflow {
                ui.menu_button(I::DOTS_THREE, |ui| {
                    for (icon, tip, cmd) in modes {
                        if ui.button(format!("{icon}  {tip}")).clicked() {
                            ev.push(BrowserEvent::Command(cmd));
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("More");
            } else {
                for (icon, tip, cmd) in modes {
                    if widgets::icon_button(ui, icon, tip, false).clicked() {
                        ev.push(BrowserEvent::Command(cmd));
                    }
                }
            }
            let used = ui.cursor().min.x - right_start;
            if (used - self.right_w).abs() > 1.0 {
                self.right_w = used;
                ui.ctx().request_repaint();
            }
        });
    }

    fn address_bar(&mut self, ui: &mut Ui, width: f32, p: &Palette, ev: &mut Vec<BrowserEvent>) {
        let h = 28.0;
        if let Some(text) = &mut self.address {
            let r = ui.add_sized([width, h], egui::TextEdit::singleline(text).hint_text("Type a folder path...").vertical_align(egui::Align::Center));
            if !r.has_focus() && !r.lost_focus() {
                r.request_focus();
            }
            if r.lost_focus() {
                // Consume Enter so the grid does not also open the focused item.
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                    let path = PathBuf::from(text.trim().trim_matches('"'));
                    if path.is_dir() {
                        ev.push(BrowserEvent::Navigate(path));
                    } else if path.is_file() {
                        if let Some(parent) = path.parent() {
                            self.pending_select = Some(path.clone());
                            ev.push(BrowserEvent::Navigate(parent.to_path_buf()));
                        }
                    }
                }
                self.address = None;
            }
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, h), Sense::click());
        ui.painter().rect(rect, CornerRadius::same(6), ui.visuals().extreme_bg_color, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        // Breadcrumb segments
        let mut segs: Vec<PathBuf> = self.folder.ancestors().map(|a| a.to_path_buf()).collect();
        segs.reverse();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(6.0, 2.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(rect.shrink(1.0));
        child.spacing_mut().item_spacing.x = 0.0;
        child.label(RichText::new(I::FOLDER).color(p.muted));
        child.add_space(4.0);
        // If too long, show only the trailing segments.
        let total_len: usize = segs.iter().map(|s| seg_name(s).len() + 3).sum();
        let max_chars = (width / 7.5) as usize;
        let mut skip = 0;
        if total_len > max_chars {
            let mut len = total_len;
            while skip + 1 < segs.len() && len > max_chars {
                len -= seg_name(&segs[skip]).len() + 3;
                skip += 1;
            }
            child.label(RichText::new("...").color(p.faint));
        }
        for (i, s) in segs.iter().enumerate().skip(skip) {
            if i > skip || skip > 0 {
                child.label(RichText::new(format!(" {} ", I::CARET_RIGHT)).color(p.faint).size(10.0));
            }
            let last = i == segs.len() - 1;
            let b = child.add(egui::Button::new(RichText::new(seg_name(s)).color(if last { p.text } else { p.muted })).frame(false).small());
            if b.clicked() && !last {
                ev.push(BrowserEvent::Navigate(s.clone()));
            }
        }
        if resp.clicked() {
            self.address = Some(self.folder.display().to_string());
        }
        resp.on_hover_text("Click to type a path");
    }

    pub fn sidebar(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        let p = core.pal;
        let favs = core.settings.favorites.clone();
        let show_preview = core.settings.show_preview_pane;
        let mut tree_bottom: Option<f32> = None;
        if show_preview {
            // Never let the preview take more than ~60% of a short sidebar.
            let max_h = (ui.available_height() * 0.6).clamp(140.0, 640.0);
            let preview_h = core.settings.preview_height.clamp(140.0, max_h);
            let r = egui::Panel::bottom("preview-pane")
                .resizable(true)
                .default_size(preview_h)
                .size_range(140.0..=max_h)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 8)))
                .show(ui, |ui| {
                    // Content must never ask for more than the panel has, or a
                    // resizable panel grows a little every frame.
                    let h = ui.available_height();
                    ui.set_max_height(h);
                    self.preview_pane(ui, core);
                });
            tree_bottom = Some(r.response.rect.min.y);
        }
        // The tree gets exactly the space above the preview pane (a bottom panel
        // only moves the cursor, which a scroll area would ignore).
        let mut rect = ui.available_rect_before_wrap();
        if let Some(b) = tree_bottom {
            rect.max.y = rect.max.y.min(b - 2.0).max(rect.min.y);
        }
        let mut tree_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
        tree_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
        let tree_action = self.tree.ui(&mut tree_ui, &self.folder.clone(), &favs, &p);
        ui.advance_cursor_after_rect(rect);
        if let Some(a) = tree_action {
            ev.push(match a {
                TreeAction::Navigate(path) => BrowserEvent::Navigate(path),
                TreeAction::AddFavorite(path) => BrowserEvent::AddFavorite(path),
                TreeAction::RemoveFavorite(path) => BrowserEvent::RemoveFavorite(path),
                TreeAction::NewFolderIn(path) => BrowserEvent::NewFolderIn(path),
                TreeAction::DropFiles { target, files, copy } => BrowserEvent::DropFiles { target, files, copy },
            });
        }
    }

    fn preview_pane(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        let Some(entry) = self.focus.and_then(|i| self.item(i)).cloned() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No file selected").color(p.faint));
            });
            return;
        };
        let avail = ui.available_size();
        let img_h = (avail.y - 118.0).max(60.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(avail.x, img_h), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(8), ui.visuals().extreme_bg_color);
        let mut dims = core.meta.get(&entry.path).and_then(|m| m.dims);
        if entry.is_media() {
            let key = ThumbKey::new(&entry.path, entry.size, entry.modified);
            match core.thumbs.get(&key) {
                Thumb::Ready(t) => {
                    let ts = t.tex.size_vec2();
                    let s = cap_upscale(((rect.width() - 12.0) / ts.x).min((rect.height() - 12.0) / ts.y).min(2.0), ts, t.source_dims);
                    let r = Rect::from_center_size(rect.center(), ts * s);
                    if dims.is_none() {
                        dims = t.source_dims;
                    }
                    let mut mesh = egui::Mesh::with_texture(t.tex.id());
                    mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    ui.painter().add(egui::Shape::mesh(mesh));
                }
                Thumb::Pending => {
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::HOURGLASS, FontId::proportional(22.0), p.faint);
                }
                Thumb::Failed => {
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(28.0), p.faint);
                }
            }
        } else {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::FOLDER, super::theme::icon_fill(54.0), p.accent.gamma_multiply(0.8));
        }
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(&entry.name).strong()).truncate());
        let mut line = Vec::new();
        if let Some((w, h)) = dims {
            line.push(format!("{w} × {h}"));
        }
        if entry.is_media() {
            line.push(catalog::human_size(entry.size));
        }
        line.push(crate::formats::type_name(&entry.ext));
        ui.label(RichText::new(line.join("  ·  ")).color(p.muted).size(12.0));
        ui.label(RichText::new(catalog::format_time(entry.modified)).color(p.faint).size(11.5));
        if entry.is_media() {
            let tag = core.tags.get(&entry.path);
            ui.horizontal(|ui| {
                if let Some(r) = widgets::star_rating(ui, tag.rating, 13.0, &p) {
                    core.tags.update(&entry.path, |t| t.rating = r);
                }
                ui.add_space(6.0);
                let flag_color = if tag.flagged { p.accent } else { p.faint };
                let font = if tag.flagged { super::theme::icon_fill(14.0) } else { FontId::proportional(14.0) };
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, I::FLAG, font, flag_color);
                if resp.on_hover_text("Flag (Space)").clicked() {
                    core.tags.update(&entry.path, |t| t.flagged = !t.flagged);
                }
            });
        }
    }

    pub fn central(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        let p = core.pal;
        // Drop target for files dragged in from Explorer is handled by the app.
        if let Some(err) = &self.error {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(format!("{}  {err}", I::WARNING)).color(p.danger));
            });
            return;
        }
        if self.items.is_empty() {
            ui.centered_and_justified(|ui| {
                if self.loading {
                    ui.spinner();
                } else if !self.filter.is_empty() || self.flagged_only || self.min_rating > 0 {
                    ui.label(RichText::new(format!("{}  No files match the current filter", I::FUNNEL)).color(p.muted));
                } else {
                    ui.label(RichText::new(format!("{}  No images or videos in this folder\n\nDrop files or folders here, or pick a folder on the left.", I::IMAGES)).color(p.muted));
                }
            });
            return;
        }
        match core.settings.browser_view {
            BrowserView::Thumbnails => self.grid(ui, core, ev),
            BrowserView::Details => self.details(ui, core, ev),
        }
        self.drag_out_check(ui, ev);
        self.keyboard(ui, core, ev);
    }

    fn grid(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        let p = core.pal;
        let ts = core.settings.thumb_size.clamp(64.0, 512.0);
        let captions = core.settings.caption();
        let caption_h = match captions {
            ThumbCaption::None => 0.0,
            ThumbCaption::Name => 22.0,
            ThumbCaption::NameDetails | ThumbCaption::NameDate => 36.0,
        };
        let pad = 8.0;
        let cell = Vec2::new(ts + pad * 2.0, ts + pad * 2.0 + caption_h);
        let avail_w = ui.available_width() - 12.0;
        let cols = ((avail_w / cell.x).floor() as usize).max(1);
        // Stretch cells to use the full width evenly.
        let cell = Vec2::new(avail_w / cols as f32, cell.y);
        let rows = self.items.len().div_ceil(cols);
        let n = self.items.len();

        // Ctrl + wheel resizes thumbnails.
        let (ctrl, wheel) = ui.input(|i| (i.modifiers.ctrl, super::wheel_delta(i)));
        if ctrl && wheel != 0.0 && ui.rect_contains_pointer(ui.max_rect()) {
            core.settings.thumb_size = (core.settings.thumb_size * (1.0 + wheel * 0.0015)).clamp(80.0, 384.0);
        }

        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("grid", &self.folder));
        if ctrl {
            scroll = scroll.scroll_source(egui::scroll_area::ScrollSource::SCROLL_BAR | egui::scroll_area::ScrollSource::DRAG);
        }
        if self.scroll_to_focus {
            if let Some(f) = self.focus {
                let row = f / cols;
                let y = row as f32 * (cell.y + ui.spacing().item_spacing.y);
                let id = ui.make_persistent_id(("grid", &self.folder));
                let offset = egui::scroll_area::State::load(ui.ctx(), id).map(|s| s.offset().y).unwrap_or(0.0);
                let view_h = ui.available_height();
                if y < offset {
                    scroll = scroll.vertical_scroll_offset(y);
                } else if y + cell.y > offset + view_h {
                    scroll = scroll.vertical_scroll_offset(y + cell.y - view_h);
                }
            }
            self.scroll_to_focus = false;
        }
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        let mut double: Option<usize> = None;
        let mut ctx_menu_for: Option<usize> = None;
        // Registered before the cells so cells take precedence for clicks.
        let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("grid-bg"), Sense::click());
        scroll.show_rows(ui, cell.y, rows, |ui, range| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            for row in range {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    for c in 0..cols {
                        let i = row * cols + c;
                        if i >= n {
                            break;
                        }
                        let entry = &self.entries[self.items[i]];
                        let (rect, resp) = ui.allocate_exact_size(cell, Sense::click_and_drag());
                        let selected = self.selected.contains(&entry.path);
                        let focused = self.focus == Some(i);
                        self.paint_cell(ui, core, entry, rect, ts, pad, captions, selected, focused, resp.hovered(), &p);
                        if resp.clicked() {
                            clicked = Some((i, ui.input(|inp| inp.modifiers)));
                        }
                        if resp.double_clicked() {
                            double = Some(i);
                        }
                        if resp.secondary_clicked() {
                            ctx_menu_for = Some(i);
                        }
                        if resp.drag_started() {
                            let files = if selected { self.selected_paths() } else { vec![entry.path.clone()] };
                            resp.dnd_set_drag_payload(DragFiles(files));
                        }
                        if entry.is_folder() {
                            if let Some(files) = resp.dnd_release_payload::<DragFiles>() {
                                let copy = ui.input(|inp| inp.modifiers.ctrl);
                                ev.push(BrowserEvent::DropFiles { target: entry.path.clone(), files: files.0.clone(), copy });
                            }
                        }
                        let dims = core.meta.get(&entry.path).and_then(|m| m.dims);
                        let tip_entry = entry;
                        resp.on_hover_ui_at_pointer(|ui| {
                            ui.label(RichText::new(&tip_entry.name).strong());
                            if let Some((w, h)) = dims {
                                ui.label(format!("{w} × {h}"));
                            }
                            if tip_entry.is_media() {
                                ui.label(format!("{}  ·  {}", catalog::human_size(tip_entry.size), catalog::format_time(tip_entry.modified)));
                            }
                        });
                    }
                });
            }
        });
        // Clicking empty space clears the selection.
        if bg.clicked() && clicked.is_none() && !ui.input(|i| i.modifiers.ctrl || i.modifiers.shift) {
            self.selected.clear();
        }
        self.handle_clicks(clicked, double, ev);
        if let Some(i) = ctx_menu_for {
            let path = self.entries[self.items[i]].path.clone();
            if !self.selected.contains(&path) {
                self.selected.clear();
                self.selected.insert(path);
                self.focus = Some(i);
            }
        }
        self.context_menu(ui, &bg, core, ev, ctx_menu_for.is_some());
    }

    /// An in-app file drag (onto tree folders) that leaves the window becomes a
    /// Windows drag, so files can be dropped on Explorer or other programs.
    fn drag_out_check(&mut self, ui: &mut Ui, ev: &mut Vec<BrowserEvent>) {
        let ctx = ui.ctx().clone();
        let Some(files) = egui::DragAndDrop::payload::<DragFiles>(&ctx) else { return };
        let rect = ctx.content_rect();
        let (down, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));
        let outside = match pos {
            Some(p) => !rect.shrink(3.0).contains(p),
            None => true,
        };
        if down && outside && !files.0.is_empty() {
            egui::DragAndDrop::clear_payload(&ctx);
            ev.push(BrowserEvent::DragOut(files.0.clone()));
        }
    }

    fn handle_clicks(&mut self, clicked: Option<(usize, egui::Modifiers)>, double: Option<usize>, ev: &mut Vec<BrowserEvent>) {
        if let Some((i, m)) = clicked {
            let path = self.entries[self.items[i]].path.clone();
            if m.shift {
                let a = clamp_anchor(self.anchor, i, self.items.len());
                if !m.ctrl {
                    self.selected.clear();
                }
                for j in a.min(i)..=a.max(i) {
                    self.selected.insert(self.entries[self.items[j]].path.clone());
                }
            } else if m.ctrl {
                if !self.selected.remove(&path) {
                    self.selected.insert(path);
                }
                self.anchor = Some(i);
            } else {
                self.selected.clear();
                self.selected.insert(path);
                self.anchor = Some(i);
            }
            self.focus = Some(i);
        }
        if let Some(i) = double {
            ev.push(BrowserEvent::Open(i));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_cell(&self, ui: &mut Ui, core: &mut Core, entry: &Entry, rect: Rect, ts: f32, pad: f32, captions: ThumbCaption, selected: bool, focused: bool, hovered: bool, p: &Palette) {
        if !ui.is_rect_visible(rect) {
            return;
        }
        let painter = ui.painter();
        let card = rect.shrink(3.0);
        let bg = if selected { p.accent_soft } else if hovered { p.hover } else { Color32::TRANSPARENT };
        painter.rect_filled(card, CornerRadius::same(8), bg);
        if focused {
            painter.rect_stroke(card, CornerRadius::same(8), Stroke::new(1.5, if selected { p.accent } else { p.faint }), egui::StrokeKind::Inside);
        } else if selected {
            painter.rect_stroke(card, CornerRadius::same(8), Stroke::new(1.0, p.accent.gamma_multiply(0.7)), egui::StrokeKind::Inside);
        }
        let img_area = Rect::from_min_size(Pos2::new(rect.center().x - ts / 2.0, rect.min.y + pad), Vec2::splat(ts));
        let mut dims = None;
        match entry.kind {
            EntryKind::Folder => {
                painter.text(img_area.center(), Align2::CENTER_CENTER, I::FOLDER, super::theme::icon_fill(ts * 0.55), p.accent.gamma_multiply(0.85));
            }
            _ => {
                let key = ThumbKey::new(&entry.path, entry.size, entry.modified);
                match core.thumbs.get(&key) {
                    Thumb::Ready(t) => {
                        dims = t.source_dims;
                        let tsz = t.tex.size_vec2();
                        let s = cap_upscale((ts / tsz.x).min(ts / tsz.y), tsz, t.source_dims);
                        let r = Rect::from_center_size(img_area.center(), tsz * s);
                        // Subtle drop shadow makes thumbnails pop off the background.
                        painter.rect_filled(r.translate(Vec2::new(0.0, 2.0)).expand(1.0), CornerRadius::same(3), Color32::from_black_alpha(if p.dark { 90 } else { 30 }));
                        let mut mesh = egui::Mesh::with_texture(t.tex.id());
                        mesh.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                        painter.add(egui::Shape::mesh(mesh));
                        if entry.kind == EntryKind::Video {
                            painter.circle_filled(r.center(), 16.0, Color32::from_black_alpha(140));
                            painter.text(r.center() + Vec2::new(1.5, 0.0), Align2::CENTER_CENTER, I::PLAY, super::theme::icon_fill(16.0), Color32::WHITE);
                        }
                    }
                    Thumb::Pending => {
                        painter.rect_filled(img_area.shrink(ts * 0.18), CornerRadius::same(6), if p.dark { Color32::from_white_alpha(6) } else { Color32::from_black_alpha(8) });
                    }
                    Thumb::Failed => {
                        let icon = if entry.kind == EntryKind::Video { I::FILM_STRIP } else { I::IMAGE_BROKEN };
                        painter.text(img_area.center(), Align2::CENTER_CENTER, icon, FontId::proportional(ts * 0.3), p.faint);
                        painter.text(img_area.center() + Vec2::new(0.0, ts * 0.25), Align2::CENTER_CENTER, entry.ext.to_uppercase(), FontId::proportional(11.0), p.faint);
                    }
                }
            }
        }
        // Badges: flag, label, rating
        if entry.is_media() {
            let tag = core.tags.get(&entry.path);
            let mut x = rect.min.x + 10.0;
            let y = rect.min.y + 12.0;
            if tag.flagged {
                painter.circle_filled(Pos2::new(x + 6.0, y + 2.0), 10.0, Color32::from_black_alpha(130));
                painter.text(Pos2::new(x + 6.0, y + 2.0), Align2::CENTER_CENTER, I::FLAG, super::theme::icon_fill(12.0), p.accent);
                x += 22.0;
            }
            if tag.label > 0 {
                let c = crate::tags::LABEL_COLORS[tag.label as usize];
                painter.circle_filled(Pos2::new(x + 6.0, y + 2.0), 6.0, Color32::from_rgb(c[0], c[1], c[2]));
            }
            if tag.rating > 0 {
                let stars: String = I::STAR.repeat(tag.rating as usize);
                let pos = Pos2::new(rect.max.x - 10.0, rect.min.y + 13.0);
                let galley = painter.layout_no_wrap(stars, super::theme::icon_fill(10.5), Color32::from_rgb(250, 196, 60));
                let bg_r = Rect::from_min_size(Pos2::new(pos.x - galley.size().x - 4.0, pos.y - galley.size().y / 2.0 - 2.0), galley.size() + Vec2::new(8.0, 4.0));
                painter.rect_filled(bg_r, CornerRadius::same(4), Color32::from_black_alpha(140));
                painter.galley(Pos2::new(bg_r.min.x + 4.0, bg_r.min.y + 2.0), galley, Color32::WHITE);
            }
        }
        if captions != ThumbCaption::None {
            let name_rect = Rect::from_min_max(Pos2::new(rect.min.x + 8.0, img_area.max.y + 4.0), Pos2::new(rect.max.x - 8.0, rect.max.y - 4.0));
            let galley = ui.painter().layout(entry.name.clone(), FontId::proportional(12.0), if selected { p.text } else { p.text.gamma_multiply(0.9) }, name_rect.width());
            let painter = ui.painter().with_clip_rect(Rect::from_min_max(name_rect.min, Pos2::new(name_rect.max.x, name_rect.min.y + 16.0)));
            let first_w = galley.rows.first().map(|r| r.rect().width()).unwrap_or(0.0);
            painter.galley(Pos2::new(name_rect.center().x - first_w / 2.0, name_rect.min.y), galley, p.text);
            if captions == ThumbCaption::Name {
                return;
            }
            let sub = if !entry.is_media() {
                "Folder".into()
            } else if captions == ThumbCaption::NameDate {
                if entry.kind == EntryKind::Image {
                    core.meta.request(&entry.path);
                }
                let taken = core.meta.get(&entry.path).and_then(|m| m.taken);
                match taken.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
                    Some(dt) => dt.naive_utc().format("%Y-%m-%d %H:%M").to_string(),
                    None => catalog::format_time(entry.modified),
                }
            } else {
                let d = dims.or_else(|| core.meta.get(&entry.path).and_then(|m| m.dims));
                let size = catalog::human_size(entry.size);
                match d {
                    Some((w, h)) => {
                        // Narrow cells: drop the file size, then the dimensions.
                        let font = FontId::proportional(10.5);
                        let fits = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), p.faint).size().x <= name_rect.width();
                        let full = format!("{w}×{h}  ·  {size}");
                        let dims_only = format!("{w}×{h}");
                        if fits(&full) {
                            full
                        } else if fits(&dims_only) {
                            dims_only
                        } else {
                            size
                        }
                    }
                    None => size,
                }
            };
            ui.painter().with_clip_rect(name_rect).text(Pos2::new(name_rect.center().x, name_rect.min.y + 24.0), Align2::CENTER_CENTER, sub, FontId::proportional(10.5), p.faint);
        }
    }

    fn details(&mut self, ui: &mut Ui, core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        use egui_extras::{Column, TableBuilder};
        let p = core.pal;
        let n = self.items.len();
        let mut clicked = None;
        let mut double = None;
        let mut ctx_for = None;
        let mut sort_click: Option<SortKey> = None;
        let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("details-bg"), Sense::click());
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click_and_drag())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0))
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(110.0))
            .column(Column::initial(110.0))
            .column(Column::initial(140.0))
            .column(Column::initial(140.0))
            .column(Column::initial(90.0));
        if self.scroll_to_focus {
            if let Some(f) = self.focus {
                table = table.scroll_to_row(f, None);
            }
            self.scroll_to_focus = false;
        }
        let sk = core.settings.sort_key;
        let desc = core.settings.sort_desc;
        table
            .header(26.0, |mut h| {
                let mut head = |h: &mut egui_extras::TableRow, label: &str, key: Option<SortKey>| {
                    h.col(|ui| {
                        let arrow = if key == Some(sk) { if desc { " ▾" } else { " ▴" } } else { "" };
                        let r = ui.add(egui::Button::new(RichText::new(format!("{label}{arrow}")).strong().color(p.muted)).frame(false));
                        if let (true, Some(k)) = (r.clicked(), key) {
                            sort_click = Some(k);
                        }
                    });
                };
                head(&mut h, "", None);
                head(&mut h, "Name", Some(SortKey::Name));
                head(&mut h, "Size", Some(SortKey::Size));
                head(&mut h, "Type", Some(SortKey::Type));
                head(&mut h, "Dimensions", Some(SortKey::Dimensions));
                head(&mut h, "Modified", Some(SortKey::Modified));
                head(&mut h, "Date taken", Some(SortKey::DateTaken));
                head(&mut h, "Rating", Some(SortKey::Rating));
            })
            .body(|body| {
                body.rows(24.0, n, |mut row| {
                    let i = row.index();
                    let entry = self.entries[self.items[i]].clone();
                    let selected = self.selected.contains(&entry.path);
                    row.set_selected(selected);
                    if entry.kind == EntryKind::Image {
                        core.meta.request(&entry.path);
                    }
                    let qm = core.meta.get(&entry.path);
                    row.col(|ui| {
                        let icon = match entry.kind {
                            EntryKind::Folder => I::FOLDER,
                            EntryKind::Video => I::FILM_STRIP,
                            EntryKind::Image => I::IMAGE,
                        };
                        ui.label(RichText::new(icon).color(if entry.is_folder() { p.accent } else { p.muted }));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&entry.name).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        if entry.is_media() {
                            ui.label(RichText::new(catalog::human_size(entry.size)).color(p.muted));
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(if entry.is_folder() { "Folder".into() } else { crate::formats::type_name(&entry.ext) }).color(p.muted));
                    });
                    row.col(|ui| {
                        if let Some((w, h)) = qm.and_then(|m| m.dims) {
                            ui.label(RichText::new(format!("{w} × {h}")).color(p.muted));
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(catalog::format_time(entry.modified)).color(p.muted));
                    });
                    row.col(|ui| {
                        if let Some(t) = qm.and_then(|m| m.taken) {
                            if let Some(dt) = chrono::DateTime::from_timestamp(t, 0) {
                                ui.label(RichText::new(dt.format("%Y-%m-%d %H:%M").to_string()).color(p.muted));
                            }
                        }
                    });
                    row.col(|ui| {
                        let r = core.tags.get(&entry.path).rating;
                        if r > 0 {
                            ui.label(RichText::new(I::STAR.repeat(r as usize)).font(super::theme::icon_fill(11.0)).color(Color32::from_rgb(250, 196, 60)));
                        }
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some((i, resp.ctx.input(|inp| inp.modifiers)));
                    }
                    if resp.double_clicked() {
                        double = Some(i);
                    }
                    if resp.secondary_clicked() {
                        ctx_for = Some(i);
                    }
                    if resp.drag_started() {
                        let files = if selected { self.selected_paths() } else { vec![entry.path.clone()] };
                        resp.dnd_set_drag_payload(DragFiles(files));
                    }
                });
            });
        if let Some(k) = sort_click {
            if core.settings.sort_key == k {
                core.settings.sort_desc = !core.settings.sort_desc;
            } else {
                core.settings.sort_key = k;
                core.settings.sort_desc = false;
            }
            self.resort_needed = true;
        }
        self.handle_clicks(clicked, double, ev);
        if let Some(i) = ctx_for {
            let path = self.entries[self.items[i]].path.clone();
            if !self.selected.contains(&path) {
                self.selected.clear();
                self.selected.insert(path);
                self.focus = Some(i);
            }
        }
        self.context_menu(ui, &bg, core, ev, ctx_for.is_some());
    }

    fn context_menu(&mut self, ui: &mut Ui, bg: &egui::Response, core: &mut Core, ev: &mut Vec<BrowserEvent>, open_now: bool) {
        let popup_id = ui.id().with("item-context");
        if open_now {
            egui::Popup::open_id(ui.ctx(), popup_id);
        }
        let has_sel = !self.selected.is_empty() || self.focus.is_some();
        let p = core.pal;
        let build = |ui: &mut Ui, ev: &mut Vec<BrowserEvent>| {
            let item = |ui: &mut Ui, cmd: Command, ev: &mut Vec<BrowserEvent>| {
                let text = format!("{}   {}", cmd.icon(), cmd.label());
                let r = ui.add(egui::Button::new(text).shortcut_text(RichText::new(cmd.shortcut()).color(p.faint)));
                if r.clicked() {
                    ev.push(BrowserEvent::Command(cmd));
                    ui.close();
                }
            };
            if has_sel {
                item(ui, Command::OpenViewer, ev);
                item(ui, Command::Compare, ev);
                item(ui, Command::Slideshow, ev);
                ui.separator();
                item(ui, Command::CopyFiles, ev);
                item(ui, Command::CopyTo, ev);
                item(ui, Command::MoveTo, ev);
                item(ui, Command::Rename, ev);
                item(ui, Command::Delete, ev);
                ui.separator();
                ui.menu_button(format!("{}   Rate & tag", I::STAR), |ui| {
                    for n in 0..=5 {
                        item(ui, Command::Rate(n), ev);
                    }
                    ui.separator();
                    item(ui, Command::ToggleFlag, ev);
                    for n in 0..=5 {
                        item(ui, Command::Label(n), ev);
                    }
                });
                ui.menu_button(format!("{}   Tools", I::WRENCH), |ui| {
                    item(ui, Command::BatchConvert, ev);
                    item(ui, Command::BatchRename, ev);
                    item(ui, Command::ContactSheet, ev);
                    item(ui, Command::FindDuplicates, ev);
                    item(ui, Command::SetWallpaper, ev);
                    item(ui, Command::OpenWithEditor, ev);
                    ui.separator();
                    item(ui, Command::CreateMultiPage, ev);
                    item(ui, Command::ImageStrip, ev);
                    item(ui, Command::CreateZip, ev);
                    item(ui, Command::Print, ev);
                    ui.separator();
                    item(ui, Command::EditComment, ev);
                    item(ui, Command::RemoveMetadata, ev);
                    item(ui, Command::ChangeDates, ev);
                    item(ui, Command::ExportXmp, ev);
                });
                ui.separator();
                item(ui, Command::ShowInExplorer, ev);
                item(ui, Command::CopyPath, ev);
                item(ui, Command::Properties, ev);
            } else {
                item(ui, Command::PasteFiles, ev);
                item(ui, Command::NewFolder, ev);
                item(ui, Command::Refresh, ev);
                item(ui, Command::SelectAll, ev);
                item(ui, Command::AddFavorite, ev);
            }
        };
        egui::Popup::context_menu(bg).id(popup_id).show(|ui| {
            ui.set_min_width(240.0);
            build(ui, ev);
        });
    }

    fn keyboard(&mut self, ui: &mut Ui, _core: &mut Core, ev: &mut Vec<BrowserEvent>) {
        if self.suppress_keys || ui.ctx().egui_wants_keyboard_input() || self.items.is_empty() {
            return;
        }
        let cols = self.current_cols(ui, _core);
        let n = self.items.len();
        let (mods, keys) = ui.input(|i| (i.modifiers, i.events.clone()));
        let mut moved = None;
        let cur = self.focus.unwrap_or(0).min(n - 1);
        for e in &keys {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if modifiers.alt {
                        continue;
                    }
                    match key {
                        Key::ArrowRight => moved = Some((cur + 1).min(n - 1)),
                        Key::ArrowLeft => moved = Some(cur.saturating_sub(1)),
                        Key::ArrowDown => moved = Some((cur + cols).min(n - 1)),
                        Key::ArrowUp => moved = Some(cur.saturating_sub(cols)),
                        Key::PageDown => moved = Some((cur + cols * 4).min(n - 1)),
                        Key::PageUp => moved = Some(cur.saturating_sub(cols * 4)),
                        Key::Home if !modifiers.ctrl => moved = Some(0),
                        Key::End if !modifiers.ctrl => moved = Some(n - 1),
                        Key::Enter => {
                            if self.focus.is_some() {
                                ev.push(BrowserEvent::Open(cur));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if let Some(i) = moved {
            let path = self.entries[self.items[i]].path.clone();
            if mods.shift {
                let a = clamp_anchor(self.anchor, cur, self.items.len());
                self.selected.clear();
                for j in a.min(i)..=a.max(i) {
                    self.selected.insert(self.entries[self.items[j]].path.clone());
                }
            } else if !mods.ctrl {
                self.selected.clear();
                self.selected.insert(path);
                self.anchor = Some(i);
            }
            self.focus = Some(i);
            self.scroll_to_focus = true;
        }
    }

    /// True within 900 ms of the last type-ahead letter: further letters extend
    /// the search instead of firing single-letter commands.
    pub fn typeahead_active(&self) -> bool {
        !self.typeahead.0.is_empty() && self.typeahead.1.elapsed().as_millis() < 900
    }

    /// Begin a type-ahead search with `c` (called by the app when a letter key
    /// is not bound to a command).
    pub fn typeahead_start(&mut self, c: &str) {
        self.typeahead = (String::new(), Instant::now());
        self.typeahead_push(c);
    }

    /// Extend the type-ahead search (the app routes all text here while it is active).
    pub fn typeahead_push(&mut self, c: &str) {
        self.typeahead.0.push_str(&c.to_lowercase());
        self.typeahead.1 = Instant::now();
        let q = self.typeahead.0.clone();
        if let Some(i) = self.items.iter().position(|&e| self.entries[e].name.to_lowercase().starts_with(&q)) {
            self.focus = Some(i);
            self.selected.clear();
            self.selected.insert(self.entries[self.items[i]].path.clone());
            self.anchor = Some(i);
            self.scroll_to_focus = true;
        }
    }

    fn current_cols(&self, ui: &Ui, core: &Core) -> usize {
        if core.settings.browser_view == BrowserView::Details {
            return 1;
        }
        let ts = core.settings.thumb_size.clamp(64.0, 512.0);
        let cell_w = ts + 16.0;
        (((ui.max_rect().width() - 12.0) / cell_w).floor() as usize).max(1)
    }

    pub fn status_text(&self, core: &Core) -> String {
        let folders = self.items.iter().filter(|&&e| self.entries[e].is_folder()).count();
        let files = self.items.len() - folders;
        let mut s = format!("{files} file{}", if files == 1 { "" } else { "s" });
        if folders > 0 {
            s += &format!(", {folders} folder{}", if folders == 1 { "" } else { "s" });
        }
        if !self.selected.is_empty() {
            let (n, bytes) = self.entries.iter().filter(|e| self.selected.contains(&e.path)).fold((0usize, 0u64), |(n, b), e| (n + 1, b + e.size));
            if n > 0 {
                s += &format!("   ·   {n} selected ({})", catalog::human_size(bytes));
            }
        }
        if core.meta.pending() > 0 {
            s += &format!("   ·   reading metadata... {}", core.meta.pending());
        }
        s
    }
}

fn seg_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().trim_end_matches('\\').to_string())
}

/// Never show a thumbnail larger than twice its source image (a 1x1 or 64x64
/// picture is not blown up to fill the cell). `fit` scales the texture `tex`.
fn cap_upscale(fit: f32, tex: Vec2, source: Option<(u32, u32)>) -> f32 {
    match source {
        Some((w, h)) if w > 0 && h > 0 && tex.x > 0.0 && tex.y > 0.0 => fit.min((2.0 * w as f32 / tex.x).min(2.0 * h as f32 / tex.y)),
        _ => fit,
    }
}

/// The Shift-selection anchor as a valid index into a list of `len` items:
/// a missing or out-of-range anchor (stale after a filter or reload) falls
/// back to `fallback`, itself clamped.
fn clamp_anchor(anchor: Option<usize>, fallback: usize, len: usize) -> usize {
    let last = len.saturating_sub(1);
    match anchor {
        Some(a) if a < len => a,
        _ => fallback.min(last),
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::clamp_anchor;

    #[test]
    fn tiny_thumbnails_are_not_blown_up() {
        use super::{cap_upscale, Vec2};
        // 1x1 source in a 1x1 texture, cell would scale it 200x: capped at 2x.
        assert_eq!(cap_upscale(200.0, Vec2::splat(1.0), Some((1, 1))), 2.0);
        // 64x64 source upscaled into a 256 texture: at most 128 px on screen.
        assert_eq!(cap_upscale(1.0, Vec2::splat(256.0), Some((64, 64))), 0.5);
        // Big sources are unaffected.
        assert_eq!(cap_upscale(0.5, Vec2::splat(256.0), Some((4000, 3000))), 0.5);
        assert_eq!(cap_upscale(3.0, Vec2::splat(10.0), None), 3.0);
    }

    #[test]
    fn anchor_is_clamped_into_range() {
        assert_eq!(clamp_anchor(Some(3), 0, 10), 3);
        assert_eq!(clamp_anchor(Some(10), 2, 10), 2);
        assert_eq!(clamp_anchor(Some(500), 7, 5), 4);
        assert_eq!(clamp_anchor(None, 1, 5), 1);
        assert_eq!(clamp_anchor(None, 9, 0), 0);
    }
}
