//! Filter tools: Sharpen (unsharp mask), Blur, and the Effects gallery.

use super::{color_row, combo, hint, info, row, section, segmented_tips, slider, slider_ex, tipped};
use crate::ui::widgets;
use crate::edit::memory::{self, remember_methods, Remember};
use crate::edit::{effects as fx, filters, EditTool, ToolUiCtx};
use egui::{vec2, Color32, Sense, Stroke};
use image::RgbaImage;
use serde::{Deserialize, Serialize};

/// Spatial radius for a render at `scale` (preview size / full size). Kernels
/// treat sigma < 0.2 as a no-op, so on a downscaled preview tiny radii are
/// raised to a small visible floor instead of silently disappearing; the final render
/// (scale 1) always uses the exact value.
const PREVIEW_MIN_SIGMA: f32 = 0.35;

fn scaled_radius(r: f32, scale: f32) -> f32 {
    let s = r * scale;
    if scale < 1.0 && r >= 0.2 {
        s.max(PREVIEW_MIN_SIGMA)
    } else {
        s
    }
}

/// Preview scale (preview size / full size) from the UI context.
fn preview_scale(cx: &ToolUiCtx) -> f32 {
    match cx.preview {
        Some(p) if cx.image_size.0 > 0 => (p.width() as f32 / cx.image_size.0 as f32).min(1.0),
        _ => 1.0,
    }
}

/// Note shown when a radius is too fine to show up in the downscaled preview.
fn fine_detail_note(ui: &mut egui::Ui, cx: &ToolUiCtx, radius: f32) {
    if radius * preview_scale(cx) < 0.5 {
        info(ui, cx, "Detail is finer than the preview resolution: the preview is approximate. Check at 100% after applying.");
    }
}

// ---------------------------------------------------------------- Sharpen

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sharpen {
    amount: f32,
    radius: f32,
    threshold: f32,
}

impl Default for Sharpen {
    fn default() -> Self {
        Sharpen { amount: 80.0, radius: 1.0, threshold: 2.0 }
    }
}

impl Remember for Sharpen {
    const KEY: &'static str = "sharpen";
    fn recalled(&mut self) {
        memory::clamp(&mut self.amount, 0.0..=500.0);
        memory::clamp(&mut self.radius, 0.1..=100.0);
        memory::clamp(&mut self.threshold, 0.0..=255.0);
    }
}

impl EditTool for Sharpen {
    fn title(&self) -> String {
        "Sharpen (unsharp mask)".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for (name, a, r, t, tip) in [
                ("Subtle", 50.0, 0.8, 2.0, "A light touch for most photos"),
                ("Medium", 100.0, 1.0, 2.0, "Clearly crisper detail"),
                ("Strong", 180.0, 1.4, 3.0, "Heavy sharpening for soft or slightly blurred photos"),
                ("Web", 60.0, 0.5, 0.0, "Fine sharpening for images made smaller for the web"),
                ("Clarity", 25.0, 30.0, 0.0, "Adds punch to midtone contrast instead of edges"),
            ] {
                if ui.button(name).on_hover_text(tip).clicked() {
                    *self = Sharpen { amount: a, radius: r, threshold: t };
                    c = true;
                }
            }
        });
        ui.add_space(4.0);
        c |= tipped(ui, "How strong the sharpening is", |ui| slider_ex(ui, "Amount", &mut self.amount, 0.0..=500.0, 80.0, " %", 1.0, 0, false));
        c |= tipped(ui, "How wide the edges that get sharpened are; small for fine detail", |ui| slider_ex(ui, "Radius", &mut self.radius, 0.1..=100.0, 1.0, " px", 0.1, 1, true));
        c |= tipped(ui, "Leave smooth areas alone; raise it to avoid sharpening noise", |ui| slider_ex(ui, "Threshold", &mut self.threshold, 0.0..=255.0, 2.0, "", 1.0, 0, false));
        ui.add_space(6.0);
        hint(ui, "Judge sharpening at 100% zoom: the preview is downscaled.");
        fine_detail_note(ui, cx, self.radius);
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        filters::unsharp_mask(img, self.amount / 100.0, scaled_radius(self.radius, scale), self.threshold.round().clamp(0.0, 255.0) as u8)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Sharpen::default();
    }
    fn can_apply(&self) -> bool {
        self.amount > 0.0
    }
    remember_methods!();
}

// ---------------------------------------------------------------- Blur

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Blur {
    /// 0 Gaussian, 1 Box, 2 Motion, 3 Surface
    mode: usize,
    radius: f32,
    angle: f32,
    distance: f32,
    threshold: f32,
}

impl Default for Blur {
    fn default() -> Self {
        Blur { mode: 0, radius: 4.0, angle: 0.0, distance: 30.0, threshold: 25.0 }
    }
}

impl Remember for Blur {
    const KEY: &'static str = "blur";
    fn recalled(&mut self) {
        self.mode = self.mode.min(3);
        memory::clamp(&mut self.radius, 0.1..=250.0);
        memory::clamp(&mut self.angle, -180.0..=180.0);
        memory::clamp(&mut self.distance, 1.0..=500.0);
        memory::clamp(&mut self.threshold, 1.0..=100.0);
    }
}

impl EditTool for Blur {
    fn title(&self) -> String {
        "Blur".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        c |= segmented_tips(
            ui,
            &mut self.mode,
            &["Gaussian", "Box", "Motion", "Smart"],
            &["Natural soft focus", "Even averaging; fast, slightly boxy", "Streaks in one direction, like camera shake", "Smooths flat areas but keeps edges sharp"],
        );
        ui.add_space(4.0);
        match self.mode {
            2 => {
                c |= tipped(ui, "Direction of the streaks", |ui| slider_ex(ui, "Angle", &mut self.angle, -180.0..=180.0, 0.0, "°", 1.0, 0, false));
                c |= tipped(ui, "Length of the streaks", |ui| slider_ex(ui, "Distance", &mut self.distance, 1.0..=500.0, 30.0, " px", 1.0, 0, true));
            }
            _ => {
                c |= tipped(ui, "How far each pixel is blended with its neighbours; larger is blurrier", |ui| slider_ex(ui, "Radius", &mut self.radius, 0.1..=250.0, 4.0, " px", 0.1, 1, true));
                if self.mode == 3 {
                    c |= tipped(ui, "How different neighbours may be and still get smoothed; higher blurs more edges", |ui| slider_ex(ui, "Threshold", &mut self.threshold, 1.0..=100.0, 25.0, "", 1.0, 0, false));
                }
            }
        }
        ui.add_space(6.0);
        hint(
            ui,
            match self.mode {
                1 => "Uniform averaging - fast, slightly boxy.",
                2 => "Streaks along a direction, like camera shake.",
                3 => "Smooths flat areas but keeps edges sharp (good for skin and noise).",
                _ => "Natural soft focus.",
            },
        );
        if self.mode != 2 {
            fine_detail_note(ui, cx, self.radius);
        }
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let r = scaled_radius(self.radius, scale);
        match self.mode {
            1 => filters::box_blur(img, r.round().max(0.0) as usize),
            2 => filters::motion_blur(img, self.angle, self.distance * scale),
            3 => filters::surface_blur(img, r, self.threshold),
            _ => filters::gaussian_blur(img, r),
        }
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Blur::default();
    }
    remember_methods!();
}

// ---------------------------------------------------------------- Effects

const EFFECTS: [&str; 14] = [
    "Grayscale",
    "Sepia",
    "Invert",
    "Posterize",
    "Solarize",
    "Emboss",
    "Edges",
    "Oil paint",
    "Pixelate",
    "Vignette",
    "Film grain",
    "Denoise",
    "B&W filter",
    "Duotone",
];

/// Hover descriptions of [`EFFECTS`], in the same order.
const EFFECT_TIPS: [&str; 14] = [
    "Black and white",
    "Warm brown old-photo look",
    "Swap dark and light, like a film negative",
    "Fewer tones for a flat, poster look",
    "Invert only the bright tones",
    "Raised, stamped-metal look",
    "Keep only the outlines",
    "Looks painted with a brush",
    "Big square blocks",
    "Darken the corners",
    "Add film-like grain",
    "Reduce noise and specks",
    "Black and white with a colored lens filter",
    "Two colors: one for shadows, one for highlights",
];

/// `Effects::kind` before the user picks one: the preview is the original.
const NO_EFFECT: usize = usize::MAX;

const BW_FILTERS: [&str; 6] = ["Neutral", "Red", "Orange", "Yellow", "Green", "Blue"];

/// Remembers the effect picked and every setting.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Effects {
    kind: usize,
    strength: f32,
    levels: f32,
    threshold: f32,
    depth: f32,
    oil_radius: f32,
    block: f32,
    vig_mid: f32,
    vig_feather: f32,
    grain_size: f32,
    grain_seed: u32,
    nr_radius: usize,
    nr_smooth: f32,
    bw: usize,
    dark: [u8; 4],
    light: [u8; 4],
    #[serde(skip)]
    thumbs: Option<Vec<egui::TextureHandle>>,
}

impl Remember for Effects {
    const KEY: &'static str = "effects";
    fn recalled(&mut self) {
        if self.kind >= EFFECTS.len() {
            self.kind = NO_EFFECT;
        }
        memory::clamp(&mut self.strength, 0.0..=100.0);
        memory::clamp(&mut self.levels, 2.0..=16.0);
        memory::clamp(&mut self.threshold, 0.0..=255.0);
        memory::clamp(&mut self.depth, 0.5..=20.0);
        memory::clamp(&mut self.oil_radius, 1.0..=16.0);
        memory::clamp(&mut self.block, 2.0..=200.0);
        memory::clamp(&mut self.vig_mid, 0.0..=1.0);
        memory::clamp(&mut self.vig_feather, 0.05..=1.0);
        memory::clamp(&mut self.grain_size, 1.0..=8.0);
        self.grain_seed = self.grain_seed.min(9999);
        self.nr_radius = self.nr_radius.min(1);
        memory::clamp(&mut self.nr_smooth, 0.0..=6.0);
        self.bw = self.bw.min(BW_FILTERS.len() - 1);
    }
}

impl Default for Effects {
    fn default() -> Self {
        Effects {
            kind: NO_EFFECT,
            strength: 100.0,
            levels: 5.0,
            threshold: 128.0,
            depth: 2.0,
            oil_radius: 4.0,
            block: 16.0,
            vig_mid: 0.35,
            vig_feather: 0.6,
            grain_size: 1.5,
            grain_seed: 1,
            nr_radius: 0,
            nr_smooth: 1.5,
            bw: 1,
            dark: [28, 36, 74, 255],
            light: [250, 226, 180, 255],
            thumbs: None,
        }
    }
}

impl Effects {
    fn default_strength(kind: usize) -> f32 {
        match kind {
            9 => 50.0,
            10 => 30.0,
            _ => 100.0,
        }
    }

    fn render(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let t = (self.strength / 100.0).clamp(0.0, 1.0);
        let px = |v: f32| (v * scale).max(0.5);
        let full = match self.kind {
            0 => return fx::grayscale(img, t),
            1 => return fx::sepia(img, t),
            2 => fx::invert(img),
            3 => fx::posterize(img, self.levels.round() as u32),
            4 => fx::solarize(img, self.threshold.round().clamp(0.0, 255.0) as u8),
            5 => return fx::emboss(img, px(self.depth), t),
            6 => return fx::edges(img, t),
            7 => fx::oil_paint(img, (self.oil_radius * scale).round().max(1.0) as u32),
            8 => fx::pixelate(img, px(self.block).max(1.0)),
            9 => return fx::vignette(img, t, self.vig_mid, self.vig_feather),
            10 => return fx::film_grain(img, t, px(self.grain_size).max(1.0), self.grain_seed),
            11 => fx::denoise(img, ((self.nr_radius + 1) as f32 * scale).round().max(1.0) as u32, px(self.nr_smooth)),
            12 => return fx::bw_filter(img, self.bw, t),
            13 => return fx::duotone(img, [self.dark[0], self.dark[1], self.dark[2]], [self.light[0], self.light[1], self.light[2]], t),
            _ => return img.clone(),
        };
        if t >= 1.0 {
            full
        } else {
            fx::mix_images(img, &full, t)
        }
    }

    fn build_thumbs(&mut self, ctx: &egui::Context, base: &RgbaImage, full_w: u32) {
        let small = crate::imaging::resize_to_fit(base, 120, 90);
        // Spatial effects use an exaggerated scale so they read at thumbnail size.
        let scale = (small.width() as f32 / full_w.max(1) as f32).max(0.5);
        let thumbs = (0..EFFECTS.len())
            .map(|k| {
                let mut e = Effects { kind: k, thumbs: None, ..Effects::default() };
                // Exaggerate the subtle ones so the thumbnail reads at a glance.
                e.strength = match k {
                    9 => 85.0,
                    10 => 60.0,
                    _ => 100.0,
                };
                let out = e.render(&small, scale);
                let ci = egui::ColorImage::from_rgba_unmultiplied([out.width() as usize, out.height() as usize], out.as_raw());
                ctx.load_texture(format!("fx-thumb-{k}"), ci, egui::TextureOptions::LINEAR)
            })
            .collect();
        self.thumbs = Some(thumbs);
    }
}

impl EditTool for Effects {
    fn title(&self) -> String {
        "Effects".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let pal = cx.palette;
        let mut c = false;
        if self.thumbs.is_none() {
            if let Some(base) = cx.preview {
                self.build_thumbs(ui.ctx(), base, cx.image_size.0);
            }
        }
        // Gallery grid.
        let cols = 4;
        let gap = 4.0;
        let avail = ui.available_width().min(312.0);
        let cw = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).floor();
        let ch = (cw * 0.72).floor() + 16.0;
        for row_i in 0..EFFECTS.len().div_ceil(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for k in row_i * cols..((row_i + 1) * cols).min(EFFECTS.len()) {
                    let (rect, resp) = ui.allocate_exact_size(vec2(cw, ch), Sense::click());
                    let sel = self.kind == k;
                    let img_rect = egui::Rect::from_min_size(rect.min, vec2(cw, ch - 16.0));
                    let painter = ui.painter_at(rect);
                    painter.rect_filled(img_rect, 4.0, if sel { pal.selected_bg } else { pal.raised });
                    if let Some(t) = self.thumbs.as_ref().and_then(|t| t.get(k)) {
                        let ts = t.size_vec2();
                        let s = (img_rect.width() / ts.x).max(img_rect.height() / ts.y);
                        let uvw = (img_rect.width() / (ts.x * s)).min(1.0);
                        let uvh = (img_rect.height() / (ts.y * s)).min(1.0);
                        let uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), vec2(uvw, uvh));
                        egui::Image::new((t.id(), img_rect.size())).uv(uv).corner_radius(4).paint_at(ui, img_rect);
                    }
                    let stroke = if sel {
                        Stroke::new(2.0, pal.accent)
                    } else if resp.hovered() {
                        Stroke::new(1.0, pal.muted)
                    } else {
                        Stroke::new(1.0, pal.border)
                    };
                    painter.rect_stroke(img_rect, 4.0, stroke, egui::StrokeKind::Inside);
                    painter.text(
                        egui::pos2(rect.center().x, rect.max.y - 7.0),
                        egui::Align2::CENTER_CENTER,
                        EFFECTS[k],
                        egui::FontId::proportional(crate::ui::theme::text::CAPTION),
                        if sel { pal.text } else { pal.muted },
                    );
                    if resp.on_hover_text(EFFECT_TIPS[k]).clicked() && !sel {
                        self.kind = k;
                        self.strength = Self::default_strength(k);
                        c = true;
                    }
                }
            });
            ui.add_space(gap);
        }
        if self.kind >= EFFECTS.len() {
            info(ui, cx, "Pick an effect above to preview it.");
            return c;
        }
        section(ui, cx, EFFECTS[self.kind]);
        let strength_label = match self.kind {
            9 | 10 => "Amount",
            _ => "Strength",
        };
        c |= tipped(ui, "How much of the effect is mixed into the image", |ui| slider(ui, strength_label, &mut self.strength, 0.0..=100.0, Self::default_strength(self.kind), " %"));
        match self.kind {
            3 => c |= tipped(ui, "Number of tones per color; fewer gives a flatter, poster look", |ui| slider_ex(ui, "Levels", &mut self.levels, 2.0..=16.0, 5.0, "", 1.0, 0, false)),
            4 => c |= tipped(ui, "Tones brighter than this are inverted", |ui| slider_ex(ui, "Threshold", &mut self.threshold, 0.0..=255.0, 128.0, "", 1.0, 0, false)),
            5 => c |= tipped(ui, "How raised the embossed edges look", |ui| slider_ex(ui, "Depth", &mut self.depth, 0.5..=20.0, 2.0, " px", 0.5, 1, false)),
            7 => c |= tipped(ui, "Size of the paint strokes", |ui| slider_ex(ui, "Brush size", &mut self.oil_radius, 1.0..=16.0, 4.0, " px", 1.0, 0, false)),
            8 => c |= tipped(ui, "Size of each square block", |ui| slider_ex(ui, "Cell size", &mut self.block, 2.0..=200.0, 16.0, " px", 1.0, 0, true)),
            9 => {
                c |= tipped(ui, "Where the darkening starts, from the centre (0) to the corners (1)", |ui| slider_ex(ui, "Midpoint", &mut self.vig_mid, 0.0..=1.0, 0.35, "", 0.01, 2, false));
                c |= tipped(ui, "How gradually the corners darken", |ui| slider_ex(ui, "Feather", &mut self.vig_feather, 0.05..=1.0, 0.6, "", 0.01, 2, false));
            }
            10 => {
                c |= tipped(ui, "Size of the grain specks", |ui| slider_ex(ui, "Grain size", &mut self.grain_size, 1.0..=8.0, 1.5, " px", 0.1, 1, false));
                c |= row(ui, "Seed", |ui| {
                    let r = widgets::value_box(ui, egui::DragValue::new(&mut self.grain_seed).range(0..=9999));
                    let b = ui.button("Shuffle").on_hover_text("Try another random grain pattern").clicked();
                    if b {
                        self.grain_seed = (self.grain_seed.wrapping_mul(1_103_515_245).wrapping_add(12345)) % 10_000;
                    }
                    r.changed() || b
                });
            }
            11 => {
                c |= row(ui, "Despeckle", |ui| segmented_tips(ui, &mut self.nr_radius, &["3×3", "5×5"], &["Remove single specks", "Remove larger specks too"]));
                c |= tipped(ui, "Extra smoothing of the remaining noise", |ui| slider_ex(ui, "Smoothing", &mut self.nr_smooth, 0.0..=6.0, 1.5, " px", 0.1, 1, false));
            }
            12 => {
                let names: Vec<String> = BW_FILTERS.iter().map(|s| s.to_string()).collect();
                c |= tipped(ui, "Like a colored filter on a black and white film camera: it lightens its own color", |ui| combo(ui, "Lens filter", "fx-bw-filter", &mut self.bw, &names));
            }
            13 => {
                c |= tipped(ui, "Color given to the dark tones", |ui| color_row(ui, "Shadows", &mut self.dark, false));
                c |= tipped(ui, "Color given to the bright tones", |ui| color_row(ui, "Highlights", &mut self.light, false));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    for (name, d, l) in [
                        ("Midnight", [28, 36, 74, 255], [250, 226, 180, 255]),
                        ("Sepia tint", [52, 32, 18, 255], [255, 240, 214, 255]),
                        ("Cyanotype", [10, 40, 80, 255], [220, 240, 250, 255]),
                        ("Rose", [60, 20, 50, 255], [255, 210, 200, 255]),
                    ] {
                        let swatch = Color32::from_rgb(d[0], d[1], d[2]);
                        if ui.button(egui::RichText::new(name).color(if pal.dark { pal.text } else { swatch })).on_hover_text("Use this pair of colors").clicked() {
                            self.dark = d;
                            self.light = l;
                            c = true;
                        }
                    }
                });
            }
            _ => {}
        }
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        self.render(img, scale)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        let thumbs = self.thumbs.take();
        *self = Effects { thumbs, ..Effects::default() };
    }
    fn can_apply(&self) -> bool {
        self.kind != NO_EFFECT
    }
    remember_methods!();
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl Effects {
        pub fn sample(kind: usize) -> Self {
            Effects { kind, strength: Self::default_strength(kind), ..Default::default() }
        }
    }

    #[test]
    fn effects_start_with_no_effect() {
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([200, 30, 30, 255]));
        assert_eq!(Effects::default().apply(&img, 1.0), img);
        let mut e = Effects::sample(0);
        e.reset();
        assert_eq!(e.apply(&img, 1.0), img);
    }

    impl Blur {
        pub fn sample() -> Self {
            Blur { mode: 2, angle: 20.0, distance: 60.0, ..Default::default() }
        }
    }

    #[test]
    fn effect_kinds_safe_and_zero_strength_identity() {
        let img = RgbaImage::from_fn(30, 20, |x, y| image::Rgba([(x * 8) as u8, (y * 12) as u8, 99, 255]));
        for k in 0..EFFECTS.len() {
            let e = Effects { kind: k, ..Effects::default() };
            for s in [1.0, 0.3] {
                assert_eq!(e.apply(&img, s).dimensions(), img.dimensions(), "{}", EFFECTS[k]);
            }
            let z = Effects { kind: k, strength: 0.0, ..Effects::default() };
            assert_eq!(z.apply(&img, 1.0), img, "{} at 0%", EFFECTS[k]);
        }
    }

    #[test]
    fn small_radius_still_previews() {
        let img = RgbaImage::from_fn(40, 40, |x, y| image::Rgba([((x * 37 + y * 11) % 256) as u8, (y * 6) as u8, 99, 255]));
        let s = Sharpen { amount: 200.0, radius: 0.5, threshold: 0.0 };
        assert_ne!(s.apply(&img, 0.28), img, "preview must show something");
        assert_eq!(scaled_radius(0.5, 1.0), 0.5);
        assert_eq!(scaled_radius(0.1, 0.25), 0.025);
    }

    #[test]
    fn filters_round_trip() {
        let used = Effects { kind: 13, strength: 60.0, dark: [1, 2, 3, 255], light: [200, 210, 220, 255], grain_seed: 77, ..Default::default() };
        let mut t = Effects::default();
        t.recall(&used.remember().unwrap());
        assert_eq!(t.remember(), used.remember());
        assert!(t.can_apply() && t.differs_from_defaults());
        assert!(t.thumbs.is_none());
        // An effect that does not exist (any more) opens with none picked.
        t.recall(&serde_json::json!({"kind": 99, "bw": 40}));
        assert_eq!((t.kind, t.bw), (NO_EFFECT, BW_FILTERS.len() - 1));
        assert!(!t.can_apply());

        let s = Sharpen { amount: 150.0, radius: 0.7, threshold: 4.0 };
        let mut t = Sharpen::default();
        t.recall(&s.remember().unwrap());
        assert_eq!((t.amount, t.radius, t.threshold), (150.0, 0.7, 4.0));
        let b = Blur::sample();
        let mut t = Blur::default();
        t.recall(&b.remember().unwrap());
        assert_eq!((t.mode, t.angle, t.distance), (2, 20.0, 60.0));
        t.reset();
        assert!(!t.differs_from_defaults());
    }

    #[test]
    fn sharpen_and_blur_identity() {
        let img = RgbaImage::from_fn(20, 20, |x, y| image::Rgba([(x * 8) as u8, (y * 12) as u8, 99, 255]));
        let s = Sharpen { amount: 0.0, ..Default::default() };
        assert_eq!(s.apply(&img, 1.0), img);
        let b = Blur { radius: 0.0, ..Default::default() };
        assert_eq!(b.apply(&img, 1.0), img);
        for mode in 0..4 {
            let b = Blur { mode, ..Default::default() };
            assert_eq!(b.apply(&img, 0.5).dimensions(), (20, 20));
        }
    }
}
