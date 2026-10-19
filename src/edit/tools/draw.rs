//! Draw Board: vector annotations (lines, arrows, shapes, highlighter, pen,
//! text callouts, numbered markers) drawn on the image, selectable and
//! editable until Apply rasterizes them with anti-aliasing.

use super::canvas::{self, Pointer, HANDLE_HIT};
use super::{color_button, hint, icon_toggle, indented, row, section, segmented, slider_ex};
use crate::ui::widgets;
use crate::edit::raster::{self, DrawObj, Kind, CALLOUT_BUBBLE, CALLOUT_PLAIN};
use crate::edit::{CanvasCtx, EditTool, ToolUiCtx};
use crate::ui::icons as I;
use egui::{pos2, vec2, Color32, Pos2, Rect, Stroke};
use image::RgbaImage;

const KINDS: [Kind; 9] = [Kind::Line, Kind::Arrow, Kind::Rect, Kind::RoundRect, Kind::Ellipse, Kind::Highlighter, Kind::Pen, Kind::Text, Kind::Number];

fn kind_icon(k: Kind) -> &'static str {
    match k {
        Kind::Line => I::LINE_SEGMENT,
        Kind::Arrow => I::ARROW_UP_RIGHT,
        Kind::Rect => I::SQUARE,
        Kind::RoundRect => I::RECTANGLE,
        Kind::Ellipse => I::CIRCLE,
        Kind::Highlighter => I::HIGHLIGHTER,
        Kind::Pen => I::PENCIL_SIMPLE,
        Kind::Text => I::CHAT_TEXT,
        Kind::Number => I::NUMBER_CIRCLE_ONE,
    }
}

fn kind_index(k: Kind) -> usize {
    KINDS.iter().position(|q| *q == k).unwrap_or(0)
}

const SWATCHES: [[u8; 4]; 8] = [
    [230, 40, 40, 255],
    [255, 140, 0, 255],
    [255, 214, 0, 255],
    [40, 170, 70, 255],
    [30, 120, 230, 255],
    [140, 70, 210, 255],
    [255, 255, 255, 255],
    [20, 20, 20, 255],
];

#[derive(Clone, Debug)]
enum Drag {
    /// Creating the selected object.
    Create([f32; 2]),
    Move([f32; 2], DrawObj),
    Handle(usize, DrawObj),
    /// Dragging out a new bubble: the tail points at the press, the bubble
    /// follows the pointer.
    Callout([f32; 2], DrawObj),
}

#[derive(Clone)]
pub struct DrawBoard {
    objs: Vec<DrawObj>,
    /// None = select / move tool.
    tool: Option<Kind>,
    /// Per-kind style templates for new objects.
    styles: Vec<DrawObj>,
    selected: Option<usize>,
    drag: Option<Drag>,
    history: Vec<Vec<DrawObj>>,
    future: Vec<Vec<DrawObj>>,
    /// Redo stack set aside while an object is being drawn: restored when the
    /// new object is discarded as too small, so a stray click keeps redo.
    tentative_future: Option<Vec<Vec<DrawObj>>>,
    /// Coalesces a run of style edits on one object into one undo step.
    style_edit: Option<usize>,
    next_number: u32,
    focus_text: bool,
    max_width: f32,
    max_font: f32,
    init: bool,
}

impl Default for DrawBoard {
    fn default() -> Self {
        let styles = KINDS.iter().map(|&k| style_for(k, 1000.0)).collect();
        DrawBoard {
            objs: Vec::new(),
            tool: Some(Kind::Arrow),
            styles,
            selected: None,
            drag: None,
            history: Vec::new(),
            future: Vec::new(),
            tentative_future: None,
            style_edit: None,
            next_number: 1,
            focus_text: false,
            max_width: 80.0,
            max_font: 400.0,
            init: false,
        }
    }
}

/// Default style for a kind, sized for an image whose short side is `short` px.
fn style_for(kind: Kind, short: f32) -> DrawObj {
    let width = (short / 220.0).round().clamp(2.0, 80.0);
    let font = (short / 24.0).round().clamp(12.0, 600.0);
    let base = DrawObj { kind, width, font_px: font, ..Default::default() };
    match kind {
        Kind::Highlighter => DrawObj { color: [255, 230, 0, 255], width: (short / 40.0).round().clamp(6.0, 400.0), ..base },
        Kind::Pen => DrawObj { width: (width * 0.8).round().max(1.0), ..base },
        Kind::Text => DrawObj { text: "Text".into(), color: [40, 40, 40, 255], width: (width * 0.5).round().max(1.0), fill: Some([255, 255, 255, 255]), shadow: true, ..base },
        Kind::Number => DrawObj { font_px: (short / 16.0).round().clamp(16.0, 600.0), text_color: [255, 255, 255, 255], shadow: true, ..base },
        _ => base,
    }
}

impl DrawBoard {
    fn snapshot(&mut self) {
        self.history.push(self.objs.clone());
        if self.history.len() > 300 {
            self.history.remove(0);
        }
        self.future.clear();
        self.tentative_future = None;
        self.style_edit = None;
    }

    /// Like `snapshot`, but the redo stack comes back if the step is dropped
    /// with `discard_tentative`.
    fn snapshot_tentative(&mut self) {
        let future = std::mem::take(&mut self.future);
        self.snapshot();
        self.tentative_future = Some(future);
    }

    fn discard_tentative(&mut self) {
        self.history.pop();
        if let Some(f) = self.tentative_future.take() {
            self.future = f;
        }
    }

    fn delete_selected(&mut self) -> bool {
        match self.selected {
            Some(i) if i < self.objs.len() => {
                self.snapshot();
                self.objs.remove(i);
                self.selected = None;
                true
            }
            _ => false,
        }
    }

    /// Control points that can be dragged for an object.
    fn handles(o: &DrawObj) -> Vec<[f32; 2]> {
        match o.kind {
            Kind::Line | Kind::Arrow => o.pts.iter().take(2).copied().collect(),
            Kind::Rect | Kind::RoundRect | Kind::Ellipse if o.pts.len() >= 2 => {
                let (a, b) = (o.pts[0], o.pts[1]);
                vec![a, [b[0], a[1]], b, [a[0], b[1]]]
            }
            Kind::Text if o.callout == CALLOUT_BUBBLE && o.pts.len() >= 3 => vec![o.pts[2]],
            _ => Vec::new(),
        }
    }

    fn set_handle(o: &mut DrawObj, orig: &DrawObj, k: usize, p: [f32; 2], shift: bool) {
        match o.kind {
            Kind::Line | Kind::Arrow => {
                let anchor = orig.pts[1 - k.min(1)];
                o.pts[k.min(1)] = if shift { snap_angle(anchor, p) } else { p };
            }
            Kind::Rect | Kind::RoundRect | Kind::Ellipse => {
                let (a, b) = (orig.pts[0], orig.pts[1]);
                // Opposite corner stays put.
                let opp = [[b[0], b[1]], [a[0], b[1]], [a[0], a[1]], [b[0], a[1]]][k.min(3)];
                let q = if shift { square(opp, p) } else { p };
                o.pts[0] = opp;
                o.pts[1] = q;
            }
            Kind::Text => {
                if o.pts.len() >= 3 {
                    o.pts[2] = p;
                }
            }
            _ => {}
        }
    }

    /// The style controls edit this: the selected object or the current template.
    fn editing_kind(&self) -> Kind {
        self.selected.and_then(|i| self.objs.get(i)).map(|o| o.kind).or(self.tool).unwrap_or(Kind::Arrow)
    }

    fn style_ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let kind = self.editing_kind();
        let sel = self.selected.filter(|&i| i < self.objs.len());
        let mut st = match sel {
            Some(i) => self.objs[i].clone(),
            None => self.styles[kind_index(kind)].clone(),
        };
        let before = st.clone();
        section(ui, cx, &format!("{}{}", kind.label(), if sel.is_some() { " (selected)" } else { "" }));
        let color_label = match kind {
            Kind::Text => "Border",
            Kind::Number => "Disc",
            _ => "Color",
        };
        ui.horizontal(|ui| {
            super::row_label(ui, color_label);
            color_button(ui, &mut st.color, false);
            ui.spacing_mut().item_spacing.x = 3.0;
            for s in SWATCHES {
                let (r, resp) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::click());
                let c = Color32::from_rgb(s[0], s[1], s[2]);
                ui.painter().rect_filled(r, 3.0, c);
                let ring = if st.color[..3] == s[..3] { Stroke::new(2.0, cx.palette.accent) } else { Stroke::new(1.0, cx.palette.border) };
                ui.painter().rect_stroke(r, 3.0, ring, egui::StrokeKind::Outside);
                if resp.clicked() {
                    st.color = s;
                }
            }
        });
        if kind != Kind::Number {
            let label = if kind == Kind::Text { "Border width" } else { "Width" };
            let min = if kind == Kind::Text { 0.0 } else { 1.0 };
            slider_ex(ui, label, &mut st.width, min..=self.max_width, before.width, " px", 1.0, 0, true);
        }
        let mut op = st.opacity * 100.0;
        if slider_ex(ui, "Opacity", &mut op, 5.0..=100.0, 100.0, " %", 1.0, 0, false) {
            st.opacity = op / 100.0;
        }
        if kind.has_fill() {
            ui.horizontal(|ui| {
                super::row_label(ui, "Fill");
                let mut on = st.fill.is_some();
                if widgets::checkbox(ui, &mut on, "").changed() {
                    st.fill = if on { Some(if kind == Kind::Text { [255, 255, 255, 255] } else { [st.color[0], st.color[1], st.color[2], 90] }) } else { None };
                }
                if let Some(f) = &mut st.fill {
                    color_button(ui, f, true);
                }
            });
        }
        match kind {
            Kind::Arrow => {
                indented(ui, |ui| widgets::checkbox(ui, &mut st.both_heads, "Both ends"));
            }
            Kind::Text => {
                row(ui, "Style", |ui| {
                    let mut c = st.callout as usize;
                    if segmented(ui, &mut c, &["Plain", "Box", "Bubble"]) {
                        st.callout = c as u8;
                    }
                });
                let mut te = egui::TextEdit::multiline(&mut st.text).desired_rows(2).desired_width(f32::INFINITY).hint_text("Type the callout text").show(ui);
                if self.focus_text {
                    // A new box shows its placeholder text selected, so typing replaces it.
                    te.response.request_focus();
                    let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(st.text.chars().count()));
                    te.state.cursor.set_char_range(Some(all));
                    te.state.store(ui.ctx(), te.response.id);
                    self.focus_text = false;
                }
                slider_ex(ui, "Text size", &mut st.font_px, 6.0..=self.max_font, before.font_px, " px", 1.0, 0, true);
                ui.horizontal(|ui| {
                    super::row_label(ui, "Text color");
                    color_button(ui, &mut st.text_color, false);
                    icon_toggle(ui, &mut st.bold, I::TEXT_B, "Bold");
                });
            }
            Kind::Number => {
                slider_ex(ui, "Size", &mut st.font_px, 8.0..=self.max_font, before.font_px, " px", 1.0, 0, true);
                ui.horizontal(|ui| {
                    super::row_label(ui, "Number");
                    color_button(ui, &mut st.text_color, false);
                    if sel.is_some() {
                        widgets::value_box(ui, egui::DragValue::new(&mut st.number).range(0..=9999));
                    } else {
                        widgets::value_box(ui, egui::DragValue::new(&mut self.next_number).range(0..=9999)).on_hover_text("Next number");
                    }
                });
            }
            _ => {}
        }
        if !matches!(kind, Kind::Highlighter) {
            indented(ui, |ui| {
                if !matches!(kind, Kind::Number | Kind::Text) {
                    widgets::checkbox(ui, &mut st.dashed, "Dashed");
                    ui.add_space(8.0);
                }
                widgets::checkbox(ui, &mut st.shadow, "Shadow");
            });
        }
        if st == before {
            return false;
        }
        if st.kind == Kind::Text && (st.text != before.text || st.font_px != before.font_px || st.bold != before.bold || st.callout != before.callout) && st.pts.len() >= 1 {
            st.fit_text_box();
        }
        // Remember as the template for new objects of this kind (minus geometry).
        let tpl = &mut self.styles[kind_index(kind)];
        *tpl = DrawObj { pts: Vec::new(), number: tpl.number, ..st.clone() };
        match sel {
            Some(i) => {
                if self.style_edit != Some(i) {
                    self.snapshot();
                    self.style_edit = Some(i);
                }
                self.objs[i] = st;
                true
            }
            None => false,
        }
    }

    /// Objects the renderer should draw (the one being dragged is shown as a
    /// live overlay instead, so it follows the pointer without lag).
    fn render_list(&self) -> Vec<&DrawObj> {
        let hidden = if self.drag.is_some() { self.selected } else { None };
        self.objs.iter().enumerate().filter(|(i, _)| Some(*i) != hidden).map(|(_, o)| o).collect()
    }
}

/// Put a bubble's tail tip at `tip` and centre the bubble on `at`.
fn place_callout(o: &mut DrawObj, tip: [f32; 2], at: [f32; 2]) {
    if o.pts.len() < 3 {
        return;
    }
    let (a, b) = (o.pts[0], o.pts[1]);
    let c = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
    let (dx, dy) = (at[0] - c[0], at[1] - c[1]);
    for p in &mut o.pts[..2] {
        p[0] += dx;
        p[1] += dy;
    }
    o.pts[2] = tip;
}

fn snap_angle(a: [f32; 2], p: [f32; 2]) -> [f32; 2] {
    let (dx, dy) = (p[0] - a[0], p[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    let ang = dy.atan2(dx);
    let step = std::f32::consts::FRAC_PI_4;
    let s = (ang / step).round() * step;
    [a[0] + len * s.cos(), a[1] + len * s.sin()]
}

fn square(a: [f32; 2], p: [f32; 2]) -> [f32; 2] {
    let r = canvas::rect_from(a, p, true);
    [if p[0] < a[0] { r[0] } else { r[2] }, if p[1] < a[1] { r[1] } else { r[3] }]
}

fn c32(c: [u8; 4], opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], (c[3] as f32 * opacity.clamp(0.0, 1.0)) as u8)
}

/// Approximate live rendering with egui shapes (used while dragging, before
/// the anti-aliased preview catches up).
fn paint_live(cx: &CanvasCtx, painter: &egui::Painter, o: &DrawObj) {
    if o.pts.is_empty() {
        return;
    }
    let k = cx.pts_per_px();
    let w = (o.width * k).max(0.5);
    let col = c32(o.color, o.opacity * if o.kind == Kind::Highlighter { 0.45 } else { 1.0 });
    let st = Stroke::new(w, col);
    let sp: Vec<Pos2> = o.pts.iter().map(|p| cx.to_screen(*p)).collect();
    match o.kind {
        Kind::Line if sp.len() >= 2 => {
            painter.line_segment([sp[0], sp[1]], st);
        }
        Kind::Arrow if sp.len() >= 2 => {
            let (a, b) = (sp[0], sp[1]);
            let d = b - a;
            let len = d.length();
            if len > 0.5 {
                let u = d / len;
                let n = vec2(-u.y, u.x);
                let hl = (o.head_len() * k).min(len * 0.9);
                let base = b - u * hl;
                painter.line_segment([a, b - u * hl * 0.7], st);
                painter.add(egui::Shape::convex_polygon(vec![b, base + n * hl * 0.5, b - u * hl * 0.78, base - n * hl * 0.5], col, Stroke::NONE));
                if o.both_heads {
                    let base2 = a + u * hl;
                    painter.add(egui::Shape::convex_polygon(vec![a, base2 - n * hl * 0.5, a + u * hl * 0.78, base2 + n * hl * 0.5], col, Stroke::NONE));
                }
            }
        }
        Kind::Rect | Kind::RoundRect | Kind::Ellipse if sp.len() >= 2 => {
            let r = Rect::from_two_pos(sp[0], sp[1]);
            let fill = o.fill.map(|f| c32(f, o.opacity)).unwrap_or(Color32::TRANSPARENT);
            match o.kind {
                Kind::Ellipse => {
                    painter.add(egui::Shape::ellipse_filled(r.center(), r.size() / 2.0, fill));
                    painter.add(egui::Shape::ellipse_stroke(r.center(), r.size() / 2.0, st));
                }
                _ => {
                    let rad = if o.kind == Kind::RoundRect { (o.width * 2.5 * k).max(r.width().min(r.height()) * 0.14) } else { 0.0 };
                    painter.rect_filled(r, rad, fill);
                    painter.rect_stroke(r, rad, st, egui::StrokeKind::Middle);
                }
            }
        }
        Kind::Pen | Kind::Highlighter => {
            if sp.len() == 1 {
                painter.circle_filled(sp[0], w / 2.0, col);
            } else {
                painter.add(egui::Shape::line(sp, st));
            }
        }
        Kind::Text if sp.len() >= 2 => {
            let r = Rect::from_two_pos(sp[0], sp[1]);
            if o.callout != CALLOUT_PLAIN {
                if o.callout == CALLOUT_BUBBLE && sp.len() >= 3 {
                    painter.line_segment([r.center(), sp[2]], Stroke::new((o.font_px * 0.3 * k).max(1.0), c32(o.fill.unwrap_or([255; 4]), o.opacity)));
                }
                painter.rect_filled(r, o.font_px * 0.35 * k, c32(o.fill.unwrap_or([255; 4]), o.opacity));
                painter.rect_stroke(r, o.font_px * 0.35 * k, Stroke::new(w, col), egui::StrokeKind::Middle);
            }
            let fid = egui::FontId::proportional((o.font_px * k).clamp(4.0, 400.0));
            painter.text(r.center(), egui::Align2::CENTER_CENTER, &o.text, fid, c32(o.text_color, o.opacity));
        }
        Kind::Number => {
            let r = o.font_px / 2.0 * k;
            painter.circle_filled(sp[0], r, col);
            let fid = egui::FontId::proportional((o.font_px * 0.6 * k).clamp(4.0, 400.0));
            painter.text(sp[0], egui::Align2::CENTER_CENTER, o.number.to_string(), fid, c32(o.text_color, o.opacity));
        }
        _ => {}
    }
}

impl EditTool for DrawBoard {
    fn title(&self) -> String {
        "Draw board".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let short = cx.image_size.0.min(cx.image_size.1).max(1) as f32;
        if !self.init {
            self.init = true;
            self.styles = KINDS.iter().map(|&k| style_for(k, short)).collect();
            self.max_width = (short / 12.0).max(20.0);
            self.max_font = (short / 3.0).max(40.0);
        }
        let mut c = false;
        // Tool palette.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
            let b = |ui: &mut egui::Ui, on: bool, icon: &str, tip: &str| widgets::icon_button_sized(ui, icon, tip, on, 32.0, 16.0).clicked();
            if b(ui, self.tool.is_none(), I::CURSOR, "Select and move (click an object, drag to move, Del deletes)") {
                self.tool = None;
            }
            for k in KINDS {
                if b(ui, self.tool == Some(k), kind_icon(k), k.label()) {
                    self.tool = Some(k);
                    self.selected = None;
                }
            }
        });
        ui.add_space(2.0);
        c |= self.style_ui(ui, cx);
        section(ui, cx, &format!("Objects ({})", self.objs.len()));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            if ui.add_enabled(!self.history.is_empty(), egui::Button::new(format!("{}  Undo", I::ARROW_COUNTER_CLOCKWISE))).clicked() {
                c |= self.undo();
            }
            if ui.add_enabled(!self.future.is_empty(), egui::Button::new(format!("{}  Redo", I::ARROW_CLOCKWISE))).clicked() {
                c |= self.redo();
            }
            if ui.add_enabled(self.selected.is_some(), egui::Button::new(format!("{}  Delete", I::TRASH))).on_hover_text("Delete the selected object (Del)").clicked() {
                c |= self.delete_selected();
            }
            if ui.add_enabled(self.selected.is_some_and(|i| i + 1 < self.objs.len()), egui::Button::new("To front")).clicked() {
                if let Some(i) = self.selected {
                    self.snapshot();
                    let o = self.objs.remove(i);
                    self.objs.push(o);
                    self.selected = Some(self.objs.len() - 1);
                    c = true;
                }
            }
            if ui.add_enabled(!self.objs.is_empty(), egui::Button::new("Clear")).clicked() {
                self.snapshot();
                self.objs.clear();
                self.selected = None;
                self.next_number = 1;
                c = true;
            }
        });
        ui.add_space(4.0);
        let how = match self.tool {
            None => "Click an object to select it; drag to move, drag handles to reshape. Del deletes.",
            Some(Kind::Text) => "Click where the text goes, or drag from the spot a bubble should point at. Type in the box above; drag the tail handle to re-aim it.",
            Some(Kind::Number) => "Click to drop numbered markers; the number counts up.",
            Some(Kind::Pen) | Some(Kind::Highlighter) => "Drag to draw freehand.",
            Some(_) => "Drag on the image to draw. Shift snaps angles / keeps squares and circles.",
        };
        hint(ui, how);
        hint(ui, "Hold Space to pan, wheel to zoom. Apply renders everything with anti-aliasing.");
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let mut out = img.clone();
        for o in self.render_list() {
            raster::render_obj(&mut out, o, scale);
        }
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        let (styles, maxw, maxf, init) = (self.styles.clone(), self.max_width, self.max_font, self.init);
        *self = DrawBoard { styles, max_width: maxw, max_font: maxf, init, ..Default::default() };
    }
    fn can_apply(&self) -> bool {
        !self.objs.is_empty()
    }
    fn wants_canvas(&self) -> bool {
        true
    }
    fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response) -> bool {
        let pt = Pointer::read(cx, resp);
        let mut changed = false;
        let tol = cx.screen_to_px(HANDLE_HIT);
        let hit_obj = |objs: &[DrawObj], p: [f32; 2]| objs.iter().rposition(|o| o.hit(p, cx.screen_to_px(5.0)));
        let sel_handle = |s: &Self, p: [f32; 2]| s.selected.and_then(|i| s.objs.get(i)).and_then(|o| Self::handles(o).iter().position(|h| canvas::dist(*h, p) <= tol));

        // ---- press
        let press = pt.drag_started || (pt.clicked && self.drag.is_none());
        if press {
            if let Some(start) = pt.start() {
                if let (Some(k), Some(i)) = (sel_handle(self, start), self.selected) {
                    if pt.drag_started {
                        self.snapshot();
                        self.drag = Some(Drag::Handle(k, self.objs[i].clone()));
                    }
                } else if self.tool.is_none() || (pt.ctrl && hit_obj(&self.objs, start).is_some()) {
                    // Select / move (Ctrl+drag moves in any tool).
                    self.selected = hit_obj(&self.objs, start);
                    if let (Some(i), true) = (self.selected, pt.drag_started) {
                        self.snapshot();
                        self.drag = Some(Drag::Move(start, self.objs[i].clone()));
                    }
                } else if let Some(kind) = self.tool {
                    let tpl = self.styles[kind_index(kind)].clone();
                    let p = cx.clamp_to_image(start);
                    let mut o = DrawObj { kind, pts: vec![p], ..tpl };
                    let create = match kind {
                        Kind::Text => {
                            if o.text.trim().is_empty() {
                                o.text = "Text".into();
                            }
                            o.fit_text_box();
                            self.focus_text = true;
                            true
                        }
                        Kind::Number => {
                            o.number = self.next_number;
                            self.next_number += 1;
                            true
                        }
                        k if k.is_two_point() => {
                            o.pts.push(p);
                            pt.drag_started
                        }
                        _ => pt.drag_started,
                    };
                    if create {
                        if pt.drag_started && !matches!(kind, Kind::Text | Kind::Number) {
                            self.snapshot_tentative();
                        } else {
                            self.snapshot();
                        }
                        self.objs.push(o);
                        self.selected = Some(self.objs.len() - 1);
                        let i = self.objs.len() - 1;
                        if pt.drag_started && kind == Kind::Text && self.objs[i].callout == CALLOUT_BUBBLE {
                            self.drag = Some(Drag::Callout(p, self.objs[i].clone()));
                        } else if pt.drag_started && matches!(kind, Kind::Text | Kind::Number) {
                            // Dragging right after dropping one moves it.
                            self.drag = Some(Drag::Move(start, self.objs[i].clone()));
                        } else if pt.drag_started {
                            self.drag = Some(Drag::Create(p));
                        }
                        changed = true;
                    }
                }
            }
        }
        // ---- drag
        if pt.dragging {
            if let (Some(d), Some(p), Some(i)) = (self.drag.clone(), pt.pos, self.selected) {
                if let Some(o) = self.objs.get_mut(i) {
                    match d {
                        Drag::Create(a) => {
                            let p = cx.clamp_to_image(p);
                            if o.kind.is_freehand() {
                                if o.pts.last().is_none_or(|l| canvas::dist(*l, p) >= cx.screen_to_px(1.5)) {
                                    o.pts.push(p);
                                }
                            } else if o.pts.len() >= 2 {
                                o.pts[1] = match (pt.shift, o.kind) {
                                    (true, Kind::Line | Kind::Arrow) => snap_angle(a, p),
                                    (true, _) => square(a, p),
                                    _ => p,
                                };
                            }
                        }
                        Drag::Move(start, orig) => {
                            *o = orig.clone();
                            o.translate(p[0] - start[0], p[1] - start[1]);
                        }
                        Drag::Handle(k, orig) => {
                            Self::set_handle(o, &orig, k, p, pt.shift);
                        }
                        Drag::Callout(start, orig) => {
                            *o = orig.clone();
                            // A short drag is a click: keep the default tail.
                            if canvas::dist(start, p) >= cx.screen_to_px(8.0) {
                                place_callout(o, start, cx.clamp_to_image(p));
                            }
                        }
                    }
                }
            }
        }
        // ---- release
        if self.drag.is_some() && (pt.drag_stopped || !resp.ctx.input(|i| i.pointer.primary_down())) {
            if let Some(Drag::Create(_)) = self.drag.take() {
                if let Some(i) = self.selected {
                    let keep = self.objs.get(i).is_some_and(|o| match o.kind {
                        k if k.is_two_point() => canvas::dist(o.pts[0], o.pts[1]) >= cx.screen_to_px(4.0),
                        _ => o.is_valid(),
                    });
                    if !keep {
                        self.objs.remove(i);
                        self.selected = None;
                        self.discard_tentative();
                    } else {
                        self.tentative_future = None;
                    }
                }
            }
            changed = true;
        }
        if pt.delete && self.delete_selected() {
            changed = true;
        }
        // ---- cursor
        if let (Some(h), true) = (pt.hover, cx.interactive) {
            let icon = if sel_handle(self, h).is_some() {
                egui::CursorIcon::Crosshair
            } else if self.drag.is_some() && matches!(self.drag, Some(Drag::Move(..))) {
                egui::CursorIcon::Grabbing
            } else if (self.tool.is_none() || pt.ctrl) && hit_obj(&self.objs, h).is_some() {
                egui::CursorIcon::Move
            } else if self.tool == Some(Kind::Text) {
                egui::CursorIcon::Text
            } else if self.tool.is_some() {
                egui::CursorIcon::Crosshair
            } else {
                egui::CursorIcon::Default
            };
            resp.ctx.set_cursor_icon(icon);
        }
        // ---- overlay
        if let Some(o) = self.selected.and_then(|i| self.objs.get(i)) {
            if self.drag.is_some() || !cx.preview_current {
                paint_live(cx, painter, o);
            }
            let b = o.bounds();
            let pad = o.width * 0.5 * cx.pts_per_px() + 4.0;
            let r = Rect::from_two_pos(cx.to_screen([b[0], b[1]]), cx.to_screen([b[2], b[3]])).expand(pad);
            canvas::ants(painter, &[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()], true, cx.palette.accent);
            for h in Self::handles(o) {
                canvas::handle(cx, painter, h, false);
            }
        }
        if self.objs.is_empty() && self.tool.is_some() {
            if let Some(h) = pt.hover {
                let s = cx.to_screen(h);
                painter.text(s + vec2(14.0, 14.0), egui::Align2::LEFT_TOP, self.tool.map(|k| k.label()).unwrap_or(""), egui::FontId::proportional(11.0), Color32::from_white_alpha(170));
            }
        }
        let _ = pos2;
        changed
    }
    fn undo(&mut self) -> bool {
        let Some(prev) = self.history.pop() else { return false };
        self.tentative_future = None;
        self.future.push(std::mem::replace(&mut self.objs, prev));
        self.selected = None;
        self.drag = None;
        self.style_edit = None;
        true
    }
    fn redo(&mut self) -> bool {
        let Some(next) = self.future.pop() else { return false };
        self.history.push(std::mem::replace(&mut self.objs, next));
        self.selected = None;
        self.drag = None;
        self.style_edit = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_renders_objects_and_undo() {
        let img = RgbaImage::from_pixel(120, 80, image::Rgba([255, 255, 255, 255]));
        let mut b = DrawBoard::default();
        assert_eq!(b.apply(&img, 1.0), img);
        b.snapshot();
        b.objs.push(DrawObj { kind: Kind::Rect, pts: vec![[10.0, 10.0], [60.0, 50.0]], color: [0, 0, 0, 255], width: 4.0, ..Default::default() });
        let out = b.apply(&img, 1.0);
        assert_eq!(out.get_pixel(10, 30)[0], 0);
        assert!(b.undo());
        assert_eq!(b.apply(&img, 1.0), img);
        assert!(b.redo());
        assert_ne!(b.apply(&img, 0.5), img);
        let mut t = b.clone_box();
        t.reset();
        assert_eq!(t.apply(&img, 1.0), img);
    }

    #[test]
    fn handles_reshape() {
        let orig = DrawObj { kind: Kind::Rect, pts: vec![[0.0, 0.0], [10.0, 10.0]], ..Default::default() };
        let mut o = orig.clone();
        DrawBoard::set_handle(&mut o, &orig, 2, [20.0, 30.0], false);
        assert_eq!(o.bounds(), [0.0, 0.0, 20.0, 30.0]);
        let line = DrawObj { kind: Kind::Line, pts: vec![[0.0, 0.0], [10.0, 0.0]], ..Default::default() };
        let mut l = line.clone();
        DrawBoard::set_handle(&mut l, &line, 1, [10.0, 9.0], true);
        assert!((l.pts[1][0] - l.pts[1][1]).abs() < 1e-3, "snapped to 45 degrees");
        assert_eq!(snap_angle([0.0, 0.0], [10.0, 1.0])[1].round(), 0.0);
    }

    #[test]
    fn dragged_callout_points_at_the_press() {
        let mut o = DrawObj { kind: Kind::Text, callout: CALLOUT_BUBBLE, pts: vec![[100.0, 100.0]], text: "Hi".into(), font_px: 20.0, ..Default::default() };
        o.fit_text_box();
        let size = [o.pts[1][0] - o.pts[0][0], o.pts[1][1] - o.pts[0][1]];
        place_callout(&mut o, [100.0, 100.0], [300.0, 50.0]);
        assert_eq!(o.pts[2], [100.0, 100.0], "tail tip at the drag start");
        let c = [(o.pts[0][0] + o.pts[1][0]) / 2.0, (o.pts[0][1] + o.pts[1][1]) / 2.0];
        assert!(canvas::dist(c, [300.0, 50.0]) < 1e-3, "bubble centred on the drag end: {c:?}");
        let now = [o.pts[1][0] - o.pts[0][0], o.pts[1][1] - o.pts[0][1]];
        assert!(canvas::dist(now, size) < 1e-3, "bubble keeps its size: {now:?} vs {size:?}");
    }
}
