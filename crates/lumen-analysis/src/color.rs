//! Color math: sRGB/PQ/HLG transfer functions, RGB to XYZ, CIE Lab (D65),
//! and the Delta-E formulas CIE76, CIE94 (graphic arts) and CIEDE2000.
//!
//! The `f64` Delta-E functions are the reference implementations (tested
//! against Sharma, Wu, Dalal 2005); the `f32` row kernels feed the maps.

use crate::frame::Transfer;
use crate::simd;
use std::sync::OnceLock;

/// Linear sRGB / BT.709 to XYZ (D65), IEC 61966-2-1.
pub static SRGB_TO_XYZ: [[f32; 3]; 3] = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192, 0.950_304_1],
];

/// Linear BT.2020 to XYZ (D65).
pub static BT2020_TO_XYZ: [[f32; 3]; 3] = [
    [0.636_958, 0.144_616_9, 0.168_881],
    [0.262_700_2, 0.677_998_1, 0.059_301_7],
    [0.000_000_0, 0.028_072_7, 1.060_985_1],
];

/// sRGB decoding of one encoded value (exact formula).
#[inline]
pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB encoding of one linear value (exact formula).
#[inline]
pub fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// 8-bit sRGB code value to linear light.
pub fn srgb_u8_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0)))
}

const EOTF_STEPS: usize = 4096;

/// sRGB EOTF sampled at 4097 points on 0..1; linear interpolation error is
/// below 3e-8, far under `f32` Lab precision needs.
fn srgb_table() -> &'static [f32; EOTF_STEPS + 1] {
    static T: OnceLock<[f32; EOTF_STEPS + 1]> = OnceLock::new();
    T.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(i as f32 / EOTF_STEPS as f32)))
}

/// PQ (ST 2084) EOTF: signal 0..1 to linear with 1.0 = 203 nits.
#[inline]
pub fn pq_to_linear(e: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let p = e.max(0.0).powf(1.0 / M2);
    let y = ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1);
    y * (10_000.0 / 203.0)
}

/// HLG inverse OETF normalized so a 75% signal (reference white) is 1.0.
#[inline]
pub fn hlg_to_linear(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    fn inv(e: f32) -> f32 {
        if e <= 0.5 {
            e * e / 3.0
        } else {
            (((e - C) / A).exp() + B) / 12.0
        }
    }
    inv(e.max(0.0)) / inv(0.75)
}

/// Applies the transfer function's EOTF to encoded values in place.
pub fn eotf_in_place(v: &mut [f32], t: Transfer) {
    match t {
        Transfer::Linear => {}
        Transfer::Srgb => {
            let tab = srgb_table();
            for x in v.iter_mut() {
                let s = *x;
                if (0.0..=1.0).contains(&s) {
                    let f = s * EOTF_STEPS as f32;
                    let i = (f as usize).min(EOTF_STEPS - 1);
                    let fr = f - i as f32;
                    *x = tab[i] + (tab[i + 1] - tab[i]) * fr;
                } else {
                    *x = s.signum() * srgb_to_linear(s.abs());
                }
            }
        }
        Transfer::Pq => v.iter_mut().for_each(|x| *x = pq_to_linear(*x)),
        Transfer::Hlg => v.iter_mut().for_each(|x| *x = hlg_to_linear(*x)),
    }
}

/// Cube root for positive inputs: exponent-hack seed plus two Halley steps
/// (relative error below 1e-7). Branch-free so the Lab loop vectorizes.
#[inline(always)]
pub fn cbrt_fast(x: f32) -> f32 {
    let seed = f32::from_bits(x.to_bits() / 3 + 709_921_077);
    let mut y = seed;
    for _ in 0..2 {
        let y3 = y * y * y;
        y *= (y3 + 2.0 * x) / (2.0 * y3 + x);
    }
    y
}

const LAB_EPS: f32 = 216.0 / 24389.0;
const LAB_KAPPA: f32 = 24389.0 / 27.0;

#[inline(always)]
fn lab_f(t: f32) -> f32 {
    let lin = (LAB_KAPPA * t + 16.0) / 116.0;
    let c = cbrt_fast(t.max(LAB_EPS));
    if t > LAB_EPS {
        c
    } else {
        lin
    }
}

#[inline(always)]
fn lab_row_impl(m: &[[f32; 3]; 3], r: &[f32], g: &[f32], b: &[f32], lo: &mut [f32], ao: &mut [f32], bo: &mut [f32]) {
    let inv = [1.0 / (m[0][0] + m[0][1] + m[0][2]), 1.0 / (m[1][0] + m[1][1] + m[1][2]), 1.0 / (m[2][0] + m[2][1] + m[2][2])];
    let n = r.len().min(g.len()).min(b.len()).min(lo.len()).min(ao.len()).min(bo.len());
    for i in 0..n {
        let (rr, gg, bb) = (r[i], g[i], b[i]);
        let x = (m[0][0] * rr + m[0][1] * gg + m[0][2] * bb) * inv[0];
        let y = (m[1][0] * rr + m[1][1] * gg + m[1][2] * bb) * inv[1];
        let z = (m[2][0] * rr + m[2][1] * gg + m[2][2] * bb) * inv[2];
        let (fx, fy, fz) = (lab_f(x), lab_f(y), lab_f(z));
        lo[i] = 116.0 * fy - 16.0;
        ao[i] = 500.0 * (fx - fy);
        bo[i] = 200.0 * (fy - fz);
    }
}

simd::avx2_dispatch!(
    /// Linear RGB row to Lab. The reference white is the matrix applied to
    /// (1, 1, 1), so neutral grays map to a* = b* = 0 exactly.
    pub fn linear_to_lab_row(m: &[[f32; 3]; 3], r: &[f32], g: &[f32], b: &[f32], lo: &mut [f32], ao: &mut [f32], bo: &mut [f32]) => lab_row_impl
);

/// CIE Lab of one 8-bit sRGB color (reference path, `f64`).
pub fn srgb8_to_lab(rgb: [u8; 3]) -> [f64; 3] {
    let lin = rgb.map(|c| srgb_to_linear(c as f32 / 255.0) as f64);
    let m = &SRGB_TO_XYZ;
    let row = |k: usize| m[k][0] as f64 * lin[0] + m[k][1] as f64 * lin[1] + m[k][2] as f64 * lin[2];
    let white = |k: usize| m[k][0] as f64 + m[k][1] as f64 + m[k][2] as f64;
    let f = |t: f64| {
        if t > 216.0 / 24389.0 {
            t.cbrt()
        } else {
            (24389.0 / 27.0 * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(row(0) / white(0)), f(row(1) / white(1)), f(row(2) / white(2)));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Lab L* to relative luminance Y (0..1).
#[inline]
pub fn lstar_to_y(l: f64) -> f64 {
    if l > 8.0 {
        ((l + 16.0) / 116.0).powi(3)
    } else {
        l / (24389.0 / 27.0)
    }
}

/// Relative luminance Y (0..1) to L*.
#[inline]
pub fn y_to_lstar(y: f64) -> f64 {
    if y > 216.0 / 24389.0 {
        116.0 * y.cbrt() - 16.0
    } else {
        y * (24389.0 / 27.0)
    }
}

/// CIE76: Euclidean distance in Lab.
pub fn delta_e76(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// CIE94, graphic arts weights (kL = 1, K1 = 0.045, K2 = 0.015), `a` is the reference.
pub fn delta_e94(a: [f64; 3], b: [f64; 3]) -> f64 {
    let c1 = a[1].hypot(a[2]);
    let c2 = b[1].hypot(b[2]);
    let dl = a[0] - b[0];
    let dc = c1 - c2;
    let dh2 = ((a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2) - dc * dc).max(0.0);
    let sc = 1.0 + 0.045 * c1;
    let sh = 1.0 + 0.015 * c1;
    (dl * dl + (dc / sc).powi(2) + dh2 / (sh * sh)).sqrt()
}

/// CIEDE2000 (kL = kC = kH = 1), following Sharma, Wu, Dalal (2005).
pub fn delta_e2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    let [l1, a1, b1] = lab1;
    let [l2, a2, b2] = lab2;
    let pow25_7 = 25f64.powi(7);
    let cb = (a1.hypot(b1) + a2.hypot(b2)) / 2.0;
    let cb7 = cb.powi(7);
    let g = 0.5 * (1.0 - (cb7 / (cb7 + pow25_7)).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = a1p.hypot(b1);
    let c2p = a2p.hypot(b2);
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            let h = b.atan2(a).to_degrees();
            if h < 0.0 {
                h + 360.0
            } else {
                h
            }
        }
    };
    let h1p = hue(b1, a1p);
    let h2p = hue(b2, a2p);
    let dlp = l2 - l1;
    let dcp = c2p - c1p;
    let cc = c1p * c2p;
    let dhp = if cc == 0.0 {
        0.0
    } else {
        let d = h2p - h1p;
        if d.abs() <= 180.0 {
            d
        } else if d > 180.0 {
            d - 360.0
        } else {
            d + 360.0
        }
    };
    let dhp_big = 2.0 * cc.sqrt() * (dhp.to_radians() / 2.0).sin();
    let lbp = (l1 + l2) / 2.0;
    let cbp = (c1p + c2p) / 2.0;
    let hbp = if cc == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (hbp - 30.0).to_radians().cos() + 0.24 * (2.0 * hbp).to_radians().cos() + 0.32 * (3.0 * hbp + 6.0).to_radians().cos()
        - 0.20 * (4.0 * hbp - 63.0).to_radians().cos();
    let dtheta = 30.0 * (-((hbp - 275.0) / 25.0).powi(2)).exp();
    let cbp7 = cbp.powi(7);
    let rc = 2.0 * (cbp7 / (cbp7 + pow25_7)).sqrt();
    let l50 = (lbp - 50.0).powi(2);
    let sl = 1.0 + 0.015 * l50 / (20.0 + l50).sqrt();
    let sc = 1.0 + 0.045 * cbp;
    let sh = 1.0 + 0.015 * cbp * t;
    let rt = -(2.0 * dtheta).to_radians().sin() * rc;
    let (tl, tc, th) = (dlp / sl, dcp / sc, dhp_big / sh);
    (tl * tl + tc * tc + th * th + rt * tc * th).max(0.0).sqrt()
}

/// Fast CIEDE2000 in `f32`: (value, hue difference within 1e-3 rad of 180
/// degrees). Polynomial atan2 / sin_cos / exp and multiple-angle identities,
/// no libm calls, so row loops vectorize.
#[inline(always)]
fn de2000_core(l1: f32, a1: f32, b1: f32, l2: f32, a2: f32, b2: f32) -> (f32, bool) {
    use std::f32::consts::{PI, TAU};
    const P25_7: f32 = 6_103_515_625.0;
    let cb = 0.5 * ((a1 * a1 + b1 * b1).sqrt() + (a2 * a2 + b2 * b2).sqrt());
    let cb2 = cb * cb;
    let cb7 = cb2 * cb2 * cb2 * cb;
    let g = 0.5 * (1.0 - (cb7 / (cb7 + P25_7)).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = (a1p * a1p + b1 * b1).sqrt();
    let c2p = (a2p * a2p + b2 * b2).sqrt();
    let h1 = simd::atan2(b1, a1p);
    let h2 = simd::atan2(b2, a2p);
    let h1p = if h1 < 0.0 { h1 + TAU } else { h1 };
    let h2p = if h2 < 0.0 { h2 + TAU } else { h2 };
    let dlp = l2 - l1;
    let dcp = c2p - c1p;
    let cc = c1p * c2p;
    let d = h2p - h1p;
    let dh = if d > PI {
        d - TAU
    } else if d < -PI {
        d + TAU
    } else {
        d
    };
    let dhp = if cc == 0.0 { 0.0 } else { dh };
    let s = h1p + h2p;
    let ad = (h1p - h2p).abs();
    let hb = if ad <= PI {
        0.5 * s
    } else if s < TAU {
        0.5 * (s + TAU)
    } else {
        0.5 * (s - TAU)
    };
    let hbp = if cc == 0.0 { s } else { hb };
    let near = cc != 0.0 && (ad - PI).abs() < 1e-3;
    let dhp_big = 2.0 * cc.sqrt() * simd::sin_cos(0.5 * dhp).0;
    let lbp = 0.5 * (l1 + l2);
    let cbp = 0.5 * (c1p + c2p);
    let (s1, c1) = simd::sin_cos(hbp);
    // cos(h - 30), cos 2h, cos(3h + 6), cos(4h - 63) from cos h and sin h.
    const C30: f32 = 0.866_025_4;
    const S30: f32 = 0.5;
    const C6: f32 = 0.994_521_9;
    const S6: f32 = 0.104_528_46;
    const C63: f32 = 0.453_990_5;
    const S63: f32 = 0.891_006_5;
    let cos_m30 = c1 * C30 + s1 * S30;
    let c2 = c1 * c1 - s1 * s1;
    let s2 = 2.0 * s1 * c1;
    let c3 = c1 * (4.0 * c1 * c1 - 3.0);
    let s3 = s1 * (3.0 - 4.0 * s1 * s1);
    let cos_3p6 = c3 * C6 - s3 * S6;
    let c4 = c2 * c2 - s2 * s2;
    let s4 = 2.0 * s2 * c2;
    let cos_4m63 = c4 * C63 + s4 * S63;
    let t = 1.0 - 0.17 * cos_m30 + 0.24 * c2 + 0.32 * cos_3p6 - 0.20 * cos_4m63;
    let q = (hbp * (180.0 / PI) - 275.0) / 25.0;
    let dtheta = (30.0 * PI / 180.0) * simd::exp_neg(-q * q);
    let cbp2 = cbp * cbp;
    let cbp7 = cbp2 * cbp2 * cbp2 * cbp;
    let rc = 2.0 * (cbp7 / (cbp7 + P25_7)).sqrt();
    let l50 = (lbp - 50.0) * (lbp - 50.0);
    let sl = 1.0 + 0.015 * l50 / (20.0 + l50).sqrt();
    let sc = 1.0 + 0.045 * cbp;
    let sh = 1.0 + 0.015 * cbp * t;
    let rt = -simd::sin_cos(2.0 * dtheta).0 * rc;
    let (tl, tc, th) = (dlp / sl, dcp / sc, dhp_big / sh);
    ((tl * tl + tc * tc + th * th + rt * tc * th).max(0.0).sqrt(), near)
}

fn f64_2000(l1: f32, a1: f32, b1: f32, l2: f32, a2: f32, b2: f32) -> f32 {
    delta_e2000([l1 as f64, a1 as f64, b1 as f64], [l2 as f64, a2 as f64, b2 as f64]) as f32
}

/// CIEDE2000 in `f32` for maps; matches the `f64` reference to about 1e-4.
/// Pairs whose hues are almost exactly opposite (where the formula jumps)
/// are recomputed in `f64` so the branch matches the reference.
pub fn delta_e2000_f32(l1: f32, a1: f32, b1: f32, l2: f32, a2: f32, b2: f32) -> f32 {
    let (d, near) = de2000_core(l1, a1, b1, l2, a2, b2);
    if near {
        f64_2000(l1, a1, b1, l2, a2, b2)
    } else {
        d
    }
}

#[inline(always)]
fn de2000_row_impl(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) {
    let n = out.len().min(l1.len()).min(a1.len()).min(b1.len()).min(l2.len()).min(a2.len()).min(b2.len());
    let mut any = false;
    for i in 0..n {
        let (d, near) = de2000_core(l1[i], a1[i], b1[i], l2[i], a2[i], b2[i]);
        out[i] = d;
        any |= near;
    }
    if any {
        for i in 0..n {
            if de2000_core(l1[i], a1[i], b1[i], l2[i], a2[i], b2[i]).1 {
                out[i] = f64_2000(l1[i], a1[i], b1[i], l2[i], a2[i], b2[i]);
            }
        }
    }
}

simd::avx2_dispatch!(
    /// CIEDE2000 of a row of Lab pairs (structure of arrays) into `out`.
    pub fn delta_e2000_row(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) => de2000_row_impl
);

/// CIE94 in `f32` (graphic arts), first color is the reference.
#[inline(always)]
pub fn delta_e94_f32(l1: f32, a1: f32, b1: f32, l2: f32, a2: f32, b2: f32) -> f32 {
    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let dl = l1 - l2;
    let dc = c1 - c2;
    let (da, db) = (a1 - a2, b1 - b2);
    let dh2 = (da * da + db * db - dc * dc).max(0.0);
    let sc = 1.0 + 0.045 * c1;
    let sh = 1.0 + 0.015 * c1;
    (dl * dl + (dc / sc) * (dc / sc) + dh2 / (sh * sh)).sqrt()
}

#[inline(always)]
fn de94_row_impl(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) {
    let n = out.len().min(l1.len()).min(a1.len()).min(b1.len()).min(l2.len()).min(a2.len()).min(b2.len());
    for i in 0..n {
        out[i] = delta_e94_f32(l1[i], a1[i], b1[i], l2[i], a2[i], b2[i]);
    }
}

simd::avx2_dispatch!(
    /// CIE94 of a row of Lab pairs into `out`.
    pub fn delta_e94_row(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) => de94_row_impl
);

#[inline(always)]
fn de76_row_impl(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) {
    let n = out.len().min(l1.len()).min(a1.len()).min(b1.len()).min(l2.len()).min(a2.len()).min(b2.len());
    for i in 0..n {
        let (dl, da, db) = (l1[i] - l2[i], a1[i] - a2[i], b1[i] - b2[i]);
        out[i] = (dl * dl + da * da + db * db).sqrt();
    }
}

simd::avx2_dispatch!(
    /// CIE76 of a row of Lab pairs into `out`.
    pub fn delta_e76_row(l1: &[f32], a1: &[f32], b1: &[f32], l2: &[f32], a2: &[f32], b2: &[f32], out: &mut [f32]) => de76_row_impl
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Sharma, Wu, Dalal (2005), Table 1: (L1, a1, b1, L2, a2, b2, dE00).
    pub(crate) const SHARMA: [[f64; 7]; 34] = [
        [50.0000, 2.6772, -79.7751, 50.0000, 0.0000, -82.7485, 2.0425],
        [50.0000, 3.1571, -77.2803, 50.0000, 0.0000, -82.7485, 2.8615],
        [50.0000, 2.8361, -74.0200, 50.0000, 0.0000, -82.7485, 3.4412],
        [50.0000, -1.3802, -84.2814, 50.0000, 0.0000, -82.7485, 1.0000],
        [50.0000, -1.1848, -84.8006, 50.0000, 0.0000, -82.7485, 1.0000],
        [50.0000, -0.9009, -85.5211, 50.0000, 0.0000, -82.7485, 1.0000],
        [50.0000, 0.0000, 0.0000, 50.0000, -1.0000, 2.0000, 2.3669],
        [50.0000, -1.0000, 2.0000, 50.0000, 0.0000, 0.0000, 2.3669],
        [50.0000, 2.4900, -0.0010, 50.0000, -2.4900, 0.0009, 7.1792],
        [50.0000, 2.4900, -0.0010, 50.0000, -2.4900, 0.0010, 7.1792],
        [50.0000, 2.4900, -0.0010, 50.0000, -2.4900, 0.0011, 7.2195],
        [50.0000, 2.4900, -0.0010, 50.0000, -2.4900, 0.0012, 7.2195],
        [50.0000, -0.0010, 2.4900, 50.0000, 0.0009, -2.4900, 4.8045],
        [50.0000, -0.0010, 2.4900, 50.0000, 0.0010, -2.4900, 4.8045],
        [50.0000, -0.0010, 2.4900, 50.0000, 0.0011, -2.4900, 4.7461],
        [50.0000, 2.5000, 0.0000, 50.0000, 0.0000, -2.5000, 4.3065],
        [50.0000, 2.5000, 0.0000, 73.0000, 25.0000, -18.0000, 27.1492],
        [50.0000, 2.5000, 0.0000, 61.0000, -5.0000, 29.0000, 22.8977],
        [50.0000, 2.5000, 0.0000, 56.0000, -27.0000, -3.0000, 31.9030],
        [50.0000, 2.5000, 0.0000, 58.0000, 24.0000, 15.0000, 19.4535],
        [50.0000, 2.5000, 0.0000, 50.0000, 3.1736, 0.5854, 1.0000],
        [50.0000, 2.5000, 0.0000, 50.0000, 3.2972, 0.0000, 1.0000],
        [50.0000, 2.5000, 0.0000, 50.0000, 1.8634, 0.5757, 1.0000],
        [50.0000, 2.5000, 0.0000, 50.0000, 3.2592, 0.3350, 1.0000],
        [60.2574, -34.0099, 36.2677, 60.4626, -34.1751, 39.4387, 1.2644],
        [63.0109, -31.0961, -5.8663, 62.8187, -29.7946, -4.0864, 1.2630],
        [61.2901, 3.7196, -5.3901, 61.4292, 2.2480, -4.9620, 1.8731],
        [35.0831, -44.1164, 3.7933, 35.0232, -40.0716, 1.5901, 1.8645],
        [22.7233, 20.0904, -46.6940, 23.0331, 14.9730, -42.5619, 2.0373],
        [36.4612, 47.8580, 18.3852, 36.2715, 50.5065, 21.2231, 1.4146],
        [90.8027, -2.0831, 1.4410, 91.1528, -1.6435, 0.0447, 1.4441],
        [90.9257, -0.5406, -0.9208, 88.6381, -0.8985, -0.7239, 1.5381],
        [6.7747, -0.2908, -2.4247, 5.8714, -0.0985, -2.2286, 0.6377],
        [2.0776, 0.0795, -1.1350, 0.9033, -0.0636, -0.5514, 0.9082],
    ];

    #[test]
    fn ciede2000_sharma_table() {
        for (i, r) in SHARMA.iter().enumerate() {
            let d = delta_e2000([r[0], r[1], r[2]], [r[3], r[4], r[5]]);
            assert!((d - r[6]).abs() < 1e-4, "pair {}: {d} vs {}", i + 1, r[6]);
            // Symmetric.
            let d2 = delta_e2000([r[3], r[4], r[5]], [r[0], r[1], r[2]]);
            assert!((d - d2).abs() < 1e-9);
        }
    }

    #[test]
    fn ciede2000_f32_matches_reference() {
        for (i, r) in SHARMA.iter().enumerate() {
            let f = r.map(|v| v as f32);
            let d = delta_e2000_f32(f[0], f[1], f[2], f[3], f[4], f[5]) as f64;
            assert!((d - r[6]).abs() < 1e-4, "pair {}: {d} vs {}", i + 1, r[6]);
        }
        // The (vectorized, dispatched) row kernel on the whole table at once.
        let col = |k: usize| SHARMA.iter().map(|r| r[k] as f32).collect::<Vec<f32>>();
        let mut out = vec![0f32; SHARMA.len()];
        delta_e2000_row(&col(0), &col(1), &col(2), &col(3), &col(4), &col(5), &mut out);
        for (i, r) in SHARMA.iter().enumerate() {
            assert!((out[i] as f64 - r[6]).abs() < 1e-4, "row pair {}: {} vs {}", i + 1, out[i], r[6]);
        }
        // Random Lab pairs against the f64 reference.
        let mut seed = 12345u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 40) as f32 / (1u64 << 24) as f32
        };
        let n = 4096;
        let v: Vec<[f32; 6]> = (0..n)
            .map(|_| {
                let (l, a, b) = (rnd() * 100.0, rnd() * 200.0 - 100.0, rnd() * 200.0 - 100.0);
                [l, a, b, (l + rnd() * 10.0 - 5.0).clamp(0.0, 100.0), a + rnd() * 10.0 - 5.0, b + rnd() * 10.0 - 5.0]
            })
            .collect();
        let col = |k: usize| v.iter().map(|r| r[k]).collect::<Vec<f32>>();
        let mut out = vec![0f32; n];
        delta_e2000_row(&col(0), &col(1), &col(2), &col(3), &col(4), &col(5), &mut out);
        let mut worst = 0f64;
        for (i, r) in v.iter().enumerate() {
            let want = delta_e2000([r[0] as f64, r[1] as f64, r[2] as f64], [r[3] as f64, r[4] as f64, r[5] as f64]);
            worst = worst.max((out[i] as f64 - want).abs());
        }
        assert!(worst < 1e-4, "worst {worst}");
    }

    #[test]
    fn delta_e_identity_and_simple_cases() {
        let c = [52.0, 10.0, -20.0];
        assert_eq!(delta_e76(c, c), 0.0);
        assert_eq!(delta_e94(c, c), 0.0);
        assert_eq!(delta_e2000(c, c), 0.0);
        assert_eq!(delta_e2000_f32(52.0, 10.0, -20.0, 52.0, 10.0, -20.0), 0.0);
        assert!((delta_e76([50.0, 0.0, 0.0], [53.0, 4.0, 0.0]) - 5.0).abs() < 1e-12);
        // Pure lightness difference: CIE94 equals CIE76.
        assert!((delta_e94([50.0, 0.0, 0.0], [55.0, 0.0, 0.0]) - 5.0).abs() < 1e-12);
        let f = delta_e94_f32(50.0, 20.0, 5.0, 52.0, 18.0, 9.0) as f64;
        assert!((f - delta_e94([50.0, 20.0, 5.0], [52.0, 18.0, 9.0])).abs() < 1e-5);
    }

    #[test]
    fn lab_of_known_colors() {
        let w = srgb8_to_lab([255, 255, 255]);
        assert!((w[0] - 100.0).abs() < 1e-4 && w[1].abs() < 1e-4 && w[2].abs() < 1e-4, "{w:?}");
        let k = srgb8_to_lab([0, 0, 0]);
        assert!(k[0].abs() < 1e-9);
        // sRGB red: L 53.24, a 80.09, b 67.20 (Lindbloom, D65).
        let r = srgb8_to_lab([255, 0, 0]);
        assert!((r[0] - 53.24).abs() < 0.02 && (r[1] - 80.09).abs() < 0.05 && (r[2] - 67.20).abs() < 0.05, "{r:?}");
        // The f32 row kernel agrees with the f64 path.
        let lut = srgb_u8_lut();
        for rgb in [[255u8, 0, 0], [12, 200, 77], [128, 128, 128], [3, 1, 250]] {
            let (mut l, mut a, mut b) = ([0f32], [0f32], [0f32]);
            let lin = rgb.map(|c| lut[c as usize]);
            linear_to_lab_row(&SRGB_TO_XYZ, &[lin[0]], &[lin[1]], &[lin[2]], &mut l, &mut a, &mut b);
            let refl = srgb8_to_lab(rgb);
            assert!((l[0] as f64 - refl[0]).abs() < 2e-4 && (a[0] as f64 - refl[1]).abs() < 5e-4 && (b[0] as f64 - refl[2]).abs() < 5e-4);
        }
    }

    #[test]
    fn transfer_functions() {
        for i in 0..=255 {
            let v = i as f32 / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-5);
        }
        let mut row: Vec<f32> = (0..=1000).map(|i| i as f32 / 1000.0).collect();
        let exact: Vec<f32> = row.iter().map(|v| srgb_to_linear(*v)).collect();
        eotf_in_place(&mut row, Transfer::Srgb);
        for (a, b) in row.iter().zip(&exact) {
            assert!((a - b).abs() < 1e-6);
        }
        // PQ 0.58 is about 203 nits; HLG 0.75 is reference white.
        assert!((pq_to_linear(0.580_689_7) - 1.0).abs() < 0.01);
        assert!((hlg_to_linear(0.75) - 1.0).abs() < 1e-6);
        for x in [1e-6f32, 0.001, 0.0089, 0.5, 1.0, 1.7] {
            assert!((cbrt_fast(x) - x.cbrt()).abs() <= 1e-6 * x.cbrt());
        }
        assert!((y_to_lstar(lstar_to_y(37.5)) - 37.5).abs() < 1e-9);
    }
}
