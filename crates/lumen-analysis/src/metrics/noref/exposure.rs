//! Exposure: clipping per channel, mean / median / percentiles of L*,
//! dynamic range in stops on linear luminance, and an 11-zone histogram
//! (equal L* bands, zone V holds middle gray).
//!
//! A sample counts as clipped within one 8-bit level of the end of its
//! range: 254 and up (blown) or 1 and down (black) in 8-bit terms, 65278 /
//! 257 at 16 bits, 254/255 and 1/255 for float RGB, and Y within one 8-bit
//! level of 16 / 235 (limited range) or 0 / max (full range) for YUV. JPEG
//! ringing and dithering leave clipped areas a level short of the end, so an
//! exact match would miss them. Highlights are clipped when any channel is
//! (a blown channel loses detail and shifts the hue), shadows when every
//! channel is (crushed to black); for RGB, `clip_lo_any_pct` and
//! `clip_hi_all_pct` give the other two readings, and `clip_{lo,hi}_{r,g,b}_pct`
//! each channel on its own.
//!
//! The map is the clipped share of each [`CLIP_TILE`] px tile in percent:
//! positive for highlights, negative for shadows (whichever share is
//! larger), no value where nothing is clipped.

use crate::color::lstar_to_y;
use crate::frame::{FrameCtx, FrameView, InputKind, Plane, PlaneView, Range, RgbData};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::stats::summarize;
use rayon::prelude::*;

/// Darkest luminance used for the dynamic range (caps it at 14 stops).
const DR_FLOOR: f64 = 1.0 / 16384.0;

/// Tile size of the clip map in pixels: small enough to outline a blown
/// window or sky, large enough to read as a share and to keep a 12 MP
/// map near 12,000 cells.
pub const CLIP_TILE: usize = 32;

/// Clip thresholds (low, high) of 8-, 16-bit and float RGB samples.
const LO8: u8 = 1;
const HI8: u8 = 254;
const LO16: u16 = 257;
const HI16: u16 = 65278;
const LOF: f32 = 1.0 / 255.0;
const HIF: f32 = 254.0 / 255.0;

/// Per-channel low/high clip counts of interleaved samples (`CH` channels):
/// `[lo_r, lo_g, lo_b, hi_r, hi_g, hi_b, all_lo, any_hi, any_lo, all_hi]`.
fn clip_rgb<T: Copy + PartialOrd + Sync, const CH: usize>(d: &[T], stride: usize, w: usize, h: usize, lo: T, hi: T) -> [u64; 10] {
    (0..h)
        .into_par_iter()
        .map(|y| {
            let row = &d[y * stride..y * stride + w * CH];
            // u32 per row (rows are far below 4 billion pixels), so the loop vectorizes.
            let mut c = [0u32; 10];
            for px in row.as_chunks::<CH>().0 {
                let l = [px[0] <= lo, px[1] <= lo, px[2] <= lo];
                let u = [px[0] >= hi, px[1] >= hi, px[2] >= hi];
                for k in 0..3 {
                    c[k] += l[k] as u32;
                    c[3 + k] += u[k] as u32;
                }
                c[6] += (l[0] & l[1] & l[2]) as u32;
                c[7] += (u[0] | u[1] | u[2]) as u32;
                c[8] += (l[0] | l[1] | l[2]) as u32;
                c[9] += (u[0] & u[1] & u[2]) as u32;
            }
            c.map(|v| v as u64)
        })
        .reduce(|| [0; 10], |a, b| std::array::from_fn(|i| a[i] + b[i]))
}

/// [`clip_rgb`] for 3 or 4 channels.
fn clip_any<T: Copy + PartialOrd + Sync>(d: &[T], stride: usize, w: usize, h: usize, ch: usize, lo: T, hi: T) -> [u64; 10] {
    if ch == 4 {
        clip_rgb::<T, 4>(d, stride, w, h, lo, hi)
    } else {
        clip_rgb::<T, 3>(d, stride, w, h, lo, hi)
    }
}

fn clip_plane<T: Copy + PartialOrd + Sync>(p: &Plane<'_, T>, lo: T, hi: T) -> (u64, u64) {
    (0..p.height)
        .into_par_iter()
        .map(|y| {
            let r = p.row(y);
            (r.iter().filter(|v| **v <= lo).count() as u64, r.iter().filter(|v| **v >= hi).count() as u64)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

/// Per-tile `[highlight, shadow, pixel]` counts of interleaved samples,
/// row-major over [`CLIP_TILE`] tiles (see the module notes).
fn tiles_rgb<T: Copy + PartialOrd + Sync, const CH: usize>(d: &[T], stride: usize, w: usize, h: usize, lo: T, hi: T) -> Vec<[u32; 3]> {
    let tw = w.div_ceil(CLIP_TILE);
    (0..h.div_ceil(CLIP_TILE))
        .into_par_iter()
        .flat_map_iter(|ty| {
            let mut c = vec![[0u32; 3]; tw];
            for y in ty * CLIP_TILE..((ty + 1) * CLIP_TILE).min(h) {
                let row = &d[y * stride..y * stride + w * CH];
                for (x, px) in row.as_chunks::<CH>().0.iter().enumerate() {
                    let t = &mut c[x / CLIP_TILE];
                    t[0] += (px[0] >= hi || px[1] >= hi || px[2] >= hi) as u32;
                    t[1] += (px[0] <= lo && px[1] <= lo && px[2] <= lo) as u32;
                    t[2] += 1;
                }
            }
            c
        })
        .collect()
}

/// [`tiles_rgb`] for 3 or 4 channels.
fn tiles_any<T: Copy + PartialOrd + Sync>(d: &[T], stride: usize, w: usize, h: usize, ch: usize, lo: T, hi: T) -> Vec<[u32; 3]> {
    if ch == 4 {
        tiles_rgb::<T, 4>(d, stride, w, h, lo, hi)
    } else {
        tiles_rgb::<T, 3>(d, stride, w, h, lo, hi)
    }
}

/// Per-tile counts of a luma plane.
fn tiles_plane<T: Copy + PartialOrd + Sync>(p: &Plane<'_, T>, lo: T, hi: T) -> Vec<[u32; 3]> {
    let (w, h) = (p.width, p.height);
    let tw = w.div_ceil(CLIP_TILE);
    (0..h.div_ceil(CLIP_TILE))
        .into_par_iter()
        .flat_map_iter(|ty| {
            let mut c = vec![[0u32; 3]; tw];
            for y in ty * CLIP_TILE..((ty + 1) * CLIP_TILE).min(h) {
                for (x, &v) in p.row(y).iter().enumerate() {
                    let t = &mut c[x / CLIP_TILE];
                    t[0] += (v >= hi) as u32;
                    t[1] += (v <= lo) as u32;
                    t[2] += 1;
                }
            }
            c
        })
        .collect()
}

/// The clipping values: the headline `clip_pct`, the worse of crushed
/// shadows and blown highlights (either one alone is an exposure problem),
/// then each side.
fn push_clip(r: &mut MetricResult, lo: f64, hi: f64) {
    let lower = Direction::LowerIsBetter;
    r.push("clip_pct", lo.max(hi), Unit::Percent, lower);
    r.push("clip_lo_pct", lo, Unit::Percent, lower);
    r.push("clip_hi_pct", hi, Unit::Percent, lower);
}

/// The clip map of a `w` x `h` frame from its tile counts: highlights red,
/// shadows blue (diverging, sign kept when pooled). The range is a little
/// wider than the values so a fully clipped tile gets a clear red or blue,
/// not the near-black ends of the colormap.
fn clip_heatmap(w: usize, h: usize, counts: &[[u32; 3]]) -> Heatmap {
    let data = counts
        .iter()
        .map(|c| {
            if c[2] == 0 || (c[0] == 0 && c[1] == 0) {
                f32::NAN
            } else if c[0] >= c[1] {
                100.0 * c[0] as f32 / c[2] as f32
            } else {
                -100.0 * c[1] as f32 / c[2] as f32
            }
        })
        .collect();
    let t = CLIP_TILE as f32;
    Heatmap::new(w.div_ceil(CLIP_TILE), h.div_ceil(CLIP_TILE), data, "Clipped share: + highlights, - shadows")
        .with_unit(Unit::Percent)
        .with_cell(t, t)
        .with_range(-133.0, 133.0)
        .with_min_span(133.0)
        .with_style(Colormap::RdBu, false, Pool::MaxAbs)
}

/// `exposure`
pub struct Exposure;

static EXPOSURE: MetricInfo = MetricInfo {
    id: "exposure",
    name: "Exposure",
    kind: MetricKind::NoReference,
    version: 4,
    inputs: &[InputKind::Lab, InputKind::NativePlanes],
    min_size: (1, 1),
    cost_ms_4k: 5.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["clip_pct"],
    description: "Clipping (worst of shadows and highlights first, then per channel), L* statistics, dynamic range in stops and an 11-zone histogram, with a map of the clipped share per tile",
};

impl Metric for Exposure {
    fn info(&self) -> &'static MetricInfo {
        &EXPOSURE
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&EXPOSURE, a)?;
        let lab = a.lab();
        let s = summarize(&lab.l, 0.0, 100.0, 16384);
        cancel.check()?;
        let n = (a.width() * a.height()) as f64;
        let pct = |c: u64| 100.0 * c as f64 / n;
        let mut r = MetricResult::new(&EXPOSURE);
        let lower = Direction::LowerIsBetter;
        let neutral = Direction::Neutral;
        match a.view() {
            FrameView::Rgb(v) => {
                let c = match v.data {
                    RgbData::U8(d) => clip_any(d, v.stride, v.width, v.height, v.channels, LO8, HI8),
                    RgbData::U16(d) => clip_any(d, v.stride, v.width, v.height, v.channels, LO16, HI16),
                    RgbData::F32(d) => clip_any(d, v.stride, v.width, v.height, v.channels, LOF, HIF),
                };
                push_clip(&mut r, pct(c[6]), pct(c[7]));
                if o.want_map {
                    let t = match v.data {
                        RgbData::U8(d) => tiles_any(d, v.stride, v.width, v.height, v.channels, LO8, HI8),
                        RgbData::U16(d) => tiles_any(d, v.stride, v.width, v.height, v.channels, LO16, HI16),
                        RgbData::F32(d) => tiles_any(d, v.stride, v.width, v.height, v.channels, LOF, HIF),
                    };
                    r.map = Some(clip_heatmap(v.width, v.height, &t));
                }
                for (k, ch) in ["r", "g", "b"].iter().enumerate() {
                    r.push(format!("clip_lo_{ch}_pct"), pct(c[k]), Unit::Percent, lower);
                    r.push(format!("clip_hi_{ch}_pct"), pct(c[3 + k]), Unit::Percent, lower);
                }
                r.push("clip_lo_any_pct", pct(c[8]), Unit::Percent, lower);
                r.push("clip_hi_all_pct", pct(c[9]), Unit::Percent, lower);
            }
            FrameView::Yuv(v) => {
                let sh = v.bit_depth.saturating_sub(8);
                let step = 1u32 << sh;
                let (lo, hi) = match v.range {
                    Range::Limited => ((16u32 << sh) + step, (235u32 << sh) - step),
                    Range::Full => (step, v.max_value().saturating_sub(step)),
                };
                let (cl, ch) = match v.y {
                    PlaneView::U8(p) => clip_plane(&p, lo.min(255) as u8, hi.min(255) as u8),
                    PlaneView::U16(p) => clip_plane(&p, lo.min(65535) as u16, hi.min(65535) as u16),
                };
                push_clip(&mut r, pct(cl), pct(ch));
                if o.want_map {
                    let (w, h) = (v.y.width(), v.y.height());
                    let t = match v.y {
                        PlaneView::U8(p) => tiles_plane(&p, lo.min(255) as u8, hi.min(255) as u8),
                        PlaneView::U16(p) => tiles_plane(&p, lo.min(65535) as u16, hi.min(65535) as u16),
                    };
                    r.map = Some(clip_heatmap(w, h, &t));
                }
            }
        }
        r.push("l_mean", s.mean, Unit::LStar, neutral);
        r.push("l_median", s.percentile(50.0), Unit::LStar, neutral);
        for (k, p) in [("l_p0_5", 0.5), ("l_p1", 1.0), ("l_p5", 5.0), ("l_p95", 95.0), ("l_p99", 99.0), ("l_p99_5", 99.5)] {
            r.push(k, s.percentile(p), Unit::LStar, neutral);
        }
        let ylo = lstar_to_y(s.percentile(0.5)).max(DR_FLOOR);
        let yhi = lstar_to_y(s.percentile(99.5)).max(DR_FLOOR);
        r.push("dr_stops", (yhi / ylo).log2(), Unit::Stops, Direction::HigherIsBetter);
        for z in 0..11 {
            let (a0, a1) = (z as f32 * 100.0 / 11.0, (z + 1) as f32 * 100.0 / 11.0);
            r.push(format!("zone_{z}"), 100.0 * s.fraction_between(a0, if z == 10 { 101.0 } else { a1 }), Unit::Percent, neutral);
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::noref::test_util::{rgb_frame, run};

    #[test]
    fn clipping_per_channel_is_exact() {
        // 40 x 10 pixels, classes by column (each column is 0.25%).
        let f = rgb_frame(40, 10, |x, _| match x {
            0..4 => [255, 128, 128],  // red blown: 10%
            4..6 => [254, 254, 254],  // every channel at 254 counts: 5%
            6..10 => [253, 253, 253], // a level short: not clipped
            10..12 => [0, 1, 1],      // black: 5%
            12..15 => [2, 0, 2],      // green crushed alone: 7.5%
            15..18 => [2, 2, 2],      // not clipped
            _ => [128, 128, 128],
        });
        let r = run(&Exposure, &f.view(), &MetricOptions::default());
        let v = |k: &str| r.get(k).unwrap_or_else(|| panic!("no {k}"));
        assert_eq!(v("clip_hi_pct"), 15.0);
        assert_eq!(v("clip_hi_all_pct"), 5.0);
        assert_eq!((v("clip_hi_r_pct"), v("clip_hi_g_pct"), v("clip_hi_b_pct")), (15.0, 5.0, 5.0));
        assert_eq!(v("clip_lo_pct"), 5.0);
        assert_eq!(v("clip_lo_any_pct"), 12.5);
        assert_eq!((v("clip_lo_r_pct"), v("clip_lo_g_pct"), v("clip_lo_b_pct")), (5.0, 12.5, 5.0));
        assert_eq!(v("clip_pct"), 15.0);
    }

    #[test]
    fn map_holds_the_clipped_share_per_tile() {
        // 2 x 2 tiles: blown, half crushed, a quarter with a blown channel, clean.
        let t = CLIP_TILE;
        let f = rgb_frame(2 * t, 2 * t, |x, y| match (x / t, y / t) {
            (0, 0) => [255, 255, 255],
            (1, 0) if y % t < t / 2 => [0, 0, 0],
            (0, 1) if x % t < t / 4 => [255, 0, 0],
            _ => [120, 120, 120],
        });
        let r = run(&Exposure, &f.view(), &MetricOptions::default());
        let m = r.map.unwrap();
        assert_eq!((m.width, m.height, m.cell_w), (2, 2, t as f32));
        assert_eq!(&m.data[..3], &[100.0, -50.0, 25.0]);
        assert!(m.data[3].is_nan());
    }
}
