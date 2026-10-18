//! Geometry: canvas resizing, free rotation, borders/frames and layer
//! compositing (straight-alpha "source over").

use super::filters;
use image::{Rgba, RgbaImage};
use rayon::prelude::*;

/// Largest sensible output dimension; keeps a typo from allocating 100 GB.
pub const MAX_DIM: u32 = 40_000;

pub fn clamp_dim(v: f64) -> u32 {
    if v.is_finite() {
        v.round().clamp(1.0, MAX_DIM as f64) as u32
    } else {
        1
    }
}

/// Anchor fractions for a 3×3 grid index (0 = top-left … 8 = bottom-right).
pub fn anchor_frac(anchor: u8) -> (f32, f32) {
    let a = anchor.min(8);
    ((a % 3) as f32 * 0.5, (a / 3) as f32 * 0.5)
}

/// Place `img` on a `w`×`h` canvas filled with `fill`, positioned by `anchor`.
/// Shrinking crops the image.
pub fn canvas(img: &RgbaImage, w: u32, h: u32, anchor: u8, fill: [u8; 4]) -> RgbaImage {
    let (w, h) = (w.clamp(1, MAX_DIM), h.clamp(1, MAX_DIM));
    let (ax, ay) = anchor_frac(anchor);
    let ox = ((w as f32 - img.width() as f32) * ax).round() as i64;
    let oy = ((h as f32 - img.height() as f32) * ay).round() as i64;
    canvas_at(img, w, h, ox, oy, fill)
}

/// Place `img` at (`ox`,`oy`) on a `w`×`h` canvas filled with `fill`.
pub fn canvas_at(img: &RgbaImage, w: u32, h: u32, ox: i64, oy: i64, fill: [u8; 4]) -> RgbaImage {
    let (w, h) = (w.clamp(1, MAX_DIM), h.clamp(1, MAX_DIM));
    if (w, h) == img.dimensions() && ox == 0 && oy == 0 {
        return img.clone();
    }
    let mut out = RgbaImage::from_pixel(w, h, Rgba(fill));
    blit(&mut out, img, ox, oy);
    out
}

/// Copy `src` into `dst` at (`ox`,`oy`) (replacing pixels), clipped.
pub fn blit(dst: &mut RgbaImage, src: &RgbaImage, ox: i64, oy: i64) {
    let (dw, dh) = (dst.width() as i64, dst.height() as i64);
    let (sw, sh) = (src.width() as i64, src.height() as i64);
    let x0 = ox.max(0);
    let x1 = (ox + sw).min(dw);
    if x1 <= x0 {
        return;
    }
    let n = ((x1 - x0) * 4) as usize;
    let stride = dw as usize * 4;
    let sstride = sw as usize * 4;
    let sraw = src.as_raw();
    dst.as_mut().par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let sy = y as i64 - oy;
        if sy < 0 || sy >= sh || (y as i64) >= dh {
            return;
        }
        let s = sy as usize * sstride + ((x0 - ox) * 4) as usize;
        row[x0 as usize * 4..x0 as usize * 4 + n].copy_from_slice(&sraw[s..s + n]);
    });
}

#[inline]
pub fn blend_px(d: &mut [u8], s: [f32; 4]) {
    // s = straight colour 0..255, alpha 0..1
    let sa = s[3];
    if sa <= 0.0 {
        return;
    }
    let da = d[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return;
    }
    for c in 0..3 {
        let v = (s[c] * sa + d[c] as f32 * da * (1.0 - sa)) / oa;
        d[c] = (v + 0.5).clamp(0.0, 255.0) as u8;
    }
    d[3] = (oa * 255.0 + 0.5).min(255.0) as u8;
}

/// Composite `src` over `dst` at (`ox`,`oy`) with extra `opacity` (0..1).
pub fn composite_over(dst: &mut RgbaImage, src: &RgbaImage, ox: i64, oy: i64, opacity: f32) {
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    let (dw, dh) = (dst.width() as i64, dst.height() as i64);
    let (sw, sh) = (src.width() as i64, src.height() as i64);
    let x0 = ox.max(0);
    let x1 = (ox + sw).min(dw);
    if x1 <= x0 || oy >= dh || oy + sh <= 0 {
        return;
    }
    let stride = dw as usize * 4;
    let sstride = sw as usize * 4;
    let sraw = src.as_raw();
    dst.as_mut().par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let sy = y as i64 - oy;
        if sy < 0 || sy >= sh {
            return;
        }
        for x in x0..x1 {
            let si = sy as usize * sstride + ((x - ox) * 4) as usize;
            let a = sraw[si + 3] as f32 / 255.0 * opacity;
            if a > 0.0 {
                blend_px(&mut row[x as usize * 4..x as usize * 4 + 4], [sraw[si] as f32, sraw[si + 1] as f32, sraw[si + 2] as f32, a]);
            }
        }
    });
}

// ---------------------------------------------------------------- rotation

/// Size of the largest axis-aligned rectangle inside a `w`×`h` rectangle rotated
/// by `angle` radians.
pub fn inscribed_rect(w: f64, h: f64, angle: f64) -> (f64, f64) {
    if w <= 0.0 || h <= 0.0 {
        return (0.0, 0.0);
    }
    let wide = w >= h;
    let (long, short) = if wide { (w, h) } else { (h, w) };
    let (s, c) = (angle.sin().abs(), angle.cos().abs());
    if short <= 2.0 * s * c * long || (s - c).abs() < 1e-10 {
        let x = 0.5 * short;
        if wide {
            (x / s.max(1e-12), x / c.max(1e-12))
        } else {
            (x / c.max(1e-12), x / s.max(1e-12))
        }
    } else {
        let cos2 = c * c - s * s;
        ((w * c - h * s) / cos2, (h * c - w * s) / cos2)
    }
}

/// Output size for a free rotation.
pub fn rotated_size(w: u32, h: u32, angle_deg: f32, crop: bool) -> (u32, u32) {
    let a = (angle_deg as f64).to_radians();
    let (w, h) = (w as f64, h as f64);
    if crop {
        let (cw, ch) = inscribed_rect(w, h, a);
        (clamp_dim(cw.min(w).floor()), clamp_dim(ch.min(h).floor()))
    } else {
        let (s, c) = (a.sin().abs(), a.cos().abs());
        // Tiny epsilon so 90° multiples don't round up by a pixel.
        (clamp_dim((w * c + h * s - 1e-6).ceil()), clamp_dim((w * s + h * c - 1e-6).ceil()))
    }
}

/// Rotate by `angle_deg` (positive = clockwise) with bilinear sampling, around
/// the image centre, into an `ow`×`oh` canvas; uncovered areas get `bg`.
pub fn rotate(img: &RgbaImage, angle_deg: f32, crop: bool, bg: [u8; 4]) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || !angle_deg.is_finite() || angle_deg.rem_euclid(360.0).abs() < 1e-4 {
        return img.clone();
    }
    let (ow, oh) = rotated_size(w, h, angle_deg, crop);
    let a = (angle_deg as f64).to_radians();
    let (s, c) = (a.sin() as f32, a.cos() as f32);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let (ocx, ocy) = (ow as f32 / 2.0, oh as f32 / 2.0);
    let raw = img.as_raw();
    let (wi, hi) = (w as i64, h as i64);
    let bga = bg[3] as f32 / 255.0;
    let bgp = [bg[0] as f32 * bga, bg[1] as f32 * bga, bg[2] as f32 * bga, bga];
    let mut out = RgbaImage::new(ow, oh);
    out.as_mut().par_chunks_mut(ow as usize * 4).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - ocy;
        for x in 0..ow as usize {
            let dx = x as f32 + 0.5 - ocx;
            // Inverse rotation (dest -> source).
            let mut sx = c * dx + s * dy + cx - 0.5;
            let mut sy = -s * dx + c * dy + cy - 0.5;
            // Points inside the source rectangle (including the outer half of
            // edge pixels) sample edge-clamped; only outside points fade to bg.
            let (wm, hm) = (w as f32 - 1.0, h as f32 - 1.0);
            if (-0.5..0.0).contains(&sx) {
                sx = 0.0;
            } else if sx > wm && sx <= wm + 0.5 {
                sx = wm;
            }
            if (-0.5..0.0).contains(&sy) {
                sy = 0.0;
            } else if sy > hm && sy <= hm + 0.5 {
                sy = hm;
            }
            let x0 = sx.floor();
            let y0 = sy.floor();
            let fx = sx - x0;
            let fy = sy - y0;
            let (x0, y0) = (x0 as i64, y0 as i64);
            let mut acc = [0f32; 4];
            for (xx, yy, wgt) in [(x0, y0, (1.0 - fx) * (1.0 - fy)), (x0 + 1, y0, fx * (1.0 - fy)), (x0, y0 + 1, (1.0 - fx) * fy), (x0 + 1, y0 + 1, fx * fy)] {
                if wgt <= 0.0 {
                    continue;
                }
                let px = if xx >= 0 && yy >= 0 && xx < wi && yy < hi {
                    let i = ((yy * wi + xx) * 4) as usize;
                    let al = raw[i + 3] as f32 / 255.0;
                    [raw[i] as f32 * al, raw[i + 1] as f32 * al, raw[i + 2] as f32 * al, al]
                } else {
                    bgp
                };
                for k in 0..4 {
                    acc[k] += px[k] * wgt;
                }
            }
            let o = &mut row[x * 4..x * 4 + 4];
            if acc[3] > 1e-6 {
                for k in 0..3 {
                    o[k] = (acc[k] / acc[3] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
            o[3] = (acc[3] * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        }
    });
    out
}

// ---------------------------------------------------------------- borders

/// Anti-aliased coverage of the pixel at (x,y) by a rounded rectangle.
#[inline]
pub fn rrect_coverage(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let (px, py) = (x + 0.5, y + 0.5);
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
    let cx = px.clamp(x0 + r, x1 - r);
    let cy = py.clamp(y0 + r, y1 - r);
    let d = if r > 0.0 && (px < x0 + r || px > x1 - r) && (py < y0 + r || py > y1 - r) {
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
    } else {
        (x0 - px).max(px - x1).max(y0 - py).max(py - y1)
    };
    (0.5 - d).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BorderSpec {
    /// Side border width (px, already scaled).
    pub width: f32,
    /// Bottom border width (polaroid style uses a thicker one).
    pub bottom: f32,
    pub color: [u8; 4],
    pub line_width: f32,
    pub line_color: [u8; 4],
    pub radius: f32,
    pub shadow: bool,
    pub shadow_blur: f32,
    pub shadow_offset: f32,
    pub shadow_opacity: f32,
    pub shadow_bg: [u8; 4],
}

pub fn border_size(w: u32, h: u32, b: &BorderSpec) -> (u32, u32, f32) {
    let bw = b.width.max(0.0).round();
    let bb = b.bottom.max(0.0).round();
    let fw = w as f32 + 2.0 * bw;
    let fh = h as f32 + bw + bb;
    let margin = if b.shadow { (b.shadow_blur.max(0.0) * 2.5 + b.shadow_offset.abs()).ceil() } else { 0.0 };
    (clamp_dim((fw + 2.0 * margin) as f64), clamp_dim((fh + 2.0 * margin) as f64), margin)
}

pub fn add_border(img: &RgbaImage, b: &BorderSpec) -> RgbaImage {
    let (w, h) = img.dimensions();
    let bw = b.width.max(0.0).round();
    let bb = b.bottom.max(0.0).round();
    if bw == 0.0 && bb == 0.0 && b.radius <= 0.0 && !b.shadow && b.line_width <= 0.0 {
        return img.clone();
    }
    let fw = clamp_dim((w as f32 + 2.0 * bw) as f64);
    let fh = clamp_dim((h as f32 + bw + bb) as f64);
    // Framed picture.
    let mut framed = RgbaImage::from_pixel(fw, fh, Rgba(b.color));
    blit(&mut framed, img, bw as i64, bw as i64);
    let lw = b.line_width.max(0.0);
    if lw > 0.0 {
        // Line drawn just outside the photo edge (inside it if there's no border).
        let (x0, y0, x1, y1) = if bw > 0.0 { (bw - lw, bw - lw, bw + w as f32 + lw, bw + h as f32 + lw) } else { (0.0, 0.0, fw as f32, fh as f32) };
        let (ix0, iy0, ix1, iy1) = if bw > 0.0 { (bw, bw, bw + w as f32, bw + h as f32) } else { (lw, lw, fw as f32 - lw, fh as f32 - lw) };
        let lc = b.line_color;
        framed.as_mut().par_chunks_mut(fw as usize * 4).enumerate().for_each(|(y, row)| {
            for x in 0..fw as usize {
                let outer = rrect_coverage(x as f32, y as f32, x0, y0, x1, y1, 0.0);
                let inner = rrect_coverage(x as f32, y as f32, ix0, iy0, ix1, iy1, 0.0);
                let cov = (outer - inner).clamp(0.0, 1.0) * lc[3] as f32 / 255.0;
                if cov > 0.0 {
                    blend_px(&mut row[x * 4..x * 4 + 4], [lc[0] as f32, lc[1] as f32, lc[2] as f32, cov]);
                }
            }
        });
    }
    let r = b.radius.max(0.0);
    if r > 0.0 {
        let (fwf, fhf) = (fw as f32, fh as f32);
        framed.as_mut().par_chunks_mut(fw as usize * 4).enumerate().for_each(|(y, row)| {
            let yf = y as f32;
            if yf >= r && yf + 1.0 <= fhf - r {
                return;
            }
            for x in 0..fw as usize {
                let cov = rrect_coverage(x as f32, yf, 0.0, 0.0, fwf, fhf, r);
                if cov < 1.0 {
                    row[x * 4 + 3] = (row[x * 4 + 3] as f32 * cov + 0.5) as u8;
                }
            }
        });
    }
    if !b.shadow {
        return framed;
    }
    let (ow, oh, m) = border_size(w, h, b);
    let mut out = RgbaImage::from_pixel(ow, oh, Rgba(b.shadow_bg));
    // Shadow = blurred alpha of the frame, offset down-right.
    let mut sh = RgbaImage::new(ow, oh);
    let alpha_op = b.shadow_opacity.clamp(0.0, 1.0);
    let off = b.shadow_offset;
    {
        let fraw = framed.as_raw();
        let (fwi, fhi) = (fw as i64, fh as i64);
        let (sx0, sy0) = ((m + off).round() as i64, (m + off).round() as i64);
        sh.as_mut().par_chunks_mut(ow as usize * 4).enumerate().for_each(|(y, row)| {
            let fy = y as i64 - sy0;
            if fy < 0 || fy >= fhi {
                return;
            }
            for x in 0..ow as i64 {
                let fx = x - sx0;
                if fx >= 0 && fx < fwi {
                    row[x as usize * 4 + 3] = (fraw[((fy * fwi + fx) * 4 + 3) as usize] as f32 * alpha_op) as u8;
                }
            }
        });
    }
    let sh = filters::gaussian_blur(&sh, b.shadow_blur.max(0.0));
    composite_over(&mut out, &sh, 0, 0, 1.0);
    composite_over(&mut out, &framed, m as i64, m as i64, 1.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| Rgba([(x * 13) as u8, (y * 7) as u8, ((x + y) * 3) as u8, 255]))
    }

    #[test]
    fn canvas_sizes_and_anchor() {
        let img = sample(10, 6);
        let c = canvas(&img, 20, 10, 4, [0, 0, 0, 0]);
        assert_eq!(c.dimensions(), (20, 10));
        assert_eq!(c.get_pixel(5, 2), img.get_pixel(0, 0));
        assert_eq!(c.get_pixel(0, 0)[3], 0);
        let tl = canvas(&img, 12, 8, 0, [9, 9, 9, 255]);
        assert_eq!(tl.get_pixel(0, 0), img.get_pixel(0, 0));
        assert_eq!(tl.get_pixel(11, 7), &Rgba([9, 9, 9, 255]));
        assert_eq!(canvas(&img, 10, 6, 4, [0; 4]), img);
        // Shrinking crops; zero sizes clamp.
        assert_eq!(canvas(&img, 4, 4, 8, [0; 4]).get_pixel(3, 3), img.get_pixel(9, 5));
        assert_eq!(canvas(&img, 0, 0, 4, [0; 4]).dimensions(), (1, 1));
    }

    #[test]
    fn rotate_identity_and_sizes() {
        let img = sample(31, 17);
        assert_eq!(rotate(&img, 0.0, true, [0; 4]), img);
        assert_eq!(rotate(&img, 360.0, false, [0; 4]), img);
        let r90 = rotate(&img, 90.0, false, [0; 4]);
        assert_eq!(r90.dimensions(), (17, 31));
        assert_eq!(r90, image::imageops::rotate90(&img));
        let e = rotate(&img, 10.0, false, [255, 255, 255, 255]);
        assert_eq!(e.dimensions(), rotated_size(31, 17, 10.0, false));
        assert!(e.width() > 31 && e.height() > 17);
        let c = rotate(&img, 10.0, true, [0; 4]);
        assert!(c.width() < 31 && c.height() < 17);
        assert!(c.pixels().all(|p| p[3] == 255), "auto-crop leaves no transparent border");
        let one = RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]));
        assert!(rotate(&one, 33.0, true, [0; 4]).width() >= 1);
    }

    #[test]
    fn inscribed_rect_square() {
        let (w, h) = inscribed_rect(100.0, 100.0, 45f64.to_radians());
        assert!((w - 70.71).abs() < 0.1 && (h - 70.71).abs() < 0.1);
        let (w, h) = inscribed_rect(100.0, 50.0, 0.0);
        assert!((w - 100.0).abs() < 1e-6 && (h - 50.0).abs() < 1e-6);
    }

    #[test]
    fn border_dims() {
        let img = sample(20, 10);
        let spec = BorderSpec {
            width: 5.0,
            bottom: 15.0,
            color: [255, 255, 255, 255],
            line_width: 1.0,
            line_color: [0, 0, 0, 255],
            radius: 4.0,
            shadow: false,
            shadow_blur: 0.0,
            shadow_offset: 0.0,
            shadow_opacity: 0.5,
            shadow_bg: [255; 4],
        };
        let b = add_border(&img, &spec);
        assert_eq!(b.dimensions(), (30, 30));
        assert_eq!(b.get_pixel(0, 0)[3], 0, "rounded corner is transparent");
        assert_eq!(b.get_pixel(10, 10), img.get_pixel(5, 5));
        let s = add_border(&img, &BorderSpec { shadow: true, shadow_blur: 4.0, shadow_offset: 3.0, ..spec });
        let (ow, oh, _) = border_size(20, 10, &BorderSpec { shadow: true, shadow_blur: 4.0, shadow_offset: 3.0, ..spec });
        assert_eq!(s.dimensions(), (ow, oh));
        let none = BorderSpec { width: 0.0, bottom: 0.0, line_width: 0.0, radius: 0.0, ..spec };
        assert_eq!(add_border(&img, &none), img);
    }

    #[test]
    fn composite_basics() {
        let mut d = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]));
        let s = RgbaImage::from_pixel(2, 2, Rgba([255, 255, 255, 255]));
        composite_over(&mut d, &s, 3, 3, 0.5);
        assert_eq!(d.get_pixel(3, 3), &Rgba([128, 128, 128, 255]));
        assert_eq!(d.get_pixel(2, 2), &Rgba([0, 0, 0, 255]));
        composite_over(&mut d, &s, -10, -10, 1.0);
    }
}
