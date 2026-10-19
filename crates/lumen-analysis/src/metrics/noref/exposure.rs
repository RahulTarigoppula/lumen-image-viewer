//! Exposure: clipping per channel, mean / median / percentiles of L*,
//! dynamic range in stops on linear luminance, and an 11-zone histogram
//! (equal L* bands, zone V holds middle gray). The map marks the clipped
//! pixels: 1 for blown highlights (a channel at full white), -1 for crushed
//! shadows (every channel black), no value elsewhere.

use crate::color::lstar_to_y;
use crate::frame::{FrameCtx, FrameView, InputKind, Plane, PlaneView, Range, RgbData};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::stats::summarize;
use rayon::prelude::*;

/// Darkest luminance used for the dynamic range (caps it at 14 stops).
const DR_FLOOR: f64 = 1.0 / 16384.0;

/// Per-channel low/high clip counts of interleaved samples (`CH` channels):
/// `[lo_r, lo_g, lo_b, hi_r, hi_g, hi_b, all_lo, any_hi]`.
fn clip_rgb<T: Copy + PartialOrd + Sync, const CH: usize>(d: &[T], stride: usize, w: usize, h: usize, lo: T, hi: T) -> [u64; 8] {
    (0..h)
        .into_par_iter()
        .map(|y| {
            let row = &d[y * stride..y * stride + w * CH];
            // u32 per row (rows are far below 4 billion pixels), so the loop vectorizes.
            let mut c = [0u32; 8];
            for px in row.as_chunks::<CH>().0 {
                let l = [px[0] <= lo, px[1] <= lo, px[2] <= lo];
                let u = [px[0] >= hi, px[1] >= hi, px[2] >= hi];
                for k in 0..3 {
                    c[k] += l[k] as u32;
                    c[3 + k] += u[k] as u32;
                }
                c[6] += (l[0] & l[1] & l[2]) as u32;
                c[7] += (u[0] | u[1] | u[2]) as u32;
            }
            c.map(|v| v as u64)
        })
        .reduce(|| [0; 8], |a, b| std::array::from_fn(|i| a[i] + b[i]))
}

/// [`clip_rgb`] for 3 or 4 channels.
fn clip_any<T: Copy + PartialOrd + Sync>(d: &[T], stride: usize, w: usize, h: usize, ch: usize, lo: T, hi: T) -> [u64; 8] {
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

/// Clip map of interleaved samples (see the module notes).
fn map_rgb<T: Copy + PartialOrd + Sync, const CH: usize>(d: &[T], stride: usize, w: usize, h: usize, lo: T, hi: T) -> Vec<f32> {
    let mut m = vec![f32::NAN; w * h];
    m.par_chunks_mut(w.max(1)).enumerate().take(h).for_each(|(y, out)| {
        let row = &d[y * stride..y * stride + w * CH];
        for (o, px) in out.iter_mut().zip(row.as_chunks::<CH>().0) {
            if px[0] >= hi || px[1] >= hi || px[2] >= hi {
                *o = 1.0;
            } else if px[0] <= lo && px[1] <= lo && px[2] <= lo {
                *o = -1.0;
            }
        }
    });
    m
}

/// [`map_rgb`] for 3 or 4 channels.
fn map_any<T: Copy + PartialOrd + Sync>(d: &[T], stride: usize, w: usize, h: usize, ch: usize, lo: T, hi: T) -> Vec<f32> {
    if ch == 4 {
        map_rgb::<T, 4>(d, stride, w, h, lo, hi)
    } else {
        map_rgb::<T, 3>(d, stride, w, h, lo, hi)
    }
}

/// Clip map of a luma plane.
fn map_plane<T: Copy + PartialOrd + Sync>(p: &Plane<'_, T>, lo: T, hi: T) -> Vec<f32> {
    let mut m = vec![f32::NAN; p.width * p.height];
    m.par_chunks_mut(p.width.max(1)).enumerate().take(p.height).for_each(|(y, out)| {
        for (o, &v) in out.iter_mut().zip(p.row(y)) {
            if v >= hi {
                *o = 1.0;
            } else if v <= lo {
                *o = -1.0;
            }
        }
    });
    m
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

/// The clip map: highlights red, shadows blue (diverging, sign kept when
/// pooled). The range is a little wider than the values so they get a clear
/// red and blue, not the near-black ends of the colormap.
fn clip_heatmap(w: usize, h: usize, m: Vec<f32>) -> Heatmap {
    Heatmap::new(w, h, m, "Clipped: 1 highlight, -1 shadow").with_unit(Unit::Count).with_range(-1.33, 1.33).with_min_span(1.33).with_style(Colormap::RdBu, false, Pool::MaxAbs)
}

/// `exposure`
pub struct Exposure;

static EXPOSURE: MetricInfo = MetricInfo {
    id: "exposure",
    name: "Exposure",
    kind: MetricKind::NoReference,
    version: 3,
    inputs: &[InputKind::Lab, InputKind::NativePlanes],
    min_size: (1, 1),
    cost_ms_4k: 4.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["clip_pct"],
    description: "Clipping (worst of shadows and highlights first), L* statistics, dynamic range in stops and an 11-zone histogram, with a map of the clipped pixels",
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
                    RgbData::U8(d) => clip_any(d, v.stride, v.width, v.height, v.channels, 0u8, 255u8),
                    RgbData::U16(d) => clip_any(d, v.stride, v.width, v.height, v.channels, 0u16, 65535u16),
                    RgbData::F32(d) => clip_any(d, v.stride, v.width, v.height, v.channels, 0f32, 1f32),
                };
                push_clip(&mut r, pct(c[6]), pct(c[7]));
                if o.want_map {
                    let m = match v.data {
                        RgbData::U8(d) => map_any(d, v.stride, v.width, v.height, v.channels, 0u8, 255u8),
                        RgbData::U16(d) => map_any(d, v.stride, v.width, v.height, v.channels, 0u16, 65535u16),
                        RgbData::F32(d) => map_any(d, v.stride, v.width, v.height, v.channels, 0f32, 1f32),
                    };
                    r.map = Some(clip_heatmap(v.width, v.height, m));
                }
                for (k, ch) in ["r", "g", "b"].iter().enumerate() {
                    r.push(format!("clip_lo_{ch}_pct"), pct(c[k]), Unit::Percent, lower);
                    r.push(format!("clip_hi_{ch}_pct"), pct(c[3 + k]), Unit::Percent, lower);
                }
            }
            FrameView::Yuv(v) => {
                let sh = v.bit_depth.saturating_sub(8);
                let (lo, hi) = match v.range {
                    Range::Limited => (16u32 << sh, 235u32 << sh),
                    Range::Full => (0, v.max_value()),
                };
                let (cl, ch) = match v.y {
                    PlaneView::U8(p) => clip_plane(&p, lo as u8, hi.min(255) as u8),
                    PlaneView::U16(p) => clip_plane(&p, lo as u16, hi.min(65535) as u16),
                };
                push_clip(&mut r, pct(cl), pct(ch));
                if o.want_map {
                    let (w, h) = (v.y.width(), v.y.height());
                    let m = match v.y {
                        PlaneView::U8(p) => map_plane(&p, lo as u8, hi.min(255) as u8),
                        PlaneView::U16(p) => map_plane(&p, lo as u16, hi.min(65535) as u16),
                    };
                    r.map = Some(clip_heatmap(w, h, m));
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
