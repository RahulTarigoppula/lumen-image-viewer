//! Tonal tools: Levels and Curves.

use super::{combo, hint, info, paint_histogram, row, section, segmented};
use crate::ui::widgets;
use crate::edit::tone::{self, LevelsParams, CHANNEL_NAMES};
use crate::edit::{EditTool, Histogram, ToolUiCtx};
use egui::{pos2, vec2, Color32, Pos2, Rect, Sense, Shape, Stroke};
use image::RgbaImage;

fn channel_color(ch: usize, dark: bool) -> Color32 {
    match ch {
        1 => Color32::from_rgb(235, 80, 80),
        2 => Color32::from_rgb(70, 200, 100),
        3 => Color32::from_rgb(80, 140, 255),
        _ => {
            if dark {
                Color32::from_gray(220)
            } else {
                Color32::from_gray(60)
            }
        }
    }
}

fn channel_hist(h: &Histogram, ch: usize) -> &[u32] {
    match ch {
        1 => &h.r,
        2 => &h.g,
        3 => &h.b,
        _ => &h.l,
    }
}

/// Width of the Levels number fields.
const FIELD_W: f32 = 72.0;

fn hist_max(data: &[u32]) -> u32 {
    data.iter().skip(1).take(254).copied().max().unwrap_or(1).max(1)
}

fn channel_picker(ui: &mut egui::Ui, ch: &mut usize) -> bool {
    row(ui, "Channel", |ui| segmented(ui, ch, &CHANNEL_NAMES))
}

fn triangle(painter: &egui::Painter, x: f32, top: f32, fill: Color32, stroke: Color32, active: bool) {
    let s = if active { 7.0 } else { 6.0 };
    let pts = vec![pos2(x, top), pos2(x - s, top + s * 1.6), pos2(x + s, top + s * 1.6)];
    painter.add(Shape::convex_polygon(pts, fill, Stroke::new(1.0, stroke)));
}

// ---------------------------------------------------------------- Levels

#[derive(Clone, Default)]
pub struct Levels {
    ch: usize,
    p: [LevelsParams; 4],
    /// Handle being dragged: 0 black, 1 gamma, 2 white, 3 out black, 4 out white.
    drag: Option<u8>,
}

impl Levels {
    fn auto(&mut self, h: &Histogram) {
        self.p = [LevelsParams::default(); 4];
        for (c, data) in [(1, &h.r), (2, &h.g), (3, &h.b)] {
            let lo = tone::percentile(data, 0.001) as f32;
            let hi = (tone::percentile(data, 0.999) as f32).max(lo + 8.0).min(255.0);
            self.p[c].in_black = lo.min(hi - 1.0);
            self.p[c].in_white = hi;
        }
    }
}

impl EditTool for Levels {
    fn title(&self) -> String {
        "Levels".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let pal = cx.palette;
        let mut c = false;
        c |= channel_picker(ui, &mut self.ch);
        ui.add_space(6.0);
        let ch = self.ch.min(3);
        let col = channel_color(ch, pal.dark);
        let width = ui.available_width();
        section(ui, cx, "Input levels");
        // Histogram + input handles.
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 136.0), Sense::click_and_drag());
        let hist_rect = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - 18.0));
        let painter = ui.painter_at(rect.expand(8.0));
        painter.rect_filled(hist_rect, 6.0, pal.input_bg);
        let data = channel_hist(cx.histogram, ch);
        paint_histogram(&painter, hist_rect.shrink(2.0), data, hist_max(data), col);
        let p = &mut self.p[ch];
        let xof = |v: f32| hist_rect.min.x + v / 255.0 * hist_rect.width();
        let vof = |x: f32| ((x - hist_rect.min.x) / hist_rect.width() * 255.0).clamp(0.0, 255.0);
        let gpos = p.in_black + (p.in_white - p.in_black) * 0.5f32.powf(p.gamma);
        // Clipped region shading.
        painter.rect_filled(Rect::from_min_max(hist_rect.min, pos2(xof(p.in_black), hist_rect.max.y)), 0.0, Color32::from_black_alpha(50));
        painter.rect_filled(Rect::from_min_max(pos2(xof(p.in_white), hist_rect.min.y), hist_rect.max), 0.0, Color32::from_black_alpha(50));
        for v in [p.in_black, gpos, p.in_white] {
            painter.line_segment([pos2(xof(v), hist_rect.min.y), pos2(xof(v), hist_rect.max.y)], Stroke::new(1.0, pal.faint.gamma_multiply(0.6)));
        }
        let top = hist_rect.max.y + 2.0;
        triangle(&painter, xof(p.in_black), top, Color32::BLACK, pal.muted, self.drag == Some(0));
        triangle(&painter, xof(gpos), top, Color32::from_gray(128), pal.muted, self.drag == Some(1));
        triangle(&painter, xof(p.in_white), top, Color32::WHITE, pal.muted, self.drag == Some(2));
        if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
            if let Some(pp) = resp.interact_pointer_pos() {
                let cands = [(0u8, xof(p.in_black)), (1, xof(gpos)), (2, xof(p.in_white))];
                self.drag = cands.iter().min_by(|a, b| (a.1 - pp.x).abs().total_cmp(&(b.1 - pp.x).abs())).map(|c| c.0);
            }
        }
        if let (Some(d), Some(pp)) = (self.drag, resp.interact_pointer_pos()) {
            if resp.is_pointer_button_down_on() && d <= 2 {
                let v = vof(pp.x).round();
                match d {
                    0 => p.in_black = v.min(p.in_white - 2.0).max(0.0),
                    2 => p.in_white = v.max(p.in_black + 2.0).min(255.0),
                    _ => {
                        let t = ((v - p.in_black) / (p.in_white - p.in_black).max(1.0)).clamp(0.02, 0.98);
                        p.gamma = (t.ln() / 0.5f32.ln()).clamp(0.1, 9.99);
                    }
                }
                c = true;
            }
        }
        if !ui.input(|i| i.pointer.primary_down()) {
            self.drag = None;
        }
        let resp = resp.on_hover_text("Drag the handles: shadows, midtones, highlights");
        if resp.double_clicked() {
            *p = LevelsParams { out_black: p.out_black, out_white: p.out_white, ..Default::default() };
            c = true;
        }
        // Three equal fields under the histogram: black at its left end, gamma in
        // the middle, white at its right end.
        let gap = ((width - 3.0 * FIELD_W) / 2.0).max(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            c |= widgets::value_box_w(ui, FIELD_W, egui::DragValue::new(&mut p.in_black).range(0.0..=253.0).speed(0.5).fixed_decimals(0)).on_hover_text("Black point").changed();
            c |= widgets::value_box_w(ui, FIELD_W, egui::DragValue::new(&mut p.gamma).range(0.1..=9.99).speed(0.01).fixed_decimals(2)).on_hover_text("Midtones (gamma)").changed();
            c |= widgets::value_box_w(ui, FIELD_W, egui::DragValue::new(&mut p.in_white).range(2.0..=255.0).speed(0.5).fixed_decimals(0)).on_hover_text("White point").changed();
        });
        if p.in_white <= p.in_black + 1.0 {
            p.in_white = (p.in_black + 2.0).min(255.0);
            p.in_black = p.in_black.min(p.in_white - 2.0);
        }
        // Output gradient.
        section(ui, cx, "Output levels");
        let (orect, oresp) = ui.allocate_exact_size(vec2(width, 30.0), Sense::click_and_drag());
        let bar = Rect::from_min_max(orect.min, pos2(orect.max.x, orect.min.y + 12.0));
        let opainter = ui.painter_at(orect.expand(8.0));
        let mut mesh = egui::Mesh::default();
        let end = if ch == 0 { Color32::WHITE } else { col };
        mesh.colored_vertex(bar.left_top(), Color32::BLACK);
        mesh.colored_vertex(bar.right_top(), end);
        mesh.colored_vertex(bar.right_bottom(), end);
        mesh.colored_vertex(bar.left_bottom(), Color32::BLACK);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        opainter.add(Shape::mesh(mesh));
        opainter.rect_stroke(bar, 2.0, Stroke::new(1.0, pal.border), egui::StrokeKind::Outside);
        let oxof = |v: f32| bar.min.x + v / 255.0 * bar.width();
        triangle(&opainter, oxof(p.out_black), bar.max.y + 2.0, Color32::BLACK, pal.muted, self.drag == Some(3));
        triangle(&opainter, oxof(p.out_white), bar.max.y + 2.0, Color32::WHITE, pal.muted, self.drag == Some(4));
        if oresp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
            if let Some(pp) = oresp.interact_pointer_pos() {
                self.drag = Some(if (pp.x - oxof(p.out_black)).abs() <= (pp.x - oxof(p.out_white)).abs() { 3 } else { 4 });
            }
        }
        if let (Some(d), Some(pp)) = (self.drag, oresp.interact_pointer_pos()) {
            if oresp.is_pointer_button_down_on() && d >= 3 {
                let v = ((pp.x - bar.min.x) / bar.width() * 255.0).clamp(0.0, 255.0).round();
                if d == 3 {
                    p.out_black = v;
                } else {
                    p.out_white = v;
                }
                c = true;
            }
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = (width - 2.0 * FIELD_W).max(2.0);
            c |= widgets::value_box_w(ui, FIELD_W, egui::DragValue::new(&mut p.out_black).range(0.0..=255.0).speed(0.5).fixed_decimals(0)).on_hover_text("Output black").changed();
            c |= widgets::value_box_w(ui, FIELD_W, egui::DragValue::new(&mut p.out_white).range(0.0..=255.0).speed(0.5).fixed_decimals(0)).on_hover_text("Output white").changed();
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(format!("{}  Auto", crate::ui::icons::MAGIC_WAND)).on_hover_text("Stretch each channel (removes color casts)").clicked() {
                self.auto(cx.histogram);
                c = true;
            }
            if ui.button("Reset channel").clicked() {
                self.p[ch] = LevelsParams::default();
                c = true;
            }
        });
        hint(ui, "Drag the handles under the histogram: shadows, midtones, highlights. Double-click the histogram to reset the input levels.");
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        tone::apply_levels(img, &self.p)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Levels::default();
    }
    fn can_apply(&self) -> bool {
        self.p != [LevelsParams::default(); 4]
    }
}

// ---------------------------------------------------------------- Curves

#[derive(Clone)]
pub struct Curves {
    ch: usize,
    pts: [Vec<[f32; 2]>; 4],
    drag: Option<usize>,
}

impl Default for Curves {
    fn default() -> Self {
        Curves { ch: 0, pts: std::array::from_fn(|_| tone::identity_curve()), drag: None }
    }
}

const PRESETS: &[(&str, &[[f32; 2]])] = &[
    ("Linear", &[[0.0, 0.0], [1.0, 1.0]]),
    ("Increase contrast", &[[0.0, 0.0], [0.25, 0.19], [0.75, 0.81], [1.0, 1.0]]),
    ("Strong contrast", &[[0.0, 0.0], [0.25, 0.13], [0.75, 0.87], [1.0, 1.0]]),
    ("Lighten", &[[0.0, 0.0], [0.5, 0.62], [1.0, 1.0]]),
    ("Darken", &[[0.0, 0.0], [0.5, 0.38], [1.0, 1.0]]),
    ("Fade (matte)", &[[0.0, 0.1], [0.5, 0.52], [1.0, 0.94]]),
    ("Negative", &[[0.0, 1.0], [1.0, 0.0]]),
];

impl EditTool for Curves {
    fn title(&self) -> String {
        "Curves".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let pal = cx.palette;
        let mut c = false;
        c |= channel_picker(ui, &mut self.ch);
        ui.add_space(6.0);
        let ch = self.ch.min(3);
        let col = channel_color(ch, pal.dark);
        let side = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click_and_drag());
        let painter = ui.painter_at(rect.expand(6.0));
        painter.rect_filled(rect, 6.0, pal.input_bg);
        let data = channel_hist(cx.histogram, ch);
        paint_histogram(&painter, rect.shrink(1.0), data, hist_max(data), col.gamma_multiply(0.45));
        let grid = Stroke::new(1.0, pal.border);
        for i in 1..4 {
            let f = i as f32 / 4.0;
            painter.line_segment([pos2(rect.min.x + f * side, rect.min.y), pos2(rect.min.x + f * side, rect.max.y)], grid);
            painter.line_segment([pos2(rect.min.x, rect.min.y + f * side), pos2(rect.max.x, rect.min.y + f * side)], grid);
        }
        painter.line_segment([rect.left_bottom(), rect.right_top()], Stroke::new(1.0, pal.faint.gamma_multiply(0.6)));
        let to_screen = |p: [f32; 2]| pos2(rect.min.x + p[0] * side, rect.max.y - p[1] * side);
        let from_screen = |p: Pos2| [((p.x - rect.min.x) / side).clamp(0.0, 1.0), ((rect.max.y - p.y) / side).clamp(0.0, 1.0)];
        // Other channels' curves, faintly.
        for other in 0..4 {
            if other == ch || self.pts[other] == tone::identity_curve() {
                continue;
            }
            let lut = tone::curve_lut(&self.pts[other]);
            let pts: Vec<Pos2> = (0..256).step_by(3).chain(std::iter::once(255)).map(|i| to_screen([i as f32 / 255.0, lut[i] as f32 / 255.0])).collect();
            painter.add(Shape::line(pts, Stroke::new(1.0, channel_color(other, pal.dark).gamma_multiply(0.45))));
        }
        let lut = tone::curve_lut(&self.pts[ch]);
        let line: Vec<Pos2> = (0..256).map(|i| to_screen([i as f32 / 255.0, lut[i] as f32 / 255.0])).collect();
        painter.add(Shape::line(line, Stroke::new(2.0, col)));

        // Interaction.
        let hit = |pts: &Vec<[f32; 2]>, p: Pos2| -> Option<usize> {
            pts.iter().enumerate().map(|(i, q)| (i, to_screen(*q).distance(p))).filter(|(_, d)| *d <= 9.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i)
        };
        let pts = &mut self.pts[ch];
        let hover = resp.hover_pos();
        if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
            if let Some(pp) = resp.interact_pointer_pos() {
                self.drag = match hit(pts, pp) {
                    Some(i) => Some(i),
                    None => {
                        let np = from_screen(pp);
                        let idx = pts.iter().position(|q| q[0] > np[0]).unwrap_or(pts.len());
                        let too_close = pts.iter().any(|q| (q[0] - np[0]).abs() < 0.02);
                        if too_close {
                            None
                        } else {
                            pts.insert(idx, np);
                            c = true;
                            Some(idx)
                        }
                    }
                };
            }
        }
        if let (Some(i), Some(pp)) = (self.drag, resp.interact_pointer_pos()) {
            if resp.is_pointer_button_down_on() && i < pts.len() {
                let mut np = from_screen(pp);
                let lo = if i > 0 { pts[i - 1][0] + 0.01 } else { 0.0 };
                let hi = if i + 1 < pts.len() { pts[i + 1][0] - 0.01 } else { 1.0 };
                np[0] = np[0].clamp(lo, hi.max(lo));
                if pts[i] != np {
                    pts[i] = np;
                    c = true;
                }
            }
        }
        if !ui.input(|i| i.pointer.primary_down()) {
            self.drag = None;
        }
        if resp.secondary_clicked() || resp.double_clicked() {
            if let Some(i) = resp.interact_pointer_pos().or(hover).and_then(|p| hit(pts, p)) {
                if pts.len() > 2 {
                    pts.remove(i);
                    self.drag = None;
                    c = true;
                }
            }
        }
        let hovered_pt = hover.and_then(|p| hit(pts, p));
        for (i, q) in pts.iter().enumerate() {
            let active = self.drag == Some(i) || hovered_pt == Some(i);
            let sp = to_screen(*q);
            painter.circle_filled(sp, if active { 5.5 } else { 4.5 }, if active { col } else { pal.panel });
            painter.circle_stroke(sp, if active { 5.5 } else { 4.5 }, Stroke::new(1.5, col));
        }
        let readout = self.drag.or(hovered_pt).and_then(|i| pts.get(i)).map(|q| format!("Input {:.0}  →  Output {:.0}", q[0] * 255.0, q[1] * 255.0));
        resp.on_hover_text("Click to add a point, drag to move, right-click or double-click to remove");
        // Always one line, so the controls below do not jump while hovering.
        info(ui, cx, readout.unwrap_or_else(|| " ".into()));
        ui.add_space(4.0);
        let names: Vec<String> = PRESETS.iter().map(|(n, _)| n.to_string()).collect();
        let mut preset = PRESETS.iter().position(|(_, p)| *p == pts.as_slice()).unwrap_or(usize::MAX);
        if combo(ui, "Preset", "curves-preset", &mut preset, &names) {
            if let Some((_, p)) = PRESETS.get(preset) {
                self.pts[ch] = p.to_vec();
                self.drag = None;
                c = true;
            }
        }
        ui.add_space(8.0);
        if ui.button("Reset channel").clicked() {
            self.pts[ch] = tone::identity_curve();
            self.drag = None;
            c = true;
        }
        hint(ui, "Click the graph to add a point, then drag it to bend the curve. Right-click or double-click a point to remove it.");
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        tone::apply_curves(img, &self.pts)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(Curves { drag: None, ..self.clone() })
    }
    fn reset(&mut self) {
        *self = Curves::default();
    }
    fn can_apply(&self) -> bool {
        // Points on the diagonal leave the image as it is.
        self.pts.iter().flatten().any(|[x, y]| (x - y).abs() > 1e-3)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl Curves {
        pub fn sample() -> Self {
            let mut c = Curves::default();
            c.pts[0] = PRESETS[2].1.to_vec();
            c.pts[3] = vec![[0.0, 0.1], [0.5, 0.45], [1.0, 1.0]];
            c
        }
    }

    #[test]
    fn levels_auto_and_identity() {
        let img = RgbaImage::from_fn(32, 32, |x, y| image::Rgba([40 + x as u8 * 3, 50 + y as u8 * 2, 90, 255]));
        let mut t = Levels::default();
        assert_eq!(t.apply(&img, 1.0), img);
        t.auto(&Histogram::compute(&img));
        let out = t.apply(&img, 1.0);
        assert!(out.pixels().any(|p| p[0] > 250));
        assert!(out.pixels().any(|p| p[0] < 5));
    }

    #[test]
    fn curves_identity_and_preset() {
        let img = RgbaImage::from_fn(16, 16, |x, y| image::Rgba([x as u8 * 16, y as u8 * 16, 7, 255]));
        let mut t = Curves::default();
        assert_eq!(t.apply(&img, 1.0), img);
        t.pts[0] = PRESETS[6].1.to_vec();
        let neg = t.apply(&img, 1.0);
        assert_eq!(neg.get_pixel(0, 0)[0], 255);
    }
}
