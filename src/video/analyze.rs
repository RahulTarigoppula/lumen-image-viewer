//! Whole-file A/B analysis: decode both videos in lockstep and compute
//! per-frame PSNR/SSIM on native planes.

use super::engine::SeqDecoder;
use super::metrics::{self, FrameMetrics};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// `offset` shifts B by whole frames (B frame = A frame + offset).
pub fn run(a: &Path, b: &Path, offset: i64, cancel: &AtomicBool, mut on_row: impl FnMut(usize, FrameMetrics)) -> Result<()> {
    let mut da = SeqDecoder::open(a)?;
    let mut db = SeqDecoder::open(b)?;
    // Apply the offset by skipping frames on one side.
    if offset > 0 {
        for _ in 0..offset {
            db.next();
        }
    }
    let mut ia: usize = 0;
    if offset < 0 {
        for _ in 0..(-offset) {
            da.next();
            ia += 1;
        }
    }
    // Decode the two streams on separate threads for throughput.
    let (tx, rx) = crossbeam_channel::bounded(4);
    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || {
            while let Some(f) = db.next() {
                if tx.send(f).is_err() {
                    break;
                }
            }
        });
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("Analysis cancelled"));
            }
            let Some(fa) = da.next() else { break };
            let Ok(fb) = rx.recv() else { break };
            match metrics::compare(&fa, &fb) {
                Some(m) => on_row(ia, m),
                None => return Err(anyhow!("A and B have different resolutions or pixel formats; metrics need identical layouts")),
            }
            ia += 1;
        }
        drop(rx);
        Ok(())
    })
}
