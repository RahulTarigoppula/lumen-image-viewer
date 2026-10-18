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
    // B is opened on its own thread below (FFmpeg contexts are not Send);
    // check up front that it opens so errors are reported immediately.
    drop(SeqDecoder::open(b)?);
    let b = b.to_path_buf();
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
            let Ok(mut db) = SeqDecoder::open(&b) else { return };
            // Apply a positive offset by skipping frames of B.
            for _ in 0..offset.max(0) {
                db.next();
            }
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
