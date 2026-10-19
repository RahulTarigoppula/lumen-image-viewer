//! Adapters from Lumen's decoded pixels to `lumen_analysis` frames and jobs:
//! the viewer's and compare's display images (8-bit sRGB RGBA, shared without
//! copying), and video mode's native planes, either the paused frames or a
//! frame range decoded by `SeqDecoder` for A and B (offset aware).

use image::RgbaImage;
use lumen_analysis::{FrameSource, FrameView, ImageSpec, RgbView, Roi, Scope, ScopeKind, SharedFrame};
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// A decoded display image (8-bit sRGB RGBA) as an analysis frame. The
/// pixels are shared with the viewer's cache, not copied.
pub struct RgbaFrame(pub Arc<RgbaImage>);

impl FrameSource for RgbaFrame {
    fn view(&self) -> FrameView<'_> {
        rgba_view(&self.0).expect("non-empty RGBA image")
    }
}

/// Borrowed analysis view of an RGBA image; `None` for an empty image.
pub fn rgba_view(img: &RgbaImage) -> Option<FrameView<'_>> {
    RgbView::rgba8(img.as_raw(), img.width() as usize, img.height() as usize).ok().map(FrameView::Rgb)
}

/// B resampled to A's size on first use (Lanczos 3), so the resize runs on
/// the analysis job's thread instead of the UI thread.
pub struct ScaledFrame {
    src: Arc<RgbaImage>,
    size: (u32, u32),
    scaled: OnceLock<RgbaImage>,
}

impl ScaledFrame {
    pub fn new(src: Arc<RgbaImage>, size: (u32, u32)) -> ScaledFrame {
        ScaledFrame { src, size: (size.0.max(1), size.1.max(1)), scaled: OnceLock::new() }
    }
}

impl FrameSource for ScaledFrame {
    fn view(&self) -> FrameView<'_> {
        let img = self.scaled.get_or_init(|| crate::imaging::resize_exact(&self.src, self.size.0, self.size.1, crate::imaging::Filter::Lanczos3));
        rgba_view(img).expect("non-empty RGBA image")
    }
}

/// Frames for an image job: A, and B at A's size.
pub struct FramePair {
    pub a: SharedFrame,
    pub b: Option<SharedFrame>,
    /// B had a different size and is resampled to A's.
    pub rescaled: bool,
}

/// A (and B) display images as analysis frames; `None` when A is empty.
pub fn image_pair(a: &Arc<RgbaImage>, b: Option<&Arc<RgbaImage>>) -> Option<FramePair> {
    let size = a.dimensions();
    if size.0 == 0 || size.1 == 0 {
        return None;
    }
    let mut rescaled = false;
    let b = b.filter(|b| b.width() > 0 && b.height() > 0).map(|b| -> SharedFrame {
        if b.dimensions() == size {
            Arc::new(RgbaFrame(b.clone()))
        } else {
            rescaled = true;
            Arc::new(ScaledFrame::new(b.clone(), size))
        }
    });
    Some(FramePair { a: Arc::new(RgbaFrame(a.clone())), b, rescaled })
}

/// Image job spec for display pixels. `source_bits` is the bit depth of the
/// file (the pixels measured are always 8-bit).
pub fn image_spec(metrics: Vec<String>, roi: Option<Roi>, file_a: Option<&Path>, file_b: Option<&Path>, rescaled: bool, source_bits: u8) -> ImageSpec {
    let scope = Scope {
        kind: ScopeKind::Image,
        file_a: file_a.map(|p| p.display().to_string()),
        file_b: file_b.map(|p| p.display().to_string()),
        rescaled,
        measured_on: (source_bits > 8).then(|| "8-bit display data".to_string()),
        ..Default::default()
    };
    ImageSpec { metrics, roi, scope, ..Default::default() }
}

/// Image coordinates `(x, y)` of a pixel and its RGBA value, if inside.
pub fn pixel_at(img: &RgbaImage, x: f32, y: f32) -> Option<[u8; 4]> {
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let (x, y) = (x as u32, y as u32);
    (x < img.width() && y < img.height()).then(|| img.get_pixel(x, y).0)
}

#[cfg(feature = "video")]
pub use video::*;

#[cfg(feature = "video")]
mod video {
    use super::*;
    use crate::video::engine::{self, FrameRange, IndexEntry, NativeFormat, SeqDecoder, YuvMatrix, YuvRange};
    use crate::video::metrics::YuvFrame;
    use anyhow::{anyhow, Result};
    use lumen_analysis::{Analyzer, JobHandle, Matrix, Range, Transfer, VideoSpec};
    use parking_lot::Mutex;
    use std::path::PathBuf;

    /// How a stream's YUV samples map to RGB.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct VideoColor {
        pub matrix: Matrix,
        pub range: Range,
        pub transfer: Transfer,
    }

    impl VideoColor {
        /// From the layout `SeqDecoder` reports for its frames.
        pub fn of_format(f: &NativeFormat) -> VideoColor {
            let matrix = match f.matrix {
                YuvMatrix::Bt601 | YuvMatrix::Fcc => Matrix::Bt601,
                YuvMatrix::Bt2020Ncl | YuvMatrix::Bt2020Cl => Matrix::Bt2020,
                // SMPTE 240M is close to BT.709; YCgCo and RGB planes have no
                // matching matrix and are rare in practice.
                YuvMatrix::Bt709 | YuvMatrix::Smpte240m | YuvMatrix::YCgCo | YuvMatrix::Identity => Matrix::Bt709,
            };
            let range = if f.range == YuvRange::Full { Range::Full } else { Range::Limited };
            VideoColor { matrix, range, transfer: transfer_of(f.transfer) }
        }

        /// From the probe of a file (the paused player frames carry no
        /// format). Unspecified matrices get playback's guess.
        pub fn of_info(info: &engine::MediaInfo) -> VideoColor {
            let matrix = match info.color_space.as_str() {
                "BT.601" => Matrix::Bt601,
                "BT.2020" => Matrix::Bt2020,
                "BT.709" | "SMPTE 240M" => Matrix::Bt709,
                _ if info.height >= 720 => Matrix::Bt709,
                _ => Matrix::Bt601,
            };
            let range = if info.color_range.starts_with("full") || info.pix_fmt.starts_with("yuvj") { Range::Full } else { Range::Limited };
            let transfer = if info.color_transfer.starts_with("PQ") {
                Transfer::Pq
            } else if info.color_transfer == "HLG" {
                Transfer::Hlg
            } else {
                Transfer::Srgb
            };
            VideoColor { matrix, range, transfer }
        }
    }

    fn transfer_of(t: engine::Transfer) -> Transfer {
        match t {
            engine::Transfer::Pq => Transfer::Pq,
            engine::Transfer::Hlg => Transfer::Hlg,
            engine::Transfer::Sdr | engine::Transfer::Srgb | engine::Transfer::Other => Transfer::Srgb,
        }
    }

    /// Native planes of a video frame with their color meaning.
    pub struct NativeFrame {
        pub frame: Arc<YuvFrame>,
        pub color: VideoColor,
    }

    impl NativeFrame {
        /// `None` for layouts the analysis crate cannot describe.
        pub fn new(frame: Arc<YuvFrame>, color: VideoColor) -> Option<NativeFrame> {
            frame.analysis_view()?;
            Some(NativeFrame { frame, color })
        }
    }

    impl FrameSource for NativeFrame {
        fn view(&self) -> FrameView<'_> {
            match self.frame.analysis_view().expect("checked in NativeFrame::new") {
                FrameView::Yuv(y) => FrameView::Yuv(y.with_color(self.color.matrix, self.color.range, self.color.transfer)),
                v => v,
            }
        }
    }

    pub fn to_engine_roi(r: Roi) -> engine::Roi {
        engine::Roi::new(r.x, r.y, r.w, r.h)
    }

    /// One video file of a job.
    #[derive(Clone)]
    pub struct VideoSide {
        pub path: PathBuf,
        /// The player's frame index, when built (saves a pass over the file).
        pub index: Option<Arc<Vec<IndexEntry>>>,
        pub fps: f64,
    }

    impl VideoSide {
        /// Presentation time of frame `idx` (the index, else counted from the frame rate).
        pub fn pts_of(&self, idx: usize) -> f64 {
            match self.index.as_ref().and_then(|ix| ix.get(idx)) {
                Some(e) => e.pts,
                None => idx as f64 / self.fps.max(1.0),
            }
        }
    }

    /// Single-frame job on the paused frames (A, and B when comparing).
    #[allow(clippy::too_many_arguments)]
    pub fn submit_video_frame(analyzer: &Analyzer, a: NativeFrame, b: Option<NativeFrame>, idx: usize, metrics: Vec<String>, roi: Option<Roi>, file_a: &Path, file_b: Option<&Path>) -> JobHandle {
        let scope = Scope {
            kind: ScopeKind::VideoFrame,
            file_a: Some(file_a.display().to_string()),
            file_b: file_b.map(|p| p.display().to_string()),
            frames: Some(lumen_analysis::FrameRange { start: idx as u64, end: idx as u64 + 1, stride: 1 }),
            ..Default::default()
        };
        let spec = ImageSpec { metrics, roi, scope, ..Default::default() };
        analyzer.submit_image(spec, Arc::new(a), b.map(|b| Arc::new(b) as SharedFrame))
    }

    /// A range of A (and the B frames `offset` later) to measure.
    #[derive(Clone)]
    pub struct VideoRangeRequest {
        pub a: VideoSide,
        pub b: Option<VideoSide>,
        /// B frame = A frame + offset.
        pub offset: i64,
        pub range: FrameRange,
        pub roi: Option<Roi>,
        pub metrics: Vec<String>,
        /// Bytes for frames in flight (the analysis crate's default is 1 GB).
        pub memory_budget: u64,
        /// A's frame count, for progress and ETA.
        pub frames_a: Option<usize>,
    }

    /// What the decoders of a range job found out while reading.
    #[derive(Default)]
    pub struct DecodeInfo {
        /// Set when decoding stopped early; the report then covers the
        /// frames read until then.
        pub error: Mutex<Option<String>>,
        /// The ROI as cut by the decoder (snapped to the chroma grid) and the
        /// full frame size; the job itself only sees the cropped frames.
        pub cut: Mutex<Option<(Roi, (u32, u32))>>,
    }

    impl DecodeInfo {
        /// Report fix-ups for decoder-side cropping and early ends: the scope
        /// describes the full frame and the region measured.
        pub fn fix_scope(&self, report: &mut lumen_analysis::AnalysisReport) {
            if let Some((roi, (w, h))) = *self.cut.lock() {
                report.scope.roi = Some(roi);
                report.scope.width = w;
                report.scope.height = h;
            }
            if let Some(e) = self.error.lock().clone() {
                report.notes.push(format!("decoding stopped early: {e}"));
            }
        }
    }

    /// A running range job.
    pub struct VideoRangeJob {
        pub handle: JobHandle,
        pub info: Arc<DecodeInfo>,
    }

    /// Frames of `req.range` that will be measured.
    pub fn range_count(req: &VideoRangeRequest) -> Option<usize> {
        let total = req.frames_a?;
        let r = if req.b.is_some() { a_range(req.range, req.offset) } else { req.range };
        let mut n = r.count(total);
        if let (Some(b), true) = (req.b.as_ref().and_then(|b| b.index.as_ref()), req.offset > 0) {
            // A frames whose partner would lie past B's end.
            let last_a = (b.len() as i64 - req.offset).max(0) as usize;
            n = FrameRange { end: Some(r.end.unwrap_or(total).min(last_a)), ..r }.count(total);
        }
        Some(n)
    }

    /// Decode A (and B) on their own threads and measure them with
    /// `analyzer`. The ROI is cut by the decoders (no full frames are copied),
    /// stride frames are decoded but not copied, and a bounded channel keeps
    /// the decoders from running ahead of the memory budget.
    pub fn submit_video_range(analyzer: &Analyzer, req: VideoRangeRequest) -> VideoRangeJob {
        let (tx, rx) = crossbeam_channel::bounded::<(u64, SharedFrame, Option<SharedFrame>)>(4);
        let info = Arc::new(DecodeInfo::default());
        let r = req.range;
        let scope = Scope {
            kind: ScopeKind::VideoRange,
            file_a: Some(req.a.path.display().to_string()),
            file_b: req.b.as_ref().map(|b| b.path.display().to_string()),
            frames: Some(lumen_analysis::FrameRange {
                start: r.start as u64,
                end: r.end.or(req.frames_a).unwrap_or(r.start) as u64,
                stride: r.stride.max(1) as u64,
            }),
            ..Default::default()
        };
        let spec = VideoSpec {
            metrics: req.metrics.clone(),
            total_frames: range_count(&req).map(|n| n as u64),
            memory_budget: req.memory_budget,
            scope,
            ..Default::default()
        };
        {
            let info = info.clone();
            std::thread::Builder::new()
                .name("lumen-analysis-decode".into())
                .spawn(move || {
                    if let Err(e) = feed(&req, &tx, &info.cut) {
                        *info.error.lock() = Some(format!("{e:#}"));
                    }
                })
                .expect("analysis decode thread");
        }
        let handle = analyzer.submit_video(spec, rx);
        VideoRangeJob { handle, info }
    }

    type Tx = crossbeam_channel::Sender<(u64, SharedFrame, Option<SharedFrame>)>;

    fn open(side: &VideoSide, roi: Option<Roi>) -> Result<SeqDecoder> {
        let mut d = SeqDecoder::open(&side.path)?;
        if let Some(ix) = &side.index {
            d.set_index(ix.clone());
        }
        d.set_roi(roi.map(to_engine_roi));
        Ok(d)
    }

    fn native(f: YuvFrame, fmt: Option<NativeFormat>) -> Option<SharedFrame> {
        let color = fmt.as_ref().map(VideoColor::of_format).unwrap_or_default();
        NativeFrame::new(Arc::new(f), color).map(|n| Arc::new(n) as SharedFrame)
    }

    /// Decoder side of a range job: reads A (and B) and sends pairs to the job.
    fn feed(req: &VideoRangeRequest, tx: &Tx, cut: &Mutex<Option<(Roi, (u32, u32))>>) -> Result<()> {
        let da = open(&req.a, req.roi)?;
        let size = da.size();
        let Some(b) = &req.b else {
            let mut it = da.into_range(req.range);
            while let Some((i, f)) = it.next() {
                note_cut(cut, req.roi, size, it.format());
                let Some(fa) = native(f, it.format()) else { return Err(anyhow!("A: unsupported pixel layout")) };
                if tx.send((i as u64, fa, None)).is_err() {
                    return Ok(());
                }
            }
            return it.error().map_or(Ok(()), |e| Err(anyhow!("A: {e:#}")));
        };
        // B is opened on its own thread (FFmpeg contexts are not Send); open
        // it here first so a size mismatch hidden by the ROI is caught.
        if req.roi.is_some() && SeqDecoder::open(&b.path)?.size() != size {
            return Err(anyhow!("A and B have different resolutions; a region needs identical sizes"));
        }
        let range = a_range(req.range, req.offset);
        let b_range = FrameRange {
            start: (range.start as i64 + req.offset).max(0) as usize,
            end: range.end.map(|e| (e as i64 + req.offset).max(0) as usize),
            stride: range.stride,
            keyframes_only: false,
        };
        let (btx, brx) = crossbeam_channel::bounded::<(usize, SharedFrame)>(4);
        let b_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        std::thread::scope(|s| -> Result<()> {
            let (side, roi, b_error2) = (b.clone(), req.roi, b_error.clone());
            s.spawn(move || {
                let db = match open(&side, roi) {
                    Ok(d) => d,
                    Err(e) => {
                        *b_error2.lock() = Some(format!("{e:#}"));
                        return;
                    }
                };
                let mut it = db.into_range(b_range);
                while let Some((i, f)) = it.next() {
                    let Some(fb) = native(f, it.format()) else {
                        *b_error2.lock() = Some("unsupported pixel layout".into());
                        return;
                    };
                    if btx.send((i, fb)).is_err() {
                        return;
                    }
                }
                if let Some(e) = it.error() {
                    *b_error2.lock() = Some(format!("{e:#}"));
                }
            });
            let mut it = da.into_range(range);
            let mut a_frames = std::iter::from_fn(|| {
                let (i, f) = it.next()?;
                note_cut(cut, req.roi, size, it.format());
                Some((i, native(f, it.format())))
            });
            let mut pairs = Pairs::new(&mut a_frames, brx.iter(), req.offset);
            let mut result = Ok(());
            for (i, fa, fb) in pairs.by_ref() {
                let Some(fa) = fa else {
                    result = Err(anyhow!("A: unsupported pixel layout"));
                    break;
                };
                if tx.send((i as u64, fa, Some(fb))).is_err() {
                    break;
                }
            }
            drop(pairs);
            drop(brx);
            drop(a_frames);
            result?;
            if let Some(e) = it.error() {
                return Err(anyhow!("A: {e:#}"));
            }
            Ok(())
        })?;
        let b_error = b_error.lock().take();
        match b_error {
            Some(e) => Err(anyhow!("B: {e}")),
            None => Ok(()),
        }
    }

    fn note_cut(cut: &Mutex<Option<(Roi, (u32, u32))>>, roi: Option<Roi>, size: (u32, u32), fmt: Option<NativeFormat>) {
        let (Some(r), Some(f)) = (roi, fmt) else { return };
        let mut c = cut.lock();
        if c.is_none() {
            let s = to_engine_roi(r).snapped(size.0, size.1, f.chroma_shift);
            *c = Some((Roi::new(s.x, s.y, s.w, s.h), size));
        }
    }

    /// `r` without the A frames a negative offset leaves without a B partner,
    /// keeping the stride grid.
    pub fn a_range(r: FrameRange, offset: i64) -> FrameRange {
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

    /// A frame thumbnail strip: A, and B beside it when comparing.
    pub fn frame_pair_thumb(a: &VideoSide, b: Option<&VideoSide>, offset: i64, idx: usize, height: u32) -> Result<RgbaImage> {
        let ta = crate::imaging::resize_to_fit(&engine::frame_at(&a.path, a.pts_of(idx))?, height * 4, height);
        let Some(b) = b else { return Ok(ta) };
        let ib = (idx as i64 + offset).max(0) as usize;
        let tb = crate::imaging::resize_to_fit(&engine::frame_at(&b.path, b.pts_of(ib))?, height * 4, height);
        let gap = 2;
        let mut out = RgbaImage::from_pixel(ta.width() + gap + tb.width(), ta.height().max(tb.height()), image::Rgba([0, 0, 0, 0]));
        image::imageops::overlay(&mut out, &ta, 0, 0);
        image::imageops::overlay(&mut out, &tb, (ta.width() + gap) as i64, 0);
        Ok(out)
    }

    /// Worst-frame thumbnails decoded on a background thread, A and B side by side.
    pub struct VideoThumbs {
        a: VideoSide,
        b: Option<VideoSide>,
        offset: i64,
        tex: std::collections::HashMap<u64, Option<egui::TextureHandle>>,
        tx: crossbeam_channel::Sender<(u64, Option<RgbaImage>)>,
        rx: crossbeam_channel::Receiver<(u64, Option<RgbaImage>)>,
    }

    impl VideoThumbs {
        pub fn new(a: VideoSide, b: Option<VideoSide>, offset: i64) -> VideoThumbs {
            let (tx, rx) = crossbeam_channel::unbounded();
            VideoThumbs { a, b, offset, tex: Default::default(), tx, rx }
        }

        /// Whether these thumbnails show `a` and `b` at `offset`.
        pub fn shows(&self, a: &Path, b: Option<&Path>, offset: i64) -> bool {
            self.a.path == a && self.b.as_ref().map(|b| b.path.as_path()) == b && self.offset == offset
        }
    }

    impl crate::analysis_ui::plots::FrameThumbs for VideoThumbs {
        fn get(&mut self, ctx: &egui::Context, idx: u64) -> Option<(egui::TextureId, egui::Vec2)> {
            while let Ok((i, img)) = self.rx.try_recv() {
                let t = img.map(|img| ctx.load_texture(format!("analysis-thumb-{i}"), egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()), egui::TextureOptions::LINEAR));
                self.tex.insert(i, t);
            }
            match self.tex.get(&idx) {
                Some(Some(t)) => Some((t.id(), t.size_vec2())),
                Some(None) => None,
                None => {
                    self.tex.insert(idx, None);
                    let (a, b, offset, tx, ctx) = (self.a.clone(), self.b.clone(), self.offset, self.tx.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let img = frame_pair_thumb(&a, b.as_ref(), offset, idx as usize, 80).ok();
                        let _ = tx.send((idx, img));
                        ctx.request_repaint();
                    });
                    None
                }
            }
        }
    }

    /// Writes `frame_<n>_A.png` (and `_B.png`) for each of `frames` into
    /// `dir`; returns the files written.
    pub fn export_frame_pairs(a: &VideoSide, b: Option<&VideoSide>, offset: i64, frames: &[u64], dir: &Path) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = Vec::new();
        for &f in frames {
            let idx = f as usize;
            let pa = dir.join(format!("frame_{idx:06}_A.png"));
            engine::frame_at(&a.path, a.pts_of(idx))?.save(&pa)?;
            out.push(pa);
            if let Some(b) = b {
                let ib = (idx as i64 + offset).max(0) as usize;
                let pb = dir.join(format!("frame_{idx:06}_B.png"));
                engine::frame_at(&b.path, b.pts_of(ib))?.save(&pb)?;
                out.push(pb);
            }
        }
        Ok(out)
    }
}

/// Pairs A frames with the B frames `offset` later, both in ascending index
/// order: B frames without an A partner are dropped, and so are A frames
/// whose partner is missing in B. Ends when either side ends.
pub struct Pairs<IA, IB, B> {
    a: IA,
    b: IB,
    offset: i64,
    pending: Option<(usize, B)>,
}

impl<IA, IB, A, B> Pairs<IA, IB, B>
where
    IA: Iterator<Item = (usize, A)>,
    IB: Iterator<Item = (usize, B)>,
{
    pub fn new(a: IA, b: IB, offset: i64) -> Self {
        Pairs { a, b, offset, pending: None }
    }
}

impl<IA, IB, A, B> Iterator for Pairs<IA, IB, B>
where
    IA: Iterator<Item = (usize, A)>,
    IB: Iterator<Item = (usize, B)>,
{
    type Item = (usize, A, B);

    fn next(&mut self) -> Option<(usize, A, B)> {
        loop {
            let (ia, fa) = self.a.next()?;
            let want = ia as i64 + self.offset;
            if want < 0 {
                continue;
            }
            let want = want as usize;
            while self.pending.as_ref().is_none_or(|(ib, _)| *ib < want) {
                self.pending = Some(self.b.next()?);
            }
            if let Some((_, fb)) = self.pending.take_if(|(ib, _)| *ib == want) {
                return Some((ia, fa, fb));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y))))
    }

    #[test]
    fn rgba_frames_share_pixels() {
        let a = img(7, 5, |x, y| [x as u8 * 30, y as u8 * 40, 9, 255]);
        let f = RgbaFrame(a.clone());
        let v = f.view();
        assert_eq!(v.size(), (7, 5));
        let FrameView::Rgb(rgb) = v else { panic!("RGB view") };
        assert!(matches!(rgb.data, lumen_analysis::RgbData::U8(d) if d.as_ptr() == a.as_raw().as_ptr()));
        assert!(rgba_view(&RgbaImage::new(0, 0)).is_none());
        assert!(image_pair(&Arc::new(RgbaImage::new(0, 3)), None).is_none());
    }

    #[test]
    fn pair_rescales_b_to_a() {
        let a = img(40, 30, |x, _| [x as u8 * 6, 0, 0, 255]);
        let same = image_pair(&a, Some(&a.clone())).unwrap();
        assert!(!same.rescaled);
        let b = img(20, 15, |x, _| [x as u8 * 12, 0, 0, 255]);
        let p = image_pair(&a, Some(&b)).unwrap();
        assert!(p.rescaled);
        assert_eq!(p.b.as_ref().unwrap().view().size(), (40, 30));
        assert!(image_pair(&a, None).unwrap().b.is_none());
    }

    #[test]
    fn image_job_on_synthetic_images() {
        let a = img(64, 64, |x, y| [(x * 4) as u8, (y * 4) as u8, 128, 255]);
        let b = img(64, 64, |x, y| [(x * 4) as u8, (y * 4) as u8, if x < 8 { 120 } else { 128 }, 255]);
        let p = image_pair(&a, Some(&b)).unwrap();
        let spec = image_spec(vec!["psnr".into(), "deltae.2000".into(), "noise".into()], None, Some(Path::new("a.png")), Some(Path::new("b.png")), p.rescaled, 16);
        assert_eq!(spec.scope.measured_on.as_deref(), Some("8-bit display data"));
        let pool = lumen_analysis::job::AnalysisPool::new(Some(2));
        let (va, vb) = (p.a.view(), p.b.as_ref().unwrap().view());
        let r = lumen_analysis::job::run_image(&pool, &spec, &va, Some(&vb), &Default::default(), &|_| {}).unwrap();
        let psnr = r.value("psnr.avg").unwrap();
        assert!(psnr > 30.0 && psnr < 100.0, "{psnr}");
        assert!(r.value("deltae.2000.max").unwrap() > 0.5);
        assert!(r.map("deltae.2000").is_some());
        assert_eq!(r.scope.file_b.as_deref(), Some("b.png"));
        // Identical: capped PSNR, zero Delta-E.
        let same = lumen_analysis::job::run_image(&pool, &spec, &va, Some(&va), &Default::default(), &|_| {}).unwrap();
        assert_eq!(same.value("psnr.avg"), Some(lumen_analysis::PSNR_CAP_DB));
        assert_eq!(same.value("deltae.2000.max"), Some(0.0));
    }

    #[test]
    fn pixel_lookup() {
        let a = img(4, 3, |x, y| [x as u8, y as u8, 0, 255]);
        assert_eq!(pixel_at(&a, 3.9, 2.2), Some([3, 2, 0, 255]));
        assert_eq!(pixel_at(&a, 4.0, 0.0), None);
        assert_eq!(pixel_at(&a, -0.5, 0.0), None);
    }

    fn pairs(a: &[usize], b: &[usize], offset: i64) -> Vec<(usize, usize)> {
        Pairs::new(a.iter().map(|&i| (i, i)), b.iter().map(|&i| (i, i)), offset).map(|(i, _, fb)| (i, fb)).collect()
    }

    #[test]
    fn pairs_follow_the_offset() {
        let all: Vec<usize> = (0..10).collect();
        assert_eq!(pairs(&[0, 3, 6], &all, 0), vec![(0, 0), (3, 3), (6, 6)]);
        assert_eq!(pairs(&[0, 3, 6, 9], &all, 2), vec![(0, 2), (3, 5), (6, 8)]);
        assert_eq!(pairs(&[0, 3, 6, 9], &all, -4), vec![(6, 2), (9, 5)]);
        // Missing B frames skip their A partner.
        assert_eq!(pairs(&[1, 2, 3, 4], &[1, 3, 4], 0), vec![(1, 1), (3, 3), (4, 4)]);
        assert_eq!(pairs(&[1, 2], &[], 0), vec![]);
    }

    #[cfg(feature = "video")]
    #[test]
    fn a_range_keeps_the_grid() {
        use crate::video::engine::FrameRange;
        assert_eq!(a_range(FrameRange::new(1, Some(12), 3), -5).start, 7);
        assert_eq!(a_range(FrameRange::new(10, None, 3), 2).start, 10);
        assert_eq!(a_range(FrameRange::new(0, None, 1), -3).start, 3);
    }

    #[cfg(feature = "video")]
    #[test]
    fn range_job_with_offset_roi_and_stride() {
        use crate::video::engine::test_media::{write_y4m, TestDir};
        use crate::video::engine::FrameRange;
        let dir = TestDir::new("adapters-range");
        let (a, b) = (dir.path("a.y4m"), dir.path("b.y4m"));
        let base = |f: usize, plane: usize, x: usize, y: usize| ((f * 13 + x * 2 + y + plane * 50) % 200) as u8;
        write_y4m(&a, 64, 48, 20, base);
        // B = A shifted by 2 frames, with noise in the top-left 16x16 block.
        write_y4m(&b, 64, 48, 22, move |f, plane, x, y| base(f.saturating_sub(2), plane, x, y) + if plane == 0 && x < 16 && y < 16 { ((x * 7 + y * 3) % 5) as u8 } else { 0 });
        let side = |p: &Path| VideoSide { path: p.to_path_buf(), index: None, fps: 25.0 };
        let analyzer = lumen_analysis::Analyzer::new(Some(2), None);
        let req = VideoRangeRequest {
            a: side(&a),
            b: Some(side(&b)),
            offset: 2,
            range: FrameRange::new(1, Some(15), 4),
            roi: Some(Roi::new(20, 20, 31, 20)),
            metrics: vec!["psnr".into(), "ssim.fast".into(), "temporal.freeze".into()],
            memory_budget: 64 << 20,
            frames_a: Some(20),
        };
        assert_eq!(range_count(&req), Some(4));
        let job = submit_video_range(&analyzer, req.clone());
        let info = job.info.clone();
        let mut r = job.handle.wait().unwrap();
        assert!(info.error.lock().is_none());
        assert_eq!(r.frames.iter().map(|f| f.idx).collect::<Vec<_>>(), vec![1, 5, 9, 13]);
        // Outside the noisy block A and B are identical; the ROI was snapped to even coordinates.
        assert!(r.series("psnr.y").iter().all(|(_, v)| *v == Some(lumen_analysis::PSNR_CAP_DB)));
        assert_eq!(r.scope.width, 32);
        assert_eq!(*info.cut.lock(), Some((Roi::new(20, 20, 32, 20), (64, 48))));
        info.fix_scope(&mut r);
        assert_eq!((r.scope.width, r.scope.roi), (64, Some(Roi::new(20, 20, 32, 20))));
        // The whole frame sees the noise.
        let whole = submit_video_range(&analyzer, VideoRangeRequest { roi: None, ..req }).handle.wait().unwrap();
        assert!(whole.series("psnr.y").iter().all(|(_, v)| v.is_some_and(|v| v < 60.0)));
    }
}
