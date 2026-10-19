//! PSNR and fast SSIM must match lumen-core's original video metrics
//! (`crates/lumen-core/src/video/metrics.rs` before the port). The original
//! per-sample implementation is copied here verbatim as the oracle.

mod common;

use common::Rng;
use lumen_analysis::frame::{OwnedFrame, OwnedPlane};
use lumen_analysis::metrics::{MetricOptions, PSNR_CAP_DB};

mod legacy {
    #[derive(Clone)]
    pub enum PlaneData {
        U8(Vec<u8>),
        U16(Vec<u16>),
    }

    #[derive(Clone)]
    pub struct Plane {
        pub width: usize,
        pub height: usize,
        pub data: PlaneData,
    }

    impl Plane {
        pub fn len(&self) -> usize {
            self.width * self.height
        }
        pub fn get(&self, i: usize) -> u32 {
            match &self.data {
                PlaneData::U8(v) => v[i] as u32,
                PlaneData::U16(v) => v[i] as u32,
            }
        }
    }

    pub fn mse_plane(a: &Plane, b: &Plane) -> f64 {
        let mut acc = 0u64;
        for i in 0..a.len() {
            let d = a.get(i) as i64 - b.get(i) as i64;
            acc += (d * d) as u64;
        }
        acc as f64 / a.len().max(1) as f64
    }

    pub fn psnr_from_mse(mse: f64, max: f64) -> f64 {
        if mse <= 1e-12 {
            f64::INFINITY
        } else {
            10.0 * (max * max / mse).log10()
        }
    }

    pub fn ssim_plane(a: &Plane, b: &Plane, bit_depth: u32) -> f64 {
        let w = a.width;
        let h = a.height;
        let bw = w / 4;
        let bh = h / 4;
        if bw < 2 || bh < 2 {
            return if mse_plane(a, b) == 0.0 { 1.0 } else { 0.0 };
        }
        let max = ((1u64 << bit_depth) - 1) as f64;
        let mut sums: Vec<[f64; 4]> = Vec::new();
        for by in 0..bh {
            for bx in 0..bw {
                let mut s = [0f64; 4];
                for y in 0..4 {
                    let row = (by * 4 + y) * w + bx * 4;
                    for x in 0..4 {
                        let p = a.get(row + x) as f64;
                        let q = b.get(row + x) as f64;
                        s[0] += p;
                        s[1] += q;
                        s[2] += p * p + q * q;
                        s[3] += p * q;
                    }
                }
                sums.push(s);
            }
        }
        let c1 = (0.01 * max).powi(2) * 64.0;
        let c2 = (0.03 * max).powi(2) * 64.0 * 63.0;
        let mut total = 0.0;
        for by in 0..bh - 1 {
            for bx in 0..bw - 1 {
                let mut s = [0f64; 4];
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let b4 = &sums[(by + dy) * bw + bx + dx];
                    for k in 0..4 {
                        s[k] += b4[k];
                    }
                }
                let (s1, s2, ss, s12) = (s[0], s[1], s[2], s[3]);
                let vars = ss * 64.0 - s1 * s1 - s2 * s2;
                let covar = s12 * 64.0 - s1 * s2;
                total += (2.0 * s1 * s2 + c1) * (2.0 * covar + c2) / ((s1 * s1 + s2 * s2 + c1) * (vars + c2));
            }
        }
        total / ((bw - 1) * (bh - 1)) as f64
    }
}

/// A pair of frames in both representations.
fn pair(w: usize, h: usize, depth: u32, noise: f64, seed: u64) -> (Vec<legacy::Plane>, Vec<legacy::Plane>, OwnedFrame, OwnedFrame) {
    let mut rng = Rng::new(seed);
    let max = ((1u32 << depth) - 1) as f64;
    let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
    let (mut la, mut lb, mut oa, mut ob) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (p, &(pw, ph)) in dims.iter().enumerate() {
        let mut a = Vec::with_capacity(pw * ph);
        let mut b = Vec::with_capacity(pw * ph);
        for y in 0..ph {
            for x in 0..pw {
                let base = (0.5 + 0.35 * ((x as f64 / 7.0 + p as f64).sin() * (y as f64 / 5.0).cos())) * max;
                let va = (base + 0.05 * max * rng.normal()).clamp(0.0, max).round();
                let vb = (va + noise * max * rng.normal()).clamp(0.0, max).round();
                a.push(va as u32);
                b.push(vb as u32);
            }
        }
        if depth > 8 {
            let (a16, b16): (Vec<u16>, Vec<u16>) = (a.iter().map(|v| *v as u16).collect(), b.iter().map(|v| *v as u16).collect());
            la.push(legacy::Plane { width: pw, height: ph, data: legacy::PlaneData::U16(a16.clone()) });
            lb.push(legacy::Plane { width: pw, height: ph, data: legacy::PlaneData::U16(b16.clone()) });
            oa.push(OwnedPlane::U16 { data: a16, width: pw, height: ph });
            ob.push(OwnedPlane::U16 { data: b16, width: pw, height: ph });
        } else {
            let (a8, b8): (Vec<u8>, Vec<u8>) = (a.iter().map(|v| *v as u8).collect(), b.iter().map(|v| *v as u8).collect());
            la.push(legacy::Plane { width: pw, height: ph, data: legacy::PlaneData::U8(a8.clone()) });
            lb.push(legacy::Plane { width: pw, height: ph, data: legacy::PlaneData::U8(b8.clone()) });
            oa.push(OwnedPlane::U8 { data: a8, width: pw, height: ph });
            ob.push(OwnedPlane::U8 { data: b8, width: pw, height: ph });
        }
    }
    (la, lb, OwnedFrame::yuv(oa, depth), OwnedFrame::yuv(ob, depth))
}

#[test]
fn psnr_and_fast_ssim_match_lumen_core() {
    let o = MetricOptions::default();
    let cases = [(64, 48, 8, 0.01), (67, 45, 8, 0.02), (7, 9, 8, 0.03), (1920, 1080, 8, 0.004), (333, 211, 10, 0.01), (130, 66, 12, 0.002), (96, 64, 16, 0.01)];
    for (i, &(w, h, depth, noise)) in cases.iter().enumerate() {
        let (la, lb, oa, ob) = pair(w, h, depth, noise, 100 + i as u64);
        let (va, vb) = (oa.view(), ob.view());
        let max = ((1u64 << depth) - 1) as f64;
        let p = common::run("psnr", &va, Some(&vb), &o);
        let mses: Vec<f64> = la.iter().zip(&lb).map(|(a, b)| legacy::mse_plane(a, b)).collect();
        for (k, name) in ["y", "u", "v"].iter().enumerate() {
            let want = legacy::psnr_from_mse(mses[k], max).min(PSNR_CAP_DB);
            let got = p.get(name).unwrap();
            assert!((got - want).abs() < 1e-9, "case {i} psnr_{name}: {got} vs {want}");
            assert!((p.get(&format!("mse_{name}")).unwrap() - mses[k]).abs() < 1e-9);
        }
        let total: usize = la.iter().map(|p| p.len()).sum();
        let mse_avg: f64 = la.iter().zip(&mses).map(|(p, m)| m * p.len() as f64 / total as f64).sum();
        assert!((p.get("avg").unwrap() - legacy::psnr_from_mse(mse_avg, max).min(PSNR_CAP_DB)).abs() < 1e-9, "case {i} avg");
        let s = common::run("ssim.fast", &va, Some(&vb), &o);
        let want = legacy::ssim_plane(&la[0], &lb[0], depth);
        assert!((s.get("y").unwrap() - want).abs() < 1e-12, "case {i} ssim {} vs {want}", s.get("y").unwrap());
        let want_u = legacy::ssim_plane(&la[1], &lb[1], depth);
        assert!((s.get("u").unwrap() - want_u).abs() < 1e-12, "case {i} ssim u");
    }
}

#[test]
fn identical_frames_cap_like_lumen_core() {
    let (la, _, oa, _) = pair(80, 60, 8, 0.0, 7);
    let v = oa.view();
    let p = common::run("psnr", &v, Some(&v), &MetricOptions::default());
    assert!(legacy::psnr_from_mse(legacy::mse_plane(&la[0], &la[0]), 255.0).is_infinite());
    for k in ["y", "u", "v", "avg"] {
        assert_eq!(p.get(k), Some(PSNR_CAP_DB));
    }
    assert_eq!(common::run("ssim.fast", &v, Some(&v), &MetricOptions::default()).get("y"), Some(1.0));
}
