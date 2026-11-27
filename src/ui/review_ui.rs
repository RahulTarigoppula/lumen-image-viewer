//! Compare mode's review tools: the mark-up strip and drawing on the panes,
//! the Review side panel (comments, notes, details) and the report export
//! dialog. The data lives in [`review::Review`]; this is its UI state.

use super::icons as I;
use super::review::{self, Detail, Mark, Prefs, Review, Shape, COLORS, WIDTHS};
use super::theme::{self, text};
use super::widgets;
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use lumen_core::report;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Rect,
    Ellipse,
    Arrow,
    Pen,
    Highlighter,
    Snip,
}

impl Tool {
    pub const ALL: [Tool; 7] = [Tool::Select, Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Highlighter, Tool::Snip];
    pub fn shape(self) -> Option<Shape> {
        match self {
            Tool::Rect => Some(Shape::Rect),
            Tool::Ellipse => Some(Shape::Ellipse),
            Tool::Arrow => Some(Shape::Arrow),
            Tool::Pen => Some(Shape::Pen),
            Tool::Highlighter => Some(Shape::Highlighter),
            _ => None,
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Tool::Select => I::CURSOR,
            Tool::Rect => I::RECTANGLE,
            Tool::Ellipse => I::CIRCLE,
            Tool::Arrow => I::ARROW_UP_RIGHT,
            Tool::Pen => I::PENCIL_SIMPLE,
            Tool::Highlighter => I::HIGHLIGHTER,
            Tool::Snip => I::SELECTION,
        }
    }
    fn tip(self) -> &'static str {
        match self {
            Tool::Select => "Select: click a mark or detail to pick it, drag to move it, drag empty space to pan",
            Tool::Rect => "Rectangle: drag to box an area (hold Shift for a square)",
            Tool::Ellipse => "Ellipse: drag to circle an area (hold Shift for a circle)",
            Tool::Arrow => "Arrow: drag from the tail to the point of interest",
            Tool::Pen => "Pen: draw freehand",
            Tool::Highlighter => "Highlighter: paint a see-through stroke",
            Tool::Snip => "Detail: drag a region; the same region is marked on every image and becomes a detail page in the report",
        }
    }
}

/// Where pane `i`'s image is on screen this frame.
#[derive(Clone, Copy, Debug)]
pub struct PaneGeo {
    pub dest: Rect,
    pub dims: (u32, u32),
    pub clip: Rect,
}

/// One pane as the panel shows it.
pub struct PaneInfo {
    pub letter: &'static str,
    pub name: String,
    pub color: Color32,
}

/// The full-resolution pixels of a pane, for the export.
pub struct ExportSource {
    pub letter: &'static str,
    pub path: PathBuf,
    pub image: Option<Arc<image::RgbaImage>>,
    pub file_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sel {
    Mark(usize, usize),
    Detail(usize),
}

enum Drag {
    Draw { pane: usize, shape: Shape, pts: Vec<[f32; 2]> },
    /// Corners as fractions of the images.
    Snip { pane: usize, start: [f32; 2], end: [f32; 2] },
    Move { pane: usize, idx: usize, last: [f32; 2], moved: bool },
    MoveDetail { pane: usize, idx: usize, last: [f32; 2], moved: bool },
    Pan,
}

type Snap = (Vec<Vec<Mark>>, Vec<Detail>);

struct ExportForm {
    title: String,
    author: String,
    format: report::Format,
    sections: report::Sections,
    image_info: bool,
    open_after: bool,
}

/// What the panel asked the compare view to do.
#[derive(Default)]
pub struct PanelOut {
    pub zoom_detail: Option<usize>,
    pub close: bool,
}

pub struct ReviewUi {
    pub review: Review,
    pub markup: bool,
    pub panel: bool,
    pub tool: Tool,
    pub color: usize,
    pub width: usize,
    pub sel: Option<Sel>,
    drag: Option<Drag>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    dirty: Option<Instant>,
    dir: PathBuf,
    /// Project folder: review files are keyed on paths relative to it.
    root: Option<PathBuf>,
    pub prefs: Prefs,
    export: Option<ExportForm>,
    job: Option<Receiver<Result<PathBuf, String>>>,
    pub last_export: Option<PathBuf>,
    pub metrics: Option<report::Table>,
    /// A row to scroll into view in the panel (a mark or detail just made).
    focus: Option<Sel>,
    /// Restored from an earlier session (for one toast).
    pub restored: bool,
}

const SAVE_DELAY: Duration = Duration::from_millis(800);

impl ReviewUi {
    pub fn new(paths: &[PathBuf]) -> ReviewUi {
        Self::new_in(paths, review::reviews_dir(), None)
    }

    /// A review stored in `dir`, keyed on paths relative to `root` (projects).
    pub fn new_in(paths: &[PathBuf], dir: PathBuf, root: Option<PathBuf>) -> ReviewUi {
        let review = review::load_in(&dir, root.as_deref(), paths);
        let restored = !review.is_empty();
        ReviewUi {
            review,
            markup: false,
            panel: false,
            tool: Tool::Rect,
            color: 0,
            width: 1,
            sel: None,
            drag: None,
            undo: Vec::new(),
            redo: Vec::new(),
            dirty: None,
            dir,
            root,
            prefs: Prefs::load(),
            export: None,
            job: None,
            last_export: None,
            metrics: None,
            focus: None,
            restored,
        }
    }

    /// The compared files changed (swap, remove): data follows its file.
    pub fn set_paths(&mut self, paths: &[PathBuf]) {
        let same = self.review.images.len() == paths.len() && self.review.images.iter().zip(paths).all(|(a, b)| &a.path == b);
        if !same {
            self.review.align(paths);
            self.sel = None;
            self.drag = None;
            self.undo.clear();
            self.redo.clear();
            self.touch();
        }
    }

    pub fn touch(&mut self) {
        self.dirty.get_or_insert_with(Instant::now);
    }

    fn snap(&self) -> Snap {
        (self.review.images.iter().map(|i| i.marks.clone()).collect(), self.review.details.clone())
    }

    fn restore(&mut self, s: Snap) {
        for (im, m) in self.review.images.iter_mut().zip(s.0) {
            im.marks = m;
        }
        self.review.details = s.1;
        self.sel = None;
        self.touch();
    }

    /// Call before a change that undo should revert.
    fn checkpoint(&mut self) {
        self.undo.push(self.snap());
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snap());
            self.restore(s);
        }
    }
    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snap());
            self.restore(s);
        }
    }

    pub fn delete_selected(&mut self) -> bool {
        match self.sel {
            Some(Sel::Mark(p, i)) if self.review.images.get(p).is_some_and(|im| i < im.marks.len()) => {
                self.checkpoint();
                self.review.images[p].marks.remove(i);
            }
            Some(Sel::Detail(i)) if i < self.review.details.len() => {
                self.checkpoint();
                self.review.details.remove(i);
            }
            _ => return false,
        }
        self.sel = None;
        self.touch();
        true
    }

    pub fn has_marks(&self) -> bool {
        self.review.mark_count() > 0 || !self.review.details.is_empty()
    }

    pub fn clear_all(&mut self) {
        if !self.has_marks() {
            return;
        }
        self.checkpoint();
        for im in &mut self.review.images {
            im.marks.clear();
        }
        self.review.details.clear();
        self.sel = None;
        self.touch();
    }

    pub fn hist_mode(&self) -> crate::edit::HistMode {
        crate::edit::HistMode::from_key(&self.prefs.histogram)
    }
    pub fn set_hist_mode(&mut self, m: crate::edit::HistMode) {
        if self.prefs.histogram != m.key() {
            self.prefs.histogram = m.key().into();
            self.prefs.save();
        }
    }

    /// Save soon after the last change; poll the export.
    pub fn tick(&mut self, ctx: &egui::Context, toasts: &mut widgets::Toasts) {
        if let Some(t) = self.dirty {
            if t.elapsed() >= SAVE_DELAY && self.drag.is_none() {
                self.dirty = None;
                let (dir, root, r) = (self.dir.clone(), self.root.clone(), self.review.clone());
                std::thread::spawn(move || {
                    if let Err(e) = review::save_in(&dir, root.as_deref(), &r) {
                        log::warn!("saving the review failed: {e}");
                    }
                });
            } else {
                ctx.request_repaint_after(SAVE_DELAY);
            }
        }
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    toasts.success(format!("Report saved: {name}"));
                    if self.prefs.open_after {
                        crate::fileops::open_with_default(&path);
                    }
                    self.last_export = Some(path);
                    self.job = None;
                }
                Ok(Err(e)) => {
                    toasts.error(format!("Could not export the report: {e}"));
                    self.job = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    toasts.error("Could not export the report");
                    self.job = None;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(100)),
            }
        }
    }

    /// Write now (leaving compare).
    pub fn save_now(&mut self) {
        if self.dirty.take().is_some() {
            if let Err(e) = review::save_in(&self.dir, self.root.as_deref(), &self.review) {
                log::warn!("saving the review failed: {e}");
            }
        }
    }

    pub fn exporting(&self) -> bool {
        self.job.is_some()
    }

    pub fn dialog_open(&self) -> bool {
        self.export.is_some()
    }

    pub fn open_export(&mut self) {
        if self.job.is_some() {
            return;
        }
        let p = &self.prefs;
        self.export = Some(ExportForm {
            title: if self.review.title.trim().is_empty() { "Comparison review".into() } else { self.review.title.clone() },
            author: p.author.clone(),
            format: p.format(),
            sections: report::Sections { title: p.title_page, overview: p.overview, per_image: p.per_image, details: p.details, metrics: p.metrics },
            image_info: p.image_info,
            open_after: p.open_after,
        });
    }

    // ------------------------------------------------------------ keys

    /// Review keys; returns true when Esc was used here.
    pub fn keys(&mut self, ui: &Ui) -> bool {
        let take = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        if take(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || take(Modifiers::COMMAND, Key::Y) {
            self.redo();
        }
        if take(Modifiers::COMMAND, Key::Z) {
            self.undo();
        }
        if take(Modifiers::COMMAND | Modifiers::SHIFT, Key::E) {
            self.open_export();
        }
        if self.sel.is_some() && (take(Modifiers::NONE, Key::Delete) || take(Modifiers::NONE, Key::Backspace)) {
            self.delete_selected();
        }
        if !egui::Popup::is_any_open(ui.ctx()) && (self.drag.is_some() || self.sel.is_some() || self.markup) && take(Modifiers::NONE, Key::Escape) {
            if self.drag.is_some() {
                self.drag = None;
            } else if self.sel.is_some() {
                self.sel = None;
            } else {
                self.markup = false;
            }
            return true;
        }
        false
    }

    // ------------------------------------------------------------ input

    /// Mark-up input on `resp` (a pane, or the shared Split area) with the
    /// pointer over pane `hover`. Returns false when the drag should pan.
    pub fn input(&mut self, ui: &Ui, resp: &egui::Response, hover: Option<usize>, geos: &[Option<PaneGeo>]) -> bool {
        let pos = ui.input(|i| i.pointer.interact_pos());
        let shift = ui.input(|i| i.modifiers.shift);
        let px = |pane: usize, p: Pos2| geos.get(pane).copied().flatten().map(|g| review::to_image(g.dest, g.dims, p));
        if resp.drag_started_by(egui::PointerButton::Primary) {
            let origin = ui.input(|i| i.pointer.press_origin()).or(pos);
            self.drag = match (hover, origin) {
                // Only on the image itself (outside it the drag pans).
                (Some(pane), Some(o)) if geos.get(pane).copied().flatten().is_some_and(|g| g.dest.intersect(g.clip).contains(o)) => {
                    let g = geos[pane].unwrap();
                    let p = review::clamp_px(review::to_image(g.dest, g.dims, o), g.dims);
                    match self.tool {
                        Tool::Select => match self.hit(pane, o, &g) {
                            Some(Sel::Mark(pn, i)) => {
                                self.sel = Some(Sel::Mark(pn, i));
                                Some(Drag::Move { pane, idx: i, last: p, moved: false })
                            }
                            Some(Sel::Detail(i)) => {
                                self.sel = Some(Sel::Detail(i));
                                Some(Drag::MoveDetail { pane, idx: i, last: p, moved: false })
                            }
                            None => Some(Drag::Pan),
                        },
                        Tool::Snip => {
                            let r = rel_point(p, g.dims);
                            Some(Drag::Snip { pane, start: r, end: r })
                        }
                        t => Some(Drag::Draw { pane, shape: t.shape().unwrap_or(Shape::Rect), pts: vec![p] }),
                    }
                }
                _ => Some(Drag::Pan),
            };
        }
        if let (Some(pos), Some(drag)) = (pos, self.drag.as_mut()) {
            if resp.dragged_by(egui::PointerButton::Primary) {
                match drag {
                    Drag::Draw { pane, shape, pts } => {
                        if let (Some(p), Some(g)) = (px(*pane, pos), geos.get(*pane).copied().flatten()) {
                            let p = review::clamp_px(p, g.dims);
                            if shape.is_freehand() {
                                let min = 2.0 / review::scale_of(g.dest, g.dims);
                                if pts.last().is_none_or(|l| (l[0] - p[0]).hypot(l[1] - p[1]) >= min) {
                                    pts.push(p);
                                }
                            } else {
                                let end = if matches!(shape, Shape::Rect | Shape::Ellipse) { review::constrain(pts[0], p, shift) } else { p };
                                pts.truncate(1);
                                pts.push(end);
                            }
                        }
                    }
                    Drag::Snip { pane, end, .. } => {
                        if let (Some(p), Some(g)) = (px(*pane, pos), geos.get(*pane).copied().flatten()) {
                            *end = rel_point(review::clamp_px(p, g.dims), g.dims);
                        }
                    }
                    Drag::Move { pane, idx, last, moved } => {
                        if let Some(p) = px(*pane, pos) {
                            let d = [p[0] - last[0], p[1] - last[1]];
                            if d != [0.0, 0.0] {
                                if !*moved {
                                    *moved = true;
                                    self.undo.push((self.review.images.iter().map(|i| i.marks.clone()).collect(), self.review.details.clone()));
                                    self.redo.clear();
                                }
                                if let Some(m) = self.review.images.get_mut(*pane).and_then(|im| im.marks.get_mut(*idx)) {
                                    m.translate(d);
                                }
                                *last = p;
                                self.dirty.get_or_insert_with(Instant::now);
                            }
                        }
                    }
                    Drag::MoveDetail { pane, idx, last, moved } => {
                        if let (Some(p), Some(g)) = (px(*pane, pos), geos.get(*pane).copied().flatten()) {
                            let d = [(p[0] - last[0]) / g.dims.0.max(1) as f32, (p[1] - last[1]) / g.dims.1.max(1) as f32];
                            if d != [0.0, 0.0] {
                                if !*moved {
                                    *moved = true;
                                    self.undo.push((self.review.images.iter().map(|i| i.marks.clone()).collect(), self.review.details.clone()));
                                    self.redo.clear();
                                }
                                if let Some(dt) = self.review.details.get_mut(*idx) {
                                    let (w, h) = (dt.rect[2] - dt.rect[0], dt.rect[3] - dt.rect[1]);
                                    let x = (dt.rect[0] + d[0]).clamp(0.0, 1.0 - w);
                                    let y = (dt.rect[1] + d[1]).clamp(0.0, 1.0 - h);
                                    dt.rect = [x, y, x + w, y + h];
                                }
                                *last = p;
                                self.dirty.get_or_insert_with(Instant::now);
                            }
                        }
                    }
                    Drag::Pan => {}
                }
            }
        }
        if resp.drag_stopped() {
            self.finish_drag(geos);
        }
        if resp.clicked() && self.tool == Tool::Select {
            if let (Some(pane), Some(pos)) = (hover, pos) {
                self.sel = geos.get(pane).copied().flatten().and_then(|g| self.hit(pane, pos, &g));
            }
        }
        // Cursor.
        if resp.hovered() {
            let over = hover.zip(ui.input(|i| i.pointer.hover_pos())).and_then(|(pane, p)| geos.get(pane).copied().flatten().and_then(|g| self.hit(pane, p, &g)));
            let icon = match (self.tool, &self.drag) {
                (_, Some(Drag::Pan)) => egui::CursorIcon::Grabbing,
                (_, Some(Drag::Move { .. } | Drag::MoveDetail { .. })) => egui::CursorIcon::Grabbing,
                (Tool::Select, _) if over.is_some() => egui::CursorIcon::Grab,
                (Tool::Select, _) => egui::CursorIcon::Default,
                _ => egui::CursorIcon::Crosshair,
            };
            ui.ctx().set_cursor_icon(icon);
        }
        !matches!(self.drag, Some(Drag::Pan) | None)
    }

    fn finish_drag(&mut self, geos: &[Option<PaneGeo>]) {
        match self.drag.take() {
            Some(Drag::Draw { pane, shape, pts }) => {
                let Some(g) = geos.get(pane).copied().flatten() else { return };
                let width = review::stroke_px(shape, self.width, g.dims);
                let m = Mark::new(shape, pts, COLORS[self.color].1, width);
                // Too small to mean anything on screen (a click).
                let b = m.bounds();
                let span = (b[2] - b[0]).max(b[3] - b[1]) * review::scale_of(g.dest, g.dims);
                if m.is_valid() && span >= 4.0 {
                    self.checkpoint();
                    self.review.images[pane].marks.push(m);
                    let s = Sel::Mark(pane, self.review.images[pane].marks.len() - 1);
                    self.sel = Some(s);
                    self.focus = Some(s);
                    self.touch();
                }
            }
            Some(Drag::Snip { pane, start, end }) => {
                let Some(g) = geos.get(pane).copied().flatten() else { return };
                let rel = review::ordered(start, end);
                let b = review::from_rel(rel, g.dims);
                let s = review::scale_of(g.dest, g.dims);
                if (b[2] - b[0]) * s >= 6.0 && (b[3] - b[1]) * s >= 6.0 {
                    self.checkpoint();
                    let n = self.review.details.len() + 1;
                    self.review.details.push(Detail { rect: rel, title: format!("Detail {n}"), comment: String::new() });
                    let s = Sel::Detail(n - 1);
                    self.sel = Some(s);
                    self.focus = Some(s);
                    self.touch();
                }
            }
            _ => {}
        }
    }

    /// The mark (on `pane`) or detail under screen point `p`.
    fn hit(&self, pane: usize, p: Pos2, g: &PaneGeo) -> Option<Sel> {
        let s = review::scale_of(g.dest, g.dims).max(1e-6);
        let ip = review::to_image(g.dest, g.dims, p);
        let tol = 6.0 / s;
        let marks = &self.review.images.get(pane)?.marks;
        if let Some(i) = marks.iter().rposition(|m| m.hit(ip, tol)) {
            return Some(Sel::Mark(pane, i));
        }
        // A detail by its border (its inside belongs to the image).
        self.review.details.iter().rposition(|d| {
            let r = review::screen_rect(g.dest, g.dims, review::from_rel(d.rect, g.dims));
            r.expand(6.0).contains(p) && !r.shrink(6.0).contains(p)
        })
        .map(Sel::Detail)
    }

    // ------------------------------------------------------------ painting

    /// Marks and details of pane `i`.
    pub fn paint(&self, ui: &Ui, i: usize, g: &PaneGeo) {
        let painter = ui.painter().with_clip_rect(g.clip);
        let s = review::scale_of(g.dest, g.dims);
        let accent = theme::palette(ui.ctx()).accent;
        for (k, d) in self.review.details.iter().enumerate() {
            let r = review::screen_rect(g.dest, g.dims, review::from_rel(d.rect, g.dims));
            let sel = self.sel == Some(Sel::Detail(k));
            dashed_rect(&painter, r, if sel { accent } else { Color32::WHITE });
            let label = if d.title.trim().is_empty() { format!("Detail {}", k + 1) } else { d.title.trim().to_string() };
            let gl = painter.layout_no_wrap(format!("{}  {label}", I::SELECTION), FontId::proportional(11.5), Color32::WHITE);
            let size = gl.size() + Vec2::new(12.0, 6.0);
            let y = if r.min.y - size.y - 3.0 >= g.clip.min.y { r.min.y - size.y - 3.0 } else { r.min.y + 3.0 };
            let lr = Rect::from_min_size(Pos2::new(r.min.x.max(g.clip.min.x + 2.0), y), size);
            painter.rect_filled(lr, CornerRadius::same(5), if sel { accent } else { Color32::from_black_alpha(170) });
            painter.galley(lr.min + Vec2::new(6.0, 3.0), gl, Color32::WHITE);
        }
        if let Some(im) = self.review.images.get(i) {
            for (k, m) in im.marks.iter().enumerate() {
                paint_mark(&painter, g, m);
                if self.sel == Some(Sel::Mark(i, k)) {
                    let b = review::screen_rect(g.dest, g.dims, m.bounds()).expand(m.width * s / 2.0 + 4.0);
                    dashed_rect(&painter, b, accent);
                }
            }
            // Badges on top of every shape.
            for (k, m) in im.marks.iter().enumerate() {
                paint_badge(&painter, g, m, k + 1);
            }
        }
        // The shape being drawn.
        match &self.drag {
            Some(Drag::Draw { pane, shape, pts }) if *pane == i && pts.len() >= 2 => {
                let m = Mark::new(*shape, pts.clone(), COLORS[self.color].1, review::stroke_px(*shape, self.width, g.dims));
                paint_mark(&painter, g, &m);
            }
            Some(Drag::Snip { start, end, .. }) => {
                // Shown on every pane, as the finished detail will be.
                let rel = review::ordered(*start, *end);
                let r = review::screen_rect(g.dest, g.dims, review::from_rel(rel, g.dims));
                dashed_rect(&painter, r, accent);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ strip

    /// The mark-up tool strip. `dims` per pane, for width changes of the
    /// selection. Undo, redo, delete, clear and Done are laid out first, from
    /// the right end, so they always show; the tools take the room left of
    /// them, and when it is short the hint goes first, then the title.
    pub fn strip(&mut self, ui: &mut Ui, p: &theme::Palette, dims: &[Option<(u32, u32)>]) {
        let row = ui.available_rect_before_wrap();
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::right_to_left(egui::Align::Center)));
        right.spacing_mut().item_spacing.x = 4.0;
        if widgets::secondary_button(&mut right, "Done").on_hover_text("Stop marking up; drag pans again  (M or Esc)").clicked() {
            self.markup = false;
            self.drag = None;
        }
        widgets::separator_v(&mut right);
        if right.add_enabled_ui(self.has_marks(), |ui| widgets::icon_button(ui, I::BROOM, "Clear: remove every mark and detail from all images (Ctrl+Z brings them back)", false)).inner.clicked() {
            self.clear_all();
        }
        if right.add_enabled_ui(self.sel.is_some(), |ui| widgets::icon_button(ui, I::TRASH, "Delete the selected mark or detail  (Del)", false)).inner.clicked() {
            self.delete_selected();
        }
        if right.add_enabled_ui(self.can_redo(), |ui| widgets::icon_button(ui, I::ARROW_CLOCKWISE, "Redo  (Ctrl+Y)", false)).inner.clicked() {
            self.redo();
        }
        if right.add_enabled_ui(self.can_undo(), |ui| widgets::icon_button(ui, I::ARROW_COUNTER_CLOCKWISE, "Undo  (Ctrl+Z)", false)).inner.clicked() {
            self.undo();
        }
        let buttons_left = right.min_rect().left();
        // The tools: the title only when they all have their full width beside it.
        let tool_icons: Vec<&str> = Tool::ALL.iter().map(|t| t.icon()).collect();
        let width_names: Vec<&str> = WIDTHS.iter().map(|w| w.0).collect();
        let s = 4.0;
        let sep = 9.0 + s;
        let tools_w = widgets::segmented_width(ui, &tool_icons) + s + sep + COLORS.len() as f32 * (22.0 + s) + sep + widgets::segmented_width(ui, &width_names) + s;
        let title = "Mark up";
        let title_w = ui.painter().layout_no_wrap(title.to_string(), text::body_strong(), p.text).size().x + s + 4.0 + s;
        let room = (buttons_left - 12.0 - row.left()).max(0.0);
        let left_rect = Rect::from_min_max(row.min, Pos2::new(row.left() + room, row.max.y));
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(left_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
        left.set_clip_rect(left_rect.intersect(ui.clip_rect()));
        {
            let ui = &mut left;
            ui.spacing_mut().item_spacing.x = s;
            if title_w + tools_w <= room {
                ui.label(RichText::new(title).font(text::body_strong()).color(p.text));
                ui.add_space(4.0);
            }
            let labels = tool_icons;
            let tips: Vec<&str> = Tool::ALL.iter().map(|t| t.tip()).collect();
            let mut idx = Tool::ALL.iter().position(|t| *t == self.tool).unwrap_or(1);
            if widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) {
                self.tool = Tool::ALL[idx];
                self.drag = None;
            }
            widgets::separator_v(ui);
            for (k, (name, c)) in COLORS.iter().enumerate() {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                let col = Color32::from_rgb(c[0], c[1], c[2]);
                let painter = ui.painter();
                if self.color == k {
                    painter.circle_stroke(r.center(), 10.0, Stroke::new(2.0, p.accent));
                } else if resp.hovered() {
                    painter.circle_stroke(r.center(), 10.0, Stroke::new(1.0, p.muted));
                }
                painter.circle_filled(r.center(), 7.0, col);
                // A light edge keeps black visible on the dark panel.
                let edge = if c.iter().map(|v| *v as u32).sum::<u32>() < 200 { Color32::from_gray(150) } else { Color32::from_black_alpha(90) };
                painter.circle_stroke(r.center(), 7.0, Stroke::new(1.0, edge));
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{name}: colour for new marks (and the selected one)"));
                if resp.clicked() {
                    self.color = k;
                    if let Some(Sel::Mark(pn, i)) = self.sel {
                        if self.review.images.get(pn).and_then(|im| im.marks.get(i)).is_some_and(|m| m.color != *c) {
                            self.checkpoint();
                            self.review.images[pn].marks[i].color = *c;
                            self.touch();
                        }
                    }
                }
            }
            widgets::separator_v(ui);
            let wt = ["Thin lines", "Medium lines", "Thick lines"];
            let mut w = self.width;
            if widgets::segmented_with_tips(ui, &mut w, &width_names, &wt) {
                self.width = w;
                if let Some(Sel::Mark(pn, i)) = self.sel {
                    if let (Some(m), Some(Some(d))) = (self.review.images.get(pn).and_then(|im| im.marks.get(i)).cloned(), dims.get(pn)) {
                        self.checkpoint();
                        self.review.images[pn].marks[i].width = review::stroke_px(m.shape, w, *d);
                        self.touch();
                    }
                }
            }
        }
        // The hint, right-aligned in the room between the tools and the
        // buttons when there is enough of it.
        let gap = Rect::from_min_max(Pos2::new(left.min_rect().right() + 12.0, row.min.y), Pos2::new(buttons_left - 12.0, row.max.y));
        if gap.width() >= 160.0 {
            let mut mid = ui.new_child(egui::UiBuilder::new().max_rect(gap).layout(egui::Layout::right_to_left(egui::Align::Center)));
            let hint = "Drag on an image to draw · Space+drag or middle-drag pans · wheel zooms";
            mid.add(egui::Label::new(RichText::new(hint).size(text::SMALL).color(p.muted)).truncate());
        }
    }

    // ------------------------------------------------------------ panel

    pub fn panel(&mut self, ui: &mut Ui, p: &theme::Palette, panes: &[PaneInfo]) -> PanelOut {
        let mut out = PanelOut::default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(I::CHAT_TEXT).size(15.0).color(p.accent));
            ui.label(RichText::new("Review").font(text::title()).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button_sized(ui, I::X, "Close the review panel  (C)", false, 24.0, 14.0).clicked() {
                    out.close = true;
                }
            });
        });
        ui.label(RichText::new("Notes are saved automatically for this set of images.").size(text::SMALL).color(p.muted));
        ui.add_space(6.0);
        let mut changed = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 8.0);
            let full = ui.available_width();
            field_label(ui, p, "Report title");
            changed |= ui.add(egui::TextEdit::singleline(&mut self.review.title).hint_text("e.g. Phone A vs Phone B, low light").desired_width(full)).on_hover_text("The title of the exported report").changed();
            ui.add_space(6.0);
            field_label(ui, p, "Overall comment");
            changed |= ui
                .add(egui::TextEdit::multiline(&mut self.review.summary).hint_text("Your verdict on the comparison as a whole").desired_rows(3).desired_width(full))
                .on_hover_text("Shown on the overview page of the report")
                .changed();
            for (pi, info) in panes.iter().enumerate() {
                let Some(im) = self.review.images.get_mut(pi) else { continue };
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    badge(ui, info.letter, info.color);
                    ui.add(egui::Label::new(RichText::new(&info.name).font(text::body_strong()).color(p.text)).truncate()).on_hover_text(im.path.display().to_string());
                });
                changed |= ui
                    .add(egui::TextEdit::multiline(&mut im.comment).hint_text(format!("Comment on {}", info.letter)).desired_rows(2).desired_width(full))
                    .on_hover_text(format!("Your comment on image {}, shown on its page of the report", info.letter))
                    .changed();
                let mut delete = None;
                for (k, m) in im.marks.iter_mut().enumerate() {
                    let sel = self.sel == Some(Sel::Mark(pi, k));
                    ui.horizontal(|ui| {
                        let c = Color32::from_rgb(m.color[0], m.color[1], m.color[2]);
                        let r = number_chip(ui, k + 1, c, sel).on_hover_text(format!("{} {}: click to select it", m.shape.label(), k + 1));
                        if r.clicked() {
                            self.sel = Some(Sel::Mark(pi, k));
                        }
                        let w = ui.available_width() - 30.0;
                        let te = ui.add(egui::TextEdit::singleline(&mut m.note).hint_text(format!("{} note", m.shape.label())).desired_width(w)).on_hover_text("A note for this mark, listed under the image in the report");
                        if self.focus == Some(Sel::Mark(pi, k)) {
                            te.scroll_to_me(Some(egui::Align::Center));
                            self.focus = None;
                        }
                        if te.gained_focus() {
                            self.sel = Some(Sel::Mark(pi, k));
                        }
                        changed |= te.changed();
                        if widgets::icon_button_sized(ui, I::X, "Delete this mark", false, 22.0, 12.0).clicked() {
                            delete = Some(k);
                        }
                    });
                    // Artifact tag and severity: counted per device in project reports.
                    ui.horizontal(|ui| {
                        ui.add_space(26.0);
                        let shown = m.tag.clone().unwrap_or_else(|| "No artifact tag".into());
                        let tip = m.tag.as_deref().and_then(|t| lumen_core::workbench::ARTIFACT_TAGS.iter().find(|(n, _)| *n == t)).map(|(_, d)| *d).unwrap_or("Tag what this mark shows (halo, noise, flare...); project reports count tags per device");
                        widgets::combo(("mark-tag", pi, k))
                            .selected_text(RichText::new(shown).size(text::SMALL))
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(m.tag.is_none(), "No artifact tag").clicked() {
                                    m.tag = None;
                                    changed = true;
                                }
                                for (name, desc) in lumen_core::workbench::ARTIFACT_TAGS {
                                    if ui.selectable_label(m.tag.as_deref() == Some(name), name).on_hover_text(desc).clicked() {
                                        m.tag = Some(name.to_string());
                                        changed = true;
                                    }
                                }
                            })
                            .response
                            .on_hover_text(tip);
                        if m.tag.is_some() {
                            for sv in review::Severity::ALL {
                                if ui.selectable_label(m.severity == sv, RichText::new(sv.label()).size(text::SMALL)).on_hover_text(sv.hint()).clicked() {
                                    m.severity = sv;
                                    changed = true;
                                }
                            }
                        }
                    });
                }
                if let Some(k) = delete {
                    self.sel = Some(Sel::Mark(pi, k));
                    self.delete_selected();
                }
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(I::SELECTION).color(p.muted));
                ui.label(RichText::new("Details").font(text::body_strong()).color(p.text));
            });
            if self.review.details.is_empty() {
                ui.label(RichText::new("Use the Detail tool in Mark up to cut the same region from every image.").size(text::SMALL).color(p.muted));
            }
            let mut delete = None;
            let n = self.review.details.len();
            for k in 0..n {
                let sel = self.sel == Some(Sel::Detail(k));
                ui.add_space(4.0);
                let fr = egui::Frame::new().fill(if sel { p.selected_bg } else { p.faint_bg }).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::same(6));
                fr.show(ui, |ui| {
                    let d = &mut self.review.details[k];
                    ui.horizontal(|ui| {
                        let w = ui.available_width() - 60.0;
                        let te = ui.add(egui::TextEdit::singleline(&mut d.title).hint_text(format!("Detail {}", k + 1)).desired_width(w)).on_hover_text("The detail's title in the report");
                        if self.focus == Some(Sel::Detail(k)) {
                            te.scroll_to_me(Some(egui::Align::Center));
                            self.focus = None;
                        }
                        if te.gained_focus() {
                            self.sel = Some(Sel::Detail(k));
                        }
                        changed |= te.changed();
                        if widgets::icon_button_sized(ui, I::MAGNIFYING_GLASS_PLUS, "Zoom every image to this detail", false, 24.0, 13.0).clicked() {
                            out.zoom_detail = Some(k);
                            self.sel = Some(Sel::Detail(k));
                        }
                        if widgets::icon_button_sized(ui, I::X, "Delete this detail", false, 24.0, 12.0).clicked() {
                            delete = Some(k);
                        }
                    });
                    let d = &mut self.review.details[k];
                    changed |= ui.add(egui::TextEdit::multiline(&mut d.comment).hint_text("What to look at here").desired_rows(2).desired_width(ui.available_width())).on_hover_text("Shown under the detail crops in the report").changed();
                });
            }
            if let Some(k) = delete {
                self.sel = Some(Sel::Detail(k));
                self.delete_selected();
            }
            ui.add_space(14.0);
            let label = if self.exporting() { format!("{}  Exporting...", I::SPINNER) } else { format!("{}  Export report...", I::EXPORT) };
            let r = ui.add_enabled_ui(!self.exporting(), |ui| widgets::primary_button(ui, p, &label)).inner;
            if r.on_hover_text("Save the review as PowerPoint, PDF or a web page  (Ctrl+Shift+E)").clicked() {
                self.open_export();
            }
            if let Some(path) = self.last_export.clone() {
                ui.add_space(4.0);
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(format!("Last report: {name}")).size(text::SMALL).color(p.muted)).truncate());
                ui.horizontal(|ui| {
                    if widgets::secondary_button(ui, "Open").on_hover_text(path.display().to_string()).clicked() {
                        crate::fileops::open_with_default(&path);
                    }
                    if widgets::secondary_button(ui, "Show in folder").on_hover_text("Show the report in File Explorer").clicked() {
                        crate::fileops::show_in_explorer(&path);
                    }
                });
            }
            ui.add_space(8.0);
        });
        if changed {
            self.touch();
        }
        out
    }

    // ------------------------------------------------------------ export

    /// The export dialog (when open); starts the export on the worker.
    pub fn export_dialog(&mut self, ctx: &egui::Context, p: &theme::Palette, sources: impl FnOnce() -> Vec<ExportSource>) {
        let Some(form) = self.export.as_mut() else { return };
        let mut decision = None;
        let n_details = self.review.details.len();
        let has_metrics = self.metrics.is_some();
        widgets::modal(ctx, egui::Id::NULL.with("cmp-export-report"), 460.0, |ui| {
            widgets::modal_title(ui, "Export report");
            let full = ui.available_width();
            field_label(ui, p, "Title");
            ui.add(egui::TextEdit::singleline(&mut form.title).desired_width(full)).on_hover_text("Shown on the title page and at the top of every page");
            ui.add_space(6.0);
            field_label(ui, p, "Author");
            ui.add(egui::TextEdit::singleline(&mut form.author).hint_text("Your name").desired_width(full)).on_hover_text("Shown on the title page");
            ui.add_space(10.0);
            field_label(ui, p, "Format");
            for f in report::Format::ALL {
                widgets::radio_value(ui, &mut form.format, f, f.label()).on_hover_text(f.hint());
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    ui.label(RichText::new(f.hint()).size(text::SMALL).color(p.muted));
                });
            }
            ui.add_space(10.0);
            field_label(ui, p, "Pages");
            let s = &mut form.sections;
            widgets::checkbox(ui, &mut s.title, "Title page").on_hover_text("Report title, date and author");
            widgets::checkbox(ui, &mut s.overview, "Overview").on_hover_text("All images side by side with the overall comment");
            widgets::checkbox(ui, &mut s.per_image, "One page per image").on_hover_text("Each image large with its comment and numbered notes");
            ui.add_enabled_ui(n_details > 0, |ui| {
                let mut off = false;
                let v = if n_details > 0 { &mut s.details } else { &mut off };
                let label = if n_details > 0 { format!("Detail pages ({n_details})") } else { "Detail pages (none marked)".to_string() };
                widgets::checkbox(ui, v, label).on_hover_text("The same region cut from every image; mark regions with the Detail tool").on_disabled_hover_text("Mark regions with the Detail tool in Mark up first");
            });
            ui.add_enabled_ui(has_metrics, |ui| {
                // Unticked while there is nothing to include.
                let mut off = false;
                let v = if has_metrics { &mut s.metrics } else { &mut off };
                widgets::checkbox(ui, v, "Metrics table").on_hover_text("The numbers measured in the Analysis panel").on_disabled_hover_text("Run Metrics in the Analysis panel (Ctrl+Q) first");
            });
            ui.add_space(6.0);
            widgets::checkbox(ui, &mut form.image_info, "Image details").on_hover_text("Size, file size and camera settings under each image");
            widgets::checkbox(ui, &mut form.open_after, "Open the report when it is ready").on_hover_text("Opens it in PowerPoint, the PDF reader or the browser");
            let any = s.title || s.overview || s.per_image || (s.details && n_details > 0) || (s.metrics && has_metrics);
            if !any {
                ui.label(RichText::new("Pick at least one page.").size(text::SMALL).color(p.warn));
            }
            if let Some(r) = widgets::dialog_buttons(ui, p, "Cancel", "Export...", widgets::ButtonKind::Primary, any) {
                decision = Some(r);
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
        });
        match decision {
            Some(false) => self.export = None,
            Some(true) => {
                let form = self.export.take().unwrap();
                self.start_export(form, sources(), ctx);
            }
            None => {}
        }
    }

    fn start_export(&mut self, form: ExportForm, sources: Vec<ExportSource>, ctx: &egui::Context) {
        let p = &mut self.prefs;
        p.author = form.author.trim().to_string();
        p.format = form.format.ext().into();
        p.title_page = form.sections.title;
        p.overview = form.sections.overview;
        p.per_image = form.sections.per_image;
        p.details = form.sections.details;
        p.metrics = form.sections.metrics;
        p.image_info = form.image_info;
        p.open_after = form.open_after;
        if self.review.title.trim().is_empty() || self.review.title != form.title {
            self.review.title = form.title.clone();
            self.dirty.get_or_insert_with(Instant::now);
        }
        let dir = p.last_dir.clone().filter(|d| d.is_dir()).or_else(|| sources.first().and_then(|s| s.path.parent().map(|d| d.to_path_buf())));
        let ext = form.format.ext();
        let mut dlg = rfd::FileDialog::new().add_filter(form.format.label(), &[ext]).set_file_name(format!("{}.{ext}", review::file_stem(&form.title)));
        if let Some(d) = &dir {
            dlg = dlg.set_directory(d);
        }
        let Some(mut out) = dlg.save_file() else {
            p.save();
            return;
        };
        if out.extension().is_none_or(|e| !e.to_string_lossy().eq_ignore_ascii_case(ext)) {
            out.set_extension(ext);
        }
        p.last_dir = out.parent().map(|d| d.to_path_buf());
        p.save();
        let images = sources
            .into_iter()
            .zip(&self.review.images)
            .filter_map(|(s, im)| {
                Some(review::ExportImage { label: s.letter.to_string(), path: s.path, image: s.image?, file_size: s.file_size, comment: im.comment.clone(), marks: im.marks.clone() })
            })
            .collect();
        let sections = report::Sections { metrics: form.sections.metrics && self.metrics.is_some(), details: form.sections.details && !self.review.details.is_empty(), ..form.sections };
        let job = review::ExportJob {
            title: form.title,
            author: form.author,
            summary: self.review.summary.clone(),
            images,
            details: self.review.details.clone(),
            metrics: self.metrics.clone(),
            sections,
            image_info: form.image_info,
            format: form.format,
            out,
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| review::run_export(&job).map_err(|e| format!("{e:#}"))));
            let _ = tx.send(res.unwrap_or_else(|_| Err("the export failed unexpectedly".into())));
            ctx.request_repaint();
        });
        self.job = Some(rx);
    }
}

fn rel_point(p: [f32; 2], dims: (u32, u32)) -> [f32; 2] {
    [p[0] / dims.0.max(1) as f32, p[1] / dims.1.max(1) as f32]
}

fn field_label(ui: &mut Ui, p: &theme::Palette, t: &str) {
    ui.label(RichText::new(t).size(text::SMALL).color(p.muted));
}

/// The pane letter in its colour.
fn badge(ui: &mut Ui, letter: &str, c: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(9), c);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, letter, theme::semibold(11.0), theme::text_on(c));
}

/// A mark number in the mark's colour; ringed when selected.
fn number_chip(ui: &mut Ui, n: usize, c: Color32, sel: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    let p = theme::palette(ui.ctx());
    if sel {
        ui.painter().circle_stroke(r.center(), 10.5, Stroke::new(2.0, p.accent));
    }
    ui.painter().circle_filled(r.center(), 8.5, c);
    let ink = review::ink_on([c.r(), c.g(), c.b()]);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), theme::semibold(10.5), Color32::from_rgb(ink[0], ink[1], ink[2]));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn dashed_rect(painter: &egui::Painter, r: Rect, c: Color32) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    painter.add(egui::Shape::dashed_line(&pts, Stroke::new(3.0, Color32::from_black_alpha(110)), 6.0, 4.0));
    painter.add(egui::Shape::dashed_line(&pts, Stroke::new(1.5, c), 6.0, 4.0));
}

fn paint_mark(painter: &egui::Painter, g: &PaneGeo, m: &Mark) {
    let s = review::scale_of(g.dest, g.dims);
    let w = (m.width * s).max(1.0);
    let c = Color32::from_rgb(m.color[0], m.color[1], m.color[2]);
    let sp = |p: [f32; 2]| review::to_screen(g.dest, g.dims, p);
    match m.shape {
        Shape::Rect if m.pts.len() >= 2 => {
            let r = Rect::from_two_pos(sp(m.pts[0]), sp(m.pts[1]));
            painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(w, c), egui::StrokeKind::Middle);
        }
        Shape::Ellipse if m.pts.len() >= 2 => {
            let r = Rect::from_two_pos(sp(m.pts[0]), sp(m.pts[1]));
            painter.add(egui::Shape::ellipse_stroke(r.center(), r.size() / 2.0, Stroke::new(w, c)));
        }
        Shape::Arrow if m.pts.len() >= 2 => {
            let (a, b) = (sp(m.pts[0]), sp(m.pts[1]));
            let v = b - a;
            let l = v.length();
            if l < 1e-3 {
                return;
            }
            let u = v / l;
            let n = Vec2::new(-u.y, u.x);
            let head = ((m.width * 4.0).max(m.width + 10.0) * s).min(l * 0.9);
            let base = b - u * head;
            painter.line_segment([a, b - u * head * 0.7], Stroke::new(w, c));
            painter.add(egui::Shape::convex_polygon(vec![b, base + n * head * 0.5, base - n * head * 0.5], c, Stroke::NONE));
        }
        Shape::Pen | Shape::Highlighter if m.pts.len() >= 2 => {
            let pts: Vec<Pos2> = m.pts.iter().map(|p| sp(*p)).collect();
            let col = if m.shape == Shape::Highlighter { Color32::from_rgba_unmultiplied(m.color[0], m.color[1], m.color[2], 110) } else { c };
            painter.add(egui::Shape::line(pts, Stroke::new(w, col)));
        }
        _ => {}
    }
}

fn paint_badge(painter: &egui::Painter, g: &PaneGeo, m: &Mark, n: usize) {
    if !m.is_valid() {
        return;
    }
    let s = review::scale_of(g.dest, g.dims).max(1e-6);
    let d_pt = (review::badge_px(g.dims) * s).clamp(15.0, 24.0);
    let c_img = review::badge_center(m.anchor(), d_pt / s, g.dims);
    let c = review::to_screen(g.dest, g.dims, c_img);
    let col = Color32::from_rgb(m.color[0], m.color[1], m.color[2]);
    painter.circle_filled(c, d_pt / 2.0, col);
    painter.circle_stroke(c, d_pt / 2.0 - 0.5, Stroke::new(1.0, Color32::from_white_alpha(220)));
    let ink = review::ink_on(m.color);
    painter.text(c, Align2::CENTER_CENTER, n.to_string(), theme::semibold(d_pt * 0.55), Color32::from_rgb(ink[0], ink[1], ink[2]));
}
