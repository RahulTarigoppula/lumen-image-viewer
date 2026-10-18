//! Filter tools: Sharpen (unsharp mask), Blur, and the Effects gallery.

use super::{color_row, info, row, section, segmented, slider, slider_ex};
use crate::edit::{effects as fx, filters, EditTool, ToolUiCtx};
use egui::{vec2, Color32, Sense, Stroke};
use image::RgbaImage;

// ---------------------------------------------------------------- Sharpen

#[derive(Clone)]
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

impl EditTool for Sharpen {
    fn title(&self) -> String {
        "Sharpen (unsharp mask)".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for (name, a, r, t) in [("Subtle", 50.0, 0.8, 2.0), ("Medium", 100.0, 1.0, 2.0), ("Strong", 180.0, 1.4, 3.0), ("Web", 60.0, 0.5, 0.0), ("Clarity", 25.0, 30.0, 0.0)] {
                if ui.button(name).clicked() {
                    *self = Sharpen { amount: a, radius: r, threshold: t };
                    c = true;
                }
            }
        });
        ui.add_space(4.0);
        c |= slider_ex(ui, "Amount", &mut self.amount, 0.0..=500.0, 80.0, " %", 1.0, 0, false);
        c |= slider_ex(ui, "Radius", &mut self.radius, 0.1..=100.0, 1.0, " px", 0.1, 1, true);
        c |= slider_ex(ui, "Threshold", &mut self.threshold, 0.0..=255.0, 2.0, "", 1.0, 0, false);
        ui.add_space(6.0);
        info(ui, cx, "Tip: judge sharpening at 100% zoom; previews are downscaled.");
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        filters::unsharp_mask(img, self.amount / 100.0, self.radius * scale, self.threshold.round().clamp(0.0, 255.0) as u8)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Sharpen::default();
    }
}

// ---------------------------------------------------------------- Blur

#[derive(Clone)]
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

impl EditTool for Blur {
    fn title(&self) -> String {
        "Blur".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        c |= segmented(ui, &mut self.mode, &["Gaussian", "Box", "Motion", "Smart"]);
        ui.add_space(4.0);
        match self.mode {
            2 => {
                c |= slider_ex(ui, "Angle", &mut self.angle, -180.0..=180.0, 0.0, "°", 1.0, 0, false);
                c |= slider_ex(ui, "Distance", &mut self.distance, 1.0..=500.0, 30.0, " px", 1.0, 0, true);
            }
            _ => {
                c |= slider_ex(ui, "Radius", &mut self.radius, 0.1..=250.0, 4.0, " px", 0.1, 1, true);
                if self.mode == 3 {
                    c |= slider_ex(ui, "Threshold", &mut self.threshold, 1.0..=100.0, 25.0, "", 1.0, 0, false);
                }
            }
        }
        ui.add_space(6.0);
        info(
            ui,
            cx,
            match self.mode {
                1 => "Uniform averaging - fast, slightly boxy.",
                2 => "Streaks along a direction, like camera shake.",
                3 => "Smooths flat areas but keeps edges sharp (good for skin and noise).",
                _ => "Natural soft focus.",
            },
        );
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let r = self.radius * scale;
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

const BW_FILTERS: [&str; 6] = ["Neutral", "Red", "Orange", "Yellow", "Green", "Blue"];

#[derive(Clone)]
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
    thumbs: Option<Vec<egui::TextureHandle>>,
}

impl Default for Effects {
    fn default() -> Self {
        Effects {
            kind: 0,
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
                    painter.rect_filled(img_rect, 4.0, pal.raised);
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
                        egui::FontId::proportional(10.5),
                        if sel { pal.text } else { pal.muted },
                    );
                    if resp.clicked() && !sel {
                        self.kind = k;
                        self.strength = Self::default_strength(k);
                        c = true;
                    }
                }
            });
            ui.add_space(gap);
        }
        section(ui, cx, EFFECTS[self.kind.min(EFFECTS.len() - 1)]);
        let strength_label = match self.kind {
            9 | 10 => "Amount",
            _ => "Strength",
        };
        c |= slider(ui, strength_label, &mut self.strength, 0.0..=100.0, Self::default_strength(self.kind), " %");
        match self.kind {
            3 => c |= slider_ex(ui, "Levels", &mut self.levels, 2.0..=16.0, 5.0, "", 1.0, 0, false),
            4 => c |= slider_ex(ui, "Threshold", &mut self.threshold, 0.0..=255.0, 128.0, "", 1.0, 0, false),
            5 => c |= slider_ex(ui, "Depth", &mut self.depth, 0.5..=20.0, 2.0, " px", 0.5, 1, false),
            7 => c |= slider_ex(ui, "Brush size", &mut self.oil_radius, 1.0..=16.0, 4.0, " px", 1.0, 0, false),
            8 => c |= slider_ex(ui, "Cell size", &mut self.block, 2.0..=200.0, 16.0, " px", 1.0, 0, true),
            9 => {
                c |= slider_ex(ui, "Midpoint", &mut self.vig_mid, 0.0..=1.0, 0.35, "", 0.01, 2, false);
                c |= slider_ex(ui, "Feather", &mut self.vig_feather, 0.05..=1.0, 0.6, "", 0.01, 2, false);
            }
            10 => {
                c |= slider_ex(ui, "Grain size", &mut self.grain_size, 1.0..=8.0, 1.5, " px", 0.1, 1, false);
                c |= row(ui, "Seed", |ui| {
                    let r = ui.add(egui::DragValue::new(&mut self.grain_seed).range(0..=9999));
                    let b = ui.small_button("Shuffle").clicked();
                    if b {
                        self.grain_seed = (self.grain_seed.wrapping_mul(1_103_515_245).wrapping_add(12345)) % 10_000;
                    }
                    r.changed() || b
                });
            }
            11 => {
                c |= row(ui, "Despeckle", |ui| segmented(ui, &mut self.nr_radius, &["3×3", "5×5"]));
                c |= slider_ex(ui, "Smoothing", &mut self.nr_smooth, 0.0..=6.0, 1.5, " px", 0.1, 1, false);
            }
            12 => c |= row(ui, "Lens filter", |ui| segmented(ui, &mut self.bw, &BW_FILTERS)),
            13 => {
                c |= color_row(ui, "Shadows", &mut self.dark, false);
                c |= color_row(ui, "Highlights", &mut self.light, false);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    for (name, d, l) in [
                        ("Midnight", [28, 36, 74, 255], [250, 226, 180, 255]),
                        ("Sepia tint", [52, 32, 18, 255], [255, 240, 214, 255]),
                        ("Cyanotype", [10, 40, 80, 255], [220, 240, 250, 255]),
                        ("Rose", [60, 20, 50, 255], [255, 210, 200, 255]),
                    ] {
                        let swatch = Color32::from_rgb(d[0], d[1], d[2]);
                        if ui.button(egui::RichText::new(name).color(if pal.dark { pal.text } else { swatch })).clicked() {
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
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl Effects {
        pub fn sample(kind: usize) -> Self {
            Effects { kind, strength: Self::default_strength(kind), ..Default::default() }
        }
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
