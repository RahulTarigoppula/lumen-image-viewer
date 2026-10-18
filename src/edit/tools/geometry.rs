//! Size-changing tools: Resize, Canvas size, Straighten / free rotate, Border.

use super::{anchor_grid, color_button, color_row, combo, drag_px, info, mp, row, section, segmented, slider, slider_ex};
use crate::edit::geom::{self, clamp_dim, BorderSpec, MAX_DIM};
use crate::edit::{EditTool, ToolUiCtx};
use crate::imaging::Filter;
use crate::ui::icons as I;
use image::RgbaImage;

fn src_size(src: (u32, u32), img: &RgbaImage, scale: f32) -> (f64, f64) {
    if src.0 > 0 && src.1 > 0 {
        (src.0 as f64, src.1 as f64)
    } else {
        let s = scale.max(1e-6) as f64;
        (img.width() as f64 / s, img.height() as f64 / s)
    }
}

// ---------------------------------------------------------------- Resize

#[derive(Clone)]
pub struct Resize {
    /// 0 = pixels, 1 = percent
    mode: usize,
    w: i64,
    h: i64,
    pct: f32,
    lock: bool,
    filter: usize,
    src: (u32, u32),
}

impl Default for Resize {
    fn default() -> Self {
        Resize { mode: 0, w: 0, h: 0, pct: 100.0, lock: true, filter: 0, src: (0, 0) }
    }
}

impl Resize {
    fn target(&self) -> (u32, u32) {
        let (sw, sh) = (self.src.0.max(1) as f64, self.src.1.max(1) as f64);
        if self.mode == 1 {
            (clamp_dim(sw * self.pct as f64 / 100.0), clamp_dim(sh * self.pct as f64 / 100.0))
        } else {
            (clamp_dim(self.w as f64), clamp_dim(self.h as f64))
        }
    }
    fn set_long_edge(&mut self, le: u32) {
        let (sw, sh) = (self.src.0.max(1) as f64, self.src.1.max(1) as f64);
        let s = le as f64 / sw.max(sh);
        self.mode = 0;
        self.w = clamp_dim(sw * s) as i64;
        self.h = clamp_dim(sh * s) as i64;
    }
}

impl EditTool for Resize {
    fn title(&self) -> String {
        "Resize / resample".into()
    }
    fn changes_size(&self) -> bool {
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        if self.src != cx.image_size {
            self.src = cx.image_size;
            self.w = self.src.0 as i64;
            self.h = self.src.1 as i64;
            c = true;
        }
        let (sw, sh) = (self.src.0.max(1) as f64, self.src.1.max(1) as f64);
        if segmented(ui, &mut self.mode, &["Pixels", "Percent"]) {
            if self.mode == 0 {
                let (w, h) = (clamp_dim(sw * self.pct as f64 / 100.0), clamp_dim(sh * self.pct as f64 / 100.0));
                self.w = w as i64;
                self.h = h as i64;
            } else {
                self.pct = ((self.w as f64 / sw * 100.0) as f32).clamp(1.0, 400.0);
            }
            c = true;
        }
        ui.add_space(4.0);
        if self.mode == 0 {
            if drag_px(ui, "Width", &mut self.w, 1..=MAX_DIM as i64) {
                if self.lock {
                    self.h = clamp_dim(self.w as f64 * sh / sw) as i64;
                }
                c = true;
            }
            if drag_px(ui, "Height", &mut self.h, 1..=MAX_DIM as i64) {
                if self.lock {
                    self.w = clamp_dim(self.h as f64 * sw / sh) as i64;
                }
                c = true;
            }
            row(ui, "", |ui| {
                let icon = if self.lock { I::LINK } else { I::LINK_BREAK };
                if ui.checkbox(&mut self.lock, format!("{icon} Keep aspect ratio")).changed() && self.lock {
                    self.h = clamp_dim(self.w as f64 * sh / sw) as i64;
                    c = true;
                }
            });
        } else {
            c |= slider_ex(ui, "Scale", &mut self.pct, 1.0..=400.0, 100.0, " %", 0.0, 1, true);
        }
        section(ui, cx, "Presets (long edge)");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for le in [3840u32, 2560, 1920, 1280, 1024, 800] {
                if ui.button(le.to_string()).on_hover_text(format!("Long edge {le} px")).clicked() {
                    self.set_long_edge(le);
                    c = true;
                }
            }
            for p in [50.0f32, 25.0] {
                if ui.button(format!("{p}%")).clicked() {
                    self.mode = 1;
                    self.pct = p;
                    c = true;
                }
            }
        });
        section(ui, cx, "Resampling");
        let names: Vec<String> = Filter::ALL.iter().map(|f| f.label().to_string()).collect();
        c |= combo(ui, "Filter", "resize-filter", &mut self.filter, &names);
        ui.add_space(8.0);
        let (tw, th) = self.target();
        info(ui, cx, format!("Original   {} × {}  ·  {}", self.src.0, self.src.1, mp(self.src.0, self.src.1)));
        let what = if (tw as u64 * th as u64) > (self.src.0 as u64 * self.src.1 as u64) { "  ·  upscaling" } else { "" };
        ui.label(egui::RichText::new(format!("New   {tw} × {th}  ·  {}{what}", mp(tw, th))).strong());
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        if self.src.0 == 0 || self.w <= 0 {
            return img.clone();
        }
        let (tw, th) = self.target();
        let ow = clamp_dim(tw as f64 * scale as f64);
        let oh = clamp_dim(th as f64 * scale as f64);
        if (ow, oh) == img.dimensions() {
            return img.clone();
        }
        crate::imaging::resize_exact(img, ow, oh, Filter::ALL[self.filter.min(Filter::ALL.len() - 1)])
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = Resize { src: (0, 0), ..Default::default() };
    }
}

// ---------------------------------------------------------------- Canvas size

#[derive(Clone)]
pub struct CanvasSize {
    /// 0 = absolute size, 1 = margins
    mode: usize,
    w: i64,
    h: i64,
    /// top, right, bottom, left
    margins: [i64; 4],
    anchor: u8,
    fill: [u8; 4],
    src: (u32, u32),
}

impl Default for CanvasSize {
    fn default() -> Self {
        CanvasSize { mode: 0, w: 0, h: 0, margins: [0; 4], anchor: 4, fill: [255, 255, 255, 255], src: (0, 0) }
    }
}

impl CanvasSize {
    fn target(&self, sw: f64, sh: f64) -> (f64, f64) {
        if self.mode == 1 {
            let m = self.margins;
            ((sw + (m[1] + m[3]) as f64).max(1.0), (sh + (m[0] + m[2]) as f64).max(1.0))
        } else {
            (self.w.max(1) as f64, self.h.max(1) as f64)
        }
    }
}

impl EditTool for CanvasSize {
    fn title(&self) -> String {
        "Canvas size".into()
    }
    fn changes_size(&self) -> bool {
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        if self.src != cx.image_size {
            self.src = cx.image_size;
            self.w = self.src.0 as i64;
            self.h = self.src.1 as i64;
            c = true;
        }
        let (sw, sh) = (self.src.0 as i64, self.src.1 as i64);
        c |= segmented(ui, &mut self.mode, &["New size", "Add margins"]);
        ui.add_space(4.0);
        if self.mode == 0 {
            c |= drag_px(ui, "Width", &mut self.w, 1..=MAX_DIM as i64);
            c |= drag_px(ui, "Height", &mut self.h, 1..=MAX_DIM as i64);
            row(ui, "", |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui.button("Original").clicked() {
                    self.w = sw;
                    self.h = sh;
                    c = true;
                }
                if ui.button("Square").on_hover_text("Square canvas fitting the image").clicked() {
                    let m = sw.max(sh);
                    self.w = m;
                    self.h = m;
                    c = true;
                }
                if ui.button("+10%").clicked() {
                    self.w = (self.w as f64 * 1.1).round() as i64;
                    self.h = (self.h as f64 * 1.1).round() as i64;
                    c = true;
                }
            });
            ui.add_space(4.0);
            row(ui, &format!("{} Anchor", I::ANCHOR), |ui| c |= anchor_grid(ui, cx, &mut self.anchor));
        } else {
            let names = ["Top", "Right", "Bottom", "Left"];
            for (i, n) in names.iter().enumerate() {
                let lim = if i % 2 == 0 { sh } else { sw };
                c |= drag_px(ui, n, &mut self.margins[i], -(lim - 1).max(0) / 2..=MAX_DIM as i64 / 2);
            }
            row(ui, "", |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (label, v) in [("All 5%", 0.05), ("All 10%", 0.10), ("Clear", 0.0)] {
                    if ui.button(label).clicked() {
                        let m = (sw.min(sh) as f64 * v).round() as i64;
                        self.margins = [m; 4];
                        c = true;
                    }
                }
            });
        }
        section(ui, cx, "Fill");
        row(ui, "Color", |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            c |= color_button(ui, &mut self.fill, true);
            ui.add_space(4.0);
            for (tip, col) in [("White", [255, 255, 255, 255]), ("Black", [0, 0, 0, 255]), ("None", [0, 0, 0, 0])] {
                if ui.small_button(tip).clicked() {
                    self.fill = col;
                    c = true;
                }
            }
        });
        ui.add_space(8.0);
        let (tw, th) = self.target(sw as f64, sh as f64);
        let (tw, th) = (clamp_dim(tw), clamp_dim(th));
        info(ui, cx, format!("Original   {sw} × {sh}"));
        ui.label(egui::RichText::new(format!("New canvas   {tw} × {th}  ·  {}", mp(tw, th))).strong());
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let (sw, sh) = src_size(self.src, img, scale);
        let s = scale as f64;
        if self.mode == 1 {
            let (tw, th) = self.target(sw, sh);
            let (ow, oh) = (clamp_dim(tw * s), clamp_dim(th * s));
            let ox = (self.margins[3] as f64 * s).round() as i64;
            let oy = (self.margins[0] as f64 * s).round() as i64;
            geom::canvas_at(img, ow, oh, ox, oy, self.fill)
        } else {
            if self.w <= 0 || self.h <= 0 {
                return img.clone();
            }
            let (ow, oh) = (clamp_dim(self.w as f64 * s), clamp_dim(self.h as f64 * s));
            if (self.w as u32, self.h as u32) == self.src && s == 1.0 {
                return img.clone();
            }
            geom::canvas(img, ow, oh, self.anchor, self.fill)
        }
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = CanvasSize::default();
    }
}

// ---------------------------------------------------------------- Free rotate

#[derive(Clone)]
pub struct FreeRotate {
    angle: f32,
    quarter: i32,
    /// 0 = auto-crop, 1 = expand canvas
    mode: usize,
    bg: [u8; 4],
    src: (u32, u32),
}

impl Default for FreeRotate {
    fn default() -> Self {
        FreeRotate { angle: 0.0, quarter: 0, mode: 0, bg: [255, 255, 255, 255], src: (0, 0) }
    }
}

impl FreeRotate {
    fn quarter_turn(&self, img: &RgbaImage) -> Option<RgbaImage> {
        match self.quarter.rem_euclid(4) {
            1 => Some(image::imageops::rotate90(img)),
            2 => Some(image::imageops::rotate180(img)),
            3 => Some(image::imageops::rotate270(img)),
            _ => None,
        }
    }
}

impl EditTool for FreeRotate {
    fn title(&self) -> String {
        "Straighten / rotate".into()
    }
    fn changes_size(&self) -> bool {
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        self.src = cx.image_size;
        c |= slider_ex(ui, "Angle", &mut self.angle, -45.0..=45.0, 0.0, "°", 0.1, 1, false);
        row(ui, "", |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (label, d) in [("-1°", -1.0f32), ("-0.1°", -0.1), ("+0.1°", 0.1), ("+1°", 1.0)] {
                if ui.small_button(label).clicked() {
                    self.angle = ((self.angle + d) * 10.0).round() / 10.0;
                    self.angle = self.angle.clamp(-45.0, 45.0);
                    c = true;
                }
            }
        });
        row(ui, "Quarter turns", |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui.button(format!("{} 90°", I::ARROW_COUNTER_CLOCKWISE)).on_hover_text("Rotate 90° left").clicked() {
                self.quarter -= 1;
                c = true;
            }
            if ui.button(format!("{} 90°", I::ARROW_CLOCKWISE)).on_hover_text("Rotate 90° right").clicked() {
                self.quarter += 1;
                c = true;
            }
        });
        section(ui, cx, "Borders");
        if ui.radio_value(&mut self.mode, 0, "Auto-crop to remove borders").changed() {
            c = true;
        }
        if ui.radio_value(&mut self.mode, 1, "Expand canvas").changed() {
            c = true;
        }
        if self.mode == 1 {
            c |= color_row(ui, "Background", &mut self.bg, true);
        }
        ui.add_space(8.0);
        let (mut w, mut h) = self.src;
        if self.quarter.rem_euclid(2) == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        let (ow, oh) = if self.angle.abs() < 1e-4 { (w, h) } else { geom::rotated_size(w.max(1), h.max(1), self.angle, self.mode == 0) };
        let total = self.quarter.rem_euclid(4) as f32 * 90.0 + self.angle;
        info(ui, cx, format!("Rotation {total:.1}°  ·  result {ow} × {oh}"));
        c
    }
    fn apply(&self, img: &RgbaImage, _scale: f32) -> RgbaImage {
        let q = self.quarter_turn(img);
        let base = q.as_ref().unwrap_or(img);
        if self.angle.abs() < 1e-4 {
            return base.clone();
        }
        geom::rotate(base, self.angle, self.mode == 0, self.bg)
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = FreeRotate::default();
    }
}

// ---------------------------------------------------------------- Border / frame

#[derive(Clone)]
pub struct AddBorder {
    /// 0 = pixels, 1 = percent of the short side
    unit: usize,
    width_px: f32,
    width_pct: f32,
    color: [u8; 4],
    polaroid: bool,
    line: bool,
    line_w: f32,
    line_color: [u8; 4],
    radius: f32,
    shadow: bool,
    shadow_blur: f32,
    shadow_offset: f32,
    shadow_opacity: f32,
    shadow_bg: [u8; 4],
    src: (u32, u32),
}

impl Default for AddBorder {
    fn default() -> Self {
        AddBorder {
            unit: 1,
            width_px: 40.0,
            width_pct: 3.0,
            color: [255, 255, 255, 255],
            polaroid: false,
            line: false,
            line_w: 2.0,
            line_color: [30, 30, 30, 255],
            radius: 0.0,
            shadow: false,
            shadow_blur: 24.0,
            shadow_offset: 12.0,
            shadow_opacity: 45.0,
            shadow_bg: [238, 238, 238, 255],
            src: (0, 0),
        }
    }
}

impl AddBorder {
    /// Border spec in the pixel space of an image of short side `short` at `scale`.
    fn spec(&self, short: f32, scale: f32) -> BorderSpec {
        let width = if self.unit == 1 { self.width_pct / 100.0 * short } else { self.width_px * scale };
        BorderSpec {
            width,
            bottom: if self.polaroid { width * 3.4 } else { width },
            color: self.color,
            line_width: if self.line { (self.line_w * scale).max(if scale < 1.0 { 0.6 } else { 0.0 }) } else { 0.0 },
            line_color: self.line_color,
            radius: self.radius * scale,
            shadow: self.shadow,
            shadow_blur: self.shadow_blur * scale,
            shadow_offset: self.shadow_offset * scale,
            shadow_opacity: self.shadow_opacity / 100.0,
            shadow_bg: self.shadow_bg,
        }
    }
}

impl EditTool for AddBorder {
    fn title(&self) -> String {
        "Border / frame".into()
    }
    fn changes_size(&self) -> bool {
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool {
        let mut c = false;
        self.src = cx.image_size;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            let d = AddBorder::default();
            let presets: [(&str, AddBorder); 5] = [
                ("Thin white", AddBorder { width_pct: 1.5, ..d.clone() }),
                ("Gallery", AddBorder { width_pct: 7.0, line: true, line_w: 3.0, ..d.clone() }),
                ("Polaroid", AddBorder { width_pct: 5.0, polaroid: true, color: [250, 249, 244, 255], shadow: true, ..d.clone() }),
                ("Black", AddBorder { width_pct: 3.0, color: [12, 12, 12, 255], ..d.clone() }),
                ("Rounded", AddBorder { width_pct: 0.0, radius: 60.0, shadow: true, ..d.clone() }),
            ];
            for (name, p) in presets {
                if ui.button(name).clicked() {
                    *self = AddBorder { src: self.src, ..p };
                    c = true;
                }
            }
        });
        section(ui, cx, "Border");
        c |= row(ui, "Units", |ui| segmented(ui, &mut self.unit, &["px", "% of short side"]));
        if self.unit == 0 {
            c |= slider_ex(ui, "Width", &mut self.width_px, 0.0..=1000.0, 40.0, " px", 1.0, 0, true);
        } else {
            c |= slider_ex(ui, "Width", &mut self.width_pct, 0.0..=25.0, 3.0, " %", 0.1, 1, false);
        }
        c |= color_row(ui, "Color", &mut self.color, true);
        c |= row(ui, "", |ui| ui.checkbox(&mut self.polaroid, "Polaroid style (thick bottom)").changed());
        c |= slider_ex(ui, "Corner radius", &mut self.radius, 0.0..=1000.0, 0.0, " px", 1.0, 0, true);
        section(ui, cx, "Inner line");
        c |= row(ui, "", |ui| ui.checkbox(&mut self.line, "Line around the photo").changed());
        if self.line {
            c |= slider_ex(ui, "Line width", &mut self.line_w, 0.5..=50.0, 2.0, " px", 0.5, 1, false);
            c |= color_row(ui, "Line color", &mut self.line_color, true);
        }
        section(ui, cx, "Drop shadow");
        c |= row(ui, "", |ui| ui.checkbox(&mut self.shadow, "Drop shadow").changed());
        if self.shadow {
            c |= slider_ex(ui, "Softness", &mut self.shadow_blur, 0.0..=300.0, 24.0, " px", 1.0, 0, false);
            c |= slider_ex(ui, "Offset", &mut self.shadow_offset, 0.0..=300.0, 12.0, " px", 1.0, 0, false);
            c |= slider(ui, "Opacity", &mut self.shadow_opacity, 0.0..=100.0, 45.0, " %");
            c |= color_row(ui, "Background", &mut self.shadow_bg, true);
        }
        ui.add_space(8.0);
        let (w, h) = self.src;
        let (ow, oh, _) = geom::border_size(w, h, &self.spec(w.min(h) as f32, 1.0));
        info(ui, cx, format!("Result   {ow} × {oh}  ·  {}", mp(ow, oh)));
        c
    }
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage {
        let short = img.width().min(img.height()) as f32;
        geom::add_border(img, &self.spec(short, scale))
    }
    fn clone_box(&self) -> Box<dyn EditTool> {
        Box::new(self.clone())
    }
    fn reset(&mut self) {
        *self = AddBorder::default();
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl FreeRotate {
        pub fn sample(angle: f32, mode: usize) -> Self {
            FreeRotate { angle, mode, bg: [40, 40, 40, 255], ..Default::default() }
        }
    }

    impl AddBorder {
        pub fn sample() -> Self {
            AddBorder { width_pct: 4.0, polaroid: true, line: true, radius: 40.0, shadow: true, ..Default::default() }
        }
    }

    fn img(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]))
    }

    #[test]
    fn resize_output_size_and_preview_scale() {
        let mut t = Resize { src: (400, 200), w: 100, h: 50, ..Default::default() };
        let full = img(400, 200);
        assert_eq!(t.apply(&full, 1.0).dimensions(), (100, 50));
        let prev = img(200, 100);
        assert_eq!(t.apply(&prev, 0.5).dimensions(), (50, 25));
        t.mode = 1;
        t.pct = 25.0;
        assert_eq!(t.apply(&full, 1.0).dimensions(), (100, 50));
        t.set_long_edge(800);
        assert_eq!(t.target(), (800, 400));
        let id = Resize { src: (400, 200), w: 400, h: 200, ..Default::default() };
        assert_eq!(id.apply(&full, 1.0), full);
    }

    #[test]
    fn canvas_tool_modes() {
        let full = img(100, 50);
        let t = CanvasSize { src: (100, 50), w: 120, h: 80, ..Default::default() };
        assert_eq!(t.apply(&full, 1.0).dimensions(), (120, 80));
        assert_eq!(t.apply(&img(50, 25), 0.5).dimensions(), (60, 40));
        let m = CanvasSize { mode: 1, src: (100, 50), margins: [10, 5, 10, 5], ..Default::default() };
        let o = m.apply(&full, 1.0);
        assert_eq!(o.dimensions(), (110, 70));
        assert_eq!(o.get_pixel(5, 10), full.get_pixel(0, 0));
    }

    #[test]
    fn rotate_tool() {
        let full = img(60, 40);
        let t = FreeRotate::default();
        assert_eq!(t.apply(&full, 1.0), full);
        let q = FreeRotate { quarter: 1, ..Default::default() };
        assert_eq!(q.apply(&full, 1.0).dimensions(), (40, 60));
        let e = FreeRotate { angle: 5.0, mode: 1, ..Default::default() };
        assert!(e.apply(&full, 1.0).width() > 60);
    }

    #[test]
    fn border_tool_polaroid() {
        let full = img(100, 100);
        let t = AddBorder { unit: 0, width_px: 10.0, polaroid: true, ..Default::default() };
        assert_eq!(t.apply(&full, 1.0).dimensions(), (120, 144));
        let z = AddBorder { width_pct: 0.0, ..Default::default() };
        assert_eq!(z.apply(&full, 1.0), full);
    }
}
