//! Text overlay and watermark tools. All sizes are relative to the image so
//! the downscaled live preview looks identical to the full-resolution result.

use super::{anchor_grid, color_button, combo, hint, icon_toggle, info, row, section, section_toggle, segmented, slider, slider_ex};
use crate::ui::widgets;
use crate::edit::text::{self as txt, Stamp, TextStyle};
use crate::edit::{geom, EditTool, ToolUiCtx};
use crate::ui::icons as I;
use image::RgbaImage;
use std::sync::Arc;

fn font_names() -> Vec<String> {
    txt::families().iter().map(|f| f.name.clone()).collect()
}

fn opacity_color_row(ui: &mut egui::Ui, label: &str, color: &mut [u8; 4], opacity: &mut f32) -> bool {
    let mut c = false;
    ui.horizontal(|ui| {
        super::row_label(ui, label);
        c |= color_button(ui, color, false);
        ui.spacing_mut().slider_width = f32::INFINITY;
        c |= ui.add(widgets::Slider::new(opacity, 0.0..=100.0).suffix(" %").fixed_decimals(0).default_value(100.0)).on_hover_text("Opacity").changed();
    });
    c
}

fn style_toggles(ui: &mut egui::Ui, bold: &mut bool, italic: &mut bool) -> bool {
    let mut c = false;
    c |= icon_toggle(ui, bold, I::TEXT_B, "Bold");
    c |= icon_toggle(ui, italic, I::TEXT_ITALIC, "Italic");
    c
}

/// Rotate a stamp; the whole rotated layer becomes the anchor box.
fn rotate_stamp(s: Stamp, angle: f32) -> Stamp {
    if angle.abs() < 0.05 {
        return s;
    }
    let image = geom::rotate(&s.image, angle, false, [0, 0, 0, 0]);
    let (w, h) = (image.width() as f32, image.height() as f32);
    // Keep the visible text box roughly centred: shrink the anchor box by the
    // padding the unrotated stamp carried around its text.
    let pad_x = (s.image.width() as f32 - s.anchor_w).max(0.0) / 2.0;
    let pad_y = (s.image.height() as f32 - s.anchor_h).max(0.0) / 2.0;
    let pad = pad_x.min(pad_y);
    Stamp { image, anchor_x: pad, anchor_y: pad, anchor_w: (w - 2.0 * pad).max(1.0), anchor_h: (h - 2.0 * pad).max(1.0) }
}

// ---------------------------------------------------------------- Add text

#[derive(Clone)]
pub struct AddText {
    text: String,
    family: usize,
    bold: bool,
    italic: bool,
    /// Font size as % of the image height.
    size: f32,
    color: [u8; 4],
    opacity: f32,
    align: usize,
    spacing: f32,
    anchor: u8,
    /// % of the short side.
    margin: f32,
    /// % of width / height.
    off_x: f32,
    off_y: f32,
    angle: f32,
    outline: bool,
    /// Outline / shadow / box sizes are % of the font size.
    outline_w: f32,
    outline_color: [u8; 4],
    shadow: bool,
    shadow_dist: f32,
    shadow_blur: f32,
    shadow_color: [u8; 4],
    bg: bool,
    bg_color: [u8; 4],
    bg_pad: f32,
    bg_radius: f32,
}

impl Default for AddText {
    fn default() -> Self {
        AddText {
            text: "Your text here".into(),
            family: 0,
            bold: true,
            italic: false,
            size: 6.0,
            color: [255, 255, 255, 255],
            opacity: 100.0,
            align: 1,
            spacing: 1.0,
            anchor: 4,
            margin: 4.0,
            off_x: 0.0,
            off_y: 0.0,
            angle: 0.0,
            outline: false,
            outline_w: 5.0,
            outline_color: [0, 0, 0, 255],
            shadow: true,
            shadow_dist: 5.0,
            shadow_blur: 8.0,
            shadow_color: [0, 0, 0, 150],
            bg: false,
            bg_color: [0, 0, 0, 140],
            bg_pad: 30.0,
            bg_radius: 20.0,
        }
    }
}

impl AddText {
    fn style(&self, img_h: f32) -> TextStyle {
        let px = (self.size / 100.0 * img_h).max(1.0);
        let f = px / 100.0;
        TextStyle {
            px,
            color: self.color,
            align: self.align as u8,
            line_spacing: self.spacing,
            tracking: 0.0,
            outline: if self.outline { self.outline_w * f } else { 0.0 },
            outline_color: self.outline_color,
            shadow: self.shadow,
            shadow_dx: self.shadow_dist * f,
            shadow_dy: self.shadow_dist * f,
            shadow_blur: self.shadow_blur * f,
            shadow_color: self.shadow_color,
            background: self.bg,
            bg_color: self.bg_color,
            bg_padding: self.bg_pad * f,
            bg_radius: self.bg_radius * f,
        }
    }
}

impl EditTool for AddText {
    fn title(&self) -> String {
        "Add text".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        c |= ui.add(egui::TextEdit::multiline(&mut self.text).desired_rows(3).desired_width(ui.available_width()).hint_text("Type your text...")).changed();
        ui.add_space(4.0);
        c |= combo(ui, "Font", "text-font", &mut self.family, &font_names());
        row(ui, "Style", |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            c |= style_toggles(ui, &mut self.bold, &mut self.italic);
            ui.add_space(6.0);
            for (i, (icon, tip)) in [(I::TEXT_ALIGN_LEFT, "Align left"), (I::TEXT_ALIGN_CENTER, "Center"), (I::TEXT_ALIGN_RIGHT, "Align right")].iter().enumerate() {
                if widgets::icon_button_sized(ui, icon, tip, self.align == i, crate::ui::theme::size::COMPACT + 4.0, crate::ui::theme::size::ICON_TOOLBAR).clicked() && self.align != i {
                    self.align = i;
                    c = true;
                }
            }
        });
        c |= slider_ex(ui, "Size", &mut self.size, 0.5..=40.0, 6.0, " %", 0.1, 1, true);
        c |= opacity_color_row(ui, "Color", &mut self.color, &mut self.opacity);
        c |= slider_ex(ui, "Line spacing", &mut self.spacing, 0.6..=3.0, 1.0, "×", 0.05, 2, false);
        section(ui, cx, "Position");
        row(ui, &format!("{} Anchor", I::ANCHOR), |ui| c |= anchor_grid(ui, cx, &mut self.anchor));
        c |= slider_ex(ui, "Margin", &mut self.margin, 0.0..=25.0, 4.0, " %", 0.1, 1, false);
        c |= slider_ex(ui, "Offset X", &mut self.off_x, -50.0..=50.0, 0.0, " %", 0.1, 1, false);
        c |= slider_ex(ui, "Offset Y", &mut self.off_y, -50.0..=50.0, 0.0, " %", 0.1, 1, false);
        c |= slider_ex(ui, "Rotation", &mut self.angle, -180.0..=180.0, 0.0, "°", 0.5, 1, false);
        c |= section_toggle(ui, &mut self.outline, "Outline");
        if self.outline {
            c |= slider_ex(ui, "Width", &mut self.outline_w, 0.5..=30.0, 5.0, " %", 0.5, 1, false);
            c |= super::color_row(ui, "Outline color", &mut self.outline_color, true);
        }
        c |= section_toggle(ui, &mut self.shadow, "Drop shadow");
        if self.shadow {
            c |= slider_ex(ui, "Distance", &mut self.shadow_dist, 0.0..=50.0, 5.0, " %", 0.5, 1, false);
            c |= slider_ex(ui, "Softness", &mut self.shadow_blur, 0.0..=60.0, 8.0, " %", 0.5, 1, false);
            c |= super::color_row(ui, "Shadow color", &mut self.shadow_color, true);
        }
        c |= section_toggle(ui, &mut self.bg, "Box behind the text");
        if self.bg {
            c |= super::color_row(ui, "Box color", &mut self.bg_color, true);
            c |= slider_ex(ui, "Padding", &mut self.bg_pad, 0.0..=150.0, 30.0, " %", 1.0, 0, false);
            c |= slider_ex(ui, "Corner radius", &mut self.bg_radius, 0.0..=100.0, 20.0, " %", 1.0, 0, false);
        }
        section(ui, cx, "Result");
        let px = (self.size / 100.0 * cx.image_size.1 as f32).round();
        info(ui, cx, format!("Font size {px:.0} px on the full image"));
        hint(ui, "Sizes are relative to the image height, so the preview matches the saved result.");
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        if self.text.trim().is_empty() || self.opacity <= 0.0 {
            return img.clone();
        }
        let (w, h) = (img.width() as f32, img.height() as f32);
        let font = txt::load(self.family, self.bold, self.italic);
        let stamp = rotate_stamp(txt::render_stamp(&font, &self.text, &self.style(h)), self.angle);
        let margin = self.margin / 100.0 * w.min(h);
        let (x, y) = txt::anchor_pos(w, h, stamp.anchor_w, stamp.anchor_h, self.anchor, margin, self.off_x / 100.0 * w, self.off_y / 100.0 * h);
        let mut out = img.clone();
        txt::place_stamp(&mut out, &stamp, x, y, self.opacity / 100.0);
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = AddText::default();
    }
    fn can_apply(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

// ---------------------------------------------------------------- Watermark

#[derive(Clone)]
pub struct Watermark {
    /// 0 = text, 1 = image
    kind: usize,
    text: String,
    family: usize,
    bold: bool,
    /// Text: font size % of image height. Image: logo width % of image width.
    size: f32,
    logo_size: f32,
    color: [u8; 4],
    opacity: f32,
    /// 0 = single, 1 = tiled diagonal
    layout: usize,
    anchor: u8,
    margin: f32,
    angle: f32,
    /// Tile gap, % of the short side.
    gap: f32,
    shadow: bool,
    logo: Option<Arc<RgbaImage>>,
    logo_name: String,
    error: Option<String>,
}

impl Default for Watermark {
    fn default() -> Self {
        Watermark {
            kind: 0,
            text: "© Your Name".into(),
            family: 0,
            bold: true,
            size: 4.0,
            logo_size: 18.0,
            color: [255, 255, 255, 255],
            opacity: 45.0,
            layout: 0,
            anchor: 8,
            margin: 3.0,
            angle: -30.0,
            gap: 8.0,
            shadow: true,
            logo: None,
            logo_name: String::new(),
            error: None,
        }
    }
}

impl Watermark {
    fn layer(&self, w: f32, h: f32) -> Option<Stamp> {
        if self.kind == 1 {
            let logo = self.logo.as_ref()?;
            let lw = (self.logo_size / 100.0 * w).round().max(1.0);
            let lh = (lw * logo.height() as f32 / logo.width().max(1) as f32).round().max(1.0);
            let img = crate::imaging::resize_exact(logo, geom::clamp_dim(lw as f64), geom::clamp_dim(lh as f64), crate::imaging::Filter::Lanczos3);
            let (iw, ih) = (img.width() as f32, img.height() as f32);
            return Some(Stamp { image: img, anchor_x: 0.0, anchor_y: 0.0, anchor_w: iw, anchor_h: ih });
        }
        if self.text.trim().is_empty() {
            return None;
        }
        let px = (self.size / 100.0 * h).max(1.0);
        let st = TextStyle {
            px,
            color: self.color,
            align: 1,
            shadow: self.shadow,
            shadow_dx: px * 0.04,
            shadow_dy: px * 0.04,
            shadow_blur: px * 0.06,
            shadow_color: [0, 0, 0, 120],
            ..Default::default()
        };
        Some(txt::render_stamp(&txt::load(self.family, self.bold, false), &self.text, &st))
    }

    fn pick_logo(&mut self) -> bool {
        let Some(path) = rfd::FileDialog::new().set_title("Choose a watermark image").add_filter("Images", &["png", "webp", "gif", "bmp", "tif", "tiff", "jpg", "jpeg"]).pick_file() else {
            return false;
        };
        match image::open(&path) {
            Ok(img) => {
                self.logo = Some(Arc::new(img.to_rgba8()));
                self.logo_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.error = None;
                true
            }
            Err(e) => {
                self.error = Some(format!("Couldn't open image: {e}"));
                false
            }
        }
    }
}

impl EditTool for Watermark {
    fn title(&self) -> String {
        "Watermark".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        c |= row(ui, "Type", |ui| segmented(ui, &mut self.kind, &["Text", "Image"]));
        ui.add_space(4.0);
        if self.kind == 0 {
            c |= ui.add(egui::TextEdit::singleline(&mut self.text).desired_width(ui.available_width()).hint_text("© Your Name")).changed();
            c |= combo(ui, "Font", "wm-font", &mut self.family, &font_names());
            row(ui, "Style", |ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                c |= icon_toggle(ui, &mut self.bold, I::TEXT_B, "Bold");
                ui.add_space(8.0);
                c |= widgets::checkbox(ui, &mut self.shadow, "Shadow").changed();
            });
            c |= slider_ex(ui, "Size", &mut self.size, 0.5..=30.0, 4.0, " %", 0.1, 1, true);
            c |= opacity_color_row(ui, "Color", &mut self.color, &mut self.opacity);
        } else {
            row(ui, "Image", |ui| {
                if ui.button(format!("{}  Choose...", I::FOLDER_OPEN)).clicked() {
                    c |= self.pick_logo();
                }
            });
            if !self.logo_name.is_empty() {
                info(ui, cx, format!("{}  {}", I::IMAGE, self.logo_name));
            } else {
                hint(ui, "A PNG with transparency works best.");
            }
            if let Some(e) = &self.error {
                ui.colored_label(cx.palette.danger, e);
            }
            c |= slider_ex(ui, "Width", &mut self.logo_size, 2.0..=100.0, 18.0, " %", 0.5, 1, false);
            c |= slider(ui, "Opacity", &mut self.opacity, 0.0..=100.0, 45.0, " %");
        }
        section(ui, cx, "Placement");
        c |= row(ui, "Layout", |ui| segmented(ui, &mut self.layout, &["Single", "Tiled"]));
        if self.layout == 0 {
            row(ui, &format!("{} Anchor", I::ANCHOR), |ui| c |= anchor_grid(ui, cx, &mut self.anchor));
            c |= slider_ex(ui, "Margin", &mut self.margin, 0.0..=25.0, 3.0, " %", 0.1, 1, false);
        } else {
            c |= slider_ex(ui, "Angle", &mut self.angle, -90.0..=90.0, -30.0, "°", 1.0, 0, false);
            c |= slider_ex(ui, "Spacing", &mut self.gap, 0.0..=50.0, 8.0, " %", 0.5, 1, false);
        }
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        let (w, h) = (img.width() as f32, img.height() as f32);
        let Some(layer) = self.layer(w, h) else { return img.clone() };
        let op = self.opacity / 100.0;
        let mut out = img.clone();
        if self.layout == 1 {
            txt::tile_layer(&mut out, &layer.image, self.angle, self.gap / 100.0 * w.min(h), op);
        } else {
            let margin = self.margin / 100.0 * w.min(h);
            let (x, y) = txt::anchor_pos(w, h, layer.anchor_w, layer.anchor_h, self.anchor, margin, 0.0, 0.0);
            txt::place_stamp(&mut out, &layer, x, y, op);
        }
        out
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        let (logo, logo_name) = (self.logo.take(), std::mem::take(&mut self.logo_name));
        *self = Watermark { logo, logo_name, ..Watermark::default() };
    }
    fn can_apply(&self) -> bool {
        if self.kind == 1 { self.logo.is_some() } else { !self.text.trim().is_empty() }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl AddText {
        pub fn sample() -> Self {
            AddText { text: "Lumen\nSunset at the lake".into(), outline: true, bg: true, angle: -8.0, size: 9.0, ..Default::default() }
        }
    }

    impl Watermark {
        pub fn sample() -> Self {
            Watermark { layout: 1, ..Default::default() }
        }
    }

    #[test]
    fn text_tool_draws_and_scales() {
        let full = RgbaImage::from_pixel(400, 300, image::Rgba([0, 0, 0, 255]));
        let t = AddText { family: usize::MAX, size: 15.0, ..Default::default() };
        let out = t.apply(&full, 1.0);
        let lit = out.pixels().filter(|p| p[0] > 128).count();
        assert!(lit > 100, "{lit}");
        // Preview at half size covers about a quarter of the pixels.
        let half = RgbaImage::from_pixel(200, 150, image::Rgba([0, 0, 0, 255]));
        let lit_h = t.apply(&half, 0.5).pixels().filter(|p| p[0] > 128).count();
        let ratio = lit_h as f32 / lit as f32;
        assert!((0.15..0.35).contains(&ratio), "{ratio}");
        let empty = AddText { text: "  ".into(), ..Default::default() };
        assert_eq!(empty.apply(&full, 1.0), full);
        let rot = AddText { angle: 30.0, bg: true, outline: true, ..Default::default() };
        assert_eq!(rot.apply(&full, 1.0).dimensions(), (400, 300));
    }

    #[test]
    fn watermark_text_image_tiled() {
        let full = RgbaImage::from_pixel(300, 200, image::Rgba([0, 0, 0, 255]));
        let t = Watermark { opacity: 100.0, ..Default::default() };
        assert_ne!(t.apply(&full, 1.0), full);
        let tiled = Watermark { layout: 1, opacity: 100.0, ..Default::default() };
        let out = tiled.apply(&full, 1.0);
        // Tiled covers both halves of the image.
        let left = (0..100).flat_map(|x| (0..200).map(move |y| (x, y))).filter(|&(x, y)| out.get_pixel(x, y)[0] > 100).count();
        let right = (200..300).flat_map(|x| (0..200).map(move |y| (x, y))).filter(|&(x, y)| out.get_pixel(x, y)[0] > 100).count();
        assert!(left > 0 && right > 0);
        let logo = Arc::new(RgbaImage::from_pixel(10, 5, image::Rgba([255, 0, 0, 255])));
        let im = Watermark { kind: 1, logo: Some(logo), opacity: 100.0, logo_size: 10.0, margin: 0.0, ..Default::default() };
        let o = im.apply(&full, 1.0);
        assert_eq!(o.get_pixel(299, 199), &image::Rgba([255, 0, 0, 255]));
        let no_logo = Watermark { kind: 1, ..Default::default() };
        assert_eq!(no_logo.apply(&full, 1.0), full);
    }
}
