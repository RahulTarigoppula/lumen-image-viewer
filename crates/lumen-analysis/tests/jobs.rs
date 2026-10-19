//! Job runner, report and export tests.

mod common;

use common::*;
use lumen_analysis::frame::{FrameSource, OwnedFrame};
use lumen_analysis::heatmap::{ColorizeOptions, RangeMode};
use lumen_analysis::job::{run_video, AnalysisPool, Analyzer, ImageSpec, JobControl, JobEvent, SharedFrame, VideoSpec};
use lumen_analysis::report::{AnalysisReport, SCHEMA};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Longest wait for any job event. Generous on purpose: a whole-workspace
/// test run on a loaded machine is slow, but nothing may wait forever.
const LIMIT: Duration = Duration::from_secs(300);

fn frame_pair(w: usize, h: usize, seed: u64, noise: f64) -> (OwnedFrame, OwnedFrame) {
    let t = texture(w, h, seed);
    (
        yuv420(w, h, quantize(&t), |x, y| ((100 + x % 50) as u8, (120 + y % 30) as u8)),
        yuv420(w, h, quantize(&add_noise(&t, noise, seed + 1)), |x, y| ((100 + x % 50) as u8, (121 + y % 30) as u8)),
    )
}

#[test]
fn image_job_streams_results_and_report() {
    let notified = Arc::new(AtomicUsize::new(0));
    let n2 = notified.clone();
    let an = Analyzer::new(Some(3), Some(Arc::new(move || {
        n2.fetch_add(1, Ordering::Relaxed);
    })));
    let (w, h) = (300, 200);
    let t = texture(w, h, 3);
    let a: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&t)));
    let b: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&add_noise(&t, 3.0, 4))));
    let ids: Vec<String> = lumen_analysis::metrics::all().iter().map(|m| m.id().to_string()).chain(["temporal.scene".to_string()]).collect();
    let spec = ImageSpec { metrics: ids.clone(), ..Default::default() };
    let mut h1 = an.submit_image(spec, a, Some(b));
    let mut metric_events = 0;
    let report = loop {
        match h1.recv_timeout(LIMIT).expect("event") {
            JobEvent::Metric(r) => {
                assert!(!r.preview);
                metric_events += 1;
            }
            JobEvent::Done(r) => break *r,
            JobEvent::Partial(_) | JobEvent::Row(_) => {}
            e => panic!("unexpected {e:?}"),
        }
    };
    assert!(h1.is_finished());
    assert_eq!(metric_events, ids.len() - 1);
    assert_eq!(h1.progress().done(), (ids.len() - 1) as u64);
    assert_eq!(h1.progress().fraction(), Some(1.0));
    assert!(notified.load(Ordering::Relaxed) >= metric_events);
    assert_eq!(report.schema, SCHEMA);
    assert_eq!((report.scope.width, report.scope.height), (300, 200));
    assert!(report.notes.iter().any(|n| n.contains("temporal.scene")));
    // Metric entries follow registry order and maps are attached.
    assert_eq!(report.metrics[0].id, "psnr");
    assert!(report.map("ssim.gauss").is_some() && report.map("deltae.2000").is_some());
    let psnr = report.value("psnr.avg").unwrap();
    assert!(psnr > 30.0 && psnr < 50.0);
    // JSON round trip (maps' pixels are not serialized).
    let json = report.to_json();
    assert!(!json.contains("inf") && !json.contains("NaN"));
    let back = AnalysisReport::from_json(&json).unwrap();
    assert_eq!(back.value("psnr.avg"), Some(psnr));
    assert!(back.map("ssim.gauss").unwrap().data.is_empty());
    let csv = report.to_csv();
    assert!(csv.starts_with("# Lumen analysis report (lumen.analysis/1"));
    assert!(csv.contains("\npsnr.r,") && csv.contains("key,value,unit,direction"), "{csv}");
}

#[test]
fn identical_image_csv_reads_cap_not_inf() {
    let an = Analyzer::new(Some(2), None);
    let (w, h) = (64, 64);
    let img: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&texture(w, h, 1))));
    let spec = ImageSpec { metrics: vec!["psnr".into(), "ssim.fast".into()], ..Default::default() };
    let r = an.submit_image(spec, img.clone(), Some(img)).wait_timeout(LIMIT).unwrap();
    let csv = r.to_csv();
    assert!(csv.contains("psnr.r,100.0000,dB,higher"), "{csv}");
    assert!(csv.contains("ssim.fast.y_db,100.0000,dB,higher"), "{csv}");
    assert!(!csv.to_lowercase().contains("inf") && !csv.contains("NaN"));
}

#[test]
fn image_job_errors_and_skips() {
    let an = Analyzer::new(Some(2), None);
    let small: SharedFrame = Arc::new(gray_rgba(32, 32, &[9; 1024]));
    // Unknown metric fails the job.
    let r = an.submit_image(ImageSpec { metrics: vec!["nope".into()], ..Default::default() }, small.clone(), None).wait_timeout(LIMIT);
    assert!(r.unwrap_err().contains("unknown metric"));
    // FR metric without B and a too-small MS-SSIM are skipped with notes.
    let other: SharedFrame = Arc::new(gray_rgba(32, 32, &[10; 1024]));
    let r = an.submit_image(ImageSpec { metrics: vec!["psnr".into(), "noise".into()], ..Default::default() }, small.clone(), None).wait_timeout(LIMIT).unwrap();
    assert!(r.notes.iter().any(|n| n.starts_with("psnr: skipped")), "{:?}", r.notes);
    assert!(r.value("noise.y").is_some());
    let r = an.submit_image(ImageSpec { metrics: vec!["ms_ssim".into()], ..Default::default() }, small, Some(other)).wait_timeout(LIMIT).unwrap();
    assert!(r.notes[0].contains("176x176"), "{:?}", r.notes);
}

#[test]
fn image_batches_run_every_pair_until_superseded() {
    let an = Analyzer::new(Some(2), None);
    let (w, h) = (160, 120);
    let a: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&texture(w, h, 3))));
    let b: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&add_noise(&texture(w, h, 3), 2.0, 1))));
    let c: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&add_noise(&texture(w, h, 3), 8.0, 2))));
    let spec = ImageSpec { metrics: vec!["psnr".into()], ..Default::default() };
    let hs = an.submit_images(vec![(spec.clone(), a.clone(), Some(b)), (spec.clone(), a.clone(), Some(c))]);
    let psnr: Vec<f64> = hs.into_iter().map(|h| h.wait_timeout(LIMIT).unwrap().value("psnr.avg").unwrap()).collect();
    // Both pairs are measured, each against A: C is the noisier one.
    assert!(psnr[0] > psnr[1], "{psnr:?}");
    // A new image job cancels a batch still running.
    let big: SharedFrame = Arc::new(gray_rgba(1600, 1200, &quantize(&texture(1600, 1200, 4))));
    let slow = ImageSpec { metrics: vec!["ssim.gauss".into(), "deltae.2000".into()], ..Default::default() };
    let hs = an.submit_images(vec![(slow.clone(), big.clone(), Some(big.clone())), (slow, big.clone(), Some(big))]);
    let last = an.submit_image(spec, a.clone(), Some(a));
    assert!(last.wait_timeout(LIMIT).is_ok());
    assert!(hs.into_iter().last().unwrap().wait_timeout(LIMIT).is_err());
}

#[test]
fn new_submission_supersedes_previous() {
    let an = Analyzer::new(Some(2), None);
    let (w, h) = (1600, 1200);
    let t = quantize(&texture(w, h, 5));
    let a: SharedFrame = Arc::new(gray_rgba(w, h, &t));
    let spec = ImageSpec { metrics: vec!["deltae.2000".into(), "ssim.gauss".into(), "ms_ssim".into(), "banding".into()], ..Default::default() };
    let h1 = an.submit_image(spec.clone(), a.clone(), Some(a.clone()));
    let h2 = an.submit_image(spec, a.clone(), Some(a));
    assert!(h2.generation > h1.generation);
    assert_eq!(an.latest_generation(), h2.generation);
    let r1 = h1.wait_timeout(LIMIT);
    assert!(r1.is_err() || r1.is_ok(), "first job ends either way");
    assert!(h2.wait_timeout(LIMIT).is_ok());
}

#[test]
fn video_job_orders_rows_runs_temporal_and_summarizes() {
    let pool = AnalysisPool::new(Some(4));
    let (w, h) = (96, 64);
    // 0..9 distinct scene A, 10..12 frozen, 13.. a cut to scene B.
    let mut frames: Vec<(u64, Arc<OwnedFrame>, Option<Arc<OwnedFrame>>)> = Vec::new();
    let (sa, sb) = (texture(w, h, 1), texture(w, h, 99).iter().map(|v| 255.0 - v).collect::<Vec<_>>());
    for i in 0..20u64 {
        let base = if i < 13 { &sa } else { &sb };
        let step = if (11..13).contains(&i) { 10 } else { i };
        let moving: Vec<f64> = base.iter().map(|v| v + step as f64 * 0.8).collect();
        let a = Arc::new(yuv420(w, h, quantize(&moving), |_, _| (128, 128)));
        let noise = 1.0 + i as f64 * 0.5;
        let b = Arc::new(yuv420(w, h, quantize(&add_noise(&moving, noise, i)), |_, _| (128, 128)));
        frames.push((i * 2, a, Some(b)));
    }
    let spec = VideoSpec {
        metrics: vec!["psnr".into(), "ssim.fast".into(), "deltae.2000".into(), "temporal.freeze".into(), "temporal.scene".into(), "temporal.flicker".into()],
        total_frames: Some(20),
        ..Default::default()
    };
    let rows = std::sync::Mutex::new(Vec::new());
    let ctl = JobControl::default();
    let rep = run_video(&pool, &spec, frames, &ctl, &|e| {
        if let JobEvent::Row(r) = e {
            rows.lock().unwrap().push(r.idx);
        }
    })
    .unwrap();
    let rows = rows.into_inner().unwrap();
    assert_eq!(rows, (0..20).map(|i| i * 2).collect::<Vec<u64>>(), "rows in order");
    assert_eq!(rep.frames.len(), 20);
    assert_eq!(ctl.progress.done(), 20);
    // PSNR falls as B's noise grows: the worst frame is the last.
    assert_eq!(rep.worst_frames("psnr.y", 1)[0].0, 38);
    assert_eq!(rep.summary_of("psnr.y").unwrap().worst_idx, Some(38));
    let g = rep.summary_of("psnr.global").unwrap().mean.unwrap();
    let m = rep.summary_of("psnr.avg").unwrap().mean.unwrap();
    assert!(g < m + 0.5 && g > 20.0, "global {g} mean {m}");
    // Freeze: frames 11 and 12 (idx 22, 24) duplicate their predecessor in A.
    let dup = rep.series("temporal.freeze.duplicate");
    let flagged: Vec<u64> = dup.iter().filter(|(_, v)| *v == Some(1.0)).map(|(i, _)| *i).collect();
    assert_eq!(flagged, vec![22, 24]);
    assert_eq!(rep.series("temporal.freeze.run")[12].1, Some(2.0));
    // B is noisy everywhere: never a duplicate.
    assert!(rep.series("temporal.freeze.duplicate_b").iter().all(|(_, v)| *v != Some(1.0)));
    // Scene cut at frame 13 (idx 26) only.
    let cuts: Vec<u64> = rep.series("temporal.scene.cut").iter().filter(|(_, v)| *v == Some(1.0)).map(|(i, _)| *i).collect();
    assert_eq!(cuts, vec![26]);
    assert!(rep.series("temporal.flicker.flicker")[0].1 == Some(0.0));
    // CSV: one row per frame, no infinities, header comments.
    let csv = rep.to_csv();
    let lines: Vec<&str> = csv.lines().filter(|l| !l.starts_with('#')).collect();
    assert!(lines[0].starts_with("frame,psnr.y,"));
    assert_eq!(lines.len(), 21);
    assert!(!csv.contains("inf") && !csv.contains("NaN"));
    // The first frame's freeze MAD is undefined: an empty cell, null in JSON.
    let col = rep.column_index("temporal.freeze.mad").unwrap();
    assert_eq!(rep.frames[0].values[col], None);
    assert!(rep.to_json().contains("null"));
}

#[test]
fn video_job_through_channel_with_budget_and_cancel() {
    let an = Analyzer::new(Some(3), None);
    let (w, h) = (128, 96);
    let (tx, rx) = crossbeam_channel::bounded::<(u64, SharedFrame, Option<SharedFrame>)>(4);
    // A "decoder" thread.
    std::thread::spawn(move || {
        for i in 0..40u64 {
            let (a, b) = frame_pair(w, h, i, 2.0);
            if tx.send((i, Arc::new(a), Some(Arc::new(b)))).is_err() {
                break;
            }
        }
    });
    // A tiny budget forces one frame in flight; results are still complete.
    let spec = VideoSpec { metrics: vec!["psnr".into(), "ssim.gauss".into()], memory_budget: 1, total_frames: Some(40), ..Default::default() };
    let rep = an.submit_video(spec, rx).wait_timeout(LIMIT).unwrap();
    assert_eq!(rep.frames.len(), 40);
    // The window size is normal operation, not something to warn about.
    assert!(rep.notes.is_empty(), "{:?}", rep.notes);
    // An endless stream, cancelled after a few rows.
    let endless = (0u64..).map(move |i| {
        let (a, b) = frame_pair(w, h, i % 5, 2.0);
        (i, Arc::new(a) as SharedFrame, Some(Arc::new(b) as SharedFrame))
    });
    let mut hdl = an.submit_video(VideoSpec { metrics: vec!["psnr".into()], ..Default::default() }, endless);
    let mut seen = 0;
    loop {
        match hdl.recv_timeout(LIMIT).expect("event") {
            JobEvent::Row(_) => {
                seen += 1;
                if seen == 5 {
                    hdl.cancel();
                }
            }
            JobEvent::Cancelled => break,
            e => panic!("unexpected {e:?}"),
        }
    }
    assert!(seen >= 5);
}

#[test]
fn roi_on_video_frames() {
    let pool = AnalysisPool::new(Some(2));
    let (w, h) = (101, 75);
    let frames: Vec<(u64, OwnedFrame, Option<OwnedFrame>)> = (0..3).map(|i| {
        let (a, b) = frame_pair(w, h, 3, 4.0);
        (i, a, Some(b))
    }).collect();
    let spec = VideoSpec { metrics: vec!["psnr".into()], roi: Some(lumen_analysis::Roi::new(11, 7, 40, 30)), ..Default::default() };
    let rep = run_video(&pool, &spec, frames, &JobControl::default(), &|_| {}).unwrap();
    let (a, b) = frame_pair(w, h, 3, 4.0);
    let (ca, used) = a.view().crop(lumen_analysis::Roi::new(11, 7, 40, 30)).unwrap();
    assert_eq!(used, lumen_analysis::Roi::new(10, 6, 42, 32));
    let (cb, _) = b.view().crop(used).unwrap();
    let direct = run("psnr", &ca, Some(&cb), &Default::default());
    assert_eq!(rep.series("psnr.y")[0].1, direct.get("y"));
}

#[test]
fn heatmap_png_and_tiff_export() {
    let (w, h) = (80, 60);
    let t = texture(w, h, 2);
    let a = gray_rgba(w, h, &quantize(&t));
    let b = gray_rgba(w, h, &quantize(&add_noise(&t, 8.0, 3)));
    let r = run("diff.abs", &a.view(), Some(&b.view()), &Default::default());
    let m = r.map.unwrap();
    let dir = std::env::temp_dir().join(format!("lumen-analysis-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png_path = dir.join("diff.png");
    m.write_png(&png_path, &ColorizeOptions { range: RangeMode::Fixed, ..m.default_options() }).unwrap();
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&png_path).unwrap()));
    let mut rd = dec.read_info().unwrap();
    let mut buf = vec![0; rd.output_buffer_size().unwrap()];
    let info = rd.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (80, 60));
    assert_eq!(&buf[..info.buffer_size()], &m.colorize(&ColorizeOptions { range: RangeMode::Fixed, ..m.default_options() })[..]);
    let tif_path = dir.join("diff.tif");
    m.write_tiff(&tif_path).unwrap();
    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&tif_path).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (80, 60));
    match dec.read_image().unwrap() {
        tiff::decoder::DecodingResult::F32(v) => assert_eq!(v, m.data),
        _ => panic!("expected f32 TIFF"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn owned_frames_and_preview_downsample() {
    let (w, h) = (31, 17);
    let f = yuv420(w, h, quantize(&texture(w, h, 4)), |x, y| (x as u8, y as u8));
    let half = OwnedFrame::downsample2(&f.view());
    let v = half.view();
    assert_eq!(v.size(), (16, 9));
    let rgb = gray_rgba(w, h, &quantize(&texture(w, h, 4)));
    assert_eq!(OwnedFrame::downsample2(&rgb.view()).view().size(), (16, 9));
    // FrameSource through Arc and Box.
    let shared: Arc<dyn FrameSource> = Arc::new(rgb);
    assert_eq!(shared.view().size(), (31, 17));
}

#[cfg(feature = "flip")]
#[test]
fn heavy_metrics_preview_first() {
    let an = Analyzer::new(Some(4), None);
    let (w, h) = (1280, 860);
    let t = texture(w, h, 7);
    let a: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&t)));
    let b: SharedFrame = Arc::new(gray_rgba(w, h, &quantize(&add_noise(&t, 5.0, 2))));
    let spec = ImageSpec { metrics: vec!["flip".into(), "psnr".into()], ..Default::default() };
    let mut hd = an.submit_image(spec, a, Some(b));
    let (mut partial, mut fin) = (None, None);
    loop {
        match hd.recv_timeout(LIMIT).expect("event") {
            JobEvent::Partial(r) => partial = Some(r),
            JobEvent::Metric(r) if r.id == "flip" => fin = Some(r),
            JobEvent::Done(_) => break,
            JobEvent::Metric(_) => {}
            e => panic!("unexpected {e:?}"),
        }
    }
    let (p, f) = (partial.expect("preview"), fin.expect("final"));
    assert!(p.preview && !f.preview);
    // The preview map covers the frame at half resolution.
    let pm = p.map.unwrap();
    assert_eq!((pm.width, pm.cell_w), (640, 2.0));
    assert_eq!(f.map.unwrap().width, 1280);
}

/// A frame whose pixels cannot be read: its metrics panic.
struct Broken;

impl FrameSource for Broken {
    fn view(&self) -> lumen_analysis::FrameView<'_> {
        panic!("unreadable frame (test)")
    }
}

#[test]
fn panicking_frame_never_hangs_a_video_job() {
    let an = Analyzer::new(Some(2), None);
    let (w, h) = (64, 48);
    let frames = (0u64..6).map(move |i| {
        let (a, b) = frame_pair(w, h, i, 2.0);
        // Frame 3 of B cannot be read; the first frame sizes the job, so it is fine.
        let b: SharedFrame = if i == 3 { Arc::new(Broken) } else { Arc::new(b) };
        (i, Arc::new(a) as SharedFrame, Some(b))
    });
    let rep = an.submit_video(VideoSpec { metrics: vec!["psnr".into()], ..Default::default() }, frames).wait_timeout(LIMIT).unwrap();
    assert_eq!(rep.frames.len(), 6);
    assert!(rep.notes.iter().any(|n| n.contains("internal error")), "{:?}", rep.notes);
}
