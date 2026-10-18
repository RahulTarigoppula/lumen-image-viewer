//! Tonal mapping: Levels and Curves lookup tables (per channel + master).

use image::RgbaImage;

/// Channel index used by Levels/Curves: 0 = composite RGB, 1..=3 = R, G, B.
pub const CHANNEL_NAMES: [&str; 4] = ["RGB", "Red", "Green", "Blue"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelsParams {
    pub in_black: f32,
    pub in_white: f32,
    /// Midtone gamma (> 1 brightens).
    pub gamma: f32,
    pub out_black: f32,
    pub out_white: f32,
}

impl Default for LevelsParams {
    fn default() -> Self {
        LevelsParams { in_black: 0.0, in_white: 255.0, gamma: 1.0, out_black: 0.0, out_white: 255.0 }
    }
}

impl LevelsParams {
    pub fn is_identity(&self) -> bool {
        *self == LevelsParams::default()
    }
    pub fn map(&self, v: f32) -> f32 {
        let lo = self.in_black.clamp(0.0, 254.0);
        let hi = self.in_white.clamp(lo + 1.0, 255.0);
        let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        let g = self.gamma.clamp(0.05, 20.0);
        let t = if g == 1.0 { t } else { t.powf(1.0 / g) };
        self.out_black + (self.out_white - self.out_black) * t
    }
    pub fn lut(&self) -> [u8; 256] {
        let mut l = [0u8; 256];
        for (i, v) in l.iter_mut().enumerate() {
            *v = self.map(i as f32).round().clamp(0.0, 255.0) as u8;
        }
        l
    }
}

/// Combine per-channel tables with the composite one: out = master(channel(x)).
pub fn compose_luts(master: &[u8; 256], chans: [&[u8; 256]; 3]) -> [[u8; 256]; 3] {
    let mut out = [[0u8; 256]; 3];
    for c in 0..3 {
        for i in 0..256 {
            out[c][i] = master[chans[c][i] as usize];
        }
    }
    out
}

pub fn apply_levels(img: &RgbaImage, p: &[LevelsParams; 4]) -> RgbaImage {
    if p.iter().all(|l| l.is_identity()) {
        return img.clone();
    }
    let luts = compose_luts(&p[0].lut(), [&p[1].lut(), &p[2].lut(), &p[3].lut()]);
    super::ops::apply_lut(img, &luts)
}

/// Value at `pct` (0..1) of a 256-bin histogram.
pub fn percentile(h: &[u32], pct: f64) -> usize {
    let total: u64 = h.iter().map(|&v| v as u64).sum();
    if total == 0 {
        return if pct < 0.5 { 0 } else { 255 };
    }
    let target = (total as f64 * pct).max(1.0) as u64;
    let mut acc = 0u64;
    for (i, &v) in h.iter().enumerate() {
        acc += v as u64;
        if acc >= target {
            return i;
        }
    }
    h.len().saturating_sub(1)
}

// ---------------------------------------------------------------- curves

/// Monotone cubic (Fritsch-Carlson) interpolation through `pts` (x sorted,
/// 0..1 coordinates) sampled into a 256-entry table. Outside the first/last
/// point the curve is flat.
pub fn curve_lut(pts: &[[f32; 2]]) -> [u8; 256] {
    let mut p: Vec<[f32; 2]> = pts.iter().map(|q| [q[0].clamp(0.0, 1.0), q[1].clamp(0.0, 1.0)]).collect();
    p.sort_by(|a, b| a[0].total_cmp(&b[0]));
    p.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-6);
    let mut lut = [0u8; 256];
    if p.is_empty() {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    if p.len() == 1 {
        return [(p[0][1] * 255.0).round() as u8; 256];
    }
    let n = p.len();
    let mut d = vec![0f32; n - 1];
    for i in 0..n - 1 {
        d[i] = (p[i + 1][1] - p[i][1]) / (p[i + 1][0] - p[i][0]);
    }
    let mut m = vec![0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) / 2.0 };
    }
    for i in 0..n - 1 {
        if d[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let a = m[i] / d[i];
            let b = m[i + 1] / d[i];
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
    }
    let eval = |x: f32| -> f32 {
        if x <= p[0][0] {
            return p[0][1];
        }
        if x >= p[n - 1][0] {
            return p[n - 1][1];
        }
        let mut i = 0;
        while i + 1 < n - 1 && x > p[i + 1][0] {
            i += 1;
        }
        let h = p[i + 1][0] - p[i][0];
        let t = (x - p[i][0]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * p[i][1] + h10 * h * m[i] + h01 * p[i + 1][1] + h11 * h * m[i + 1]
    };
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (eval(i as f32 / 255.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    lut
}

pub fn identity_curve() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0]]
}

pub fn apply_curves(img: &RgbaImage, curves: &[Vec<[f32; 2]>; 4]) -> RgbaImage {
    let luts: Vec<[u8; 256]> = curves.iter().map(|c| curve_lut(c)).collect();
    let ident: [u8; 256] = std::array::from_fn(|i| i as u8);
    if luts.iter().all(|l| *l == ident) {
        return img.clone();
    }
    let t = compose_luts(&luts[0], [&luts[1], &luts[2], &luts[3]]);
    super::ops::apply_lut(img, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(17, 9, |x, y| image::Rgba([(x * 15) as u8, (y * 28) as u8, (x * y) as u8, 200]))
    }

    #[test]
    fn levels_identity() {
        let img = sample();
        assert_eq!(apply_levels(&img, &[LevelsParams::default(); 4]), img);
        let l = LevelsParams::default().lut();
        assert!(l.iter().enumerate().all(|(i, &v)| v == i as u8));
    }

    #[test]
    fn levels_values() {
        let p = LevelsParams { in_black: 50.0, in_white: 200.0, ..Default::default() };
        let l = p.lut();
        assert_eq!(l[0], 0);
        assert_eq!(l[50], 0);
        assert_eq!(l[200], 255);
        assert_eq!(l[125], 128);
        let inv = LevelsParams { out_black: 255.0, out_white: 0.0, ..Default::default() };
        assert_eq!(inv.lut()[0], 255);
        // Degenerate inputs don't panic.
        let bad = LevelsParams { in_black: 255.0, in_white: 0.0, gamma: 0.0, ..Default::default() };
        let _ = bad.lut();
    }

    #[test]
    fn curves_identity_and_monotone() {
        let img = sample();
        let id = [identity_curve(), identity_curve(), identity_curve(), identity_curve()];
        assert_eq!(apply_curves(&img, &id), img);
        let l = curve_lut(&[[0.0, 0.0], [0.25, 0.15], [0.75, 0.85], [1.0, 1.0]]);
        assert!(l.windows(2).all(|w| w[1] >= w[0]));
        assert!(l[64] < 64 && l[191] > 191);
        assert_eq!(curve_lut(&[[0.5, 0.5]]), [128u8; 256]);
        let flat = curve_lut(&[[0.2, 0.3], [0.8, 0.6]]);
        assert_eq!(flat[0], 77);
        assert_eq!(flat[255], 153);
    }

    #[test]
    fn percentile_basic() {
        let mut h = vec![0u32; 256];
        h[10] = 50;
        h[200] = 50;
        assert_eq!(percentile(&h, 0.01), 10);
        assert_eq!(percentile(&h, 0.99), 200);
        assert_eq!(percentile(&[0; 256], 0.99), 255);
    }
}
