//! Brush retouching: Clone stamp and Healing brush (same tool, two modes).

use super::canvas::{self, Pointer};
use super::{info, section, slider, slider_ex};
use crate::edit::retouch::{self, BrushStroke};
use crate::edit::{CanvasCtx, EditTool, ToolUiCtx};
use crate::ui::icons as I;
use egui::{Color32, Stroke};
use image::RgbaImage;

/// A redo step.
#[derive(Clone)]
enum Redo {
    Stroke(BrushStroke),
    Clear,
}

#[derive(Clone)]
pub struct CloneStamp {
    heal: bool,
    strokes: Vec<BrushStroke>,
    future: Vec<Redo>,
    /// Stroke lists removed by Clear (each one undo step). A Clear always
    /// leaves no strokes, so the newest entry is due for restoring whenever
    /// `strokes` is empty.
    cleared: Vec<Vec<BrushStroke>>,
    /// Alt+click source point (full-resolution px).
    source: Option<[f32; 2]>,
    /// Offset of the aligned clone (source - destination), fixed after the first stroke.
    offset: Option<[f32; 2]>,
    aligned: bool,
    radius: f32,
    hardness: f32,
    opacity: f32,
    painting: bool,
    hover: Option<[f32; 2]>,
    max_radius: f32,
    init: bool,
}

impl CloneStamp {
    pub fn new(heal: bool) -> Self {
        CloneStamp {
            heal,
            strokes: Vec::new(),
            future: Vec::new(),
            cleared: Vec::new(),
            source: None,
            offset: None,
            aligned: true,
            radius: 30.0,
            hardness: if heal { 0.5 } else { 0.7 },
            opacity: 1.0,
            painting: false,
            hover: None,
            max_radius: 500.0,
            init: false,
        }
    }

    fn current_offset(&self, dest: [f32; 2]) -> Option<[f32; 2]> {
        match (self.aligned, self.offset, self.source) {
            (true, Some(o), _) => Some(o),
            (_, _, Some(s)) => Some([s[0] - dest[0], s[1] - dest[1]]),
            _ => None,
        }
    }
}

impl EditTool for CloneStamp {
    fn title(&self) -> String {
        if self.heal { "Healing brush" } else { "Clone stamp" }.into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let short = cx.image_size.0.min(cx.image_size.1).max(1) as f32;
        if !self.init {
            self.init = true;
            self.radius = (short / 60.0).round().clamp(3.0, 800.0);
            self.max_radius = (short / 4.0).max(20.0);
        }
        let mut c = false;
        let mut i = self.heal as usize;
        if super::row(ui, "Mode", |ui| super::segmented(ui, &mut i, &[&format!("{}  Clone", I::STAMP), &format!("{}  Heal", I::BANDAIDS)])) {
            self.heal = i == 1;
        }
        let mut size = self.radius * 2.0;
        if slider_ex(ui, "Size", &mut size, 2.0..=self.max_radius * 2.0, (short / 30.0).round().max(6.0), " px", 1.0, 0, true) {
            self.radius = (size / 2.0).max(1.0);
        }
        let mut hard = self.hardness * 100.0;
        if slider(ui, "Hardness", &mut hard, 0.0..=100.0, if self.heal { 50.0 } else { 70.0 }, " %") {
            self.hardness = hard / 100.0;
        }
        let mut op = self.opacity * 100.0;
        if slider_ex(ui, "Opacity", &mut op, 1.0..=100.0, 100.0, " %", 1.0, 0, false) {
            self.opacity = op / 100.0;
        }
        ui.horizontal(|ui| {
            ui.add_space(super::LABEL_W + 4.0);
            if ui.checkbox(&mut self.aligned, "Aligned").on_hover_text("Keep the same source offset for every stroke").changed() && !self.aligned {
                self.offset = None;
            }
        });
        section(ui, cx, &format!("Strokes ({})", self.strokes.len()));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            if ui.add_enabled(!self.strokes.is_empty() || !self.cleared.is_empty(), egui::Button::new(format!("{}  Undo", I::ARROW_COUNTER_CLOCKWISE))).clicked() {
                c |= self.undo();
            }
            if ui.add_enabled(!self.future.is_empty(), egui::Button::new(format!("{}  Redo", I::ARROW_CLOCKWISE))).clicked() {
                c |= self.redo();
            }
            if ui.add_enabled(!self.strokes.is_empty(), egui::Button::new("Clear")).clicked() {
                // One undo step brings every stroke back.
                self.cleared.push(std::mem::take(&mut self.strokes));
                self.future.clear();
                c = true;
            }
        });
        ui.add_space(4.0);
        let status = match self.source {
            None => "Alt+click to pick the source area, then paint over what to cover.".to_string(),
            Some(_) if self.heal => "Paint over the blemish; texture comes from the source, color from around the stroke.".into(),
            Some(_) => "Paint to copy from the source (the crosshair follows your brush).".into(),
        };
        info(ui, cx, status);
        info(ui, cx, "[ and ] change the brush size. Hold Space to pan, wheel to zoom.");
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let mut out = img.clone();
        retouch::apply_strokes(&mut out, &self.strokes, scale);
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        let heal = self.heal;
        *self = CloneStamp::new(heal);
    }
    fn wants_canvas(&self) -> bool {
        true
    }
    fn canvas(&mut self, cx: &CanvasCtx, painter: &egui::Painter, resp: &egui::Response) -> bool {
        let pt = Pointer::read(cx, resp);
        let mut changed = false;
        // While painting, the brush follows the pointer even where egui does
        // not report a hover (dragging, fast moves past the view edge).
        let at = pt.hover.or(if self.painting { pt.pos } else { None });
        if at.is_some() || !self.painting {
            self.hover = at;
        }
        // Brush size keys.
        if cx.interactive && pt.hover.is_some() && !resp.ctx.egui_wants_keyboard_input() {
            let (dn, up) = resp.ctx.input(|i| (i.key_pressed(egui::Key::OpenBracket), i.key_pressed(egui::Key::CloseBracket)));
            if dn {
                self.radius = (self.radius / 1.2).max(1.0);
            }
            if up {
                self.radius = (self.radius * 1.2).min(self.max_radius);
            }
        }
        // Alt+click sets the source.
        let alt_press = cx.interactive && pt.alt && (pt.clicked || pt.drag_started);
        if alt_press {
            if let Some(p) = pt.start() {
                self.source = Some(cx.clamp_to_image(p));
                self.offset = None;
            }
        } else if (pt.drag_started || pt.clicked) && !self.painting {
            if let Some(p) = pt.start() {
                if let Some(off) = self.current_offset(p) {
                    if self.aligned {
                        self.offset = Some(off);
                    }
                    self.strokes.push(BrushStroke { pts: vec![p], radius: self.radius, hardness: self.hardness, opacity: self.opacity, offset: off, heal: self.heal });
                    self.future.clear();
                    self.painting = pt.drag_started;
                    changed = true;
                }
            }
        }
        if self.painting && pt.dragging {
            if let (Some(p), Some(s)) = (pt.pos, self.strokes.last_mut()) {
                let min_step = (s.radius * 0.15).max(cx.screen_to_px(1.5));
                if s.pts.last().is_none_or(|l| canvas::dist(*l, p) >= min_step) {
                    s.pts.push(p);
                    changed = true;
                }
            }
        }
        if self.painting && (pt.drag_stopped || !resp.ctx.input(|i| i.pointer.primary_down())) {
            self.painting = false;
        }
        // Overlay: brush ring, source crosshair, and the live stroke path.
        if let Some(h) = self.hover {
            canvas::brush_ring(cx, painter, h, self.radius, Some(self.hardness));
            if cx.interactive {
                resp.ctx.set_cursor_icon(if pt.alt { egui::CursorIcon::Cell } else { egui::CursorIcon::None });
            }
            // Mid-stroke the source moves with the brush by that stroke's offset.
            let off = if self.painting { self.strokes.last().map(|s| s.offset) } else { self.current_offset(h) };
            let src = if pt.alt { None } else { off.map(|o| [h[0] + o[0], h[1] + o[1]]) };
            if let Some(s) = src {
                canvas::brush_ring(cx, painter, s, self.radius, None);
                canvas::crosshair(cx, painter, s, Color32::from_rgb(120, 200, 255));
            }
            if pt.alt {
                canvas::crosshair(cx, painter, h, Color32::from_rgb(120, 200, 255));
            }
        }
        if let (Some(s), false) = (self.source, self.offset.is_some() && self.aligned) {
            if self.hover.is_none() {
                canvas::crosshair(cx, painter, s, Color32::from_rgb(120, 200, 255));
            }
        }
        if self.painting && !cx.preview_current {
            if let Some(s) = self.strokes.last() {
                let pts: Vec<egui::Pos2> = s.pts.iter().map(|p| cx.to_screen(*p)).collect();
                let w = (s.radius * 2.0 * cx.pts_per_px()).max(1.0);
                painter.add(egui::Shape::line(pts, Stroke::new(w, Color32::from_white_alpha(28))));
            }
        }
        changed
    }
    fn undo(&mut self) -> bool {
        if let Some(s) = self.strokes.pop() {
            self.future.push(Redo::Stroke(s));
            return true;
        }
        match self.cleared.pop() {
            Some(list) => {
                self.strokes = list;
                self.future.push(Redo::Clear);
                true
            }
            None => false,
        }
    }
    fn redo(&mut self) -> bool {
        match self.future.pop() {
            Some(Redo::Stroke(s)) => {
                self.strokes.push(s);
                true
            }
            Some(Redo::Clear) => {
                self.cleared.push(std::mem::take(&mut self.strokes));
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strokes_apply_and_undo() {
        let img = RgbaImage::from_fn(80, 40, |x, _| if x < 40 { image::Rgba([250, 10, 10, 255]) } else { image::Rgba([10, 10, 250, 255]) });
        let mut t = CloneStamp::new(false);
        assert_eq!(t.apply(&img, 1.0), img);
        t.strokes.push(BrushStroke { pts: vec![[60.0, 20.0], [70.0, 20.0]], radius: 5.0, hardness: 1.0, opacity: 1.0, offset: [-40.0, 0.0], heal: false });
        assert_eq!(t.apply(&img, 1.0).get_pixel(65, 20)[0], 250);
        assert!(t.undo());
        assert_eq!(t.apply(&img, 1.0), img);
        assert!(t.redo());
        assert!(!t.redo());
        let mut h = t.clone_box();
        h.reset();
        assert_eq!(h.apply(&img, 0.5), img);
    }

    #[test]
    fn clear_is_one_undo_step() {
        let mut t = CloneStamp::new(false);
        let s = |x: f32| BrushStroke { pts: vec![[x, 5.0]], radius: 2.0, hardness: 1.0, opacity: 1.0, offset: [1.0, 0.0], heal: false };
        t.strokes = vec![s(1.0), s(2.0), s(3.0)];
        t.cleared.push(std::mem::take(&mut t.strokes)); // what Clear does
        t.strokes.push(s(4.0));
        assert!(t.undo()); // the stroke after the clear
        assert!(t.strokes.is_empty());
        assert!(t.undo()); // the clear, in one step
        assert_eq!(t.strokes.len(), 3);
        assert!(t.redo());
        assert!(t.strokes.is_empty());
        assert!(t.redo());
        assert_eq!(t.strokes.len(), 1);
        assert!(!t.redo());
    }
}
