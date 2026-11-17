//! No-reference diagnostics on a single image or frame (luma `f32` or Lab).
//! Values in "levels" are 8-bit-equivalent code values whatever the source depth.

pub mod banding;
pub mod blockiness;
pub mod cast;
pub mod exposure;
pub mod noise;
pub mod sharpness;
pub mod visual_noise;

use crate::frame::PlaneF32;
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::metrics::Unit;
use rayon::prelude::*;

/// Sum, sum of squares and count of one tile.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TileAcc {
    pub sum: f64,
    pub sq: f64,
    pub n: u64,
}

impl TileAcc {
    pub fn add(&mut self, o: &TileAcc) {
        self.sum += o.sum;
        self.sq += o.sq;
        self.n += o.n;
    }

    pub fn mean(&self) -> f64 {
        if self.n == 0 {
            f64::NAN
        } else {
            self.sum / self.n as f64
        }
    }

    pub fn var(&self) -> f64 {
        if self.n == 0 {
            return f64::NAN;
        }
        let m = self.mean();
        (self.sq / self.n as f64 - m * m).max(0.0)
    }
}

/// Tile grid of interior-pixel statistics for a 3x3 stencil.
pub(crate) struct Tiles {
    pub tw: usize,
    pub th: usize,
    pub tile: usize,
    pub acc: Vec<TileAcc>,
}

impl Tiles {
    pub fn total(&self) -> TileAcc {
        let mut t = TileAcc::default();
        for a in &self.acc {
            t.add(a);
        }
        t
    }

    /// Tile map of `f(tile)`, positioned over the frame.
    pub fn map(&self, f: impl Fn(&TileAcc) -> f64, label: &str, unit: Unit) -> Heatmap {
        let data = self.acc.iter().map(|t| f(t) as f32).collect();
        let t = self.tile as f32;
        Heatmap::new(self.tw, self.th, data, label).with_unit(unit).with_cell(t, t).with_style(Colormap::Viridis, false, Pool::Max)
    }
}

/// Runs a 3x3 stencil over the interior of `l`. `f(up, mid, down, out)` writes
/// `out[x - 1]` for x in 1..w-1 (NaN to exclude a pixel); results are
/// accumulated per `tile` x `tile` tile in parallel over tile rows.
pub(crate) fn tile_stencil(l: &PlaneF32, tile: usize, f: impl Fn(&[f32], &[f32], &[f32], &mut [f32]) + Sync) -> Tiles {
    let (w, h) = (l.width, l.height);
    let tile = tile.max(8);
    let (tw, th) = (w.div_ceil(tile), h.div_ceil(tile));
    let rows: Vec<Vec<TileAcc>> = (0..th)
        .into_par_iter()
        .map(|ty| {
            let mut acc = vec![TileAcc::default(); tw];
            if w < 3 || h < 3 {
                return acc;
            }
            let mut buf = vec![0f32; w - 2];
            for y in (ty * tile).max(1)..((ty + 1) * tile).min(h - 1) {
                f(l.row(y - 1), l.row(y), l.row(y + 1), &mut buf);
                for (t, a) in acc.iter_mut().enumerate() {
                    let x0 = (t * tile).max(1);
                    let x1 = ((t + 1) * tile).min(w - 1);
                    if x1 <= x0 {
                        continue;
                    }
                    let (mut s, mut q, mut n) = (0f32, 0f32, 0u64);
                    for &v in &buf[x0 - 1..x1 - 1] {
                        if v.is_nan() {
                            continue;
                        }
                        s += v;
                        q += v * v;
                        n += 1;
                    }
                    a.sum += s as f64;
                    a.sq += q as f64;
                    a.n += n;
                }
            }
            acc
        })
        .collect();
    Tiles { tw, th, tile, acc: rows.into_iter().flatten().collect() }
}

/// Sobel gradients at x (1..w-1) of three rows, unscaled.
#[inline(always)]
pub(crate) fn sobel(up: &[f32], mid: &[f32], dn: &[f32], x: usize) -> (f32, f32) {
    let gx = (up[x + 1] + 2.0 * mid[x + 1] + dn[x + 1]) - (up[x - 1] + 2.0 * mid[x - 1] + dn[x - 1]);
    let gy = (dn[x - 1] + 2.0 * dn[x] + dn[x + 1]) - (up[x - 1] + 2.0 * up[x] + up[x + 1]);
    (gx, gy)
}

/// Helpers for the unit tests of the no-reference metrics.
#[cfg(test)]
pub(crate) mod test_util {
    use crate::frame::{FrameCtx, FrameView, OwnedFrame};
    use crate::job::CancelToken;
    use crate::metrics::{Metric, MetricOptions, MetricResult};

    /// xorshift64* PRNG (deterministic, no dependency).
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Rng(seed.max(1))
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn uniform(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Standard normal (Box-Muller).
        pub fn normal(&mut self) -> f64 {
            let u1 = self.uniform().max(1e-12);
            let u2 = self.uniform();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// Runs a no-reference metric on `view`.
    pub fn run(m: &dyn Metric, view: &FrameView<'_>, o: &MetricOptions) -> MetricResult {
        let ctx = FrameCtx::new(*view);
        m.compute(&ctx, None, o, &CancelToken::new()).expect("metric failed")
    }

    /// An RGBA8 frame of `w` x `h` pixels from `f(x, y) -> [r, g, b]`.
    pub fn rgb_frame(w: usize, h: usize, mut f: impl FnMut(usize, usize) -> [u8; 3]) -> OwnedFrame {
        let mut d = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let p = f(x, y);
                d.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        OwnedFrame::rgba8(w, h, d)
    }
}
