//! A/B analysis: decode both videos in lockstep and compute per-frame
//! PSNR/SSIM on native planes, over the whole file or a frame range, on the
//! whole picture or a region of it.

use super::engine::{FrameRange, IndexEntry, Roi, SeqDecoder};
use super::metrics::{self, FrameMetrics, YuvFrame};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const LAYOUT_ERROR: &str = "A and B have different resolutions or pixel formats; metrics need identical layouts";

/// What to measure. The default is every frame of the whole picture.
#[derive(Clone, Default)]
pub struct Spec {
    /// Shifts B by whole frames (B frame = A frame + offset).
    pub offset: i64,
    /// Frames of A to measure (A frames without a B partner are left out).
    pub range: FrameRange,
    /// Measure only this area (luma pixels, the same in A and B).
    pub roi: Option<Roi>,
    /// Frame indexes built elsewhere (the player's), so seeking within the
    /// range needs no extra pass over the files.
    pub index_a: Option<Arc<Vec<IndexEntry>>>,
    pub index_b: Option<Arc<Vec<IndexEntry>>>,
}

/// Every frame of the whole picture; `offset` shifts B by whole frames
/// (B frame = A frame + offset).
pub fn run(a: &Path, b: &Path, offset: i64, cancel: &AtomicBool, on_row: impl FnMut(usize, FrameMetrics)) -> Result<()> {
    run_spec(a, b, &Spec { offset, ..Default::default() }, cancel, on_row)
}

/// Measure `spec`; `on_row` gets A's frame index and the metrics, in order.
pub fn run_spec(a: &Path, b: &Path, spec: &Spec, cancel: &AtomicBool, mut on_row: impl FnMut(usize, FrameMetrics)) -> Result<()> {
    let offset = spec.offset;
    let mut da = SeqDecoder::open(a)?;
    if let Some(ix) = &spec.index_a {
        da.set_index(ix.clone());
    }
    // B is opened on its own thread below (FFmpeg contexts are not Send);
    // check up front that it opens so errors are reported immediately.
    let size_b = SeqDecoder::open(b)?.size();
    // A region would hide a size mismatch from the per-frame layout check.
    if spec.roi.is_some() && da.size() != size_b {
        return Err(anyhow!(LAYOUT_ERROR));
    }
    da.set_roi(spec.roi);
    let range = a_range(spec.range, offset);
    // B reads the partners of A's frames: the same grid shifted by the
    // offset, or A's keyframes (B's own keyframes are elsewhere).
    let b_frames = if range.keyframes_only {
        let ix = da.index()?;
        let end = range.end.unwrap_or(usize::MAX).min(ix.len());
        let keys = (range.start.min(end)..end).filter(|&i| ix[i].key).filter_map(|i| usize::try_from(i as i64 + offset).ok());
        Some(keys.collect::<Vec<_>>())
    } else {
        None
    };
    let b_range = FrameRange {
        start: (range.start as i64 + offset).max(0) as usize,
        end: range.end.map(|e| (e as i64 + offset).max(0) as usize),
        stride: range.stride,
        keyframes_only: false,
    };
    let (b, roi, index_b) = (b.to_path_buf(), spec.roi, spec.index_b.clone());
    // Decode the two streams on separate threads for throughput.
    let (tx, rx) = crossbeam_channel::bounded::<Result<(usize, YuvFrame), String>>(4);
    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || {
            let Ok(mut db) = SeqDecoder::open(&b) else { return };
            if let Some(ix) = index_b {
                db.set_index(ix);
            }
            db.set_roi(roi);
            let mut frames = match b_frames {
                Some(list) => db.into_frames(list),
                None => db.into_range(b_range),
            };
            for f in frames.by_ref() {
                if tx.send(Ok(f)).is_err() {
                    return;
                }
            }
            if let Some(e) = frames.error() {
                let _ = tx.send(Err(format!("B: {e:#}")));
            }
        });
        let mut fa_iter = da.into_range(range);
        let mut pending_b: Option<(usize, YuvFrame)> = None;
        'frames: loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("Analysis cancelled"));
            }
            let Some((ia, fa)) = fa_iter.next() else { break };
            let want = (ia as i64 + offset) as usize;
            // B frames before the partner have no A frame (missing in A).
            while pending_b.as_ref().is_none_or(|(ib, _)| *ib < want) {
                match rx.recv() {
                    Ok(Ok(f)) => pending_b = Some(f),
                    Ok(Err(e)) => return Err(anyhow!(e)),
                    Err(_) => break 'frames,
                }
            }
            let Some((_, fb)) = pending_b.take_if(|(ib, _)| *ib == want) else {
                // B has no frame `want`: skip this A frame.
                continue;
            };
            match metrics::compare(&fa, &fb) {
                Some(m) => on_row(ia, m),
                None => return Err(anyhow!(LAYOUT_ERROR)),
            }
        }
        drop(rx);
        match fa_iter.error() {
            Some(e) => Err(anyhow!("A: {e:#}")),
            None => Ok(()),
        }
    })
}

/// `r` without the A frames a negative offset leaves without a B partner,
/// keeping the stride grid.
fn a_range(r: FrameRange, offset: i64) -> FrameRange {
    let min = (-offset).max(0) as usize;
    let stride = r.stride.max(1);
    let start = if r.start >= min {
        r.start
    } else if r.keyframes_only {
        min
    } else {
        r.start + (min - r.start).div_ceil(stride) * stride
    };
    FrameRange { start, stride, ..r }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::engine::test_media::{write_y4m, TestDir};

    fn rows(a: &Path, b: &Path, spec: &Spec) -> Vec<(usize, FrameMetrics)> {
        let mut out = Vec::new();
        run_spec(a, b, spec, &AtomicBool::new(false), |i, m| out.push((i, m))).unwrap();
        out
    }

    /// A and B are the same clip except for noise in B's top-left 16x16 block.
    fn pair(dir: &TestDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let a = dir.path("a.y4m");
        let b = dir.path("b.y4m");
        let base = |f: usize, plane: usize, x: usize, y: usize| ((f * 13 + x * 2 + y + plane * 50) % 200) as u8;
        write_y4m(&a, 64, 48, 20, base);
        write_y4m(&b, 64, 48, 20, move |f, plane, x, y| base(f, plane, x, y) + if plane == 0 && x < 16 && y < 16 { ((x * 7 + y * 3) % 5) as u8 } else { 0 });
        (a, b)
    }

    #[test]
    fn whole_file_is_the_default() {
        let dir = TestDir::new("analyze-whole");
        let (a, b) = pair(&dir);
        let mut old = Vec::new();
        run(&a, &b, 0, &AtomicBool::new(false), |i, m| old.push((i, m))).unwrap();
        assert_eq!(old.iter().map(|r| r.0).collect::<Vec<_>>(), (0..20).collect::<Vec<_>>());
        assert!(old.iter().all(|(_, m)| m.psnr_y.is_finite() && m.psnr_u.is_infinite()));
        // Identical files: infinite PSNR, SSIM 1.
        let same = rows(&a, &a, &Spec::default());
        assert_eq!(same.len(), 20);
        assert!(same.iter().all(|(_, m)| m.psnr_avg.is_infinite() && (m.ssim_y - 1.0).abs() < 1e-9));
    }

    #[test]
    fn range_stride_and_offset() {
        let dir = TestDir::new("analyze-range");
        let (a, b) = pair(&dir);
        let spec = Spec { range: FrameRange::new(3, Some(15), 4), ..Default::default() };
        assert_eq!(rows(&a, &b, &spec).iter().map(|r| r.0).collect::<Vec<_>>(), vec![3, 7, 11]);
        // B = A + 2: A frames 18 and 19 have no partner.
        let spec = Spec { offset: 2, range: FrameRange::new(10, None, 3), ..Default::default() };
        assert_eq!(rows(&a, &b, &spec).iter().map(|r| r.0).collect::<Vec<_>>(), vec![10, 13, 16]);
        // B = A - 5: A frames before 5 have no partner; the grid is kept.
        let spec = Spec { offset: -5, range: FrameRange::new(1, Some(12), 3), ..Default::default() };
        assert_eq!(rows(&a, &b, &spec).iter().map(|r| r.0).collect::<Vec<_>>(), vec![7, 10]);
        // Every Y4M frame is a keyframe.
        let spec = Spec { range: FrameRange { keyframes_only: true, ..FrameRange::new(17, None, 1) }, ..Default::default() };
        assert_eq!(rows(&a, &b, &spec).iter().map(|r| r.0).collect::<Vec<_>>(), vec![17, 18, 19]);
    }

    #[test]
    fn roi_limits_the_measured_area() {
        let dir = TestDir::new("analyze-roi");
        let (a, b) = pair(&dir);
        // Outside the noisy block: identical.
        let clean = rows(&a, &b, &Spec { roi: Some(Roi::new(20, 20, 30, 20)), ..Default::default() });
        assert_eq!(clean.len(), 20);
        assert!(clean.iter().all(|(_, m)| m.psnr_y.is_infinite()));
        // Only the noisy block: lower PSNR than over the whole frame.
        let whole = rows(&a, &b, &Spec::default());
        let noisy = rows(&a, &b, &Spec { roi: Some(Roi::new(0, 0, 16, 16)), ..Default::default() });
        assert!(noisy[0].1.psnr_y < whole[0].1.psnr_y - 5.0, "{} vs {}", noisy[0].1.psnr_y, whole[0].1.psnr_y);
        // A region must not hide a size mismatch.
        let c = dir.path("c.y4m");
        write_y4m(&c, 80, 48, 2, |_, _, _, _| 0);
        assert!(run_spec(&a, &c, &Spec { roi: Some(Roi::new(0, 0, 8, 8)), ..Default::default() }, &AtomicBool::new(false), |_, _| {}).is_err());
    }
}
