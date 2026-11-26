//! Text overlay and watermark tools. All sizes are relative to the image so
//! the downscaled live preview looks identical to the full-resolution result.

use super::{anchor_grid, color_button, combo, hint, icon_toggle, info, row, section, section_toggle, segmented_tips, slider, slider_ex, tipped};
use crate::ui::widgets;
use crate::edit::memory::{self, remember_methods, Remember};
use crate::edit::text::{self as txt, Stamp, TextStyle};
use crate::edit::{geom, EditTool, ToolUiCtx};
use crate::ui::icons as I;
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn font_names() -> Vec<String> {
    txt::families().iter().map(|f| f.name.clone()).collect()
}

/// A font remembered by name (the list depends on the fonts installed); one
/// that is not there any more gives the first.
mod font_name {
    use crate::edit::text as txt;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(family: &usize, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(txt::families().get(*family).map_or("", |f| f.name.as_str()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
        let name = String::deserialize(d)?;
        Ok(txt::families().iter().position(|f| f.name == name).unwrap_or(0))
    }
}

fn opacity_color_row(ui: &mut egui::Ui, label: &str, color: &mut [u8; 4], opacity: &mut f32) -> bool {
    let mut c = false;
    ui.horizontal(|ui| {
        super::row_label(ui, label);
        c |= color_button(ui, color, false);
        ui.spacing_mut().slider_width = f32::INFINITY;
        c |= ui.add(widgets::Slider::new(opacity, 0.0..=100.0).suffix(" %").fixed_decimals(0).default_value(100.0)).on_hover_text("Opacity: lower lets the image show through").changed();
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

/// Remembers everything, the text too.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AddText {
    text: String,
    #[serde(rename = "font", with = "font_name")]
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
            // Empty: the field shows "Your text here" as a placeholder and
            // Apply stays off until something is typed.
            text: String::new(),
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

impl Remember for AddText {
    const KEY: &'static str = "add_text";
    fn recalled(&mut self) {
        memory::clamp(&mut self.size, 0.5..=40.0);
        memory::clamp(&mut self.opacity, 0.0..=100.0);
        self.align = self.align.min(2);
        memory::clamp(&mut self.spacing, 0.6..=3.0);
        self.anchor = self.anchor.min(8);
        memory::clamp(&mut self.margin, 0.0..=25.0);
        memory::clamp(&mut self.off_x, -50.0..=50.0);
        memory::clamp(&mut self.off_y, -50.0..=50.0);
        memory::clamp(&mut self.angle, -180.0..=180.0);
        memory::clamp(&mut self.outline_w, 0.5..=30.0);
        memory::clamp(&mut self.shadow_dist, 0.0..=50.0);
        memory::clamp(&mut self.shadow_blur, 0.0..=60.0);
        memory::clamp(&mut self.bg_pad, 0.0..=150.0);
        memory::clamp(&mut self.bg_radius, 0.0..=100.0);
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
        c |= ui.add(egui::TextEdit::multiline(&mut self.text).desired_rows(3).desired_width(ui.available_width()).hint_text("Your text here")).changed();
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
        c |= tipped(ui, "Height of the letters, as a percentage of the image height", |ui| slider_ex(ui, "Size", &mut self.size, 0.5..=40.0, 6.0, " %", 0.1, 1, true));
        c |= opacity_color_row(ui, "Color", &mut self.color, &mut self.opacity);
        c |= slider_ex(ui, "Line spacing", &mut self.spacing, 0.6..=3.0, 1.0, "×", 0.05, 2, false);
        section(ui, cx, "Position");
        tipped(ui, "Which side or corner of the image the text sits at", |ui| row(ui, &format!("{} Anchor", I::ANCHOR), |ui| c |= anchor_grid(ui, cx, &mut self.anchor)));
        c |= tipped(ui, "Space between the text and the image edge", |ui| slider_ex(ui, "Margin", &mut self.margin, 0.0..=25.0, 4.0, " %", 0.1, 1, false));
        c |= slider_ex(ui, "Offset X", &mut self.off_x, -50.0..=50.0, 0.0, " %", 0.1, 1, false);
        c |= slider_ex(ui, "Offset Y", &mut self.off_y, -50.0..=50.0, 0.0, " %", 0.1, 1, false);
        c |= tipped(ui, "Turn the text by this angle", |ui| slider_ex(ui, "Rotation", &mut self.angle, -180.0..=180.0, 0.0, "°", 0.5, 1, false));
        c |= section_toggle(ui, &mut self.outline, "Outline");
        if self.outline {
            c |= tipped(ui, "Thickness of the outline", |ui| slider_ex(ui, "Width", &mut self.outline_w, 0.5..=30.0, 5.0, " %", 0.5, 1, false));
            c |= super::color_row(ui, "Outline color", &mut self.outline_color, true);
        }
        c |= section_toggle(ui, &mut self.shadow, "Drop shadow");
        if self.shadow {
            c |= tipped(ui, "How far the shadow falls from the text", |ui| slider_ex(ui, "Distance", &mut self.shadow_dist, 0.0..=50.0, 5.0, " %", 0.5, 1, false));
            c |= tipped(ui, "How blurry the shadow is", |ui| slider_ex(ui, "Softness", &mut self.shadow_blur, 0.0..=60.0, 8.0, " %", 0.5, 1, false));
            c |= super::color_row(ui, "Shadow color", &mut self.shadow_color, true);
        }
        c |= tipped(ui, "A filled box behind the text so it reads on any background", |ui| section_toggle(ui, &mut self.bg, "Box behind the text"));
        if self.bg {
            c |= tipped(ui, "Color of the box (with see-through amount)", |ui| super::color_row(ui, "Box color", &mut self.bg_color, true));
            c |= tipped(ui, "Space between the text and the box edge", |ui| slider_ex(ui, "Padding", &mut self.bg_pad, 0.0..=150.0, 30.0, " %", 1.0, 0, false));
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
    remember_methods!();
}

// ---------------------------------------------------------------- Watermark

/// Remembers everything; the logo by its file, read again when the tool opens.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    /// 0 = text, 1 = image
    kind: usize,
    text: String,
    #[serde(rename = "font", with = "font_name")]
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
    /// The file the logo came from.
    logo_path: Option<PathBuf>,
    #[serde(skip)]
    logo: Option<Arc<RgbaImage>>,
    #[serde(skip)]
    logo_name: String,
    #[serde(skip)]
    error: Option<String>,
}

impl Remember for Watermark {
    const KEY: &'static str = "watermark";
    fn recalled(&mut self) {
        self.kind = self.kind.min(1);
        memory::clamp(&mut self.size, 0.5..=30.0);
        memory::clamp(&mut self.logo_size, 2.0..=100.0);
        memory::clamp(&mut self.opacity, 0.0..=100.0);
        self.layout = self.layout.min(1);
        self.anchor = self.anchor.min(8);
        memory::clamp(&mut self.margin, 0.0..=25.0);
        memory::clamp(&mut self.angle, -90.0..=90.0);
        memory::clamp(&mut self.gap, 0.0..=50.0);
        // The logo is read from its file again; one that is gone is forgotten.
        if let Some(path) = self.logo_path.take() {
            if !self.load_logo(&path) {
                self.error = None;
            }
        }
    }
    /// Reset keeps the logo.
    fn reset_to(&self) -> Self {
        Watermark { logo_path: self.logo_path.clone(), ..Watermark::default() }
    }
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
            logo_path: None,
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
        self.load_logo(&path)
    }

    fn load_logo(&mut self, path: &Path) -> bool {
        match image::open(path) {
            Ok(img) => {
                self.logo = Some(Arc::new(img.to_rgba8()));
                self.logo_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.logo_path = Some(path.to_path_buf());
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
        c |= row(ui, "Type", |ui| segmented_tips(ui, &mut self.kind, &["Text", "Image"], &["Stamp a line of text", "Stamp a logo or picture from a file"]));
        ui.add_space(4.0);
        if self.kind == 0 {
            c |= ui.add(egui::TextEdit::singleline(&mut self.text).desired_width(ui.available_width()).hint_text("© Your Name")).on_hover_text("The watermark text").changed();
            c |= combo(ui, "Font", "wm-font", &mut self.family, &font_names());
            row(ui, "Style", |ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                c |= icon_toggle(ui, &mut self.bold, I::TEXT_B, "Bold");
                ui.add_space(8.0);
                c |= widgets::checkbox(ui, &mut self.shadow, "Shadow").on_hover_text("A soft shadow so the text reads on light and dark areas").changed();
            });
            c |= tipped(ui, "Height of the letters, as a percentage of the image height", |ui| slider_ex(ui, "Size", &mut self.size, 0.5..=30.0, 4.0, " %", 0.1, 1, true));
            c |= opacity_color_row(ui, "Color", &mut self.color, &mut self.opacity);
        } else {
            row(ui, "Image", |ui| {
                if ui.button(format!("{}  Choose...", I::FOLDER_OPEN)).on_hover_text("Pick the logo or picture to stamp").clicked() {
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
            c |= tipped(ui, "Width of the logo, as a percentage of the image width", |ui| slider_ex(ui, "Width", &mut self.logo_size, 2.0..=100.0, 18.0, " %", 0.5, 1, false));
            c |= slider(ui, "Opacity", &mut self.opacity, 0.0..=100.0, 45.0, " %");
        }
        section(ui, cx, "Placement");
        c |= row(ui, "Layout", |ui| segmented_tips(ui, &mut self.layout, &["Single", "Tiled"], &["One watermark in a corner or side", "Repeat the watermark across the whole image"]));
        if self.layout == 0 {
            tipped(ui, "Which side or corner of the image the watermark sits at", |ui| row(ui, &format!("{} Anchor", I::ANCHOR), |ui| c |= anchor_grid(ui, cx, &mut self.anchor)));
            c |= tipped(ui, "Space between the watermark and the image edge", |ui| slider_ex(ui, "Margin", &mut self.margin, 0.0..=25.0, 3.0, " %", 0.1, 1, false));
        } else {
            c |= tipped(ui, "Slant of the repeated watermarks", |ui| slider_ex(ui, "Angle", &mut self.angle, -90.0..=90.0, -30.0, "°", 1.0, 0, false));
            c |= tipped(ui, "Space between the repeated watermarks", |ui| slider_ex(ui, "Spacing", &mut self.gap, 0.0..=50.0, 8.0, " %", 0.5, 1, false));
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
        *self = Watermark { logo, logo_name, ..self.reset_to() };
    }
    fn can_apply(&self) -> bool {
        if self.kind == 1 { self.logo.is_some() } else { !self.text.trim().is_empty() }
    }
    remember_methods!();
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
        let t = AddText { text: "Your text here".into(), family: usize::MAX, size: 15.0, ..Default::default() };
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
        // The default text is empty: nothing to draw, so Apply stays off.
        assert_eq!(AddText::default().apply(&full, 1.0), full);
        assert!(!AddText::default().can_apply());
        let rot = AddText { text: "Your text here".into(), angle: 30.0, bg: true, outline: true, ..Default::default() };
        assert_eq!(rot.apply(&full, 1.0).dimensions(), (400, 300));
    }

    /// Add text comes back with its text and its font, remembered by name.
    #[test]
    fn text_round_trip_with_font_name() {
        let last = txt::families().len() - 1;
        let used = AddText { text: "Summer 2026".into(), family: last, italic: true, size: 12.5, anchor: 2, outline: true, ..Default::default() };
        let v = used.remember().unwrap();
        assert_eq!(v["font"], txt::families()[last].name.as_str());
        let mut t = AddText::default();
        t.recall(&v);
        assert_eq!(t.remember(), Some(v));
        assert_eq!((t.text.as_str(), t.family, t.italic, t.size, t.anchor), ("Summer 2026", last, true, 12.5, 2));
        assert!(t.can_apply() && t.differs_from_defaults());
        // A font that is not installed any more gives the first one.
        let mut t = AddText::default();
        t.recall(&serde_json::json!({"text": "x", "font": "No Such Font"}));
        assert_eq!((t.text.as_str(), t.family), ("x", 0));
    }

    /// Watermark reads its logo from the remembered file; a file that is gone
    /// leaves no logo (and no error). Reset keeps the logo.
    #[test]
    fn watermark_remembers_logo_file() {
        let f = crate::edit::memory::tests::TempFile::new("logo");
        let png = f.0.with_extension("png");
        RgbaImage::from_pixel(6, 3, image::Rgba([255, 0, 0, 255])).save(&png).unwrap();
        let mut used = Watermark { kind: 1, opacity: 80.0, layout: 1, angle: 15.0, ..Default::default() };
        assert!(used.load_logo(&png));
        let v = used.remember().unwrap();
        let mut t = Watermark::default();
        t.recall(&v);
        assert_eq!(t.logo.as_ref().map(|l| l.dimensions()), Some((6, 3)));
        assert_eq!((t.kind, t.opacity, t.layout, t.angle), (1, 80.0, 1, 15.0));
        assert!(!t.logo_name.is_empty() && t.can_apply());
        t.reset();
        assert!(t.logo.is_some() && !t.differs_from_defaults());
        std::fs::remove_file(&png).unwrap();
        let mut t = Watermark::default();
        t.recall(&v);
        assert!(t.logo.is_none() && t.logo_path.is_none() && t.error.is_none());
        assert_eq!(t.kind, 1);
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
