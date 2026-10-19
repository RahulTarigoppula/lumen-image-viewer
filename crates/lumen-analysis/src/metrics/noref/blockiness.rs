//! Blockiness: ratio of the mean luma gradient across 8x8 block boundaries
//! to the mean gradient inside blocks, horizontally and vertically, with a
//! search over the grid offset 0..7 (Wang, Sheikh, Bovik 2002 family; FFmpeg
//! `blockdetect` is similar). Score = mean of both ratios minus 1, floored
//! at 0: 0 means no visible grid, 0.3 means boundary steps 30% stronger.

use crate::frame::{FrameCtx, InputKind};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;

const B: usize = 8;
/// Added to both means (8-bit levels) so flat areas read as ratio 1.
const EPS: f64 = 0.5;

/// Per-tile gradient sums by phase: `h[k]` sums |I(x) - I(x-1)| over x with
/// x % 8 == k, `v[k]` sums |I(y) - I(y-1)| over rows with y % 8 == k.
#[derive(Clone, Copy, Default)]
struct Phases {
    h: [f64; B],
    hn: [u64; B],
    v: [f64; B],
    vn: [u64; B],
}

impl Phases {
    fn add(&mut self, o: &Phases) {
        for k in 0..B {
            self.h[k] += o.h[k];
            self.hn[k] += o.hn[k];
            self.v[k] += o.v[k];
            self.vn[k] += o.vn[k];
        }
    }
}

/// Boundary / interior mean ratio for phase sums at `offset`.
fn ratio(s: &[f64; B], n: &[u64; B], offset: usize) -> f64 {
    let (mut is, mut inn) = (0.0, 0u64);
    for k in 0..B {
        if k != offset {
            is += s[k];
            inn += n[k];
        }
    }
    if n[offset] == 0 || inn == 0 {
        return 1.0;
    }
    (s[offset] / n[offset] as f64 + EPS) / (is / inn as f64 + EPS)
}

fn best(s: &[f64; B], n: &[u64; B]) -> (usize, f64) {
    (0..B).map(|o| (o, ratio(s, n, o))).fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a })
}

/// `blockiness`
pub struct Blockiness;

static BLOCKINESS: MetricInfo = MetricInfo {
    id: "blockiness",
    name: "Blockiness",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (16, 16),
    cost_ms_4k: 10.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["score"],
    description: "8x8 block boundary to interior gradient ratio with grid offset search, with a tile map",
};

impl Metric for Blockiness {
    fn info(&self) -> &'static MetricInfo {
        &BLOCKINESS
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&BLOCKINESS, a)?;
        let l = a.luma();
        let (w, h) = (l.width, l.height);
        // Tiles are a multiple of 8 so phases inside a tile line up with the frame.
        let tile = ((o.tile as usize).max(B) / B) * B;
        let (tw, th) = (w.div_ceil(tile), h.div_ceil(tile));
        let tiles: Vec<Vec<Phases>> = (0..th)
            .into_par_iter()
            .map(|ty| {
                let mut acc = vec![Phases::default(); tw];
                if cancel.is_cancelled() {
                    return acc;
                }
                let mut dh = vec![0f32; w];
                let mut dv = vec![0f32; w];
                for y in ty * tile..((ty + 1) * tile).min(h) {
                    let row = l.row(y);
                    // dh[x] = |I(x) - I(x-1)|, dh[0] = 0 (no left neighbour).
                    dh[0] = 0.0;
                    for x in 1..w {
                        dh[x] = (row[x] - row[x - 1]).abs() * 255.0;
                    }
                    let has_up = y > 0;
                    if has_up {
                        let up = l.row(y - 1);
                        for x in 0..w {
                            dv[x] = (row[x] - up[x]).abs() * 255.0;
                        }
                    }
                    let yk = y % B;
                    for (t, p) in acc.iter_mut().enumerate() {
                        let x0 = t * tile;
                        let x1 = ((t + 1) * tile).min(w);
                        let seg = &dh[x0..x1];
                        let mut lanes = [0f32; B];
                        let full = seg.len() / B * B;
                        for c in seg[..full].as_chunks::<B>().0 {
                            for k in 0..B {
                                lanes[k] += c[k];
                            }
                        }
                        for (i, v) in seg[full..].iter().enumerate() {
                            lanes[(full + i) % B] += v;
                        }
                        for k in 0..B {
                            p.h[k] += lanes[k] as f64;
                            // Phase k holds columns x0 + k, x0 + k + 8, ...; x = 0 has no gradient.
                            let cnt = (x1 - x0).saturating_sub(k).div_ceil(B) as u64;
                            p.hn[k] += cnt - (x0 == 0 && k == 0) as u64;
                        }
                        if has_up {
                            p.v[yk] += dv[x0..x1].iter().sum::<f32>() as f64;
                            p.vn[yk] += (x1 - x0) as u64;
                        }
                    }
                }
                acc
            })
            .collect();
        cancel.check()?;
        let tiles: Vec<Phases> = tiles.into_iter().flatten().collect();
        let mut g = Phases::default();
        for t in &tiles {
            g.add(t);
        }
        let (ox, rh) = best(&g.h, &g.hn);
        let (oy, rv) = best(&g.v, &g.vn);
        let score = |rh: f64, rv: f64| ((rh + rv) / 2.0 - 1.0).max(0.0);
        let mut r = MetricResult::new(&BLOCKINESS);
        r.push("score", score(rh, rv), Unit::Score, Direction::LowerIsBetter);
        r.push("ratio_h", rh, Unit::Score, Direction::LowerIsBetter);
        r.push("ratio_v", rv, Unit::Score, Direction::LowerIsBetter);
        r.push("offset_x", ox as f64, Unit::Pixels, Direction::Neutral);
        r.push("offset_y", oy as f64, Unit::Pixels, Direction::Neutral);
        if o.want_map {
            let data = tiles.iter().map(|t| score(ratio(&t.h, &t.hn, ox), ratio(&t.v, &t.vn, oy)) as f32).collect();
            let t = tile as f32;
            r.map = Some(Heatmap::new(tw, th, data, "Blockiness").with_unit(Unit::Score).with_range(0.0, 0.5).with_min_span(0.3).with_cell(t, t).with_style(Colormap::Magma, false, Pool::Max));
        }
        Ok(r)
    }
}
