//! Problem areas found from real metric maps of synthetic images.

mod common;

use common::*;
use lumen_analysis::metrics::MetricOptions;
use lumen_analysis::{find_regions, Heatmap, RegionKind};

const IDS: [&str; 5] = ["sharpness.laplacian", "sharpness.tenengrad", "blockiness", "banding", "exposure"];

/// Smooth shapes and fine noise, nothing aligned to an 8 px grid (the
/// shared `texture` has a 16 px checkerboard, which looks like blocking).
fn natural(w: usize, h: usize, seed: u64) -> Vec<f64> {
    let v: Vec<f64> = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f64, (i / w) as f64);
            90.0 + 60.0 * x / w as f64 + 35.0 * (x / 7.3).sin() * (y / 5.1).cos() + 25.0 * ((x + 2.0 * y) / 13.7).sin()
        })
        .collect();
    add_noise(&v, 3.0, seed)
}

fn regions_of(w: usize, h: usize, v: &[u8]) -> Vec<lumen_analysis::Region> {
    let f = gray_rgba(w, h, v);
    let o = MetricOptions::default();
    let maps: Vec<(&str, Heatmap)> = IDS.iter().filter_map(|id| run(id, &f.view(), None, &o).map.map(|m| (*id, m))).collect();
    let refs: Vec<(&str, &Heatmap)> = maps.iter().map(|(id, m)| (*id, m)).collect();
    find_regions(&refs, w as u32, h as u32)
}

#[test]
fn blurred_quadrant_is_one_blurry_area() {
    let (w, h) = (512, 512);
    let sharp = add_noise(&texture(w, h, 7), 2.0, 3);
    let soft = blur(&sharp, w, h, 3.0);
    // Bottom-right quadrant blurred.
    let v: Vec<f64> = (0..w * h).map(|i| if i % w >= w / 2 && i / w >= h / 2 { soft[i] } else { sharp[i] }).collect();
    let r = regions_of(w, h, &quantize(&v));
    let blurry: Vec<_> = r.iter().filter(|r| r.kind == RegionKind::Blurry).collect();
    assert_eq!(blurry.len(), 1, "{r:?}");
    let b = blurry[0].rect;
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    assert!(cx > 256.0 && cy > 256.0, "centre in the blurred quadrant: {b:?}");
    assert!(b[0] >= 192.0 && b[1] >= 192.0, "does not spill far out of it: {b:?}");
    assert!(blurry[0].area_pct > 12.0 && blurry[0].area_pct < 40.0, "{}", blurry[0].area_pct);
    // The sharp original has none.
    assert!(regions_of(w, h, &quantize(&sharp)).iter().all(|r| r.kind != RegionKind::Blurry));
}

#[test]
fn flat_and_clean_images_have_none() {
    let (w, h) = (320, 240);
    assert!(regions_of(w, h, &vec![128u8; w * h]).is_empty());
    let clean = quantize(&natural(w, h, 5));
    assert!(regions_of(w, h, &clean).is_empty(), "{:?}", regions_of(w, h, &clean));
}

#[test]
fn block_averaged_corner_is_blocky() {
    let (w, h) = (512, 384);
    let mut v = natural(w, h, 9);
    // Top-left 192 x 192: each 8 x 8 block replaced by its mean (heavy compression).
    for by in 0..24 {
        for bx in 0..24 {
            let idx: Vec<usize> = (0..64).map(|k| (by * 8 + k / 8) * w + bx * 8 + k % 8).collect();
            let m = idx.iter().map(|&i| v[i]).sum::<f64>() / 64.0;
            for i in idx {
                v[i] = m;
            }
        }
    }
    let r = regions_of(w, h, &quantize(&v));
    let blocky: Vec<_> = r.iter().filter(|r| r.kind == RegionKind::Blocky).collect();
    assert_eq!(blocky.len(), 1, "{r:?}");
    assert!(blocky[0].rect[2] <= 256.0 && blocky[0].rect[3] <= 256.0, "{:?}", blocky[0].rect);
}
