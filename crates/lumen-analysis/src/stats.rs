//! Histogram-based summaries (mean, min, max, percentiles) of large `f32`
//! buffers, computed in parallel without sorting or copying.

use rayon::prelude::*;

const CHUNK: usize = 1 << 18;

/// Summary of a sample set; percentiles come from a fixed-range histogram.
#[derive(Clone, Debug)]
pub struct Summary {
    pub count: u64,
    pub mean: f64,
    pub min: f32,
    pub max: f32,
    lo: f32,
    hi: f32,
    hist: Vec<u64>,
}

impl Summary {
    /// Value at percentile `p` (0..100), linearly interpolated inside the
    /// histogram bin and clamped to the observed min/max.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.count == 0 {
            return f64::NAN;
        }
        let target = (p.clamp(0.0, 100.0) / 100.0) * self.count as f64;
        let bw = (self.hi - self.lo) as f64 / self.hist.len() as f64;
        let mut cum = 0u64;
        for (i, &c) in self.hist.iter().enumerate() {
            if c > 0 && (cum + c) as f64 >= target {
                let frac = ((target - cum as f64) / c as f64).clamp(0.0, 1.0);
                let v = self.lo as f64 + (i as f64 + frac) * bw;
                return v.clamp(self.min as f64, self.max as f64);
            }
            cum += c;
        }
        self.max as f64
    }

    /// Fraction (0..1) of samples in the histogram bin range `[a, b)`.
    pub fn fraction_between(&self, a: f32, b: f32) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let n = self.hist.len();
        let bin = |v: f32| (((v - self.lo) / (self.hi - self.lo)) * n as f32).clamp(0.0, n as f32) as usize;
        let (i0, i1) = (bin(a), bin(b));
        self.hist[i0..i1.max(i0)].iter().sum::<u64>() as f64 / self.count as f64
    }

    pub fn histogram(&self) -> &[u64] {
        &self.hist
    }
}

/// Streaming accumulator behind [`Summary`]: feed slices (e.g. one image
/// row at a time) from one worker, merge workers, then [`Accum::finish`].
#[derive(Clone, Debug)]
pub struct Accum {
    lo: f32,
    hi: f32,
    scale: f32,
    hist: Vec<u32>,
    sum: f64,
    count: u64,
    min: f32,
    max: f32,
}

impl Accum {
    pub fn new(lo: f32, hi: f32, bins: usize) -> Self {
        let bins = bins.max(1);
        Accum {
            lo,
            hi,
            scale: bins as f32 / (hi - lo).max(f32::MIN_POSITIVE),
            // One extra sink bin collects NaN.
            hist: vec![0; bins + 1],
            sum: 0.0,
            count: 0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
        }
    }

    /// Adds every non-NaN value of `v`. Values outside `lo..hi` count in the
    /// first or last bin; min, max and mean stay exact.
    pub fn push(&mut self, v: &[f32]) {
        const L: usize = 8;
        const PART: usize = 1024;
        let bins = self.hist.len() - 1;
        let last = (bins - 1) as f32;
        let mut idx = [0u32; PART];
        for part in v.chunks(PART) {
            // Vectorizable pass: lane sums, min / max, counts and bin indices
            // (NaN goes to the sink bin past the end).
            let (mut ls, mut ln) = ([0f32; L], [0u32; L]);
            let (mut mn, mut mx) = ([f32::INFINITY; L], [f32::NEG_INFINITY; L]);
            let full = part.len() / L * L;
            for (k, c) in part[..full].as_chunks::<L>().0.iter().enumerate() {
                let ix = &mut idx[k * L..k * L + L];
                for j in 0..L {
                    let x = c[j];
                    let ok = !x.is_nan();
                    ls[j] += if ok { x } else { 0.0 };
                    ln[j] += ok as u32;
                    mn[j] = mn[j].min(x);
                    mx[j] = mx[j].max(x);
                    ix[j] = if ok { ((x - self.lo) * self.scale).clamp(0.0, last) as u32 } else { bins as u32 };
                }
            }
            for (i, &x) in part.iter().enumerate().skip(full) {
                let ok = !x.is_nan();
                ls[0] += if ok { x } else { 0.0 };
                ln[0] += ok as u32;
                mn[0] = mn[0].min(x);
                mx[0] = mx[0].max(x);
                idx[i] = if ok { ((x - self.lo) * self.scale).clamp(0.0, last) as u32 } else { bins as u32 };
            }
            for &i in &idx[..part.len()] {
                self.hist[i as usize] += 1;
            }
            self.sum += ls.iter().map(|v| *v as f64).sum::<f64>();
            self.count += ln.iter().map(|v| *v as u64).sum::<u64>();
            self.min = mn.iter().fold(self.min, |a, b| a.min(*b));
            self.max = mx.iter().fold(self.max, |a, b| a.max(*b));
        }
    }

    pub fn merge(mut self, o: Accum) -> Accum {
        for (x, y) in self.hist.iter_mut().zip(&o.hist) {
            *x += y;
        }
        self.sum += o.sum;
        self.count += o.count;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self
    }

    pub fn finish(self) -> Summary {
        Summary {
            count: self.count,
            mean: if self.count > 0 { self.sum / self.count as f64 } else { f64::NAN },
            min: self.min,
            max: self.max,
            lo: self.lo,
            hi: self.hi,
            hist: self.hist[..self.hist.len() - 1].iter().map(|v| *v as u64).collect(),
        }
    }
}

/// Summarizes `data`, skipping NaN (see [`Accum::push`] for out-of-range values).
pub fn summarize(data: &[f32], lo: f32, hi: f32, bins: usize) -> Summary {
    data.par_chunks(CHUNK)
        .map(|c| {
            let mut a = Accum::new(lo, hi, bins);
            a.push(c);
            a
        })
        .reduce(|| Accum::new(lo, hi, bins), Accum::merge)
        .finish()
}

/// Exact mean of an `f32` buffer (f64 accumulation of row-sized partial sums).
pub fn mean(data: &[f32]) -> f64 {
    if data.is_empty() {
        return f64::NAN;
    }
    let s: f64 = data.par_chunks(CHUNK).map(|c| c.chunks(1024).map(|p| p.iter().sum::<f32>() as f64).sum::<f64>()).sum();
    s / data.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_uniform_ramp() {
        let v: Vec<f32> = (0..100_000).map(|i| i as f32 / 100_000.0 * 10.0).collect();
        let s = summarize(&v, 0.0, 10.0, 4096);
        assert_eq!(s.count, 100_000);
        assert!((s.mean - 5.0).abs() < 1e-3);
        assert!((s.percentile(50.0) - 5.0).abs() < 0.01);
        assert!((s.percentile(95.0) - 9.5).abs() < 0.01);
        assert!((s.percentile(100.0) - s.max as f64).abs() < 1e-6);
        assert!((s.fraction_between(0.0, 1.0) - 0.1).abs() < 0.001);
        assert!((mean(&v) - 5.0).abs() < 1e-3);
    }

    #[test]
    fn nan_is_skipped() {
        let s = summarize(&[1.0, f32::NAN, 3.0], 0.0, 4.0, 16);
        assert_eq!(s.count, 2);
        assert_eq!(s.mean, 2.0);
        assert_eq!((s.min, s.max), (1.0, 3.0));
    }
}
