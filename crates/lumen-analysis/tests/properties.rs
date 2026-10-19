//! Property tests: identity, monotonicity in noise and blur, ROI equals
//! crop-then-compute, 4:2:0 chroma mapping, and behavior checks of the
//! no-reference diagnostics on constructed inputs.

mod common;

use common::*;
use lumen_analysis::color;
use lumen_analysis::frame::{FrameView, OwnedFrame, PlaneView, Roi};
use lumen_analysis::metrics::{self, MetricOptions, PSNR_CAP_DB};

const W: usize = 256;
const H: usize = 192;

fn color_texture(seed: u64) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let base = texture(W, H, seed);
    let g: Vec<f64> = base.iter().enumerate().map(|(i, v)| v * 0.8 + 20.0 * ((i % W) as f64 / 13.0).sin()).collect();
    let b: Vec<f64> = base.iter().enumerate().map(|(i, v)| 255.0 - v * 0.6 + 15.0 * ((i / W) as f64 / 9.0).cos()).collect();
    (base, g, b)
}

fn rgba_of(c: &(Vec<f64>, Vec<f64>, Vec<f64>)) -> OwnedFrame {
    rgba(W, H, &quantize(&c.0), &quantize(&c.1), &quantize(&c.2))
}

fn map3(c: &(Vec<f64>, Vec<f64>, Vec<f64>), f: impl Fn(&[f64], u64) -> Vec<f64>) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    (f(&c.0, 1), f(&c.1, 2), f(&c.2, 3))
}

#[test]
fn identical_inputs() {
    let o = MetricOptions::default();
    let img = rgba_of(&color_texture(1));
    let yuv = yuv420(W, H, quantize(&texture(W, H, 2)), |x, y| ((x * 3 % 256) as u8, (y * 5 % 256) as u8));
    for f in [&img, &yuv] {
        let v = f.view();
        let p = run("psnr", &v, Some(&v), &o);
        for val in &p.values {
            if val.key.starts_with("mse") || val.key.starts_with("maxabs") {
                assert_eq!(val.value, 0.0);
            } else {
                assert_eq!(val.value, PSNR_CAP_DB, "{}", val.key);
            }
        }
        assert_eq!(val(&run("psnr.rgb", &v, Some(&v), &o), "avg"), PSNR_CAP_DB);
        assert_eq!(val(&run("ssim.fast", &v, Some(&v), &o), "y"), 1.0);
        assert!((val(&run("ssim.gauss", &v, Some(&v), &o), "y") - 1.0).abs() < 1e-6);
        assert!((val(&run("ms_ssim", &v, Some(&v), &o), "y") - 1.0).abs() < 1e-6);
        for id in ["deltae.76", "deltae.94", "deltae.2000"] {
            let r = run(id, &v, Some(&v), &o);
            assert_eq!(val(&r, "mean"), 0.0, "{id}");
            assert_eq!(val(&r, "max"), 0.0, "{id}");
            assert!(r.map.as_ref().unwrap().data.iter().all(|x| *x == 0.0));
        }
        let d = run("diff.abs", &v, Some(&v), &o);
        assert_eq!((val(&d, "max"), val(&d, "changed_pct")), (0.0, 0.0));
        assert_eq!(val(&run("diff.signed", &v, Some(&v), &o), "mean"), 0.0);
    }
}

#[test]
fn known_psnr_value() {
    let a = gray_rgba(32, 32, &[100; 1024]);
    let b = gray_rgba(32, 32, &[105; 1024]);
    let p = run("psnr", &a.view(), Some(&b.view()), &MetricOptions::default());
    assert!((val(&p, "r") - 34.1514).abs() < 1e-3);
    assert_eq!(val(&p, "mse_avg"), 25.0);
    assert_eq!(val(&p, "maxabs_g"), 5.0);
}

/// Values of `id.key` for each distortion level; asserts strict monotonicity.
fn assert_monotonic(vals: &[f64], decreasing: bool, what: &str) {
    for w in vals.windows(2) {
        if decreasing {
            assert!(w[1] < w[0], "{what} not decreasing: {vals:?}");
        } else {
            assert!(w[1] > w[0], "{what} not increasing: {vals:?}");
        }
    }
}

#[test]
fn monotonic_in_noise() {
    let o = MetricOptions::default();
    let c = color_texture(3);
    let a = rgba_of(&c);
    let sigmas = [1.0, 3.0, 6.0, 12.0, 24.0];
    let mut rows: Vec<Vec<f64>> = Vec::new();
    for &s in &sigmas {
        let b = rgba_of(&map3(&c, |v, k| add_noise(v, s, 50 + k)));
        let (va, vb) = (a.view(), b.view());
        let nr = run("noise", &vb, None, &MetricOptions { noise_mask_edges: false, ..o.clone() });
        rows.push(vec![
            val(&run("psnr", &va, Some(&vb), &o), "avg"),
            val(&run("ssim.fast", &va, Some(&vb), &o), "y"),
            val(&run("ssim.gauss", &va, Some(&vb), &o), "y"),
            val(&run("ms_ssim", &va, Some(&vb), &o), "y"),
            val(&run("deltae.2000", &va, Some(&vb), &o), "mean"),
            val(&run("deltae.76", &va, Some(&vb), &o), "p95"),
            val(&nr, "y"),
        ]);
    }
    let col = |i: usize| rows.iter().map(|r| r[i]).collect::<Vec<_>>();
    assert_monotonic(&col(0), true, "psnr");
    assert_monotonic(&col(1), true, "ssim.fast");
    assert_monotonic(&col(2), true, "ssim.gauss");
    assert_monotonic(&col(3), true, "ms_ssim");
    assert_monotonic(&col(4), false, "deltae.2000");
    assert_monotonic(&col(5), false, "deltae.76 p95");
    assert_monotonic(&col(6), false, "noise");
}

#[test]
fn monotonic_in_blur() {
    let o = MetricOptions::default();
    let t = texture(W, H, 4);
    let a = gray_rgba(W, H, &quantize(&t));
    let mut rows: Vec<Vec<f64>> = Vec::new();
    for &s in &[0.6, 1.0, 1.6, 2.5, 4.0] {
        let b = gray_rgba(W, H, &quantize(&blur(&t, W, H, s)));
        let (va, vb) = (a.view(), b.view());
        rows.push(vec![
            val(&run("ssim.gauss", &va, Some(&vb), &o), "y"),
            val(&run("ms_ssim", &va, Some(&vb), &o), "y"),
            val(&run("sharpness.laplacian", &vb, None, &o), "var"),
            val(&run("sharpness.tenengrad", &vb, None, &o), "mean_g2"),
        ]);
    }
    let col = |i: usize| rows.iter().map(|r| r[i]).collect::<Vec<_>>();
    assert_monotonic(&col(0), true, "ssim.gauss");
    assert_monotonic(&col(1), true, "ms_ssim");
    assert_monotonic(&col(2), true, "laplacian");
    assert_monotonic(&col(3), true, "tenengrad");
}

#[test]
fn immerkaer_estimates_known_sigma() {
    let (w, h) = (400, 300);
    for sigma in [2.0, 5.0, 10.0] {
        let flat = vec![128.0; w * h];
        let img = gray_rgba(w, h, &quantize(&add_noise(&flat, sigma, 9)));
        let r = run("noise", &img.view(), None, &MetricOptions { noise_mask_edges: false, ..MetricOptions::default() });
        let est = val(&r, "y");
        assert!((est - sigma).abs() < 0.08 * sigma + 0.15, "sigma {sigma}: {est}");
        // Gray input: chroma is noise free.
        assert!(val(&r, "cb") < 0.05);
        let masked = val(&run("noise", &img.view(), None, &MetricOptions::default()), "y");
        assert!((masked - sigma).abs() < 0.3 * sigma, "masked sigma {sigma}: {masked}");
    }
}

/// Packed copy of a crop (the "crop then compute" side).
fn crop_copy(v: &FrameView<'_>, r: Roi) -> (OwnedFrame, Roi) {
    let (c, used) = v.crop(r).unwrap();
    (OwnedFrame::from_view(&c), used)
}

#[test]
fn roi_equals_crop_then_compute() {
    let o = MetricOptions::default();
    let (w, h) = (320, 240);
    let t = texture(w, h, 5);
    let n = add_noise(&t, 6.0, 11);
    let rgb_a = gray_rgba(w, h, &quantize(&t));
    let rgb_b = rgba(w, h, &quantize(&n), &quantize(&t), &quantize(&blur(&t, w, h, 1.0)));
    let yuv_a = yuv420(w, h, quantize(&t), |x, y| ((x * 7 + y) as u8, (y * 3) as u8));
    let yuv_b = yuv420(w, h, quantize(&n), |x, y| ((x * 7 + y + 1) as u8, (y * 3 + (x % 3)) as u8));
    let ids: Vec<&str> = metrics::all().iter().map(|m| m.id()).collect();
    for (fa, fb, roi) in [(&rgb_a, &rgb_b, Roi::new(13, 7, 201, 181)), (&yuv_a, &yuv_b, Roi::new(17, 9, 199, 183))] {
        let (va, vb) = (fa.view(), fb.view());
        let (ca, used) = crop_copy(&va, roi);
        let (cb, _) = crop_copy(&vb, used);
        let (sa, _) = va.crop(roi).unwrap();
        let (sb, _) = vb.crop(used).unwrap();
        for id in &ids {
            let m = metrics::get(id).unwrap();
            let fr = m.info().kind == metrics::MetricKind::FullReference;
            let strided = run(id, &sa, fr.then_some(&sb), &o);
            let packed = run(id, &ca.view(), fr.then_some(&cb.view()), &o);
            assert_eq!(strided.values.len(), packed.values.len(), "{id}");
            for (x, y) in strided.values.iter().zip(&packed.values) {
                assert!(x.value == y.value || (x.value - y.value).abs() <= 1e-9 * x.value.abs().max(1.0), "{id}.{}: {} vs {}", x.key, x.value, y.value);
            }
            if let (Some(p), Some(q)) = (&strided.map, &packed.map) {
                assert_eq!(p.data, q.data, "{id} map");
            }
        }
    }
}

#[test]
fn roi_through_image_job_places_map() {
    use lumen_analysis::job::{run_image, AnalysisPool, ImageSpec, JobControl};
    let (w, h) = (200, 150);
    let t = texture(w, h, 6);
    let a = gray_rgba(w, h, &quantize(&t));
    let b = gray_rgba(w, h, &quantize(&add_noise(&t, 4.0, 1)));
    let pool = AnalysisPool::new(Some(2));
    let spec = ImageSpec { metrics: vec!["ssim.gauss".into(), "psnr".into()], roi: Some(Roi::new(31, 20, 100, 90)), ..Default::default() };
    let rep = run_image(&pool, &spec, &a.view(), Some(&b.view()), &JobControl::default(), &|_| {}).unwrap();
    let m = rep.map("ssim.gauss").unwrap();
    assert_eq!((m.x0, m.y0), (36.0, 25.0));
    assert_eq!((m.width, m.height), (90, 80));
    assert_eq!(rep.scope.roi, Some(Roi::new(31, 20, 100, 90)));
    let (ca, _) = crop_copy(&a.view(), Roi::new(31, 20, 100, 90));
    let (cb, _) = crop_copy(&b.view(), Roi::new(31, 20, 100, 90));
    let direct = run("ssim.gauss", &ca.view(), Some(&cb.view()), &MetricOptions::default());
    assert_eq!(rep.value("ssim.gauss.y"), Some(val(&direct, "y")));
}

#[test]
fn chroma_roi_mapping_420_odd_offsets() {
    // Chroma samples encode their own coordinates.
    let (w, h) = (9, 11);
    let f = yuv420(w, h, (0..w * h).map(|i| i as u8).collect(), |x, y| (x as u8, y as u8));
    let v = f.view();
    let check = |roi: Roi, want: Roi, cx: u8, cy: u8, cw: usize, ch: usize| {
        let (c, used) = v.crop(roi).unwrap();
        assert_eq!(used, want, "roi {roi:?}");
        let FrameView::Yuv(y) = c else { panic!() };
        assert_eq!((y.y.width(), y.y.height()), (want.w as usize, want.h as usize));
        let PlaneView::U8(yp) = y.y else { panic!() };
        assert_eq!(yp.row(0)[0], (want.y as usize * w + want.x as usize) as u8, "luma origin");
        let (PlaneView::U8(u), PlaneView::U8(q)) = y.uv.unwrap() else { panic!() };
        assert_eq!((u.width, u.height), (cw, ch), "chroma size for {roi:?}");
        assert_eq!(u.row(0)[0], cx);
        assert_eq!(q.row(0)[0], cy);
        assert_eq!(u.row(ch - 1)[cw - 1], cx + cw as u8 - 1);
        assert_eq!(q.row(ch - 1)[cw - 1], cy + ch as u8 - 1);
    };
    check(Roi::new(3, 5, 4, 3), Roi::new(2, 4, 6, 4), 1, 2, 3, 2);
    check(Roi::new(1, 1, 1, 1), Roi::new(0, 0, 2, 2), 0, 0, 1, 1);
    // Right / bottom edges of an odd-sized frame stay at the border.
    check(Roi::new(7, 9, 2, 2), Roi::new(6, 8, 3, 3), 3, 4, 2, 2);
    check(Roi::new(0, 0, 9, 11), Roi::new(0, 0, 9, 11), 0, 0, 5, 6);
    // Out of frame is an error, partial overlap is clamped.
    assert!(v.crop(Roi::new(20, 20, 4, 4)).is_err());
    assert_eq!(v.crop(Roi::new(8, 10, 10, 10)).unwrap().1, Roi::new(8, 10, 1, 1));
}

#[test]
fn deltae_map_matches_reference_formula() {
    let mut rng = Rng::new(77);
    let (w, h) = (40, 30);
    let mut pa = Vec::new();
    let mut pb = Vec::new();
    for _ in 0..w * h {
        let c: [u8; 3] = std::array::from_fn(|_| (rng.uniform() * 255.0) as u8);
        let d: [u8; 3] = std::array::from_fn(|k| (c[k] as f64 + rng.normal() * 12.0).clamp(0.0, 255.0) as u8);
        pa.push(c);
        pb.push(d);
    }
    let split = |p: &[[u8; 3]], k: usize| p.iter().map(|c| c[k]).collect::<Vec<u8>>();
    let a = rgba(w, h, &split(&pa, 0), &split(&pa, 1), &split(&pa, 2));
    let b = rgba(w, h, &split(&pb, 0), &split(&pb, 1), &split(&pb, 2));
    let o = MetricOptions::default();
    let r00 = run("deltae.2000", &a.view(), Some(&b.view()), &o);
    let r94 = run("deltae.94", &a.view(), Some(&b.view()), &o);
    let r76 = run("deltae.76", &a.view(), Some(&b.view()), &o);
    let (m00, m94, m76) = (r00.map.clone().unwrap(), r94.map.unwrap(), r76.map.unwrap());
    let mut sum = 0.0;
    for i in 0..w * h {
        let (la, lb) = (color::srgb8_to_lab(pa[i]), color::srgb8_to_lab(pb[i]));
        let d = color::delta_e2000(la, lb);
        sum += d;
        assert!((m00.data[i] as f64 - d).abs() < 2e-3, "px {i}: {} vs {d}", m00.data[i]);
        assert!((m94.data[i] as f64 - color::delta_e94(la, lb)).abs() < 2e-3);
        assert!((m76.data[i] as f64 - color::delta_e76(la, lb)).abs() < 2e-3);
    }
    assert!((val(&r00, "mean") - sum / (w * h) as f64).abs() < 1e-3);
    assert!(val(&r00, "p95") <= val(&r00, "max") && val(&r00, "median") <= val(&r00, "p95"));
}

#[test]
fn exposure_clipping_zones_and_range() {
    let (w, h) = (64, 64);
    let half: Vec<u8> = (0..w * h).map(|i| if i % w < w / 2 { 0 } else { 255 }).collect();
    let r = run("exposure", &gray_rgba(w, h, &half).view(), None, &MetricOptions::default());
    assert!((val(&r, "clip_lo_pct") - 50.0).abs() < 1e-9);
    assert!((val(&r, "clip_hi_pct") - 50.0).abs() < 1e-9);
    assert!((val(&r, "zone_0") - 50.0).abs() < 1e-6 && (val(&r, "zone_10") - 50.0).abs() < 1e-6);
    assert!((val(&r, "dr_stops") - 14.0).abs() < 1e-3, "{}", val(&r, "dr_stops"));
    // sRGB 118 is about 18% gray, L* 49.6: all in zone V.
    let r = run("exposure", &gray_rgba(w, h, &vec![118; w * h]).view(), None, &MetricOptions::default());
    assert!((val(&r, "l_mean") - 49.6).abs() < 0.3, "{}", val(&r, "l_mean"));
    assert!((val(&r, "zone_5") - 100.0).abs() < 1e-6);
    assert_eq!(val(&r, "clip_hi_pct"), 0.0);
    assert!(val(&r, "dr_stops").abs() < 0.01);
    // Limited-range YUV: Y 16 is black, 235 white.
    let y: Vec<u8> = (0..w * h).map(|i| if i < w * h / 4 { 16 } else if i < w * h / 2 { 235 } else { 128 }).collect();
    let r = run("exposure", &yuv420(w, h, y, |_, _| (128, 128)).view(), None, &MetricOptions::default());
    assert!((val(&r, "clip_lo_pct") - 25.0).abs() < 1e-9 && (val(&r, "clip_hi_pct") - 25.0).abs() < 1e-9);
    assert!(val(&r, "l_p0_5") < 0.5 && val(&r, "l_p99_5") > 99.5);
}

#[test]
fn color_cast_direction() {
    let (w, h) = (48, 48);
    let o = MetricOptions::default();
    let gray = rgba(w, h, &vec![128; w * h], &vec![128; w * h], &vec![128; w * h]);
    assert!(val(&run("color_cast", &gray.view(), None, &o), "delta_e") < 0.01);
    // Mild warm tint: still near neutral, measured on those pixels.
    let warm = rgba(w, h, &vec![136; w * h], &vec![128; w * h], &vec![118; w * h]);
    let r = run("color_cast", &warm.view(), None, &o);
    assert!(val(&r, "b") > 3.0 && val(&r, "a") > 0.0, "{:?}", r.values);
    assert!(val(&r, "hue") > 0.0 && val(&r, "hue") < 90.0);
    assert_eq!(val(&r, "neutral_pct"), 100.0);
    assert!(r.notes[0].contains("near-neutral"), "{}", r.notes[0]);
    // Strong blue: no neutral pixels left, gray-world fallback.
    let blue = rgba(w, h, &vec![100; w * h], &vec![125; w * h], &vec![170; w * h]);
    let r = run("color_cast", &blue.view(), None, &o);
    assert!(val(&r, "b") < -10.0, "{:?}", r.values);
    assert!(r.notes[0].contains("cool") || r.notes[0].contains("blue"), "{}", r.notes[0]);
    assert!(r.notes[0].contains("gray-world"), "{}", r.notes[0]);
}

#[test]
fn blockiness_detects_grid_and_offset() {
    let (w, h) = (256, 192);
    // Smooth texture with no 8-periodic structure.
    let mut rng = Rng::new(8);
    let t: Vec<f64> = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f64, (i / w) as f64);
            128.0 + 50.0 * (x / 9.7).sin() * (y / 6.3).cos() + 30.0 * ((x + y) / 23.1).sin() + 2.0 * rng.normal()
        })
        .collect();
    let o = MetricOptions::default();
    let clean = run("blockiness", &gray_rgba(w, h, &quantize(&t)).view(), None, &o);
    // 8x8 block means on a grid shifted by (3, 5).
    let mut b = t.clone();
    for by in 0..h / 8 + 1 {
        for bx in 0..w / 8 + 1 {
            let xs: Vec<usize> = (0..8).map(|i| bx * 8 + i + 3).filter(|x| *x >= 3 && *x < w).collect();
            let ys: Vec<usize> = (0..8).map(|i| by * 8 + i + 5).filter(|y| *y >= 5 && *y < h).collect();
            if xs.is_empty() || ys.is_empty() {
                continue;
            }
            let t = &t;
            let m: f64 = ys.iter().flat_map(|y| xs.iter().map(move |x| t[y * w + x])).sum::<f64>() / (xs.len() * ys.len()) as f64;
            for y in &ys {
                for x in &xs {
                    b[y * w + x] = 0.3 * t[y * w + x] + 0.7 * m;
                }
            }
        }
    }
    let blocky = run("blockiness", &gray_rgba(w, h, &quantize(&b)).view(), None, &o);
    assert!(val(&clean, "score") < 0.05, "clean {:?}", clean.values);
    assert!(val(&blocky, "score") > 0.5 + val(&clean, "score"), "blocky {:?}", blocky.values);
    assert_eq!(val(&blocky, "offset_x"), 3.0);
    assert_eq!(val(&blocky, "offset_y"), 5.0);
    let m = blocky.map.unwrap();
    assert_eq!((m.width, m.height), (4, 3));
}

#[test]
fn banding_posterized_vs_dithered() {
    let (w, h) = (640, 64);
    // A smooth ramp of 40 levels: posterized (rounded) vs dithered.
    let ramp: Vec<f64> = (0..w * h).map(|i| 100.0 + 40.0 * (i % w) as f64 / w as f64).collect();
    let o = MetricOptions::default();
    let banded = run("banding", &gray_rgba(w, h, &quantize(&ramp)).view(), None, &o);
    let dithered = run("banding", &gray_rgba(w, h, &quantize(&add_noise(&ramp, 1.5, 4))).view(), None, &o);
    let textured = run("banding", &gray_rgba(w, h, &quantize(&texture(w, h, 3))).view(), None, &o);
    assert!(val(&banded, "score") > 1.0, "{:?}", banded.values);
    assert!(val(&dithered, "score") < 0.2 * val(&banded, "score"), "{:?}", dithered.values);
    assert!(val(&textured, "score") < 0.2 * val(&banded, "score"), "{:?}", textured.values);
    assert!(banded.map.unwrap().data.iter().filter(|v| **v > 0.0).count() > 30 * (h - 6));
}

#[test]
fn sharpness_tile_maps_cover_frame() {
    let (w, h) = (200, 130);
    let img = gray_rgba(w, h, &quantize(&texture(w, h, 2)));
    let r = run("sharpness.laplacian", &img.view(), None, &MetricOptions::default());
    let m = r.map.unwrap();
    assert_eq!((m.width, m.height, m.cell_w), (4, 3, 64.0));
    assert!(m.data.iter().all(|v| v.is_finite() && *v > 0.0));
}

#[test]
fn psnr_rgb_on_yuv_converts() {
    let (w, h) = (64, 48);
    let a = yuv420(w, h, quantize(&texture(w, h, 1)), |_, _| (128, 128));
    let b = yuv420(w, h, quantize(&add_noise(&texture(w, h, 1), 3.0, 2)), |_, _| (128, 128));
    let r = run("psnr.rgb", &a.view(), Some(&b.view()), &MetricOptions::default());
    // Neutral chroma: R = G = B, so the three channels agree.
    assert!((val(&r, "r") - val(&r, "g")).abs() < 1e-3 && (val(&r, "r") - val(&r, "b")).abs() < 1e-3);
    assert!(val(&r, "avg") > 30.0 && val(&r, "avg") < 60.0);
    // Mismatched kinds are refused by the native PSNR.
    let rgb = gray_rgba(w, h, &[0; 64 * 48]);
    let m = metrics::get("psnr").unwrap();
    let (ca, cb) = (lumen_analysis::FrameCtx::new(a.view()), lumen_analysis::FrameCtx::new(rgb.view()));
    assert!(m.compute(&ca, Some(&cb), &MetricOptions::default(), &lumen_analysis::CancelToken::new()).is_err());
}

#[test]
fn registry_is_consistent() {
    let mut ids = std::collections::HashSet::new();
    for m in metrics::all() {
        let i = m.info();
        assert!(ids.insert(i.id), "duplicate id {}", i.id);
        assert!(i.version >= 1 && !i.inputs.is_empty() && i.cost_ms_4k > 0.0);
        assert!(i.cost_ms(1920, 1080) < i.cost_ms_4k);
    }
    for t in metrics::temporal::all() {
        assert!(ids.insert(t.id));
    }
    for p in ["quick", "encode_qa", "photo_technical"] {
        for id in metrics::preset(p) {
            assert!(ids.contains(id), "preset {p} names unknown {id}");
        }
    }
}

#[cfg(all(feature = "ssimulacra2", feature = "butteraugli", feature = "flip"))]
#[test]
fn perceptual_v2_identity_and_monotonic() {
    let o = MetricOptions::default();
    let c = color_texture(12);
    let a = rgba_of(&c);
    let v = a.view();
    let s = run("ssimulacra2", &v, Some(&v), &o);
    assert!(val(&s, "score") > 99.9, "{:?}", s.values);
    let b = run("butteraugli", &v, Some(&v), &o);
    assert!(val(&b, "score") < 1e-6);
    let f = run("flip", &v, Some(&v), &o);
    assert_eq!(val(&f, "mean"), 0.0);
    let mut rows = Vec::new();
    for sigma in [2.0, 6.0, 15.0] {
        let d = rgba_of(&map3(&c, |p, k| add_noise(p, sigma, 70 + k)));
        let dv = d.view();
        let bu = run("butteraugli", &v, Some(&dv), &o);
        let fl = run("flip", &v, Some(&dv), &o);
        assert_eq!(bu.map.as_ref().map(|m| (m.width, m.height)), Some((W as u32, H as u32)));
        assert_eq!(fl.map.as_ref().map(|m| m.data.len()), Some(W * H));
        rows.push([val(&run("ssimulacra2", &v, Some(&dv), &o), "score"), val(&bu, "score"), val(&fl, "mean")]);
    }
    let col = |i: usize| rows.iter().map(|r| r[i]).collect::<Vec<_>>();
    assert_monotonic(&col(0), true, "ssimulacra2");
    assert_monotonic(&col(1), false, "butteraugli");
    assert_monotonic(&col(2), false, "flip");
    // YUV input goes through the derived linear RGB.
    let y = yuv420(W, H, quantize(&c.0), |_, _| (128, 128));
    let yv = y.view();
    assert!(val(&run("ssimulacra2", &yv, Some(&yv), &o), "score") > 99.9);
}

#[test]
fn every_metric_has_a_deliberate_headline() {
    // A blue-only shift: R and G are identical, so "the first value" (R)
    // would read 100 dB. The headline must be the average.
    let c = color_texture(7);
    let a = rgba_of(&c);
    let shifted = (c.0.clone(), c.1.clone(), c.2.iter().map(|v| (v + 30.0).min(255.0)).collect());
    let b = rgba_of(&shifted);
    let o = MetricOptions::default();
    let psnr = run("psnr", &a.view(), Some(&b.view()), &o);
    let head = psnr.main().unwrap();
    assert_eq!(head.key, "avg");
    assert!(head.value < 40.0, "{}", head.value);
    assert_eq!(val(&psnr, "r"), PSNR_CAP_DB);
    assert_eq!(run("psnr.rgb", &a.view(), Some(&b.view()), &o).main().unwrap().key, "avg");
    // YUV: luma.
    let ya = yuv420(W, H, quantize(&c.0), |_, _| (128, 128));
    let yb = yuv420(W, H, quantize(&c.1), |_, _| (128, 128));
    assert_eq!(run("psnr", &ya.view(), Some(&yb.view()), &o).main().unwrap().key, "y");
    // Exposure: the worse of both clipping sides, whichever it is.
    let bright: Vec<u8> = (0..W * H).map(|i| if i % 4 == 0 { 255 } else { 128 }).collect();
    let r = run("exposure", &gray_rgba(W, H, &bright).view(), None, &o);
    let head = r.main().unwrap();
    assert_eq!(head.key, "clip_pct");
    assert!((head.value - 25.0).abs() < 1e-9 && val(&r, "clip_lo_pct") == 0.0);
    // Every registered metric names a headline its results contain.
    for m in metrics::all() {
        let i = m.info();
        assert!(!i.headline.is_empty(), "{} has no headline", i.id);
        let bv = b.view();
        let r = run(i.id, &a.view(), (i.kind == metrics::MetricKind::FullReference).then_some(&bv), &o);
        let k = r.main().unwrap_or_else(|| panic!("{} has no values", i.id)).key.clone();
        assert!(i.headline.contains(&k.as_str()), "{}: headline {k} not declared", i.id);
        assert_eq!(metrics::headline_key(i.id, r.values.iter().map(|v| v.key.as_str())), Some(k.as_str()));
    }
    for t in metrics::temporal::all() {
        assert!(!t.headline.is_empty(), "{} has no headline", t.id);
    }
}
