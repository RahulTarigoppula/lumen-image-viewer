//! Vector annotation objects for the Draw Board and their anti-aliased
//! rasterization (tiny-skia). Each object is rendered into its own small
//! layer covering only its bounding box, then composited onto the photo, so
//! a few arrows on a 50 MP image cost almost nothing.

use super::{filters, geom, text};
use image::RgbaImage;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Line,
    Arrow,
    Rect,
    RoundRect,
    Ellipse,
    Highlighter,
    Pen,
    Text,
    Number,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Line => "Line",
            Kind::Arrow => "Arrow",
            Kind::Rect => "Rectangle",
            Kind::RoundRect => "Rounded rectangle",
            Kind::Ellipse => "Ellipse",
            Kind::Highlighter => "Highlighter",
            Kind::Pen => "Pen",
            Kind::Text => "Text",
            Kind::Number => "Number marker",
        }
    }
    /// Whether the object is defined by two corner / end points.
    pub fn is_two_point(self) -> bool {
        matches!(self, Kind::Line | Kind::Arrow | Kind::Rect | Kind::RoundRect | Kind::Ellipse)
    }
    pub fn is_freehand(self) -> bool {
        matches!(self, Kind::Pen | Kind::Highlighter)
    }
    pub fn has_fill(self) -> bool {
        matches!(self, Kind::Rect | Kind::RoundRect | Kind::Ellipse | Kind::Text)
    }
}

/// Text box style.
pub const CALLOUT_PLAIN: u8 = 0;
pub const CALLOUT_BOX: u8 = 1;
pub const CALLOUT_BUBBLE: u8 = 2;

/// One annotation. Coordinates and sizes are full-resolution image pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawObj {
    pub kind: Kind,
    /// Line/Arrow: [start, end]. Rect/RoundRect/Ellipse: two corners.
    /// Pen/Highlighter: the polyline. Text: [box min, box max, tail tip].
    /// Number: [centre].
    pub pts: Vec<[f32; 2]>,
    /// Stroke colour (Number: disc colour). Straight RGBA.
    pub color: [u8; 4],
    pub fill: Option<[u8; 4]>,
    pub width: f32,
    /// 0..1 for the whole object.
    pub opacity: f32,
    pub text: String,
    /// Text size (Number: disc diameter).
    pub font_px: f32,
    pub text_color: [u8; 4],
    pub bold: bool,
    pub callout: u8,
    pub number: u32,
    pub both_heads: bool,
    pub dashed: bool,
    pub shadow: bool,
}

impl Default for DrawObj {
    fn default() -> Self {
        DrawObj {
            kind: Kind::Arrow,
            pts: Vec::new(),
            color: [230, 40, 40, 255],
            fill: None,
            width: 6.0,
            opacity: 1.0,
            text: String::new(),
            font_px: 48.0,
            text_color: [20, 20, 20, 255],
            bold: true,
            callout: CALLOUT_BUBBLE,
            number: 1,
            both_heads: false,
            dashed: false,
            shadow: false,
        }
    }
}

#[inline]
fn finite(p: [f32; 2]) -> bool {
    p[0].is_finite() && p[1].is_finite()
}

pub fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let (wx, wy) = (p[0] - a[0], p[1] - a[1]);
    let l2 = vx * vx + vy * vy;
    let t = if l2 > 1e-9 { ((wx * vx + wy * vy) / l2).clamp(0.0, 1.0) } else { 0.0 };
    ((wx - t * vx).powi(2) + (wy - t * vy).powi(2)).sqrt()
}

fn dist_to_polyline(p: [f32; 2], pts: &[[f32; 2]]) -> f32 {
    match pts.len() {
        0 => f32::MAX,
        1 => dist_to_segment(p, pts[0], pts[0]),
        _ => pts.windows(2).map(|s| dist_to_segment(p, s[0], s[1])).fold(f32::MAX, f32::min),
    }
}

/// Size of `text` set at `px` (cheap: measured at a small size and scaled,
/// glyph outlines scale linearly).
pub fn measure_text(text: &str, px: f32, bold: bool) -> [f32; 2] {
    if text.trim().is_empty() {
        return [px * 0.6, px];
    }
    let probe = 64.0;
    let font = text::load(0, bold, false);
    let st = text::TextStyle { px: probe, ..Default::default() };
    let s = text::render_stamp(&font, text, &st);
    let k = px / probe;
    [s.anchor_w * k, s.anchor_h * k]
}

impl DrawObj {
    pub fn is_valid(&self) -> bool {
        if self.pts.is_empty() || !self.pts.iter().all(|p| finite(*p)) {
            return false;
        }
        match self.kind {
            k if k.is_two_point() => {
                self.pts.len() >= 2 && {
                    let (a, b) = (self.pts[0], self.pts[1]);
                    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() >= 1.0
                }
            }
            Kind::Text => self.pts.len() >= 3 && !self.text.trim().is_empty(),
            _ => true,
        }
    }

    /// Padding between a text box border and its text.
    pub fn text_pad(&self) -> f32 {
        if self.callout == CALLOUT_PLAIN {
            0.0
        } else {
            self.font_px * 0.45
        }
    }

    /// Resize a text box to fit its text (keeps the top-left corner).
    pub fn fit_text_box(&mut self) {
        if self.kind != Kind::Text || self.pts.is_empty() {
            return;
        }
        let [tw, th] = measure_text(&self.text, self.font_px, self.bold);
        let pad = self.text_pad();
        let p0 = self.pts[0];
        let p1 = [p0[0] + tw + 2.0 * pad, p0[1] + th + 2.0 * pad];
        if self.pts.len() < 2 {
            self.pts.push(p1);
        } else {
            self.pts[1] = p1;
        }
        if self.pts.len() < 3 {
            self.pts.push([p0[0] - self.font_px, p1[1] + self.font_px * 1.6]);
        }
    }

    /// Geometric bounding box (x0, y0, x1, y1) without stroke / shadow extent.
    pub fn bounds(&self) -> [f32; 4] {
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        let pts: &[[f32; 2]] = if self.kind == Kind::Text && self.callout != CALLOUT_BUBBLE { &self.pts[..self.pts.len().min(2)] } else { &self.pts };
        for p in pts {
            b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
        }
        if self.kind == Kind::Number {
            let r = self.font_px / 2.0;
            if let Some(c) = self.pts.first() {
                b = [c[0] - r, c[1] - r, c[0] + r, c[1] + r];
            }
        }
        if self.pts.is_empty() {
            [0.0; 4]
        } else {
            b
        }
    }

    /// Bounds including everything that can be painted.
    fn paint_bounds(&self) -> [f32; 4] {
        let b = self.bounds();
        let mut e = self.width.max(0.0) * 0.5 + 2.0;
        if self.kind == Kind::Arrow {
            e += self.head_len();
        }
        if self.shadow {
            let (off, blur) = self.shadow_params();
            e += off + blur * 3.0;
        }
        [b[0] - e, b[1] - e, b[2] + e, b[3] + e]
    }

    pub fn head_len(&self) -> f32 {
        (self.width * 4.0).max(self.width + 10.0)
    }

    fn shadow_params(&self) -> (f32, f32) {
        let base = self.width.max(self.font_px * 0.08).max(2.0);
        (base * 0.8, base * 0.9)
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        for p in &mut self.pts {
            p[0] += dx;
            p[1] += dy;
        }
    }

    pub fn scaled(&self, s: f32) -> DrawObj {
        DrawObj { pts: self.pts.iter().map(|p| [p[0] * s, p[1] * s]).collect(), width: self.width * s, font_px: self.font_px * s, ..self.clone() }
    }

    /// Whether image point `p` touches the object (`tol` extra pixels).
    pub fn hit(&self, p: [f32; 2], tol: f32) -> bool {
        if !self.is_valid() && !(self.kind == Kind::Text && self.pts.len() >= 2) {
            return false;
        }
        let hw = self.width * 0.5 + tol;
        let b = self.bounds();
        let inside_box = p[0] >= b[0] - tol && p[0] <= b[2] + tol && p[1] >= b[1] - tol && p[1] <= b[3] + tol;
        match self.kind {
            Kind::Line | Kind::Arrow => dist_to_segment(p, self.pts[0], self.pts[1]) <= hw.max(tol),
            Kind::Pen | Kind::Highlighter => dist_to_polyline(p, &self.pts) <= hw.max(tol),
            Kind::Rect | Kind::RoundRect => {
                if self.fill.is_some() {
                    inside_box
                } else {
                    let d = [(p[0] - b[0]).abs(), (p[0] - b[2]).abs(), (p[1] - b[1]).abs(), (p[1] - b[3]).abs()];
                    let near_v = (d[0] <= hw || d[1] <= hw) && p[1] >= b[1] - hw && p[1] <= b[3] + hw;
                    let near_h = (d[2] <= hw || d[3] <= hw) && p[0] >= b[0] - hw && p[0] <= b[2] + hw;
                    near_v || near_h
                }
            }
            Kind::Ellipse => {
                let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
                let (rx, ry) = (((b[2] - b[0]) / 2.0).max(0.5), ((b[3] - b[1]) / 2.0).max(0.5));
                let n = (((p[0] - cx) / rx).powi(2) + ((p[1] - cy) / ry).powi(2)).sqrt();
                if self.fill.is_some() {
                    n <= 1.0 + hw / rx.min(ry)
                } else {
                    (n - 1.0).abs() * rx.min(ry) <= hw
                }
            }
            Kind::Text => {
                let r = [self.pts[0], self.pts[1]];
                let in_rect = p[0] >= r[0][0].min(r[1][0]) - tol && p[0] <= r[0][0].max(r[1][0]) + tol && p[1] >= r[0][1].min(r[1][1]) - tol && p[1] <= r[0][1].max(r[1][1]) + tol;
                let on_tail = self.callout == CALLOUT_BUBBLE && self.pts.len() >= 3 && {
                    let c = [(r[0][0] + r[1][0]) / 2.0, (r[0][1] + r[1][1]) / 2.0];
                    dist_to_segment(p, c, self.pts[2]) <= tol.max(self.font_px * 0.2)
                };
                in_rect || on_tail
            }
            Kind::Number => {
                let c = self.pts[0];
                ((p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sqrt() <= self.font_px / 2.0 + tol
            }
        }
    }
}

fn paint(c: [u8; 4], alpha: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], ((c[3] as f32) * alpha.clamp(0.0, 1.0)).round() as u8);
    p.anti_alias = true;
    p
}

fn stroke(width: f32, dashed: bool) -> Stroke {
    let width = width.max(0.1);
    Stroke {
        width,
        line_cap: if dashed { LineCap::Butt } else { LineCap::Round },
        line_join: LineJoin::Round,
        dash: if dashed { StrokeDash::new(vec![width * 2.6, width * 1.8], 0.0) } else { None },
        ..Default::default()
    }
}

/// Rounded-rectangle path with an optional speech-bubble tail towards `tail`.
pub fn bubble_path(r: [f32; 4], radius: f32, tail: Option<[f32; 2]>, tail_w: f32) -> Option<tiny_skia::Path> {
    let (x0, y0, x1, y1) = (r[0].min(r[2]), r[1].min(r[3]), r[0].max(r[2]), r[1].max(r[3]));
    if !(x1 - x0 >= 1.0 && y1 - y0 >= 1.0) {
        return None;
    }
    let rr = radius.clamp(0.0, ((x1 - x0).min(y1 - y0)) / 2.0);
    // Which side the tail leaves from (0 top, 1 right, 2 bottom, 3 left).
    let side = tail.filter(|t| finite(*t)).and_then(|t| {
        let over = [y0 - t[1], t[0] - x1, t[1] - y1, x0 - t[0]];
        let (i, v) = over.iter().copied().enumerate().max_by(|a, b| a.1.total_cmp(&b.1))?;
        (v > 1.0).then_some((i, t))
    });
    let k = 0.5523 * rr;
    let mut pb = PathBuilder::new();
    let tail_on = |edge: usize, lo: f32, hi: f32| -> Option<(f32, f32, [f32; 2])> {
        let (s, t) = side?;
        if s != edge {
            return None;
        }
        let hw = tail_w.min((hi - lo) / 2.0 - 1.0);
        if hw < 1.0 {
            return None;
        }
        let along = if edge % 2 == 0 { t[0] } else { t[1] };
        let c = along.clamp(lo + hw, hi - hw);
        Some((c - hw, c + hw, t))
    };
    pb.move_to(x0 + rr, y0);
    if let Some((a, b, t)) = tail_on(0, x0 + rr, x1 - rr) {
        pb.line_to(a, y0);
        pb.line_to(t[0], t[1]);
        pb.line_to(b, y0);
    }
    pb.line_to(x1 - rr, y0);
    pb.cubic_to(x1 - rr + k, y0, x1, y0 + rr - k, x1, y0 + rr);
    if let Some((a, b, t)) = tail_on(1, y0 + rr, y1 - rr) {
        pb.line_to(x1, a);
        pb.line_to(t[0], t[1]);
        pb.line_to(x1, b);
    }
    pb.line_to(x1, y1 - rr);
    pb.cubic_to(x1, y1 - rr + k, x1 - rr + k, y1, x1 - rr, y1);
    if let Some((a, b, t)) = tail_on(2, x0 + rr, x1 - rr) {
        pb.line_to(b, y1);
        pb.line_to(t[0], t[1]);
        pb.line_to(a, y1);
    }
    pb.line_to(x0 + rr, y1);
    pb.cubic_to(x0 + rr - k, y1, x0, y1 - rr + k, x0, y1 - rr);
    if let Some((a, b, t)) = tail_on(3, y0 + rr, y1 - rr) {
        pb.line_to(x0, b);
        pb.line_to(t[0], t[1]);
        pb.line_to(x0, a);
    }
    pb.line_to(x0, y0 + rr);
    pb.cubic_to(x0, y0 + rr - k, x0 + rr - k, y0, x0 + rr, y0);
    pb.close();
    pb.finish()
}

/// Smooth freehand path (quadratic curves through segment midpoints).
fn smooth_path(pts: &[[f32; 2]]) -> Option<tiny_skia::Path> {
    let first = *pts.first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first[0], first[1]);
    if pts.len() == 1 {
        // A dot: zero-length segment drawn with a round cap.
        pb.line_to(first[0] + 0.01, first[1]);
    } else if pts.len() == 2 {
        pb.line_to(pts[1][0], pts[1][1]);
    } else {
        for i in 1..pts.len() - 1 {
            let (p, q) = (pts[i], pts[i + 1]);
            pb.quad_to(p[0], p[1], (p[0] + q[0]) / 2.0, (p[1] + q[1]) / 2.0);
        }
        let l = pts[pts.len() - 1];
        pb.line_to(l[0], l[1]);
    }
    pb.finish()
}

fn line_path(a: [f32; 2], b: [f32; 2]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(a[0], a[1]);
    pb.line_to(b[0], b[1]);
    pb.finish()
}

/// Arrow head polygon with its tip at `tip`, pointing away from `from`.
fn head_path(from: [f32; 2], tip: [f32; 2], len: f32) -> Option<(tiny_skia::Path, [f32; 2])> {
    let (dx, dy) = (tip[0] - from[0], tip[1] - from[1]);
    let l = (dx * dx + dy * dy).sqrt();
    if l < 1e-3 {
        return None;
    }
    let len = len.min(l * 0.9);
    let (ux, uy) = (dx / l, dy / l);
    let (nx, ny) = (-uy, ux);
    let half = len * 0.5;
    let base = [tip[0] - ux * len, tip[1] - uy * len];
    let notch = [tip[0] - ux * len * 0.78, tip[1] - uy * len * 0.78];
    let mut pb = PathBuilder::new();
    pb.move_to(tip[0], tip[1]);
    pb.line_to(base[0] + nx * half, base[1] + ny * half);
    pb.line_to(notch[0], notch[1]);
    pb.line_to(base[0] - nx * half, base[1] - ny * half);
    pb.close();
    Some((pb.finish()?, [tip[0] - ux * len * 0.7, tip[1] - uy * len * 0.7]))
}

/// Draw `o` (already scaled and translated into layer space) into `pm`.
fn draw_shape(pm: &mut Pixmap, o: &DrawObj) {
    let tr = Transform::identity();
    let a = 1.0;
    match o.kind {
        Kind::Line => {
            if let Some(p) = line_path(o.pts[0], o.pts[1]) {
                pm.stroke_path(&p, &paint(o.color, a), &stroke(o.width, o.dashed), tr, None);
            }
        }
        Kind::Arrow => {
            let (mut s, mut e) = (o.pts[0], o.pts[1]);
            let hl = o.head_len();
            let col = paint(o.color, a);
            if let Some((h, back)) = head_path(s, e, hl) {
                pm.fill_path(&h, &col, FillRule::Winding, tr, None);
                e = back;
            }
            if o.both_heads {
                if let Some((h, back)) = head_path(o.pts[1], s, hl) {
                    pm.fill_path(&h, &col, FillRule::Winding, tr, None);
                    s = back;
                }
            }
            if let Some(p) = line_path(s, e) {
                pm.stroke_path(&p, &col, &stroke(o.width, o.dashed), tr, None);
            }
        }
        Kind::Rect | Kind::RoundRect | Kind::Ellipse => {
            let b = o.bounds();
            let path = match o.kind {
                Kind::Ellipse => tiny_skia::Rect::from_ltrb(b[0], b[1], b[2], b[3]).and_then(PathBuilder::from_oval),
                Kind::RoundRect => {
                    let short = (b[2] - b[0]).min(b[3] - b[1]);
                    bubble_path(b, (o.width * 2.5).max(short * 0.14), None, 0.0)
                }
                _ => tiny_skia::Rect::from_ltrb(b[0], b[1], b[2], b[3]).map(PathBuilder::from_rect),
            };
            if let Some(p) = path {
                if let Some(f) = o.fill {
                    pm.fill_path(&p, &paint(f, a), FillRule::Winding, tr, None);
                }
                if o.width > 0.05 {
                    let mut st = stroke(o.width, o.dashed);
                    if o.kind == Kind::Rect {
                        st.line_join = LineJoin::Miter;
                    }
                    pm.stroke_path(&p, &paint(o.color, a), &st, tr, None);
                }
            }
        }
        Kind::Pen | Kind::Highlighter => {
            if let Some(p) = smooth_path(&o.pts) {
                let mut st = stroke(o.width, o.dashed && o.kind == Kind::Pen);
                if o.kind == Kind::Highlighter {
                    st.line_cap = LineCap::Square;
                }
                pm.stroke_path(&p, &paint(o.color, a), &st, tr, None);
            }
        }
        Kind::Text => {
            if o.callout != CALLOUT_PLAIN && o.pts.len() >= 2 {
                let b = [o.pts[0][0], o.pts[0][1], o.pts[1][0], o.pts[1][1]];
                let tail = if o.callout == CALLOUT_BUBBLE { o.pts.get(2).copied() } else { None };
                if let Some(p) = bubble_path(b, o.font_px * 0.35, tail, o.font_px * 0.35) {
                    pm.fill_path(&p, &paint(o.fill.unwrap_or([255, 255, 255, 255]), a), FillRule::Winding, tr, None);
                    if o.width > 0.05 {
                        pm.stroke_path(&p, &paint(o.color, a), &stroke(o.width, o.dashed), tr, None);
                    }
                }
            }
        }
        Kind::Number => {
            let c = o.pts[0];
            let r = o.font_px / 2.0;
            if let Some(p) = PathBuilder::from_circle(c[0], c[1], r.max(0.5)) {
                pm.fill_path(&p, &paint(o.color, a), FillRule::Winding, tr, None);
                let ring = (r * 0.09).max(1.0);
                if let Some(p2) = PathBuilder::from_circle(c[0], c[1], (r - ring / 2.0).max(0.5)) {
                    pm.stroke_path(&p2, &paint([255, 255, 255, 230], a), &stroke(ring, false), tr, None);
                }
            }
        }
    }
}

/// Premultiplied pixmap to a straight-alpha image.
fn pixmap_to_rgba(pm: &Pixmap) -> RgbaImage {
    let mut out = RgbaImage::new(pm.width(), pm.height());
    for (d, s) in out.as_mut().chunks_exact_mut(4).zip(pm.pixels()) {
        let c = s.demultiply();
        d.copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

/// Text / numbers on top of the vector part (layer space).
fn draw_text(layer: &mut RgbaImage, o: &DrawObj) {
    let (txt, px, color, center) = match o.kind {
        Kind::Text if o.pts.len() >= 2 && !o.text.trim().is_empty() => {
            let c = [(o.pts[0][0] + o.pts[1][0]) / 2.0, (o.pts[0][1] + o.pts[1][1]) / 2.0];
            (o.text.clone(), o.font_px, o.text_color, c)
        }
        Kind::Number if !o.pts.is_empty() => {
            let s = o.number.to_string();
            let px = o.font_px * if s.len() > 2 { 0.45 } else { 0.6 };
            (s, px, o.text_color, o.pts[0])
        }
        _ => return,
    };
    if !(px >= 1.0) {
        return;
    }
    let font = text::load(0, o.bold || o.kind == Kind::Number, false);
    let plain_shadow = o.kind == Kind::Text && o.callout == CALLOUT_PLAIN && o.shadow;
    let st = text::TextStyle {
        px,
        color,
        align: 1,
        shadow: plain_shadow,
        shadow_dx: px * 0.05,
        shadow_dy: px * 0.05,
        shadow_blur: px * 0.06,
        ..Default::default()
    };
    let stamp = text::render_stamp(&font, &txt, &st);
    let x = center[0] - stamp.anchor_w / 2.0;
    // Optical centring: glyph boxes include the descender area.
    let y = center[1] - stamp.anchor_h / 2.0 + if o.kind == Kind::Number { px * 0.04 } else { 0.0 };
    text::place_stamp(layer, &stamp, x, y, 1.0);
}

/// Composite a straight-alpha layer at (`ox`, `oy`), normally or as a
/// multiply (highlighter), with extra `opacity`.
fn composite(img: &mut RgbaImage, layer: &RgbaImage, ox: u32, oy: u32, opacity: f32, multiply: bool) {
    if !multiply {
        geom::composite_over(img, layer, ox as i64, oy as i64, opacity);
        return;
    }
    use rayon::prelude::*;
    let (iw, ih) = img.dimensions();
    let (lw, lh) = layer.dimensions();
    let lraw = layer.as_raw();
    img.as_mut().par_chunks_mut(iw as usize * 4).enumerate().skip(oy as usize).take(lh as usize).for_each(|(y, row)| {
        let ly = y - oy as usize;
        if y >= ih as usize {
            return;
        }
        for lx in 0..lw as usize {
            let x = ox as usize + lx;
            if x >= iw as usize {
                break;
            }
            let si = (ly * lw as usize + lx) * 4;
            let a = lraw[si + 3] as f32 / 255.0 * opacity;
            if a <= 0.0 {
                continue;
            }
            let d = &mut row[x * 4..x * 4 + 4];
            for c in 0..3 {
                let m = d[c] as f32 * lraw[si + c] as f32 / 255.0;
                d[c] = (d[c] as f32 + (m - d[c] as f32) * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
}

/// Render one object onto `img` (`o` in full-resolution pixels, drawn at `scale`).
pub fn render_obj(img: &mut RgbaImage, o: &DrawObj, scale: f32) {
    if !o.is_valid() || !(scale > 0.0) {
        return;
    }
    let s = o.scaled(scale);
    let (iw, ih) = img.dimensions();
    let Some((x0, y0, w, h)) = super::mask::clip_box(s.paint_bounds(), iw, ih) else { return };
    let mut local = s.clone();
    local.translate(-(x0 as f32), -(y0 as f32));
    let Some(mut pm) = Pixmap::new(w, h) else { return };
    draw_shape(&mut pm, &local);
    let mut layer = pixmap_to_rgba(&pm);
    draw_text(&mut layer, &local);
    let opacity = o.opacity.clamp(0.0, 1.0);
    if s.shadow && s.kind != Kind::Highlighter && !(s.kind == Kind::Text && s.callout == CALLOUT_PLAIN) {
        let (off, blur) = s.shadow_params();
        let mut sh = layer.clone();
        for p in sh.pixels_mut() {
            p.0 = [0, 0, 0, (p.0[3] as f32 * 0.45) as u8];
        }
        let sh = filters::gaussian_blur(&sh, blur);
        let (sx, sy) = (x0 as f32 + off, y0 as f32 + off);
        geom::composite_over(img, &sh, sx.round() as i64, sy.round() as i64, opacity);
    }
    composite(img, &layer, x0, y0, opacity, s.kind == Kind::Highlighter);
}

/// Render all objects in order.
pub fn render_all(img: &RgbaImage, objs: &[DrawObj], scale: f32) -> RgbaImage {
    let mut out = img.clone();
    for o in objs {
        render_obj(&mut out, o, scale);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]))
    }

    fn obj(kind: Kind, pts: Vec<[f32; 2]>) -> DrawObj {
        DrawObj { kind, pts, color: [0, 0, 0, 255], width: 4.0, ..Default::default() }
    }

    #[test]
    fn line_is_antialiased() {
        let img = white(60, 40);
        let out = render_all(&img, &[obj(Kind::Line, vec![[5.0, 5.0], [55.0, 30.0]])], 1.0);
        let vals: Vec<u8> = out.pixels().map(|p| p[0]).collect();
        assert!(vals.iter().any(|&v| v == 0 || v < 10), "solid core");
        assert!(vals.iter().any(|&v| v > 40 && v < 215), "partial coverage at the edge");
        assert_eq!(out.get_pixel(55, 5)[0], 255, "far from the line");
        assert!(out.pixels().all(|p| p[3] == 255));
    }

    #[test]
    fn shapes_and_fill() {
        let img = white(80, 80);
        let mut r = obj(Kind::Rect, vec![[10.0, 10.0], [70.0, 70.0]]);
        let out = render_all(&img, &[r.clone()], 1.0);
        assert_eq!(out.get_pixel(40, 40)[0], 255, "unfilled interior");
        assert_eq!(out.get_pixel(10, 40)[0], 0, "border");
        r.fill = Some([0, 0, 255, 255]);
        let out = render_all(&img, &[r], 1.0);
        assert_eq!(out.get_pixel(40, 40).0, [0, 0, 255, 255]);
        let e = obj(Kind::Ellipse, vec![[10.0, 10.0], [70.0, 50.0]]);
        let out = render_all(&img, &[e], 1.0);
        assert_eq!(out.get_pixel(10, 30)[0], 0);
        assert_eq!(out.get_pixel(12, 12)[0], 255, "corner outside the ellipse");
        let rr = obj(Kind::RoundRect, vec![[10.0, 10.0], [70.0, 70.0]]);
        let out = render_all(&img, &[rr], 1.0);
        assert_eq!(out.get_pixel(10, 11)[0], 255, "rounded corner");
        assert_eq!(out.get_pixel(10, 40)[0], 0);
    }

    #[test]
    fn arrow_head_and_scale() {
        let img = white(100, 50);
        let a = obj(Kind::Arrow, vec![[10.0, 25.0], [90.0, 25.0]]);
        let out = render_all(&img, &[a.clone()], 1.0);
        // The head is wider than the shaft.
        assert!(out.get_pixel(79, 29)[0] < 128);
        assert_eq!(out.get_pixel(40, 29)[0], 255);
        // Preview at half scale lands at half coordinates.
        let small = render_all(&white(50, 25), &[a], 0.5);
        assert!(small.get_pixel(25, 12)[0] < 60);
    }

    #[test]
    fn highlighter_multiplies() {
        let mut img = white(40, 20);
        for x in 0..20 {
            for y in 0..20 {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        let mut h = obj(Kind::Highlighter, vec![[2.0, 10.0], [38.0, 10.0]]);
        h.color = [255, 230, 0, 255];
        h.width = 8.0;
        let out = render_all(&img, &[h], 1.0);
        assert_eq!(out.get_pixel(10, 10).0, [0, 0, 0, 255], "black stays black under a highlighter");
        assert_eq!(out.get_pixel(30, 10)[2], 0);
        assert_eq!(out.get_pixel(30, 10)[0], 255);
    }

    #[test]
    fn text_number_pen_and_opacity() {
        let img = white(200, 120);
        let mut t = DrawObj { kind: Kind::Text, pts: vec![[20.0, 20.0]], text: "Hi there".into(), font_px: 24.0, ..Default::default() };
        t.fit_text_box();
        assert_eq!(t.pts.len(), 3);
        assert!(t.pts[1][0] > 60.0);
        let n = DrawObj { kind: Kind::Number, pts: vec![[150.0, 80.0]], font_px: 30.0, number: 7, text_color: [255, 255, 255, 255], ..Default::default() };
        let pen = DrawObj { kind: Kind::Pen, pts: vec![[10.0, 100.0], [30.0, 110.0], [50.0, 100.0], [70.0, 112.0]], opacity: 0.5, color: [0, 0, 0, 255], ..Default::default() };
        let out = render_all(&img, &[t.clone(), n, pen], 1.0);
        assert!(out.get_pixel(150, 66)[0] < 255 || out.get_pixel(150, 66)[1] < 255, "number disc");
        let mid = (t.pts[0][0] + t.pts[1][0]) / 2.0;
        let row: Vec<u8> = (t.pts[0][1] as u32..t.pts[1][1] as u32).map(|y| out.get_pixel(mid as u32, y)[0]).collect();
        assert!(row.iter().any(|&v| v < 128), "text drawn inside the box");
        // 50% opacity black pen: never darker than ~mid grey.
        let darkest = (95..115).flat_map(|y| (10..70).map(move |x| (x, y))).map(|(x, y)| out.get_pixel(x, y)[0]).min().unwrap();
        assert!((110..=150).contains(&darkest), "{darkest}");
    }

    #[test]
    fn hit_testing() {
        let l = obj(Kind::Line, vec![[0.0, 0.0], [100.0, 0.0]]);
        assert!(l.hit([50.0, 3.0], 2.0));
        assert!(!l.hit([50.0, 10.0], 2.0));
        let r = obj(Kind::Rect, vec![[0.0, 0.0], [100.0, 100.0]]);
        assert!(r.hit([0.0, 50.0], 2.0));
        assert!(!r.hit([50.0, 50.0], 2.0));
        let e = DrawObj { fill: Some([1, 2, 3, 255]), ..obj(Kind::Ellipse, vec![[0.0, 0.0], [100.0, 50.0]]) };
        assert!(e.hit([50.0, 25.0], 1.0));
        assert!(!e.hit([2.0, 2.0], 1.0));
        let n = DrawObj { kind: Kind::Number, pts: vec![[10.0, 10.0]], font_px: 20.0, ..Default::default() };
        assert!(n.hit([15.0, 15.0], 0.0));
    }

    /// Visual check: `LUMEN_DUMP_DIR=<dir> cargo test dump_board -- --ignored`.
    #[test]
    #[ignore]
    fn dump_board() {
        let Ok(dir) = std::env::var("LUMEN_DUMP_DIR") else { return };
        let img = RgbaImage::from_fn(900, 600, |x, y| image::Rgba([(120 + x / 8) as u8, (140 + y / 8) as u8, 170, 255]));
        let mut t = DrawObj { kind: Kind::Text, pts: vec![[480.0, 60.0]], text: "Look here!\nSecond line".into(), font_px: 30.0, width: 2.0, color: [40, 40, 40, 255], fill: Some([255, 255, 255, 255]), shadow: true, ..Default::default() };
        t.fit_text_box();
        t.pts[2] = [420.0, 260.0];
        let objs = vec![
            DrawObj { kind: Kind::Arrow, pts: vec![[60.0, 60.0], [300.0, 200.0]], width: 7.0, shadow: true, ..Default::default() },
            DrawObj { kind: Kind::Arrow, pts: vec![[60.0, 300.0], [300.0, 300.0]], width: 4.0, both_heads: true, dashed: true, color: [30, 120, 230, 255], ..Default::default() },
            DrawObj { kind: Kind::Rect, pts: vec![[340.0, 320.0], [560.0, 440.0]], width: 5.0, ..Default::default() },
            DrawObj { kind: Kind::RoundRect, pts: vec![[600.0, 320.0], [860.0, 440.0]], width: 4.0, color: [40, 170, 70, 255], fill: Some([40, 170, 70, 70]), ..Default::default() },
            DrawObj { kind: Kind::Ellipse, pts: vec![[60.0, 400.0], [300.0, 560.0]], width: 6.0, color: [140, 70, 210, 255], ..Default::default() },
            DrawObj { kind: Kind::Highlighter, pts: vec![[340.0, 500.0], [600.0, 505.0], [860.0, 498.0]], width: 28.0, color: [255, 230, 0, 255], ..Default::default() },
            DrawObj { kind: Kind::Pen, pts: (0..40).map(|i| [600.0 + i as f32 * 6.0, 200.0 + (i as f32 * 0.5).sin() * 30.0]).collect(), width: 4.0, color: [20, 20, 20, 255], ..Default::default() },
            DrawObj { kind: Kind::Number, pts: vec![[380.0, 140.0]], font_px: 44.0, number: 1, text_color: [255, 255, 255, 255], shadow: true, ..Default::default() },
            DrawObj { kind: Kind::Number, pts: vec![[440.0, 140.0]], font_px: 44.0, number: 12, text_color: [255, 255, 255, 255], color: [30, 120, 230, 255], ..Default::default() },
            DrawObj { kind: Kind::Line, pts: vec![[600.0, 560.0], [860.0, 580.0]], width: 3.0, ..Default::default() },
            t,
        ];
        let out = render_all(&img, &objs, 1.0);
        out.save(std::path::Path::new(&dir).join("draw-board.png")).unwrap();
        let small = render_all(&crate::imaging::resize_to_fit(&img, 300, 200), &objs, 1.0 / 3.0);
        small.save(std::path::Path::new(&dir).join("draw-board-small.png")).unwrap();
    }

    #[test]
    fn degenerate_objects_are_safe() {
        let img = white(10, 10);
        let objs = vec![
            obj(Kind::Line, vec![[5.0, 5.0], [5.0, 5.0]]),
            obj(Kind::Arrow, vec![[f32::NAN, 0.0], [3.0, 3.0]]),
            obj(Kind::Rect, vec![[-500.0, -500.0], [-400.0, -400.0]]),
            obj(Kind::Pen, vec![[5.0, 5.0]]),
            obj(Kind::Text, vec![[1.0, 1.0]]),
            obj(Kind::Number, vec![]),
            DrawObj { width: 0.0, ..obj(Kind::Ellipse, vec![[0.0, 0.0], [9.0, 9.0]]) },
            DrawObj { shadow: true, ..obj(Kind::Arrow, vec![[0.0, 0.0], [9.0, 9.0]]) },
        ];
        let out = render_all(&img, &objs, 1.0);
        assert_eq!(out.dimensions(), (10, 10));
        let _ = render_all(&img, &objs, 0.0);
        let _ = render_all(&RgbaImage::new(1, 1), &objs, 3.0);
        assert!(bubble_path([0.0, 0.0, 0.5, 0.5], 3.0, Some([10.0, 10.0]), 2.0).is_none());
        assert!(bubble_path([0.0, 0.0, 50.0, 30.0], 6.0, Some([25.0, 80.0]), 6.0).is_some());
    }
}
