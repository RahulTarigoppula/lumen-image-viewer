//! Objective quality metrics on native YUV planes (PSNR, SSIM) and simple
//! scene-change scoring. Pure Rust; independent of FFmpeg. The PSNR and SSIM
//! kernels delegate to `lumen-analysis` (typed, integer fast paths), and
//! [`YuvFrame`] is a `lumen_analysis::FrameSource`, so decoded frames can be
//! fed to analysis jobs without copying.

/// One plane of samples. 8-bit content uses `u8`, high bit depth `u16`.
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
    /// Borrowed analysis view of the plane.
    pub fn view(&self) -> lumen_analysis::PlaneView<'_> {
        use lumen_analysis::frame::Plane as P;
        match &self.data {
            PlaneData::U8(v) => lumen_analysis::PlaneView::U8(P { data: v, width: self.width, height: self.height, stride: self.width }),
            PlaneData::U16(v) => lumen_analysis::PlaneView::U16(P { data: v, width: self.width, height: self.height, stride: self.width }),
        }
    }
    pub fn get(&self, i: usize) -> u32 {
        match &self.data {
            PlaneData::U8(v) => v[i] as u32,
            PlaneData::U16(v) => v[i] as u32,
        }
    }
}

/// Planar YUV (or gray) frame at native resolution.
#[derive(Clone)]
pub struct YuvFrame {
    pub planes: Vec<Plane>,
    pub bit_depth: u32,
}

impl YuvFrame {
    pub fn max_value(&self) -> f64 {
        ((1u64 << self.bit_depth) - 1) as f64
    }
    /// Analysis view (BT.709 limited range assumed; the decoder does not
    /// record the matrix). `None` for layouts other than gray or 3 planes.
    pub fn analysis_view(&self) -> Option<lumen_analysis::FrameView<'_>> {
        let y = self.planes.first()?.view();
        let uv = match self.planes.len() {
            1 => None,
            3 => Some((self.planes[1].view(), self.planes[2].view())),
            _ => return None,
        };
        lumen_analysis::YuvView::new(y, uv, self.bit_depth).ok().map(lumen_analysis::FrameView::Yuv)
    }
    pub fn same_layout(&self, other: &YuvFrame) -> bool {
        self.bit_depth == other.bit_depth
            && self.planes.len() == other.planes.len()
            && self.planes.iter().zip(&other.planes).all(|(a, b)| a.width == b.width && a.height == b.height)
    }
}

impl lumen_analysis::FrameSource for YuvFrame {
    fn view(&self) -> lumen_analysis::FrameView<'_> {
        self.analysis_view().expect("YuvFrame has 1 or 3 planes with a valid chroma layout")
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct FrameMetrics {
    pub psnr_y: f64,
    pub psnr_u: f64,
    pub psnr_v: f64,
    pub psnr_avg: f64,
    pub ssim_y: f64,
    pub mse_avg: f64,
}

pub fn mse_plane(a: &Plane, b: &Plane) -> f64 {
    assert_eq!(a.len(), b.len());
    if let Some(e) = lumen_analysis::metrics::reference::psnr::plane_error(&a.view(), &b.view()) {
        return e.mse();
    }
    // Mixed sample types, or equal length with different shapes (not produced by the decoder).
    let mut acc = 0u64;
    for i in 0..a.len() {
        let d = a.get(i) as i64 - b.get(i) as i64;
        acc += (d * d) as u64;
    }
    acc as f64 / a.len().max(1) as f64
}

/// Ceiling for reported PSNR (and SSIM in dB). Identical planes have an
/// infinite PSNR, which spreadsheets cannot read, so summaries, the UI and
/// CSV exports show this value instead.
pub const PSNR_CAP_DB: f64 = 100.0;

/// `v` limited to [`PSNR_CAP_DB`] (NaN, meaning "no such plane", is kept).
pub fn cap_db(v: f64) -> f64 {
    if v.is_nan() { v } else { v.min(PSNR_CAP_DB) }
}

pub fn psnr_from_mse(mse: f64, max: f64) -> f64 {
    if mse <= 1e-12 {
        f64::INFINITY
    } else {
        10.0 * (max * max / mse).log10()
    }
}

/// FFmpeg/x264-style SSIM: 8x8 windows at stride 4 built from 4x4 block sums.
pub fn ssim_plane(a: &Plane, b: &Plane, bit_depth: u32) -> f64 {
    let max = ((1u64 << bit_depth) - 1) as f64;
    lumen_analysis::metrics::reference::ssim::ssim_fast_plane(&a.view(), &b.view(), max, &lumen_analysis::CancelToken::new()).unwrap_or(0.0)
}

pub fn ssim_db(ssim: f64) -> f64 {
    if ssim >= 1.0 {
        f64::INFINITY
    } else {
        10.0 * (1.0 / (1.0 - ssim)).log10()
    }
}

/// Full metric set for a pair of frames with identical layout.
pub fn compare(a: &YuvFrame, b: &YuvFrame) -> Option<FrameMetrics> {
    if !a.same_layout(b) || a.planes.is_empty() {
        return None;
    }
    let max = a.max_value();
    let mses: Vec<f64> = a.planes.iter().zip(&b.planes).map(|(p, q)| mse_plane(p, q)).collect();
    let total: usize = a.planes.iter().map(|p| p.len()).sum();
    let mse_avg: f64 = a.planes.iter().zip(&mses).map(|(p, m)| m * p.len() as f64 / total as f64).sum();
    let ssim_y = ssim_plane(&a.planes[0], &b.planes[0], a.bit_depth);
    Some(FrameMetrics {
        psnr_y: psnr_from_mse(mses[0], max),
        psnr_u: mses.get(1).map(|m| psnr_from_mse(*m, max)).unwrap_or(f64::NAN),
        psnr_v: mses.get(2).map(|m| psnr_from_mse(*m, max)).unwrap_or(f64::NAN),
        psnr_avg: psnr_from_mse(mse_avg, max),
        ssim_y,
        mse_avg,
    })
}

/// Summary over a whole sequence (mean of per-frame PSNR and PSNR of mean MSE).
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub frames: usize,
    pub mean_psnr_y: f64,
    pub mean_psnr_avg: f64,
    pub global_psnr: f64,
    pub mean_ssim: f64,
    pub min_psnr_y: f64,
    pub min_ssim: f64,
}

pub fn summarize(rows: &[FrameMetrics], max: f64) -> Summary {
    if rows.is_empty() {
        return Summary::default();
    }
    let n = rows.len() as f64;
    let cap = cap_db;
    Summary {
        frames: rows.len(),
        mean_psnr_y: rows.iter().map(|r| cap(r.psnr_y)).sum::<f64>() / n,
        mean_psnr_avg: rows.iter().map(|r| cap(r.psnr_avg)).sum::<f64>() / n,
        global_psnr: cap(psnr_from_mse(rows.iter().map(|r| r.mse_avg).sum::<f64>() / n, max)),
        mean_ssim: rows.iter().map(|r| r.ssim_y).sum::<f64>() / n,
        min_psnr_y: rows.iter().map(|r| cap(r.psnr_y)).fold(f64::INFINITY, f64::min),
        min_ssim: rows.iter().map(|r| r.ssim_y).fold(f64::INFINITY, f64::min),
    }
}

/// Per-frame metrics as CSV (one row per frame, header comment first).
/// PSNR and SSIM dB values are capped at [`PSNR_CAP_DB`] so identical frames
/// read as 100.0000 instead of `inf`; a missing plane leaves its cell empty.
pub fn to_csv(rows: &[(usize, FrameMetrics)]) -> String {
    let db = |v: f64| if v.is_nan() { String::new() } else { format!("{:.4}", cap_db(v)) };
    let mut s = format!("# Lumen per-frame metrics (A = reference). PSNR and SSIM dB are capped at {PSNR_CAP_DB:.2} dB; identical frames read {PSNR_CAP_DB:.2}.\n");
    s += "frame,psnr_y,psnr_u,psnr_v,psnr_avg,ssim_y,ssim_db\n";
    for (i, m) in rows {
        s += &format!("{i},{},{},{},{},{:.6},{}\n", db(m.psnr_y), db(m.psnr_u), db(m.psnr_v), db(m.psnr_avg), m.ssim_y, db(ssim_db(m.ssim_y)));
    }
    s
}

/// FFmpeg `scdet`-style scene score from two consecutive luma planes (0..100).
pub fn scene_score(prev: &Plane, cur: &Plane, prev_mafd: f64, bit_depth: u32) -> (f64, f64) {
    if prev.len() != cur.len() || cur.len() == 0 {
        return (0.0, prev_mafd);
    }
    let max = ((1u64 << bit_depth) - 1) as f64;
    let step = (cur.len() / 200_000).max(1);
    let mut sad = 0f64;
    let mut n = 0usize;
    let mut i = 0;
    while i < cur.len() {
        sad += (cur.get(i) as f64 - prev.get(i) as f64).abs();
        n += 1;
        i += step;
    }
    let mafd = sad / n.max(1) as f64 / max * 100.0;
    let diff = (mafd - prev_mafd).abs();
    (mafd.min(diff).clamp(0.0, 100.0), mafd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Plane {
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                v.push(f(x, y));
            }
        }
        Plane { width: w, height: h, data: PlaneData::U8(v) }
    }

    #[test]
    fn identical_frames() {
        let p = plane(64, 48, |x, y| ((x * 3 + y * 5) % 256) as u8);
        let f = YuvFrame { planes: vec![p.clone(), plane(32, 24, |_, _| 128), plane(32, 24, |_, _| 128)], bit_depth: 8 };
        let m = compare(&f, &f).unwrap();
        assert!(m.psnr_y.is_infinite());
        assert!((m.ssim_y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn identical_frames_cap_at_100_db() {
        assert_eq!(cap_db(f64::INFINITY), PSNR_CAP_DB);
        assert_eq!(cap_db(42.5), 42.5);
        assert!(cap_db(f64::NAN).is_nan());
        let m = FrameMetrics { psnr_y: f64::INFINITY, psnr_u: f64::INFINITY, psnr_v: f64::INFINITY, psnr_avg: f64::INFINITY, ssim_y: 1.0, mse_avg: 0.0 };
        let s = summarize(&[m, m], 255.0);
        assert_eq!(s.mean_psnr_y, PSNR_CAP_DB);
        assert_eq!(s.global_psnr, PSNR_CAP_DB);
        assert_eq!(s.min_psnr_y, PSNR_CAP_DB);
    }

    #[test]
    fn csv_has_no_infinities() {
        let same = FrameMetrics { psnr_y: f64::INFINITY, psnr_u: f64::INFINITY, psnr_v: f64::NAN, psnr_avg: f64::INFINITY, ssim_y: 1.0, mse_avg: 0.0 };
        let csv = to_csv(&[(7, same)]);
        assert!(!csv.contains("inf") && !csv.contains("NaN"), "{csv}");
        assert!(csv.starts_with('#'));
        assert_eq!(csv.lines().nth(2), Some("7,100.0000,100.0000,,100.0000,1.000000,100.0000"));
    }

    #[test]
    fn known_psnr() {
        // Constant difference of 5 → MSE 25 → PSNR = 10*log10(255²/25) ≈ 34.15 dB.
        let a = plane(32, 32, |_, _| 100);
        let b = plane(32, 32, |_, _| 105);
        let mse = mse_plane(&a, &b);
        assert!((mse - 25.0).abs() < 1e-9);
        assert!((psnr_from_mse(mse, 255.0) - 34.1514).abs() < 1e-3);
    }

    #[test]
    fn ssim_drops_with_noise() {
        let a = plane(64, 64, |x, y| ((x * 7 + y * 13) % 200) as u8);
        let b = plane(64, 64, |x, y| (((x * 7 + y * 13) % 200) as i32 + if (x + y) % 2 == 0 { 20 } else { -20 }).clamp(0, 255) as u8);
        let s = ssim_plane(&a, &b, 8);
        assert!(s < 0.95 && s > 0.0, "ssim {s}");
    }

    #[test]
    fn scene_change_detects_cut() {
        let a = plane(64, 64, |_, _| 20);
        let b = plane(64, 64, |_, _| 230);
        let (score, _) = scene_score(&a, &b, 0.0, 8);
        assert!(score > 50.0);
        let (score2, _) = scene_score(&a, &a, 0.0, 8);
        assert!(score2 < 1.0);
    }
}
