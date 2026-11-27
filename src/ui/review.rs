//! The review of a comparison: marks drawn on the images (in image pixels, so
//! they stay put when zooming and panning), their notes, comments per image
//! and overall, and detail regions (fractions 0..1 of every image). Saved per
//! set of images under `<data dir>/reviews/` and exported as a report
//! (`lumen_core::report`).

use crate::edit::raster::{self, DrawObj, Kind};
use anyhow::{Context, Result};
use egui::{Pos2, Rect};
use image::RgbaImage;
use lumen_core::report;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Shape {
    Rect,
    Ellipse,
    Arrow,
    Pen,
    Highlighter,
}

impl Shape {
    pub fn label(self) -> &'static str {
        match self {
            Shape::Rect => "Rectangle",
            Shape::Ellipse => "Ellipse",
            Shape::Arrow => "Arrow",
            Shape::Pen => "Pen",
            Shape::Highlighter => "Highlighter",
        }
    }
    pub fn is_freehand(self) -> bool {
        matches!(self, Shape::Pen | Shape::Highlighter)
    }
}

/// One mark on one image. Points are image pixels (full resolution).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub shape: Shape,
    /// Rect / Ellipse: two corners. Arrow: tail, tip. Pen / Highlighter: the stroke.
    pub pts: Vec<[f32; 2]>,
    pub color: [u8; 3],
    /// Stroke width in image pixels.
    pub width: f32,
    #[serde(default)]
    pub note: String,
    /// Artifact tag from `lumen_core::workbench::ARTIFACT_TAGS` ("Halo").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    pub severity: Severity,
}

pub use lumen_core::workbench::Severity;

/// A region of interest shown on every image: fractions 0..1 of each image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub rect: [f32; 4],
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub comment: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageReview {
    pub path: PathBuf,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub marks: Vec<Mark>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    /// In pane order (A, B, ...).
    #[serde(default)]
    pub images: Vec<ImageReview>,
    #[serde(default)]
    pub details: Vec<Detail>,
}

impl Review {
    #[cfg(test)]
    pub fn new(paths: &[PathBuf]) -> Review {
        Review { images: paths.iter().map(|p| ImageReview { path: p.clone(), ..Default::default() }).collect(), ..Default::default() }
    }

    /// Nothing written or drawn yet.
    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty() && self.summary.trim().is_empty() && self.details.is_empty() && self.images.iter().all(|i| i.comment.trim().is_empty() && i.marks.is_empty())
    }

    pub fn mark_count(&self) -> usize {
        self.images.iter().map(|i| i.marks.len()).sum()
    }

    /// Repair what a damaged or hand-edited file can hold: detail boxes
    /// inside the image (0..1) with their corners in order, and no marks or
    /// boxes with values that are not numbers.
    pub fn sanitize(&mut self) {
        self.details.retain(|d| d.rect.iter().all(|v| v.is_finite()));
        for d in &mut self.details {
            let r = d.rect.map(|v| v.clamp(0.0, 1.0));
            d.rect = ordered([r[0], r[1]], [r[2], r[3]]);
        }
        for im in &mut self.images {
            im.marks.retain(|m| m.width.is_finite() && m.pts.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
        }
    }

    /// Reorder (and fill in) the images to match `paths`; images not in
    /// `paths` are dropped, new ones start empty.
    pub fn align(&mut self, paths: &[PathBuf]) {
        let mut old = std::mem::take(&mut self.images);
        self.images = paths
            .iter()
            .map(|p| match old.iter().position(|i| same_file(&i.path, p)).or_else(|| old.iter().position(|i| same_name(&i.path, p))) {
                Some(k) => {
                    let mut im = old.remove(k);
                    im.path = p.clone();
                    im
                }
                None => ImageReview { path: p.clone(), ..Default::default() },
            })
            .collect();
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Same file name and parent folder name: the folder was moved or copied
/// (project reviews keep their data then).
fn same_name(a: &Path, b: &Path) -> bool {
    let tail = |p: &Path| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase());
        let parent = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_lowercase());
        (parent, name)
    };
    let (ta, tb) = (tail(a), tail(b));
    ta.1.is_some() && ta == tb
}

// ---------------------------------------------------------------- persistence

pub fn reviews_dir() -> PathBuf {
    crate::settings::data_dir().join("reviews")
}

/// File name for a set of images: the same set in any order gives the same
/// name (so swapping A and B keeps the review).
pub fn review_key(paths: &[PathBuf]) -> String {
    review_key_in(None, paths)
}

/// Like [`review_key`], with paths inside `root` taken relative to it, so a
/// project's reviews keep their names when the project folder moves.
pub fn review_key_in(root: Option<&Path>, paths: &[PathBuf]) -> String {
    let mut names: Vec<String> = paths
        .iter()
        .map(|p| match root.and_then(|r| lumen_core::workbench::detect::relative(r, p)) {
            Some(rel) => rel.to_lowercase(),
            None => p.to_string_lossy().to_lowercase(),
        })
        .collect();
    names.sort();
    // FNV-1a: stable across builds, unlike the std hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for n in &names {
        for b in n.bytes().chain(std::iter::once(0)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

pub fn review_file(dir: &Path, paths: &[PathBuf]) -> PathBuf {
    review_file_in(dir, None, paths)
}

pub fn review_file_in(dir: &Path, root: Option<&Path>, paths: &[PathBuf]) -> PathBuf {
    dir.join(format!("{}.json", review_key_in(root, paths)))
}

/// Where a project keeps its reviews: beside the project file.
pub fn project_reviews_dir(root: &Path) -> PathBuf {
    root.join("lumen-reviews")
}

/// The saved review for `paths` (empty when there is none or it is unreadable).
pub fn load(dir: &Path, paths: &[PathBuf]) -> Review {
    load_in(dir, None, paths)
}

/// Read-only (reports): an unreadable file gives an empty review.
pub fn load_in(dir: &Path, root: Option<&Path>, paths: &[PathBuf]) -> Review {
    let mut r = std::fs::read_to_string(review_file_in(dir, root, paths)).ok().and_then(|t| serde_json::from_str::<Review>(&t).ok()).unwrap_or_default();
    r.sanitize();
    r.align(paths);
    r
}

/// A review loaded for editing, and what was wrong with its file.
#[derive(Default)]
pub struct Loaded {
    pub review: Review,
    /// The file could not be read as a review: it was renamed to this, so
    /// the next save does not replace it.
    pub set_aside: Option<PathBuf>,
    /// The file is there but could not be read (and not set aside): saving
    /// over it is not safe.
    pub unreadable: Option<String>,
}

/// Load the review of `paths` for editing. A damaged file is never replaced
/// by an empty review: it is kept beside it as `<name>.broken`.
pub fn load_checked_in(dir: &Path, root: Option<&Path>, paths: &[PathBuf]) -> Loaded {
    let file = review_file_in(dir, root, paths);
    let mut out = Loaded::default();
    match std::fs::read(&file) {
        Ok(bytes) => match serde_json::from_slice::<Review>(&bytes) {
            Ok(r) => out.review = r,
            Err(e) => {
                let aside = broken_name(&file);
                match std::fs::rename(&file, &aside) {
                    Ok(()) => {
                        log::warn!("review {} is damaged ({e}); kept as {}", file.display(), aside.display());
                        out.set_aside = Some(aside);
                    }
                    Err(e2) => out.unreadable = Some(format!("{e} ({e2})")),
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => out.unreadable = Some(e.to_string()),
    }
    out.review.sanitize();
    out.review.align(paths);
    out
}

/// `<name>.broken` beside `file`, numbered when that is taken.
fn broken_name(file: &Path) -> PathBuf {
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    (1..1000).map(|i| if i == 1 { file.with_file_name(format!("{name}.broken")) } else { file.with_file_name(format!("{name}.broken-{i}")) }).find(|p| !p.exists()).unwrap_or_else(|| file.with_file_name(format!("{name}.broken")))
}

/// Write `r` for its images. An empty review is written only to replace an
/// earlier file (so clearing everything sticks).
pub fn save(dir: &Path, r: &Review) -> Result<()> {
    save_in(dir, None, r)
}

/// Write `r` now on this thread, after any queued background write.
pub fn save_in(dir: &Path, root: Option<&Path>, r: &Review) -> Result<()> {
    if let Some((file, bytes)) = save_bytes_in(dir, root, r)? {
        lumen_core::persist::write_now(&file, &bytes)?;
    }
    Ok(())
}

/// The file and content to write for `r`; `None` when there is nothing to
/// save (an empty review and no earlier file). Written with
/// `lumen_core::persist`: one writer, a unique temp file flushed to disk and
/// swapped in, so overlapping saves never leave a half-written file.
pub fn save_bytes_in(dir: &Path, root: Option<&Path>, r: &Review) -> Result<Option<(PathBuf, Vec<u8>)>> {
    let paths: Vec<PathBuf> = r.images.iter().map(|i| i.path.clone()).collect();
    let file = review_file_in(dir, root, &paths);
    if r.is_empty() && !file.exists() {
        return Ok(None);
    }
    Ok(Some((file, serde_json::to_vec_pretty(r)?)))
}

/// Remembered export choices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub author: String,
    /// "pptx", "pdf" or "html".
    pub format: String,
    pub title_page: bool,
    pub overview: bool,
    pub per_image: bool,
    pub details: bool,
    pub metrics: bool,
    pub image_info: bool,
    pub open_after: bool,
    pub last_dir: Option<PathBuf>,
    /// "lum", "rgb" or "both".
    pub histogram: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            author: std::env::var("USERNAME").unwrap_or_default(),
            format: "pptx".into(),
            title_page: true,
            overview: true,
            per_image: true,
            details: true,
            metrics: true,
            image_info: true,
            open_after: true,
            last_dir: None,
            histogram: "both".into(),
        }
    }
}

impl Prefs {
    pub fn load() -> Prefs {
        std::fs::read_to_string(reviews_dir().join("prefs.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        let dir = reviews_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(t) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(dir.join("prefs.json"), t);
        }
    }
    pub fn format(&self) -> report::Format {
        report::Format::ALL.into_iter().find(|f| f.ext() == self.format).unwrap_or(report::Format::Pptx)
    }
}

// ---------------------------------------------------------------- styles

/// Mark colours: none of them close to the A-D badge colours (blue, orange,
/// green, purple).
pub const COLORS: [(&str, [u8; 3]); 6] = [
    ("Red", [235, 45, 45]),
    ("Yellow", [255, 214, 0]),
    ("Pink", [240, 70, 170]),
    ("Cyan", [0, 205, 225]),
    ("White", [255, 255, 255]),
    ("Black", [24, 24, 24]),
];

/// Stroke widths per 1000 px of the image's long side: thin, medium, thick.
pub const WIDTHS: [(&str, f32); 3] = [("Thin", 2.0), ("Medium", 4.0), ("Thick", 7.0)];

/// Stroke width in image pixels for width step `w` on an image of `dims`.
pub fn stroke_px(shape: Shape, w: usize, dims: (u32, u32)) -> f32 {
    let base = WIDTHS[w.min(WIDTHS.len() - 1)].1 * dims.0.max(dims.1).max(1) as f32 / 1000.0;
    let base = base.max(1.0);
    if shape == Shape::Highlighter {
        base * 4.0
    } else {
        base
    }
}

/// Black or white, whichever reads on `c`.
pub fn ink_on(c: [u8; 3]) -> [u8; 3] {
    let l = 0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32;
    if l > 150.0 {
        [20, 20, 20]
    } else {
        [255, 255, 255]
    }
}

// ---------------------------------------------------------------- geometry

/// Image pixel to screen point, for an image of `dims` drawn into `dest`.
pub fn to_screen(dest: Rect, dims: (u32, u32), p: [f32; 2]) -> Pos2 {
    let s = dest.width() / dims.0.max(1) as f32;
    Pos2::new(dest.min.x + p[0] * s, dest.min.y + p[1] * s)
}

/// Screen point to image pixel (unclamped).
pub fn to_image(dest: Rect, dims: (u32, u32), p: Pos2) -> [f32; 2] {
    let s = (dest.width() / dims.0.max(1) as f32).max(1e-9);
    [(p.x - dest.min.x) / s, (p.y - dest.min.y) / s]
}

pub fn clamp_px(p: [f32; 2], dims: (u32, u32)) -> [f32; 2] {
    [p[0].clamp(0.0, dims.0 as f32), p[1].clamp(0.0, dims.1 as f32)]
}

/// (x0, y0, x1, y1) with x0 <= x1, y0 <= y1.
pub fn ordered(a: [f32; 2], b: [f32; 2]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
}

/// A detail box (fractions 0..1) moved by `d`, kept inside the image. Never
/// panics, also for a box wider than the image or with values that are not
/// numbers (a damaged file).
pub fn moved_box(r: [f32; 4], d: [f32; 2]) -> [f32; 4] {
    let (w, h) = (r[2] - r[0], r[3] - r[1]);
    let x = (r[0] + d[0]).min(1.0 - w).max(0.0);
    let y = (r[1] + d[1]).min(1.0 - h).max(0.0);
    [x, y, x + w, y + h]
}

/// The drag end for a box from `a` to `b`; `square` makes it a square (or a
/// circle) the size of the longer side.
pub fn constrain(a: [f32; 2], b: [f32; 2], square: bool) -> [f32; 2] {
    if !square {
        return b;
    }
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let s = dx.abs().max(dy.abs());
    [a[0] + s * if dx < 0.0 { -1.0 } else { 1.0 }, a[1] + s * if dy < 0.0 { -1.0 } else { 1.0 }]
}

/// An image-pixel rectangle as fractions of the image.
#[cfg(test)]
pub fn to_rel(r: [f32; 4], dims: (u32, u32)) -> [f32; 4] {
    let (w, h) = (dims.0.max(1) as f32, dims.1.max(1) as f32);
    [(r[0] / w).clamp(0.0, 1.0), (r[1] / h).clamp(0.0, 1.0), (r[2] / w).clamp(0.0, 1.0), (r[3] / h).clamp(0.0, 1.0)]
}

/// Fractions of an image to its pixels.
pub fn from_rel(r: [f32; 4], dims: (u32, u32)) -> [f32; 4] {
    let (w, h) = (dims.0 as f32, dims.1 as f32);
    [r[0] * w, r[1] * h, r[2] * w, r[3] * h]
}

/// Whole pixels (x, y, w, h) of a relative region, at least 1 x 1, inside the image.
pub fn crop_box(r: [f32; 4], dims: (u32, u32)) -> (u32, u32, u32, u32) {
    let p = from_rel(r, dims);
    let x0 = (p[0].floor().max(0.0) as u32).min(dims.0.saturating_sub(1));
    let y0 = (p[1].floor().max(0.0) as u32).min(dims.1.saturating_sub(1));
    let x1 = (p[2].ceil() as u32).clamp(x0 + 1, dims.0.max(x0 + 1));
    let y1 = (p[3].ceil() as u32).clamp(y0 + 1, dims.1.max(y0 + 1));
    (x0, y0, x1 - x0, y1 - y0)
}

impl Mark {
    /// A new mark with no note or tag.
    pub fn new(shape: Shape, pts: Vec<[f32; 2]>, color: [u8; 3], width: f32) -> Mark {
        Mark { shape, pts, color, width, note: String::new(), tag: None, severity: Severity::default() }
    }

    /// The mark's line in a report: "Halo (severe): roof edge".
    pub fn report_note(&self) -> String {
        let note = self.note.trim();
        match &self.tag {
            Some(t) if note.is_empty() => format!("{t} ({})", self.severity.label().to_lowercase()),
            Some(t) => format!("{t} ({}): {note}", self.severity.label().to_lowercase()),
            None => note.to_string(),
        }
    }

    pub fn draw_obj(&self) -> DrawObj {
        let kind = match self.shape {
            Shape::Rect => Kind::Rect,
            Shape::Ellipse => Kind::Ellipse,
            Shape::Arrow => Kind::Arrow,
            Shape::Pen => Kind::Pen,
            Shape::Highlighter => Kind::Highlighter,
        };
        let c = self.color;
        DrawObj { kind, pts: self.pts.clone(), color: [c[0], c[1], c[2], 255], width: self.width, opacity: 1.0, ..Default::default() }
    }

    pub fn is_valid(&self) -> bool {
        self.draw_obj().is_valid() && (!self.shape.is_freehand() || self.pts.len() >= 2)
    }

    /// Bounding box in image pixels.
    pub fn bounds(&self) -> [f32; 4] {
        self.pts.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }

    pub fn hit(&self, p: [f32; 2], tol: f32) -> bool {
        self.draw_obj().hit(p, tol)
    }

    pub fn translate(&mut self, d: [f32; 2]) {
        for p in &mut self.pts {
            p[0] += d[0];
            p[1] += d[1];
        }
    }

    /// Where the number badge goes (image pixels): the arrow's tail, the
    /// stroke's start, else the box's top-left corner.
    pub fn anchor(&self) -> [f32; 2] {
        match self.shape {
            Shape::Arrow | Shape::Pen | Shape::Highlighter => self.pts.first().copied().unwrap_or([0.0, 0.0]),
            _ => {
                let b = self.bounds();
                [b[0], b[1]]
            }
        }
    }
}

/// Number badge diameter in image pixels for an image of `dims`.
pub fn badge_px(dims: (u32, u32)) -> f32 {
    (dims.0.max(dims.1) as f32 / 28.0).max(16.0)
}

/// The badge centre: up and left of the anchor so it does not cover the
/// marked area, kept inside the image.
pub fn badge_center(anchor: [f32; 2], d: f32, dims: (u32, u32)) -> [f32; 2] {
    let r = d / 2.0;
    let cx = (anchor[0] - r * 0.8).clamp(r, (dims.0 as f32 - r).max(r));
    let cy = (anchor[1] - r * 0.8).clamp(r, (dims.1 as f32 - r).max(r));
    [cx, cy]
}

/// The marks and their number badges as Draw Board objects (image pixels).
pub fn burn_objects(marks: &[Mark], dims: (u32, u32)) -> Vec<DrawObj> {
    let d = badge_px(dims);
    let mut objs: Vec<DrawObj> = marks.iter().filter(|m| m.is_valid()).map(|m| m.draw_obj()).collect();
    for (n, m) in marks.iter().filter(|m| m.is_valid()).enumerate() {
        let ink = ink_on(m.color);
        objs.push(DrawObj {
            kind: Kind::Number,
            pts: vec![badge_center(m.anchor(), d, dims)],
            color: [m.color[0], m.color[1], m.color[2], 255],
            text_color: [ink[0], ink[1], ink[2], 255],
            font_px: d,
            number: n as u32 + 1,
            bold: true,
            ..Default::default()
        });
    }
    objs
}

// ---------------------------------------------------------------- export

/// One image of an export.
#[derive(Clone)]
pub struct ExportImage {
    pub label: String,
    pub path: PathBuf,
    pub image: Arc<RgbaImage>,
    pub file_size: u64,
    pub comment: String,
    pub marks: Vec<Mark>,
}

/// Everything an export needs; built on the UI thread, run on a worker.
#[derive(Clone)]
pub struct ExportJob {
    pub title: String,
    pub author: String,
    pub summary: String,
    pub images: Vec<ExportImage>,
    pub details: Vec<Detail>,
    pub metrics: Option<report::Table>,
    pub sections: report::Sections,
    pub image_info: bool,
    pub format: report::Format,
    pub out: PathBuf,
    /// Company branding (logo path absolute); empty keeps Lumen's look.
    pub branding: report::Branding,
}

/// Long side of the exported images (pixels).
const PICTURE_SIDE: u32 = 2400;
const CROP_SIDE: u32 = 1600;

/// The image scaled to at most `PICTURE_SIDE` with its marks and badges drawn in.
pub fn annotated(img: &RgbaImage, marks: &[Mark]) -> RgbaImage {
    let dims = img.dimensions();
    let small = lumen_core::imaging::resize_to_fit(img, PICTURE_SIDE, PICTURE_SIDE);
    let scale = small.width() as f32 / dims.0.max(1) as f32;
    raster::render_all(&small, &burn_objects(marks, dims), scale)
}

/// The detail region `r` cut from `img`, with the marks that cross it.
pub fn detail_crop(img: &RgbaImage, r: [f32; 4], marks: &[Mark]) -> RgbaImage {
    let dims = img.dimensions();
    let (x, y, w, h) = crop_box(r, dims);
    let mut crop = image::imageops::crop_imm(img, x, y, w, h).to_image();
    for mut o in burn_objects(marks, dims) {
        o.translate(-(x as f32), -(y as f32));
        raster::render_obj(&mut crop, &o, 1.0);
    }
    crop
}

fn fmt_size(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

/// "4000 × 3000 · 3.2 MB · Canon EOS R5 · 1/250 s · f/4 · ISO 400".
pub fn info_line(dims: (u32, u32), file_size: u64, exif: Option<&crate::meta::ExifSummary>) -> String {
    let mut parts = vec![format!("{} × {}", dims.0, dims.1)];
    if file_size > 0 {
        parts.push(fmt_size(file_size));
    }
    if let Some(e) = exif {
        let iso = e.iso.as_ref().map(|v| if v.to_ascii_uppercase().starts_with("ISO") { v.clone() } else { format!("ISO {v}") });
        parts.extend([e.camera.clone(), e.exposure.as_deref().map(tidy_exposure), e.aperture.clone(), iso].into_iter().flatten());
    }
    parts.join(" · ")
}

/// "1/41.1522633744856 s" as "1/41 s"; other forms unchanged.
fn tidy_exposure(e: &str) -> String {
    let t = e.trim().trim_end_matches('s').trim();
    if let Some(d) = t.strip_prefix("1/").and_then(|d| d.parse::<f64>().ok()) {
        return format!("1/{} s", d.round().max(1.0));
    }
    e.to_string()
}

pub fn today() -> String {
    chrono::Local::now().format("%-d %B %Y").to_string()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

/// Build the report: images with their marks burned in, detail crops.
pub fn build_report(job: &ExportJob) -> Result<report::Report> {
    use rayon::prelude::*;
    let items = job
        .images
        .par_iter()
        .map(|im| -> Result<report::Item> {
            let pic = report::Picture::from_rgba(&annotated(&im.image, &im.marks), PICTURE_SIDE)?;
            let details = if job.image_info { info_line(im.image.dimensions(), im.file_size, crate::meta::read_exif(&im.path).as_ref()) } else { String::new() };
            Ok(report::Item {
                label: im.label.clone(),
                name: name_of(&im.path),
                details,
                picture: pic,
                comment: im.comment.trim().to_string(),
                notes: im.marks.iter().filter(|m| m.is_valid()).map(|m| m.report_note()).collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut details = Vec::new();
    for (n, d) in job.details.iter().enumerate() {
        let crops = job
            .images
            .par_iter()
            .map(|im| Ok((im.label.clone(), report::Picture::from_rgba(&detail_crop(&im.image, d.rect, &im.marks), CROP_SIDE)?)))
            .collect::<Result<Vec<_>>>()?;
        let title = if d.title.trim().is_empty() { format!("Detail {}", n + 1) } else { d.title.trim().to_string() };
        details.push(report::Detail { title, comment: d.comment.trim().to_string(), crops });
    }
    let (brand, warning) = report::Brand::load(&job.branding, None);
    if let Some(w) = warning {
        log::warn!("review report: {w}");
    }
    Ok(report::Report {
        brand,
        title: if job.title.trim().is_empty() { "Comparison review".into() } else { job.title.trim().to_string() },
        author: job.author.trim().to_string(),
        date: today(),
        summary: job.summary.trim().to_string(),
        items,
        details,
        metrics: job.metrics.clone(),
        sections: job.sections,
    })
}

pub fn run_export(job: &ExportJob) -> Result<PathBuf> {
    let r = build_report(job)?;
    report::write(&r, job.format, &job.out).with_context(|| format!("writing {}", job.out.display()))?;
    Ok(job.out.clone())
}

/// A file name from the report title.
pub fn file_stem(title: &str) -> String {
    let s: String = title.trim().chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        "Comparison review".into()
    } else {
        s.chars().take(80).collect()
    }
}

// ---------------------------------------------------------------- screen helpers

/// A rectangle in screen points from an image-pixel box.
pub fn screen_rect(dest: Rect, dims: (u32, u32), b: [f32; 4]) -> Rect {
    Rect::from_two_pos(to_screen(dest, dims, [b[0], b[1]]), to_screen(dest, dims, [b[2], b[3]]))
}

/// Screen points per image pixel.
pub fn scale_of(dest: Rect, dims: (u32, u32)) -> f32 {
    dest.width() / dims.0.max(1) as f32
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;

    fn mark(shape: Shape, pts: Vec<[f32; 2]>) -> Mark {
        Mark::new(shape, pts, COLORS[0].1, 4.0)
    }

    #[test]
    fn screen_image_round_trip() {
        // A 4000 x 2000 image drawn at a quarter size, offset.
        let dest = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(1000.0, 500.0));
        let dims = (4000, 2000);
        assert_eq!(to_screen(dest, dims, [0.0, 0.0]), Pos2::new(100.0, 50.0));
        assert_eq!(to_screen(dest, dims, [4000.0, 2000.0]), Pos2::new(1100.0, 550.0));
        let p = to_image(dest, dims, Pos2::new(350.0, 175.0));
        assert!((p[0] - 1000.0).abs() < 1e-3 && (p[1] - 500.0).abs() < 1e-3);
        // Zooming moves `dest`; the mark keeps its image position.
        let zoomed = Rect::from_min_size(Pos2::new(-900.0, -450.0), Vec2::new(4000.0, 2000.0));
        let s = to_screen(zoomed, dims, p);
        assert!((s - Pos2::new(100.0, 50.0)).length() < 1e-3);
        assert_eq!(to_image(zoomed, dims, s), p);
    }

    #[test]
    fn relative_regions_map_to_every_image() {
        let a = (4000, 3000);
        let b = (1000, 750);
        let rel = to_rel([1000.0, 750.0, 2000.0, 1500.0], a);
        assert_eq!(rel, [0.25, 0.25, 0.5, 0.5]);
        assert_eq!(from_rel(rel, b), [250.0, 187.5, 500.0, 375.0]);
        assert_eq!(crop_box(rel, b), (250, 187, 250, 188));
        // Clamped to the image and at least one pixel.
        assert_eq!(to_rel([-10.0, -10.0, 5000.0, 4000.0], a), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(crop_box([0.5, 0.5, 0.5, 0.5], b), (500, 375, 1, 1));
        assert_eq!(crop_box([1.0, 1.0, 1.0, 1.0], b), (999, 749, 1, 1));
    }

    #[test]
    fn constrain_makes_squares() {
        assert_eq!(constrain([10.0, 10.0], [30.0, 15.0], true), [30.0, 30.0]);
        assert_eq!(constrain([10.0, 10.0], [0.0, 40.0], true), [-20.0, 40.0]);
        assert_eq!(constrain([10.0, 10.0], [0.0, 40.0], false), [0.0, 40.0]);
        assert_eq!(ordered([5.0, 1.0], [2.0, 8.0]), [2.0, 1.0, 5.0, 8.0]);
    }

    #[test]
    fn marks_hit_and_validate() {
        let r = mark(Shape::Rect, vec![[10.0, 10.0], [110.0, 60.0]]);
        assert!(r.is_valid());
        assert!(r.hit([10.0, 30.0], 2.0));
        assert!(!r.hit([60.0, 35.0], 2.0));
        assert!(!mark(Shape::Rect, vec![[10.0, 10.0], [10.2, 10.2]]).is_valid());
        assert!(!mark(Shape::Pen, vec![[10.0, 10.0]]).is_valid());
        let a = mark(Shape::Arrow, vec![[0.0, 0.0], [100.0, 0.0]]);
        assert!(a.hit([50.0, 1.0], 2.0));
        assert_eq!(a.anchor(), [0.0, 0.0]);
        assert_eq!(r.anchor(), [10.0, 10.0]);
    }

    #[test]
    fn badges_stay_inside() {
        let c = badge_center([0.0, 0.0], 40.0, (1000, 800));
        assert_eq!(c, [20.0, 20.0]);
        let c = badge_center([500.0, 400.0], 40.0, (1000, 800));
        assert_eq!(c, [484.0, 384.0]);
        let objs = burn_objects(&[mark(Shape::Rect, vec![[100.0, 100.0], [300.0, 200.0]]), mark(Shape::Pen, vec![[1.0, 1.0]])], (1000, 800));
        // The invalid pen is skipped: one shape and one badge numbered 1.
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[1].kind, Kind::Number);
        assert_eq!(objs[1].number, 1);
    }

    #[test]
    fn serde_round_trip_and_align() {
        let paths = vec![PathBuf::from("C:/x/a.jpg"), PathBuf::from("C:/x/b.jpg")];
        let mut r = Review::new(&paths);
        r.title = "A vs B".into();
        r.summary = "B is sharper".into();
        r.images[1].comment = "soft corners".into();
        r.images[0].marks.push(Mark { note: "halo".into(), ..mark(Shape::Ellipse, vec![[1.0, 2.0], [30.0, 40.0]]) });
        r.details.push(Detail { rect: [0.1, 0.2, 0.3, 0.4], title: "Roof".into(), comment: "banding".into() });
        let text = serde_json::to_string(&r).unwrap();
        let back: Review = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
        // Missing fields default.
        let min: Review = serde_json::from_str(r#"{"images":[{"path":"C:/x/a.jpg"}]}"#).unwrap();
        assert_eq!(min.images[0].comment, "");
        // Swapped panes: data follows its file.
        let mut s = back.clone();
        let swapped = vec![paths[1].clone(), PathBuf::from("C:/X/A.JPG")];
        s.align(&swapped);
        assert_eq!(s.images[0].comment, "soft corners");
        assert_eq!(s.images[1].marks.len(), 1);
        assert_eq!(review_key(&paths), review_key(&swapped));
        assert_ne!(review_key(&paths), review_key(&paths[..1]));
    }

    #[test]
    fn project_reviews_follow_a_moved_folder() {
        let a = PathBuf::from("C:/caps/A56/night.jpg");
        let b = PathBuf::from("C:/caps/X/night.jpg");
        let moved = [PathBuf::from("D:/share/caps/A56/night.jpg"), PathBuf::from("D:/share/caps/X/night.jpg")];
        assert_eq!(review_key_in(Some(Path::new("C:/caps")), &[a.clone(), b.clone()]), review_key_in(Some(Path::new("D:/share/caps")), &moved));
        assert_ne!(review_key_in(None, &[a.clone(), b.clone()]), review_key_in(None, &moved));
        let mut r = Review::new(&[a, b]);
        r.images[1].comment = "soft".into();
        r.align(&moved);
        assert_eq!(r.images[1].comment, "soft");
        assert_eq!(r.images[1].path, moved[1]);
        // Same file name in another device folder is a different file.
        assert!(!same_name(Path::new("C:/caps/A56/night.jpg"), Path::new("C:/caps/X/night.jpg")));
    }

    #[test]
    fn report_notes_carry_tags() {
        let mut m = mark(Shape::Rect, vec![[0.0, 0.0], [10.0, 10.0]]);
        assert_eq!(m.report_note(), "");
        m.tag = Some("Halo".into());
        m.severity = Severity::Severe;
        assert_eq!(m.report_note(), "Halo (severe)");
        m.note = " roof ".into();
        assert_eq!(m.report_note(), "Halo (severe): roof");
        let back: Mark = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        // Old files without tags still load.
        let old: Mark = serde_json::from_str(r#"{"shape":"Rect","pts":[[0,0],[1,1]],"color":[1,2,3],"width":2.0}"#).unwrap();
        assert_eq!(old.tag, None);
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("lumen-review-{}", std::process::id()));
        let paths = vec![PathBuf::from("C:/x/a.jpg"), PathBuf::from("C:/x/b.jpg")];
        // Nothing to save: no file.
        save(&dir, &Review::new(&paths)).unwrap();
        assert!(!review_file(&dir, &paths).exists());
        let mut r = Review::new(&paths);
        r.summary = "ok".into();
        save(&dir, &r).unwrap();
        let back = load(&dir, &[paths[1].clone(), paths[0].clone()]);
        assert_eq!(back.summary, "ok");
        assert_eq!(back.images[0].path, paths[1]);
        // Clearing everything replaces the file.
        save(&dir, &Review::new(&paths)).unwrap();
        assert!(load(&dir, &paths).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_review_is_set_aside_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("lumen-review-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = vec![PathBuf::from("C:/x/a.jpg"), PathBuf::from("C:/x/b.jpg")];
        let file = review_file(&dir, &paths);
        // Half a file, as a crash during a write could leave it.
        std::fs::write(&file, b"{\"summary\":\"lots of work\",\"images\":[{\"pa").unwrap();
        let l = load_checked_in(&dir, None, &paths);
        assert!(l.review.is_empty());
        assert!(l.unreadable.is_none());
        let aside = l.set_aside.expect("set aside");
        assert_eq!(aside.file_name().unwrap().to_string_lossy(), format!("{}.broken", file.file_name().unwrap().to_string_lossy()));
        // Saving the new review leaves the damaged one alone.
        let mut r = l.review;
        r.summary = "new".into();
        save(&dir, &r).unwrap();
        assert!(std::fs::read_to_string(&aside).unwrap().contains("lots of work"));
        assert_eq!(load(&dir, &paths).summary, "new");
        // A second damaged file gets its own name.
        std::fs::write(&file, b"not json").unwrap();
        let again = load_checked_in(&dir, None, &paths).set_aside.unwrap();
        assert_ne!(again, aside);
        assert!(aside.exists() && again.exists());
        // No temp files are left behind.
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert!(names.iter().all(|n| !n.contains("tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_detail_boxes_are_repaired_and_moving_them_never_panics() {
        let mut r = Review::new(&[PathBuf::from("C:/x/a.jpg")]);
        r.details.push(Detail { rect: [0.9, -0.5, 0.1, 3.0], title: String::new(), comment: String::new() });
        r.details.push(Detail { rect: [f32::NAN, 0.0, 0.5, 0.5], title: String::new(), comment: String::new() });
        r.images[0].marks.push(mark(Shape::Rect, vec![[f32::INFINITY, 0.0], [10.0, 10.0]]));
        r.images[0].marks.push(mark(Shape::Rect, vec![[0.0, 0.0], [10.0, 10.0]]));
        r.sanitize();
        assert_eq!(r.details.len(), 1);
        assert_eq!(r.details[0].rect, [0.1, 0.0, 0.9, 1.0]);
        assert_eq!(r.images[0].marks.len(), 1);
        // Moving keeps a box inside the image.
        assert_eq!(moved_box([0.25, 0.25, 0.5, 0.5], [0.75, -0.5]), [0.75, 0.0, 1.0, 0.25]);
        // A box wider than the image (a damaged file) or not a number: no panic.
        assert_eq!(moved_box([-0.5, 0.0, 1.5, 0.5], [0.1, 0.0])[0], 0.0);
        let _ = moved_box([f32::NAN, 0.0, 0.5, f32::NAN], [0.1, 0.1]);
    }

    fn gray(w: u32, h: u32) -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_pixel(w, h, image::Rgba([128, 128, 128, 255])))
    }

    #[test]
    fn marks_are_burned_in() {
        let img = gray(400, 300);
        let m = Mark { color: [255, 0, 0], width: 6.0, ..mark(Shape::Rect, vec![[100.0, 100.0], [300.0, 200.0]]) };
        let out = annotated(&img, std::slice::from_ref(&m));
        assert_eq!(out.dimensions(), (400, 300));
        let p = out.get_pixel(200, 100).0;
        assert!(p[0] > 200 && p[1] < 60, "{p:?}");
        assert_eq!(out.get_pixel(200, 150).0, [128, 128, 128, 255]);
        // The crop (y 75..150) shows the box's top edge at y 100.
        let crop = detail_crop(&img, [0.25, 0.25, 0.75, 0.5], &[m]);
        assert_eq!(crop.dimensions(), (200, 75));
        assert!(crop.get_pixel(100, 25).0[0] > 200);
        assert_eq!(crop.get_pixel(100, 60).0, [128, 128, 128, 255]);
    }

    #[test]
    fn report_has_items_notes_and_crops() {
        let img = gray(320, 240);
        let ims = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, l)| ExportImage {
                label: l.to_string(),
                path: PathBuf::from(format!("C:/nope/img{i}.jpg")),
                image: img.clone(),
                file_size: 2048,
                comment: format!(" comment {i} "),
                marks: vec![Mark { note: "edge".into(), ..mark(Shape::Arrow, vec![[10.0, 10.0], [100.0, 80.0]]) }, mark(Shape::Rect, vec![[1.0, 1.0], [1.1, 1.1]])],
            })
            .collect();
        let job = ExportJob {
            title: "  ".into(),
            author: "Rahul".into(),
            summary: "fine".into(),
            images: ims,
            details: vec![Detail { rect: [0.0, 0.0, 0.5, 0.5], title: String::new(), comment: "x".into() }],
            metrics: None,
            sections: report::Sections::default(),
            image_info: true,
            format: report::Format::Html,
            out: PathBuf::new(),
            // A logo that is not there: the report is made without it.
            branding: report::Branding { company: "Acme".into(), logo: "C:/nope/logo.png".into(), accent: Some([200, 30, 60]), footer: "Confidential".into() },
        };
        let r = build_report(&job).unwrap();
        assert!(r.brand.logo.is_none() && r.brand.company == "Acme" && r.brand.accent == [200, 30, 60] && r.brand.footer == "Confidential");
        assert_eq!(r.title, "Comparison review");
        assert_eq!(r.items.len(), 3);
        assert_eq!(r.items[1].comment, "comment 1");
        // The invalid mark has no note slot.
        assert_eq!(r.items[0].notes, vec!["edge".to_string()]);
        assert_eq!(r.items[0].details, "320 × 240 · 2 KB");
        assert_eq!(r.details.len(), 1);
        assert_eq!(r.details[0].title, "Detail 1");
        assert_eq!(r.details[0].crops.len(), 3);
        assert_eq!((r.details[0].crops[0].1.w, r.details[0].crops[0].1.h), (160, 120));
        let dir = std::env::temp_dir().join(format!("lumen-review-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in report::Format::ALL {
            let out = dir.join(format!("r.{}", f.ext()));
            run_export(&ExportJob { format: f, out: out.clone(), ..job.clone() }).unwrap();
            assert!(std::fs::metadata(&out).unwrap().len() > 500);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exposure_is_rounded() {
        assert_eq!(tidy_exposure("1/41.1522633744856 s"), "1/41 s");
        assert_eq!(tidy_exposure("1/250 s"), "1/250 s");
        assert_eq!(tidy_exposure("2 s"), "2 s");
    }

    #[test]
    fn file_stems_are_safe() {
        assert_eq!(file_stem("A vs B: low/light?"), "A vs B_ low_light_");
        assert_eq!(file_stem("  "), "Comparison review");
    }
}
