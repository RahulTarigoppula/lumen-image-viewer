//! Shared on-image interaction for the interactive tools: overlay painting
//! helpers, pointer state, and a small shape editor (draw / select / move /
//! resize / delete rectangles, ellipses and lasso polygons, with undo).

use crate::edit::mask::Shape;
use crate::edit::CanvasCtx;
use egui::{Color32, Pos2, Rect, Shape as EShape, Stroke};

/// Screen-space hit radius for handles (points).
pub const HANDLE_HIT: f32 = 9.0;

/// Pointer state for this frame, in image coordinates.
pub struct Pointer {
    pub hover: Option<[f32; 2]>,
    /// Where the primary press began (exact, before the drag threshold).
    pub press_origin: Option<[f32; 2]>,
    pub pos: Option<[f32; 2]>,
    pub drag_started: bool,
    pub dragging: bool,
    pub drag_stopped: bool,
    pub clicked: bool,
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    /// Delete / Backspace pressed while no text field has focus.
    pub delete: bool,
}

impl Pointer {
    pub fn read(cx: &CanvasCtx, resp: &egui::Response) -> Pointer {
        let ctx = &resp.ctx;
        let typing = ctx.egui_wants_keyboard_input();
        let (origin, mods, del) = ctx.input(|i| (i.pointer.press_origin(), i.modifiers, !typing && (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))));
        let on = cx.interactive;
        Pointer {
            hover: resp.hover_pos().filter(|p| cx.view.contains(*p)).map(|p| cx.to_image(p)),
            press_origin: origin.map(|p| cx.to_image(p)),
            pos: resp.interact_pointer_pos().map(|p| cx.to_image(p)),
            drag_started: on && resp.drag_started_by(egui::PointerButton::Primary),
            dragging: on && resp.dragged_by(egui::PointerButton::Primary),
            drag_stopped: on && resp.drag_stopped_by(egui::PointerButton::Primary),
            clicked: on && resp.clicked(),
            alt: mods.alt,
            shift: mods.shift,
            ctrl: mods.command,
            delete: on && del,
        }
    }
    /// Start of the current drag (press origin, falling back to the position).
    pub fn start(&self) -> Option<[f32; 2]> {
        self.press_origin.or(self.pos)
    }
}

pub fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Accent colour used for selection outlines and handles.
pub fn accent(cx: &CanvasCtx) -> Color32 {
    cx.palette.accent
}

/// Square handle at an image point.
pub fn handle(cx: &CanvasCtx, painter: &egui::Painter, p: [f32; 2], active: bool) {
    let s = cx.to_screen(p);
    let r = Rect::from_center_size(s, egui::vec2(9.0, 9.0));
    painter.rect_filled(r, 2.0, if active { accent(cx) } else { Color32::WHITE });
    painter.rect_stroke(r, 2.0, Stroke::new(1.2, accent(cx)), egui::StrokeKind::Middle);
}

/// High-contrast dashed outline along screen points (black under white).
pub fn ants(painter: &egui::Painter, pts: &[Pos2], closed: bool, color: Color32) {
    if pts.len() < 2 {
        return;
    }
    let mut v = pts.to_vec();
    if closed {
        v.push(pts[0]);
    }
    painter.add(EShape::line(v.clone(), Stroke::new(1.0, Color32::from_black_alpha(170))));
    painter.extend(EShape::dashed_line(&v, Stroke::new(1.0, color), 5.0, 4.0));
}

/// Screen outline of a shape.
pub fn shape_outline(cx: &CanvasCtx, s: &Shape) -> Vec<Pos2> {
    match s {
        Shape::Rect(_) => {
            let b = s.bbox();
            vec![cx.to_screen([b[0], b[1]]), cx.to_screen([b[2], b[1]]), cx.to_screen([b[2], b[3]]), cx.to_screen([b[0], b[3]])]
        }
        Shape::Ellipse(_) => {
            let b = s.bbox();
            let (c, r) = ([(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0], [(b[2] - b[0]) / 2.0, (b[3] - b[1]) / 2.0]);
            (0..72).map(|i| {
                let t = i as f32 / 72.0 * std::f32::consts::TAU;
                cx.to_screen([c[0] + r[0] * t.cos(), c[1] + r[1] * t.sin()])
            }).collect()
        }
        Shape::Polygon(p) => p.iter().map(|q| cx.to_screen(*q)).collect(),
    }
}

/// Brush cursor: a thin double ring of `radius` image pixels.
pub fn brush_ring(cx: &CanvasCtx, painter: &egui::Painter, at: [f32; 2], radius: f32, hardness: Option<f32>) {
    let c = cx.to_screen(at);
    let r = (radius * cx.pts_per_px()).max(1.5);
    painter.circle_stroke(c, r + 0.5, Stroke::new(1.0, Color32::from_black_alpha(160)));
    painter.circle_stroke(c, r, Stroke::new(1.0, Color32::from_white_alpha(230)));
    if let Some(h) = hardness {
        if h < 0.95 && r * h > 3.0 {
            painter.circle_stroke(c, r * h, Stroke::new(1.0, Color32::from_white_alpha(70)));
        }
    }
}

/// Small crosshair marker (clone source etc.).
pub fn crosshair(cx: &CanvasCtx, painter: &egui::Painter, at: [f32; 2], color: Color32) {
    let c = cx.to_screen(at);
    for (d, col, w) in [(7.0, Color32::from_black_alpha(170), 3.0), (6.0, color, 1.4)] {
        painter.line_segment([c - egui::vec2(d, 0.0), c + egui::vec2(d, 0.0)], Stroke::new(w, col));
        painter.line_segment([c - egui::vec2(0.0, d), c + egui::vec2(0.0, d)], Stroke::new(w, col));
    }
}

/// Image-space rectangle from two points, optionally constrained to a square.
pub fn rect_from(a: [f32; 2], mut b: [f32; 2], square: bool) -> [f32; 4] {
    if square {
        let s = (b[0] - a[0]).abs().max((b[1] - a[1]).abs());
        b = [a[0] + s * (b[0] - a[0]).signum(), a[1] + s * (b[1] - a[1]).signum()];
    }
    [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
}

// ---------------------------------------------------------------- shape editor

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NewShape {
    #[default]
    Rect,
    Ellipse,
    Lasso,
}

#[derive(Clone, Debug)]
enum SDrag {
    Create([f32; 2]),
    Lasso,
    Move([f32; 2], Shape),
    Corner(usize, Shape),
}

/// A list of editable shapes with undo. Coordinates: full-resolution pixels.
#[derive(Clone, Default)]
pub struct ShapeEditor {
    pub shapes: Vec<Shape>,
    pub selected: Option<usize>,
    drag: Option<SDrag>,
    lasso: Vec<[f32; 2]>,
    history: Vec<Vec<Shape>>,
    future: Vec<Vec<Shape>>,
}

impl ShapeEditor {
    fn snapshot(&mut self) {
        self.history.push(self.shapes.clone());
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        self.future.clear();
    }
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.history.pop() else { return false };
        self.future.push(std::mem::replace(&mut self.shapes, prev));
        self.selected = None;
        self.drag = None;
        true
    }
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.future.pop() else { return false };
        self.history.push(std::mem::replace(&mut self.shapes, next));
        self.selected = None;
        self.drag = None;
        true
    }
    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }
    pub fn delete_selected(&mut self) -> bool {
        match self.selected {
            Some(i) if i < self.shapes.len() => {
                self.snapshot();
                self.shapes.remove(i);
                self.selected = None;
                true
            }
            _ => false,
        }
    }
    pub fn clear(&mut self) -> bool {
        if self.shapes.is_empty() {
            return false;
        }
        self.snapshot();
        self.shapes.clear();
        self.selected = None;
        true
    }
    /// Add a shape programmatically (one undo step).
    pub fn push(&mut self, s: Shape) {
        self.snapshot();
        self.shapes.push(s);
        self.selected = Some(self.shapes.len() - 1);
    }
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
    /// Shapes the renderer should use now (excludes nothing; in-progress
    /// rectangles are already in the list).
    pub fn committed(&self) -> &[Shape] {
        &self.shapes
    }

    fn corners(s: &Shape) -> Vec<[f32; 2]> {
        match s {
            Shape::Polygon(_) => Vec::new(),
            _ => {
                let b = s.bbox();
                vec![[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]]]
            }
        }
    }

    fn with_box(s: &Shape, b: [f32; 4]) -> Shape {
        match s {
            Shape::Rect(_) => Shape::Rect(b),
            Shape::Ellipse(_) => Shape::Ellipse(b),
            other => other.clone(),
        }
    }

    /// Handle pointer input and paint outlines. Returns true when the shapes changed.
    pub fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response, kind: NewShape, outline: Color32) -> bool {
        let pt = Pointer::read(cx, resp);
        let mut changed = false;
        let tol = cx.screen_to_px(HANDLE_HIT);
        if pt.drag_started {
            if let Some(start) = pt.start() {
                // Corner of the selected shape?
                let corner = self.selected.and_then(|i| self.shapes.get(i).map(|s| (i, s))).and_then(|(i, s)| Self::corners(s).iter().position(|c| dist(*c, start) <= tol).map(|k| (i, k, s.clone())));
                let hit = self.shapes.iter().rposition(|s| s.contains(start));
                if let Some((i, k, s)) = corner {
                    self.snapshot();
                    self.selected = Some(i);
                    self.drag = Some(SDrag::Corner(k, s));
                } else if let (Some(i), false) = (hit, pt.shift) {
                    self.snapshot();
                    self.selected = Some(i);
                    self.drag = Some(SDrag::Move(start, self.shapes[i].clone()));
                } else {
                    self.snapshot();
                    let start = cx.clamp_to_image(start);
                    match kind {
                        NewShape::Lasso => {
                            self.lasso = vec![start];
                            self.drag = Some(SDrag::Lasso);
                            self.selected = None;
                        }
                        _ => {
                            let s = if kind == NewShape::Ellipse { Shape::Ellipse([start[0], start[1], start[0], start[1]]) } else { Shape::Rect([start[0], start[1], start[0], start[1]]) };
                            self.shapes.push(s);
                            self.selected = Some(self.shapes.len() - 1);
                            self.drag = Some(SDrag::Create(start));
                        }
                    }
                }
            }
        }
        if pt.dragging {
            if let (Some(d), Some(p)) = (self.drag.clone(), pt.pos) {
                let p_cl = cx.clamp_to_image(p);
                match d {
                    SDrag::Create(a) => {
                        if let Some(s) = self.selected.and_then(|i| self.shapes.get_mut(i)) {
                            *s = Self::with_box(s, rect_from(a, p_cl, pt.shift));
                            changed = true;
                        }
                    }
                    SDrag::Lasso => {
                        if self.lasso.last().is_none_or(|l| dist(*l, p_cl) >= cx.screen_to_px(3.0)) {
                            self.lasso.push(p_cl);
                        }
                    }
                    SDrag::Move(start, orig) => {
                        if let Some(s) = self.selected.and_then(|i| self.shapes.get_mut(i)) {
                            *s = orig.translated(p[0] - start[0], p[1] - start[1]);
                            changed = true;
                        }
                    }
                    SDrag::Corner(k, orig) => {
                        if let Some(s) = self.selected.and_then(|i| self.shapes.get_mut(i)) {
                            let b = orig.bbox();
                            let opposite = [[b[2], b[3]], [b[0], b[3]], [b[0], b[1]], [b[2], b[1]]][k.min(3)];
                            *s = Self::with_box(&orig, rect_from(opposite, p_cl, pt.shift));
                            changed = true;
                        }
                    }
                }
            }
        }
        if pt.drag_stopped || (self.drag.is_some() && !resp.ctx.input(|i| i.pointer.primary_down())) {
            match self.drag.take() {
                Some(SDrag::Lasso) => {
                    let pts = std::mem::take(&mut self.lasso);
                    let s = Shape::Polygon(pts);
                    if s.is_valid() {
                        self.shapes.push(s);
                        self.selected = Some(self.shapes.len() - 1);
                        changed = true;
                    } else {
                        self.history.pop();
                    }
                }
                Some(SDrag::Create(_)) => {
                    // Too small to be intentional: drop it (and its undo step).
                    let tiny = self.selected.and_then(|i| self.shapes.get(i)).is_some_and(|s| {
                        let b = s.bbox();
                        (b[2] - b[0]) < cx.screen_to_px(3.0) || (b[3] - b[1]) < cx.screen_to_px(3.0)
                    });
                    if tiny {
                        if let Some(i) = self.selected.take() {
                            self.shapes.remove(i);
                        }
                        self.history.pop();
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if pt.clicked {
            if let Some(p) = pt.pos {
                self.selected = self.shapes.iter().rposition(|s| s.contains(p));
            }
        }
        if pt.delete && self.delete_selected() {
            changed = true;
        }
        // Cursor
        if let Some(h) = pt.hover {
            let over_corner = self.selected.and_then(|i| self.shapes.get(i)).is_some_and(|s| Self::corners(s).iter().any(|c| dist(*c, h) <= tol));
            let icon = if over_corner {
                egui::CursorIcon::Crosshair
            } else if self.shapes.iter().any(|s| s.contains(h)) && !pt.shift {
                egui::CursorIcon::Move
            } else {
                egui::CursorIcon::Crosshair
            };
            if cx.interactive {
                resp.ctx.set_cursor_icon(icon);
            }
        }
        // Paint
        for (i, s) in self.shapes.iter().enumerate() {
            let sel = self.selected == Some(i);
            ants(painter, &shape_outline(cx, s), true, if sel { accent(cx) } else { outline });
        }
        if !self.lasso.is_empty() {
            let pts: Vec<Pos2> = self.lasso.iter().map(|p| cx.to_screen(*p)).collect();
            ants(painter, &pts, false, outline);
        }
        if let Some(s) = self.selected.and_then(|i| self.shapes.get(i)) {
            for c in Self::corners(s) {
                handle(cx, painter, c, false);
            }
        }
        changed
    }
}

/// Undo / Redo / Delete / Clear button row for a [`ShapeEditor`].
pub fn shape_buttons(ui: &mut egui::Ui, ed: &mut ShapeEditor) -> bool {
    use crate::ui::icons as I;
    let mut c = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
        if ui.add_enabled(ed.can_undo(), egui::Button::new(format!("{}  Undo", I::ARROW_COUNTER_CLOCKWISE))).clicked() {
            c |= ed.undo();
        }
        if ui.add_enabled(ed.can_redo(), egui::Button::new(format!("{}  Redo", I::ARROW_CLOCKWISE))).clicked() {
            c |= ed.redo();
        }
        if ui.add_enabled(ed.selected.is_some(), egui::Button::new(format!("{}  Delete", I::TRASH))).on_hover_text("Delete the selected area (Del)").clicked() {
            c |= ed.delete_selected();
        }
        if ui.add_enabled(!ed.shapes.is_empty(), egui::Button::new("Clear")).clicked() {
            c |= ed.clear();
        }
    });
    c
}
