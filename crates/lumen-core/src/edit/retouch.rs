//! Local retouching pixel operations: clone stamp, healing brush, red-eye
//! removal and obscuring regions (pixelate / blur / solid fill). Everything
//! works on bounded boxes around the edit, so cost scales with the edited area,
//! not the photo size.

use super::filters;
use super::mask::{self, Mask, Shape};
use image::RgbaImage;
use rayon::prelude::*;

/// One clone / heal brush stroke, in full-resolution image pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct BrushStroke {
    pub pts: Vec<[f32; 2]>,
    pub radius: f32,
    /// 0 = very soft, 1 = hard edge.
    pub hardness: f32,
    /// 0..1
    pub opacity: f32,
    /// Source minus destination (where pixels are copied from).
    pub offset: [f32; 2],
    /// Healing: match the colour and brightness of the destination.
    pub heal: bool,
}

impl BrushStroke {
    pub fn scaled(&self, s: f32) -> BrushStroke {
        BrushStroke {
            pts: self.pts.iter().map(|p| [p[0] * s, p[1] * s]).collect(),
            radius: (self.radius * s).max(0.5),
            offset: [self.offset[0] * s, self.offset[1] * s],
            ..self.clone()
        }
    }
}

/// Bilinear RGBA sample from a sub-image placed at (`ox`, `oy`) in image space.
/// None when outside it.
#[inline]
fn sample_patch(p: &RgbaImage, ox: f32, oy: f32, x: f32, y: f32) -> Option<[f32; 4]> {
    let (lx, ly) = (x - ox - 0.5, y - oy - 0.5);
    let (w, h) = (p.width() as f32, p.height() as f32);
    if !(lx >= -0.5 && ly >= -0.5 && lx <= w - 0.5 && ly <= h - 0.5) {
        return None;
    }
    Some(filters::sample_bilinear(p.as_raw(), p.width() as usize, p.height() as usize, lx, ly))
}

/// Copy the box (x0, y0, w, h) out of `img`.
fn patch(img: &RgbaImage, b: (u32, u32, u32, u32)) -> RgbaImage {
    image::imageops::crop_imm(img, b.0, b.1, b.2, b.3).to_image()
}

/// Apply clone / heal strokes in order (later strokes see earlier results).
/// `scale` maps the strokes' full-resolution coordinates onto `img`.
pub fn apply_strokes(img: &mut RgbaImage, strokes: &[BrushStroke], scale: f32) {
    for s in strokes {
        let s = s.scaled(scale);
        if s.opacity <= 0.0 || s.pts.is_empty() || !s.offset.iter().all(|v| v.is_finite()) {
            continue;
        }
        if s.heal {
            heal_stroke(img, &s);
        } else {
            clone_stroke(img, &s);
        }
    }
}

fn clone_stroke(img: &mut RgbaImage, s: &BrushStroke) {
    let (iw, ih) = img.dimensions();
    let m = mask::stroke_mask(&s.pts, s.radius, s.hardness, iw, ih);
    if m.is_empty() {
        return;
    }
    let [dx, dy] = s.offset;
    let src_box = [m.x0 as f32 + dx - 2.0, m.y0 as f32 + dy - 2.0, (m.x0 + m.w) as f32 + dx + 2.0, (m.y0 + m.h) as f32 + dy + 2.0];
    let Some(sb) = mask::clip_box(src_box, iw, ih) else { return };
    let src = patch(img, sb);
    let op = s.opacity.clamp(0.0, 1.0);
    for_mask_rows(img, &m, |x, y, c, d| {
        if let Some(v) = sample_patch(&src, sb.0 as f32, sb.1 as f32, x as f32 + 0.5 + dx, y as f32 + 0.5 + dy) {
            let t = c * op;
            for k in 0..4 {
                d[k] = (d[k] as f32 + (v[k] - d[k] as f32) * t).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
}

/// Visit every pixel of the mask box with nonzero coverage (parallel over rows).
fn for_mask_rows(img: &mut RgbaImage, m: &Mask, f: impl Fn(u32, u32, f32, &mut [u8]) + Sync) {
    let iw = img.width();
    let stride = iw as usize * 4;
    let (x0, y0, x1, y1) = m.bounds();
    img.as_mut().par_chunks_mut(stride).enumerate().skip(y0 as usize).take((y1 - y0) as usize).for_each(|(y, row)| {
        let y = y as u32;
        for x in x0..x1.min(iw) {
            let c = m.data[(y - y0) as usize * m.w as usize + (x - x0) as usize];
            if c > 0 {
                f(x, y, c as f32 / 255.0, &mut row[x as usize * 4..x as usize * 4 + 4]);
            }
        }
    });
}

/// Healing: clone the source texture, then add a smooth colour offset field
/// that matches the destination around the stroke's border (a fast membrane /
/// normalised-convolution approximation of Poisson blending).
fn heal_stroke(img: &mut RgbaImage, s: &BrushStroke) {
    let (iw, ih) = img.dimensions();
    let m = mask::stroke_mask(&s.pts, s.radius, s.hardness, iw, ih);
    if m.is_empty() {
        return;
    }
    let [dx, dy] = s.offset;
    // Work box: the mask grown by a border band whose colours drive the match.
    let band = (s.radius * 0.6).clamp(2.0, 400.0);
    let Some(wb) = mask::clip_box([m.x0 as f32 - band, m.y0 as f32 - band, (m.x0 + m.w) as f32 + band, (m.y0 + m.h) as f32 + band], iw, ih) else { return };
    let Some(sb) = mask::clip_box([wb.0 as f32 + dx - 2.0, wb.1 as f32 + dy - 2.0, (wb.0 + wb.2) as f32 + dx + 2.0, (wb.1 + wb.3) as f32 + dy + 2.0], iw, ih) else { return };
    let src = patch(img, sb);
    let (ww, wh) = (wb.2 as usize, wb.3 as usize);
    let sigma = (s.radius * 0.7).max(1.5);
    // The colour offset field is very smooth (Gaussian of `sigma`), so it is
    // built on a grid coarsened by `f` (f = 1 for small brushes: exact). Source
    // colours are sampled on the fly rather than stored, so memory is about
    // 16 bytes per coarse cell instead of ~48 bytes per pixel of the stroke's
    // bounding box (a long diagonal stroke on a large photo would otherwise
    // need gigabytes).
    let f = heal_grid_factor(sigma, ww, wh);
    let (cw, ch) = (ww.div_ceil(f), wh.div_ceil(f));
    let sample_src = |x: u32, y: u32| sample_patch(&src, sb.0 as f32, sb.1 as f32, x as f32 + 0.5 + dx, y as f32 + 0.5 + dy);
    // Boundary samples: destination minus source just outside the stroke,
    // summed per coarse cell (one band of coarse rows per task).
    let raw = img.as_raw();
    let band_rows: Vec<(Vec<f32>, [Vec<f32>; 3], [f64; 3], f64)> = (0..ch)
        .into_par_iter()
        .map(|cy| {
            let mut wgt = vec![0f32; cw];
            let mut diff: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0f32; cw]);
            let (mut acc, mut cnt) = ([0f64; 3], 0f64);
            for ry in cy * f..((cy + 1) * f).min(wh) {
                for rx in 0..ww {
                    let (x, y) = (wb.0 + rx as u32, wb.1 + ry as u32);
                    if m.at(x, y) > 0.0 {
                        continue;
                    }
                    if let Some(sv) = sample_src(x, y) {
                        let di = (y as usize * iw as usize + x as usize) * 4;
                        let cx = rx / f;
                        wgt[cx] += 1.0;
                        for c in 0..3 {
                            let d = raw[di + c] as f32 - sv[c];
                            diff[c][cx] += d;
                            acc[c] += d as f64;
                        }
                        cnt += 1.0;
                    }
                }
            }
            (wgt, diff, acc, cnt)
        })
        .collect();
    let n = cw * ch;
    let mut wsum = Vec::with_capacity(n);
    let mut diff: [Vec<f32>; 3] = std::array::from_fn(|_| Vec::with_capacity(n));
    let (mut acc, mut cnt) = ([0f64; 3], 0f64);
    for (w, d, a, c) in band_rows {
        wsum.extend_from_slice(&w);
        for k in 0..3 {
            diff[k].extend_from_slice(&d[k]);
            acc[k] += a[k];
        }
        cnt += c;
    }
    let mean = if cnt > 0.0 { acc.map(|a| (a / cnt) as f32) } else { [0.0; 3] };
    let cs = sigma / f as f32;
    mask::blur_plane(&mut wsum, cw, ch, cs);
    for d in diff.iter_mut() {
        mask::blur_plane(d, cw, ch, cs);
    }
    // Cell sums hold up to f^2 samples: scale the "no boundary nearby" threshold.
    let ws_min = 1e-3 * (f * f) as f32;
    let op = s.opacity.clamp(0.0, 1.0);
    for_mask_rows(img, &m, |x, y, c, d| {
        let Some(sv) = sample_src(x, y) else { return };
        let (rx, ry) = ((x - wb.0) as f32, (y - wb.1) as f32);
        let (gx, gy) = ((rx + 0.5) / f as f32 - 0.5, (ry + 0.5) / f as f32 - 0.5);
        let ws = sample_plane(&wsum, cw, ch, gx, gy);
        let t = c * op;
        for k in 0..3 {
            let off = if ws > ws_min { sample_plane(&diff[k], cw, ch, gx, gy) / ws } else { mean[k] };
            let v = sv[k] + off;
            d[k] = (d[k] as f32 + (v - d[k] as f32) * t).round().clamp(0.0, 255.0) as u8;
        }
        d[3] = (d[3] as f32 + (sv[3] - d[3] as f32) * t).round().clamp(0.0, 255.0) as u8;
    });
}

/// Coarsening factor for the healing offset field: exact (1) for small
/// brushes, otherwise about sigma / 3 (the field has no detail finer than
/// that), raised further so the grid never exceeds ~4 M cells.
fn heal_grid_factor(sigma: f32, ww: usize, wh: usize) -> usize {
    const MAX_CELLS: usize = 4 << 20;
    let mut f = ((sigma / 3.0).floor() as usize).max(1);
    while ww.div_ceil(f) * wh.div_ceil(f) > MAX_CELLS {
        f += 1;
    }
    f
}

/// Bilinear sample of a single-channel plane with edge clamping.
#[inline]
fn sample_plane(p: &[f32], w: usize, h: usize, x: f32, y: f32) -> f32 {
    let x = x.max(0.0).min((w - 1) as f32);
    let y = y.max(0.0).min((h - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let top = p[y0 * w + x0] * (1.0 - fx) + p[y0 * w + x1] * fx;
    let bot = p[y1 * w + x0] * (1.0 - fx) + p[y1 * w + x1] * fx;
    top * (1.0 - fy) + bot * fy
}

// ---------------------------------------------------------------- red eye

/// Remove red-eye inside the ellipses inscribed in `boxes` (x0, y0, x1, y1,
/// full-resolution pixels; scaled by `scale`). `sensitivity` 0..1 widens what
/// counts as red, `darken` 0..1 deepens the corrected pupil.
pub fn red_eye(img: &mut RgbaImage, boxes: &[[f32; 4]], scale: f32, sensitivity: f32, darken: f32) {
    let (iw, ih) = img.dimensions();
    let sens = sensitivity.clamp(0.0, 1.0);
    let darken = darken.clamp(0.0, 1.0);
    // Red / mean(green, blue) ratio needed to count as red.
    let thresh = 2.1 - 0.9 * sens;
    for b in boxes {
        let b = b.map(|v| v * scale);
        let (bx0, by0, bx1, by1) = (b[0].min(b[2]), b[1].min(b[3]), b[0].max(b[2]), b[1].max(b[3]));
        let Some((x0, y0, w, h)) = mask::clip_box([bx0, by0, bx1, by1], iw, ih) else { continue };
        let (cx, cy) = ((bx0 + bx1) / 2.0, (by0 + by1) / 2.0);
        let (rx, ry) = (((bx1 - bx0) / 2.0).max(0.5), ((by1 - by0) / 2.0).max(0.5));
        let (wu, hu) = (w as usize, h as usize);
        let raw = img.as_raw();
        // Weight map: redness x position inside the ellipse.
        let mut wgt = vec![0f32; wu * hu];
        wgt.par_chunks_mut(wu).enumerate().for_each(|(ry_i, row)| {
            for (rx_i, v) in row.iter_mut().enumerate() {
                let (x, y) = (x0 as usize + rx_i, y0 as usize + ry_i);
                let (nx, ny) = (((x as f32 + 0.5) - cx) / rx, ((y as f32 + 0.5) - cy) / ry);
                let e = 1.0 - smoothstep(0.75, 1.0, (nx * nx + ny * ny).sqrt());
                if e <= 0.0 {
                    continue;
                }
                let i = (y * iw as usize + x) * 4;
                let (r, g, bl) = (raw[i] as f32, raw[i + 1] as f32, raw[i + 2] as f32);
                let gb = (g + bl) / 2.0;
                let ratio = r / (gb + 10.0);
                let red = smoothstep(thresh * 0.8, thresh * 1.1, ratio) * smoothstep(40.0, 80.0, r);
                *v = e * red;
            }
        });
        // Fill speckles and grow the weight slightly so the anti-aliased rim of
        // the pupil is corrected too (a plain blur would leave a red ring).
        let mut soft = wgt.clone();
        mask::blur_plane(&mut soft, wu, hu, (rx.min(ry) * 0.06).clamp(0.6, 6.0));
        for (w, s) in wgt.iter_mut().zip(&soft) {
            *w = w.max((s * 2.0).min(1.0));
        }
        let stride = iw as usize * 4;
        img.as_mut().par_chunks_mut(stride).enumerate().skip(y0 as usize).take(hu).for_each(|(y, row)| {
            for x in x0 as usize..x0 as usize + wu {
                let t = wgt[(y - y0 as usize) * wu + (x - x0 as usize)].clamp(0.0, 1.0);
                if t <= 0.002 {
                    continue;
                }
                let p = &mut row[x * 4..x * 4 + 4];
                let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
                let gb = (g + b) / 2.0;
                let k = 1.0 - 0.45 * darken;
                let target = [gb * k, g * k, b * k];
                for c in 0..3 {
                    let cur = [r, g, b][c];
                    p[c] = (cur + (target[c] - cur) * t).round().clamp(0.0, 255.0) as u8;
                }
            }
        });
    }
}

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------- obscure regions

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Obscure {
    /// Mosaic with square cells of this size (px).
    Pixelate(f32),
    /// Gaussian blur with this sigma (px).
    Blur(f32),
    /// Solid colour (straight RGBA).
    Fill([u8; 4]),
}

/// Obscure the union of `shapes` (full-resolution pixels, scaled by `scale`).
/// `feather` softens the edge (px, full resolution).
pub fn obscure(img: &mut RgbaImage, shapes: &[Shape], mode: Obscure, feather: f32, scale: f32) {
    let (iw, ih) = img.dimensions();
    let shapes: Vec<Shape> = shapes.iter().map(|s| s.scaled(scale)).collect();
    let m = mask::shapes_mask(&shapes, feather * scale, iw, ih);
    if m.is_empty() {
        return;
    }
    match mode {
        Obscure::Fill(c) => {
            for_mask_rows(img, &m, |_, _, cov, d| {
                let t = cov * c[3] as f32 / 255.0;
                for k in 0..3 {
                    d[k] = (d[k] as f32 + (c[k] as f32 - d[k] as f32) * t).round() as u8;
                }
                d[3] = (d[3] as f32 + (255.0 - d[3] as f32) * t).round() as u8;
            });
        }
        Obscure::Pixelate(cell) => {
            // No 2 px floor: on a downscaled preview a small cell must stay
            // proportionally small, or the preview looks coarser than the result.
            let cell = (cell * scale).round().clamp(1.0, 4096.0) as u32;
            // Average every grid cell (aligned to the image origin) that touches the mask.
            let (gx0, gy0) = (m.x0 / cell, m.y0 / cell);
            let (gx1, gy1) = ((m.x0 + m.w).div_ceil(cell), (m.y0 + m.h).div_ceil(cell));
            let raw = img.as_raw();
            let cols = (gx1 - gx0) as usize;
            let avgs: Vec<[u8; 4]> = (0..((gy1 - gy0) as usize * cols))
                .into_par_iter()
                .map(|i| {
                    let (gx, gy) = (gx0 + (i % cols) as u32, gy0 + (i / cols) as u32);
                    let (x0, y0) = (gx * cell, gy * cell);
                    let (x1, y1) = ((x0 + cell).min(iw), (y0 + cell).min(ih));
                    let mut s = [0u64; 4];
                    for y in y0..y1 {
                        for x in x0..x1 {
                            let j = (y as usize * iw as usize + x as usize) * 4;
                            let a = raw[j + 3] as u64;
                            for c in 0..3 {
                                s[c] += raw[j + c] as u64 * a;
                            }
                            s[3] += a;
                        }
                    }
                    let n = ((x1 - x0) * (y1 - y0)).max(1) as u64;
                    if s[3] == 0 {
                        [0, 0, 0, 0]
                    } else {
                        [(s[0] / s[3]) as u8, (s[1] / s[3]) as u8, (s[2] / s[3]) as u8, (s[3] / n).min(255) as u8]
                    }
                })
                .collect();
            for_mask_rows(img, &m, |x, y, cov, d| {
                let v = avgs[((y / cell - gy0) as usize) * cols + (x / cell - gx0) as usize];
                for k in 0..4 {
                    d[k] = (d[k] as f32 + (v[k] as f32 - d[k] as f32) * cov).round() as u8;
                }
            });
        }
        Obscure::Blur(sigma) => {
            let sigma = (sigma * scale).clamp(0.5, 2000.0);
            let pad = sigma * 3.0 + 2.0;
            let Some(b) = mask::clip_box([m.x0 as f32 - pad, m.y0 as f32 - pad, (m.x0 + m.w) as f32 + pad, (m.y0 + m.h) as f32 + pad], iw, ih) else { return };
            let region = patch(img, b);
            let blurred = filters::gaussian_blur(&region, sigma);
            mask::blend_masked(img, &blurred, (b.0, b.1), &m, false, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| if (x / 4 + y / 4) % 2 == 0 { image::Rgba([230, 230, 230, 255]) } else { image::Rgba([20, 20, 20, 255]) })
    }

    #[test]
    fn clone_copies_offset_pixels() {
        let mut img = RgbaImage::from_fn(60, 30, |x, _| if x < 30 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
        let s = BrushStroke { pts: vec![[45.0, 15.0]], radius: 6.0, hardness: 1.0, opacity: 1.0, offset: [-30.0, 0.0], heal: false };
        apply_strokes(&mut img, &[s.clone()], 1.0);
        assert_eq!(img.get_pixel(45, 15).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(55, 15).0, [0, 0, 255, 255]);
        // Half scale: same stroke at preview resolution.
        let mut small = RgbaImage::from_fn(30, 15, |x, _| if x < 15 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
        apply_strokes(&mut small, &[s], 0.5);
        assert_eq!(small.get_pixel(22, 7)[0], 255);
    }

    #[test]
    fn clone_source_off_image_is_safe() {
        let mut img = checker(20, 20);
        let orig = img.clone();
        let s = BrushStroke { pts: vec![[10.0, 10.0], [12.0, 12.0]], radius: 4.0, hardness: 0.5, opacity: 1.0, offset: [500.0, 500.0], heal: false };
        apply_strokes(&mut img, &[s.clone()], 1.0);
        assert_eq!(img, orig);
        apply_strokes(&mut img, &[BrushStroke { heal: true, ..s }], 1.0);
        assert_eq!(img, orig);
        let nan = BrushStroke { pts: vec![[f32::NAN, 1.0]], radius: f32::NAN, hardness: 0.5, opacity: 1.0, offset: [1.0, 1.0], heal: true };
        apply_strokes(&mut img, &[nan], 1.0);
    }

    #[test]
    fn heal_matches_destination_tone() {
        // Source texture is a dark checker, destination area is bright and flat
        // with a blemish; healing must keep the texture but adopt the brightness.
        let mut img = RgbaImage::from_fn(120, 60, |x, y| {
            if x < 60 {
                let v = if (x / 3 + y / 3) % 2 == 0 { 60 } else { 40 };
                image::Rgba([v, v, v, 255])
            } else {
                image::Rgba([200, 200, 200, 255])
            }
        });
        for y in 27..33 {
            for x in 87..93 {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        let s = BrushStroke { pts: vec![[90.0, 30.0]], radius: 8.0, hardness: 0.6, opacity: 1.0, offset: [-60.0, 0.0], heal: true };
        apply_strokes(&mut img, &[s.clone()], 1.0);
        let c = img.get_pixel(90, 30)[0] as i32;
        assert!((c - 200).abs() < 25, "healed centre {c}");
        // Plain clone would have copied the dark texture.
        let mut img2 = img.clone();
        apply_strokes(&mut img2, &[BrushStroke { heal: false, ..s }], 1.0);
        assert!(img2.get_pixel(90, 30)[0] < 80);
    }

    #[test]
    fn heal_with_coarse_offset_grid() {
        // A big brush uses the coarsened offset field (f > 1); the result must
        // still adopt the destination brightness.
        assert_eq!(heal_grid_factor(3.0, 100, 100), 1);
        assert!(heal_grid_factor(21.0, 100, 100) >= 7);
        // A huge work box is bounded to ~4 M cells.
        let f = heal_grid_factor(2.0, 10_000, 8_000);
        assert!(10_000usize.div_ceil(f) * 8_000usize.div_ceil(f) <= 4 << 20);
        let mut img = RgbaImage::from_fn(400, 200, |x, _| if x < 200 { image::Rgba([50, 50, 50, 255]) } else { image::Rgba([210, 210, 210, 255]) });
        let s = BrushStroke { pts: vec![[260.0, 60.0], [340.0, 140.0]], radius: 30.0, hardness: 0.6, opacity: 1.0, offset: [-200.0, 0.0], heal: true };
        apply_strokes(&mut img, &[s], 1.0);
        let c = img.get_pixel(300, 100)[0] as i32;
        assert!((c - 210).abs() < 25, "healed centre {c}");
    }

    #[test]
    fn red_eye_desaturates_red_only() {
        let mut img = RgbaImage::from_fn(40, 40, |x, y| {
            let d = ((x as f32 - 20.0).powi(2) + (y as f32 - 20.0).powi(2)).sqrt();
            if d < 8.0 {
                image::Rgba([220, 40, 50, 255])
            } else {
                image::Rgba([90, 120, 160, 255])
            }
        });
        red_eye(&mut img, &[[8.0, 8.0, 32.0, 32.0]], 1.0, 0.5, 0.5);
        let p = img.get_pixel(20, 20);
        assert!(p[0] < 80, "pupil still red: {p:?}");
        assert_eq!(img.get_pixel(2, 2).0, [90, 120, 160, 255]);
        // Non-red pixels inside the box are untouched.
        assert_eq!(img.get_pixel(12, 30).0, [90, 120, 160, 255]);
        // Degenerate boxes never panic.
        red_eye(&mut img, &[[5.0, 5.0, 5.0, 5.0], [-50.0, -50.0, -10.0, -10.0], [f32::NAN, 0.0, 1.0, 1.0]], 1.0, 1.0, 1.0);
    }

    #[test]
    fn obscure_modes() {
        let base = checker(64, 64);
        let r = [Shape::Rect([8.0, 8.0, 40.0, 40.0])];
        let mut px = base.clone();
        obscure(&mut px, &r, Obscure::Pixelate(16.0), 0.0, 1.0);
        // Inside one cell everything is identical.
        assert_eq!(px.get_pixel(17, 17), px.get_pixel(30, 30));
        assert_eq!(px.get_pixel(50, 50), base.get_pixel(50, 50));
        let mut bl = base.clone();
        obscure(&mut bl, &r, Obscure::Blur(6.0), 0.0, 1.0);
        let v = bl.get_pixel(24, 24)[0];
        assert!(v > 70 && v < 180, "{v}");
        assert_eq!(bl.get_pixel(60, 60), base.get_pixel(60, 60));
        let mut f = base.clone();
        obscure(&mut f, &[Shape::Ellipse([0.0, 0.0, 64.0, 64.0])], Obscure::Fill([0, 0, 0, 255]), 2.0, 0.5);
        assert_eq!(f.get_pixel(16, 16).0, [0, 0, 0, 255]);
        assert_eq!(f.get_pixel(63, 63), base.get_pixel(63, 63));
        // Empty / off-image shapes are no-ops.
        let mut e = base.clone();
        obscure(&mut e, &[Shape::Rect([100.0, 100.0, 120.0, 120.0])], Obscure::Pixelate(8.0), 3.0, 1.0);
        assert_eq!(e, base);
    }
}
