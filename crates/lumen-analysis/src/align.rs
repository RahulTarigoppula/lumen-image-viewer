//! Field-of-view matching: estimate the homography that maps one photo of a
//! scene onto another taken with a different lens, crop or position, plus
//! helpers to render the moving image in the reference frame and to find the
//! common field of view.
//!
//! Pipeline, all at a working resolution (`AlignOptions::max_side`):
//! grayscale area downscale, an image pyramid (factor sqrt 2, 5 levels),
//! FAST-9 corners ranked by Harris response with 3x3 non-max suppression,
//! grid bucketing and sub-pixel refinement, orientation by intensity
//! centroid, rotated BRIEF (256 bit) on a Gaussian smoothed level,
//! brute-force Hamming matching (rayon, u64 popcount) with a Lowe ratio test
//! and a mutual cross-check, RANSAC (normalized DLT, MSAC scoring,
//! degenerate sample rejection, adaptive iteration count) and weighted least
//! squares refinement on the inliers. The model is then scaled from working
//! to full-resolution pixel coordinates.
//!
//! Coordinates: pixel centers sit at integer coordinates, so pixel (0, 0)
//! covers [-0.5, 0.5] x [-0.5, 0.5] and a `w` x `h` image covers
//! [-0.5, w - 0.5] x [-0.5, h - 0.5]. RGBA buffers are tightly packed, 4 bytes
//! per pixel, row-major.

use crate::job::CancelToken;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

/// Geometric model fitted between the two images.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Model {
    /// Scale, rotation and translation (4 degrees of freedom).
    Similarity,
    /// General affine map (6 degrees of freedom).
    Affine,
    /// Full projective map (8 degrees of freedom). Falls back to
    /// [`Model::Similarity`] when the homography is not supported by enough
    /// inliers or is implausible.
    #[default]
    Homography,
}

/// Tuning for [`align_rgba`]. The defaults suit phone photos from 2 to 200 MP.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlignOptions {
    /// Longest side of the working images in pixels (each image is area
    /// downscaled to fit; smaller images are used as they are).
    pub max_side: u32,
    /// Feature budget per image, spread over the pyramid levels.
    pub max_features: usize,
    /// Lowe ratio test: best distance must be below `ratio` times the second
    /// best (0.6 strict .. 0.9 lenient).
    pub ratio: f32,
    /// RANSAC inlier threshold: reprojection error in working-resolution
    /// pixels.
    pub ransac_px: f32,
    /// Upper bound on RANSAC iterations (the adaptive count usually stops
    /// much earlier).
    pub iterations: usize,
    /// Model to fit.
    pub model: Model,
}

impl Default for AlignOptions {
    fn default() -> Self {
        Self { max_side: 1024, max_features: 2000, ratio: 0.8, ransac_px: 3.0, iterations: 2000, model: Model::Homography }
    }
}

/// A 3x3 projective transform, row-major. As produced by [`align_rgba`] it
/// maps (x, y, 1) in MOVING image pixels to REFERENCE image pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Homography(pub [f64; 9]);

impl Default for Homography {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Homography {
    pub const IDENTITY: Homography = Homography([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    pub fn identity() -> Self {
        Self::IDENTITY
    }

    /// `p + (tx, ty)`.
    pub fn translation(tx: f64, ty: f64) -> Self {
        Homography([1.0, 0.0, tx, 0.0, 1.0, ty, 0.0, 0.0, 1.0])
    }

    /// `(sx * x, sy * y)`.
    pub fn scaling(sx: f64, sy: f64) -> Self {
        Homography([sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0])
    }

    /// `scale * R(rotation_deg) * p + (tx, ty)`. In image coordinates (y
    /// down) a positive angle turns clockwise on screen.
    pub fn similarity(scale: f64, rotation_deg: f64, tx: f64, ty: f64) -> Self {
        let (s, c) = rotation_deg.to_radians().sin_cos();
        Homography([scale * c, -scale * s, tx, scale * s, scale * c, ty, 0.0, 0.0, 1.0])
    }

    /// Maps a point. Points on the horizon line (w = 0) give infinities.
    pub fn map(&self, p: [f64; 2]) -> [f64; 2] {
        let h = &self.0;
        let w = h[6] * p[0] + h[7] * p[1] + h[8];
        [(h[0] * p[0] + h[1] * p[1] + h[2]) / w, (h[3] * p[0] + h[4] * p[1] + h[5]) / w]
    }

    /// The inverse transform, `None` when the matrix is singular.
    pub fn inverse(&self) -> Option<Homography> {
        let [a, b, c, d, e, f, g, h, i] = self.0;
        let m = self.0.iter().fold(0f64, |m, v| m.max(v.abs()));
        if !m.is_finite() || m == 0.0 {
            return None;
        }
        let c0 = e * i - f * h;
        let c1 = f * g - d * i;
        let c2 = d * h - e * g;
        let det = a * c0 + b * c1 + c * c2;
        if !det.is_finite() || det.abs() <= 1e-14 * m * m * m {
            return None;
        }
        let adj = [c0, c * h - b * i, b * f - c * e, c1, a * i - c * g, c * d - a * f, c2, b * g - a * h, a * e - b * d];
        let mut r = adj.map(|v| v / det);
        let rm = r.iter().fold(0f64, |m, v| m.max(v.abs()));
        if r[8].abs() > 1e-12 * rm {
            let s = r[8];
            r = r.map(|v| v / s);
        }
        if r.iter().all(|v| v.is_finite()) {
            Some(Homography(r))
        } else {
            None
        }
    }

    /// Matrix product `self * other`: the transform that applies `other`
    /// first, then `self`, so `a.compose(&b).map(p) == a.map(b.map(p))`.
    pub fn compose(&self, other: &Homography) -> Homography {
        Homography(mul3(&self.0, &other.0))
    }

    /// The same transform scaled so the last entry is 1 (unchanged when that
    /// entry is 0).
    pub fn normalized(&self) -> Homography {
        let s = self.0[8];
        if s != 0.0 && s.is_finite() {
            Homography(self.0.map(|v| v / s))
        } else {
            *self
        }
    }

    pub fn is_finite(&self) -> bool {
        self.0.iter().all(|v| v.is_finite())
    }

    /// Local linear part at `p`: `[dX/dx, dX/dy, dY/dx, dY/dy]`.
    pub fn jacobian(&self, p: [f64; 2]) -> [f64; 4] {
        jacobian(&self.0, p)
    }

    /// Local scale at `p` (output pixels per input pixel, geometric mean of
    /// the two axes).
    pub fn scale_at(&self, p: [f64; 2]) -> f64 {
        let j = self.jacobian(p);
        (j[0] * j[3] - j[1] * j[2]).abs().sqrt()
    }

    /// Local rotation at `p` in degrees (positive turns clockwise on screen).
    pub fn rotation_deg_at(&self, p: [f64; 2]) -> f64 {
        let j = self.jacobian(p);
        (j[2] - j[1]).atan2(j[0] + j[3]).to_degrees()
    }
}

/// Outcome of a successful [`align_rgba`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlignResult {
    /// Maps moving image pixels to reference image pixels (full resolution).
    pub h: Homography,
    /// Putative matches (after ratio test and cross-check).
    pub matches: usize,
    /// Matches consistent with `h`.
    pub inliers: usize,
    /// Reprojection RMS of the inliers in full-resolution reference pixels.
    pub rms_px: f64,
    /// Reference pixels per moving pixel at the moving image center.
    pub scale: f64,
    /// Rotation at the moving image center in degrees (positive turns
    /// clockwise on screen).
    pub rotation_deg: f64,
    /// Heuristic 0..1 from inlier count, inlier ratio, residual and fallback.
    /// Above about 0.5 the alignment is normally trustworthy; below 0.2 treat
    /// it as a guess.
    pub confidence: f32,
    /// Model actually used (may be the fallback).
    pub model: Model,
    /// Features detected in the reference and the moving image.
    pub features: [usize; 2],
}

/// Why [`align_rgba`] gave up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlignError {
    /// One of the images has too little texture to find corners.
    TooFewFeatures,
    /// Features were found but no plausible geometric model explains enough
    /// matches (different scenes, or too large a change).
    NoConsensus,
    Cancelled,
    BadInput(String),
}

impl fmt::Display for AlignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlignError::TooFewFeatures => write!(f, "not enough detail to align"),
            AlignError::NoConsensus => write!(f, "no consistent alignment found"),
            AlignError::Cancelled => write!(f, "cancelled"),
            AlignError::BadInput(s) => write!(f, "bad input: {s}"),
        }
    }
}

impl std::error::Error for AlignError {}

// ---------------------------------------------------------------- constants

/// Pyramid levels and the scale step between them.
const LEVELS: usize = 5;
const LEVEL_SCALE: f64 = std::f64::consts::SQRT_2;
/// Keypoints keep this distance from the level border (BRIEF patch radius 15).
const BORDER: usize = 16;
/// Smallest level side that still holds keypoints.
const MIN_LEVEL: usize = 2 * BORDER + 8;
/// Intensity centroid patch radius.
const PATCH_R: i32 = 15;
/// FAST thresholds (0..255 gray): normal, and the retry for low contrast.
const FAST_HI: f32 = 20.0;
const FAST_LO: f32 = 7.0;
/// Bucketing cell size in level pixels.
const CELL: usize = 32;
/// Matches above this Hamming distance (of 256) are never accepted.
const MAX_HAMMING: u32 = 80;
const MIN_FEATURES: usize = 8;

// ---------------------------------------------------------------- public API

/// Estimates the transform mapping `moving` onto `reference` (both RGBA8,
/// tightly packed). Alpha is ignored. `cancel` is polled between stages and
/// during RANSAC.
#[allow(clippy::too_many_arguments)]
pub fn align_rgba(
    reference: &[u8],
    rw: u32,
    rh: u32,
    moving: &[u8],
    mw: u32,
    mh: u32,
    opts: &AlignOptions,
    cancel: Option<&CancelToken>,
) -> Result<AlignResult, AlignError> {
    check_input(reference, rw, rh, "reference")?;
    check_input(moving, mw, mh, "moving")?;
    let cancelled = || cancel.is_some_and(|c| c.is_cancelled());
    if cancelled() {
        return Err(AlignError::Cancelled);
    }

    let (rww, rwh) = working_size(rw, rh, opts.max_side);
    let (mww, mwh) = working_size(mw, mh, opts.max_side);
    let n_feat = opts.max_features.clamp(16, 20_000);
    let (fr, fm) = rayon::join(
        || detect_features(gray_from_rgba(reference, rw as usize, rh as usize, rww, rwh), rww, rwh, n_feat),
        || detect_features(gray_from_rgba(moving, mw as usize, mh as usize, mww, mwh), mww, mwh, n_feat),
    );
    if cancelled() {
        return Err(AlignError::Cancelled);
    }
    if fr.len() < MIN_FEATURES || fm.len() < MIN_FEATURES {
        return Err(AlignError::TooFewFeatures);
    }

    let pairs = match_features(&fm, &fr, opts.ratio.clamp(0.1, 1.0));
    if cancelled() {
        return Err(AlignError::Cancelled);
    }
    let pts: Vec<Corr> = pairs
        .iter()
        .map(|&(i, j)| {
            let s = fm[i].scale.min(fr[j].scale) as f64;
            Corr { s: [fm[i].x as f64, fm[i].y as f64], d: [fr[j].x as f64, fr[j].y as f64], w: s * s }
        })
        .collect();

    let geo = Geo { min_s: 0.02 * diag(mww, mwh), min_d: 0.02 * diag(rww, rwh) };
    let thresh = (opts.ransac_px as f64).max(0.5);
    let iters = opts.iterations.max(10);
    let attempt = |model: Model| -> Result<Option<(Model, Fit)>, AlignError> {
        let fit = ransac(&pts, model, thresh, iters, &geo, cancel)?;
        Ok(fit.filter(|f| f.inliers.len() >= min_inliers(model) && plausible(&f.h, mww, mwh)).map(|f| (model, f)))
    };
    let chosen = match opts.model {
        Model::Homography => match attempt(Model::Homography)? {
            Some(x) => Some(x),
            None => attempt(Model::Similarity)?,
        },
        Model::Affine => match attempt(Model::Affine)? {
            Some(x) => Some(x),
            None => attempt(Model::Similarity)?,
        },
        Model::Similarity => attempt(Model::Similarity)?,
    };
    let Some((model, fit)) = chosen else {
        return Err(AlignError::NoConsensus);
    };

    // Working to full resolution: H_full = Sr^-1 * H_work * Sm.
    let sm = pixel_scale(mw, mh, mww, mwh);
    let sr = pixel_scale(rw, rh, rww, rwh);
    let (Some(sm_inv), Some(sr_inv)) = (sm.inverse(), sr.inverse()) else {
        return Err(AlignError::BadInput("degenerate size".into()));
    };
    let hw = Homography(fit.h);
    let h_full = sr_inv.compose(&hw).compose(&sm).normalized();
    if !h_full.is_finite() {
        return Err(AlignError::NoConsensus);
    }

    let n_in = fit.inliers.len();
    let mut e_work = 0.0;
    let mut e_full = 0.0;
    for &i in &fit.inliers {
        let c = &pts[i];
        e_work += err2(&fit.h, c);
        let q = h_full.map(sm_inv.map(c.s));
        let d = sr_inv.map(c.d);
        e_full += (q[0] - d[0]).powi(2) + (q[1] - d[1]).powi(2);
    }
    let rms_work = (e_work / n_in as f64).sqrt();
    let rms_px = (e_full / n_in as f64).sqrt();
    let center = [(mw as f64 - 1.0) * 0.5, (mh as f64 - 1.0) * 0.5];

    let ratio = n_in as f64 / pts.len().max(1) as f64;
    let c_count = 1.0 - (-((n_in - min_inliers(model)) as f64) / 40.0).exp();
    let c_ratio = (ratio / 0.5).min(1.0);
    let c_err = 1.0 / (1.0 + (rms_work / (0.5 * thresh)).powi(2));
    let mut conf = c_count * (0.4 + 0.6 * c_ratio) * (0.5 + 0.5 * c_err);
    if model != opts.model {
        conf *= 0.8;
    }

    Ok(AlignResult {
        h: h_full,
        matches: pts.len(),
        inliers: n_in,
        rms_px,
        scale: h_full.scale_at(center),
        rotation_deg: h_full.rotation_deg_at(center),
        confidence: conf.clamp(0.0, 1.0) as f32,
        model,
        features: [fr.len(), fm.len()],
    })
}

/// Renders `src` into the reference frame: output pixel (x, y) samples `src`
/// bilinearly at `h^-1(x, y)`; pixels that fall outside `src` (or behind the
/// horizon) are transparent (all four bytes 0). `h` maps `src` pixels to
/// output pixels, as returned by [`align_rgba`]. The identity reproduces
/// `src` exactly.
pub fn warp_rgba(src: &[u8], sw: u32, sh: u32, h: &Homography, out_w: u32, out_h: u32) -> Vec<u8> {
    let (ow, oh) = (out_w as usize, out_h as usize);
    let (swu, shu) = (sw as usize, sh as usize);
    let mut out = vec![0u8; ow * oh * 4];
    if ow == 0 || oh == 0 || swu == 0 || shu == 0 || src.len() < swu * shu * 4 {
        return out;
    }
    let Some(inv) = h.inverse() else {
        return out;
    };
    let m = inv.0;
    // Forward matrix with a positive denominator at the source center: a
    // source point is in front when its forward denominator is positive.
    let mut f = h.0;
    let cx = (sw as f64 - 1.0) * 0.5;
    let cy = (sh as f64 - 1.0) * 0.5;
    let dc = f[6] * cx + f[7] * cy + f[8];
    if !(dc.abs() > 0.0) {
        return out;
    }
    if dc < 0.0 {
        f = f.map(|v| -v);
    }
    let (xmax, ymax) = (sw as f64 - 0.5, sh as f64 - 0.5);
    let (sxm, sym) = (swu as isize - 1, shu as isize - 1);
    out.par_chunks_mut(ow * 4).enumerate().for_each(|(y, row)| {
        let yf = y as f64;
        for x in 0..ow {
            let xf = x as f64;
            let w = m[6] * xf + m[7] * yf + m[8];
            if !(w.abs() > 1e-300) {
                continue;
            }
            let px = (m[0] * xf + m[1] * yf + m[2]) / w;
            let py = (m[3] * xf + m[4] * yf + m[5]) / w;
            if !(px >= -0.5 && px <= xmax && py >= -0.5 && py <= ymax) {
                continue;
            }
            if f[6] * px + f[7] * py + f[8] <= 0.0 {
                continue;
            }
            let x0f = px.floor();
            let y0f = py.floor();
            let fx = (px - x0f) as f32;
            let fy = (py - y0f) as f32;
            let x0 = (x0f as isize).clamp(0, sxm) as usize;
            let x1 = (x0f as isize + 1).clamp(0, sxm) as usize;
            let y0 = (y0f as isize).clamp(0, sym) as usize;
            let y1 = (y0f as isize + 1).clamp(0, sym) as usize;
            let p00 = &src[(y0 * swu + x0) * 4..(y0 * swu + x0) * 4 + 4];
            let p10 = &src[(y0 * swu + x1) * 4..(y0 * swu + x1) * 4 + 4];
            let p01 = &src[(y1 * swu + x0) * 4..(y1 * swu + x0) * 4 + 4];
            let p11 = &src[(y1 * swu + x1) * 4..(y1 * swu + x1) * 4 + 4];
            let w00 = (1.0 - fx) * (1.0 - fy);
            let w10 = fx * (1.0 - fy);
            let w01 = (1.0 - fx) * fy;
            let w11 = fx * fy;
            let o = &mut row[x * 4..x * 4 + 4];
            for c in 0..4 {
                let v = p00[c] as f32 * w00 + p10[c] as f32 * w10 + p01[c] as f32 * w01 + p11[c] as f32 * w11;
                o[c] = (v + 0.5) as u8;
            }
        }
    });
    out
}

/// Axis-aligned bounds `[x0, y0, x1, y1]` of the rectangle `r` (same
/// layout) mapped through `h`. When the rectangle crosses the horizon line
/// of `h` the bounds are infinite.
pub fn map_rect(h: &Homography, r: [f64; 4]) -> [f64; 4] {
    let corners = [[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]];
    let Some(q) = map_quad(h, &corners) else {
        return [f64::NEG_INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY];
    };
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for p in q {
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
    }
    b
}

/// The part of the reference frame covered by the warped moving image, as a
/// convex polygon in reference pixel coordinates (the moving image outline
/// mapped through `h` and clipped to the reference). Empty when they do not
/// overlap or `h` is degenerate.
pub fn overlap_polygon(h: &Homography, mw: u32, mh: u32, rw: u32, rh: u32) -> Vec<[f64; 2]> {
    if mw == 0 || mh == 0 || rw == 0 || rh == 0 {
        return Vec::new();
    }
    let (a, b) = (mw as f64 - 0.5, mh as f64 - 0.5);
    let corners = [[-0.5, -0.5], [a, -0.5], [a, b], [-0.5, b]];
    let Some(q) = map_quad(h, &corners) else {
        return Vec::new();
    };
    // Must stay a convex quad (any orientation).
    let mut sign = 0.0;
    for k in 0..4 {
        let c = cross3(q[k], q[(k + 1) % 4], q[(k + 2) % 4]);
        if !(c.abs() > 1e-9) || (sign != 0.0 && (c > 0.0) != (sign > 0.0)) {
            return Vec::new();
        }
        sign = c;
    }
    let (x1, y1) = (rw as f64 - 0.5, rh as f64 - 0.5);
    let mut poly = q.to_vec();
    poly = clip_poly(&poly, 0, 1.0, -0.5);
    poly = clip_poly(&poly, 0, -1.0, -x1);
    poly = clip_poly(&poly, 1, 1.0, -0.5);
    poly = clip_poly(&poly, 1, -1.0, -y1);
    if poly.len() < 3 {
        poly.clear();
    }
    poly
}

/// The common field of view as an axis-aligned rectangle `[x0, y0, x1, y1]`
/// in reference pixel coordinates: a large (near maximal area) rectangle
/// inside both the reference frame and the warped moving image, so a crop
/// to it holds content in both. `None` when there is no overlap. The same
/// region of the moving image is `h^-1` of this rectangle; use
/// [`pixel_bounds`] to turn it into whole pixels.
pub fn overlap_rect(h: &Homography, mw: u32, mh: u32, rw: u32, rh: u32) -> Option<[f64; 4]> {
    let poly = overlap_polygon(h, mw, mh, rw, rh);
    if poly.len() < 3 {
        return None;
    }
    let ymin = poly.iter().fold(f64::INFINITY, |m, p| m.min(p[1]));
    let ymax = poly.iter().fold(f64::NEG_INFINITY, |m, p| m.max(p[1]));
    if !(ymax - ymin > 1e-6) {
        return None;
    }
    // For a convex polygon the left edge x(y) is convex and the right edge
    // concave, so the widest x range valid over [y0, y1] is set by the spans
    // at y0 and y1 alone.
    let rect_at = |y0: f64, y1: f64| -> Option<([f64; 4], f64)> {
        if !(y1 > y0) || y0 < ymin || y1 > ymax {
            return None;
        }
        let (l0, r0) = span_at(&poly, y0)?;
        let (l1, r1) = span_at(&poly, y1)?;
        let l = l0.max(l1);
        let r = r0.min(r1);
        if !(r > l) {
            return None;
        }
        Some(([l, y0, r, y1], (r - l) * (y1 - y0)))
    };
    const N: usize = 96;
    let ys: Vec<f64> = (0..=N).map(|i| if i == N { ymax } else { ymin + (ymax - ymin) * i as f64 / N as f64 }).collect();
    let mut best: Option<([f64; 4], f64)> = None;
    let mut best_y = (ymin, ymax);
    for i in 0..N {
        for j in i + 1..=N {
            if let Some((r, a)) = rect_at(ys[i], ys[j]) {
                if best.is_none_or(|b| a > b.1) {
                    best = Some((r, a));
                    best_y = (ys[i], ys[j]);
                }
            }
        }
    }
    let (mut rect, mut area) = best?;
    let (mut y0, mut y1) = best_y;
    let mut step = (ymax - ymin) / N as f64;
    for _ in 0..80 {
        let mut improved = false;
        for (d0, d1) in [(step, 0.0), (-step, 0.0), (0.0, step), (0.0, -step), (step, step), (-step, -step)] {
            let (c0, c1) = ((y0 + d0).clamp(ymin, ymax), (y1 + d1).clamp(ymin, ymax));
            if let Some((r, a)) = rect_at(c0, c1) {
                if a > area {
                    rect = r;
                    area = a;
                    y0 = c0;
                    y1 = c1;
                    improved = true;
                }
            }
        }
        if !improved {
            step *= 0.5;
            if step < 1e-4 {
                break;
            }
        }
    }
    Some(rect)
}

/// Whole pixels whose centers lie inside `r` (`[x0, y0, x1, y1]` in pixel
/// coordinates), clamped to a `w` x `h` image, as `[x, y, width, height]`.
pub fn pixel_bounds(r: [f64; 4], w: u32, h: u32) -> Option<[u32; 4]> {
    if w == 0 || h == 0 || !r.iter().all(|v| v.is_finite()) {
        return None;
    }
    let x0 = r[0].ceil().max(0.0);
    let y0 = r[1].ceil().max(0.0);
    let x1 = r[2].floor().min(w as f64 - 1.0);
    let y1 = r[3].floor().min(h as f64 - 1.0);
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some([x0 as u32, y0 as u32, (x1 - x0) as u32 + 1, (y1 - y0) as u32 + 1])
}

// ---------------------------------------------------------------- input and resampling

fn check_input(buf: &[u8], w: u32, h: u32, what: &str) -> Result<(), AlignError> {
    if w == 0 || h == 0 {
        return Err(AlignError::BadInput(format!("{what} image is empty")));
    }
    let need = w as usize * h as usize * 4;
    if buf.len() < need {
        return Err(AlignError::BadInput(format!("{what} buffer holds {} bytes, {w}x{h} RGBA needs {need}", buf.len())));
    }
    Ok(())
}

fn working_size(w: u32, h: u32, max_side: u32) -> (usize, usize) {
    let m = w.max(h) as f64;
    let s = (max_side.max(64) as f64 / m).min(1.0);
    (((w as f64 * s).round() as usize).max(1), ((h as f64 * s).round() as usize).max(1))
}

fn diag(w: usize, h: usize) -> f64 {
    ((w * w + h * h) as f64).sqrt()
}

/// Full-resolution pixel coordinates to working-resolution ones (area
/// resampling keeps pixel areas aligned: x_w + 0.5 = f * (x + 0.5)).
fn pixel_scale(w: u32, h: u32, ww: usize, wh: usize) -> Homography {
    let fx = ww as f64 / w as f64;
    let fy = wh as f64 / h as f64;
    Homography([fx, 0.0, 0.5 * fx - 0.5, 0.0, fy, 0.5 * fy - 0.5, 0.0, 0.0, 1.0])
}

/// Area (box) resampling taps: output `i` covers input [i * s, (i + 1) * s).
struct Taps {
    start: Vec<usize>,
    off: Vec<usize>,
    w: Vec<f32>,
}

impl Taps {
    fn new(n_in: usize, n_out: usize) -> Taps {
        let scale = n_in as f64 / n_out as f64;
        let mut start = Vec::with_capacity(n_out);
        let mut off = Vec::with_capacity(n_out + 1);
        let mut w = Vec::with_capacity(n_out * 3);
        off.push(0);
        for i in 0..n_out {
            let a = i as f64 * scale;
            let b = ((i + 1) as f64 * scale).min(n_in as f64);
            let k0 = (a.floor() as usize).min(n_in - 1);
            let k1 = (b.ceil() as usize).clamp(k0 + 1, n_in);
            let span = (b - a).max(1e-9);
            start.push(k0);
            for k in k0..k1 {
                let lo = a.max(k as f64);
                let hi = b.min(k as f64 + 1.0);
                w.push(((hi - lo).max(0.0) / span) as f32);
            }
            off.push(w.len());
        }
        Taps { start, off, w }
    }

    fn get(&self, i: usize) -> (usize, &[f32]) {
        (self.start[i], &self.w[self.off[i]..self.off[i + 1]])
    }
}

/// Luma (BT.601 weights, 0..255) of an RGBA8 image, area resampled.
fn gray_from_rgba(rgba: &[u8], w: usize, h: usize, ow: usize, oh: usize) -> Vec<f32> {
    let tx = Taps::new(w, ow);
    let mut tmp = vec![0f32; ow * h];
    tmp.par_chunks_mut(ow).enumerate().for_each(|(y, row)| {
        let src = &rgba[y * w * 4..(y + 1) * w * 4];
        for (i, o) in row.iter_mut().enumerate() {
            let (k0, ws) = tx.get(i);
            let mut acc = 0f32;
            for (j, &wt) in ws.iter().enumerate() {
                let p = &src[(k0 + j) * 4..(k0 + j) * 4 + 3];
                acc += wt * (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32);
            }
            *o = acc;
        }
    });
    resize_vertical(tmp, ow, h, oh)
}

fn resize_vertical(src: Vec<f32>, w: usize, h: usize, oh: usize) -> Vec<f32> {
    if oh == h {
        return src;
    }
    let ty = Taps::new(h, oh);
    let mut out = vec![0f32; w * oh];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (k0, ws) = ty.get(y);
        for (j, &wt) in ws.iter().enumerate() {
            let s = &src[(k0 + j) * w..(k0 + j + 1) * w];
            for (o, &v) in row.iter_mut().zip(s) {
                *o += wt * v;
            }
        }
    });
    out
}

fn resize_area(src: &[f32], w: usize, h: usize, ow: usize, oh: usize) -> Vec<f32> {
    let tx = Taps::new(w, ow);
    let mut tmp = vec![0f32; ow * h];
    tmp.par_chunks_mut(ow).enumerate().for_each(|(y, row)| {
        let s = &src[y * w..(y + 1) * w];
        for (i, o) in row.iter_mut().enumerate() {
            let (k0, ws) = tx.get(i);
            let mut acc = 0f32;
            for (j, &wt) in ws.iter().enumerate() {
                acc += wt * s[k0 + j];
            }
            *o = acc;
        }
    });
    resize_vertical(tmp, ow, h, oh)
}

fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 2.5).ceil() as i32;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    for v in &mut k {
        *v /= s;
    }
    k
}

fn blur(src: &[f32], w: usize, h: usize, k: &[f32]) -> Vec<f32> {
    let r = (k.len() / 2) as isize;
    let mut tmp = vec![0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let s = &src[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0f32;
            for (j, &kv) in k.iter().enumerate() {
                let xx = (x as isize + j as isize - r).clamp(0, w as isize - 1) as usize;
                acc += kv * s[xx];
            }
            *o = acc;
        }
    });
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (j, &kv) in k.iter().enumerate() {
            let yy = (y as isize + j as isize - r).clamp(0, h as isize - 1) as usize;
            let s = &tmp[yy * w..(yy + 1) * w];
            for (o, &v) in row.iter_mut().zip(s) {
                *o += kv * v;
            }
        }
    });
    out
}

// ---------------------------------------------------------------- features

#[derive(Clone, Copy, Debug)]
struct Feature {
    /// Working-resolution (pyramid level 0) coordinates.
    x: f32,
    y: f32,
    /// Level scale (1 at level 0, smaller on coarser levels).
    scale: f32,
    desc: [u64; 4],
}

struct Kp {
    xi: usize,
    yi: usize,
    x: f32,
    y: f32,
}

/// Bresenham circle of radius 3 used by FAST.
const CIRCLE: [(i32, i32); 16] = [
    (0, -3),
    (1, -3),
    (2, -2),
    (3, -1),
    (3, 0),
    (3, 1),
    (2, 2),
    (1, 3),
    (0, 3),
    (-1, 3),
    (-2, 2),
    (-3, 1),
    (-3, 0),
    (-3, -1),
    (-2, -2),
    (-1, -3),
];

/// True when the 16-bit circular mask holds 9 contiguous set bits.
fn has_run9(m: u32) -> bool {
    let m = m | (m << 16);
    let mut r = m;
    for s in 1..9 {
        r &= m >> s;
    }
    r != 0
}

fn fast9(img: &[f32], w: usize, h: usize, t: f32) -> Vec<(u32, u32)> {
    if w <= 2 * BORDER || h <= 2 * BORDER {
        return Vec::new();
    }
    let offs: [isize; 16] = CIRCLE.map(|(dx, dy)| dy as isize * w as isize + dx as isize);
    let rows: Vec<Vec<(u32, u32)>> = (BORDER..h - BORDER)
        .into_par_iter()
        .map(|y| {
            let mut v = Vec::new();
            for x in BORDER..w - BORDER {
                let i = (y * w + x) as isize;
                let p = img[i as usize];
                let hi = p + t;
                let lo = p - t;
                let at = |k: usize| img[(i + offs[k]) as usize];
                let c = [at(0), at(4), at(8), at(12)];
                let nb = c.iter().filter(|&&q| q > hi).count();
                let nd = c.iter().filter(|&&q| q < lo).count();
                if nb < 2 && nd < 2 {
                    continue;
                }
                let mut bm = 0u32;
                let mut dm = 0u32;
                for k in 0..16 {
                    let q = at(k);
                    if q > hi {
                        bm |= 1 << k;
                    } else if q < lo {
                        dm |= 1 << k;
                    }
                }
                if has_run9(bm) || has_run9(dm) {
                    v.push((x as u32, y as u32));
                }
            }
            v
        })
        .collect();
    rows.concat()
}

/// Harris response over a 7x7 window (central differences).
fn harris(img: &[f32], w: usize, x: usize, y: usize) -> f32 {
    let (mut a, mut b, mut c) = (0f32, 0f32, 0f32);
    for yy in y - 3..=y + 3 {
        let row = yy * w;
        for xx in x - 3..=x + 3 {
            let i = row + xx;
            let ix = img[i + 1] - img[i - 1];
            let iy = img[i + w] - img[i - w];
            a += ix * ix;
            b += iy * iy;
            c += ix * iy;
        }
    }
    a * b - c * c - 0.04 * (a + b) * (a + b)
}

/// Vertex offset of the parabola through (-1, a), (0, c), (1, b).
fn subpixel(a: f32, c: f32, b: f32) -> f32 {
    let den = a - 2.0 * c + b;
    if den < 0.0 {
        (0.5 * (a - b) / den).clamp(-0.5, 0.5)
    } else {
        0.0
    }
}

/// FAST-9 corners ranked by Harris, non-max suppressed, spread over a grid.
fn detect_level(img: &[f32], w: usize, h: usize, n_target: usize) -> Vec<Kp> {
    if n_target == 0 || w < MIN_LEVEL || h < MIN_LEVEL {
        return Vec::new();
    }
    let mut corners = fast9(img, w, h, FAST_HI);
    if corners.len() < 2 * n_target {
        corners = fast9(img, w, h, FAST_LO);
    }
    if corners.is_empty() {
        return Vec::new();
    }
    let scores: Vec<f32> = corners.par_iter().map(|&(x, y)| harris(img, w, x as usize, y as usize)).collect();
    let mut map = vec![0f32; w * h];
    for (&(x, y), &s) in corners.iter().zip(&scores) {
        if s > 0.0 {
            map[y as usize * w + x as usize] = s;
        }
    }
    let wi = w as isize;
    let mut kept: Vec<usize> = (0..corners.len())
        .into_par_iter()
        .filter(|&k| {
            let s = scores[k];
            if !(s > 0.0) {
                return false;
            }
            let i = (corners[k].1 as usize * w + corners[k].0 as usize) as isize;
            for dy in [-1isize, 0, 1] {
                for dx in [-1isize, 0, 1] {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let j = i + dy * wi + dx;
                    let o = map[j as usize];
                    if o > s || (o == s && j < i) {
                        return false;
                    }
                }
            }
            true
        })
        .collect();
    kept.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(Ordering::Equal).then(a.cmp(&b)));

    // Grid bucketing: an even share per cell first, then the strongest rest.
    let gw = w.div_ceil(CELL);
    let gh = h.div_ceil(CELL);
    let cells = gw * gh;
    let cap = n_target.div_ceil(cells).max(1);
    let mut count = vec![0usize; cells];
    let mut chosen = Vec::with_capacity(n_target);
    let mut rest = Vec::new();
    for &k in &kept {
        if chosen.len() >= n_target {
            break;
        }
        let (x, y) = corners[k];
        let c = (y as usize / CELL) * gw + x as usize / CELL;
        if count[c] < cap {
            count[c] += 1;
            chosen.push(k);
        } else {
            rest.push(k);
        }
    }
    for &k in &rest {
        if chosen.len() >= n_target {
            break;
        }
        chosen.push(k);
    }

    chosen
        .iter()
        .map(|&k| {
            let (x, y) = (corners[k].0 as usize, corners[k].1 as usize);
            let s = scores[k];
            let ox = subpixel(harris(img, w, x - 1, y), s, harris(img, w, x + 1, y));
            let oy = subpixel(harris(img, w, x, y - 1), s, harris(img, w, x, y + 1));
            Kp { xi: x, yi: y, x: x as f32 + ox, y: y as f32 + oy }
        })
        .collect()
}

/// Orientation by intensity centroid over a disc of radius 15.
fn ic_angle(img: &[f32], w: usize, x: usize, y: usize) -> f32 {
    let (mut m10, mut m01) = (0f32, 0f32);
    for dy in -PATCH_R..=PATCH_R {
        let span = ((PATCH_R * PATCH_R - dy * dy) as f32).sqrt() as i32;
        let row = (y as i32 + dy) as usize * w;
        for dx in -span..=span {
            let v = img[row + (x as i32 + dx) as usize];
            m10 += dx as f32 * v;
            m01 += dy as f32 * v;
        }
    }
    m01.atan2(m10)
}

/// Deterministic pseudo-random generator (SplitMix64).
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Two independent standard normal samples (Box-Muller).
    fn gauss2(&mut self) -> (f64, f64) {
        let u1 = 1.0 - self.f64();
        let u2 = self.f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let t = std::f64::consts::TAU * u2;
        (r * t.cos(), r * t.sin())
    }
}

/// The BRIEF sampling pattern: 256 point pairs, isotropic Gaussian (sigma
/// 31 / 5) inside a disc of radius 14 so every rotation stays in the patch.
fn pattern() -> &'static [[f32; 4]; 256] {
    static P: OnceLock<[[f32; 4]; 256]> = OnceLock::new();
    P.get_or_init(|| {
        let mut rng = Rng::new(0x1F2E_3D4C_5B6A_7988);
        let point = |rng: &mut Rng| loop {
            let (a, b) = rng.gauss2();
            let (x, y) = (a * 6.2, b * 6.2);
            if x * x + y * y <= 14.0 * 14.0 {
                return (x as f32, y as f32);
            }
        };
        let mut p = [[0f32; 4]; 256];
        for e in p.iter_mut() {
            loop {
                let a = point(&mut rng);
                let b = point(&mut rng);
                if (a.0 - b.0).hypot(a.1 - b.1) >= 1.5 {
                    *e = [a.0, a.1, b.0, b.1];
                    break;
                }
            }
        }
        p
    })
}

/// Rotated BRIEF on the smoothed level image.
fn describe(sm: &[f32], w: usize, x: usize, y: usize, angle: f32) -> [u64; 4] {
    let (s, c) = angle.sin_cos();
    let base = (y * w + x) as isize;
    let wi = w as isize;
    let mut d = [0u64; 4];
    for (k, p) in pattern().iter().enumerate() {
        let ax = (c * p[0] - s * p[1]).round() as isize;
        let ay = (s * p[0] + c * p[1]).round() as isize;
        let bx = (c * p[2] - s * p[3]).round() as isize;
        let by = (s * p[2] + c * p[3]).round() as isize;
        let va = sm[(base + ay * wi + ax) as usize];
        let vb = sm[(base + by * wi + bx) as usize];
        if va < vb {
            d[k >> 6] |= 1u64 << (k & 63);
        }
    }
    d
}

/// Pyramid, corners, orientation and descriptors of one working image.
fn detect_features(g0: Vec<f32>, w: usize, h: usize, n: usize) -> Vec<Feature> {
    let mut levels: Vec<(Vec<f32>, usize, usize)> = vec![(g0, w, h)];
    for l in 1..LEVELS {
        let f = LEVEL_SCALE.powi(l as i32);
        let lw = (w as f64 / f).round() as usize;
        let lh = (h as f64 / f).round() as usize;
        if lw < MIN_LEVEL || lh < MIN_LEVEL {
            break;
        }
        let next = {
            let (p, pw, ph) = &levels[levels.len() - 1];
            resize_area(p, *pw, *ph, lw, lh)
        };
        levels.push((next, lw, lh));
    }
    let q = 1.0 / LEVEL_SCALE;
    let total: f64 = (0..levels.len()).map(|l| q.powi(l as i32)).sum();
    let budgets: Vec<usize> = (0..levels.len()).map(|l| (n as f64 * q.powi(l as i32) / total).round() as usize).collect();
    let kernel = gaussian_kernel(2.0);
    let per: Vec<Vec<Feature>> = levels
        .par_iter()
        .zip(budgets.par_iter())
        .map(|((img, lw, lh), &nb)| {
            let (lw, lh) = (*lw, *lh);
            let kps = detect_level(img, lw, lh, nb);
            if kps.is_empty() {
                return Vec::new();
            }
            let sm = blur(img, lw, lh, &kernel);
            let sx = lw as f32 / w as f32;
            let sy = lh as f32 / h as f32;
            kps.iter()
                .map(|k| {
                    let angle = ic_angle(img, lw, k.xi, k.yi);
                    Feature {
                        x: (k.x + 0.5) / sx - 0.5,
                        y: (k.y + 0.5) / sy - 0.5,
                        scale: 0.5 * (sx + sy),
                        desc: describe(&sm, lw, k.xi, k.yi, angle),
                    }
                })
                .collect()
        })
        .collect();
    per.concat()
}

// ---------------------------------------------------------------- matching

fn hamming(a: &[u64; 4], b: &[u64; 4]) -> u32 {
    (a[0] ^ b[0]).count_ones() + (a[1] ^ b[1]).count_ones() + (a[2] ^ b[2]).count_ones() + (a[3] ^ b[3]).count_ones()
}

/// Best match index and distance, and the second best distance among
/// features not at the best one's location (the same corner found on two
/// pyramid levels must not fail the ratio test).
fn best_two(q: &[u64; 4], set: &[Feature]) -> (usize, u32, u32) {
    let (mut bi, mut b1) = (usize::MAX, u32::MAX);
    for (j, f) in set.iter().enumerate() {
        let d = hamming(q, &f.desc);
        if d < b1 {
            b1 = d;
            bi = j;
        }
    }
    if bi == usize::MAX {
        return (bi, b1, u32::MAX);
    }
    let best = &set[bi];
    let r = 4.0 / best.scale;
    let r2 = r * r;
    let mut b2 = u32::MAX;
    for (j, f) in set.iter().enumerate() {
        if j == bi {
            continue;
        }
        let d = hamming(q, &f.desc);
        if d < b2 {
            let (dx, dy) = (f.x - best.x, f.y - best.y);
            if dx * dx + dy * dy > r2 {
                b2 = d;
            }
        }
    }
    (bi, b1, b2)
}

fn best_one(q: &[u64; 4], set: &[Feature]) -> usize {
    let (mut bi, mut b1) = (usize::MAX, u32::MAX);
    for (j, f) in set.iter().enumerate() {
        let d = hamming(q, &f.desc);
        if d < b1 {
            b1 = d;
            bi = j;
        }
    }
    bi
}

/// Ratio-tested, mutually best matches as (moving index, reference index).
fn match_features(mv: &[Feature], rf: &[Feature], ratio: f32) -> Vec<(usize, usize)> {
    let fwd: Vec<(usize, u32, u32)> = mv.par_iter().map(|f| best_two(&f.desc, rf)).collect();
    let bwd: Vec<usize> = rf.par_iter().map(|f| best_one(&f.desc, mv)).collect();
    fwd.iter()
        .enumerate()
        .filter_map(|(i, &(j, d1, d2))| {
            if j == usize::MAX || d1 > MAX_HAMMING || bwd[j] != i {
                return None;
            }
            if d2 != u32::MAX && d1 as f32 >= ratio * d2 as f32 {
                return None;
            }
            Some((i, j))
        })
        .collect()
}

// ---------------------------------------------------------------- geometry

/// A putative correspondence: moving point `s`, reference point `d`, least
/// squares weight `w`.
#[derive(Clone, Copy, Debug)]
struct Corr {
    s: [f64; 2],
    d: [f64; 2],
    w: f64,
}

/// Minimum spread of a sample in the moving and reference images.
struct Geo {
    min_s: f64,
    min_d: f64,
}

struct Fit {
    h: [f64; 9],
    inliers: Vec<usize>,
}

fn mul3(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut r = [0f64; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
        }
    }
    r
}

fn jacobian(h: &[f64; 9], p: [f64; 2]) -> [f64; 4] {
    let w = h[6] * p[0] + h[7] * p[1] + h[8];
    let x = (h[0] * p[0] + h[1] * p[1] + h[2]) / w;
    let y = (h[3] * p[0] + h[4] * p[1] + h[5]) / w;
    [(h[0] - x * h[6]) / w, (h[1] - x * h[7]) / w, (h[3] - y * h[6]) / w, (h[4] - y * h[7]) / w]
}

fn cross3(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Maps a quad; `None` when its corners straddle the horizon line of `h`.
fn map_quad(h: &Homography, c: &[[f64; 2]; 4]) -> Option<[[f64; 2]; 4]> {
    let m = &h.0;
    let mut pos = 0;
    let mut neg = 0;
    let mut out = [[0f64; 2]; 4];
    for (o, p) in out.iter_mut().zip(c) {
        let w = m[6] * p[0] + m[7] * p[1] + m[8];
        if w > 1e-12 {
            pos += 1;
        } else if w < -1e-12 {
            neg += 1;
        }
        *o = [(m[0] * p[0] + m[1] * p[1] + m[2]) / w, (m[3] * p[0] + m[4] * p[1] + m[5]) / w];
    }
    if (pos == 4 || neg == 4) && out.iter().all(|p| p[0].is_finite() && p[1].is_finite()) {
        Some(out)
    } else {
        None
    }
}

/// Sutherland-Hodgman against the half plane `sign * p[axis] + offset >= 0`.
fn clip_poly(poly: &[[f64; 2]], axis: usize, sign: f64, offset: f64) -> Vec<[f64; 2]> {
    let n = poly.len();
    let mut out = Vec::with_capacity(n + 2);
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let da = sign * a[axis] + offset;
        let db = sign * b[axis] + offset;
        if da >= 0.0 {
            out.push(a);
        }
        if (da >= 0.0) != (db >= 0.0) {
            let t = da / (da - db);
            out.push([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]);
        }
    }
    out
}

/// Horizontal extent of a convex polygon at height `y`.
fn span_at(poly: &[[f64; 2]], y: f64) -> Option<(f64, f64)> {
    let n = poly.len();
    let (mut l, mut r) = (f64::INFINITY, f64::NEG_INFINITY);
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        if (a[1] - y) * (b[1] - y) > 0.0 {
            continue;
        }
        if a[1] == b[1] {
            l = l.min(a[0].min(b[0]));
            r = r.max(a[0].max(b[0]));
        } else {
            let x = a[0] + (y - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
            l = l.min(x);
            r = r.max(x);
        }
    }
    if l <= r {
        Some((l, r))
    } else {
        None
    }
}

/// Dense linear solve with partial pivoting.
fn solve<const N: usize>(mut a: [[f64; N]; N], mut b: [f64; N]) -> Option<[f64; N]> {
    let scale = a.iter().flatten().fold(0f64, |m, v| m.max(v.abs()));
    if !scale.is_finite() || scale == 0.0 {
        return None;
    }
    for col in 0..N {
        let mut piv = col;
        for r in col + 1..N {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if !(a[piv][col].abs() > scale * 1e-13) {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for r in col + 1..N {
            let f = a[r][col] / a[col][col];
            if f != 0.0 {
                for c in col..N {
                    let t = f * a[col][c];
                    a[r][c] -= t;
                }
                let t = f * b[col];
                b[r] -= t;
            }
        }
    }
    let mut x = [0f64; N];
    for r in (0..N).rev() {
        let mut s = b[r];
        for c in r + 1..N {
            s -= a[r][c] * x[c];
        }
        x[r] = s / a[r][r];
    }
    if x.iter().all(|v| v.is_finite()) {
        Some(x)
    } else {
        None
    }
}

/// Weighted centroids of the subset (moving, reference).
fn centroids(pts: &[Corr], idx: &[usize]) -> Option<([f64; 2], [f64; 2], f64)> {
    let mut sw = 0.0;
    let mut cs = [0.0; 2];
    let mut cd = [0.0; 2];
    for &i in idx {
        let c = &pts[i];
        sw += c.w;
        cs[0] += c.w * c.s[0];
        cs[1] += c.w * c.s[1];
        cd[0] += c.w * c.d[0];
        cd[1] += c.w * c.d[1];
    }
    if !(sw > 0.0) {
        return None;
    }
    Some(([cs[0] / sw, cs[1] / sw], [cd[0] / sw, cd[1] / sw], sw))
}

/// Weighted least squares similarity (exact for 2 points).
fn fit_similarity(pts: &[Corr], idx: &[usize]) -> Option<[f64; 9]> {
    let (cs, cd, _) = centroids(pts, idx)?;
    let (mut re, mut im, mut den) = (0.0, 0.0, 0.0);
    for &i in idx {
        let c = &pts[i];
        let (sx, sy) = (c.s[0] - cs[0], c.s[1] - cs[1]);
        let (dx, dy) = (c.d[0] - cd[0], c.d[1] - cd[1]);
        re += c.w * (sx * dx + sy * dy);
        im += c.w * (sx * dy - sy * dx);
        den += c.w * (sx * sx + sy * sy);
    }
    if !(den > 1e-12) {
        return None;
    }
    let (a, b) = (re / den, im / den);
    let tx = cd[0] - (a * cs[0] - b * cs[1]);
    let ty = cd[1] - (b * cs[0] + a * cs[1]);
    Some([a, -b, tx, b, a, ty, 0.0, 0.0, 1.0])
}

/// Weighted least squares affine map (exact for 3 points).
fn fit_affine(pts: &[Corr], idx: &[usize]) -> Option<[f64; 9]> {
    let (cs, cd, _) = centroids(pts, idx)?;
    let mut m = [[0f64; 2]; 2];
    let mut rx = [0f64; 2];
    let mut ry = [0f64; 2];
    for &i in idx {
        let c = &pts[i];
        let (sx, sy) = (c.s[0] - cs[0], c.s[1] - cs[1]);
        let (dx, dy) = (c.d[0] - cd[0], c.d[1] - cd[1]);
        m[0][0] += c.w * sx * sx;
        m[0][1] += c.w * sx * sy;
        m[1][1] += c.w * sy * sy;
        rx[0] += c.w * sx * dx;
        rx[1] += c.w * sy * dx;
        ry[0] += c.w * sx * dy;
        ry[1] += c.w * sy * dy;
    }
    m[1][0] = m[0][1];
    let ab = solve(m, rx)?;
    let cdv = solve(m, ry)?;
    let tx = cd[0] - ab[0] * cs[0] - ab[1] * cs[1];
    let ty = cd[1] - cdv[0] * cs[0] - cdv[1] * cs[1];
    Some([ab[0], ab[1], tx, cdv[0], cdv[1], ty, 0.0, 0.0, 1.0])
}

/// Hartley normalization: centroid to the origin, mean distance sqrt 2.
fn norm_params(pts: &[Corr], idx: &[usize], dst: bool) -> (f64, f64, f64) {
    let pick = |i: usize| if dst { pts[i].d } else { pts[i].s };
    let n = idx.len() as f64;
    let (mut cx, mut cy) = (0.0, 0.0);
    for &i in idx {
        let p = pick(i);
        cx += p[0];
        cy += p[1];
    }
    cx /= n;
    cy /= n;
    let mut md = 0.0;
    for &i in idx {
        let p = pick(i);
        md += ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt();
    }
    md /= n;
    let k = if md > 1e-12 { std::f64::consts::SQRT_2 / md } else { 1.0 };
    (cx, cy, k)
}

/// Weighted normalized DLT with h33 = 1 (exact for 4 points).
fn fit_homography(pts: &[Corr], idx: &[usize]) -> Option<[f64; 9]> {
    if idx.len() < 4 {
        return None;
    }
    let (sx, sy, sk) = norm_params(pts, idx, false);
    let (dx, dy, dk) = norm_params(pts, idx, true);
    let mut ata = [[0f64; 8]; 8];
    let mut atb = [0f64; 8];
    for &i in idx {
        let c = &pts[i];
        let x = (c.s[0] - sx) * sk;
        let y = (c.s[1] - sy) * sk;
        let u = (c.d[0] - dx) * dk;
        let v = (c.d[1] - dy) * dk;
        let r1 = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y];
        let r2 = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y];
        for a in 0..8 {
            for b in 0..8 {
                ata[a][b] += c.w * (r1[a] * r1[b] + r2[a] * r2[b]);
            }
            atb[a] += c.w * (r1[a] * u + r2[a] * v);
        }
    }
    let hn = solve(ata, atb)?;
    let hn = [hn[0], hn[1], hn[2], hn[3], hn[4], hn[5], hn[6], hn[7], 1.0];
    let ts = [sk, 0.0, -sk * sx, 0.0, sk, -sk * sy, 0.0, 0.0, 1.0];
    let td_inv = [1.0 / dk, 0.0, dx, 0.0, 1.0 / dk, dy, 0.0, 0.0, 1.0];
    let h = mul3(&mul3(&td_inv, &hn), &ts);
    if !(h[8].abs() > 1e-12) {
        return None;
    }
    let s = h[8];
    let h = h.map(|v| v / s);
    if h.iter().all(|v| v.is_finite()) {
        Some(h)
    } else {
        None
    }
}

fn fit_model(model: Model, pts: &[Corr], idx: &[usize]) -> Option<[f64; 9]> {
    match model {
        Model::Similarity => fit_similarity(pts, idx),
        Model::Affine => fit_affine(pts, idx),
        Model::Homography => fit_homography(pts, idx),
    }
}

fn sample_size(model: Model) -> usize {
    match model {
        Model::Similarity => 2,
        Model::Affine => 3,
        Model::Homography => 4,
    }
}

fn min_inliers(model: Model) -> usize {
    match model {
        Model::Similarity => 8,
        Model::Affine => 10,
        Model::Homography => 15,
    }
}

/// Squared forward transfer error; infinite behind the horizon.
fn err2(h: &[f64; 9], c: &Corr) -> f64 {
    let (x, y) = (c.s[0], c.s[1]);
    let w = h[6] * x + h[7] * y + h[8];
    if !(w > 1e-9) {
        return f64::INFINITY;
    }
    let u = (h[0] * x + h[1] * y + h[2]) / w - c.d[0];
    let v = (h[3] * x + h[4] * y + h[5]) / w - c.d[1];
    u * u + v * v
}

fn tri_ok(pts: &[Corr], a: usize, b: usize, c: usize, g: &Geo) -> bool {
    let cs = cross3(pts[a].s, pts[b].s, pts[c].s);
    let cd = cross3(pts[a].d, pts[b].d, pts[c].d);
    cs.abs() > g.min_s * g.min_s && cd.abs() > g.min_d * g.min_d && (cs > 0.0) == (cd > 0.0)
}

/// Rejects collinear or orientation-flipping minimal samples.
fn good_sample(pts: &[Corr], idx: &[usize], g: &Geo) -> bool {
    match idx.len() {
        2 => {
            let (a, b) = (&pts[idx[0]], &pts[idx[1]]);
            (a.s[0] - b.s[0]).hypot(a.s[1] - b.s[1]) > g.min_s && (a.d[0] - b.d[0]).hypot(a.d[1] - b.d[1]) > g.min_d
        }
        3 => tri_ok(pts, idx[0], idx[1], idx[2], g),
        _ => [(0, 1, 2), (0, 1, 3), (0, 2, 3), (1, 2, 3)].iter().all(|&(a, b, c)| tri_ok(pts, idx[a], idx[b], idx[c], g)),
    }
}

fn inlier_set(h: &[f64; 9], pts: &[Corr], t2: f64) -> Vec<usize> {
    (0..pts.len()).filter(|&i| err2(h, &pts[i]) < t2).collect()
}

/// Iterations needed to draw one all-inlier sample with 99.5% probability.
fn adaptive_iterations(inlier_ratio: f64, k: usize, max: usize) -> usize {
    let floor = max.min(100);
    let p = inlier_ratio.powi(k as i32);
    if p >= 1.0 - 1e-12 {
        return floor;
    }
    if p <= 1e-12 {
        return max;
    }
    let n = ((1.0f64 - 0.995).ln() / (1.0 - p).ln()).ceil();
    if n.is_finite() {
        (n as usize).clamp(floor, max)
    } else {
        max
    }
}

/// RANSAC with MSAC scoring and least squares refinement.
fn ransac(pts: &[Corr], model: Model, thresh: f64, max_iter: usize, g: &Geo, cancel: Option<&CancelToken>) -> Result<Option<Fit>, AlignError> {
    let k = sample_size(model);
    let n = pts.len();
    if n < k + 1 {
        return Ok(None);
    }
    let t2 = thresh * thresh;
    let mut rng = Rng::new(0x5EED_A11C_0FFE_E000 ^ n as u64);
    let mut best: Option<([f64; 9], f64)> = None;
    let mut limit = max_iter;
    let mut it = 0;
    let mut idx = [0usize; 4];
    while it < limit {
        it += 1;
        if it % 128 == 0 && cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(AlignError::Cancelled);
        }
        for a in 0..k {
            loop {
                let r = rng.below(n);
                if !idx[..a].contains(&r) {
                    idx[a] = r;
                    break;
                }
            }
        }
        if !good_sample(pts, &idx[..k], g) {
            continue;
        }
        let Some(h) = fit_model(model, pts, &idx[..k]) else {
            continue;
        };
        let mut score = 0.0;
        let mut count = 0usize;
        for c in pts {
            let e = err2(&h, c);
            if e < t2 {
                count += 1;
                score += e;
            } else {
                score += t2;
            }
        }
        if best.as_ref().is_none_or(|b| score < b.1) {
            best = Some((h, score));
            limit = adaptive_iterations(count as f64 / n as f64, k, max_iter);
        }
    }
    let Some((mut h, _)) = best else {
        return Ok(None);
    };
    let mut inl = inlier_set(&h, pts, t2);
    for _ in 0..5 {
        if inl.len() < k {
            break;
        }
        let Some(h2) = fit_model(model, pts, &inl) else {
            break;
        };
        let inl2 = inlier_set(&h2, pts, t2);
        if (inl2.len() as f64) < 0.9 * inl.len() as f64 {
            break;
        }
        let same = inl2 == inl;
        h = h2;
        inl = inl2;
        if same {
            break;
        }
    }
    Ok(Some(Fit { h, inliers: inl }))
}

/// Rejects wild models at working resolution: corners behind the horizon,
/// strong perspective, flips, non-convex outlines, extreme scale or shear.
fn plausible(h: &[f64; 9], w: usize, hgt: usize) -> bool {
    if !h.iter().all(|v| v.is_finite()) {
        return false;
    }
    let (a, b) = ((w - 1).max(1) as f64, (hgt - 1).max(1) as f64);
    let corners = [[0.0, 0.0], [a, 0.0], [a, b], [0.0, b]];
    let mut den = [0f64; 4];
    let mut q = [[0f64; 2]; 4];
    for k in 0..4 {
        let p = corners[k];
        den[k] = h[6] * p[0] + h[7] * p[1] + h[8];
        if !(den[k] > 1e-9) {
            return false;
        }
        q[k] = [(h[0] * p[0] + h[1] * p[1] + h[2]) / den[k], (h[3] * p[0] + h[4] * p[1] + h[5]) / den[k]];
    }
    let dmax = den.iter().fold(0f64, |m, &v| m.max(v));
    let dmin = den.iter().fold(f64::INFINITY, |m, &v| m.min(v));
    if dmax / dmin > 2.5 {
        return false;
    }
    // Source corners run with positive cross products; so must the mapped
    // ones (convex, not mirrored).
    for k in 0..4 {
        if !(cross3(q[k], q[(k + 1) % 4], q[(k + 2) % 4]) > 0.0) {
            return false;
        }
    }
    let j = jacobian(h, [a * 0.5, b * 0.5]);
    let det = j[0] * j[3] - j[1] * j[2];
    if !(det > 0.0) {
        return false;
    }
    let scale = det.sqrt();
    if !(0.2..=5.0).contains(&scale) {
        return false;
    }
    let fro2 = j.iter().map(|v| v * v).sum::<f64>();
    let s1 = ((fro2 + (fro2 * fro2 - 4.0 * det * det).max(0.0).sqrt()) * 0.5).sqrt();
    let s2 = det / s1;
    s1 / s2 <= 2.5
}

#[cfg(test)]
mod tests {
    use super::*;

    const RW: u32 = 400;
    const RH: u32 = 300;
    const OFF: (f64, f64) = (300.0, 250.0);

    /// Procedural test scene: gradient background with random rotated
    /// rectangles, ellipses and triangles of many sizes.
    fn scene_image(w: usize, h: usize, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        let mut img = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                img[i] = (60 + 90 * x / w) as u8;
                img[i + 1] = (70 + 70 * y / h) as u8;
                img[i + 2] = 120;
                img[i + 3] = 255;
            }
        }
        for _ in 0..w * h / 500 {
            let kind = rng.below(3);
            let cx = rng.f64() * w as f64;
            let cy = rng.f64() * h as f64;
            let r = rng.f64();
            let a = 3.0 + r * r * 30.0;
            let b = a * (0.4 + 0.6 * rng.f64());
            let (sn, cs) = (rng.f64() * std::f64::consts::PI).sin_cos();
            let col = [rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8];
            let ext = a.max(b) + 1.0;
            let x0 = (cx - ext).floor().max(0.0) as usize;
            let x1 = ((cx + ext).ceil() as usize).min(w - 1);
            let y0 = (cy - ext).floor().max(0.0) as usize;
            let y1 = ((cy + ext).ceil() as usize).min(h - 1);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let dx = x as f64 - cx;
                    let dy = y as f64 - cy;
                    let u = cs * dx + sn * dy;
                    let v = -sn * dx + cs * dy;
                    let inside = match kind {
                        0 => u.abs() <= a && v.abs() <= b,
                        1 => (u / a).powi(2) + (v / b).powi(2) <= 1.0,
                        _ => v.abs() <= b && u.abs() <= a * (v + b) / (2.0 * b),
                    };
                    if inside {
                        let i = (y * w + x) * 4;
                        img[i..i + 3].copy_from_slice(&col);
                    }
                }
            }
        }
        img
    }

    fn scene() -> &'static (Vec<u8>, u32, u32) {
        static S: OnceLock<(Vec<u8>, u32, u32)> = OnceLock::new();
        S.get_or_init(|| (scene_image(1000, 800, 7), 1000, 800))
    }

    fn reference() -> Vec<u8> {
        let (s, w, h) = scene();
        warp_rgba(s, *w, *h, &Homography::translation(-OFF.0, -OFF.1), RW, RH)
    }

    /// Moving image whose pixel p shows the reference content at `t(p)`,
    /// with a brightness change and noise.
    fn moving_for(t: &Homography) -> Vec<u8> {
        let (s, w, h) = scene();
        let b = t.inverse().unwrap().compose(&Homography::translation(-OFF.0, -OFF.1));
        let mut m = warp_rgba(s, *w, *h, &b, RW, RH);
        let mut rng = Rng::new(99);
        for px in m.chunks_exact_mut(4) {
            for v in px.iter_mut().take(3) {
                let n = rng.below(7) as f64 - 3.0;
                *v = (*v as f64 * 1.08 + 12.0 + n).round().clamp(0.0, 255.0) as u8;
            }
        }
        m
    }

    fn about_center(inner: &Homography, tx: f64, ty: f64) -> Homography {
        let c = [(RW as f64 - 1.0) * 0.5, (RH as f64 - 1.0) * 0.5];
        Homography::translation(c[0] + tx, c[1] + ty).compose(inner).compose(&Homography::translation(-c[0], -c[1]))
    }

    fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
        (a[0] - b[0]).hypot(a[1] - b[1])
    }

    /// Worst error of `h` against the truth `t` over grid points of the
    /// moving image that land inside the reference.
    fn worst_error(h: &Homography, t: &Homography) -> (f64, usize) {
        let mut n = 0;
        let mut worst = 0f64;
        for gy in 0..7 {
            for gx in 0..9 {
                let p = [20.0 + gx as f64 * 45.0, 15.0 + gy as f64 * 45.0];
                let q = t.map(p);
                if q[0] < 8.0 || q[1] < 8.0 || q[0] > RW as f64 - 9.0 || q[1] > RH as f64 - 9.0 {
                    continue;
                }
                worst = worst.max(dist(h.map(p), q));
                n += 1;
            }
        }
        (worst, n)
    }

    fn check(t: &Homography, opts: &AlignOptions) -> AlignResult {
        let r = reference();
        let m = moving_for(t);
        let res = align_rgba(&r, RW, RH, &m, RW, RH, opts, None).unwrap_or_else(|e| panic!("align failed: {e}"));
        let (worst, n) = worst_error(&res.h, t);
        assert!(n >= 6, "only {n} check points");
        assert!(worst < 1.5, "worst error {worst:.3} px, {res:?}");
        res
    }

    #[test]
    fn identity_pair() {
        let r = reference();
        let res = align_rgba(&r, RW, RH, &r, RW, RH, &AlignOptions::default(), None).unwrap();
        let (worst, n) = worst_error(&res.h, &Homography::identity());
        assert!(n > 30);
        assert!(worst < 0.3, "worst {worst}, {res:?}");
        assert!(res.inliers > 50, "{res:?}");
        assert!(res.confidence > 0.5, "{res:?}");
        assert!((res.scale - 1.0).abs() < 0.01 && res.rotation_deg.abs() < 0.2, "{res:?}");
    }

    #[test]
    fn recovers_similarities() {
        let cases = [(0.6, 8.0, 10.0, -6.0), (1.6, -10.0, -12.0, 8.0), (1.25, 4.0, 20.0, 10.0), (0.8, -6.0, -15.0, 5.0), (1.0, 2.0, 30.0, -20.0)];
        for (s, deg, tx, ty) in cases {
            let t = about_center(&Homography::similarity(s, deg, 0.0, 0.0), tx, ty);
            let res = check(&t, &AlignOptions::default());
            assert!((res.scale / s - 1.0).abs() < 0.03, "scale {} vs {s}: {res:?}", res.scale);
            assert!((res.rotation_deg - deg).abs() < 1.0, "rotation {} vs {deg}: {res:?}", res.rotation_deg);
            assert!(res.confidence > 0.3, "{res:?}");
        }
    }

    #[test]
    fn recovers_mild_perspective() {
        let p = Homography([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 2.5e-4, -1.5e-4, 1.0]);
        let t = about_center(&Homography::similarity(1.1, 3.0, 0.0, 0.0).compose(&p), 8.0, -5.0);
        let res = check(&t, &AlignOptions::default());
        assert_eq!(res.model, Model::Homography);
    }

    #[test]
    fn similarity_and_affine_models() {
        let t = about_center(&Homography::similarity(0.9, -5.0, 0.0, 0.0), 6.0, 4.0);
        let res = check(&t, &AlignOptions { model: Model::Similarity, ..Default::default() });
        assert_eq!(res.model, Model::Similarity);
        let res = check(&t, &AlignOptions { model: Model::Affine, ..Default::default() });
        assert_eq!(res.model, Model::Affine);
    }

    #[test]
    fn flat_image_has_too_few_features() {
        let flat = vec![[120u8, 130, 140, 255]; 200 * 150].concat();
        let res = align_rgba(&flat, 200, 150, &flat, 200, 150, &AlignOptions::default(), None);
        assert_eq!(res.unwrap_err(), AlignError::TooFewFeatures);
    }

    #[test]
    fn bad_input_and_cancel() {
        let r = reference();
        let res = align_rgba(&r[..100], RW, RH, &r, RW, RH, &AlignOptions::default(), None);
        assert!(matches!(res, Err(AlignError::BadInput(_))));
        let res = align_rgba(&r, 0, RH, &r, RW, RH, &AlignOptions::default(), None);
        assert!(matches!(res, Err(AlignError::BadInput(_))));
        let tok = CancelToken::new();
        tok.cancel();
        let res = align_rgba(&r, RW, RH, &r, RW, RH, &AlignOptions::default(), Some(&tok));
        assert_eq!(res.unwrap_err(), AlignError::Cancelled);
    }

    #[test]
    fn homography_algebra() {
        let h = Homography([1.1, 0.05, 12.0, -0.03, 0.95, -7.0, 1e-4, -2e-4, 1.0]);
        let inv = h.inverse().unwrap();
        let a = Homography::similarity(1.3, 20.0, 5.0, -3.0);
        for p in [[0.0, 0.0], [100.0, 50.0], [399.0, 299.0], [-20.0, 310.0]] {
            assert!(dist(inv.map(h.map(p)), p) < 1e-9);
            assert!(dist(h.map(inv.map(p)), p) < 1e-9);
            assert!(dist(a.compose(&h).map(p), a.map(h.map(p))) < 1e-9);
        }
        let id = inv.compose(&h).normalized();
        for (x, y) in id.0.iter().zip(Homography::IDENTITY.0.iter()) {
            assert!((x - y).abs() < 1e-12, "{id:?}");
        }
        assert_eq!(Homography::identity().inverse(), Some(Homography::IDENTITY));
        assert!(Homography([0.0; 9]).inverse().is_none());
        assert!(Homography([1.0, 2.0, 3.0, 2.0, 4.0, 6.0, 0.0, 0.0, 1.0]).inverse().is_none());

        assert_eq!(map_rect(&Homography::translation(10.0, 20.0), [0.0, 0.0, 100.0, 50.0]), [10.0, 20.0, 110.0, 70.0]);
        let r = map_rect(&Homography::similarity(1.0, 90.0, 0.0, 0.0), [0.0, 0.0, 10.0, 20.0]);
        for (x, y) in r.iter().zip([-20.0, 0.0, 0.0, 10.0]) {
            assert!((x - y).abs() < 1e-9, "{r:?}");
        }
        let back = map_rect(&inv, map_rect(&h, [0.0, 0.0, 50.0, 40.0]));
        assert!(back[0] <= 1e-9 && back[1] <= 1e-9 && back[2] >= 50.0 - 1e-9 && back[3] >= 40.0 - 1e-9);
        let s = Homography::similarity(2.0, 30.0, 1.0, 1.0);
        assert!((s.scale_at([3.0, 4.0]) - 2.0).abs() < 1e-12);
        assert!((s.rotation_deg_at([3.0, 4.0]) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn warp_identity_and_translation_are_exact() {
        let mut rng = Rng::new(3);
        let (w, h) = (37usize, 23usize);
        let src: Vec<u8> = (0..w * h * 4).map(|_| rng.below(256) as u8).collect();
        assert_eq!(warp_rgba(&src, w as u32, h as u32, &Homography::identity(), w as u32, h as u32), src);
        let t = warp_rgba(&src, w as u32, h as u32, &Homography::translation(2.0, 3.0), w as u32, h as u32);
        for y in 0..h {
            for x in 0..w {
                let o = &t[(y * w + x) * 4..(y * w + x) * 4 + 4];
                if x >= 2 && y >= 3 {
                    let i = ((y - 3) * w + x - 2) * 4;
                    assert_eq!(o, &src[i..i + 4]);
                } else {
                    assert_eq!(o, &[0, 0, 0, 0]);
                }
            }
        }
        // Singular transforms and empty sources give a transparent frame.
        assert!(warp_rgba(&src, w as u32, h as u32, &Homography([0.0; 9]), 5, 4).iter().all(|&v| v == 0));
        assert_eq!(warp_rgba(&[], 0, 0, &Homography::identity(), 3, 2).len(), 24);
    }

    #[test]
    fn overlap_rectangles() {
        let h = Homography::translation(100.0, 50.0).compose(&Homography::scaling(0.5, 0.5));
        let r = overlap_rect(&h, 400, 300, 400, 300).unwrap();
        for (x, y) in r.iter().zip([99.75, 49.75, 299.75, 199.75]) {
            assert!((x - y).abs() < 1e-3, "{r:?}");
        }
        assert_eq!(pixel_bounds(r, 400, 300), Some([100, 50, 200, 150]));

        // Rotated and zoomed out: the rectangle lies inside both frames.
        let t = about_center(&Homography::similarity(1.2, 15.0, 0.0, 0.0), 20.0, -10.0);
        let r = overlap_rect(&t, RW, RH, RW, RH).unwrap();
        let inv = t.inverse().unwrap();
        let eps = 1e-6;
        for c in [[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]] {
            assert!(c[0] >= -0.5 - eps && c[0] <= RW as f64 - 0.5 + eps && c[1] >= -0.5 - eps && c[1] <= RH as f64 - 0.5 + eps);
            let m = inv.map(c);
            assert!(m[0] >= -0.5 - eps && m[0] <= RW as f64 - 0.5 + eps && m[1] >= -0.5 - eps && m[1] <= RH as f64 - 0.5 + eps, "{c:?} -> {m:?}");
        }
        assert!((r[2] - r[0]) * (r[3] - r[1]) > 0.3 * (RW * RH) as f64, "{r:?}");
        assert!(overlap_polygon(&t, RW, RH, RW, RH).len() >= 4);

        // Disjoint frames.
        assert!(overlap_rect(&Homography::translation(1000.0, 0.0), 100, 100, 100, 100).is_none());
    }

    #[test]
    fn timing_probe_12mp() {
        let (s, sw, sh) = scene();
        let (w, h) = (4000u32, 3000u32);
        let to_ref = Homography::scaling(10.0, 10.0).compose(&Homography::translation(-OFF.0, -OFF.1));
        let t0 = std::time::Instant::now();
        let r = warp_rgba(s, *sw, *sh, &to_ref, w, h);
        let warp_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let c = [(w as f64 - 1.0) * 0.5, (h as f64 - 1.0) * 0.5];
        let t = Homography::translation(c[0] + 60.0, c[1] - 40.0)
            .compose(&Homography::similarity(1.2, 5.0, 0.0, 0.0))
            .compose(&Homography::translation(-c[0], -c[1]));
        let m = warp_rgba(s, *sw, *sh, &t.inverse().unwrap().compose(&to_ref), w, h);
        let mut best = f64::INFINITY;
        let mut res = None;
        for _ in 0..3 {
            let t1 = std::time::Instant::now();
            res = Some(align_rgba(&r, w, h, &m, w, h, &AlignOptions::default(), None));
            best = best.min(t1.elapsed().as_secs_f64() * 1000.0);
        }
        let res = res.unwrap();
        let err = res.as_ref().ok().map(|a| dist(a.h.map(c), t.map(c)));
        panic!("TIMING align 12MP best {best:.1} ms, warp 12MP {warp_ms:.1} ms, center error {err:?}, {res:?}");
    }
}
