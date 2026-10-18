//! Bounded coverage masks for local edits (brush strokes, selections, regions).
//!
//! A [`Mask`] covers only the bounding box of what it describes, so a small
//! brush stroke on a 50 MP photo costs a few kilobytes, not 50 MB. Coverage is
//! stored as u8 (0..255); everything outside the box is 0.

use image::RgbaImage;
use rayon::prelude::*;

#[derive(Clone, Debug, Default)]
pub struct Mask {
    /// Top-left of the box in image pixels.
    pub x0: u32,
    pub y0: u32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn new(x0: u32, y0: u32, w: u32, h: u32) -> Mask {
        Mask { x0, y0, w, h, data: vec![0; w as usize * h as usize] }
    }
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0 || self.data.iter().all(|&v| v == 0)
    }
    /// Coverage 0..1 at an image pixel (0 outside the box).
    #[inline]
    pub fn at(&self, x: u32, y: u32) -> f32 {
        if x < self.x0 || y < self.y0 || x >= self.x0 + self.w || y >= self.y0 + self.h {
            return 0.0;
        }
        self.data[(y - self.y0) as usize * self.w as usize + (x - self.x0) as usize] as f32 / 255.0
    }
    /// Box as (x0, y0, x1, y1), exclusive end.
    pub fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.x0, self.y0, self.x0 + self.w, self.y0 + self.h)
    }
}

/// Clip a float box (x0, y0, x1, y1) to the image, rounding outwards.
/// Returns None when it does not intersect.
pub fn clip_box(b: [f32; 4], iw: u32, ih: u32) -> Option<(u32, u32, u32, u32)> {
    if !b.iter().all(|v| v.is_finite()) {
        return None;
    }
    let x0 = b[0].min(b[2]).floor().max(0.0);
    let y0 = b[1].min(b[3]).floor().max(0.0);
    let x1 = b[0].max(b[2]).ceil().min(iw as f32);
    let y1 = b[1].max(b[3]).ceil().min(ih as f32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0 as u32, y0 as u32, x1 as u32 - x0 as u32, y1 as u32 - y0 as u32))
}

/// Brush falloff: 1 inside `radius * hardness`, smooth to 0 at `radius`
/// (at least a one-pixel ramp so hard brushes are still anti-aliased).
#[inline]
pub fn falloff(d: f32, radius: f32, hardness: f32) -> f32 {
    let inner = (radius * hardness.clamp(0.0, 1.0)).min(radius - 1.0).max(0.0);
    if d <= inner {
        1.0
    } else if d >= radius {
        0.0
    } else {
        let t = (d - inner) / (radius - inner).max(1e-3);
        1.0 - t * t * (3.0 - 2.0 * t)
    }
}

#[inline]
fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let (wx, wy) = (p[0] - a[0], p[1] - a[1]);
    let l2 = vx * vx + vy * vy;
    let t = if l2 > 1e-9 { ((wx * vx + wy * vy) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (wx - t * vx, wy - t * vy);
    (dx * dx + dy * dy).sqrt()
}

/// Drop points closer than `min_d` to the previously kept one (keeps the last).
pub fn simplify(pts: &[[f32; 2]], min_d: f32) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
    for &p in pts {
        if !p[0].is_finite() || !p[1].is_finite() {
            continue;
        }
        match out.last() {
            Some(l) if ((p[0] - l[0]).powi(2) + (p[1] - l[1]).powi(2)).sqrt() < min_d => {}
            _ => out.push(p),
        }
    }
    if let (Some(&last), Some(&kept)) = (pts.last(), out.last()) {
        if last != kept && last[0].is_finite() && last[1].is_finite() {
            out.push(last);
        }
    }
    out
}

/// Coverage of a round brush dragged along `pts` (image pixels), clipped to
/// the image. Each segment is stamped as an exact capsule (max-combined), so
/// there is no dab spacing pattern.
pub fn stroke_mask(pts: &[[f32; 2]], radius: f32, hardness: f32, iw: u32, ih: u32) -> Mask {
    let radius = radius.clamp(0.5, 20_000.0);
    let pts = simplify(pts, (radius * 0.08).max(0.5));
    let Some(first) = pts.first() else { return Mask::default() };
    let (mut bx0, mut by0, mut bx1, mut by1) = (first[0], first[1], first[0], first[1]);
    for p in &pts {
        bx0 = bx0.min(p[0]);
        by0 = by0.min(p[1]);
        bx1 = bx1.max(p[0]);
        by1 = by1.max(p[1]);
    }
    let Some((x0, y0, w, h)) = clip_box([bx0 - radius - 1.0, by0 - radius - 1.0, bx1 + radius + 1.0, by1 + radius + 1.0], iw, ih) else {
        return Mask::default();
    };
    let mut m = Mask::new(x0, y0, w, h);
    let segs: Vec<([f32; 2], [f32; 2])> = if pts.len() == 1 { vec![(pts[0], pts[0])] } else { pts.windows(2).map(|s| (s[0], s[1])).collect() };
    let wu = w as usize;
    m.data.par_chunks_mut(wu).enumerate().for_each(|(row, line)| {
        let py = (y0 as usize + row) as f32 + 0.5;
        for (a, b) in &segs {
            if py < a[1].min(b[1]) - radius - 1.0 || py > a[1].max(b[1]) + radius + 1.0 {
                continue;
            }
            let lx = ((a[0].min(b[0]) - radius - 1.0).floor() - x0 as f32).max(0.0) as usize;
            let rx = (((a[0].max(b[0]) + radius + 1.0).ceil() - x0 as f32).max(0.0) as usize).min(wu);
            for (i, v) in line.iter_mut().enumerate().take(rx).skip(lx) {
                let px = (x0 as usize + i) as f32 + 0.5;
                let c = falloff(dist_to_segment([px, py], *a, *b), radius, hardness);
                let c8 = (c * 255.0 + 0.5) as u8;
                if c8 > *v {
                    *v = c8;
                }
            }
        }
    });
    m
}

/// A selection / region shape in image pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// x0, y0, x1, y1
    Rect([f32; 4]),
    Ellipse([f32; 4]),
    /// Closed polygon (lasso).
    Polygon(Vec<[f32; 2]>),
}

impl Shape {
    pub fn bbox(&self) -> [f32; 4] {
        match self {
            Shape::Rect(r) | Shape::Ellipse(r) => [r[0].min(r[2]), r[1].min(r[3]), r[0].max(r[2]), r[1].max(r[3])],
            Shape::Polygon(p) => {
                let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                for q in p {
                    b = [b[0].min(q[0]), b[1].min(q[1]), b[2].max(q[0]), b[3].max(q[1])];
                }
                if p.is_empty() {
                    [0.0; 4]
                } else {
                    b
                }
            }
        }
    }
    pub fn scaled(&self, s: f32) -> Shape {
        match self {
            Shape::Rect(r) => Shape::Rect(r.map(|v| v * s)),
            Shape::Ellipse(r) => Shape::Ellipse(r.map(|v| v * s)),
            Shape::Polygon(p) => Shape::Polygon(p.iter().map(|q| [q[0] * s, q[1] * s]).collect()),
        }
    }
    pub fn translated(&self, dx: f32, dy: f32) -> Shape {
        match self {
            Shape::Rect(r) => Shape::Rect([r[0] + dx, r[1] + dy, r[2] + dx, r[3] + dy]),
            Shape::Ellipse(r) => Shape::Ellipse([r[0] + dx, r[1] + dy, r[2] + dx, r[3] + dy]),
            Shape::Polygon(p) => Shape::Polygon(p.iter().map(|q| [q[0] + dx, q[1] + dy]).collect()),
        }
    }
    /// Point-in-shape test (image pixels).
    pub fn contains(&self, p: [f32; 2]) -> bool {
        match self {
            Shape::Rect(_) => {
                let b = self.bbox();
                p[0] >= b[0] && p[0] <= b[2] && p[1] >= b[1] && p[1] <= b[3]
            }
            Shape::Ellipse(_) => {
                let b = self.bbox();
                let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
                let (rx, ry) = (((b[2] - b[0]) / 2.0).max(1e-3), ((b[3] - b[1]) / 2.0).max(1e-3));
                ((p[0] - cx) / rx).powi(2) + ((p[1] - cy) / ry).powi(2) <= 1.0
            }
            Shape::Polygon(q) => {
                let mut inside = false;
                let n = q.len();
                for i in 0..n {
                    let (a, b) = (q[i], q[(i + 1) % n]);
                    if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
                        inside = !inside;
                    }
                }
                inside
            }
        }
    }
    /// Whether the shape is big enough to matter.
    pub fn is_valid(&self) -> bool {
        let b = self.bbox();
        let ok = b.iter().all(|v| v.is_finite()) && b[2] - b[0] >= 1.0 && b[3] - b[1] >= 1.0;
        match self {
            Shape::Polygon(p) => ok && p.len() >= 3,
            _ => ok,
        }
    }
    pub fn path(&self) -> Option<tiny_skia::Path> {
        let b = self.bbox();
        match self {
            Shape::Rect(_) => tiny_skia::Rect::from_ltrb(b[0], b[1], b[2], b[3]).map(tiny_skia::PathBuilder::from_rect),
            Shape::Ellipse(_) => tiny_skia::Rect::from_ltrb(b[0], b[1], b[2], b[3]).and_then(tiny_skia::PathBuilder::from_oval),
            Shape::Polygon(p) => {
                let mut pb = tiny_skia::PathBuilder::new();
                let mut it = p.iter();
                let f = it.next()?;
                pb.move_to(f[0], f[1]);
                for q in it {
                    pb.line_to(q[0], q[1]);
                }
                pb.close();
                pb.finish()
            }
        }
    }
}

/// Anti-aliased union of `shapes`, feathered by a Gaussian of `feather` px,
/// clipped to the image. The box is grown so the feather has room.
pub fn shapes_mask(shapes: &[Shape], feather: f32, iw: u32, ih: u32) -> Mask {
    let feather = if feather.is_finite() { feather.clamp(0.0, 2000.0) } else { 0.0 };
    let valid: Vec<&Shape> = shapes.iter().filter(|s| s.is_valid()).collect();
    if valid.is_empty() || iw == 0 || ih == 0 {
        return Mask::default();
    }
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for s in &valid {
        let sb = s.bbox();
        b = [b[0].min(sb[0]), b[1].min(sb[1]), b[2].max(sb[2]), b[3].max(sb[3])];
    }
    let grow = feather * 2.5 + 2.0;
    let Some((x0, y0, w, h)) = clip_box([b[0] - grow, b[1] - grow, b[2] + grow, b[3] + grow], iw, ih) else {
        return Mask::default();
    };
    let Some(mut tm) = tiny_skia::Mask::new(w, h) else { return Mask::default() };
    let tr = tiny_skia::Transform::from_translate(-(x0 as f32), -(y0 as f32));
    for s in valid {
        if let Some(p) = s.path() {
            tm.fill_path(&p, tiny_skia::FillRule::Winding, true, tr);
        }
    }
    let mut m = Mask { x0, y0, w, h, data: tm.data().to_vec() };
    if feather >= 0.3 {
        blur_plane(&mut m.data, w as usize, h as usize, feather);
    }
    m
}

/// Something a separable box blur can run on.
pub trait PlaneVal: Copy + Send + Sync + Default {
    fn to(self) -> f64;
    fn from(v: f64) -> Self;
}
impl PlaneVal for u8 {
    #[inline]
    fn to(self) -> f64 {
        self as f64
    }
    #[inline]
    fn from(v: f64) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
}
impl PlaneVal for f32 {
    #[inline]
    fn to(self) -> f64 {
        self as f64
    }
    #[inline]
    fn from(v: f64) -> Self {
        v as f32
    }
}

/// Box radius whose 3-pass result approximates a Gaussian of `sigma`.
pub fn box_radius_for(sigma: f32) -> usize {
    // Three passes of radius r have variance r(r+1).
    (((1.0 + 4.0 * sigma.max(0.0) * sigma.max(0.0)).sqrt() - 1.0) / 2.0).round().max(1.0) as usize
}

/// Approximate Gaussian blur (3 box passes) of a single-channel plane with
/// edge clamping. O(pixels) regardless of the radius.
pub fn blur_plane<T: PlaneVal>(data: &mut [T], w: usize, h: usize, sigma: f32) {
    if w == 0 || h == 0 || data.len() != w * h || !(sigma > 0.0) {
        return;
    }
    let r = box_radius_for(sigma).min(w.max(h));
    for _ in 0..3 {
        box_pass_h(data, w, r);
        box_pass_v(data, w, h, r);
    }
}

fn box_pass_h<T: PlaneVal>(data: &mut [T], w: usize, r: usize) {
    let d = (2 * r + 1) as f64;
    data.par_chunks_mut(w).for_each_init(
        || vec![T::default(); w],
        |tmp, row| {
            tmp.copy_from_slice(row);
            let at = |i: i64| tmp[i.clamp(0, w as i64 - 1) as usize].to();
            let ri = r as i64;
            let mut s: f64 = (-ri..=ri).map(at).sum();
            for x in 0..w as i64 {
                row[x as usize] = T::from(s / d);
                s += at(x + ri + 1) - at(x - ri);
            }
        },
    );
}

fn box_pass_v<T: PlaneVal>(data: &mut [T], w: usize, h: usize, r: usize) {
    // Column strips processed in parallel; each strip slides a window of row sums.
    let src = data.to_vec();
    let strip = 64usize;
    let d = (2 * r + 1) as f64;
    let ri = r as i64;
    let cols: Vec<usize> = (0..w).step_by(strip).collect();
    let results: Vec<(usize, Vec<T>)> = cols
        .par_iter()
        .map(|&c0| {
            let cw = strip.min(w - c0);
            let at = |x: usize, y: i64| src[y.clamp(0, h as i64 - 1) as usize * w + c0 + x].to();
            let mut sums: Vec<f64> = (0..cw).map(|x| (-ri..=ri).map(|y| at(x, y)).sum()).collect();
            let mut out = vec![T::default(); cw * h];
            for y in 0..h as i64 {
                for x in 0..cw {
                    out[y as usize * cw + x] = T::from(sums[x] / d);
                    sums[x] += at(x, y + ri + 1) - at(x, y - ri);
                }
            }
            (c0, out)
        })
        .collect();
    for (c0, out) in results {
        let cw = strip.min(w - c0);
        for y in 0..h {
            data[y * w + c0..y * w + c0 + cw].copy_from_slice(&out[y * cw..(y + 1) * cw]);
        }
    }
}

/// Blend `fx` into `img` by mask coverage (`invert` uses 1 - coverage
/// everywhere, including outside the mask box). `fx` must be image-sized, or
/// cover exactly the box `fx_origin` .. `fx_origin + fx.size`.
pub fn blend_masked(img: &mut RgbaImage, fx: &RgbaImage, fx_origin: (u32, u32), m: &Mask, invert: bool, opacity: f32) {
    let opacity = opacity.clamp(0.0, 1.0);
    let (iw, ih) = img.dimensions();
    let (fw, fh) = fx.dimensions();
    let (ox, oy) = fx_origin;
    let fraw = fx.as_raw();
    let stride = iw as usize * 4;
    img.as_mut().par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let y = y as u32;
        if y >= ih || y < oy || y >= oy + fh {
            return;
        }
        let (xa, xb) = if invert { (ox, (ox + fw).min(iw)) } else { (m.x0.max(ox), (m.x0 + m.w).min(ox + fw).min(iw)) };
        if !invert && (y < m.y0 || y >= m.y0 + m.h) {
            return;
        }
        for x in xa..xb {
            let mut c = m.at(x, y);
            if invert {
                c = 1.0 - c;
            }
            let t = c * opacity;
            if t <= 0.0 {
                continue;
            }
            let si = ((y - oy) as usize * fw as usize + (x - ox) as usize) * 4;
            let d = &mut row[x as usize * 4..x as usize * 4 + 4];
            for k in 0..4 {
                d[k] = (d[k] as f32 + (fraw[si + k] as f32 - d[k] as f32) * t).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falloff_shape() {
        assert_eq!(falloff(0.0, 10.0, 0.5), 1.0);
        assert_eq!(falloff(10.0, 10.0, 0.5), 0.0);
        let mid = falloff(7.5, 10.0, 0.5);
        assert!(mid > 0.2 && mid < 0.8);
        // Hard brush still has a 1 px anti-aliased edge.
        assert!(falloff(9.5, 10.0, 1.0) > 0.0 && falloff(9.5, 10.0, 1.0) < 1.0);
        // Tiny radius does not divide by zero.
        assert!(falloff(0.2, 0.5, 1.0).is_finite());
    }

    #[test]
    fn stroke_mask_capsule() {
        let m = stroke_mask(&[[10.0, 10.0], [40.0, 10.0]], 5.0, 0.8, 64, 32);
        assert!(m.w > 30 && m.h >= 10);
        assert_eq!(m.at(25, 10), 1.0);
        assert_eq!(m.at(25, 25), 0.0);
        assert!(m.at(25, 14) > 0.0 && m.at(25, 14) < 1.0);
        // Off-image and degenerate strokes are empty, never panic.
        assert!(stroke_mask(&[[-100.0, -100.0]], 5.0, 0.5, 64, 32).is_empty());
        assert!(stroke_mask(&[], 5.0, 0.5, 64, 32).is_empty());
        assert!(stroke_mask(&[[f32::NAN, 1.0]], 5.0, 0.5, 64, 32).is_empty());
        let single = stroke_mask(&[[0.0, 0.0]], 3.0, 1.0, 1, 1);
        assert_eq!(single.at(0, 0), 1.0);
    }

    #[test]
    fn shapes_mask_aa_and_feather() {
        let m = shapes_mask(&[Shape::Rect([10.0, 10.0, 30.5, 30.0])], 0.0, 64, 64);
        assert_eq!(m.at(20, 20), 1.0);
        assert_eq!(m.at(5, 5), 0.0);
        let edge = m.at(30, 20);
        assert!(edge > 0.3 && edge < 0.7, "anti-aliased half pixel {edge}");
        let f = shapes_mask(&[Shape::Ellipse([10.0, 10.0, 50.0, 50.0])], 4.0, 64, 64);
        assert!(f.at(30, 30) > 0.99);
        assert!(f.at(30, 10) > 0.2 && f.at(30, 10) < 0.8);
        let lasso = shapes_mask(&[Shape::Polygon(vec![[0.0, 0.0], [60.0, 0.0], [0.0, 60.0]])], 0.0, 64, 64);
        assert_eq!(lasso.at(5, 5), 1.0);
        assert_eq!(lasso.at(55, 55), 0.0);
        assert!(shapes_mask(&[Shape::Polygon(vec![[1.0, 1.0]])], 2.0, 64, 64).is_empty());
        assert!(shapes_mask(&[Shape::Rect([100.0, 100.0, 200.0, 200.0])], 2.0, 64, 64).is_empty());
    }

    #[test]
    fn blur_plane_preserves_flat_and_mass() {
        let mut flat = vec![77u8; 30 * 20];
        blur_plane(&mut flat, 30, 20, 5.0);
        assert!(flat.iter().all(|&v| v == 77));
        let mut spot = vec![0f32; 41 * 41];
        spot[20 * 41 + 20] = 1000.0;
        blur_plane(&mut spot, 41, 41, 3.0);
        let sum: f32 = spot.iter().sum();
        assert!((sum - 1000.0).abs() < 1.0, "{sum}");
        assert!(spot[20 * 41 + 20] < 100.0 && spot[20 * 41 + 23] > 1.0);
    }

    #[test]
    fn polygon_contains() {
        let s = Shape::Polygon(vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        assert!(s.contains([5.0, 5.0]));
        assert!(!s.contains([15.0, 5.0]));
        assert!(Shape::Ellipse([0.0, 0.0, 10.0, 20.0]).contains([5.0, 10.0]));
        assert!(!Shape::Ellipse([0.0, 0.0, 10.0, 20.0]).contains([0.5, 0.5]));
    }

    #[test]
    fn blend_masked_respects_coverage() {
        let mut img = RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 0, 255]));
        let fx = RgbaImage::from_pixel(20, 20, image::Rgba([200, 200, 200, 255]));
        let m = shapes_mask(&[Shape::Rect([5.0, 5.0, 10.0, 10.0])], 0.0, 20, 20);
        let mut inv = img.clone();
        blend_masked(&mut img, &fx, (0, 0), &m, false, 1.0);
        assert_eq!(img.get_pixel(7, 7)[0], 200);
        assert_eq!(img.get_pixel(15, 15)[0], 0);
        blend_masked(&mut inv, &fx, (0, 0), &m, true, 0.5);
        assert_eq!(inv.get_pixel(7, 7)[0], 0);
        assert_eq!(inv.get_pixel(15, 15)[0], 100);
    }
}
