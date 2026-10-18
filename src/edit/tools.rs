//! Concrete adjustment tools (dialogs). `make_tool` maps a command to its tool.

use super::{ops, EditTool, ToolUiCtx};
use crate::ui::commands::Command;
use image::RgbaImage;

pub fn make_tool(cmd: Command) -> Option<Box<dyn EditTool>> {
    match cmd {
        Command::AdjustColors => Some(Box::new(AdjustColors::default())),
        _ => None,
    }
}

/// Labeled slider with a double-click-to-reset affordance. Returns true if changed.
pub fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, default: f32, suffix: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let lr = ui.add_sized([96.0, 18.0], egui::Label::new(label).sense(egui::Sense::click()));
        if lr.double_clicked() {
            *value = default;
            changed = true;
        }
        lr.on_hover_text("Double-click to reset");
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
}

impl Default for AdjustColors {
    fn default() -> Self {
        AdjustColors { brightness: 0.0, contrast: 0.0, gamma: 1.0, saturation: 0.0, vibrance: 0.0, hue: 0.0, temperature: 0.0, tint: 0.0, exposure: 0.0 }
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
