//! Lossless JPEG editing: coefficient round trips, crops checked against an
//! independent decoder (the `image` crate's), rotations, damaged files and
//! metadata.

use super::*;
use image::{RgbImage, RgbaImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor as S};

// Made by testdata/make.py with libjpeg-turbo: progressive files with
// successive approximation, restart markers and changing tables.
const PROG420: &[u8] = include_bytes!("testdata/prog420_rst.jpg");
const PROG444: &[u8] = include_bytes!("testdata/prog444.jpg");
const PROG422: &[u8] = include_bytes!("testdata/prog422_rows.jpg");
const PROG_GRAY: &[u8] = include_bytes!("testdata/prog_gray_rst.jpg");
const PROG_CMYK: &[u8] = include_bytes!("testdata/prog_cmyk.jpg");
const BASE420: &[u8] = include_bytes!("testdata/base420_rst.jpg");

const FIXTURES: [(&str, &[u8]); 6] = [("prog420_rst", PROG420), ("prog444", PROG444), ("prog422_rows", PROG422), ("prog_gray_rst", PROG_GRAY), ("prog_cmyk", PROG_CMYK), ("base420_rst", BASE420)];

/// Every sampling jpeg-encoder writes: 4:4:4, 4:2:2, 4:2:0, 4:4:0, 4:1:1 and rarer ones.
const SAMPLINGS: [S; 8] = [S::F_1_1, S::F_2_1, S::F_2_2, S::F_1_2, S::F_4_1, S::F_1_4, S::F_2_4, S::F_4_2];

fn xorshift(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

/// A test picture with gradients, hard edges and noise (deterministic).
fn picture(w: u32, h: u32, seed: u32) -> RgbImage {
    let mut s = seed.wrapping_mul(2_654_435_761) | 1;
    RgbImage::from_fn(w, h, |x, y| {
        let (dx, dy) = (x as f32 - w as f32 * 0.35, y as f32 - h as f32 * 0.4);
        let disk = dx * dx + dy * dy < (w.min(h) as f32 * 0.2).powi(2);
        let base: [i32; 3] = if disk {
            [240, 50, 40]
        } else if x > w * 3 / 5 && y < h / 2 {
            [30, 40, 210]
        } else {
            [(x * 255 / w.max(1)) as i32, (y * 255 / h.max(1)) as i32, 128 + ((x as f32 / 4.0).sin() * 90.0) as i32]
        };
        let n = (xorshift(&mut s) % 41) as i32 - 20;
        image::Rgb([(base[0] + n).clamp(0, 255) as u8, (base[1] - n).clamp(0, 255) as u8, (base[2] + n / 2).clamp(0, 255) as u8])
    })
}

fn encode_with(data: &[u8], w: u32, h: u32, color: ColorType, sampling: S, progressive: bool, restart: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out, 88);
    e.set_sampling_factor(sampling);
    e.set_progressive(progressive);
    e.set_restart_interval(restart);
    e.encode(data, w as u16, h as u16, color).unwrap();
    out
}

fn jpeg(img: &RgbImage, sampling: S, progressive: bool, restart: u16) -> Vec<u8> {
    encode_with(img.as_raw(), img.width(), img.height(), ColorType::Rgb, sampling, progressive, restart)
}

/// Decoded by the `image` crate (zune-jpeg), without EXIF orientation.
fn rgb(data: &[u8]) -> RgbImage {
    image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).unwrap().to_rgb8()
}

fn rgba(data: &[u8]) -> RgbaImage {
    image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).unwrap().to_rgba8()
}

/// `segs` (marker, payload) put right after the start of image.
fn with_segments(jpeg: &[u8], segs: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    for (m, b) in segs {
        out.extend_from_slice(&[0xFF, *m]);
        out.extend_from_slice(&((b.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn exif_segment(tiff: &[u8]) -> (u8, Vec<u8>) {
    (0xE1, [b"Exif\0\0".as_slice(), tiff].concat())
}

/// Same size, components, quantization and coefficients in every block that
/// holds picture.
fn same_coefficients(a: &Jpeg, b: &Jpeg) -> Result<(), String> {
    if (a.width, a.height) != (b.width, b.height) {
        return Err(format!("size {}x{} vs {}x{}", a.width, a.height, b.width, b.height));
    }
    if a.comps.len() != b.comps.len() {
        return Err("component count".into());
    }
    for (i, (ca, cb)) in a.comps.iter().zip(&b.comps).enumerate() {
        if (ca.id, ca.h, ca.v, ca.q) != (cb.id, cb.h, cb.v, cb.q) {
            return Err(format!("component {i}: header"));
        }
        let (rw, rh) = a.real_blocks(ca);
        for by in 0..rh {
            for bx in 0..rw {
                if ca.blocks[by * ca.bw + bx] != cb.blocks[by * cb.bw + bx] {
                    return Err(format!("component {i}: block {bx},{by}"));
                }
            }
        }
    }
    Ok(())
}

/// Largest and mean channel difference between `got` and the region of
/// `want` at (`ox`, `oy`), leaving out `margin` pixels at `got`'s edges.
fn diff(got: &RgbaImage, want: &RgbaImage, (ox, oy): (u32, u32), margin: u32) -> (u8, f64) {
    let (mut worst, mut sum, mut n) = (0u8, 0u64, 0u64);
    for y in margin..got.height().saturating_sub(margin) {
        for x in margin..got.width().saturating_sub(margin) {
            let (p, q) = (got.get_pixel(x, y).0, want.get_pixel(x + ox, y + oy).0);
            for k in 0..3 {
                let d = p[k].abs_diff(q[k]);
                worst = worst.max(d);
                sum += d as u64;
                n += 1;
            }
        }
    }
    (worst, sum as f64 / n.max(1) as f64)
}

fn to_rgba(img: RgbImage) -> RgbaImage {
    image::DynamicImage::ImageRgb8(img).to_rgba8()
}

#[test]
fn round_trip_keeps_every_coefficient() {
    let img = picture(75, 53, 1);
    for s in SAMPLINGS {
        for progressive in [false, true] {
            for restart in [0u16, 5] {
                let what = format!("{s:?} progressive={progressive} restart={restart}");
                let src = jpeg(&img, s, progressive, restart);
                let a = decode::parse(&src).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_eq!(a.progressive, progressive, "{what}");
                assert_eq!(a.restart, restart, "{what}");
                let out = encode::write(&a).unwrap_or_else(|e| panic!("{what}: {e}"));
                let b = decode::parse(&out).unwrap_or_else(|e| panic!("{what}: rewritten: {e}"));
                assert!(!b.progressive);
                assert_eq!(b.restart, restart, "{what}");
                same_coefficients(&a, &b).unwrap_or_else(|e| panic!("{what}: {e}"));
                // Written again from the copy: the very same bytes.
                assert_eq!(encode::write(&b).unwrap(), out, "{what}");
                if matches!(s, S::F_1_1 | S::F_2_1 | S::F_2_2 | S::F_1_2 | S::F_4_1) {
                    assert_eq!(rgb(&out).dimensions(), (75, 53), "{what}");
                }
                // The same picture to an independent decoder (it reads
                // jpeg-encoder's interleaved baseline and progressive files).
                if matches!(s, S::F_1_1 | S::F_2_1 | S::F_2_2) {
                    let (worst, _) = diff(&rgba(&out), &rgba(&src), (0, 0), 0);
                    assert!(worst <= 1, "{what}: {worst}");
                }
            }
        }
    }
}

#[test]
fn gray_and_four_channel_round_trips() {
    let img = picture(43, 29, 2);
    let gray: Vec<u8> = img.pixels().map(|p| p.0[1]).collect();
    let cmyk: Vec<u8> = img.pixels().flat_map(|p| [p.0[0], p.0[1], p.0[2], 255 - p.0[1]]).collect();
    let cases = [
        ("gray", encode_with(&gray, 43, 29, ColorType::Luma, S::F_2_2, false, 0)),
        ("gray progressive", encode_with(&gray, 43, 29, ColorType::Luma, S::F_1_1, true, 3)),
        ("cmyk", encode_with(&cmyk, 43, 29, ColorType::Cmyk, S::F_2_2, false, 0)),
        ("ycck progressive", encode_with(&cmyk, 43, 29, ColorType::Ycck, S::F_2_1, true, 4)),
    ];
    for (what, src) in cases {
        let a = decode::parse(&src).unwrap_or_else(|e| panic!("{what}: {e}"));
        let out = encode::write(&a).unwrap();
        let b = decode::parse(&out).unwrap_or_else(|e| panic!("{what}: rewritten: {e}"));
        same_coefficients(&a, &b).unwrap_or_else(|e| panic!("{what}: {e}"));
        // The Adobe segment that says how to read the colors stays.
        assert_eq!(a.segments.iter().any(|(m, _)| *m == 0xEE), b.segments.iter().any(|(m, _)| *m == 0xEE), "{what}");
    }
}

#[test]
fn progressive_files_from_libjpeg() {
    for (name, data) in FIXTURES {
        let a = decode::parse(data).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(a.progressive, name.starts_with("prog"), "{name}");
        let out = encode::write(&a).unwrap();
        let b = decode::parse(&out).unwrap();
        same_coefficients(&a, &b).unwrap_or_else(|e| panic!("{name}: {e}"));
        // An independent decoder sees the same picture.
        let (worst, _) = diff(&rgba(&out), &rgba(data), (0, 0), 0);
        assert!(worst <= 1, "{name}: {worst}");
    }
    assert_eq!(decode::parse(PROG420).unwrap().restart, 7);
    assert_eq!(decode::parse(BASE420).unwrap().restart, 3);
    assert_eq!(decode::parse(PROG_CMYK).unwrap().comps.len(), 4);
}

/// Crop `src` to `rect` (no EXIF orientation) and check the result: the
/// kept area, the coefficient blocks, and (with `pixels`) the decoded
/// picture away from the new edges.
fn check_crop(name: &str, src: &[u8], rect: (u32, u32, u32, u32), pixels: bool) {
    let full = decode::parse(src).unwrap();
    let (mw, mh) = full.imcu();
    let e = crop(src, rect).unwrap_or_else(|e| panic!("{name}: {e}"));
    let (x, y, w, h) = e.kept;
    assert_eq!((x, y), (rect.0 / mw * mw, rect.1 / mh * mh), "{name}");
    assert_eq!((x + w, y + h), (rect.0 + rect.2, rect.1 + rect.3), "{name}");
    assert_eq!(e.stored, (w, h), "{name}");
    assert_eq!(full.grid().snap(rect), e.kept, "{name}");
    let c = decode::parse(&e.bytes).unwrap_or_else(|e| panic!("{name}: cropped: {e}"));
    assert_eq!((c.width, c.height), (w, h), "{name}");
    for (i, (cs, cc)) in full.comps.iter().zip(&c.comps).enumerate() {
        assert_eq!((cs.id, cs.h, cs.v, cs.q), (cc.id, cc.h, cc.v, cc.q), "{name}");
        let (ox, oy) = ((x / mw) as usize * cs.h, (y / mh) as usize * cs.v);
        let (rw, rh) = c.real_blocks(cc);
        for by in 0..rh {
            for bx in 0..rw {
                assert_eq!(cc.blocks[by * cc.bw + bx], cs.blocks[(oy + by) * cs.bw + ox + bx], "{name}: component {i} block {bx},{by}");
            }
        }
    }
    if pixels {
        // Chroma upsampling looks across the new edges, so leave them out.
        let reference = rgba(&encode::write(&full).unwrap());
        let got = rgba(&e.bytes);
        assert_eq!(got.dimensions(), (w, h), "{name}");
        let (worst, _) = diff(&got, &reference, (x, y), 8);
        assert!(worst <= 2, "{name}: {worst}");
    }
}

#[test]
fn crops_keep_the_picture() {
    let img = picture(160, 120, 3);
    for s in SAMPLINGS {
        // The independent decoder upsamples the common samplings only.
        let pixels = matches!(s, S::F_1_1 | S::F_2_1 | S::F_2_2 | S::F_1_2);
        for (progressive, restart) in [(false, 0), (false, 3), (true, 0)] {
            let src = jpeg(&img, s, progressive, restart);
            let name = format!("{s:?} progressive={progressive} restart={restart}");
            check_crop(&name, &src, (37, 21, 90, 70), pixels);
            check_crop(&name, &src, (0, 0, 160, 120), pixels);
            check_crop(&name, &src, (64, 32, 96, 88), pixels);
            check_crop(&name, &src, (155, 117, 5, 3), false);
        }
    }
    let gray: Vec<u8> = img.pixels().map(|p| p.0[0]).collect();
    check_crop("gray", &encode_with(&gray, 160, 120, ColorType::Luma, S::F_1_1, false, 0), (13, 9, 100, 50), true);
    check_crop("prog420_rst", PROG420, (19, 17, 80, 60), true);
    check_crop("prog444", PROG444, (9, 9, 40, 30), true);
    check_crop("prog422_rows", PROG422, (20, 9, 60, 35), true);
    check_crop("prog_gray_rst", PROG_GRAY, (13, 5, 50, 30), true);
    check_crop("prog_cmyk", PROG_CMYK, (3, 3, 30, 20), true);
    check_crop("base420_rst", BASE420, (17, 33, 60, 18), true);
}

#[test]
fn crop_follows_the_exif_orientation() {
    let img = picture(160, 96, 4);
    let base = jpeg(&img, S::F_2_2, false, 0);
    // The full picture as decoded, stored orientation.
    let stored = rgba(&encode::write(&decode::parse(&base).unwrap()).unwrap());
    for o in 1..=8u8 {
        let src = with_segments(&base, &[exif_segment(&crate::encode::minimal_exif(o as u16))]);
        let grid = probe(&src).unwrap();
        assert_eq!(grid.orientation, o);
        assert_eq!(grid.step, (16, 16));
        assert_eq!(grid.display_size(), if o >= 5 { (96, 160) } else { (160, 96) });
        let rect = (17, 21, 70, 50);
        let e = crop(&src, rect).unwrap();
        let k = e.kept;
        assert_eq!(k, grid.snap(rect), "o={o}");
        assert!(k.0 <= rect.0 && k.1 <= rect.1 && k.0 + k.2 >= rect.0 + rect.2 && k.1 + k.3 >= rect.1 + rect.3, "o={o}: {k:?}");
        // Only the edges that are the stored left and top move.
        assert!(rect.0 - k.0 < 16 && rect.1 - k.1 < 16 && k.0 + k.2 - rect.0 - rect.2 < 16 && k.1 + k.3 - rect.1 - rect.3 < 16, "o={o}: {k:?}");
        // The orientation stays, and the picture shown is that part of the original.
        assert_eq!(probe(&e.bytes).unwrap().orientation, o);
        let shown = crate::imaging::orient(rgba(&e.bytes), o);
        assert_eq!(shown.dimensions(), (k.2, k.3), "o={o}");
        let (worst, _) = diff(&shown, &crate::imaging::orient(stored.clone(), o), (k.0, k.1), 8);
        assert!(worst <= 2, "o={o}: {worst}");
    }
}

#[test]
fn displayed_and_stored_rectangles_agree() {
    let (sw, sh) = (13u32, 7u32);
    let stored = RgbaImage::from_fn(sw, sh, |x, y| image::Rgba([x as u8, y as u8, 0, 255]));
    for o in 1..=8u8 {
        let g = CropGrid { stored: (sw, sh), orientation: o, step: (4, 2) };
        let shown = crate::imaging::orient(stored.clone(), o);
        let (dw, dh) = g.display_size();
        assert_eq!(shown.dimensions(), (dw, dh), "o={o}");
        for r in [(0, 0, 1, 1), (2, 1, 5, 3), (1, 3, 4, 2), (0, 0, dw, dh)] {
            let s = g.to_stored(r);
            assert_eq!(g.to_display(s), r, "o={o}");
            let a = image::imageops::crop_imm(&shown, r.0, r.1, r.2, r.3).to_image();
            let b = crate::imaging::orient(image::imageops::crop_imm(&stored, s.0, s.1, s.2, s.3).to_image(), o);
            assert!(a == b, "o={o} r={r:?}");
        }
        // `axes` names the displayed edge that is the stored left or top
        // edge: pushing it out by a pixel moves only that stored edge.
        let (ax, ay) = g.axes();
        let r = (2, 2, 3, 3);
        let s = g.to_stored(r);
        for (axis, wider) in [(ax, if ax.from_end { (2, 2, 4, 3) } else { (1, 2, 4, 3) }), (ay, if ay.from_end { (2, 2, 3, 4) } else { (2, 1, 3, 4) })] {
            let t = g.to_stored(wider);
            if axis.step == 4 {
                assert_eq!((t.0 + 1, t.1, t.2, t.3), (s.0, s.1, s.2 + 1, s.3), "o={o}");
            } else {
                assert_eq!((t.0, t.1 + 1, t.2, t.3), (s.0, s.1, s.2, s.3 + 1), "o={o}");
            }
        }
        let snapped = g.to_stored(g.snap((3, 3, 2, 2)));
        assert_eq!((snapped.0 % 4, snapped.1 % 2), (0, 0), "o={o}");
    }
}

/// `x` applied to pixels the way `ops::transform` moves blocks.
fn apply_xform(img: &RgbaImage, x: Xform) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (ow, oh) = if x.transpose { (h, w) } else { (w, h) };
    RgbaImage::from_fn(ow, oh, |ox, oy| {
        let xx = if x.flip_h { ow - 1 - ox } else { ox };
        let yy = if x.flip_v { oh - 1 - oy } else { oy };
        let (sx, sy) = if x.transpose { (yy, xx) } else { (xx, yy) };
        *img.get_pixel(sx, sy)
    })
}

#[test]
fn transforms_match_the_exif_orientations() {
    let probe_img = RgbaImage::from_fn(5, 3, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
    for o in 1..=8u8 {
        assert!(apply_xform(&probe_img, Xform::of_orientation(o)) == crate::imaging::orient(probe_img.clone(), o), "o={o}");
    }
}

#[test]
fn turning_back_restores_the_coefficients() {
    use Transform::*;
    let img = picture(64, 48, 5);
    for s in [S::F_1_1, S::F_2_1, S::F_2_2, S::F_1_2, S::F_4_1] {
        let src = jpeg(&img, s, false, 0);
        let a = decode::parse(&src).unwrap();
        for (t1, t2) in [(Rotate90, Rotate270), (Rotate270, Rotate90), (Rotate180, Rotate180), (FlipH, FlipH), (FlipV, FlipV)] {
            let once = rotate(&src, t1).unwrap_or_else(|e| panic!("{s:?} {t1:?}: {e}"));
            assert_eq!(once.trimmed, (0, 0), "{s:?} {t1:?}");
            let back = rotate(&once.bytes, t2).unwrap();
            same_coefficients(&a, &decode::parse(&back.bytes).unwrap()).unwrap_or_else(|e| panic!("{s:?} {t1:?} {t2:?}: {e}"));
        }
        // Four quarter turns, and transposing twice.
        let mut b = src.clone();
        for _ in 0..4 {
            b = rotate(&b, Rotate90).unwrap().bytes;
        }
        same_coefficients(&a, &decode::parse(&b).unwrap()).unwrap_or_else(|e| panic!("{s:?} 4 x 90: {e}"));
        let mut j = a.clone();
        ops::transform(&mut j, Xform::of_orientation(5)).unwrap();
        assert_ne!((j.width, j.height), (a.width, a.height));
        ops::transform(&mut j, Xform::of_orientation(5)).unwrap();
        same_coefficients(&a, &j).unwrap_or_else(|e| panic!("{s:?} transpose: {e}"));
    }
}

#[test]
fn turned_pictures_look_turned() {
    use Transform::*;
    let img = picture(80, 48, 6);
    for s in [S::F_1_1, S::F_2_1, S::F_2_2] {
        let src = jpeg(&img, s, false, 0);
        let reference = rgba(&src);
        for t in [Rotate90, Rotate180, Rotate270, FlipH, FlipV] {
            let e = rotate(&src, t).unwrap();
            let got = rgba(&e.bytes);
            let want = t.apply(&reference);
            assert_eq!(got.dimensions(), want.dimensions(), "{s:?} {t:?}");
            let (worst, mean) = diff(&got, &want, (0, 0), 2);
            // Without chroma subsampling only the decoder's rounding differs;
            // with it, the upsampling filter runs the other way round too.
            if s == S::F_1_1 {
                assert!(worst <= 6 && mean <= 0.5, "{s:?} {t:?}: {worst} {mean}");
            } else {
                assert!(mean <= 2.5, "{s:?} {t:?}: {worst} {mean}");
            }
        }
    }
    let gray: Vec<u8> = img.pixels().map(|p| p.0[2]).collect();
    let src = encode_with(&gray, 80, 48, ColorType::Luma, S::F_1_1, false, 0);
    let e = rotate(&src, Rotate90).unwrap();
    let (worst, mean) = diff(&rgba(&e.bytes), &Rotate90.apply(&rgba(&src)), (0, 0), 0);
    assert!(worst <= 6 && mean <= 0.5, "gray: {worst} {mean}");
}

#[test]
fn partial_edge_blocks_are_trimmed() {
    use Transform::*;
    // 4:2:0 has 16 px blocks: 75 = 4 x 16 + 11, 53 = 3 x 16 + 5.
    let src = jpeg(&picture(75, 53, 7), S::F_2_2, false, 0);
    for (t, size, trimmed) in [(Rotate90, (48, 75), (5, 0)), (Rotate270, (53, 64), (0, 11)), (FlipH, (64, 53), (11, 0)), (FlipV, (75, 48), (0, 5)), (Rotate180, (64, 48), (11, 5))] {
        let e = rotate(&src, t).unwrap();
        assert_eq!((e.stored, e.trimmed), (size, trimmed), "{t:?}");
        let j = decode::parse(&e.bytes).unwrap();
        assert_eq!((j.width, j.height), size, "{t:?}");
    }
    // Turning right drops the bottom rows (they would become a partial left column).
    let reference = rgba(&src);
    let top = image::imageops::crop_imm(&reference, 0, 0, 75, 48).to_image();
    let e = rotate(&src, Rotate90).unwrap();
    let (_, mean) = diff(&rgba(&e.bytes), &Rotate90.apply(&top), (0, 0), 2);
    assert!(mean <= 2.5, "{mean}");
    // Too small to keep anything.
    let tiny = jpeg(&picture(12, 10, 8), S::F_2_2, false, 0);
    assert!(rotate(&tiny, FlipH).is_err());
    assert!(rotate(&tiny, Rotate180).is_err());
}

#[test]
fn turning_applies_and_resets_the_orientation() {
    use Transform::*;
    let base = jpeg(&picture(64, 48, 9), S::F_1_1, false, 0);
    let src = with_segments(&base, &[exif_segment(&crate::encode::minimal_exif(6))]);
    let shown = crate::imaging::orient(rgba(&base), 6);
    let e = rotate(&src, Rotate90).unwrap();
    assert_eq!(probe(&e.bytes).unwrap().orientation, 1);
    let (worst, mean) = diff(&rgba(&e.bytes), &Rotate90.apply(&shown), (0, 0), 2);
    assert!(worst <= 6 && mean <= 0.5, "{worst} {mean}");
}

#[test]
fn files_without_an_orientation_tag_turn_for_real() {
    use Transform::*;
    // EXIF that has no orientation tag (and is too big to just replace).
    let mut tiff = b"II*\0\x08\0\0\0\x01\0\x0f\x01\x02\0\x40\0\0\0\x1a\0\0\0\0\0\0\0".to_vec();
    tiff.extend_from_slice(&[b'x'; 64]);
    let src = with_segments(&jpeg(&picture(64, 48, 10), S::F_2_1, false, 0), &[exif_segment(&tiff)]);
    let dir = std::env::temp_dir().join(format!("lumen-lossless-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("turn.jpg");
    std::fs::write(&path, &src).unwrap();
    let turn = crate::encode::lossless_jpeg_transform(&path, Rotate90).unwrap();
    assert!(turn.physical);
    assert_eq!(turn.trimmed, (0, 0));
    let after = std::fs::read(&path).unwrap();
    let j = decode::parse(&after).unwrap();
    assert_eq!((j.width, j.height), (48, 64));
    // The rest of the EXIF is kept.
    assert!(j.segments.iter().any(|(m, b)| *m == 0xE1 && b.windows(64).any(|w| w == [b'x'; 64])));
    // Lossless crop of the file in place, and the grid read from the file.
    let grid = probe_file(&path).unwrap();
    assert_eq!((grid.stored, grid.step, grid.orientation), ((48, 64), (8, 16), 1));
    let e = crop_file(&path, (10, 20, 30, 40)).unwrap();
    assert_eq!(e.kept, (8, 16, 32, 44));
    assert!(e.bytes.is_empty());
    assert_eq!(probe_file(&path).unwrap().stored, (32, 44));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crop_updates_the_metadata() {
    let src = jpeg(&picture(160, 120, 11), S::F_2_2, false, 0);
    let old_thumb = jpeg(&picture(32, 24, 12), S::F_2_2, false, 0);
    let icc1 = [b"ICC_PROFILE\0\x01\x02".as_slice(), [7u8; 100].as_slice()].concat();
    let icc2 = [b"ICC_PROFILE\0\x02\x02".as_slice(), [8u8; 50].as_slice()].concat();
    let xmp = [meta::XMP_HEADER, br#"<x:xmpmeta><rdf:Description hdrgm:Version="1.0" xmp:Rating="5" exif:PixelXDimension="160"/></x:xmpmeta>"#.as_slice()].concat();
    let mpf = b"MPF\0II*\0\x08\0\0\0\0\0".to_vec();
    let segs = [exif_segment(&meta::tests::sample_tiff(1, &old_thumb)), (0xE2, icc1.clone()), (0xE2, icc2.clone()), (0xE1, xmp), (0xE2, mpf), (0xFE, b"a comment".to_vec())];
    let mut file = with_segments(&src, &segs);
    // The gain map image after the end of the picture.
    file.extend_from_slice(&old_thumb);
    let e = crop(&file, (20, 20, 100, 70)).unwrap();
    assert!(e.gain_map_removed);
    assert_eq!(e.kept, (16, 16, 104, 74));
    let j = decode::parse(&e.bytes).unwrap();
    assert!(j.trailer.is_empty());
    let markers: Vec<u8> = j.segments.iter().map(|(m, _)| *m).collect();
    assert_eq!(markers, [0xE1, 0xE2, 0xE2, 0xE1, 0xFE, 0xE0]);
    assert_eq!(j.segments[1].1, icc1);
    assert_eq!(j.segments[2].1, icc2);
    assert_eq!(j.segments[4].1, b"a comment");
    let x = std::str::from_utf8(&j.segments[3].1).unwrap();
    assert!(!x.contains("hdrgm:") && x.contains(r#"xmp:Rating="5""#) && x.contains(r#"exif:PixelXDimension="104""#), "{x}");
    // EXIF: the new size and a thumbnail of the cropped picture.
    let tiff = j.segments[0].1[6..].to_vec();
    let ex = exif::Reader::new().read_raw(tiff.clone()).unwrap();
    let get = |tag, ifd| ex.get_field(tag, ifd).and_then(|f| f.value.get_uint(0)).unwrap();
    assert_eq!(get(exif::Tag::PixelXDimension, exif::In::PRIMARY), 104);
    assert_eq!(get(exif::Tag::PixelYDimension, exif::In::PRIMARY), 74);
    assert_eq!(get(exif::Tag::Orientation, exif::In::PRIMARY), 1);
    let (off, len) = (get(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL) as usize, get(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL) as usize);
    let thumb = rgba(&tiff[off..off + len]);
    assert_eq!(thumb.dimensions(), (13, 10));
    // About the colors of the cropped picture.
    let cropped = rgba(&e.bytes);
    let mean = |img: &RgbaImage, k: usize| img.pixels().map(|p| p.0[k] as f64).sum::<f64>() / (img.width() * img.height()) as f64;
    for k in 0..3 {
        assert!((mean(&thumb, k) - mean(&cropped, k)).abs() < 12.0, "channel {k}: {} vs {}", mean(&thumb, k), mean(&cropped, k));
    }
    // Without MPF or a gain map, data after the picture (a motion photo video) stays.
    let plain = [src.as_slice(), b"video bytes".as_slice()].concat();
    let e = crop(&plain, (20, 20, 100, 70)).unwrap();
    assert!(!e.gain_map_removed);
    assert!(e.bytes.ends_with(b"\xFF\xD9video bytes"));
}

#[test]
fn damaged_files_fail_cleanly() {
    let inputs = [BASE420.to_vec(), PROG420.to_vec(), PROG422.to_vec(), PROG_GRAY.to_vec(), jpeg(&picture(40, 24, 13), S::F_2_1, false, 2)];
    let mut seed = 0x2545_F491u32;
    for data in &inputs {
        // Cut short anywhere: an error (or, cut after the end marker, fine).
        for n in (0..data.len()).step_by(5) {
            assert!(decode::parse(&data[..n]).is_err(), "cut at {n}");
            let _ = probe(&data[..n]);
        }
        let err = decode::parse(&data[..data.len() * 2 / 3]).unwrap_err().to_string();
        assert!(err.contains("incomplete") || err.contains("damaged"), "{err}");
        // A few bytes changed anywhere: never a panic.
        for _ in 0..250 {
            let mut d = data.clone();
            for _ in 0..3 {
                let i = xorshift(&mut seed) as usize % d.len();
                d[i] = xorshift(&mut seed) as u8;
            }
            if let Ok(j) = decode::parse(&d) {
                let _ = encode::write(&j);
                let _ = crop(&d, (5, 5, 20, 10));
                let _ = rotate(&d, Transform::Rotate90);
            }
        }
    }
}

#[test]
fn unsupported_kinds_are_named() {
    let base = jpeg(&picture(16, 16, 14), S::F_1_1, false, 0);
    let sof = base.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    let with = |at: usize, b: &[u8]| {
        let mut d = base.clone();
        d[at..at + b.len()].copy_from_slice(b);
        d
    };
    let msg = |d: &[u8]| decode::parse(d).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(msg(&with(sof + 1, &[0xC9])).contains("arithmetic"));
    assert!(msg(&with(sof + 1, &[0xCA])).contains("arithmetic"));
    assert!(probe(&with(sof + 1, &[0xC9])).unwrap_err().to_string().contains("arithmetic"));
    assert!(msg(&with(sof + 1, &[0xC3])).contains("lossless"));
    assert!(msg(&with(sof + 4, &[12])).contains("12-bit"));
    // A huge frame over a few bytes of data: refused before allocating.
    assert!(msg(&with(sof + 5, &[0xFF, 0xFF, 0xFF, 0xFF])).contains("too large"));
    assert!(msg(&with(sof + 5, &[0x0F, 0xA0, 0x0F, 0xA0])).contains("damaged"));
    assert!(decode::parse(b"GIF89a").is_err());
    assert!(probe(&[]).is_err());
    // A frame without picture data.
    assert!(decode::parse(&[0xFF, 0xD8, 0xFF, 0xD9]).is_err());
}

#[test]
fn crop_grid_reports_the_block_size() {
    let img = picture(48, 32, 15);
    for (s, step) in [(S::F_1_1, (8, 8)), (S::F_2_1, (16, 8)), (S::F_2_2, (16, 16)), (S::F_1_2, (8, 16)), (S::F_4_1, (32, 8))] {
        assert_eq!(probe(&jpeg(&img, s, false, 0)).unwrap().step, step, "{s:?}");
    }
    // Gray pictures always have an 8 px grid.
    let gray = vec![128u8; 48 * 32];
    assert_eq!(probe(&encode_with(&gray, 48, 32, ColorType::Luma, S::F_2_2, false, 0)).unwrap().step, (8, 8));
    assert_eq!(probe(PROG_GRAY).unwrap().step, (8, 8));
}

/// Release-build speed check: `cargo test -p lumen-core --release -- --ignored speed --nocapture`.
#[test]
#[ignore]
fn speed() {
    let img = picture(6000, 4000, 16);
    let src = jpeg(&img, S::F_2_2, false, 0);
    let t = std::time::Instant::now();
    let j = decode::parse(&src).unwrap();
    let parsed = t.elapsed();
    let out = encode::write(&j).unwrap();
    eprintln!("24 MP 4:2:0, {} MB: parse {:?}, write {:?}", src.len() >> 20, parsed, t.elapsed() - parsed);
    assert!(!out.is_empty());
}
