//! The flows documented in API.md, end to end (keeps the guide honest).

use lumen_analysis::cache::{CacheKey, DiskCache, DEFAULT_MAX_BYTES};
use lumen_analysis::frame::{Plane, PlaneView};
use lumen_analysis::{
    metrics, AnalysisReport, Analyzer, ColorizeOptions, FrameRange, FrameView, ImageSpec, JobEvent, Matrix, OwnedFrame, Range, RangeMode, RgbData, RgbView, Roi, Scope, SharedFrame, Transfer, VideoSpec, YuvView,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn rgba(w: usize, h: usize, shift: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[((x * 3 + y) % 256) as u8, ((x ^ y) % 256) as u8 / 2 + shift, ((y * 5) % 256) as u8, 255]);
        }
    }
    v
}

#[test]
fn image_flow_from_guide() {
    let (w, h) = (240, 200);
    // Borrowed views, including a strided buffer.
    let bytes = rgba(w, h, 0);
    let view = FrameView::Rgb(RgbView::rgba8(&bytes, w, h).unwrap());
    let strided = FrameView::Rgb(RgbView::new(RgbData::U8(&bytes), w - 10, h, w * 4, 4).unwrap());
    assert_eq!(strided.size(), (w - 10, h));
    assert_eq!(view.size(), (w, h));

    let repaints = Arc::new(AtomicUsize::new(0));
    let r2 = repaints.clone();
    let analyzer = Analyzer::new(Some(2), Some(Arc::new(move || {
        r2.fetch_add(1, Ordering::Relaxed);
    })));
    let a: SharedFrame = Arc::new(OwnedFrame::rgba8(w, h, bytes.clone()));
    let b: SharedFrame = Arc::new(OwnedFrame::rgba8(w, h, rgba(w, h, 3)));
    let spec = ImageSpec {
        metrics: metrics::preset("encode_qa").iter().map(|s| s.to_string()).collect(),
        roi: Some(Roi::new(20, 10, 200, 180)),
        scope: Scope { file_a: Some("a.png".into()), file_b: Some("b.png".into()), ..Default::default() },
        ..Default::default()
    };
    let mut job = analyzer.submit_image(spec, a, Some(b));
    assert_eq!(job.generation, analyzer.latest_generation());
    let mut report = None;
    let mut metrics_seen = Vec::new();
    // The UI polls each frame; here we poll with a short sleep, for at most
    // 5 minutes (a loaded machine is slow; a hang must still end the test).
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    while std::time::Instant::now() < deadline {
        while let Some(ev) = job.try_recv() {
            match ev {
                JobEvent::Metric(r) => metrics_seen.push(r.id),
                JobEvent::Done(r) => report = Some(*r),
                JobEvent::Failed(m) => panic!("{m}"),
                _ => {}
            }
        }
        if report.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = report.expect("job finished");
    assert!(repaints.load(Ordering::Relaxed) > 0);
    assert!(metrics_seen.contains(&"ssim.gauss".to_string()));
    assert_eq!(report.scope.roi, Some(Roi::new(20, 10, 200, 180)));
    assert!(report.value("ssim.gauss.y").unwrap() > 0.5);
    // Temporal ids in an image job are noted, not run.
    assert!(report.notes.iter().any(|n| n.contains("temporal")));

    // Heatmap for display.
    let m = report.map("ssim.gauss").unwrap().for_display();
    let opts = ColorizeOptions { range: RangeMode::Auto, alpha: 160, ..m.default_options() };
    let px = m.colorize(&opts);
    assert_eq!(px.len(), m.width as usize * m.height as usize * 4);
    let (x, y, ew, eh) = m.extent();
    assert_eq!((x, y, ew, eh), (25.0, 15.0, 190.0, 170.0));
    assert!(m.value_at(100.0, 100.0).is_some() && m.value_at(5.0, 5.0).is_none());
    assert_eq!(lumen_analysis::heatmap::legend(opts.colormap, 256, opts.invert).len(), 256);

    // Export and cache.
    let dir = std::env::temp_dir().join(format!("lumen-analysis-api-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    report.write_json(&dir.join("r.json")).unwrap();
    report.write_csv(&dir.join("r.csv")).unwrap();
    m.write_png(&dir.join("m.png"), &opts).unwrap();
    m.write_tiff(&dir.join("m.tif")).unwrap();
    let cache = DiskCache::new(dir.join("analysis"), DEFAULT_MAX_BYTES);
    let key = CacheKey::builder()
        .file("a", &dir.join("r.json"))
        .metric("ssim.gauss", metrics::get("ssim.gauss").unwrap().info().version)
        .params(&lumen_analysis::MetricOptions::default())
        .roi(Some(Roi::new(20, 10, 200, 180)))
        .range(None)
        .finish();
    assert!(cache.get::<AnalysisReport>(&key).is_none());
    cache.put(&key, &report).unwrap();
    let back: AnalysisReport = cache.get(&key).unwrap();
    assert_eq!(back.value("ssim.gauss.y"), report.value("ssim.gauss.y"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Planes with a row stride larger than the width, as FFmpeg hands them out.
struct StridedYuv {
    w: usize,
    h: usize,
    stride: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl lumen_analysis::FrameSource for StridedYuv {
    fn view(&self) -> FrameView<'_> {
        let (cw, ch) = (self.w.div_ceil(2), self.h.div_ceil(2));
        let y = PlaneView::U8(Plane::new(&self.y, self.w, self.h, self.stride).unwrap());
        let u = PlaneView::U8(Plane::new(&self.u, cw, ch, self.stride / 2).unwrap());
        let v = PlaneView::U8(Plane::new(&self.v, cw, ch, self.stride / 2).unwrap());
        FrameView::Yuv(YuvView::new(y, Some((u, v)), 8).unwrap().with_color(Matrix::Bt709, Range::Limited, Transfer::Srgb))
    }
}

fn strided(w: usize, h: usize, t: u64, noise: u64) -> StridedYuv {
    let stride = w + 32;
    let mut y = vec![0u8; stride * h];
    for r in 0..h {
        for c in 0..w {
            let n = ((c as u64 * 7 + r as u64 * 13 + t * noise) % 5) as u8;
            y[r * stride + c] = (40 + (c + r + t as usize * 3) % 160) as u8 + n;
        }
    }
    let u = vec![128u8; stride / 2 * h.div_ceil(2)];
    StridedYuv { w, h, stride, y, u: u.clone(), v: u }
}

#[test]
fn video_flow_from_guide() {
    let analyzer = Analyzer::new(Some(4), None);
    let (tx, rx) = crossbeam_channel::bounded::<(u64, SharedFrame, Option<SharedFrame>)>(8);
    std::thread::spawn(move || {
        for idx in (0..60u64).step_by(2) {
            let a: SharedFrame = Arc::new(strided(160, 90, idx, 0));
            let b: SharedFrame = Arc::new(strided(160, 90, idx, 1));
            if tx.send((idx, a, Some(b))).is_err() {
                break;
            }
        }
    });
    let spec = VideoSpec {
        metrics: vec!["psnr".into(), "ssim.fast".into(), "temporal.freeze".into(), "temporal.scene".into()],
        total_frames: Some(30),
        scope: Scope { file_a: Some("a.mkv".into()), file_b: Some("b.mkv".into()), frames: Some(FrameRange { start: 0, end: 60, stride: 2 }), ..Default::default() },
        ..Default::default()
    };
    let mut job = analyzer.submit_video(spec, rx);
    let mut rows = Vec::new();
    let report = loop {
        match job.recv_timeout(Duration::from_secs(300)).expect("event") {
            JobEvent::Row(r) => rows.push(r.idx),
            JobEvent::Done(r) => break *r,
            e => panic!("unexpected {e:?}"),
        }
    };
    assert_eq!(rows, (0..60).step_by(2).collect::<Vec<u64>>());
    assert_eq!(job.progress().done(), 30);
    assert_eq!(report.frames.len(), 30);
    assert!(report.summary_of("psnr.y").unwrap().mean.unwrap() > 30.0);
    assert!(report.summary_of("psnr.global").is_some());
    assert_eq!(report.worst_frames("ssim.fast.y", 3).len(), 3);
    let csv = report.to_csv();
    assert!(csv.contains("# scope: video range, 160x90, frames 0..60 step 2"), "{csv}");
    assert!(csv.contains("# A: a.mkv"));
}
