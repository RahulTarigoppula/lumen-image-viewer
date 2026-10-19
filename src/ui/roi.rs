//! Rectangle editor drawn over an image: the crop tool's area and the
//! analysis region of interest. The rectangle lives in image pixels; the
//! caller passes the image-to-screen transform every frame, so the same
//! rectangle can be shown in several synchronized panes.
//!
//! Drag outside the rectangle (or anywhere when there is none) to draw a new
//! one, drag inside to move it, drag edges or corners to resize. Optional:
//! aspect lock, Shift for a square, Esc to clear, a size label, and a
//! minimum size below which the rectangle is drawn as a warning.

use egui::{Color32, CursorIcon, FontId, Key, Modifiers, Painter, Pos2, Rect, Response, Stroke, Ui, Vec2};

/// Maps image pixels to screen points: an image of `size` pixels drawn into `dest`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageTransform {
    pub dest: Rect,
    pub size: Vec2,
}

impl ImageTransform {
    pub fn new(dest: Rect, size: (u32, u32)) -> ImageTransform {
        ImageTransform { dest, size: Vec2::new(size.0.max(1) as f32, size.1.max(1) as f32) }
    }

    pub fn to_screen(&self, p: Pos2) -> Pos2 {
        Pos2::new(self.dest.min.x + p.x / self.size.x * self.dest.width(), self.dest.min.y + p.y / self.size.y * self.dest.height())
    }

    pub fn to_image(&self, p: Pos2) -> Pos2 {
        Pos2::new((p.x - self.dest.min.x) / self.dest.width() * self.size.x, (p.y - self.dest.min.y) / self.dest.height() * self.size.y)
    }

    /// Screen rectangle of an image-space `[x0, y0, x1, y1]`.
    pub fn rect_to_screen(&self, r: [f32; 4]) -> Rect {
        Rect::from_min_max(self.to_screen(Pos2::new(r[0], r[1])), self.to_screen(Pos2::new(r[2], r[3])))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
    New,
}

impl Handle {
    fn left(self) -> bool {
        matches!(self, Handle::W | Handle::NW | Handle::SW)
    }
    fn right(self) -> bool {
        matches!(self, Handle::E | Handle::NE | Handle::SE)
    }
    fn top(self) -> bool {
        matches!(self, Handle::N | Handle::NW | Handle::NE)
    }
    fn bottom(self) -> bool {
        matches!(self, Handle::S | Handle::SW | Handle::SE)
    }
}

/// Look and keyboard behaviour of a [`RoiEditor`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoiOptions {
    /// Outline and handle color (the warning color replaces it when too small).
    pub color: Color32,
    /// Darken the image outside the rectangle.
    pub shade_outside: bool,
    /// Rule-of-thirds guides inside the rectangle.
    pub thirds: bool,
    /// Show "W × H" next to the rectangle.
    pub size_label: bool,
    /// Holding Shift while drawing or resizing makes a square.
    pub shift_square: bool,
    /// Esc removes the rectangle (only while there is one, so Esc still
    /// reaches the caller otherwise).
    pub esc_clears: bool,
}

impl RoiOptions {
    /// The crop tool: shaded outside, thirds, no keyboard handling.
    pub fn crop() -> RoiOptions {
        RoiOptions { color: Color32::WHITE, shade_outside: true, thirds: true, size_label: false, shift_square: false, esc_clears: false }
    }

    /// An analysis region: outline in `color` with its size, Shift for a
    /// square, Esc clears.
    #[allow(dead_code)] // Used by the analysis panel.
    pub fn region(color: Color32) -> RoiOptions {
        RoiOptions { color, shade_outside: false, thirds: false, size_label: true, shift_square: true, esc_clears: true }
    }
}

/// What [`RoiEditor::show`] saw this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RoiResponse {
    /// The rectangle moved, was resized, drawn or cleared.
    pub changed: bool,
    /// Esc removed the rectangle.
    pub cleared: bool,
    /// A drag is in progress.
    pub dragging: bool,
    /// The rectangle is below the minimum size (see [`RoiEditor::set_min_size`]).
    pub too_small: bool,
}

const WARN: Color32 = Color32::from_rgb(240, 170, 40);

/// An editable rectangle in image pixels (`[x0, y0, x1, y1]`, x0 < x1, y0 < y1,
/// at least 1 px, inside the image).
pub struct RoiEditor {
    rect: Option<[f32; 4]>,
    /// Width / height lock; Shift (with `shift_square`) overrides it with 1.
    aspect: Option<f32>,
    pub opts: RoiOptions,
    min_size: Option<(u32, u32)>,
    min_note: String,
    drag: Option<(Handle, Pos2, [f32; 4])>,
    /// Image size the rectangle refers to.
    size: (f32, f32),
}

impl RoiEditor {
    /// No rectangle yet, on an image of `size` pixels.
    pub fn new(size: (f32, f32), opts: RoiOptions) -> RoiEditor {
        RoiEditor { rect: None, aspect: None, opts, min_size: None, min_note: String::new(), drag: None, size }
    }

    pub fn with_rect(mut self, r: [f32; 4]) -> RoiEditor {
        self.set_rect(Some(r));
        self
    }

    pub fn rect(&self) -> Option<[f32; 4]> {
        self.rect
    }

    /// Replace the rectangle (clamped to the image); `None` removes it.
    pub fn set_rect(&mut self, r: Option<[f32; 4]>) {
        self.rect = r.map(|mut r| {
            if r[0] > r[2] {
                r.swap(0, 2);
            }
            if r[1] > r[3] {
                r.swap(1, 3);
            }
            clamp_rect(&mut r, self.size.0, self.size.1);
            r
        });
        self.drag = None;
    }

    #[allow(dead_code)] // Used by the analysis panel.
    pub fn clear(&mut self) {
        self.set_rect(None);
    }

    /// The rectangle rounded to whole pixels: (x, y, w, h).
    #[allow(dead_code)] // Used by the analysis panel.
    pub fn pixel_rect(&self) -> Option<(u32, u32, u32, u32)> {
        self.rect.map(pixel_rect)
    }

    #[allow(dead_code)] // Used by the analysis panel.
    pub fn image_size(&self) -> (f32, f32) {
        self.size
    }

    /// The image the rectangle refers to changed size (for example after a
    /// rotate): scale the rectangle with it. Returns the (x, y) scale applied,
    /// if any, so callers can scale their own copies.
    pub fn set_image_size(&mut self, size: (f32, f32)) -> Option<(f32, f32)> {
        if self.size == size {
            return None;
        }
        let k = (size.0 / self.size.0.max(1.0), size.1 / self.size.1.max(1.0));
        self.size = size;
        self.drag = None;
        if let Some(r) = &mut self.rect {
            *r = [r[0] * k.0, r[1] * k.1, r[2] * k.0, r[3] * k.1];
            clamp_rect(r, size.0, size.1);
        }
        Some(k)
    }

    pub fn aspect(&self) -> Option<f32> {
        self.aspect
    }

    /// Lock width / height to `a` (`None` unlocks), fitting the current
    /// rectangle to it around its center.
    pub fn set_aspect(&mut self, a: Option<f32>) {
        self.aspect = a;
        let (Some(a), Some(r)) = (a, &mut self.rect) else { return };
        let (iw, ih) = self.size;
        let cx = (r[0] + r[2]) / 2.0;
        let cy = (r[1] + r[3]) / 2.0;
        let mut w = r[2] - r[0];
        let mut h = w / a;
        if h > ih {
            h = ih;
            w = h * a;
        }
        if w > iw {
            w = iw;
            h = w / a;
        }
        *r = [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0];
        clamp_rect(r, iw, ih);
    }

    /// Below `min` (pixels) the rectangle is drawn in a warning color with
    /// `note` (for example "MS-SSIM needs 176 px") next to its size, and
    /// [`RoiResponse::too_small`] is set. `None` removes the limit.
    #[allow(dead_code)] // Used by the analysis panel.
    pub fn set_min_size(&mut self, min: Option<(u32, u32)>, note: impl Into<String>) {
        self.min_size = min;
        self.min_note = note.into();
    }

    pub fn too_small(&self) -> bool {
        match (self.min_size, self.pixel_rect()) {
            (Some((mw, mh)), Some((_, _, w, h))) => w < mw || h < mh,
            _ => false,
        }
    }

    #[allow(dead_code)] // Used by the analysis panel.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Draw the rectangle with `painter` and handle dragging on `resp` (the
    /// image area's response; it needs a drag sense). `xf` maps this
    /// editor's image pixels to the screen.
    pub fn show(&mut self, ui: &Ui, painter: &Painter, xf: &ImageTransform, resp: &Response) -> RoiResponse {
        let mut out = RoiResponse::default();
        // Follow the transform's image size (callers may pass a resized image).
        self.set_image_size((xf.size.x, xf.size.y));
        if self.opts.esc_clears && self.rect.is_some() && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.set_rect(None);
            out.changed = true;
            out.cleared = true;
        }
        let (iw, ih) = self.size;
        let sr = self.rect.map(|r| xf.rect_to_screen(r));
        if let Some(sr) = sr {
            self.paint(painter, xf.dest, sr);
        }
        let hit = |pos: Pos2| -> Handle {
            let Some(sr) = sr else { return Handle::New };
            let m = 10.0;
            let near_l = (pos.x - sr.min.x).abs() < m;
            let near_r = (pos.x - sr.max.x).abs() < m;
            let near_t = (pos.y - sr.min.y).abs() < m;
            let near_b = (pos.y - sr.max.y).abs() < m;
            let in_y = pos.y > sr.min.y - m && pos.y < sr.max.y + m;
            let in_x = pos.x > sr.min.x - m && pos.x < sr.max.x + m;
            match (near_l && in_y, near_r && in_y, near_t && in_x, near_b && in_x) {
                (true, _, true, _) => Handle::NW,
                (_, true, true, _) => Handle::NE,
                (true, _, _, true) => Handle::SW,
                (_, true, _, true) => Handle::SE,
                (true, _, _, _) => Handle::W,
                (_, true, _, _) => Handle::E,
                (_, _, true, _) => Handle::N,
                (_, _, _, true) => Handle::S,
                _ if sr.contains(pos) => Handle::Move,
                _ => Handle::New,
            }
        };
        if let Some(pos) = resp.hover_pos() {
            let icon = match self.drag.map(|d| d.0).unwrap_or_else(|| hit(pos)) {
                Handle::NW | Handle::SE => CursorIcon::ResizeNwSe,
                Handle::NE | Handle::SW => CursorIcon::ResizeNeSw,
                Handle::W | Handle::E => CursorIcon::ResizeHorizontal,
                Handle::N | Handle::S => CursorIcon::ResizeVertical,
                Handle::Move => CursorIcon::Move,
                Handle::New => CursorIcon::Crosshair,
            };
            ui.ctx().set_cursor_icon(icon);
        }
        if resp.drag_started() {
            // Where the button went down: the drag is only detected after
            // the pointer moved a little, which may have left a handle.
            let origin = ui.input(|i| i.pointer.press_origin());
            if let Some(pos) = origin.filter(|p| resp.rect.contains(*p)).or_else(|| resp.interact_pointer_pos()) {
                self.drag = Some((hit(pos), pos, self.rect.unwrap_or([0.0; 4])));
            }
        }
        if resp.dragged() {
            if let (Some((h, start, orig)), Some(pos)) = (self.drag, resp.interact_pointer_pos()) {
                let square = self.opts.shift_square && ui.input(|i| i.modifiers.shift);
                let aspect = if square { Some(1.0) } else { self.aspect };
                let r = drag_rect(h, xf.to_image(start), xf.to_image(pos), orig, aspect, (iw, ih));
                if self.rect != Some(r) {
                    self.rect = Some(r);
                    out.changed = true;
                }
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        out.dragging = self.drag.is_some();
        out.too_small = self.too_small();
        out
    }

    fn paint(&self, painter: &Painter, dest: Rect, sr: Rect) {
        let warn = self.too_small();
        let color = if warn { WARN } else { self.opts.color };
        if self.opts.shade_outside {
            let shade = Color32::from_black_alpha(150);
            painter.rect_filled(Rect::from_min_max(dest.min, Pos2::new(dest.max.x, sr.min.y)), 0.0, shade);
            painter.rect_filled(Rect::from_min_max(Pos2::new(dest.min.x, sr.max.y), dest.max), 0.0, shade);
            painter.rect_filled(Rect::from_min_max(Pos2::new(dest.min.x, sr.min.y), Pos2::new(sr.min.x, sr.max.y)), 0.0, shade);
            painter.rect_filled(Rect::from_min_max(Pos2::new(sr.max.x, sr.min.y), Pos2::new(dest.max.x, sr.max.y)), 0.0, shade);
        } else {
            // A dark halo keeps the outline visible on light and dark images.
            painter.rect_stroke(sr, 0.0, Stroke::new(3.5, Color32::from_black_alpha(110)), egui::StrokeKind::Middle);
        }
        painter.rect_stroke(sr, 0.0, Stroke::new(1.5, color), egui::StrokeKind::Middle);
        if self.opts.thirds {
            for i in 1..3 {
                let fx = sr.min.x + sr.width() * i as f32 / 3.0;
                let fy = sr.min.y + sr.height() * i as f32 / 3.0;
                painter.line_segment([Pos2::new(fx, sr.min.y), Pos2::new(fx, sr.max.y)], Stroke::new(1.0, Color32::from_white_alpha(70)));
                painter.line_segment([Pos2::new(sr.min.x, fy), Pos2::new(sr.max.x, fy)], Stroke::new(1.0, Color32::from_white_alpha(70)));
            }
        }
        for pt in [sr.left_top(), sr.right_top(), sr.left_bottom(), sr.right_bottom()] {
            painter.rect_filled(Rect::from_center_size(pt, Vec2::splat(10.0)), 2.0, color);
        }
        if self.opts.size_label {
            let Some((_, _, w, h)) = self.pixel_rect() else { return };
            let mut text = format!("{w} × {h}");
            if warn && !self.min_note.is_empty() {
                text = format!("{text}  {}", self.min_note);
            }
            let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), if warn { WARN } else { Color32::WHITE });
            let pad = Vec2::new(6.0, 3.0);
            let size = galley.size() + pad * 2.0;
            // Below the rectangle, or inside it when that would leave the view;
            // always inside the visible pane (a region at its right edge with
            // the "too small" note would otherwise be cut off).
            let clip = painter.clip_rect().intersect(dest);
            let below = sr.left_bottom() + Vec2::new(0.0, 8.0);
            let mut min = if below.y + size.y <= clip.max.y { below } else { sr.left_bottom() - Vec2::new(-8.0, size.y + 8.0) };
            min.x = min.x.min(clip.max.x - size.x - 4.0).max(clip.min.x + 4.0);
            min.y = min.y.min(clip.max.y - size.y - 4.0).max(clip.min.y + 4.0);
            let bg = Rect::from_min_size(min, size);
            painter.rect_filled(bg, 4.0, Color32::from_black_alpha(170));
            painter.galley(bg.min + pad, galley, Color32::WHITE);
        }
    }
}

/// The rectangle after dragging handle `h` from `start` to `pos` (image
/// pixels), starting from `orig`.
fn drag_rect(h: Handle, start: Pos2, pos: Pos2, orig: [f32; 4], aspect: Option<f32>, (iw, ih): (f32, f32)) -> [f32; 4] {
    let (sx, sy, px, py) = (start.x, start.y, pos.x, pos.y);
    let (dx, dy) = (px - sx, py - sy);
    let mut r = orig;
    match h {
        Handle::Move => {
            let w = r[2] - r[0];
            let hh = r[3] - r[1];
            r[0] = safe_clamp(orig[0] + dx, 0.0, (iw - w).max(0.0));
            r[1] = safe_clamp(orig[1] + dy, 0.0, (ih - hh).max(0.0));
            r[2] = r[0] + w;
            r[3] = r[1] + hh;
        }
        Handle::New => {
            r = [sx.min(px), sy.min(py), sx.max(px), sy.max(py)];
            if let Some(a) = aspect {
                let w = r[2] - r[0];
                let hgt = w / a;
                if py >= sy {
                    r[3] = r[1] + hgt
                } else {
                    r[1] = r[3] - hgt
                }
            }
        }
        _ => {
            if h.left() {
                r[0] = orig[0] + dx;
            }
            if h.right() {
                r[2] = orig[2] + dx;
            }
            if h.top() {
                r[1] = orig[1] + dy;
            }
            if h.bottom() {
                r[3] = orig[3] + dy;
            }
            if let Some(a) = aspect {
                if matches!(h, Handle::N | Handle::S) {
                    // Top or bottom edge: the width follows the height, which
                    // stops at the image border (vertically, or where the
                    // width reaches the right edge).
                    let sign = if r[3] >= r[1] { 1.0 } else { -1.0 };
                    let room = match (h, sign > 0.0) {
                        (Handle::N, true) => r[3],
                        (Handle::S, true) => ih - r[1],
                        _ => f32::INFINITY,
                    };
                    let hgt = (r[3] - r[1]).abs().min((iw - r[0]).max(1.0) / a).min(room.max(1.0));
                    if h == Handle::N {
                        r[1] = r[3] - sign * hgt;
                    } else {
                        r[3] = r[1] + sign * hgt;
                    }
                    r[2] = r[0] + hgt * a;
                } else {
                    let w = (r[2] - r[0]).abs();
                    let hgt = w / a;
                    if h.top() {
                        r[1] = r[3] - hgt;
                    } else {
                        r[3] = r[1] + hgt;
                    }
                }
            }
        }
    }
    if r[0] > r[2] {
        r.swap(0, 2);
    }
    if r[1] > r[3] {
        r.swap(1, 3);
    }
    clamp_rect(&mut r, iw, ih);
    r
}

/// `[x0, y0, x1, y1]` rounded to whole pixels: (x, y, w, h).
pub fn pixel_rect(r: [f32; 4]) -> (u32, u32, u32, u32) {
    let (x, y) = (r[0].round().max(0.0) as u32, r[1].round().max(0.0) as u32);
    let (w, h) = ((r[2] - r[0]).round().max(1.0) as u32, (r[3] - r[1]).round().max(1.0) as u32);
    (x, y, w, h)
}

/// `v` limited to `lo..=hi` that never panics: an inverted range collapses
/// to `lo` and NaN bounds are ignored (`f32::clamp` panics on both).
fn safe_clamp(v: f32, lo: f32, hi: f32) -> f32 {
    v.max(lo).min(hi.max(lo))
}

/// Keep `r` inside an `iw` x `ih` image and at least 1 px in each direction.
pub fn clamp_rect(r: &mut [f32; 4], iw: f32, ih: f32) {
    let (iw, ih) = (iw.max(1.0), ih.max(1.0));
    r[0] = safe_clamp(r[0], 0.0, iw - 1.0);
    r[1] = safe_clamp(r[1], 0.0, ih - 1.0);
    r[2] = safe_clamp(r[2], r[0] + 1.0, iw);
    r[3] = safe_clamp(r[3], r[1] + 1.0, ih);
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: (f32, f32) = (200.0, 100.0);

    fn p(x: f32, y: f32) -> Pos2 {
        Pos2::new(x, y)
    }

    #[test]
    fn clamp_never_panics() {
        // Rect larger than the (rotated) image.
        let mut r = [10.0, 10.0, 900.0, 300.0];
        clamp_rect(&mut r, 200.0, 0.5);
        assert!(r[0] <= r[2] && r[1] <= r[3] && r[2] <= 200.0);
        assert!(safe_clamp(1.0, f32::NAN, f32::NAN).is_finite());
    }

    #[test]
    fn draw_move_resize() {
        // Drawing up and to the left still gives an ordered rectangle.
        assert_eq!(drag_rect(Handle::New, p(50.0, 60.0), p(20.0, 10.0), [0.0; 4], None, IMG), [20.0, 10.0, 50.0, 60.0]);
        // Moving stops at the image edge without changing the size.
        let r = [20.0, 10.0, 50.0, 60.0];
        assert_eq!(drag_rect(Handle::Move, p(30.0, 30.0), p(500.0, -40.0), r, None, IMG), [170.0, 0.0, 200.0, 50.0]);
        // Corner and edge resizing.
        assert_eq!(drag_rect(Handle::SE, p(50.0, 60.0), p(70.0, 65.0), r, None, IMG), [20.0, 10.0, 70.0, 65.0]);
        assert_eq!(drag_rect(Handle::W, p(20.0, 30.0), p(5.0, 99.0), r, None, IMG), [5.0, 10.0, 50.0, 60.0]);
        // Dragging an edge across the opposite one flips instead of collapsing.
        assert_eq!(drag_rect(Handle::E, p(50.0, 30.0), p(0.0, 30.0), r, None, IMG), [0.0, 10.0, 20.0, 60.0]);
    }

    #[test]
    fn aspect_and_square() {
        // Square (Shift) while drawing and resizing.
        assert_eq!(drag_rect(Handle::New, p(10.0, 10.0), p(40.0, 90.0), [0.0; 4], Some(1.0), IMG), [10.0, 10.0, 40.0, 40.0]);
        let r = [20.0, 20.0, 60.0, 40.0];
        assert_eq!(drag_rect(Handle::SE, p(60.0, 40.0), p(80.0, 41.0), r, Some(1.0), IMG), [20.0, 20.0, 80.0, 80.0]);
        // Top/bottom edges drive the width under a lock.
        assert_eq!(drag_rect(Handle::S, p(40.0, 40.0), p(40.0, 50.0), r, Some(2.0), IMG), [20.0, 20.0, 80.0, 50.0]);
        // ... up to the right edge of the image, keeping the aspect.
        assert_eq!(drag_rect(Handle::N, p(40.0, 20.0), p(40.0, -60.0), r, Some(2.0), IMG), [20.0, 0.0, 100.0, 40.0]);
        assert_eq!(drag_rect(Handle::S, p(40.0, 40.0), p(40.0, 99.0), r, Some(4.0), IMG), [20.0, 20.0, 200.0, 65.0]);
        // Fitting a new aspect keeps the center.
        let mut e = RoiEditor::new(IMG, RoiOptions::crop()).with_rect([20.0, 20.0, 100.0, 60.0]);
        e.set_aspect(Some(1.0));
        assert_eq!(e.rect(), Some([20.0, 0.0, 100.0, 80.0]));
        e.set_aspect(None);
        assert_eq!(e.rect(), Some([20.0, 0.0, 100.0, 80.0]));
    }

    #[test]
    fn esc_clears_only_when_asked() {
        let ctx = egui::Context::default();
        let xf = ImageTransform::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0)), (200, 100));
        let frame = |e: &mut RoiEditor, events: Vec<egui::Event>| -> RoiResponse {
            let mut out = RoiResponse::default();
            let mut full = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                let resp = ui.allocate_rect(xf.dest, egui::Sense::click_and_drag());
                out = e.show(ui, ui.painter(), &xf, &resp);
            });
            // No renderer here: drop the font atlas upload (debug builds assert
            // that texture deltas are applied).
            full.textures_delta.clear();
            out
        };
        let esc = || vec![egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }];
        let mut crop = RoiEditor::new(IMG, RoiOptions::crop()).with_rect([10.0, 10.0, 50.0, 50.0]);
        frame(&mut crop, vec![]);
        assert!(!frame(&mut crop, esc()).cleared);
        assert!(crop.rect().is_some());
        let mut roi = RoiEditor::new(IMG, RoiOptions::region(Color32::WHITE)).with_rect([10.0, 10.0, 50.0, 50.0]);
        frame(&mut roi, vec![]);
        let out = frame(&mut roi, esc());
        assert!(out.cleared && out.changed && roi.rect().is_none());
    }

    #[test]
    fn size_changes_min_size_and_pixels() {
        let mut e = RoiEditor::new(IMG, RoiOptions::region(Color32::WHITE)).with_rect([10.4, 20.6, 60.0, 50.0]);
        assert_eq!(e.pixel_rect(), Some((10, 21, 50, 29)));
        assert!(!e.too_small());
        e.set_min_size(Some((176, 176)), "MS-SSIM needs 176 px");
        assert!(e.too_small());
        assert_eq!(e.set_image_size((400.0, 50.0)), Some((2.0, 0.5)));
        assert_eq!(e.set_image_size((400.0, 50.0)), None);
        assert_eq!(e.rect(), Some([20.8, 10.3, 120.0, 25.0]));
        e.clear();
        assert_eq!(e.rect(), None);
        assert!(!e.too_small());
        // A transform maps image pixels both ways.
        let xf = ImageTransform::new(Rect::from_min_size(p(100.0, 50.0), Vec2::new(400.0, 200.0)), (200, 100));
        assert_eq!(xf.to_screen(p(50.0, 25.0)), p(200.0, 100.0));
        assert_eq!(xf.to_image(p(200.0, 100.0)), p(50.0, 25.0));
    }
}
