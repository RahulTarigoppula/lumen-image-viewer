//! Region tools drawn directly on the image: Blur / pixelate region (obscure
//! sensitive details), Selection (apply another adjustment inside an area)
//! and Red-eye removal.

use super::canvas::{self, NewShape, ShapeEditor};
use super::{color_row, info, row, section, segmented, slider, slider_ex};
use crate::edit::mask::{self, Shape};
use crate::edit::retouch::{self, Obscure};
use crate::edit::{CanvasCtx, EditTool, ToolUiCtx};
use egui::Color32;
use image::RgbaImage;

fn short_side(cx: &ToolUiCtx) -> f32 {
    cx.image_size.0.min(cx.image_size.1).max(1) as f32
}

fn shape_kind_row(ui: &mut egui::Ui, kind: &mut NewShape, lasso: bool) {
    use crate::ui::icons as I;
    let mut i = *kind as usize;
    let rect = format!("{}  Rectangle", I::SQUARE);
    let ell = format!("{}  Ellipse", I::CIRCLE);
    let las = format!("{}  Lasso", I::LASSO);
    let labels: Vec<&str> = if lasso { vec![&rect, &ell, &las] } else { vec![&rect, &ell] };
    row(ui, "Shape", |ui| segmented(ui, &mut i, &labels));
    *kind = match i {
        1 => NewShape::Ellipse,
        2 if lasso => NewShape::Lasso,
        _ => NewShape::Rect,
    };
}

// ---------------------------------------------------------------- obscure

#[derive(Clone)]
pub struct ObscureRegion {
    ed: ShapeEditor,
    kind: NewShape,
    /// 0 pixelate, 1 blur, 2 solid fill
    mode: usize,
    /// Mosaic cell / blur strength in full-resolution px.
    cell: f32,
    blur: f32,
    color: [u8; 4],
    feather: f32,
    init: bool,
}

impl Default for ObscureRegion {
    fn default() -> Self {
        ObscureRegion { ed: ShapeEditor::default(), kind: NewShape::Rect, mode: 0, cell: 16.0, blur: 12.0, color: [0, 0, 0, 255], feather: 0.0, init: false }
    }
}

impl ObscureRegion {
    fn mode(&self) -> Obscure {
        match self.mode {
            1 => Obscure::Blur(self.blur),
            2 => Obscure::Fill(self.color),
            _ => Obscure::Pixelate(self.cell),
        }
    }
}

impl EditTool for ObscureRegion {
    fn title(&self) -> String {
        "Blur / pixelate region".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let short = short_side(cx);
        if !self.init {
            self.init = true;
            self.cell = (short / 45.0).round().clamp(6.0, 400.0);
            self.blur = (short / 70.0).round().clamp(4.0, 400.0);
        }
        let mut c = false;
        shape_kind_row(ui, &mut self.kind, false);
        c |= row(ui, "Effect", |ui| segmented(ui, &mut self.mode, &["Pixelate", "Blur", "Solid"]));
        match self.mode {
            1 => c |= slider_ex(ui, "Strength", &mut self.blur, 1.0..=(short / 4.0).max(10.0), (short / 70.0).round().max(4.0), " px", 1.0, 0, true),
            2 => c |= color_row(ui, "Colour", &mut self.color, false),
            _ => c |= slider_ex(ui, "Cell size", &mut self.cell, 2.0..=(short / 4.0).max(10.0), (short / 45.0).round().max(6.0), " px", 1.0, 0, true),
        }
        c |= slider_ex(ui, "Feather", &mut self.feather, 0.0..=(short / 20.0).max(5.0), 0.0, " px", 0.5, 1, false);
        section(ui, cx, &format!("Areas ({})", self.ed.shapes.len()));
        c |= canvas::shape_buttons(ui, &mut self.ed);
        ui.add_space(4.0);
        info(ui, cx, "Drag on the image to cover an area. Click to select, drag to move, drag a corner to resize, Del removes. Shift draws a square / circle. Hold Space to pan.");
        if self.mode == 1 {
            info(ui, cx, "Tip: for faces and text that must stay unreadable, Pixelate or Solid is safer than Blur.");
        }
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let mut out = img.clone();
        retouch::obscure(&mut out, self.ed.committed(), self.mode(), self.feather, scale);
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = ObscureRegion::default();
    }
    fn wants_canvas(&self) -> bool {
        true
    }
    fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response) -> bool {
        self.ed.canvas(cx, painter, resp, self.kind, Color32::WHITE)
    }
    fn undo(&mut self) -> bool {
        self.ed.undo()
    }
    fn redo(&mut self) -> bool {
        self.ed.redo()
    }
}

// ---------------------------------------------------------------- selection

/// Adjustments that can run inside a selection (size-preserving tools only).
const INNER: [&str; 6] = ["Adjust colors", "Levels", "Curves", "Sharpen", "Blur", "Effects"];

fn make_inner(i: usize) -> Box<dyn EditTool> {
    match i {
        1 => Box::new(super::tone::Levels::default()),
        2 => Box::new(super::tone::Curves::default()),
        3 => Box::new(super::filter::Sharpen::default()),
        4 => Box::new(super::filter::Blur::default()),
        5 => Box::new(super::filter::Effects::default()),
        _ => Box::new(super::AdjustColors::default()),
    }
}

pub struct Selection {
    ed: ShapeEditor,
    kind: NewShape,
    feather: f32,
    invert: bool,
    which: usize,
    inner: Box<dyn EditTool>,
    show_outline: bool,
    init: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Selection { ed: ShapeEditor::default(), kind: NewShape::Rect, feather: 8.0, invert: false, which: 0, inner: make_inner(0), show_outline: true, init: false }
    }
}

impl Clone for Selection {
    fn clone(&self) -> Self {
        Selection { ed: self.ed.clone(), inner: self.inner.clone_box(), ..*self }
    }
}

// `..*self` above needs the remaining fields to be Copy; keep them simple.
impl Selection {
    fn margin(&self, w: u32, h: u32, scale: f32) -> f32 {
        // Room for neighbourhood filters (blur, sharpen) around the selection.
        (w.max(h) as f32 * 0.03).max(16.0) + self.feather * scale * 3.0
    }
}

impl EditTool for Selection {
    fn title(&self) -> String {
        "Selection".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let short = short_side(cx);
        if !self.init {
            self.init = true;
            self.feather = (short / 200.0).round().clamp(1.0, 200.0);
        }
        let mut c = false;
        shape_kind_row(ui, &mut self.kind, true);
        c |= slider_ex(ui, "Feather", &mut self.feather, 0.0..=(short / 10.0).max(5.0), (short / 200.0).round().max(1.0), " px", 0.5, 1, true);
        ui.horizontal(|ui| {
            ui.add_space(super::LABEL_W + 4.0);
            c |= ui.checkbox(&mut self.invert, "Invert (outside)").changed();
            ui.checkbox(&mut self.show_outline, "Show outline");
        });
        section(ui, cx, &format!("Areas ({})", self.ed.shapes.len()));
        c |= canvas::shape_buttons(ui, &mut self.ed);
        info(ui, cx, "Drag on the image to select. Several areas add up; Shift+drag starts a new area inside another.");
        section(ui, cx, "Adjustment");
        let opts: Vec<String> = INNER.iter().map(|s| s.to_string()).collect();
        let before = self.which;
        if super::combo(ui, "Apply", "selection-inner", &mut self.which, &opts) && self.which != before {
            self.inner = make_inner(self.which);
            c = true;
        }
        ui.add_space(4.0);
        egui::Frame::NONE.inner_margin(egui::Margin { left: 6, right: 0, top: 2, bottom: 2 }).show(ui, |ui| {
            c |= self.inner.ui(ui, cx);
        });
        if self.ed.shapes.is_empty() && !self.invert {
            info(ui, cx, "Nothing is selected yet, so nothing changes.");
        }
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let (w, h) = img.dimensions();
        let shapes: Vec<Shape> = self.ed.committed().iter().map(|s| s.scaled(scale)).collect();
        let m = mask::shapes_mask(&shapes, self.feather * scale, w, h);
        let mut out = img.clone();
        if self.invert {
            let fx = self.inner.apply(img, scale);
            if fx.dimensions() == img.dimensions() {
                mask::blend_masked(&mut out, &fx, (0, 0), &m, true, 1.0);
            }
            return out;
        }
        if m.is_empty() {
            return out;
        }
        let pad = self.margin(w, h, scale);
        let (x0, y0, x1, y1) = m.bounds();
        let Some(b) = mask::clip_box([x0 as f32 - pad, y0 as f32 - pad, x1 as f32 + pad, y1 as f32 + pad], w, h) else { return out };
        let region = image::imageops::crop_imm(img, b.0, b.1, b.2, b.3).to_image();
        let fx = self.inner.apply(&region, scale);
        if fx.dimensions() == region.dimensions() {
            mask::blend_masked(&mut out, &fx, (b.0, b.1), &m, false, 1.0);
        }
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Selection::default();
    }
    fn wants_canvas(&self) -> bool {
        true
    }
    fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response) -> bool {
        let p = if self.show_outline || self.ed.is_dragging() { painter.clone() } else {
            let mut q = painter.clone();
            q.set_opacity(0.0);
            q
        };
        self.ed.canvas(cx, &p, resp, self.kind, Color32::WHITE)
    }
    fn undo(&mut self) -> bool {
        self.ed.undo()
    }
    fn redo(&mut self) -> bool {
        self.ed.redo()
    }
}

// ---------------------------------------------------------------- red eye

#[derive(Clone)]
pub struct RedEye {
    ed: ShapeEditor,
    sensitivity: f32,
    darken: f32,
    init: bool,
    click_size: f32,
}

impl Default for RedEye {
    fn default() -> Self {
        RedEye { ed: ShapeEditor::default(), sensitivity: 50.0, darken: 40.0, init: false, click_size: 40.0 }
    }
}

impl EditTool for RedEye {
    fn title(&self) -> String {
        "Red-eye removal".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        if !self.init {
            self.init = true;
            self.click_size = (short_side(cx) / 30.0).clamp(8.0, 600.0);
        }
        let mut c = false;
        c |= slider(ui, "Sensitivity", &mut self.sensitivity, 0.0..=100.0, 50.0, " %");
        c |= slider(ui, "Darken", &mut self.darken, 0.0..=100.0, 40.0, " %");
        section(ui, cx, &format!("Eyes ({})", self.ed.shapes.len()));
        c |= canvas::shape_buttons(ui, &mut self.ed);
        ui.add_space(4.0);
        info(ui, cx, "Drag a box around each red pupil, or just click on it. Only strongly red pixels inside the box change. Raise Sensitivity if some red remains.");
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let mut out = img.clone();
        let boxes: Vec<[f32; 4]> = self.ed.committed().iter().map(|s| s.bbox()).collect();
        retouch::red_eye(&mut out, &boxes, scale, self.sensitivity / 100.0, self.darken / 100.0);
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = RedEye::default();
    }
    fn wants_canvas(&self) -> bool {
        true
    }
    fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response) -> bool {
        // A plain click on empty space drops a default-sized eye box.
        if cx.interactive && resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos().map(|p| cx.to_image(p)) {
                if !self.ed.shapes.iter().any(|s| s.contains(p)) {
                    let r = self.click_size / 2.0;
                    self.ed.push(Shape::Ellipse([p[0] - r, p[1] - r, p[0] + r, p[1] + r]));
                    return true;
                }
            }
        }
        self.ed.canvas(cx, painter, resp, NewShape::Ellipse, Color32::from_rgb(255, 120, 120))
    }
    fn undo(&mut self) -> bool {
        self.ed.undo()
    }
    fn redo(&mut self) -> bool {
        self.ed.redo()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_limits_effect() {
        let img = RgbaImage::from_pixel(100, 80, image::Rgba([100, 100, 100, 255]));
        let mut s = Selection { feather: 0.0, which: 5, inner: make_inner(5), ..Default::default() };
        // Effects default = grayscale; use invert-like visible effect via Adjust colors instead.
        s.inner = Box::new(super::super::AdjustColors { brightness: 60.0, ..Default::default() });
        assert_eq!(s.apply(&img, 1.0), img, "empty selection is a no-op");
        s.ed.push(Shape::Rect([10.0, 10.0, 40.0, 40.0]));
        let out = s.apply(&img, 1.0);
        assert!(out.get_pixel(25, 25)[0] > 120);
        assert_eq!(out.get_pixel(80, 60)[0], 100);
        s.invert = true;
        let inv = s.apply(&img, 0.5);
        let small = crate::imaging::resize_to_fit(&img, 50, 40);
        let inv_small = s.apply(&small, 0.5);
        assert_eq!(inv.dimensions(), img.dimensions());
        assert_eq!(inv_small.get_pixel(12, 12)[0], 100, "inside stays when inverted");
        assert!(inv_small.get_pixel(40, 30)[0] > 120);
    }

    #[test]
    fn region_tools_defaults_safe() {
        let img = RgbaImage::from_pixel(9, 7, image::Rgba([10, 200, 30, 255]));
        let mut o = ObscureRegion::default();
        o.ed.push(Shape::Ellipse([-5.0, -5.0, 4.0, 4.0]));
        for mode in 0..3 {
            o.mode = mode;
            assert_eq!(o.apply(&img, 0.3).dimensions(), (9, 7));
        }
        let mut r = RedEye::default();
        r.ed.push(Shape::Rect([0.0, 0.0, 100.0, 100.0]));
        assert_eq!(r.apply(&img, 1.0), img, "green is not red");
    }
}
