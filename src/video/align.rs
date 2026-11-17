//! Auto-align of start points: decode the start of every video at low
//! resolution off the UI thread and estimate how far each one runs behind
//! or ahead of A (the math is in `lumen_core::video::timealign`).

use super::engine::SeqDecoder;
use super::timealign::{self, Alignment, FrameSig, Params, GRID};
use anyhow::{anyhow, Result};
use crossbeam_channel::{Receiver, TryRecvError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Seconds decoded from the start of each video.
pub const SCAN_SECS: f64 = 45.0;

/// Largest offset searched, seconds (either way).
pub const MAX_LAG: f64 = 10.0;

const CANCELLED: &str = "Cancelled";

/// What auto-align found for one video.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// A, the video the others are aligned to.
    Reference,
    Found(Alignment),
    /// Too little changes in the picture to compare.
    NoSignal,
    /// The video could not be decoded.
    Failed(String),
}

/// A running auto-align.
pub struct AlignJob {
    /// The videos, in pane order (a result for other videos is stale).
    pub paths: Vec<PathBuf>,
    progress: Arc<Vec<AtomicU32>>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<Vec<Outcome>, String>>,
}

impl AlignJob {
    /// Start on `(path, rotation)` of every video; A first.
    pub fn start(videos: Vec<(PathBuf, i32)>, ctx: egui::Context) -> AlignJob {
        let progress: Arc<Vec<AtomicU32>> = Arc::new(videos.iter().map(|_| AtomicU32::new(0)).collect());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let paths = videos.iter().map(|v| v.0.clone()).collect();
        {
            let (progress, cancel) = (progress.clone(), cancel.clone());
            std::thread::spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&videos, &progress, &cancel))).unwrap_or_else(|_| Err("Auto-align stopped unexpectedly".into()));
                let _ = tx.send(res);
                ctx.request_repaint();
            });
        }
        AlignJob { paths, progress, cancel, rx }
    }

    /// 0..1 over all videos.
    pub fn progress(&self) -> f32 {
        let n = self.progress.len().max(1) as f32;
        self.progress.iter().map(|p| f32::from_bits(p.load(Ordering::Relaxed))).sum::<f32>() / n
    }

    /// Ask the job to stop (it then reports nothing).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The result once done: one outcome per video, A first.
    pub fn poll(&self) -> Option<Result<Vec<Outcome>, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("Auto-align stopped unexpectedly".into())),
        }
    }
}

/// True for the error of a job that was cancelled.
pub fn is_cancelled(e: &str) -> bool {
    e == CANCELLED
}

fn run(videos: &[(PathBuf, i32)], progress: &[AtomicU32], cancel: &AtomicBool) -> Result<Vec<Outcome>, String> {
    // Every video on its own thread (FFmpeg contexts are opened where they are used).
    let sigs: Vec<Result<Vec<FrameSig>>> = std::thread::scope(|s| {
        let handles: Vec<_> = videos
            .iter()
            .zip(progress)
            .map(|((path, rot), p)| s.spawn(move || signature(path, *rot, cancel, &|f| p.store(f.to_bits(), Ordering::Relaxed))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("the decoder stopped unexpectedly")))).collect()
    });
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    let params = Params { max_lag: MAX_LAG, ..Params::default() };
    let a = match sigs.first() {
        Some(Ok(a)) => a,
        Some(Err(e)) => return Err(format!("A could not be read: {e}")),
        None => return Err("No videos".into()),
    };
    let fa = timealign::features(a, params.rate);
    Ok(sigs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i == 0 {
                return Outcome::Reference;
            }
            match s {
                Err(e) => Outcome::Failed(e.to_string()),
                Ok(b) => {
                    let found = fa.as_ref().and_then(|fa| timealign::features(b, params.rate).and_then(|fb| timealign::align(fa, &fb, &params)));
                    match found {
                        Some(al) => Outcome::Found(al),
                        None => Outcome::NoSignal,
                    }
                }
            }
        })
        .collect())
}

/// The first [`SCAN_SECS`] of a video as coarse luma grids, turned like
/// the picture is shown.
fn signature(path: &Path, rotation: i32, cancel: &AtomicBool, progress: &dyn Fn(f32)) -> Result<Vec<FrameSig>> {
    let mut dec = SeqDecoder::open(path)?;
    dec.set_fast(true);
    let mut out = Vec::new();
    while let Some((t, cells)) = dec.next_luma_grid(GRID, GRID) {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!(CANCELLED));
        }
        if t > SCAN_SECS {
            break;
        }
        let (cells, _, _) = timealign::rotate_cells(&cells, GRID, GRID, rotation);
        out.push(FrameSig { t, cells });
        if out.len() % 16 == 0 {
            progress((t / SCAN_SECS).clamp(0.0, 1.0) as f32);
        }
    }
    progress(1.0);
    if out.len() < 2 {
        return Err(anyhow!("no frames could be decoded"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::engine::test_media::{write_y4m, TestDir};
    use super::*;
    use crate::video::timealign::Confidence;

    /// Brightness of 4 x 4 cells that change at their own pace (scene frame
    /// `s`, any sign).
    fn level(s: i64, cell: usize) -> u8 {
        let period = 3 + (cell % 5) as i64;
        let q = (s + cell as i64 * 7).div_euclid(period);
        let mut h = (q as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (cell as u64 + 1).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        h ^= h >> 29;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 32;
        40 + (h % 170) as u8
    }

    /// 8 s at 25 fps of the scene, `delay` frames late.
    fn clip(path: &Path, delay: i64) {
        write_y4m(path, 64, 36, 200, |f, plane, x, y| if plane > 0 { 128 } else { level(f as i64 - delay, (y * 4 / 36) * 4 + x * 4 / 64) });
    }

    #[test]
    fn finds_the_offset_between_two_clips() {
        let dir = TestDir::new("align");
        let (a, b, c) = (dir.path("a.y4m"), dir.path("b.y4m"), dir.path("c.y4m"));
        clip(&a, 0);
        clip(&b, 7);
        clip(&c, -12);
        let no = AtomicBool::new(false);
        let sa = signature(&a, 0, &no, &|_| {}).unwrap();
        assert_eq!(sa.len(), 200);
        assert_eq!(sa[0].cells.len(), GRID * GRID);
        assert!((sa[25].t - 1.0).abs() < 1e-6);
        let progress: Vec<AtomicU32> = (0..3).map(|_| AtomicU32::new(0)).collect();
        let res = run(&[(a, 0), (b, 0), (c, 0)], &progress, &no).unwrap();
        assert!(matches!(res[0], Outcome::Reference));
        for (i, want) in [(1, 0.28), (2, -0.48)] {
            match &res[i] {
                Outcome::Found(al) => {
                    assert!((al.offset - want).abs() < 0.02, "video {i}: {al:?}");
                    assert!(al.level() >= Confidence::Medium, "video {i}: {al:?}");
                }
                other => panic!("video {i}: {other:?}"),
            }
        }
        assert!(progress.iter().all(|p| f32::from_bits(p.load(Ordering::Relaxed)) == 1.0));
    }

    #[test]
    fn cancel_and_unreadable_files() {
        let dir = TestDir::new("align-cancel");
        let a = dir.path("a.y4m");
        clip(&a, 0);
        let progress: Vec<AtomicU32> = (0..2).map(|_| AtomicU32::new(0)).collect();
        let err = run(&[(a.clone(), 0), (a.clone(), 0)], &progress, &AtomicBool::new(true)).unwrap_err();
        assert!(is_cancelled(&err));
        let res = run(&[(a, 0), (dir.path("missing.mp4"), 0)], &progress, &AtomicBool::new(false)).unwrap();
        assert!(matches!(res[1], Outcome::Failed(_)), "{res:?}");
    }
}
