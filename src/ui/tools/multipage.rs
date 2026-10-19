//! Output engines for the "Create multi-page file", "Image strip" and
//! "Create ZIP" tools. No UI code here; every engine reports progress through
//! `OutProgress`, honours its cancel flag and writes to a temporary file that
//! is renamed into place only on success.

use super::pdf::{self, PdfWriter};
use crate::decode::{self, DecodeOptions};
use crate::formats::SaveFormat;
use image::{Rgba, RgbaImage};
use parking_lot::Mutex;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Progress shared between a job and its window.
#[derive(Default)]
pub struct OutProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    pub phase: Mutex<String>,
}

impl OutProgress {
    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 {
            0.0
        } else {
            (self.done.load(Ordering::Relaxed) as f64 / t as f64) as f32
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn set(&self, phase: &str, total: u64) {
        *self.phase.lock() = phase.to_string();
        self.done.store(0, Ordering::SeqCst);
        self.total.store(total, Ordering::SeqCst);
    }
    fn step(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
}

pub const CANCELLED: &str = "Cancelled";

/// A temporary output file next to the target, removed unless committed.
struct TempOut {
    tmp: PathBuf,
    target: PathBuf,
    committed: bool,
}

impl TempOut {
    fn new(target: &Path) -> Result<(TempOut, std::fs::File), String> {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let tmp = target.with_file_name(format!(".{name}.lumen-tmp"));
        let f = std::fs::File::create(&tmp).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
        Ok((TempOut { tmp, target: target.to_path_buf(), committed: false }, f))
    }
    fn commit(mut self) -> Result<(), String> {
        // The window confirmed any overwrite before starting the job. A rename
        // replaces an existing target in one step on Windows, so a failure
        // leaves the old file untouched; the finished output is then kept
        // under its temporary name instead of being thrown away.
        match std::fs::rename(&self.tmp, &self.target) {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(e) => {
                self.committed = true;
                Err(format!("Cannot create {}: {e}. The new file was kept as {}", self.target.display(), self.tmp.display()))
            }
        }
    }
}

/// Flush a buffered output file and make sure it reached the disk, so a full
/// disk is reported instead of leaving a truncated file behind.
fn finish_file(w: BufWriter<std::fs::File>) -> Result<(), String> {
    let f = w.into_inner().map_err(|e| format!("Cannot write the file: {}", e.error()))?;
    f.sync_all().map_err(|e| format!("Cannot write the file: {e}"))
}

/// A writer that remembers the first I/O error, for encoders that finish
/// their output in `Drop` (where errors are otherwise lost).
struct Tracked<W> {
    inner: W,
    err: Option<std::io::Error>,
}

impl<W> Tracked<W> {
    fn new(inner: W) -> Self {
        Tracked { inner, err: None }
    }
    fn note<T>(&mut self, r: std::io::Result<T>) -> std::io::Result<T> {
        if let Err(e) = &r {
            if e.kind() != std::io::ErrorKind::Interrupted && self.err.is_none() {
                self.err = Some(std::io::Error::new(e.kind(), e.to_string()));
            }
        }
        r
    }
    fn into_result(self) -> Result<W, String> {
        match self.err {
            Some(e) => Err(format!("Cannot write the file: {e}")),
            None => Ok(self.inner),
        }
    }
}

impl<W: Write> Write for Tracked<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let r = self.inner.write(buf);
        self.note(r)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let r = self.inner.flush();
        self.note(r)
    }
}

impl<W: Seek> Seek for Tracked<W> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let r = self.inner.seek(pos);
        self.note(r)
    }
}

impl Drop for TempOut {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

fn load(path: &Path) -> Result<RgbaImage, String> {
    let name = crate::ui::batch::file_name(path);
    // A decoder panic on one file becomes an error naming that file.
    let d = std::panic::catch_unwind(|| decode::decode_file(path, DecodeOptions { color_manage: true, animations: false }))
        .map_err(|e| format!("{name}: unexpected error ({})", crate::ui::batch::convert::panic_message(&*e)))?
        .map_err(|e| format!("{name}: {e:#}"))?;
    Ok(std::sync::Arc::try_unwrap(d.image).unwrap_or_else(|a| (*a).clone()))
}

fn has_alpha(img: &RgbaImage) -> bool {
    img.as_raw().iter().skip(3).step_by(4).any(|&a| a != 255)
}

/// Flatten onto `bg` and fit inside a `w` x `h` canvas (letterboxed, centered).
pub fn letterbox(img: &RgbaImage, w: u32, h: u32, bg: [u8; 3]) -> RgbaImage {
    let s = (w as f64 / img.width().max(1) as f64).min(h as f64 / img.height().max(1) as f64);
    let nw = ((img.width() as f64 * s).round() as u32).clamp(1, w);
    let nh = ((img.height() as f64 * s).round() as u32).clamp(1, h);
    let scaled = if (nw, nh) == img.dimensions() { img.clone() } else { crate::imaging::resize_exact(img, nw, nh, crate::imaging::Filter::Lanczos3) };
    let mut out = RgbaImage::from_pixel(w, h, Rgba([bg[0], bg[1], bg[2], 255]));
    crate::edit::geom::composite_over(&mut out, &scaled, ((w - nw) / 2) as i64, ((h - nh) / 2) as i64, 1.0);
    out
}

// ================================================================ multi-page

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MultiFormat {
    Pdf,
    Tiff,
    Gif,
    WebP,
}

impl MultiFormat {
    pub const ALL: [MultiFormat; 4] = [MultiFormat::Pdf, MultiFormat::Tiff, MultiFormat::Gif, MultiFormat::WebP];
    pub fn label(self) -> &'static str {
        match self {
            MultiFormat::Pdf => "PDF document",
            MultiFormat::Tiff => "Multi-page TIFF",
            MultiFormat::Gif => "Animated GIF",
            MultiFormat::WebP => "Animated WebP",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            MultiFormat::Pdf => "pdf",
            MultiFormat::Tiff => "tif",
            MultiFormat::Gif => "gif",
            MultiFormat::WebP => "webp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfPage {
    /// Each page is the size of its image (at `dpi`).
    Image,
    A4,
    Letter,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MultiSettings {
    pub format: MultiFormat,
    pub pdf_page: PdfPage,
    /// Margin for A4 / Letter pages, mm.
    pub margin_mm: f32,
    pub jpeg_quality: u8,
    /// Resolution for image-sized PDF pages and the TIFF resolution tag.
    pub dpi: u32,
    /// Longest side of PDF page images (0 = keep the original size).
    pub pdf_max_side: u32,
    /// Animation: time per frame.
    pub frame_ms: u32,
    /// Animation: longest side of the frames.
    pub anim_max_side: u32,
    pub webp_quality: f32,
    pub bg: [u8; 3],
}

impl Default for MultiSettings {
    fn default() -> Self {
        MultiSettings {
            format: MultiFormat::Pdf,
            pdf_page: PdfPage::A4,
            margin_mm: 10.0,
            jpeg_quality: 88,
            dpi: 150,
            pdf_max_side: 3000,
            frame_ms: 800,
            anim_max_side: 800,
            webp_quality: 85.0,
            bg: [255, 255, 255],
        }
    }
}

/// Page size in points for an image, with auto orientation for paper sizes.
pub fn pdf_page_size(s: &MultiSettings, w: u32, h: u32) -> (f32, f32) {
    let paper = match s.pdf_page {
        PdfPage::Image => {
            let dpi = s.dpi.max(1) as f32;
            return (w as f32 * 72.0 / dpi, h as f32 * 72.0 / dpi);
        }
        PdfPage::A4 => pdf::A4_PT,
        PdfPage::Letter => pdf::LETTER_PT,
    };
    if w > h {
        (paper.1, paper.0)
    } else {
        paper
    }
}

fn write_pdf(files: &[PathBuf], s: &MultiSettings, f: std::fs::File, prog: &OutProgress, title: &str) -> Result<(), String> {
    let mut w = PdfWriter::new(BufWriter::new(f)).map_err(|e| e.to_string())?;
    for p in files {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let mut img = load(p)?;
        if s.pdf_max_side > 0 {
            img = crate::imaging::resize_to_fit(&img, s.pdf_max_side, s.pdf_max_side);
        }
        let (iw, ih) = img.dimensions();
        if iw > 65535 || ih > 65535 {
            return Err(format!("{} is too large for a PDF page", crate::ui::batch::file_name(p)));
        }
        let rgb = crate::encode::flatten_rgb(&img, s.bg);
        drop(img);
        let mut jpeg = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, s.jpeg_quality.clamp(1, 100));
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
        enc.encode(&rgb, iw as u16, ih as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())?;
        let (pw, ph) = pdf_page_size(s, iw, ih);
        let margin = if s.pdf_page == PdfPage::Image { 0.0 } else { s.margin_mm.max(0.0) / 25.4 * 72.0 };
        let rect = pdf::fit_rect(pw, ph, margin, iw, ih);
        w.add_jpeg_page(&jpeg, iw, ih, false, pw, ph, rect).map_err(|e| e.to_string())?;
        prog.step(1);
    }
    finish_file(w.finish(title).map_err(|e| e.to_string())?)
}

fn write_tiff(files: &[PathBuf], s: &MultiSettings, f: std::fs::File, prog: &OutProgress) -> Result<(), String> {
    use tiff::encoder::{colortype, Compression, Rational, TiffEncoder};
    use tiff::tags::ResolutionUnit;
    let err = |e: tiff::TiffError| e.to_string();
    let mut out = Tracked::new(BufWriter::new(f));
    let mut enc = TiffEncoder::new(&mut out).map_err(err)?.with_compression(Compression::Lzw);
    let res = Rational { n: s.dpi.max(1), d: 1 };
    for p in files {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let img = load(p)?;
        let (w, h) = img.dimensions();
        if has_alpha(&img) {
            let mut ie = enc.new_image::<colortype::RGBA8>(w, h).map_err(err)?;
            ie.resolution(ResolutionUnit::Inch, res.clone());
            ie.write_data(img.as_raw()).map_err(err)?;
        } else {
            let rgb = crate::encode::flatten_rgb(&img, [0, 0, 0]);
            drop(img);
            let mut ie = enc.new_image::<colortype::RGB8>(w, h).map_err(err)?;
            ie.resolution(ResolutionUnit::Inch, res.clone());
            ie.write_data(&rgb).map_err(err)?;
        }
        prog.step(1);
    }
    finish_file(out.into_result()?)
}

/// Canvas size for an animation: the first frame fitted inside `max_side`.
pub fn anim_canvas(first: (u32, u32), max_side: u32) -> (u32, u32) {
    let (w, h) = (first.0.max(1) as f64, first.1.max(1) as f64);
    let m = max_side.max(16) as f64;
    let s = (m / w.max(h)).min(1.0);
    (((w * s).round() as u32).max(1), ((h * s).round() as u32).max(1))
}

fn write_gif(files: &[PathBuf], s: &MultiSettings, f: std::fs::File, prog: &OutProgress) -> Result<(), String> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let mut canvas = None;
    let mut out = Tracked::new(BufWriter::new(f));
    let mut enc = GifEncoder::new_with_speed(&mut out, 10);
    enc.set_repeat(Repeat::Infinite).map_err(|e| e.to_string())?;
    for p in files {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let img = load(p)?;
        let (cw, ch) = *canvas.get_or_insert_with(|| anim_canvas(img.dimensions(), s.anim_max_side.min(4000)));
        let frame = letterbox(&img, cw, ch, s.bg);
        drop(img);
        let delay = image::Delay::from_numer_denom_ms(s.frame_ms.max(20), 1);
        enc.encode_frame(image::Frame::from_parts(frame, 0, 0, delay)).map_err(|e| e.to_string())?;
        prog.step(1);
    }
    // The GIF trailer is written when the encoder is dropped; `Tracked`
    // catches a failure there.
    drop(enc);
    finish_file(out.into_result()?)
}

/// Animated WebP keeps every frame in memory until it is encoded.
pub const MAX_WEBP_ANIM_PIXELS: u64 = 250_000_000;

/// Why an animated WebP of `frames` frames on a `w` x `h` canvas is refused.
pub fn webp_anim_limit_error(frames: usize, w: u32, h: u32) -> Option<String> {
    let total = frames as u64 * w as u64 * h as u64;
    (total > MAX_WEBP_ANIM_PIXELS).then(|| {
        format!(
            "An animated WebP of {frames} frames at {w} x {h} px needs too much memory ({} MP in total, the limit is {} MP). Use fewer images, a smaller frame size or an animated GIF.",
            total / 1_000_000,
            MAX_WEBP_ANIM_PIXELS / 1_000_000
        )
    })
}

fn write_webp(files: &[PathBuf], s: &MultiSettings, mut f: std::fs::File, prog: &OutProgress) -> Result<(), String> {
    let mut frames: Vec<RgbaImage> = Vec::new();
    let mut canvas = None;
    for p in files {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let img = load(p)?;
        let (cw, ch) = *canvas.get_or_insert_with(|| anim_canvas(img.dimensions(), s.anim_max_side.min(4000)));
        if frames.is_empty() {
            if let Some(e) = webp_anim_limit_error(files.len(), cw, ch) {
                return Err(e);
            }
        }
        frames.push(letterbox(&img, cw, ch, s.bg));
        prog.step(1);
    }
    let Some((cw, ch)) = canvas else { return Err("No images".into()) };
    *prog.phase.lock() = "Encoding animation".into();
    let mut config = libwebp_sys::WebPConfig::new().map_err(|_| "WebP encoder setup failed".to_string())?;
    config.quality = s.webp_quality.clamp(1.0, 100.0);
    let mut enc = webp::AnimEncoder::new(cw, ch, &config);
    enc.set_loop_count(0);
    enc.set_bgcolor([s.bg[0], s.bg[1], s.bg[2], 255]);
    let step = s.frame_ms.max(20) as i32;
    for (i, fr) in frames.iter().enumerate() {
        enc.add_frame(webp::AnimFrame::from_rgba(fr.as_raw(), cw, ch, i as i32 * step));
    }
    let mem = enc.try_encode().map_err(|e| format!("WebP encoding failed: {e:?}"))?;
    drop(enc);
    drop(frames);
    f.write_all(&mem).map_err(|e| format!("Cannot write the file: {e}"))?;
    f.sync_all().map_err(|e| format!("Cannot write the file: {e}"))
}

/// Combine `files` into one multi-page / animated file at `out`.
pub fn write_multi(files: &[PathBuf], s: &MultiSettings, out: &Path, prog: &OutProgress) -> Result<(), String> {
    if files.is_empty() {
        return Err("No images".into());
    }
    prog.set("Adding pages", files.len() as u64);
    let (tmp, f) = TempOut::new(out)?;
    let title = out.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match s.format {
        MultiFormat::Pdf => write_pdf(files, s, f, prog, &title)?,
        MultiFormat::Tiff => write_tiff(files, s, f, prog)?,
        MultiFormat::Gif => write_gif(files, s, f, prog)?,
        MultiFormat::WebP => write_webp(files, s, f, prog)?,
    }
    if prog.cancelled() {
        return Err(CANCELLED.into());
    }
    tmp.commit()
}

// ===================================================================== strip

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StripDir {
    Horizontal,
    Vertical,
}

/// How images are sized across the strip (height for horizontal strips,
/// width for vertical ones).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StripSize {
    /// Keep every image's own size.
    Original,
    /// Scale all to the smallest one.
    Smallest,
    /// Scale all to the largest one.
    Largest,
    /// Scale all to `custom` px.
    Custom,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StripSettings {
    pub dir: StripDir,
    pub size: StripSize,
    pub custom: u32,
    pub spacing: u32,
    pub margin: u32,
    pub bg: [u8; 4],
    pub format: SaveFormat,
    pub jpeg_quality: u8,
    pub out_dir: Option<PathBuf>,
    pub name: String,
}

impl Default for StripSettings {
    fn default() -> Self {
        StripSettings {
            dir: StripDir::Horizontal,
            size: StripSize::Smallest,
            custom: 1080,
            spacing: 10,
            margin: 0,
            bg: [255, 255, 255, 255],
            format: SaveFormat::Jpeg,
            jpeg_quality: 92,
            out_dir: None,
            name: "Strip".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StripLayout {
    pub width: u32,
    pub height: u32,
    /// (x, y, w, h) for every image.
    pub cells: Vec<(u32, u32, u32, u32)>,
}

/// Largest strip we will allocate (RGBA, 4 bytes per pixel).
pub const MAX_STRIP_PIXELS: u64 = 300_000_000;

/// Lay out images of the given sizes in a row or column.
pub fn strip_layout(dims: &[(u32, u32)], s: &StripSettings) -> StripLayout {
    let horiz = s.dir == StripDir::Horizontal;
    // Size across the strip (height in a row, width in a column) and along it.
    let across = |d: (u32, u32)| if horiz { d.1 } else { d.0 };
    let along = |d: (u32, u32)| if horiz { d.0 } else { d.1 };
    let dims: Vec<(u32, u32)> = dims.iter().map(|&(w, h)| (w.max(1), h.max(1))).collect();
    let target = match s.size {
        StripSize::Original => None,
        StripSize::Smallest => dims.iter().map(|&d| across(d)).min(),
        StripSize::Largest => dims.iter().map(|&d| across(d)).max(),
        StripSize::Custom => Some(s.custom.max(1)),
    };
    let sized: Vec<(u32, u32)> = dims
        .iter()
        .map(|&d| match target {
            None => (along(d), across(d)),
            Some(t) => ((((along(d) as f64) * t as f64 / across(d) as f64).round() as u32).max(1), t),
        })
        .collect();
    let thick = sized.iter().map(|d| d.1).max().unwrap_or(1);
    let mut pos = s.margin;
    let mut cells = Vec::with_capacity(sized.len());
    for (i, &(a, c)) in sized.iter().enumerate() {
        if i > 0 {
            pos += s.spacing;
        }
        let off = s.margin + (thick - c) / 2;
        cells.push(if horiz { (pos, off, a, c) } else { (off, pos, c, a) });
        pos += a;
    }
    let length = pos + s.margin;
    let breadth = thick + 2 * s.margin;
    let (width, height) = if horiz { (length, breadth) } else { (breadth, length) };
    StripLayout { width: width.max(1), height: height.max(1), cells }
}

/// Why this strip cannot be written, if it can't.
pub fn strip_limit_error(l: &StripLayout, format: SaveFormat) -> Option<String> {
    let px = l.width as u64 * l.height as u64;
    if px > MAX_STRIP_PIXELS {
        return Some(format!("The strip would be {} x {} px ({} MP); the limit is {} MP. Use a smaller size.", l.width, l.height, px / 1_000_000, MAX_STRIP_PIXELS / 1_000_000));
    }
    let max = match format {
        SaveFormat::Jpeg => 65_535,
        SaveFormat::WebP => 16_383,
        _ => u32::MAX,
    };
    if l.width > max || l.height > max {
        return Some(format!("This format allows at most {max} px per side; the strip is {} x {} px.", l.width, l.height));
    }
    None
}

/// Draw a strip. `get(i)` returns the i-th image (any size; it is scaled to its cell).
pub fn render_strip(l: &StripLayout, bg: [u8; 4], prog: &OutProgress, mut get: impl FnMut(usize) -> Result<RgbaImage, String>) -> Result<RgbaImage, String> {
    let mut out = RgbaImage::from_pixel(l.width, l.height, Rgba(bg));
    for (i, &(x, y, w, h)) in l.cells.iter().enumerate() {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let img = get(i)?;
        let scaled = if img.dimensions() == (w, h) { img } else { crate::imaging::resize_exact(&img, w, h, crate::imaging::Filter::Lanczos3) };
        crate::edit::geom::composite_over(&mut out, &scaled, x as i64, y as i64, 1.0);
        prog.step(1);
    }
    Ok(out)
}

/// Build the strip from files and save it to `out`.
pub fn write_strip(files: &[PathBuf], dims: &[(u32, u32)], s: &StripSettings, out: &Path, prog: &OutProgress) -> Result<(), String> {
    let l = strip_layout(dims, s);
    if let Some(e) = strip_limit_error(&l, s.format) {
        return Err(e);
    }
    prog.set("Joining images", files.len() as u64 + 1);
    let img = render_strip(&l, s.bg, prog, |i| load(&files[i]))?;
    *prog.phase.lock() = "Saving".into();
    let opts = crate::encode::SaveOptions { format: s.format, jpeg_quality: s.jpeg_quality, keep_metadata: false, ..Default::default() };
    let bytes = crate::encode::encode(&img, &opts).map_err(|e| e.to_string())?;
    let (tmp, mut f) = TempOut::new(out)?;
    f.write_all(&bytes).map_err(|e| format!("Cannot write the file: {e}"))?;
    f.sync_all().map_err(|e| format!("Cannot write the file: {e}"))?;
    drop(f);
    tmp.commit()?;
    prog.step(1);
    Ok(())
}

// ======================================================================= zip

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ZipSettings {
    /// Store already-compressed formats (JPEG, PNG, WebP, video...) without
    /// recompressing: much faster, nearly the same size.
    pub store_compressed: bool,
    /// Keep the folder structure relative to the common parent folder.
    pub keep_folders: bool,
}

impl Default for ZipSettings {
    fn default() -> Self {
        ZipSettings { store_compressed: true, keep_folders: false }
    }
}

fn already_compressed(p: &Path) -> bool {
    let e = crate::formats::ext_of(p);
    matches!(
        e.as_str(),
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "webp" | "gif" | "heic" | "heif" | "avif" | "jxl" | "zip" | "7z" | "rar" | "mp4" | "mov" | "mkv" | "webm" | "avi" | "mp3" | "m4a" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "raf" | "orf" | "rw2"
    )
}

/// The deepest folder containing every file.
pub fn common_parent(files: &[PathBuf]) -> PathBuf {
    let mut it = files.iter().filter_map(|f| f.parent());
    let Some(first) = it.next() else { return PathBuf::new() };
    let mut base: Vec<_> = first.components().collect();
    for p in it {
        let c: Vec<_> = p.components().collect();
        let n = base.iter().zip(&c).take_while(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str())).count();
        base.truncate(n);
    }
    base.iter().collect()
}

/// A safe archive entry name for `f` relative to `base` (a folder `f` is
/// in, compared case-insensitively): only plain folder and file names, joined
/// with '/'. Drive letters, UNC prefixes, root and '.'/'..' components never
/// reach the archive, so extracting it cannot write outside its folder.
pub fn entry_name(f: &Path, base: &Path) -> String {
    use std::path::Component;
    let skip = base.components().count();
    let parts: Vec<String> = f
        .components()
        .skip(skip)
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_string()),
            _ => None,
        })
        .flat_map(|n| n.split(['/', '\\']).map(str::to_string).collect::<Vec<_>>())
        .map(|n| n.trim_end_matches([' ', '.']).to_string())
        .filter(|n| !n.is_empty() && n != "." && n != "..")
        .collect();
    if parts.is_empty() {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let name = name.trim_end_matches([' ', '.']);
        return if name.is_empty() || name == ".." { "file".into() } else { name.to_string() };
    }
    parts.join("/")
}

/// Archive entry names (forward slashes, unique, case-insensitive).
pub fn entry_names(files: &[PathBuf], keep_folders: bool) -> Vec<String> {
    let base = common_parent(files);
    let mut seen = std::collections::HashSet::new();
    files
        .iter()
        .map(|f| {
            let name = if keep_folders { entry_name(f, &base) } else { entry_name(Path::new(f.file_name().unwrap_or_default()), Path::new("")) };
            let (stem, ext) = match name.rfind('.') {
                Some(i) if i > name.rfind('/').map(|j| j + 1).unwrap_or(0) => (name[..i].to_string(), name[i..].to_string()),
                _ => (name.clone(), String::new()),
            };
            let mut candidate = name.clone();
            let mut k = 2;
            while !seen.insert(candidate.to_lowercase()) {
                candidate = format!("{stem} ({k}){ext}");
                k += 1;
            }
            candidate
        })
        .collect()
}

/// Create a ZIP archive of `files` at `out`. Returns the archive size.
pub fn write_zip(files: &[PathBuf], s: &ZipSettings, out: &Path, prog: &OutProgress) -> Result<u64, String> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;
    let sizes: Vec<u64> = files.iter().map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)).collect();
    prog.set("Compressing", sizes.iter().sum::<u64>().max(1));
    let names = entry_names(files, s.keep_folders);
    let (tmp, f) = TempOut::new(out)?;
    let mut zw = zip::ZipWriter::new(BufWriter::new(f));
    let mut buf = vec![0u8; 1 << 20];
    for (i, file) in files.iter().enumerate() {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let method = if s.store_compressed && already_compressed(file) { CompressionMethod::Stored } else { CompressionMethod::Deflated };
        let mut opts = SimpleFileOptions::default().compression_method(method).large_file(sizes[i] >= 0xFFFF_FFFF);
        if let Some(t) = std::fs::metadata(file).ok().and_then(|m| m.modified().ok()) {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            use chrono::{Datelike, Timelike};
            if let Ok(zt) = zip::DateTime::from_date_and_time(dt.year().clamp(1980, 2107) as u16, dt.month() as u8, dt.day() as u8, dt.hour() as u8, dt.minute() as u8, dt.second().min(59) as u8) {
                opts = opts.last_modified_time(zt);
            }
        }
        zw.start_file(names[i].as_str(), opts).map_err(|e| e.to_string())?;
        let mut src = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
        loop {
            if prog.cancelled() {
                return Err(CANCELLED.into());
            }
            let n = src.read(&mut buf).map_err(|e| format!("{}: {e}", file.display()))?;
            if n == 0 {
                break;
            }
            zw.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            prog.step(n as u64);
        }
    }
    finish_file(zw.finish().map_err(|e| e.to_string())?)?;
    tmp.commit()?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lumen-out-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_png(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let p = dir.join(name);
        RgbaImage::from_fn(w, h, |x, y| Rgba([(x * 5) as u8, (y * 9) as u8, 128, 255])).save(&p).unwrap();
        p
    }

    #[test]
    fn strip_geometry() {
        let s = StripSettings { spacing: 10, margin: 5, size: StripSize::Smallest, ..Default::default() };
        let l = strip_layout(&[(400, 200), (100, 100), (300, 600)], &s);
        // All scaled to height 100: 200, 100, 50 wide.
        assert_eq!(l.cells, vec![(5, 5, 200, 100), (215, 5, 100, 100), (325, 5, 50, 100)]);
        assert_eq!((l.width, l.height), (380, 110));
        let v = StripSettings { dir: StripDir::Vertical, size: StripSize::Custom, custom: 50, spacing: 0, margin: 0, ..Default::default() };
        let l = strip_layout(&[(100, 50), (50, 100)], &v);
        assert_eq!(l.cells, vec![(0, 0, 50, 25), (0, 25, 50, 100)]);
        assert_eq!((l.width, l.height), (50, 125));
        // Original sizes are centered across the strip.
        let o = StripSettings { size: StripSize::Original, spacing: 0, margin: 0, ..Default::default() };
        let l = strip_layout(&[(10, 40), (10, 20)], &o);
        assert_eq!(l.cells, vec![(0, 0, 10, 40), (10, 10, 10, 20)]);
        let huge = strip_layout(&[(60000, 60000), (60000, 60000)], &StripSettings { size: StripSize::Largest, ..Default::default() });
        assert!(strip_limit_error(&huge, SaveFormat::Png).is_some());
    }

    #[test]
    fn strip_and_animation_render() {
        let dir = tmpdir("strip");
        let a = write_png(&dir, "a.png", 40, 20);
        let b = write_png(&dir, "b.png", 20, 20);
        let s = StripSettings { spacing: 4, ..Default::default() };
        let out = dir.join("strip.png");
        let s = StripSettings { format: SaveFormat::Png, ..s };
        write_strip(&[a.clone(), b.clone()], &[(40, 20), (20, 20)], &s, &out, &OutProgress::default()).unwrap();
        {
            let back = image::open(&out).unwrap();
            assert_eq!((back.width(), back.height()), (64, 20));
        }
        for fmt in [MultiFormat::Gif, MultiFormat::WebP, MultiFormat::Tiff, MultiFormat::Pdf] {
            let out = dir.join(format!("multi.{}", fmt.ext()));
            let ms = MultiSettings { format: fmt, anim_max_side: 32, ..Default::default() };
            write_multi(&[a.clone(), b.clone()], &ms, &out, &OutProgress::default()).unwrap();
            let bytes = std::fs::read(&out).unwrap();
            assert!(bytes.len() > 50, "{fmt:?}");
            match fmt {
                MultiFormat::Gif => {
                    use image::AnimationDecoder;
                    let frames = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes)).unwrap().into_frames().collect_frames().unwrap();
                    assert_eq!(frames.len(), 2);
                    assert_eq!(frames[0].buffer().dimensions(), (32, 16));
                }
                MultiFormat::Tiff => {
                    let mut d = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
                    assert_eq!(d.dimensions().unwrap(), (40, 20));
                    assert!(d.more_images());
                    d.next_image().unwrap();
                    assert_eq!(d.dimensions().unwrap(), (20, 20));
                }
                MultiFormat::Pdf => assert!(bytes.starts_with(b"%PDF-1.4")),
                MultiFormat::WebP => assert_eq!(&bytes[8..12], b"WEBP"),
            }
        }
        // Nothing left behind by the temp-file dance.
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".lumen-tmp")));
        // A cancelled job leaves no output.
        let prog = OutProgress::default();
        prog.cancel.store(true, Ordering::SeqCst);
        let out = dir.join("cancelled.pdf");
        assert!(write_multi(&[a, b], &MultiSettings::default(), &out, &prog).is_err());
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_entry_names_are_sanitized() {
        // Files on different drives share no folder: no drive letters or roots.
        let files = vec![PathBuf::from(r"C:\Photos\2024\a.jpg"), PathBuf::from(r"D:\Backup\b.jpg")];
        assert_eq!(entry_names(&files, true), vec!["Photos/2024/a.jpg", "Backup/b.jpg"]);
        // UNC paths and verbatim prefixes are stripped too.
        let files = vec![PathBuf::from(r"\\server\share\pics\a.jpg"), PathBuf::from(r"\\?\C:\other\b.jpg")];
        assert_eq!(entry_names(&files, true), vec!["pics/a.jpg", "other/b.jpg"]);
        // '..' and '.' components never reach the archive.
        assert_eq!(entry_name(Path::new(r"C:\a\..\..\evil\x.png"), Path::new("")), "a/evil/x.png");
        assert_eq!(entry_name(Path::new(r"C:\a\.\b.png"), Path::new(r"C:\")), "a/b.png");
        assert_eq!(entry_name(Path::new(r"C:\"), Path::new("")), "file");
        for n in entry_names(&[PathBuf::from(r"C:\x\..\y.png"), PathBuf::from(r"\\?\UNC\srv\sh\z.png")], true) {
            assert!(!n.contains(':') && !n.starts_with('/') && !n.contains('\\') && !n.split('/').any(|c| c == ".." || c == "."), "{n}");
        }
        // Without folders only the file name is used.
        assert_eq!(entry_names(&[PathBuf::from(r"C:\a\p.png"), PathBuf::from(r"D:\b\p.png")], false), vec!["p.png", "p (2).png"]);
    }

    #[test]
    fn webp_animation_memory_cap() {
        assert!(webp_anim_limit_error(10, 1000, 1000).is_none());
        assert!(webp_anim_limit_error(500, 4000, 3000).is_some());
    }

    #[test]
    fn tracked_writer_reports_errors() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut t = Tracked::new(Full);
        let _ = t.write(b"x");
        assert!(t.into_result().is_err());
        let mut ok = Tracked::new(Vec::new());
        ok.write_all(b"abc").unwrap();
        assert_eq!(ok.into_result().unwrap(), b"abc");
    }

    #[test]
    fn zip_roundtrip_and_names() {
        let dir = tmpdir("zip");
        std::fs::create_dir_all(dir.join("x")).unwrap();
        let a = write_png(&dir, "a.png", 8, 8);
        let b = dir.join("x").join("a.png");
        std::fs::copy(&a, &b).unwrap();
        let c = dir.join("notes.txt");
        std::fs::write(&c, "hello hello hello hello".repeat(100)).unwrap();
        let files = vec![a.clone(), b.clone(), c.clone()];
        assert_eq!(entry_names(&files, false), vec!["a.png", "a (2).png", "notes.txt"]);
        assert_eq!(entry_names(&files, true), vec!["a.png", "x/a.png", "notes.txt"]);
        // Case differences in the shared folder still give relative names.
        let upper = PathBuf::from(dir.join("x").join("a.png").to_string_lossy().to_uppercase());
        assert_eq!(entry_names(&[a.clone(), upper], true), vec!["a.png", "X/A.PNG"]);
        let out = dir.join("out.zip");
        let prog = OutProgress::default();
        let size = write_zip(&files, &ZipSettings::default(), &out, &prog).unwrap();
        assert!(size > 0);
        assert_eq!(prog.fraction(), 1.0);
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(z.len(), 3);
        let mut txt = String::new();
        z.by_name("notes.txt").unwrap().read_to_string(&mut txt).unwrap();
        assert_eq!(txt.len(), 2300);
        assert_eq!(z.by_name("notes.txt").unwrap().compression(), zip::CompressionMethod::Deflated);
        assert_eq!(z.by_name("a.png").unwrap().compression(), zip::CompressionMethod::Stored);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
