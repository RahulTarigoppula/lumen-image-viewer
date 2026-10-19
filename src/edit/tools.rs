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

/// Width of the label column used by every tool row.
pub const LABEL_W: f32 = 96.0;

fn row_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
    // Fixed-width column, text left-aligned (add_sized would centre it).
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.allocate_ui_with_layout(Vec2::new(LABEL_W, 18.0), layout, |ui| {
        ui.set_min_width(LABEL_W);
        ui.add(egui::Label::new(label).truncate().sense(Sense::click()))
    })
    .inner
}

/// Labeled slider with a double-click-to-reset affordance. Returns true if changed.
/// Adapts to the available panel width.
pub fn slider<N: egui::emath::Numeric>(ui: &mut egui::Ui, label: &str, value: &mut N, range: std::ops::RangeInclusive<N>, default: N, suffix: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let lr = row_label(ui, label);
        if lr.double_clicked() {
            *value = default;
            changed = true;
        }
        lr.on_hover_text("Double-click to reset");
        ui.spacing_mut().slider_width = (ui.available_width() - 74.0).max(60.0);
        let r = ui.add(egui::Slider::new(value, range).suffix(suffix).clamping(egui::SliderClamping::Always));
        if r.changed() {
            changed = true;
        }
        if r.double_clicked() {
            *value = default;
            changed = true;
        }
    });
    changed
}

/// Like [`slider`] but with a step size / fixed decimals and optional log scale.
#[allow(clippy::too_many_arguments)]
pub fn slider_ex(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, default: f32, suffix: &str, step: f64, decimals: usize, log: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let lr = row_label(ui, label);
        if lr.double_clicked() {
            *value = default;
            changed = true;
        }
        lr.on_hover_text("Double-click to reset");
        ui.spacing_mut().slider_width = (ui.available_width() - 74.0).max(60.0);
        let mut s = egui::Slider::new(value, range).suffix(suffix).clamping(egui::SliderClamping::Always).fixed_decimals(decimals).logarithmic(log);
        if step > 0.0 {
            s = s.step_by(step);
        }
        let r = ui.add(s);
        if r.changed() {
            changed = true;
        }
        if r.double_clicked() {
            *value = default;
            changed = true;
        }
    });
    changed
}

/// Labeled integer drag value (for pixel sizes). Returns true if changed.
pub fn drag_px(ui: &mut egui::Ui, label: &str, value: &mut i64, range: std::ops::RangeInclusive<i64>) -> bool {
    row(ui, label, |ui| ui.add(egui::DragValue::new(value).range(range).suffix(" px").speed(1.0)).changed())
}

/// A row with the standard label column followed by arbitrary widgets.
pub fn row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        row_label(ui, label);
        add(ui)
    })
    .inner
}

/// Small muted section heading.
pub fn section(ui: &mut egui::Ui, cx: &ToolUiCtx, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text.to_uppercase()).size(10.5).color(cx.palette.muted).strong());
    ui.add_space(2.0);
}

/// Colour swatch button (straight-alpha RGBA).
pub fn color_button(ui: &mut egui::Ui, rgba: &mut [u8; 4], alpha: bool) -> bool {
    let mut c = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
    let mode = if alpha { egui::color_picker::Alpha::OnlyBlend } else { egui::color_picker::Alpha::Opaque };
    if egui::color_picker::color_edit_button_srgba(ui, &mut c, mode).changed() {
        let n = c.to_srgba_unmultiplied();
        if n != *rgba {
            *rgba = if alpha { n } else { [n[0], n[1], n[2], 255] };
            return true;
        }
    }
    false
}

pub fn color_row(ui: &mut egui::Ui, label: &str, rgba: &mut [u8; 4], alpha: bool) -> bool {
    row(ui, label, |ui| color_button(ui, rgba, alpha))
}

/// Segmented control (a row of selectable labels). Returns true if changed.
pub fn segmented(ui: &mut egui::Ui, value: &mut usize, labels: &[&str]) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, l) in labels.iter().enumerate() {
            if ui.selectable_label(*value == i, *l).clicked() && *value != i {
                *value = i;
                changed = true;
            }
        }
    });
    changed
}

/// Labeled combo box over string options.
pub fn combo(ui: &mut egui::Ui, label: &str, id: &str, value: &mut usize, options: &[String]) -> bool {
    let mut changed = false;
    row(ui, label, |ui| {
        let w = (ui.available_width() - 4.0).clamp(80.0, 200.0);
        egui::ComboBox::from_id_salt(id).width(w).selected_text(options.get(*value).cloned().unwrap_or_default()).show_ui(ui, |ui| {
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

/// 3×3 anchor picker (0 = top-left ... 8 = bottom-right).
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
        let fill = if sel { p.accent } else if hovered { p.hover } else { p.raised };
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(r, 4.0, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        painter.circle_filled(r.center(), if sel { 3.5 } else { 2.0 }, if sel { Color32::WHITE } else { p.faint });
        if resp.clicked() && hovered && !sel {
            *anchor = i;
            changed = true;
        }
    }
    resp.on_hover_text("Anchor position");
    changed
}

/// Paint one histogram channel into `rect` (sqrt scaled, like `Histogram::paint`).
pub fn paint_histogram(painter: &egui::Painter, rect: Rect, data: &[u32], max: u32, color: Color32) {
    if data.is_empty() {
        return;
    }
    let mut mesh = egui::Mesh::default();
    let w = rect.width() / data.len() as f32;
    for (i, &v) in data.iter().enumerate() {
        let hgt = ((v as f32 / max.max(1) as f32).sqrt().min(1.0)) * rect.height();
        let x = rect.min.x + i as f32 * w;
        mesh.add_colored_rect(Rect::from_min_max(egui::pos2(x, rect.max.y - hgt), egui::pos2(x + w + 0.3, rect.max.y)), color);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Muted one-line info text.
pub fn info(ui: &mut egui::Ui, cx: &ToolUiCtx, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).size(11.5).color(cx.palette.muted));
}

/// Number of megapixels as a short string.
pub fn mp(w: u32, h: u32) -> String {
    let m = w as f64 * h as f64 / 1e6;
    if m >= 10.0 {
        format!("{m:.0} MP")
    } else {
        format!("{m:.1} MP")
    }
}

#[derive(Clone)]
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
    fn ui(&mut self, ui: &mut egui::Ui, _cx: &ToolUiCtx) -> bool {
        let mut c = false;
        c |= slider(ui, "Exposure", &mut self.exposure, -3.0..=3.0, 0.0, " EV");
        c |= slider(ui, "Brightness", &mut self.brightness, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Contrast", &mut self.contrast, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Gamma", &mut self.gamma, 0.2..=3.0, 1.0, "");
        ui.separator();
        c |= slider(ui, "Shadows", &mut self.shadows, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Highlights", &mut self.highlights, -100.0..=100.0, 0.0, "");
        ui.separator();
        c |= slider(ui, "Saturation", &mut self.saturation, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Vibrance", &mut self.vibrance, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Hue", &mut self.hue, -180.0..=180.0, 0.0, "°");
        ui.separator();
        c |= slider(ui, "Temperature", &mut self.temperature, -100.0..=100.0, 0.0, "");
        c |= slider(ui, "Tint", &mut self.tint, -100.0..=100.0, 0.0, "");
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
