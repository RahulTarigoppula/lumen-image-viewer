//! Concrete adjustment tools (dialogs). `make_tool` maps a command to its tool.
//! Shared UI helpers live here; the tools themselves are grouped in submodules.

mod canvas;
mod draw;
mod filter;
mod geometry;
mod region;
mod retouch;
mod text;
mod tone;

use super::{ops, EditTool, ToolUiCtx};
use crate::ui::commands::Command;
use crate::ui::theme::{self, size};
use crate::ui::widgets;
use egui::{Color32, Rect, Sense, Vec2};
use image::RgbaImage;

pub fn make_tool(cmd: Command) -> Option<Box<dyn EditTool>> {
    Some(match cmd {
        Command::AdjustColors => Box::new(AdjustColors::default()),
        Command::Resize => Box::new(geometry::Resize::default()),
        Command::CanvasSize => Box::new(geometry::CanvasSize::default()),
        Command::FreeRotate => Box::new(geometry::FreeRotate::default()),
        Command::AddBorder => Box::new(geometry::AddBorder::default()),
        Command::Levels => Box::new(tone::Levels::default()),
        Command::Curves => Box::new(tone::Curves::default()),
        Command::Sharpen => Box::new(filter::Sharpen::default()),
        Command::Blur => Box::new(filter::Blur::default()),
        Command::Effects => Box::new(filter::Effects::default()),
        Command::AddText => Box::new(text::AddText::default()),
        Command::Watermark => Box::new(text::Watermark::default()),
        _ => return None,
    })
}

/// Interactive (on-canvas) tools by stable name, so commands can be wired
/// without this module depending on new `Command` variants:
///
/// | name           | tool                                   |
/// |----------------|----------------------------------------|
/// | `draw_board`   | Draw board (annotations)               |
/// | `clone_stamp`  | Clone stamp                            |
/// | `heal`         | Healing brush                          |
/// | `red_eye`      | Red-eye removal                        |
/// | `blur_region`  | Blur / pixelate region                 |
/// | `selection`    | Selection (adjust inside an area)      |
#[allow(dead_code)] // Wired to commands by the app (see the table above).
pub fn make_tool_by_name(name: &str) -> Option<Box<dyn EditTool>> {
    Some(match name {
        "draw_board" => Box::new(draw::DrawBoard::default()),
        "clone_stamp" => Box::new(retouch::CloneStamp::new(false)),
        "heal" => Box::new(retouch::CloneStamp::new(true)),
        "red_eye" => Box::new(region::RedEye::default()),
        "blur_region" => Box::new(region::ObscureRegion::default()),
        "selection" => Box::new(region::Selection::default()),
        _ => return None,
    })
}

//// Width of the label column used by every tool row.
pub const LABEL_W: f32 = size::LABEL_W;

/// Width of a pixel field ("60000 px").
const PX_W: f32 = 84.0;

fn row_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
    // Fixed-width column, text left-aligned (add_sized would centre it), as tall
    // as the value boxes so labels and fields share a baseline.
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.allocate_ui_with_layout(Vec2::new(LABEL_W, size::COMPACT), layout, |ui| {
        ui.set_min_width(LABEL_W);
        ui.add(egui::Label::new(label).truncate().sense(Sense::click()))
    })
    .inner
}

/// A labelled [`widgets::Slider`] row: 96 pt label, the rail, a 64 pt value box.
/// Double-clicking the label or the rail resets to `default`. Signed ranges
/// (Exposure, Contrast, Hue...) fill from 0.
fn slider_row<'a, N: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &'a mut N,
    range: std::ops::RangeInclusive<N>,
    default: N,
    build: impl FnOnce(widgets::Slider<'a, N>) -> widgets::Slider<'a, N>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let lr = row_label(ui, label);
        if lr.double_clicked() {
            *value = default;
            changed = true;
        }
        lr.on_hover_text("Double-click to reset");
        let signed = range.start().to_f64() < 0.0 && range.end().to_f64() > 0.0;
        let mut s = widgets::Slider::new(value, range).default_value(default);
        if signed {
            s = s.bipolar();
        }
        // The rail takes what the label and the value box leave.
        ui.spacing_mut().slider_width = f32::INFINITY;
        if ui.add(build(s)).changed() {
            changed = true;
        }
    });
    changed
}

/// Labeled slider with a double-click-to-reset affordance. Returns true if changed.
/// Adapts to the available panel width.
pub fn slider<N: egui::emath::Numeric>(ui: &mut egui::Ui, label: &str, value: &mut N, range: std::ops::RangeInclusive<N>, default: N, suffix: &str) -> bool {
    // Whole numbers on wide float ranges (Brightness 0, not 0.0).
    let whole = !N::INTEGRAL && range.end().to_f64() - range.start().to_f64() >= 20.0;
    slider_row(ui, label, value, range, default, |s| if whole { s.suffix(suffix).fixed_decimals(0) } else { s.suffix(suffix) })
}

/// Like [`slider`] but with a step size / fixed decimals and optional log scale.
#[allow(clippy::too_many_arguments)]
pub fn slider_ex(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, default: f32, suffix: &str, step: f64, decimals: usize, log: bool) -> bool {
    slider_row(ui, label, value, range, default, |s| {
        let s = s.suffix(suffix).fixed_decimals(decimals).logarithmic(log);
        if step > 0.0 { s.step_by(step) } else { s }
    })
}

/// Labeled integer drag value (for pixel sizes). Returns true if changed.
pub fn drag_px(ui: &mut egui::Ui, label: &str, value: &mut i64, range: std::ops::RangeInclusive<i64>) -> bool {
    row(ui, label, |ui| widgets::value_box_w(ui, PX_W, egui::DragValue::new(value).range(range).suffix(" px").speed(1.0)).changed())
}

/// A row with the standard label column followed by arbitrary widgets.
pub fn row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        row_label(ui, label);
        add(ui)
    })
    .inner
}

/// A row of controls indented to the value column (checkboxes under sliders).
pub fn indented<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
        add(ui)
    })
    .inner
}

/// A section that can be switched on (Outline, Drop shadow): the checkbox heads
/// it at the label column, its rows follow while it is on.
pub fn section_toggle(ui: &mut egui::Ui, on: &mut bool, label: &str) -> bool {
    ui.add_space(8.0);
    widgets::checkbox(ui, on, label).changed()
}

/// Section label inside a tool panel (sentence case, 11 pt semibold, muted).
pub fn section(ui: &mut egui::Ui, cx: &ToolUiCtx, text: &str) {
    ui.add_space(6.0);
    widgets::section_title(ui, cx.palette, text);
}

/// Colour swatch button (straight-alpha RGBA).
pub fn color_button(ui: &mut egui::Ui, rgba: &mut [u8; 4], alpha: bool) -> bool {
    let mut c = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
    let mode = if alpha { egui::color_picker::Alpha::OnlyBlend } else { egui::color_picker::Alpha::Opaque };
    let r = egui::color_picker::color_edit_button_srgba(ui, &mut c, mode);
    // A hairline edge, so white shows on the light panel (and black on the dark one).
    let edge = theme::palette(ui.ctx()).border;
    ui.painter().rect_stroke(r.rect, ui.visuals().widgets.inactive.corner_radius, egui::Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    if r.changed() {
        let n = c.to_srgba_unmultiplied();
        if n != *rgba {
            *rgba = if alpha { n } else { [n[0], n[1], n[2], 255] };
            return true;
        }
    }
    false
}

/// One-click colour choices next to a [`color_button`] (White, Black, None):
/// 22 pt swatches with the colour's name in the tooltip, a checkerboard for
/// transparent, and an accent ring on the one in use.
pub fn color_presets(ui: &mut egui::Ui, rgba: &mut [u8; 4], presets: &[(&str, [u8; 4])]) -> bool {
    let p = theme::palette(ui.ctx());
    let mut changed = false;
    for (name, col) in presets {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
        let r = rect.shrink(1.0);
        let painter = ui.painter();
        if col[3] == 0 {
            // Checkerboard: 4 x 4 cells.
            let cell = r.width() / 4.0;
            painter.rect_filled(r, 4.0, Color32::WHITE);
            for i in 0..4 {
                for j in 0..4 {
                    if (i + j) % 2 == 1 {
                        let c = Rect::from_min_size(r.min + Vec2::new(i as f32 * cell, j as f32 * cell), Vec2::splat(cell));
                        painter.rect_filled(c, 0.0, Color32::from_gray(200));
                    }
                }
            }
        } else {
            painter.rect_filled(r, 4.0, Color32::from_rgba_unmultiplied(col[0], col[1], col[2], col[3]));
        }
        let on = rgba == col;
        let stroke = if on { egui::Stroke::new(2.0, p.accent) } else if resp.hovered() { egui::Stroke::new(1.0, p.text) } else { egui::Stroke::new(1.0, p.border) };
        painter.rect_stroke(if on { rect.expand(1.0) } else { r }, 4.0, stroke, egui::StrokeKind::Inside);
        if resp.on_hover_text(*name).clicked() && !on {
            *rgba = *col;
            changed = true;
        }
    }
    changed
}

pub fn color_row(ui: &mut egui::Ui, label: &str, rgba: &mut [u8; 4], alpha: bool) -> bool {
    row(ui, label, |ui| color_button(ui, rgba, alpha))
}

/// Segmented control (one control, equal segments). Returns true if changed.
pub fn segmented(ui: &mut egui::Ui, value: &mut usize, labels: &[&str]) -> bool {
    widgets::segmented(ui, value, labels)
}

/// Labeled combo box over string options.
pub fn combo(ui: &mut egui::Ui, label: &str, id: &str, value: &mut usize, options: &[String]) -> bool {
    let mut changed = false;
    row(ui, label, |ui| {
        let w = (ui.available_width() - 4.0).clamp(80.0, 200.0);
        widgets::combo(id).width(w).selected_text(options.get(*value).cloned().unwrap_or_else(|| "Custom".into())).show_ui(ui, |ui| {
            for (i, o) in options.iter().enumerate() {
                if ui.selectable_label(*value == i, o).clicked() && *value != i {
                    *value = i;
                    changed = true;
                }
            }
        });
    });
    changed
}

/// Small on / off button with an icon (Bold, Italic): the one selected style.
pub fn icon_toggle(ui: &mut egui::Ui, on: &mut bool, icon: &str, tip: &str) -> bool {
    if widgets::icon_button_sized(ui, icon, tip, *on, size::COMPACT + 4.0, size::ICON_TOOLBAR).clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// 3x3 anchor picker (0 = top-left ... 8 = bottom-right).
pub fn anchor_grid(ui: &mut egui::Ui, cx: &ToolUiCtx, anchor: &mut u8) -> bool {
    let p = cx.palette;
    let cell = 20.0;
    let gap = 3.0;
    let size = Vec2::splat(cell * 3.0 + gap * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter_at(rect);
    let mut changed = false;
    for i in 0..9u8 {
        let (cxi, cyi) = ((i % 3) as f32, (i / 3) as f32);
        let r = Rect::from_min_size(rect.min + Vec2::new(cxi * (cell + gap), cyi * (cell + gap)), Vec2::splat(cell));
        let hovered = resp.hover_pos().is_some_and(|h| r.contains(h));
        let sel = *anchor == i;
        let fill = if sel { p.accent_fill } else if hovered { p.hover } else { p.raised };
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(r, 4.0, egui::Stroke::new(1.0, if sel { p.accent_fill } else { p.border }), egui::StrokeKind::Inside);
        painter.circle_filled(r.center(), if sel { 3.5 } else { 2.0 }, if sel { p.on_accent } else { p.faint });
        if resp.clicked() && hovered && !sel {
            *anchor = i;
            changed = true;
        }
    }
    resp.on_hover_text("Anchor position");
    changed
}

/// A histogram channel smoothed with a small [1 2 3 2 1] kernel (JPEG and
/// gradient images leave every other bin empty, which reads as spikes) and
/// scaled to 0..1 by the square root, so the shape of a low-key image still reads.
pub fn smooth_bins(data: &[u32], max: u32) -> Vec<f32> {
    const K: [f32; 5] = [1.0, 2.0, 3.0, 2.0, 1.0];
    let n = data.len() as isize;
    let m = max.max(1) as f32;
    (0..n)
        .map(|i| {
            let (mut sum, mut wsum) = (0.0, 0.0);
            for (k, w) in K.iter().enumerate() {
                let j = i + k as isize - 2;
                if (0..n).contains(&j) {
                    sum += data[j as usize] as f32 * w;
                    wsum += w;
                }
            }
            (sum / wsum.max(1.0) / m).sqrt().min(1.0)
        })
        .collect()
}

/// Paint one smoothed histogram channel as a filled area with a 1 pt line on top.
pub fn paint_area(painter: &egui::Painter, rect: Rect, bins: &[f32], fill: Color32, line: Color32) {
    let n = bins.len();
    if n < 2 {
        return;
    }
    let x = |i: usize| rect.min.x + i as f32 / (n - 1) as f32 * rect.width();
    let y = |v: f32| rect.max.y - v * rect.height();
    // A strip of quads down to the baseline (any shape, unlike a convex polygon).
    let mut mesh = egui::Mesh::default();
    for (i, &v) in bins.iter().enumerate() {
        mesh.colored_vertex(egui::pos2(x(i), rect.max.y), fill);
        mesh.colored_vertex(egui::pos2(x(i), y(v)), fill);
        if i > 0 {
            let k = i as u32 * 2;
            mesh.add_triangle(k - 2, k - 1, k + 1);
            mesh.add_triangle(k - 2, k + 1, k);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
    let pts: Vec<egui::Pos2> = bins.iter().enumerate().map(|(i, &v)| egui::pos2(x(i), y(v))).collect();
    painter.add(egui::Shape::line(pts, egui::Stroke::new(1.0, line)));
}

/// Paint one histogram channel into `rect` (smoothed, sqrt scaled, like `Histogram::paint`).
pub fn paint_histogram(painter: &egui::Painter, rect: Rect, data: &[u32], max: u32, color: Color32) {
    if data.is_empty() {
        return;
    }
    let bins = smooth_bins(data, max);
    paint_area(painter, rect, &bins, color.gamma_multiply(0.45), color.gamma_multiply(0.9));
}

/// Muted info text for readouts (original and resulting size, status).
pub fn info(ui: &mut egui::Ui, cx: &ToolUiCtx, text: impl Into<String>) {
    ui.add(egui::Label::new(egui::RichText::new(text.into()).size(theme::text::SMALL).color(cx.palette.muted)).wrap());
}

fn hints_id() -> egui::Id {
    egui::Id::unique("lumen-tool-hints")
}

/// How-to text for the open tool. Not drawn in the body: the tool panel shows it
/// in the tooltip of the (i) button in its header, so the controls stay compact.
pub fn hint(ui: &egui::Ui, text: impl Into<String>) {
    let text = text.into();
    ui.ctx().data_mut(|d| d.get_temp_mut_or_default::<Vec<String>>(hints_id()).push(text));
}

/// The hints given since the last call (the tool panel calls this after the tool's `ui`).
pub fn take_hints(ctx: &egui::Context) -> Vec<String> {
    ctx.data_mut(|d| d.remove_temp::<Vec<String>>(hints_id())).unwrap_or_default()
}

// Number of megapixels as a short string.
pub fn mp(w: u32, h: u32) -> String {
    let m = w as f64 * h as f64 / 1e6;
    if m >= 10.0 {
        format!("{m:.0} MP")
    } else {
        format!("{m:.1} MP")
    }
}

#[derive(Clone, PartialEq)]
pub struct AdjustColors {
    pub brightness: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub saturation: f32,
    pub vibrance: f32,
    pub hue: f32,
    pub temperature: f32,
    pub tint: f32,
    pub exposure: f32,
    /// -100..100: lift (+) or deepen (-) dark areas.
    pub shadows: f32,
    /// -100..100: recover (+) or brighten (-) bright areas.
    pub highlights: f32,
}

impl Default for AdjustColors {
    fn default() -> Self {
        AdjustColors { brightness: 0.0, contrast: 0.0, gamma: 1.0, saturation: 0.0, vibrance: 0.0, hue: 0.0, temperature: 0.0, tint: 0.0, exposure: 0.0, shadows: 0.0, highlights: 0.0 }
    }
}

impl EditTool for AdjustColors {
    fn title(&self) -> String {
        "Adjust colors".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        section(ui, cx, "Light");
        c |= slider_ex(ui, "Exposure", &mut self.exposure, -3.0..=3.0, 0.0, " EV", 0.01, 2, false);
        c |= slider(ui, "Brightness", &mut self.brightness, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Contrast", &mut self.contrast, -100.0..=100.0, 0.0, "");
        // Log scale over 1/4..4 so the neutral 1.00 sits in the middle, filled from there.
        c |= slider_row(ui, "Gamma", &mut self.gamma, 0.25..=4.0, 1.0, |s| s.origin(1.0).logarithmic(true).fixed_decimals(2));
        section(ui, cx, "Tone");
        c |= slider(ui, "Shadows", &mut self.shadows, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Highlights", &mut self.highlights, -100.0..=100.0, 0.0, "");
        section(ui, cx, "Color");
        c |= slider(ui, "Saturation", &mut self.saturation, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Vibrance", &mut self.vibrance, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Hue", &mut self.hue, -180.0..=180.0, 0.0, "°");
        section(ui, cx, "White balance");
        c |= slider(ui, "Temperature", &mut self.temperature, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Tint", &mut self.tint, -100.0..=100.0, 0.0, "");
        hint(ui, "Double-click a slider or its label to reset it.");
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        let s = self.clone();
        // Local tone mapping first (it judges brightness from the original);
        // its luminance grid is relative to the image size, so preview and
        // final match without needing `scale`.
        let toned;
        let img = if s.shadows != 0.0 || s.highlights != 0.0 {
            toned = crate::edit::tone::shadows_highlights(img, s.shadows / 100.0, s.highlights / 100.0);
            &toned
        } else {
            img
        };
        let exp = 2f32.powf(s.exposure);
        let bright = s.brightness / 100.0 * 0.5;
        let contrast = if s.contrast >= 0.0 { 1.0 + s.contrast / 100.0 * 2.0 } else { 1.0 + s.contrast / 100.0 };
        let inv_gamma = 1.0 / s.gamma.max(0.01);
        let sat = 1.0 + s.saturation / 100.0;
        let vib = s.vibrance / 100.0;
        let hue = s.hue / 360.0;
        let temp = s.temperature / 100.0 * 0.15;
        let tint = s.tint / 100.0 * 0.12;
        ops::map_pixels(img, move |[mut r, mut g, mut b]| {
            if exp != 1.0 {
                r *= exp;
                g *= exp;
                b *= exp;
            }
            r += temp;
            b -= temp;
            g -= tint;
            r += bright;
            g += bright;
            b += bright;
            r = (r - 0.5) * contrast + 0.5;
            g = (g - 0.5) * contrast + 0.5;
            b = (b - 0.5) * contrast + 0.5;
            if inv_gamma != 1.0 {
                r = r.max(0.0).powf(inv_gamma);
                g = g.max(0.0).powf(inv_gamma);
                b = b.max(0.0).powf(inv_gamma);
            }
            if sat != 1.0 || vib != 0.0 || hue != 0.0 {
                let [mut h, mut s2, l] = ops::rgb_to_hsl([r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]);
                h = (h + hue).rem_euclid(1.0);
                s2 *= sat;
                if vib != 0.0 {
                    // Vibrance boosts muted colors more than already-saturated ones.
                    s2 += vib * (1.0 - s2) * s2.max(0.15);
                }
                let c = ops::hsl_to_rgb([h, s2.clamp(0.0, 1.0), l]);
                r = c[0];
                g = c[1];
                b = c[2];
            }
            [r, g, b]
        })
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn can_apply(&self) -> bool {
        *self != Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Visual check: `LUMEN_DUMP=<in.jpg> cargo test dump_samples -- --ignored`
    /// writes sample renders next to the input.
    #[test]
    #[ignore]
    fn dump_samples() {
        let Ok(p) = std::env::var("LUMEN_DUMP") else { return };
        let img = image::open(&p).unwrap().to_rgba8();
        let dir = std::path::Path::new(&p).parent().unwrap().to_path_buf();
        let save = |n: &str, i: &RgbaImage| i.save(dir.join(format!("dump-{n}.png"))).unwrap();
        save("rotate", &geometry::FreeRotate::sample(7.5, 0).apply(&img, 1.0));
        save("rotate-expand", &geometry::FreeRotate::sample(-12.0, 1).apply(&img, 1.0));
        save("border", &geometry::AddBorder::sample().apply(&img, 1.0));
        save("text", &text::AddText::sample().apply(&img, 1.0));
        save("wm-tiled", &text::Watermark::sample().apply(&img, 1.0));
        save("curves", &tone::Curves::sample().apply(&img, 1.0));
        for k in [5usize, 6, 7, 8, 9, 10, 13] {
            save(&format!("fx{k}"), &filter::Effects::sample(k).apply(&img, 1.0));
        }
        save("motion", &filter::Blur::sample().apply(&img, 1.0));
    }

    /// Every registered tool, at default parameters, must not panic on tiny
    /// images and must keep the size when it says it does.
    #[test]
    fn all_tools_defaults_safe() {
        let cmds = [
            Command::AdjustColors,
            Command::Resize,
            Command::CanvasSize,
            Command::FreeRotate,
            Command::AddBorder,
            Command::Levels,
            Command::Curves,
            Command::Sharpen,
            Command::Blur,
            Command::Effects,
            Command::AddText,
            Command::Watermark,
        ];
        for cmd in cmds {
            let tool = make_tool(cmd).expect("tool registered");
            for (w, h) in [(1, 1), (3, 2), (40, 25)] {
                let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 9) as u8, (y * 5) as u8, 77, 255]));
                for scale in [1.0, 0.25] {
                    let out = tool.apply(&img, scale);
                    assert!(out.width() >= 1 && out.height() >= 1, "{cmd:?}");
                    if !tool.changes_size() {
                        assert_eq!(out.dimensions(), img.dimensions(), "{cmd:?}");
                    }
                }
            }
            let mut t2 = tool.clone_box();
            t2.reset();
            assert!(!t2.title().is_empty());
        }
    }

    /// Apply stays disabled until a tool would change the image: tools whose
    /// defaults leave it as it is start disabled, as Effects does with nothing
    /// picked.
    #[test]
    fn apply_needs_a_change() {
        for cmd in [Command::AdjustColors, Command::Resize, Command::CanvasSize, Command::FreeRotate, Command::Levels, Command::Curves, Command::Effects] {
            assert!(!make_tool(cmd).unwrap().can_apply(), "{cmd:?}");
        }
        for cmd in [Command::Sharpen, Command::Blur, Command::AddBorder, Command::AddText, Command::Watermark] {
            assert!(make_tool(cmd).unwrap().can_apply(), "{cmd:?}");
        }
        let changed = AdjustColors { gamma: 1.2, ..Default::default() };
        assert!(changed.can_apply());
    }

    #[test]
    fn named_tools_defaults_safe() {
        // Every name `make_tool_by_name` accepts.
        for name in ["draw_board", "clone_stamp", "heal", "red_eye", "blur_region", "selection"] {
            let mut tool = make_tool_by_name(name).expect("tool registered");
            assert!(tool.wants_canvas(), "{name}");
            assert!(!tool.undo(), "{name}: nothing to undo yet");
            for (w, h) in [(1, 1), (3, 2), (40, 25)] {
                let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 9) as u8, (y * 5) as u8, 77, 255]));
                for scale in [1.0, 0.25] {
                    let out = tool.apply(&img, scale);
                    assert_eq!(out.dimensions(), img.dimensions(), "{name}");
                    assert_eq!(out, img, "{name}: defaults change nothing");
                }
            }
            let mut t2 = tool.clone_box();
            t2.reset();
            assert!(!t2.title().is_empty());
        }
        assert!(make_tool_by_name("nope").is_none());
    }

    /// Timing of the interactive tools' final render on 50 MP:
    /// `cargo test bench_tools_50mp -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_tools_50mp() {
        use crate::edit::mask::Shape;
        use crate::edit::raster::{DrawObj, Kind};
        use crate::edit::retouch::{self, BrushStroke, Obscure};
        let img = RgbaImage::from_fn(8660, 5774, |x, y| image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 255]));
        let t = |name: &str, f: &dyn Fn() -> RgbaImage| {
            let s = std::time::Instant::now();
            let o = f();
            eprintln!("{name:>16}: {:>6} ms  ({}x{})", s.elapsed().as_millis(), o.width(), o.height());
        };
        let objs = vec![
            DrawObj { kind: Kind::Arrow, pts: vec![[100.0, 100.0], [8000.0, 5000.0]], width: 40.0, shadow: true, ..Default::default() },
            DrawObj { kind: Kind::Rect, pts: vec![[50.0, 50.0], [8600.0, 5700.0]], width: 30.0, ..Default::default() },
            DrawObj { kind: Kind::Highlighter, pts: vec![[100.0, 3000.0], [8500.0, 3100.0]], width: 200.0, color: [255, 230, 0, 255], ..Default::default() },
            DrawObj { kind: Kind::Number, pts: vec![[4000.0, 2000.0]], font_px: 400.0, number: 3, ..Default::default() },
        ];
        t("draw board", &|| crate::edit::raster::render_all(&img, &objs, 1.0));
        let stroke = |heal| BrushStroke { pts: (0..60).map(|i| [2000.0 + i as f32 * 40.0, 3000.0 + (i as f32 * 0.2).sin() * 300.0]).collect(), radius: 150.0, hardness: 0.5, opacity: 1.0, offset: [-800.0, -600.0], heal };
        t("clone stroke", &|| {
            let mut o = img.clone();
            retouch::apply_strokes(&mut o, &[stroke(false)], 1.0);
            o
        });
        t("heal stroke", &|| {
            let mut o = img.clone();
            retouch::apply_strokes(&mut o, &[stroke(true)], 1.0);
            o
        });
        let shapes = vec![Shape::Rect([500.0, 500.0, 3500.0, 2500.0]), Shape::Ellipse([4000.0, 3000.0, 7000.0, 5000.0])];
        for (n, m) in [("pixelate", Obscure::Pixelate(60.0)), ("blur region", Obscure::Blur(40.0)), ("fill region", Obscure::Fill([0, 0, 0, 255]))] {
            t(n, &|| {
                let mut o = img.clone();
                retouch::obscure(&mut o, &shapes, m, 6.0, 1.0);
                o
            });
        }
        t("red eye", &|| {
            let mut o = img.clone();
            retouch::red_eye(&mut o, &[[3000.0, 2000.0, 3400.0, 2400.0]], 1.0, 0.5, 0.5);
            o
        });
        let mut sel = region::Selection::default();
        sel.push_shape_for_bench(Shape::Ellipse([1000.0, 1000.0, 7000.0, 5000.0]));
        t("selection", &|| sel.apply(&img, 1.0));
        t("shadows/highl.", &|| AdjustColors { shadows: 60.0, highlights: 40.0, ..Default::default() }.apply(&img, 1.0));
    }

    #[test]
    fn adjust_colors_shadows_highlights() {
        let img = RgbaImage::from_fn(64, 32, |x, _| if x < 32 { image::Rgba([25, 25, 25, 255]) } else { image::Rgba([240, 240, 240, 255]) });
        let t = AdjustColors { shadows: 80.0, highlights: 60.0, ..Default::default() };
        let out = t.apply(&img, 1.0);
        assert!(out.get_pixel(4, 16)[0] > 35);
        assert!(out.get_pixel(60, 16)[0] < 235);
        assert_eq!(AdjustColors::default().apply(&img, 0.3), img);
    }
}
