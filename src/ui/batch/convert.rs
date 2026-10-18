//! Batch conversion engine: settings, per-file processing and the background
//! worker pool. No UI code lives here.

use super::logic::{self, ResizeSpec, TemplateCtx};
use super::pipeline::{Pipeline, StepCtx};
use crate::decode::{self, DecodeOptions};
use crate::encode::{Chroma, SaveOptions};
use crate::formats::SaveFormat;
use crate::imaging::Filter;
use crossbeam_channel::{Receiver, Sender};
use image::RgbaImage;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DestMode {
    /// Next to the original.
    Same,
    /// A subfolder (by name) next to the original.
    Subfolder,
    /// A chosen folder.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConflictPolicy {
    Rename,
    Overwrite,
    Skip,
}

impl ConflictPolicy {
    pub const ALL: [ConflictPolicy; 3] = [ConflictPolicy::Rename, ConflictPolicy::Skip, ConflictPolicy::Overwrite];
    pub fn label(self) -> &'static str {
        match self {
            ConflictPolicy::Rename => "Add a number, e.g. photo (2).jpg",
            ConflictPolicy::Skip => "Skip the file",
            ConflictPolicy::Overwrite => "Overwrite existing file",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConvertSettings {
    /// Keep each file's own format (when Lumen can write it).
    pub keep_format: bool,
    pub format: SaveFormat,
    pub jpeg_quality: u8,
    pub jpeg_chroma: Chroma,
    pub jpeg_progressive: bool,
    pub webp_quality: f32,
    pub webp_lossless: bool,
    pub png_best: bool,
    pub resize: ResizeSpec,
    pub filter: Filter,
    pub grayscale: bool,
    pub auto_enhance: bool,
    pub sharpen: bool,
    pub dest: DestMode,
    pub subfolder: String,
    pub out_folder: Option<PathBuf>,
    pub template: String,
    pub start: i64,
    pub conflict: ConflictPolicy,
    pub keep_exif: bool,
    pub preserve_date: bool,
    pub include_subfolders: bool,
    /// Run the advanced pipeline after the basic options.
    pub use_advanced: bool,
    pub advanced: Pipeline,
}

impl Default for ConvertSettings {
    fn default() -> Self {
        ConvertSettings {
            keep_format: false,
            format: SaveFormat::Jpeg,
            jpeg_quality: 90,
            jpeg_chroma: Chroma::S420,
            jpeg_progressive: false,
            webp_quality: 85.0,
            webp_lossless: false,
            png_best: false,
            resize: ResizeSpec::default(),
            filter: Filter::Lanczos3,
            grayscale: false,
            auto_enhance: false,
            sharpen: true,
            dest: DestMode::Subfolder,
            subfolder: "Converted".into(),
            out_folder: None,
            template: "{name}".into(),
            start: 1,
            conflict: ConflictPolicy::Rename,
            keep_exif: true,
            preserve_date: false,
            include_subfolders: false,
            use_advanced: false,
            advanced: Pipeline::default(),
        }
    }
}

impl ConvertSettings {
    pub fn target_format(&self, input: &Path) -> SaveFormat {
        if self.keep_format {
            SaveFormat::from_ext(&crate::formats::ext_of(input)).unwrap_or(SaveFormat::Jpeg)
        } else {
            self.format
        }
    }

    pub fn save_options(&self, format: SaveFormat, exif: Option<Vec<u8>>) -> SaveOptions {
        SaveOptions {
            format,
            jpeg_quality: self.jpeg_quality,
            jpeg_progressive: self.jpeg_progressive,
            jpeg_chroma: self.jpeg_chroma,
            webp_quality: self.webp_quality,
            webp_lossless: self.webp_lossless,
            png_best_compression: self.png_best,
            keep_metadata: self.keep_exif,
            exif: if self.keep_exif { exif } else { None },
            icc: None,
        }
    }

    pub fn output_dir(&self, input: &Path) -> PathBuf {
        let parent = input.parent().map(Path::to_path_buf).unwrap_or_default();
        match self.dest {
            DestMode::Same => parent,
            DestMode::Subfolder => {
                let name = logic::sanitize_file_stem(self.subfolder.trim());
                parent.join(name)
            }
            DestMode::Folder => self.out_folder.clone().unwrap_or(parent),
        }
    }

    /// The file name (with extension) the `index`-th input will get, before
    /// conflict resolution.
    pub fn output_name(&self, input: &Path, index: usize, dims: (u32, u32), taken: Option<chrono::NaiveDateTime>) -> String {
        let info = logic::NameInfo::from_path(input);
        let modified = std::fs::metadata(input).ok().and_then(|m| m.modified().ok()).map(logic::systime_to_naive);
        let cx = TemplateCtx {
            name: &info.stem,
            ext: &info.ext,
            n: self.start + index as i64,
            pad: 1,
            modified,
            taken,
            width: Some(dims.0),
            height: Some(dims.1),
            folder: &info.folder,
        };
        let template = if self.template.trim().is_empty() { "{name}" } else { self.template.as_str() };
        let stem = logic::sanitize_file_stem(&logic::expand(template, &cx));
        format!("{stem}.{}", self.target_format(input).ext())
    }

    /// True when converting `files` with these settings can replace an
    /// original: the conflict policy is Overwrite and, for at least one file,
    /// the output folder is the file's own folder (whatever the destination
    /// mode says - a chosen folder or a subfolder named "." can be the source
    /// folder too).
    pub fn overwrites_originals(&self, files: &[PathBuf]) -> bool {
        if self.conflict != ConflictPolicy::Overwrite {
            return false;
        }
        let mut seen: HashSet<String> = HashSet::new();
        for f in files {
            let parent = f.parent().map(Path::to_path_buf).unwrap_or_default();
            if !seen.insert(path_key(&parent)) {
                continue;
            }
            if same_dir(&self.output_dir(f), &parent) {
                return true;
            }
        }
        false
    }
}

/// Apply adjustments and resizing, then the advanced pipeline (if enabled).
pub fn render(src: RgbaImage, s: &ConvertSettings, cx: &StepCtx) -> RgbaImage {
    let img = render_basic(src, s);
    if s.use_advanced {
        s.advanced.apply(img, cx)
    } else {
        img
    }
}

/// The DPI to write into the output, if the advanced pipeline sets one.
pub fn output_dpi(s: &ConvertSettings) -> Option<u16> {
    if s.use_advanced {
        s.advanced.dpi()
    } else {
        None
    }
}

/// Encode with the batch settings (including the DPI metadata).
pub fn encode_output(img: &RgbaImage, s: &ConvertSettings, fmt: SaveFormat, exif: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
    let bytes = crate::encode::encode(img, &s.save_options(fmt, exif)).map_err(|e| e.to_string())?;
    Ok(match output_dpi(s) {
        Some(dpi) => super::pipeline::set_dpi(bytes, fmt, dpi),
        None => bytes,
    })
}

fn render_basic(src: RgbaImage, s: &ConvertSettings) -> RgbaImage {
    let mut img = src;
    if s.auto_enhance {
        img = crate::edit::ops::auto_levels(&img);
    }
    if s.grayscale {
        grayscale_in_place(&mut img);
    }
    let (w, h) = img.dimensions();
    let (nw, nh) = logic::output_size(w, h, &s.resize);
    if (nw, nh) != (w, h) {
        img = crate::imaging::resize_exact(&img, nw, nh, s.filter);
        if s.sharpen && nw < w {
            img = image::imageops::unsharpen(&img, 0.5, 2);
        }
    }
    img
}

pub fn grayscale_in_place(img: &mut RgbaImage) {
    use rayon::prelude::*;
    img.as_mut().par_chunks_mut(4 * 1024).for_each(|chunk| {
        for p in chunk.chunks_exact_mut(4) {
            let y = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
            let y = (y + 0.5).clamp(0.0, 255.0) as u8;
            p[0] = y;
            p[1] = y;
            p[2] = y;
        }
    });
}

fn taken_from_exif(exif: Option<&Vec<u8>>) -> Option<chrono::NaiveDateTime> {
    exif.and_then(|e| crate::meta::read_exif_bytes(e)).and_then(|s| s.taken_ts).and_then(logic::ts_to_naive)
}

// ------------------------------------------------------------------- estimate

pub struct Estimate {
    pub name: String,
    pub in_dims: (u32, u32),
    pub out_dims: (u32, u32),
    pub in_bytes: u64,
    pub out_bytes: usize,
    /// Reduced copies of the source and the result for the before/after preview.
    pub before: Option<RgbaImage>,
    pub after: Option<RgbaImage>,
}

/// Longest side of the before/after preview images.
pub const PREVIEW_SIDE: u32 = 900;

/// Cache of the decoded preview file so tweaking options doesn't re-decode it.
pub type DecodeCache = Arc<Mutex<Option<(PathBuf, decode::Decoded)>>>;

/// Render the `index`-th file with the current settings and estimate the output.
pub fn estimate(input: &Path, index: usize, s: &ConvertSettings, cache: &DecodeCache) -> Result<Estimate, String> {
    let dec = {
        let cached = cache.lock().as_ref().filter(|(p, _)| p == input).map(|(_, d)| d.clone());
        match cached {
            Some(d) => d,
            None => {
                let d = decode::decode_file(input, DecodeOptions { color_manage: true, animations: false }).map_err(|e| format!("{e:#}"))?;
                *cache.lock() = Some((input.to_path_buf(), d.clone()));
                d
            }
        }
    };
    let in_dims = (dec.width(), dec.height());
    let fmt = s.target_format(input);
    let taken = taken_from_exif(dec.exif.as_ref());
    let cx = StepCtx::for_file(input, s.start + index as i64, taken);
    let img = render((*dec.image).clone(), s, &cx);
    let out_dims = img.dimensions();
    // Encode a reduced copy for very large outputs and extrapolate by pixel count.
    let px = out_dims.0 as f64 * out_dims.1 as f64;
    let out_bytes = if px > 8e6 && fmt != SaveFormat::Ico {
        let small = crate::imaging::resize_to_fit(&img, 2400, 2400);
        let f = px / (small.width() as f64 * small.height() as f64);
        (encode_output(&small, s, fmt, dec.exif.clone())?.len() as f64 * f) as usize
    } else {
        encode_output(&img, s, fmt, dec.exif.clone())?.len()
    };
    let out_dims = if fmt == SaveFormat::Ico { (out_dims.0.min(256), out_dims.1.min(256)) } else { out_dims };
    Ok(Estimate {
        name: s.output_name(input, index, out_dims, taken),
        in_dims,
        out_dims,
        in_bytes: std::fs::metadata(input).map(|m| m.len()).unwrap_or(0),
        out_bytes,
        before: Some(crate::imaging::resize_to_fit(&dec.image, PREVIEW_SIDE, PREVIEW_SIDE)),
        after: Some(crate::imaging::resize_to_fit(&img, PREVIEW_SIDE, PREVIEW_SIDE)),
    })
}

// --------------------------------------------------------------------- worker

#[derive(Clone, Debug)]
pub enum FileStatus {
    Pending,
    Working,
    Done { out: PathBuf, in_bytes: u64, out_bytes: u64, dims: (u32, u32) },
    Skipped(String),
    Failed(String),
}

pub struct Progress {
    pub total: usize,
    pub done: AtomicUsize,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
}

pub struct Job {
    pub progress: Arc<Progress>,
    pub rx: Receiver<(usize, FileStatus)>,
    pub started: std::time::Instant,
    pub elapsed: Option<std::time::Duration>,
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Whether two folder paths name the same folder (canonicalized when they exist).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => {
            let norm = |p: &Path| path_key(p).trim_end_matches(['\\', '/']).trim_end_matches("\\.").to_string();
            norm(a) == norm(b)
        }
    }
}

/// Turn a panic payload into a readable message.
pub fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into())
}

/// Resolve a conflict and reserve the final path. None = skip.
fn resolve(target: PathBuf, policy: ConflictPolicy, input: &Path, reserved: &Mutex<HashSet<String>>) -> Result<PathBuf, String> {
    let mut r = reserved.lock();
    let key = path_key(&target);
    let in_batch = r.contains(&key);
    let on_disk = target.exists();
    let is_source = path_key(input) == key;
    let final_path = if !in_batch && !on_disk {
        target
    } else {
        match policy {
            ConflictPolicy::Skip if !in_batch && is_source => return Err("Output would replace the original".into()),
            ConflictPolicy::Skip => return Err(format!("{} already exists", target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
            ConflictPolicy::Overwrite if !in_batch => target,
            _ => {
                let parent = target.parent().map(Path::to_path_buf).unwrap_or_default();
                let stem = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                let mut found = None;
                for i in 2..100_000 {
                    let p = parent.join(format!("{stem} ({i}){ext}"));
                    if !p.exists() && !r.contains(&path_key(&p)) {
                        found = Some(p);
                        break;
                    }
                }
                found.ok_or_else(|| "Could not find a free file name".to_string())?
            }
        }
    };
    r.insert(path_key(&final_path));
    Ok(final_path)
}

fn process_one(input: &Path, index: usize, s: &ConvertSettings, reserved: &Mutex<HashSet<String>>) -> FileStatus {
    let meta = match std::fs::metadata(input) {
        Ok(m) => m,
        Err(e) => return FileStatus::Failed(format!("Cannot read file: {e}")),
    };
    let dec = match decode::decode_file(input, DecodeOptions { color_manage: true, animations: false }) {
        Ok(d) => d,
        Err(e) => return FileStatus::Failed(format!("{e:#}")),
    };
    let exif = dec.exif.clone();
    let taken = taken_from_exif(exif.as_ref());
    let img = Arc::try_unwrap(dec.image).unwrap_or_else(|a| (*a).clone());
    let cx = StepCtx::for_file(input, s.start + index as i64, taken);
    let img = render(img, s, &cx);
    let fmt = s.target_format(input);
    let dims = if fmt == SaveFormat::Ico { (img.width().min(256), img.height().min(256)) } else { img.dimensions() };
    let dir = s.output_dir(input);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return FileStatus::Failed(format!("Cannot create {}: {e}", dir.display()));
    }
    let target = dir.join(s.output_name(input, index, dims, taken));
    let out = match resolve(target, s.conflict, input, reserved) {
        Ok(p) => p,
        Err(why) => return FileStatus::Skipped(why),
    };
    let bytes = match encode_output(&img, s, fmt, exif) {
        Ok(b) => b,
        Err(e) => return FileStatus::Failed(e),
    };
    // write_atomic writes a temporary file and renames it into place, so the
    // original survives any failure even when it is being replaced.
    if let Err(e) = crate::encode::write_atomic(&out, &bytes) {
        return FileStatus::Failed(e.to_string());
    }
    if s.preserve_date {
        if let Ok(t) = meta.modified() {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&out) {
                let _ = f.set_modified(t);
            }
        }
    }
    FileStatus::Done { out, in_bytes: meta.len(), out_bytes: bytes.len() as u64, dims }
}

pub fn worker_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(1).clamp(1, 8)
}

/// Start converting `files` on a background pool of `cores - 1` threads.
pub fn start(files: Vec<PathBuf>, s: ConvertSettings, ctx: egui::Context) -> Job {
    let progress = Arc::new(Progress { total: files.len(), done: AtomicUsize::new(0), cancel: AtomicBool::new(false), finished: AtomicBool::new(false) });
    let (tx, rx) = crossbeam_channel::unbounded::<(usize, FileStatus)>();
    let prog = progress.clone();
    std::thread::Builder::new()
        .name("lumen-batch-convert".into())
        .spawn(move || {
            // `finished` must be set even if the pool itself fails.
            let c2 = ctx.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(files, s, prog.clone(), tx, c2)));
            prog.finished.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        })
        .expect("spawn batch thread");
    Job { progress, rx, started: std::time::Instant::now(), elapsed: None }
}

fn run(files: Vec<PathBuf>, s: ConvertSettings, prog: Arc<Progress>, tx: Sender<(usize, FileStatus)>, ctx: egui::Context) {
    use rayon::prelude::*;
    let reserved = Mutex::new(HashSet::new());
    let work = |i: usize| {
        if prog.cancel.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send((i, FileStatus::Working));
        ctx.request_repaint();
        // A decoder or encoder panic on one file fails that file only.
        let st = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process_one(&files[i], i, &s, &reserved)))
            .unwrap_or_else(|e| FileStatus::Failed(format!("Unexpected error: {}", panic_message(&*e))));
        let _ = tx.send((i, st));
        prog.done.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();
    };
    match rayon::ThreadPoolBuilder::new().num_threads(worker_threads()).thread_name(|i| format!("lumen-batch-{i}")).build() {
        Ok(pool) => pool.install(|| (0..files.len()).into_par_iter().with_max_len(1).for_each(work)),
        Err(_) => (0..files.len()).for_each(work),
    }
}

// ------------------------------------------------------------------ prefs file

/// Batch window preferences persisted to `batch.json` in the data folder.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BatchPrefs {
    pub convert: ConvertSettings,
    pub rename: logic::RenameSettings,
    pub sheet: crate::ui::tools::sheet::SheetSettings,
    pub print: crate::ui::tools::print::PrintOptions,
    pub multi: crate::ui::tools::multipage::MultiSettings,
    pub strip: crate::ui::tools::multipage::StripSettings,
    pub zip: crate::ui::tools::multipage::ZipSettings,
}

fn prefs_path() -> PathBuf {
    crate::settings::data_dir().join("batch.json")
}

pub fn load_prefs() -> BatchPrefs {
    std::fs::read(prefs_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_prefs(update: impl FnOnce(&mut BatchPrefs)) {
    let mut p = load_prefs();
    update(&mut p);
    if let Ok(json) = serde_json::to_vec_pretty(&p) {
        let path = prefs_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::encode::write_atomic(&path, &json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_naming_and_dirs() {
        let mut s = ConvertSettings { template: "{name}_{width}x{height}".into(), format: SaveFormat::WebP, ..Default::default() };
        let input = PathBuf::from("C:\\pics\\IMG 1.HEIC");
        assert_eq!(s.output_name(&input, 0, (800, 600), None), "IMG 1_800x600.webp");
        s.template = "a/b:{n}".into();
        s.start = 5;
        assert_eq!(s.output_name(&input, 2, (1, 1), None), "a_b_7.webp");
        s.keep_format = true;
        assert_eq!(s.output_name(&PathBuf::from("C:\\x\\p.png"), 0, (1, 1), None), "a_b_5.png");
        assert_eq!(s.output_name(&input, 0, (1, 1), None), "a_b_5.jpg", "unwritable format falls back to JPEG");
        s.dest = DestMode::Subfolder;
        s.subfolder = "Small".into();
        assert_eq!(s.output_dir(&input), PathBuf::from("C:\\pics\\Small"));
        s.dest = DestMode::Same;
        assert_eq!(s.output_dir(&input), PathBuf::from("C:\\pics"));
        s.dest = DestMode::Folder;
        s.out_folder = Some(PathBuf::from("D:\\out"));
        assert_eq!(s.output_dir(&input), PathBuf::from("D:\\out"));
    }

    #[test]
    fn overwrite_guard_covers_every_destination() {
        let dir = std::env::temp_dir().join(format!("lumen-conv-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = vec![dir.join("a.jpg")];
        let mut s = ConvertSettings { conflict: ConflictPolicy::Overwrite, dest: DestMode::Same, ..Default::default() };
        assert!(s.overwrites_originals(&files));
        s.dest = DestMode::Folder;
        s.out_folder = Some(dir.clone());
        assert!(s.overwrites_originals(&files), "chosen folder == source folder");
        s.out_folder = Some(PathBuf::from(format!("{}\\", dir.display())));
        assert!(s.overwrites_originals(&files), "trailing separator");
        s.out_folder = Some(dir.join("other"));
        assert!(!s.overwrites_originals(&files));
        s.dest = DestMode::Subfolder;
        s.subfolder = ".".into();
        assert!(!s.overwrites_originals(&files), "'.' is sanitized to a real subfolder name");
        s.subfolder = "Converted".into();
        assert!(!s.overwrites_originals(&files));
        s.dest = DestMode::Same;
        s.conflict = ConflictPolicy::Rename;
        assert!(!s.overwrites_originals(&files));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflict_resolution_reserves_names() {
        let dir = std::env::temp_dir().join(format!("lumen-conv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("a.jpg");
        std::fs::write(&existing, b"x").unwrap();
        let src = dir.join("src.png");
        let reserved = Mutex::new(HashSet::new());
        let r1 = resolve(existing.clone(), ConflictPolicy::Rename, &src, &reserved).unwrap();
        let r2 = resolve(existing.clone(), ConflictPolicy::Rename, &src, &reserved).unwrap();
        assert_eq!(r1, dir.join("a (2).jpg"));
        assert_eq!(r2, dir.join("a (3).jpg"));
        assert!(resolve(existing.clone(), ConflictPolicy::Skip, &src, &reserved).is_err());
        let fresh = dir.join("b.jpg");
        assert_eq!(resolve(fresh.clone(), ConflictPolicy::Overwrite, &src, &reserved).unwrap(), fresh);
        // A second batch file mapping to the same name never overwrites the first output.
        assert_eq!(resolve(fresh.clone(), ConflictPolicy::Overwrite, &src, &reserved).unwrap(), dir.join("b (2).jpg"));
        let reserved = Mutex::new(HashSet::new());
        assert_eq!(resolve(existing.clone(), ConflictPolicy::Overwrite, &src, &reserved).unwrap(), existing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn end_to_end_convert() {
        let dir = std::env::temp_dir().join(format!("lumen-conv-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = RgbaImage::new(400, 200);
        for (x, y, p) in img.enumerate_pixels_mut() {
            p.0 = [(x % 256) as u8, (y % 256) as u8, 90, 255];
        }
        let input = dir.join("wide.png");
        img.save(&input).unwrap();
        let s = ConvertSettings {
            format: SaveFormat::Jpeg,
            resize: ResizeSpec { mode: logic::ResizeMode::LongEdge, a: 100, ..Default::default() },
            dest: DestMode::Subfolder,
            subfolder: "out".into(),
            template: "{name}-{n:2}".into(),
            grayscale: true,
            ..Default::default()
        };
        let reserved = Mutex::new(HashSet::new());
        match process_one(&input, 0, &s, &reserved) {
            FileStatus::Done { out, dims, .. } => {
                assert_eq!(out, dir.join("out").join("wide-01.jpg"));
                assert_eq!(dims, (100, 50));
                let back = image::open(&out).unwrap().to_rgba8();
                assert_eq!(back.dimensions(), (100, 50));
                let p = back.get_pixel(50, 25).0;
                assert!((p[0] as i32 - p[2] as i32).abs() <= 3, "grayscale: {p:?}");
            }
            other => panic!("{other:?}"),
        }
        assert!(input.exists(), "original untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
