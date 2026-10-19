//! Timings at 1080p and 4K (ignored by default). Run with
//! `cargo test -p lumen-analysis --release --test perf -- --ignored --nocapture`.
//! Prints, per metric, the best of 7 runs with derived inputs already
//! computed, next to the plan's 4K budget (`MetricInfo::cost_ms_4k`). Uses a
//! normal-priority pool so other load on the machine does not starve it.

mod common;

use common::*;
use lumen_analysis::frame::{FrameCtx, InputKind, OwnedFrame};
use lumen_analysis::job::CancelToken;
use lumen_analysis::metrics::{self, MetricKind, MetricOptions};
use std::time::Instant;

fn best_ms(mut f: impl FnMut()) -> f64 {
    f();
    (0..7)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_secs_f64() * 1000.0
        })
        .fold(f64::INFINITY, f64::min)
}

fn frames(w: usize, h: usize) -> [(OwnedFrame, OwnedFrame, &'static str); 2] {
    // Cheap synthetic content (the per-pixel test generators are too slow at 4K).
    let mut rng = Rng::new(5);
    let a: Vec<u8> = (0..w * h).map(|i| (((i % w) * 255 / w) as f64 + 40.0 * ((i % w) as f64 / 7.0).sin() + 30.0 * ((i / w) as f64 / 5.0).cos()).clamp(0.0, 255.0) as u8).collect();
    let b: Vec<u8> = a.iter().map(|v| (*v as f64 + 4.0 * (rng.uniform() - 0.5) * 2.0).clamp(0.0, 255.0) as u8).collect();
    let yuv = (yuv420(w, h, a.clone(), |x, y| ((x % 200) as u8, (y % 200) as u8)), yuv420(w, h, b.clone(), |x, y| ((x % 200) as u8, (y % 201) as u8)), "YUV 4:2:0 8-bit");
    let mut ra = Vec::with_capacity(w * h * 4);
    let mut rb = Vec::with_capacity(w * h * 4);
    for i in 0..w * h {
        ra.extend_from_slice(&[a[i], a[(i + 7) % (w * h)], 255 - a[i], 255]);
        rb.extend_from_slice(&[b[i], a[(i + 7) % (w * h)], 255 - b[i], 255]);
    }
    let rgb = (OwnedFrame::rgba8(w, h, ra), OwnedFrame::rgba8(w, h, rb), "RGBA8");
    [yuv, rgb]
}

#[test]
#[ignore]
fn perf_1080p_and_4k() {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1)).build().unwrap();
    println!("pool: {} threads", pool.current_num_threads());
    let opts = MetricOptions::default();
    let cancel = CancelToken::new();
    for (w, h) in [(1920usize, 1080usize), (3840, 2160)] {
        for (fa, fb, kind) in frames(w, h) {
            println!("\n{w}x{h} {kind}");
            let (va, vb) = (fa.view(), fb.view());
            pool.install(|| {
                for k in [InputKind::Luma, InputKind::Lab] {
                    let t = best_ms(|| {
                        let c = FrameCtx::new(va);
                        c.prepare(&[k]);
                    });
                    println!("  derived {:<22} {t:8.2} ms", format!("{k:?}"));
                }
                let ca = FrameCtx::new(va);
                let cb = FrameCtx::new(vb);
                ca.prepare(&[InputKind::Luma, InputKind::Lab]);
                cb.prepare(&[InputKind::Luma, InputKind::Lab]);
                for m in metrics::all() {
                    let info = m.info();
                    let b = (info.kind == MetricKind::FullReference).then_some(&cb);
                    if m.compute(&ca, b, &opts, &cancel).is_err() {
                        println!("  {:<22} n/a", info.id);
                        continue;
                    }
                    let with_map = best_ms(|| {
                        m.compute(&ca, b, &opts, &cancel).unwrap();
                    });
                    let no_map = best_ms(|| {
                        m.compute(&ca, b, &MetricOptions { want_map: false, ..opts.clone() }, &cancel).unwrap();
                    });
                    println!("  {:<22} {no_map:8.2} ms  (with map {with_map:8.2} ms; plan 4K budget {:6.1} ms)", info.id, info.cost_ms(3840, 2160));
                }
            });
        }
    }
}

/// Single-threaded cost per pixel of each metric at 1080p (less sensitive to
/// other load on the machine than the parallel timings).
#[test]
#[ignore]
fn perf_single_thread_ns_per_px() {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let (w, h) = (1920usize, 1080usize);
    let opts = MetricOptions { want_map: false, ..MetricOptions::default() };
    let cancel = CancelToken::new();
    for (fa, fb, kind) in frames(w, h) {
        println!("
{kind}, 1 thread, ns per pixel (x 8.3M / 8 cores = 4K ms on 8 cores)");
        let (va, vb) = (fa.view(), fb.view());
        pool.install(|| {
            let ns = |t: f64| t * 1e6 / (w * h) as f64;
            for k in [InputKind::Luma, InputKind::Lab] {
                let t = best_ms(|| FrameCtx::new(va).prepare(&[k]));
                println!("  derived {:<20} {:7.2} ns/px  ({:6.1} ms 4K/8 cores)", format!("{k:?}"), ns(t), ns(t) * 8.3 / 8.0);
            }
            let ca = FrameCtx::new(va);
            let cb = FrameCtx::new(vb);
            ca.prepare(&[InputKind::Luma, InputKind::Lab]);
            cb.prepare(&[InputKind::Luma, InputKind::Lab]);
            for m in metrics::all() {
                let info = m.info();
                let b = (info.kind == MetricKind::FullReference).then_some(&cb);
                if m.compute(&ca, b, &opts, &cancel).is_err() {
                    continue;
                }
                let t = best_ms(|| {
                    m.compute(&ca, b, &opts, &cancel).unwrap();
                });
                println!("  {:<28} {:7.2} ns/px  ({:6.1} ms 4K/8 cores; budget {:5.1})", info.id, ns(t), ns(t) * 8.3 / 8.0, info.cost_ms_4k);
            }
        });
    }
}
