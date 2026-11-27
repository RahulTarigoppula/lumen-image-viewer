//! Side-by-side comparison of 2-4 images with synchronized zoom/pan, plus
//! split-slider and difference modes for pairs, and review tools: marks,
//! notes, comments and detail regions exported as a report (`review_ui`).
//!
//! View model: every pane has a `View`. With the *relative* basis the zoom is a
//! multiple of the pane's fit zoom and the centre is a normalized image
//! coordinate, so images of different resolutions stay aligned (same framing).
//! With the *pixel* basis the zoom is physical px per image px and the centre
//! is an offset in image pixels from the image centre, so both show the same
//! pixel scale. When sync is on, an operation is computed on the pane under the
//! cursor and the resulting `View` is copied to every pane.

use super::commands::Command;
use super::icons as I;
use super::keymap::{self, KeyMode};
use super::theme::{self, size, text};
use super::widgets;
use crate::app::Core;
use super::review_ui::{self, PaneGeo, ReviewUi};
use crate::edit::{HistMode, Histogram, HistogramPaint};
use crate::loader::{LoadState, LoadedImage};
use crate::render::{self, GpuImage};
use crossbeam_channel::{unbounded, Receiver};
use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------- layout

/// How Side by side arranges the panes (the Show menu). Auto picks
/// whichever of the others shows these images largest in this window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneLayout {
    Auto,
    Columns,
    Rows,
    Grid,
}

impl PaneLayout {
    const ALL: [PaneLayout; 4] = [PaneLayout::Auto, PaneLayout::Columns, PaneLayout::Rows, PaneLayout::Grid];

    fn label(self) -> &'static str {
        match self {
            PaneLayout::Auto => "Auto",
            PaneLayout::Columns => "Columns",
            PaneLayout::Rows => "Rows",
            PaneLayout::Grid => "Grid",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            PaneLayout::Auto => I::LAYOUT,
            PaneLayout::Columns => I::COLUMNS,
            PaneLayout::Rows => I::ROWS,
            PaneLayout::Grid => I::GRID_FOUR,
        }
    }

    fn tip(self) -> &'static str {
        match self {
            PaneLayout::Auto => "Columns, rows or a grid: whichever shows the images largest for their shape and the window",
            PaneLayout::Columns => "The images next to each other in one row",
            PaneLayout::Rows => "The images one above the other",
            PaneLayout::Grid => "Two rows of two",
        }
    }

    /// The cells of `n` panes (`None` for Auto, which depends on the images).
    fn grid(self, n: usize) -> Option<Grid> {
        let n = n.max(1);
        match self {
            PaneLayout::Auto => None,
            PaneLayout::Columns => Some(Grid { cols: n, rows: 1 }),
            PaneLayout::Rows => Some(Grid { cols: 1, rows: n }),
            PaneLayout::Grid => {
                let cols = (n as f32).sqrt().ceil() as usize;
                Some(Grid { cols, rows: n.div_ceil(cols) })
            }
        }
    }
}

/// Panes in `cols` x `rows` cells, filled row by row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
}

impl Grid {
    /// The named layout this is (for "Auto (grid)").
    fn kind(self) -> PaneLayout {
        if self.rows <= 1 {
            PaneLayout::Columns
        } else if self.cols <= 1 {
            PaneLayout::Rows
        } else {
            PaneLayout::Grid
        }
    }
}

/// Rectangles of `n` panes in `g` cells, `gap` points apart. A last row
/// with fewer panes is centred, its panes as wide as the others.
pub fn grid_rects(n: usize, g: Grid, area: Rect, gap: f32) -> Vec<Rect> {
    let (cols, rows) = (g.cols.max(1), g.rows.max(1));
    let w = (area.width() - gap * (cols as f32 - 1.0)) / cols as f32;
    let h = (area.height() - gap * (rows as f32 - 1.0)) / rows as f32;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = (n - row * cols).min(cols);
            let x = area.min.x + (cols - in_row) as f32 * (w + gap) / 2.0 + col as f32 * (w + gap);
            let y = area.min.y + row as f32 * (h + gap);
            Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
        })
        .collect()
}

/// The cells Auto uses for images of `aspects` (width / height) in `area`:
/// one row, one column or (3 or 4 images) a grid, whichever has the images,
/// each fitted to its cell, cover the most of the area. Another layout has
/// to win by more than 2% to replace one row, then the grid.
pub fn auto_grid(aspects: &[f32], area: Vec2, gap: f32) -> Grid {
    let n = aspects.len().max(1);
    let mut options = vec![Grid { cols: n, rows: 1 }];
    if n >= 3 {
        options.extend(PaneLayout::Grid.grid(n));
    }
    if n >= 2 {
        options.push(Grid { cols: 1, rows: n });
    }
    let covered = |g: Grid| -> f32 {
        let cells = grid_rects(aspects.len(), g, Rect::from_min_size(Pos2::ZERO, area), gap);
        cells
            .iter()
            .zip(aspects)
            .map(|(c, a)| {
                let a = if a.is_finite() && *a > 0.0 { *a } else { 4.0 / 3.0 };
                let w = c.width().min(c.height() * a).max(0.0);
                w * w / a
            })
            .sum()
    };
    let mut best = (options[0], covered(options[0]));
    for g in options.into_iter().skip(1) {
        let c = covered(g);
        if c > best.1 * 1.02 {
            best = (g, c);
        }
    }
    best.0
}

// ---------------------------------------------------------------- view maths

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    Relative,
    Pixel,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub fit: bool,
    pub zoom: f32,
    pub center: Vec2,
}

impl View {
    pub fn fitted(basis: Basis) -> View {
        match basis {
            Basis::Relative => View { fit: true, zoom: 1.0, center: Vec2::splat(0.5) },
            Basis::Pixel => View { fit: true, zoom: 1.0, center: Vec2::ZERO },
        }
    }
}

pub fn fit_of(img: (u32, u32), pane_px: Vec2) -> f32 {
    (pane_px.x / img.0.max(1) as f32).min(pane_px.y / img.1.max(1) as f32).max(1e-6)
}

fn isz(img: (u32, u32)) -> Vec2 {
    Vec2::new(img.0.max(1) as f32, img.1.max(1) as f32)
}

/// Effective (zoom in physical px per image px, image pixel at the pane centre).
/// `fit_ref` is the shared fit zoom used by the pixel basis.
pub fn pane_transform(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32) -> (f32, Vec2) {
    let s = isz(img);
    match basis {
        Basis::Relative => {
            let fz = fit_of(img, pane_px);
            if v.fit {
                (fz, s * 0.5)
            } else {
                (fz * v.zoom, Vec2::new(v.center.x * s.x, v.center.y * s.y))
            }
        }
        Basis::Pixel => {
            if v.fit {
                (fit_ref, s * 0.5)
            } else {
                (v.zoom, s * 0.5 + v.center)
            }
        }
    }
}

/// Turn a "fit" view into explicit numbers so it can be modified.
pub fn materialize(v: &View, basis: Basis, fit_ref: f32) -> View {
    if !v.fit {
        return *v;
    }
    match basis {
        Basis::Relative => View { fit: false, zoom: 1.0, center: Vec2::splat(0.5) },
        Basis::Pixel => View { fit: false, zoom: fit_ref, center: Vec2::ZERO },
    }
}

const MIN_Z: f32 = 0.005;
const MAX_Z: f32 = 64.0;

/// Zoom by `factor` keeping the image point under `anchor` (physical px from the pane centre) fixed.
pub fn zoom_at(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32, factor: f32, anchor: Vec2) -> View {
    let m = materialize(v, basis, fit_ref);
    let (z, c) = pane_transform(&m, basis, img, pane_px, fit_ref);
    let a = c + anchor / z;
    let nz = (z * factor).clamp(MIN_Z, MAX_Z);
    let nc = a - anchor / nz;
    from_effective(basis, img, pane_px, nz, nc)
}

/// Build a view from an effective zoom and image centre (inverse of `pane_transform`).
pub fn from_effective(basis: Basis, img: (u32, u32), pane_px: Vec2, z: f32, c: Vec2) -> View {
    let s = isz(img);
    let v = match basis {
        Basis::Relative => View { fit: false, zoom: z / fit_of(img, pane_px), center: Vec2::new(c.x / s.x, c.y / s.y) },
        Basis::Pixel => View { fit: false, zoom: z, center: c - s * 0.5 },
    };
    clamp_view(v, basis, img)
}

/// Pan by `delta` physical pixels (positive = content moves right/down).
pub fn pan_by(v: &View, basis: Basis, img: (u32, u32), pane_px: Vec2, fit_ref: f32, delta: Vec2) -> View {
    let m = materialize(v, basis, fit_ref);
    let (z, c) = pane_transform(&m, basis, img, pane_px, fit_ref);
    from_effective(basis, img, pane_px, z, c - delta / z)
}

fn clamp_view(mut v: View, basis: Basis, img: (u32, u32)) -> View {
    let s = isz(img);
    match basis {
        Basis::Relative => {
            v.center = Vec2::new(v.center.x.clamp(0.0, 1.0), v.center.y.clamp(0.0, 1.0));
        }
        Basis::Pixel => {
            v.center = Vec2::new(v.center.x.clamp(-s.x / 2.0, s.x / 2.0), v.center.y.clamp(-s.y / 2.0, s.y / 2.0));
        }
    }
    v
}

/// Destination rectangle (points) of the whole image in a pane.
pub fn dest_rect(pane: Rect, ppp: f32, img: (u32, u32), z: f32, c: Vec2) -> Rect {
    let min = pane.center() - c * z / ppp;
    let min = Pos2::new((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
    Rect::from_min_size(min, isz(img) * z / ppp)
}

// ---------------------------------------------------------------- difference

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffStats {
    /// Mean absolute difference per channel (0-255).
    pub mean: f64,
    pub max: u8,
    pub mse: f64,
    /// Peak signal-to-noise ratio in dB (infinite for identical images).
    pub psnr: f64,
    /// Percentage of pixels where any channel differs by more than 2.
    pub changed_pct: f64,
}

pub fn psnr_from_mse(mse: f64) -> f64 {
    if mse <= 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// |A - B| per RGB channel (images must have equal dimensions) and statistics.
pub fn difference(a: &RgbaImage, b: &RgbaImage) -> (RgbaImage, DiffStats) {
    use rayon::prelude::*;
    assert_eq!(a.dimensions(), b.dimensions());
    let (w, h) = a.dimensions();
    let mut out = RgbaImage::new(w, h);
    let row = w as usize * 4;
    let rows_per = (65536 / row.max(1)).max(1);
    let chunk = row * rows_per;
    let parts: Vec<(u64, u64, u8, u64)> = out
        .par_chunks_mut(chunk)
        .zip(a.as_raw().par_chunks(chunk))
        .zip(b.as_raw().par_chunks(chunk))
        .map(|((o, pa), pb)| {
            let (mut sum, mut sq, mut mx, mut changed) = (0u64, 0u64, 0u8, 0u64);
            for ((o, x), y) in o.chunks_exact_mut(4).zip(pa.chunks_exact(4)).zip(pb.chunks_exact(4)) {
                let mut px_max = 0u8;
                for c in 0..3 {
                    let d = x[c].abs_diff(y[c]);
                    o[c] = d;
                    sum += d as u64;
                    sq += (d as u64) * (d as u64);
                    px_max = px_max.max(d);
                }
                o[3] = 255;
                mx = mx.max(px_max);
                if px_max > 2 {
                    changed += 1;
                }
            }
            (sum, sq, mx, changed)
        })
        .collect();
    let n = (w as u64 * h as u64).max(1);
    let (sum, sq, mx, changed) = parts.into_iter().fold((0u64, 0u64, 0u8, 0u64), |acc, p| (acc.0 + p.0, acc.1 + p.1, acc.2.max(p.2), acc.3 + p.3));
    let mse = sq as f64 / (n * 3) as f64;
    let stats = DiffStats { mean: sum as f64 / (n * 3) as f64, max: mx, mse, psnr: psnr_from_mse(mse), changed_pct: changed as f64 * 100.0 / n as f64 };
    (out, stats)
}

/// Multiply a difference image by `amp` (saturating) for display.
pub fn amplify(diff: &RgbaImage, amp: f32) -> RgbaImage {
    use rayon::prelude::*;
    let mut out = diff.clone();
    if (amp - 1.0).abs() < 1e-3 {
        return out;
    }
    let lut: Vec<u8> = (0..256).map(|v| ((v as f32 * amp).round().min(255.0)) as u8).collect();
    out.par_chunks_mut(4 * 16384).for_each(|c| {
        for px in c.chunks_exact_mut(4) {
            px[0] = lut[px[0] as usize];
            px[1] = lut[px[1] as usize];
            px[2] = lut[px[2] as usize];
        }
    });
    out
}

/// Pick the largest pyramid level with at most `max_px` pixels.
fn level_under(pyr: &[Arc<RgbaImage>], max_px: u64) -> Arc<RgbaImage> {
    for l in pyr {
        if l.width() as u64 * l.height() as u64 <= max_px {
            return l.clone();
        }
    }
    pyr.last().cloned().expect("pyramid has a level")
}

/// Results of an "Align to A" job.
enum AlignMsg {
    Pane { i: usize, image: Arc<RgbaImage>, pyramid: Vec<Arc<RgbaImage>>, result: lumen_analysis::align::AlignResult },
    Failed { i: usize, error: String },
    /// Every pane done: the field of view all aligned panes share, as fractions of A.
    Done { common: Option<[f32; 4]> },
}

enum DiffMsg {
    Done { raw: Arc<RgbaImage>, stats: DiffStats, amp: f32, pyramid: Vec<Arc<RgbaImage>>, note: Option<String> },
    Amplified { amp: f32, pyramid: Vec<Arc<RgbaImage>> },
}

#[derive(Default)]
struct DiffState {
    key: Option<(PathBuf, PathBuf)>,
    raw: Option<Arc<RgbaImage>>,
    stats: Option<DiffStats>,
    gpu: Option<(f32, GpuImage)>,
    busy: Option<Receiver<DiffMsg>>,
    note: Option<String>,
}

// ---------------------------------------------------------------- panes

/// A view mode's segment label: its name (like the app's other segmented
/// controls), or its icon when the bar is narrow.
fn mode_label(m: CmpMode, with_name: bool) -> String {
    if with_name { m.label().to_string() } else { m.icon().to_string() }
}

/// Names shown next to the toolbar icons when the bar has room.
const SYNC_LABEL: &str = "Sync";
/// The Show menu's button always has its name.
const SHOW_LABEL: &str = "Show";
const SWAP_LABEL: &str = "Swap";
const MARKUP_LABEL: &str = "Mark up";
const REVIEW_LABEL: &str = "Review";

/// A small muted heading over a group of menu rows ("Layout").
fn menu_caption(ui: &mut Ui, label: &str) {
    let p = theme::palette(ui.ctx());
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(RichText::new(label).font(text::caption()).color(p.muted));
    });
}

/// A toolbar toggle or action: icon and name when `labels`, else the icon.
fn bar_button(ui: &mut Ui, icon: &str, label: &str, tip: &str, selected: bool, labels: bool) -> egui::Response {
    if labels {
        widgets::labeled_icon_button(ui, icon, label, tip, selected)
    } else {
        widgets::icon_button(ui, icon, tip, selected)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpMode {
    SideBySide,
    Split,
    Difference,
}

impl CmpMode {
    fn label(self) -> &'static str {
        match self {
            CmpMode::SideBySide => "Side by side",
            CmpMode::Split => "Split",
            CmpMode::Difference => "Difference",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            CmpMode::SideBySide => I::COLUMNS,
            CmpMode::Split => I::SQUARE_SPLIT_HORIZONTAL,
            CmpMode::Difference => I::SUBTRACT_SQUARE,
        }
    }
    fn command(self) -> Command {
        match self {
            CmpMode::SideBySide => Command::ViewSideBySide,
            CmpMode::Split => Command::ViewSplit,
            CmpMode::Difference => Command::ViewDifference,
        }
    }
    /// The mode's keys from the keymap ("B").
    fn key(self) -> String {
        key_text(self.command())
    }
}

/// `cmd`'s keys in Compare from the active keymap ("Q / `"), "" when unbound.
fn key_text(cmd: Command) -> String {
    keymap::mode_text(&keymap::active(), KeyMode::Compare, cmd)
}

/// A tooltip with `cmd`'s Compare keys: "Swap A and B  (X)".
fn tip(text: &str, cmd: Command) -> String {
    keymap::tip(text, KeyMode::Compare, cmd)
}

/// Commands that do nothing in a blind test: no culling, ratings, mark up,
/// review or report, no camera data, and no swapping of the sides being voted
/// on. Their keys are still taken, so they do not reach anything else.
fn blind_off(cmd: Command) -> bool {
    use Command::*;
    matches!(cmd, Rate(_) | ToggleFlag | KeepOnly | Delete | ToggleMarkup | ToggleReview | ToggleCameraInfo | SwapAB | LineProfile | ExportReport)
}

/// Toolbar parts that move into the "..." menu when the toolbar is too
/// narrow (the Analysis panel open, a large UI scale), first to go first.
/// Help goes earlier, before the mode names (see `Compare::bar_plan`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BarPart {
    /// The Show menu: overlays, layout, Metadata.
    Views,
    Basis,
    Help,
    Extras,
    Cull,
    Swap,
    Sync,
    /// Zoom out, the zoom percentage and zoom in (Fit stays).
    Zoom,
    /// Mark up and Review.
    Review,
}

impl BarPart {
    const ALL: [BarPart; 9] = [BarPart::Views, BarPart::Basis, BarPart::Help, BarPart::Extras, BarPart::Cull, BarPart::Swap, BarPart::Sync, BarPart::Zoom, BarPart::Review];
}

/// Width of the Difference view's Amplify slider and its value box ("×50").
const AMP_RAIL_W: f32 = 64.0;
const AMP_VALUE_W: f32 = 36.0;

/// Width of the toolbar's zoom percentage ("100%").
const ZOOM_LABEL_W: f32 = 46.0;

/// What a mode is for, for its tooltip.
fn mode_tip(m: CmpMode) -> String {
    let what = match m {
        CmpMode::SideBySide => "Side by side: every image in its own pane",
        CmpMode::Split => "Split: A and B in one view with a divider you drag",
        CmpMode::Difference => "Difference: |A - B|, bright where the images differ",
    };
    tip(what, m.command())
}

/// The modes in toolbar order.
const MODES: [CmpMode; 3] = [CmpMode::SideBySide, CmpMode::Split, CmpMode::Difference];

/// How compare starts every time: side by side, zoom & pan linked, same field of view.
pub const START: (CmpMode, bool, Basis) = (CmpMode::SideBySide, true, Basis::Relative);

/// What the toolbar shows at its width (see `Compare::bar_plan`).
#[derive(Debug)]
struct BarPlan {
    count: bool,
    title: bool,
    /// Mode buttons with their names (else icons, names in the tooltips).
    labels: bool,
    /// Sync, Swap, Mark up and Review with their names next to the icons.
    icon_labels: bool,
    /// Difference's "Amplify" word before its slider (the tooltip says it too).
    amp_label: bool,
    /// The last resort: the mode buttons become one menu.
    modes_menu: bool,
    /// Parts moved into the "..." menu.
    hidden: Vec<BarPart>,
    /// Parts not offered at all, not even in the "..." menu: in a blind test
    /// culling, swapping, mark up and review (and the camera strip and
    /// Metadata of the Show menu) would show what is hidden or change the
    /// sides being voted on.
    omitted: Vec<BarPart>,
}

impl BarPlan {
    /// The part is on the toolbar itself.
    fn shown(&self, b: BarPart) -> bool {
        !self.hidden.contains(&b) && !self.omitted.contains(&b)
    }
}

struct Pane {
    path: PathBuf,
    loaded: Option<Arc<LoadedImage>>,
    gpu: Option<GpuImage>,
    /// "Align to A": the image not on screen (the aligned one while
    /// alignment is off, the original while it is on).
    alt_gpu: Option<GpuImage>,
    /// This pane warped into A's frame, and how it was aligned.
    aligned: Option<Arc<RgbaImage>>,
    align: Option<Result<lumen_analysis::align::AlignResult, String>>,
    /// `gpu` holds the aligned image.
    showing_aligned: bool,
    error: Option<String>,
    hist: Option<Histogram>,
    hist_rx: Option<Receiver<Histogram>>,
    exif: crate::bg::BgSlot<PathBuf, Option<crate::meta::ExifSummary>>,
    /// The file is on a drive without a Recycle Bin (network, removable):
    /// deleting it is permanent.
    no_bin: bool,
}

// ---------------------------------------------------------------- culling

/// The per-pane culling button (shown on hover, in the caption row): the
/// same words as the toolbar's Keep.
fn keep_text(i: usize) -> String {
    format!("Keep {}", LETTERS[i.min(3)])
}

/// Between the parts of a pane caption (name, size, file size, zoom).
const CAPTION_SEP: &str = "  ·  ";

/// Colour label used to mark rejected images ("Red").
pub const REJECT_LABEL: u8 = 1;

/// "Keep this, reject others": the kept image gets flagged (and loses a reject
/// label), every other image gets the red "rejected" label and loses its flag.
pub fn keep_reject(tags: &[crate::tags::Tag], keep: usize) -> Vec<crate::tags::Tag> {
    tags.iter()
        .enumerate()
        .map(|(i, t)| {
            let mut t = *t;
            if i == keep {
                t.flagged = true;
                if t.label == REJECT_LABEL {
                    t.label = 0;
                }
            } else {
                t.flagged = false;
                t.label = REJECT_LABEL;
            }
            t
        })
        .collect()
}

/// One-line camera summary: camera, lens, exposure, aperture, ISO, focal length.
pub fn exif_line(e: &crate::meta::ExifSummary) -> String {
    let iso = e.iso.as_ref().map(|v| if v.to_ascii_uppercase().starts_with("ISO") { v.clone() } else { format!("ISO {v}") });
    [e.camera.clone(), e.lens.clone(), e.exposure.clone(), e.aperture.clone(), iso, e.focal.clone()].into_iter().flatten().collect::<Vec<_>>().join("  ·  ")
}

/// Index of the rect containing `pos`.
pub fn pane_at(rects: &[Rect], pos: Pos2) -> Option<usize> {
    rects.iter().position(|r| r.contains(pos))
}

impl Pane {
    fn dims(&self) -> Option<(u32, u32)> {
        self.gpu.as_ref().map(|g| (g.width, g.height))
    }
}

/// A compare opened from a workbench project scene (see `ui::workbench`).
#[derive(Clone, Debug, Default)]
pub struct SceneLink {
    /// Scene index in the project and the number of scenes.
    pub scene: usize,
    pub count: usize,
    pub name: String,
    /// Device name per pane (shown instead of the file name).
    pub labels: Vec<String>,
    pub colors: Vec<[u8; 3]>,
    /// Blind test: names, files and camera data are hidden.
    pub blind: bool,
    /// Blind test progress line ("Trial 7").
    pub progress: String,
    pub done: bool,
    /// The project's own report branding (logo path absolute), if it has one.
    pub branding: Option<lumen_core::report::Branding>,
}

/// The Metadata window was asked for (the app opens it, on the tab used
/// last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoRequest {
    Metadata,
}

/// What a linked compare asks the workbench to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkRequest {
    /// Previous (-1) or next (+1) scene.
    Step(i32),
    ToggleScorecard,
    /// Blind vote: the left pane (0), the right pane (1) or the same (2).
    Vote(u8),
    SetDone(bool),
    /// Leave the blind test.
    EndBlind,
}

pub struct Compare {
    pub paths: Vec<PathBuf>,
    /// Set when the compare shows a project scene.
    pub link: Option<SceneLink>,
    /// Requests for the workbench, taken by the app each frame.
    pub link_requests: Vec<LinkRequest>,
    /// The workbench scorecard panel is shown (the app draws it).
    pub scorecard: bool,
    /// The Show menu asked for the Metadata window.
    pub info_request: Option<InfoRequest>,
    /// Pixel values under the pointer, at the same spot in every pane (V).
    probe: bool,
    /// Line profile tool (P): drag a line; the same line in every pane.
    profile_mode: bool,
    /// The line as fractions of each image (start, end).
    profile: Option<([f32; 2], [f32; 2])>,
    /// Clipping, false color, peaking, one channel or a gain map view in every pane (O).
    inspect: super::inspect_view::InspectMode,
    inspect_cache: super::inspect_view::InspectCache,
    /// Screen headroom of the HDR rendition view, shared by the panes.
    hdr_headroom: f32,
    /// "Align to A" is on (panes B-D show their aligned images).
    align_on: bool,
    align_job: Option<Receiver<AlignMsg>>,
    /// After aligning: zoom every pane to the field of view they share
    /// (fractions of A).
    pending_fov: Option<[f32; 4]>,
    panes: Vec<Pane>,
    views: Vec<View>,
    mode: CmpMode,
    /// How Side by side arranges the panes.
    arrange: PaneLayout,
    sync: bool,
    basis: Basis,
    selected: usize,
    show_hist: bool,
    split: f32,
    amp: f32,
    diff: DiffState,
    help: bool,
    exit: bool,
    last_area: Rect,
    /// Pane under the pointer this frame (culling keys act on it, else on `selected`).
    hover_pane: Option<usize>,
    /// The pane the open context menu acts on.
    menu_pane: usize,
    show_exif: bool,
    /// Pane waiting for the delete confirmation.
    confirm_delete: Option<usize>,
    /// Files deleted from compare mode (the app refreshes views).
    deleted: Vec<PathBuf>,
    /// "Analyze..." was chosen in a pane's context menu.
    analyze: bool,
    /// A menu or list was open when this frame started.
    popup_open: bool,
    /// Marks, comments, details and the report export.
    rv: ReviewUi,
    /// The Analysis panel was open last frame (one right panel at a time).
    analysis_was_open: bool,
    /// Tag changes since the app last asked (notice, each file's tags before):
    /// the app puts them on its undo history and shows the notice.
    tag_changes: Vec<(String, Vec<(PathBuf, crate::tags::Tag)>)>,
    /// Ctrl+Z with no mark to undo: undo the last tag change.
    undo_tags: bool,
}

fn name_of(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

/// `text` shortened in the middle ("scene_0...DeviceY.jpg") to fit `max_w`
/// points in `font`: compared files often share their start, so the end
/// (device, number, extension) stays visible. Empty when not even the dots fit.
pub fn middle_ellipsis(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let w = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    if w(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // About a third of the kept characters for the start, the rest for the end.
    let cut = |keep: usize| -> String {
        let head = keep / 3;
        chars[..head].iter().chain(['.', '.', '.'].iter()).chain(chars[chars.len() - (keep - head)..].iter()).collect()
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let keep = (lo + hi).div_ceil(2);
        if w(&cut(keep)) <= max_w {
            lo = keep;
        } else {
            hi = keep - 1;
        }
    }
    if lo == 0 && w("...") > max_w {
        return String::new();
    }
    cut(lo)
}

fn fmt_size(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

const LETTERS: [&str; 4] = ["A", "B", "C", "D"];

/// Width in samples of the steepest 10%-90% rise (or fall) along `s`:
/// `None` when the line crosses no clear brightness step.
pub fn edge_width(s: &[f32]) -> Option<f32> {
    let n = s.len();
    if n < 4 {
        return None;
    }
    // Light smoothing against noise.
    let sm: Vec<f32> = (0..n).map(|i| (s[i.saturating_sub(1)] + s[i] + s[(i + 1).min(n - 1)]) / 3.0).collect();
    let lo = sm.iter().copied().fold(f32::MAX, f32::min);
    let hi = sm.iter().copied().fold(f32::MIN, f32::max);
    if hi - lo < 20.0 {
        return None;
    }
    let (t10, t90) = (lo + 0.1 * (hi - lo), lo + 0.9 * (hi - lo));
    // The rise: from the last point under 10% before the first point over
    // 90%; or the same for a fall.
    let rising = sm.iter().position(|v| *v <= t10).unwrap_or(0) < sm.iter().position(|v| *v >= t90).unwrap_or(0);
    let cross = |th: f32, above: bool, from: usize| -> Option<f32> {
        (from..n - 1).find_map(|k| {
            let (a, b) = (sm[k], sm[k + 1]);
            let hit = if above { a < th && b >= th } else { a > th && b <= th };
            hit.then(|| k as f32 + (th - a) / (b - a))
        })
    };
    let (a, b) = if rising { (cross(t10, true, 0)?, cross(t90, true, 0)?) } else { (cross(t90, false, 0)?, cross(t10, false, 0)?) };
    (b > a).then_some(b - a)
}

/// Plain words for an alignment confidence.
fn confidence_word(c: f32) -> &'static str {
    if c >= 0.5 {
        "good match"
    } else if c >= 0.2 {
        "rough match"
    } else {
        "uncertain"
    }
}

impl Compare {
    pub fn new(paths: Vec<PathBuf>, _core: &Core) -> Self {
        let paths: Vec<PathBuf> = paths.into_iter().take(4).collect();
        let panes = paths
            .iter()
            .map(|p| Pane { path: p.clone(), loaded: None, gpu: None, alt_gpu: None, aligned: None, align: None, showing_aligned: false, error: None, hist: None, hist_rx: None, exif: Default::default(), no_bin: crate::fileops::is_permanent_delete(std::slice::from_ref(p)) })
            .collect();
        let (mode, sync, basis) = START;
        let views = vec![View::fitted(basis); paths.len()];
        let rv = ReviewUi::new(&paths);
        Compare {
            paths,
            link: None,
            link_requests: Vec::new(),
            scorecard: true,
            info_request: None,
            probe: false,
            profile_mode: false,
            profile: None,
            inspect: super::inspect_view::InspectMode::Off,
            inspect_cache: Default::default(),
            hdr_headroom: f32::INFINITY,
            align_on: false,
            align_job: None,
            pending_fov: None,
            panes,
            views,
            mode,
            arrange: PaneLayout::Auto,
            sync,
            basis,
            selected: 0,
            show_hist: false,
            split: 0.5,
            amp: 4.0,
            diff: DiffState::default(),
            help: false,
            exit: false,
            last_area: Rect::NOTHING,
            hover_pane: None,
            menu_pane: 0,
            show_exif: false,
            confirm_delete: None,
            deleted: Vec::new(),
            analyze: false,
            popup_open: false,
            rv,
            analysis_was_open: false,
            tag_changes: Vec::new(),
            undo_tags: false,
        }
    }

    /// A compare of a workbench project scene: device names on the panes,
    /// the scene bar, and the review kept in the project folder.
    pub fn new_scene(paths: Vec<PathBuf>, core: &Core, link: SceneLink, root: &std::path::Path) -> Self {
        let mut c = Self::new(paths, core);
        c.rv = ReviewUi::new_in(&c.paths, super::review::project_reviews_dir(root), Some(root.to_path_buf()));
        c.link = Some(link);
        c
    }

    /// The metrics table for the report (from the Analysis panel), or `None`.
    pub fn set_metrics_table(&mut self, t: Option<lumen_core::report::Table>) {
        self.rv.metrics = t;
    }

    /// Files deleted while comparing, since the last call.
    pub fn take_deleted(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.deleted)
    }

    /// The pane culling actions apply to: under the pointer, else the selected one.
    fn action_pane(&self) -> Option<usize> {
        if self.panes.is_empty() {
            return None;
        }
        Some(self.hover_pane.filter(|&i| i < self.panes.len()).unwrap_or(self.selected.min(self.panes.len() - 1)))
    }

    /// Tag changes made here since the last call (notice, tags before) and
    /// whether Ctrl+Z asked to undo the last one. The app records them for undo.
    pub fn take_tag_changes(&mut self) -> (Vec<(String, Vec<(PathBuf, crate::tags::Tag)>)>, bool) {
        (std::mem::take(&mut self.tag_changes), std::mem::take(&mut self.undo_tags))
    }

    fn rate(&mut self, core: &mut Core, i: usize, r: u8) {
        let Some(pane) = self.panes.get(i) else { return };
        let before = vec![(pane.path.clone(), core.tags.get(&pane.path))];
        core.tags.update(&pane.path, |t| t.rating = r);
        let msg = if r == 0 { format!("{}: rating cleared", LETTERS[i]) } else { format!("{}: {} star{}", LETTERS[i], r, if r == 1 { "" } else { "s" }) };
        self.tag_changes.push((msg, before));
    }

    fn toggle_flag(&mut self, core: &mut Core, i: usize) {
        let Some(pane) = self.panes.get(i) else { return };
        let before = vec![(pane.path.clone(), core.tags.get(&pane.path))];
        core.tags.update(&pane.path, |t| t.flagged = !t.flagged);
        let on = core.tags.get(&pane.path).flagged;
        self.tag_changes.push((format!("{}: {}", LETTERS[i], if on { "flagged" } else { "flag removed" }), before));
    }

    fn keep_only(&mut self, core: &mut Core, keep: usize) {
        if keep >= self.panes.len() || self.panes.len() < 2 {
            return;
        }
        let tags: Vec<crate::tags::Tag> = self.panes.iter().map(|p| core.tags.get(&p.path)).collect();
        let before: Vec<(PathBuf, crate::tags::Tag)> = self.panes.iter().map(|p| p.path.clone()).zip(tags.iter().copied()).collect();
        for (pane, t) in self.panes.iter().zip(keep_reject(&tags, keep)) {
            core.tags.update(&pane.path, |x| *x = t);
        }
        // The other images' labels and flags were overwritten: the notice can put them back.
        let msg = format!("Kept {} (flagged), marked {} other{} as rejected (red label)", LETTERS[keep], self.panes.len() - 1, if self.panes.len() == 2 { "" } else { "s" });
        self.tag_changes.push((msg, before));
    }

    fn delete_pane(&mut self, core: &mut Core, i: usize) {
        let Some(path) = self.panes.get(i).map(|p| p.path.clone()) else { return };
        match crate::fileops::recycle(std::slice::from_ref(&path)) {
            Ok(outcome) => {
                core.loader.invalidate(&path);
                core.thumbs.forget(&path);
                core.meta.forget(&path);
                core.tags.removed(&path);
                self.drop_pane(i);
                self.deleted.push(path.clone());
                core.toasts.success(match outcome {
                    crate::fileops::TrashOutcome::Recycled => format!("Moved {} to the Recycle Bin", name_of(&path)),
                    crate::fileops::TrashOutcome::DeletedPermanently => format!("Deleted {} (network or removable drive: no Recycle Bin)", name_of(&path)),
                });
                if self.panes.is_empty() {
                    self.exit = true;
                }
            }
            Err(e) => core.toasts.error(format!("Could not delete {}: {e}", name_of(&path))),
        }
    }

    /// What deleting pane `i` does, as its menu item says it.
    fn delete_label(&self, i: usize) -> String {
        let letter = LETTERS[i.min(3)];
        if self.panes.get(i).is_some_and(|p| p.no_bin) {
            format!("Delete {letter} permanently (no Recycle Bin on its drive)")
        } else {
            format!("Move {letter} to the Recycle Bin")
        }
    }

    fn pair_mode(&self) -> bool {
        self.mode != CmpMode::SideBySide && self.panes.len() == 2
    }

    fn set_mode(&mut self, m: CmpMode) {
        if m != CmpMode::SideBySide && self.panes.len() != 2 {
            return;
        }
        self.mode = m;
        if m == CmpMode::Difference {
            self.rv.markup = false;
        }
    }

    /// Mark up works where each image is drawn in its own geometry.
    fn markup_allowed(&self) -> bool {
        !(self.pair_mode() && self.mode == CmpMode::Difference) && !self.panes.iter().any(|p| p.showing_aligned)
    }

    // ------------------------------------------------------------ align to A

    /// Show or hide the aligned images of panes B-D (aligning them first).
    fn toggle_alignment(&mut self, core: &mut Core) {
        if self.align_on {
            self.align_on = false;
            self.show_aligned(false);
            core.toasts.info("Alignment off: the original images are shown");
            return;
        }
        if self.panes.len() < 2 {
            return;
        }
        if self.rv.markup {
            self.rv.markup = false;
        }
        self.align_on = true;
        if self.panes[1..].iter().all(|p| p.align.is_some()) {
            self.show_aligned(true);
            return;
        }
        let Some(a) = self.panes[0].loaded.as_ref().map(|l| l.decoded.image.clone()) else {
            self.align_on = false;
            core.toasts.info("Wait until the images are loaded, then align");
            return;
        };
        let others: Vec<(usize, Arc<RgbaImage>)> = self.panes.iter().enumerate().skip(1).filter_map(|(i, p)| Some((i, p.loaded.as_ref()?.decoded.image.clone()))).collect();
        if others.len() + 1 != self.panes.len() {
            self.align_on = false;
            core.toasts.info("Wait until the images are loaded, then align");
            return;
        }
        let (tx, rx) = unbounded();
        let ctx = core.ctx.clone();
        std::thread::spawn(move || {
            crate::bg::background_priority();
            let (aw, ah) = a.dimensions();
            let mut common: Option<[f64; 4]> = Some([0.0, 0.0, aw as f64, ah as f64]);
            for (i, b) in others {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let opts = lumen_analysis::align::AlignOptions::default();
                    let r = lumen_analysis::align::align_rgba(a.as_raw(), aw, ah, b.as_raw(), b.width(), b.height(), &opts, None).map_err(|e| e.to_string())?;
                    let warped = lumen_analysis::align::warp_rgba(b.as_raw(), b.width(), b.height(), &r.h, aw, ah);
                    let img = RgbaImage::from_raw(aw, ah, warped).ok_or_else(|| "could not build the aligned image".to_string())?;
                    let overlap = lumen_analysis::align::overlap_rect(&r.h, b.width(), b.height(), aw, ah);
                    Ok::<_, String>((Arc::new(img), r, overlap))
                }));
                match res {
                    Ok(Ok((image, result, overlap))) => {
                        common = match (common, overlap) {
                            (Some(c), Some(o)) => {
                                let r = [c[0].max(o[0]), c[1].max(o[1]), c[2].min(o[2]), c[3].min(o[3])];
                                (r[2] > r[0] && r[3] > r[1]).then_some(r)
                            }
                            _ => None,
                        };
                        let pyramid = render::build_pyramid(image.clone());
                        let _ = tx.send(AlignMsg::Pane { i, image, pyramid, result });
                    }
                    Ok(Err(error)) => {
                        let _ = tx.send(AlignMsg::Failed { i, error });
                    }
                    Err(_) => {
                        let _ = tx.send(AlignMsg::Failed { i, error: "the alignment failed unexpectedly".into() });
                    }
                }
                ctx.request_repaint();
            }
            let rel = common.map(|c| [(c[0] / aw as f64) as f32, (c[1] / ah as f64) as f32, (c[2] / aw as f64) as f32, (c[3] / ah as f64) as f32]);
            let _ = tx.send(AlignMsg::Done { common: rel });
            ctx.request_repaint();
        });
        self.align_job = Some(rx);
        core.toasts.info("Aligning the images to A...");
    }

    /// Swap the aligned images in (or the originals back).
    fn show_aligned(&mut self, on: bool) {
        for p in self.panes.iter_mut().skip(1) {
            if p.showing_aligned != on && p.alt_gpu.is_some() {
                std::mem::swap(&mut p.gpu, &mut p.alt_gpu);
                p.showing_aligned = on;
            }
        }
        // The difference view recomputes with the shown images.
        self.diff = DiffState::default();
    }

    /// Forget the alignment (A changed: swap, remove).
    fn reset_alignment(&mut self) {
        self.show_aligned(false);
        for p in &mut self.panes {
            p.alt_gpu = None;
            p.aligned = None;
            p.align = None;
        }
        self.align_on = false;
        self.align_job = None;
        self.pending_fov = None;
    }

    fn poll_alignment(&mut self, core: &mut Core) {
        let Some(rx) = &self.align_job else { return };
        let mut msgs = Vec::new();
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        let disconnected = msgs.is_empty() && matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected));
        for m in msgs {
            match m {
                AlignMsg::Pane { i, image, pyramid, result } => {
                    if let Some(p) = self.panes.get_mut(i) {
                        p.alt_gpu = Some(GpuImage::new(&core.ctx, &format!("cmp-aligned:{}", p.path.display()), pyramid));
                        p.aligned = Some(image);
                        p.align = Some(Ok(result));
                    }
                }
                AlignMsg::Failed { i, error } => {
                    if let Some(p) = self.panes.get_mut(i) {
                        p.align = Some(Err(error));
                    }
                }
                AlignMsg::Done { common } => {
                    self.align_job = None;
                    if self.align_on {
                        self.show_aligned(true);
                        self.pending_fov = common;
                    }
                    let parts: Vec<String> = self
                        .panes
                        .iter()
                        .enumerate()
                        .skip(1)
                        .map(|(i, p)| match &p.align {
                            Some(Ok(r)) => format!("{} {:.2}x, {}", LETTERS[i.min(3)], r.scale, confidence_word(r.confidence)),
                            Some(Err(e)) => format!("{}: {e}", LETTERS[i.min(3)]),
                            None => format!("{}: not aligned", LETTERS[i.min(3)]),
                        })
                        .collect();
                    if self.panes.iter().skip(1).all(|p| matches!(p.align, Some(Err(_)))) {
                        self.align_on = false;
                        core.toasts.error(format!("Could not align: {}", parts.join("; ")));
                    } else {
                        core.toasts.success(format!("Aligned to A ({}). Every pane shows the field of view they share.", parts.join("; ")));
                    }
                    return;
                }
            }
        }
        if disconnected {
            self.align_job = None;
            self.align_on = false;
        } else {
            core.ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn blind(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.blind)
    }

    fn toggle_markup(&mut self, core: &mut Core) {
        if self.blind() {
            return;
        }
        if !self.rv.markup && !self.markup_allowed() {
            core.toasts.info("Mark up works in Side by side and Split");
            return;
        }
        self.rv.markup = !self.rv.markup;
    }

    /// The Review panel; it replaces the Analysis panel (one right panel at a time).
    fn toggle_review(&mut self, core: &mut Core) {
        if self.blind() {
            return;
        }
        self.rv.panel = !self.rv.panel;
        if self.rv.panel && core.analysis_open {
            core.analysis_request = Some(crate::analysis_ui::PanelRequest::Toggle);
        }
    }

    // ------------------------------------------------------------ loading

    fn update_loading(&mut self, core: &mut Core) {
        let mut keep = Vec::new();
        for pane in &mut self.panes {
            if pane.gpu.is_none() && pane.error.is_none() {
                match core.loader.state(&pane.path) {
                    Some(LoadState::Ready(_)) => {
                        if let Some(img) = core.loader.get_ready(&pane.path) {
                            pane.gpu = Some(GpuImage::new(&core.ctx, &format!("cmp:{}", pane.path.display()), img.pyramid.clone()));
                            pane.loaded = Some(img);
                        }
                    }
                    Some(LoadState::Failed(e)) => pane.error = Some(e.clone()),
                    _ => {
                        core.loader.request(&pane.path, 0, None);
                        keep.push(pane.path.clone());
                    }
                }
            }
            if self.show_hist && pane.hist.is_none() && pane.hist_rx.is_none() {
                if let Some(l) = &pane.loaded {
                    let img = l.decoded.image.clone();
                    let (tx, rx) = unbounded();
                    let ctx = core.ctx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(Histogram::compute(&img));
                        ctx.request_repaint();
                    });
                    pane.hist_rx = Some(rx);
                }
            }
            if let Some(rx) = &pane.hist_rx {
                if let Ok(h) = rx.try_recv() {
                    pane.hist = Some(h);
                    pane.hist_rx = None;
                }
            }
        }
        if !keep.is_empty() {
            core.loader.retain_queued(&keep);
        }
    }

    fn update_diff(&mut self, ctx: &egui::Context) {
        if self.mode != CmpMode::Difference || self.panes.len() != 2 {
            return;
        }
        let (Some(a), Some(b)) = (self.panes[0].loaded.clone(), self.panes[1].loaded.clone()) else { return };
        let aligned_b = self.panes[1].aligned.clone().filter(|_| self.panes[1].showing_aligned);
        let key = (self.panes[0].path.clone(), self.panes[1].path.clone());
        if self.diff.key.as_ref() != Some(&key) {
            self.diff = DiffState { key: Some(key), ..Default::default() };
            let (tx, rx) = unbounded();
            let amp = self.amp;
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let ia = level_under(&a.pyramid, 40_000_000);
                    let mut notes = Vec::new();
                    if ia.width() != a.decoded.image.width() {
                        notes.push(format!("computed at {}×{}", ia.width(), ia.height()));
                    }
                    let ib = if let Some(al) = aligned_b {
                        notes.push("B aligned to A".to_string());
                        if al.dimensions() == ia.dimensions() {
                            al
                        } else {
                            Arc::new(crate::imaging::resize_exact(&al, ia.width(), ia.height(), crate::imaging::Filter::Bilinear))
                        }
                    } else if b.decoded.image.dimensions() == ia.dimensions() {
                        b.decoded.image.clone()
                    } else {
                        // Use the smallest level that is still at least as large, then resample.
                        let src = b.pyramid.iter().rev().find(|l| l.width() >= ia.width() && l.height() >= ia.height()).cloned().unwrap_or_else(|| b.pyramid[0].clone());
                        if b.decoded.image.dimensions() != a.decoded.image.dimensions() {
                            notes.push("B resampled to A's size".to_string());
                        }
                        if src.dimensions() == ia.dimensions() {
                            src
                        } else {
                            Arc::new(crate::imaging::resize_exact(&src, ia.width(), ia.height(), crate::imaging::Filter::Bilinear))
                        }
                    };
                    let (raw, stats) = difference(&ia, &ib);
                    let raw = Arc::new(raw);
                    let pyramid = render::build_pyramid(Arc::new(amplify(&raw, amp)));
                    DiffMsg::Done { raw, stats, amp, pyramid, note: if notes.is_empty() { None } else { Some(notes.join(" · ")) } }
                }));
                if let Ok(msg) = res {
                    let _ = tx.send(msg);
                }
                ctx2.request_repaint();
            });
            self.diff.busy = Some(rx);
        }
        if let Some(rx) = &self.diff.busy {
            match rx.try_recv() {
                Ok(DiffMsg::Done { raw, stats, amp, pyramid, note }) => {
                    self.diff.raw = Some(raw);
                    self.diff.stats = Some(stats);
                    self.diff.note = note;
                    self.diff.gpu = Some((amp, GpuImage::new(ctx, "cmp:diff", pyramid)));
                    self.diff.busy = None;
                }
                Ok(DiffMsg::Amplified { amp, pyramid }) => {
                    self.diff.gpu = Some((amp, GpuImage::new(ctx, "cmp:diff", pyramid)));
                    self.diff.busy = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.diff.busy = None;
                    self.diff.note = Some("Could not compute the difference".into());
                }
                Err(_) => {}
            }
        }
        // Re-amplify when the slider moved.
        if self.diff.busy.is_none() {
            if let (Some(raw), Some((amp, _))) = (&self.diff.raw, &self.diff.gpu) {
                if (*amp - self.amp).abs() > 1e-3 {
                    let raw = raw.clone();
                    let amp = self.amp;
                    let (tx, rx) = unbounded();
                    let ctx2 = ctx.clone();
                    std::thread::spawn(move || {
                        let pyramid = render::build_pyramid(Arc::new(amplify(&raw, amp)));
                        let _ = tx.send(DiffMsg::Amplified { amp, pyramid });
                        ctx2.request_repaint();
                    });
                    self.diff.busy = Some(rx);
                }
            }
        }
    }

    // ------------------------------------------------------------ view ops

    fn fit_ref(&self, rects: &[Rect], ppp: f32) -> f32 {
        let mut f = f32::INFINITY;
        for (i, p) in self.panes.iter().enumerate() {
            if let (Some(d), Some(r)) = (p.dims(), rects.get(i).or(rects.first())) {
                f = f.min(fit_of(d, r.size() * ppp));
            }
        }
        if f.is_finite() {
            f
        } else {
            1.0
        }
    }

    /// Apply `op` computed on pane `src` to it (and to all panes when synced).
    fn apply(&mut self, src: usize, op: impl Fn(&View) -> View) {
        let v = op(&self.views[src]);
        if self.sync {
            for w in &mut self.views {
                *w = v;
            }
        } else {
            self.views[src] = v;
        }
    }

    fn fit_all(&mut self) {
        let b = self.basis;
        for v in &mut self.views {
            *v = View::fitted(b);
        }
    }

    fn set_basis(&mut self, b: Basis) {
        if b != self.basis {
            self.basis = b;
            self.fit_all();
        }
    }

    /// Rect each pane's image is laid out in (single shared rect for pair
    /// modes). Everything that maps the panes to the screen uses this.
    fn layout(&self, area: Rect) -> Vec<Rect> {
        if !area.is_positive() {
            return Vec::new();
        }
        if self.pair_mode() {
            vec![area; self.panes.len()]
        } else {
            grid_rects(self.panes.len(), self.grid(area), area, 1.0)
        }
    }

    /// The cells Side by side uses in `area`: the chosen layout, or Auto's.
    fn grid(&self, area: Rect) -> Grid {
        self.arrange.grid(self.panes.len()).unwrap_or_else(|| self.auto_layout(area))
    }

    /// The cells Auto picks for the images' shapes in `area` (4:3 for an
    /// image still loading).
    fn auto_layout(&self, area: Rect) -> Grid {
        let aspects: Vec<f32> = self.panes.iter().map(|p| p.dims().map_or(4.0 / 3.0, |(w, h)| w as f32 / h.max(1) as f32)).collect();
        auto_grid(&aspects, area.size(), 1.0)
    }

    /// "Match framing" and "Match pixels" differ only for images of
    /// different sizes, and not in Difference (one image on screen): the
    /// choice is offered only then.
    fn basis_matters(&self) -> bool {
        if self.pair_mode() && self.mode == CmpMode::Difference {
            return false;
        }
        let mut dims = self.panes.iter().filter_map(|p| p.dims());
        let Some(first) = dims.next() else { return false };
        dims.any(|d| d != first)
    }

    fn effective(&self, i: usize, rects: &[Rect], ppp: f32, fit_ref: f32) -> Option<(f32, Vec2, (u32, u32))> {
        let d = self.panes.get(i)?.dims()?;
        let r = rects.get(i)?;
        let (z, c) = pane_transform(&self.views[i], self.basis, d, r.size() * ppp, fit_ref);
        Some((z, c, d))
    }

    fn zoom_step(&mut self, src: usize, rects: &[Rect], ppp: f32, factor: f32, anchor_pt: Option<Pos2>) {
        let (Some(d), Some(&r)) = (self.panes.get(src).and_then(|p| p.dims()), rects.get(src)) else { return };
        let fr = self.fit_ref(rects, ppp);
        let basis = self.basis;
        let anchor = anchor_pt.map(|a| (a - r.center()) * ppp).unwrap_or(Vec2::ZERO);
        self.apply(src, |v| zoom_at(v, basis, d, r.size() * ppp, fr, factor, anchor));
    }

    fn actual_size(&mut self, rects: &[Rect], ppp: f32) {
        let src = self.selected.min(self.panes.len().saturating_sub(1));
        let Some((z, _, _)) = self.effective(src, rects, ppp, self.fit_ref(rects, ppp)) else { return };
        self.zoom_step(src, rects, ppp, 1.0 / z, None);
    }

    /// What the Analysis panel works on: panes A and B.
    pub fn analysis_subject(&self) -> crate::analysis_ui::Subject {
        use crate::analysis_ui::{Pixels, Subject, SubjectImage, SubjectKind};
        let img = |p: &Pane| SubjectImage {
            path: p.path.clone(),
            size: p.loaded.as_ref().map(|l| l.decoded.image.dimensions()),
            pixels: p.loaded.as_ref().map(|l| Pixels::Ready(l.decoded.image.clone())).unwrap_or(Pixels::Pending),
            frame: None,
        };
        Subject { kind: SubjectKind::Pair, a: self.panes.first().map(img), b: self.panes.get(1).map(img) }
    }

    /// Adds review marks (pane index, mark in that pane's image pixels), for
    /// example AI findings; marks already there and blind tests are skipped.
    /// Returns how many were added.
    pub fn add_marks(&mut self, marks: Vec<(usize, super::review::Mark)>) -> usize {
        if self.blind() {
            return 0;
        }
        let mut added = 0;
        for (pane, m) in marks {
            // Marks belong to the original image, not the aligned view.
            if self.panes.get(pane).is_some_and(|p| p.showing_aligned) {
                continue;
            }
            if let Some(im) = self.rv.review.images.get_mut(pane) {
                if !im.marks.contains(&m) {
                    im.marks.push(m);
                    added += 1;
                }
            }
        }
        if added > 0 {
            self.rv.touch();
        }
        added
    }

    /// Shows `r` (fractions 0..1 of pane `i`'s image) in every synced pane.
    pub fn zoom_to_region(&mut self, i: usize, r: [f32; 4], ppp: f32) {
        let rects = self.layout(self.last_area);
        let (Some(d), Some(&pane)) = (self.panes.get(i).and_then(|p| p.dims()), rects.get(i)) else { return };
        let (w, h) = (d.0 as f32, d.1 as f32);
        let (rw, rh) = (((r[2] - r[0]) * w).max(16.0), ((r[3] - r[1]) * h).max(16.0));
        let z = ((pane.width() * ppp / rw).min(pane.height() * ppp / rh) * 0.6).min(8.0);
        let c = Vec2::new((r[0] + r[2]) / 2.0 * w, (r[1] + r[3]) / 2.0 * h);
        let basis = self.basis;
        self.apply(i, |_| from_effective(basis, d, pane.size() * ppp, z, c));
    }

    /// The files for the EXIF / Metadata window: (label, path, badge color).
    pub fn info_files(&self, core: &Core) -> Vec<(String, PathBuf, Color32)> {
        self.panes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let label = match self.pane_label(i) {
                    Some(l) => format!("{}  {l}", LETTERS[i.min(3)]),
                    None => LETTERS[i.min(3)].to_string(),
                };
                (label, p.path.clone(), self.pane_color(core, i))
            })
            .collect()
    }

    /// Pane `i` (and linked panes) fit region `r` (fractions) exactly.
    fn zoom_to_region_exact(&mut self, i: usize, r: [f32; 4], ppp: f32) {
        let rects = self.layout(self.last_area);
        let (Some(d), Some(&pane)) = (self.panes.get(i).and_then(|p| p.dims()), rects.get(i)) else { return };
        let (w, h) = (d.0 as f32, d.1 as f32);
        let (rw, rh) = (((r[2] - r[0]) * w).max(16.0), ((r[3] - r[1]) * h).max(16.0));
        let z = (pane.width() * ppp / rw).min(pane.height() * ppp / rh).min(8.0);
        let c = Vec2::new((r[0] + r[2]) / 2.0 * w, (r[1] + r[3]) / 2.0 * h);
        let basis = self.basis;
        self.apply(i, |_| from_effective(basis, d, pane.size() * ppp, z, c));
    }

    /// Every pane shows detail `k` (the same fractions of each image).
    fn zoom_to_detail(&mut self, k: usize, ppp: f32) {
        let Some(r) = self.rv.review.details.get(k).map(|d| d.rect) else { return };
        if self.sync && self.basis == Basis::Relative {
            self.zoom_to_region(0, r, ppp);
            return;
        }
        let sync = std::mem::replace(&mut self.sync, false);
        for i in 0..self.panes.len() {
            self.zoom_to_region(i, r, ppp);
        }
        self.sync = sync;
    }

    fn pan(&mut self, src: usize, rects: &[Rect], ppp: f32, delta_pt: Vec2) {
        let (Some(d), Some(&r)) = (self.panes.get(src).and_then(|p| p.dims()), rects.get(src)) else { return };
        let fr = self.fit_ref(rects, ppp);
        let basis = self.basis;
        self.apply(src, |v| pan_by(v, basis, d, r.size() * ppp, fr, delta_pt * ppp));
    }

    /// Take pane `i` out of the comparison (the remove button; at least two stay).
    fn remove_pane(&mut self, i: usize) {
        if self.panes.len() <= 2 {
            return;
        }
        self.drop_pane(i);
    }

    /// Everything that goes with pane `i` when it leaves (removed or its file
    /// deleted): its view and path, its device name and color in a project
    /// scene, and the alignment, whose results are kept by pane index (a
    /// running alignment is dropped, so none lands on the wrong pane).
    fn drop_pane(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        self.reset_alignment();
        self.panes.remove(i);
        self.views.remove(i);
        self.paths.remove(i);
        if let Some(l) = &mut self.link {
            if i < l.labels.len() {
                l.labels.remove(i);
            }
            if i < l.colors.len() {
                l.colors.remove(i);
            }
        }
        self.rv.set_paths(&self.paths);
        self.diff = DiffState::default();
        self.selected = self.selected.min(self.panes.len().saturating_sub(1));
        self.hover_pane = None;
        if self.panes.len() != 2 {
            self.mode = CmpMode::SideBySide;
        }
    }

    fn swap_ab(&mut self) {
        if self.panes.len() >= 2 {
            self.reset_alignment();
            self.panes.swap(0, 1);
            self.views.swap(0, 1);
            self.paths.swap(0, 1);
            if let Some(l) = &mut self.link {
                if l.labels.len() >= 2 {
                    l.labels.swap(0, 1);
                }
                if l.colors.len() >= 2 {
                    l.colors.swap(0, 1);
                }
            }
            self.rv.set_paths(&self.paths);
        }
    }

    // ------------------------------------------------------------ input

    fn keys(&mut self, ui: &Ui, core: &mut Core, rects: &[Rect], ppp: f32) {
        if self.confirm_delete.is_some() || self.rv.dialog_open() || ui.ctx().egui_wants_keyboard_input() || keymap::capturing() {
            return; // the dialog (a text field, the shortcut editor) owns the keyboard
        }
        // Fixed keys: Del on the selected mark, Esc out of mark up.
        let rv_esc = self.rv.keys(ui);
        // Esc with a menu or a list open only closes it (egui does).
        if !rv_esc && !self.popup_open && !egui::Popup::is_any_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            if self.help {
                self.help = false;
            } else {
                self.exit = true;
            }
        }
        // Everything else comes from the keymap (Settings, Keyboard, Compare).
        // A key whose command does not apply now passes on, as it always did.
        let km = keymap::active();
        let (blind, markup) = (self.blind(), self.rv.markup);
        let fired = ui.input_mut(|i| {
            keymap::consume(i, |key, mods| {
                // Space is held to pan while marking up.
                if markup && key == Key::Space {
                    return None;
                }
                // The blind test's votes win over Compare's keys.
                if let Some(c) = km.lookup_lenient(KeyMode::Blind, key, mods).filter(|_| blind) {
                    return Some(c);
                }
                km.lookup_lenient(KeyMode::Compare, key, mods).filter(|c| self.takes_key(*c, rects))
            })
        });
        for c in fired {
            self.exec(c, core, rects, ppp);
        }
    }

    /// Whether a key bound to `cmd` is used now: scene keys only in a
    /// project, Tab and Space only in Split, culling keys only with a pane.
    fn takes_key(&self, cmd: Command, rects: &[Rect]) -> bool {
        use Command::*;
        if self.blind() && blind_off(cmd) {
            return true; // taken and dropped
        }
        match cmd {
            Rate(_) | ToggleFlag | KeepOnly | Delete => self.action_pane().is_some(),
            NextScene | PrevScene => self.link.is_some(),
            ToggleScorecard => self.link.is_some() && !self.blind(),
            JumpDivider => self.pair_mode() && self.mode == CmpMode::Split,
            PanLeft | PanRight | PanUp | PanDown => rects.get(self.selected.min(self.panes.len().saturating_sub(1))).is_some(),
            VoteLeft | VoteRight | VoteSame => self.blind(),
            _ => true,
        }
    }

    /// Do `cmd` (from its key, the command palette or a menu). False when it
    /// does not apply here (no project scene, a blind test, no pane...).
    fn exec(&mut self, cmd: Command, core: &mut Core, rects: &[Rect], ppp: f32) -> bool {
        use Command::*;
        if self.blind() && blind_off(cmd) {
            return false;
        }
        let src = self.selected.min(self.panes.len().saturating_sub(1));
        match cmd {
            NextScene | PrevScene if self.link.is_some() => self.link_requests.push(LinkRequest::Step(if cmd == NextScene { 1 } else { -1 })),
            ToggleScorecard if self.link.is_some() => self.link_requests.push(LinkRequest::ToggleScorecard),
            VoteLeft | VoteRight | VoteSame if self.blind() => self.link_requests.push(LinkRequest::Vote(match cmd {
                VoteLeft => 0,
                VoteRight => 1,
                _ => 2,
            })),
            // Culling acts on the pane under the pointer (else the selected one).
            Rate(_) | ToggleFlag | KeepOnly | Delete => {
                let Some(t) = self.action_pane() else { return false };
                match cmd {
                    Rate(r) => self.rate(core, t, r.min(5)),
                    ToggleFlag => self.toggle_flag(core, t),
                    KeepOnly => self.keep_only(core, t),
                    _ => self.confirm_delete = Some(t),
                }
            }
            JumpDivider if self.pair_mode() && self.mode == CmpMode::Split => self.split = if self.split < 0.5 { 1.0 } else { 0.0 },
            ZoomIn => self.zoom_step(src, rects, ppp, 1.25, None),
            ZoomOut => self.zoom_step(src, rects, ppp, 0.8, None),
            ZoomFit => self.fit_all(),
            ZoomActual => self.actual_size(rects, ppp),
            PanLeft | PanRight | PanUp | PanDown => {
                let Some(r) = rects.get(src) else { return false };
                let step = r.size() * 0.1;
                let d = match cmd {
                    PanLeft => Vec2::new(step.x, 0.0),
                    PanRight => Vec2::new(-step.x, 0.0),
                    PanUp => Vec2::new(0.0, step.y),
                    _ => Vec2::new(0.0, -step.y),
                };
                self.pan(src, rects, ppp, d);
            }
            ViewSideBySide | ViewSplit | ViewDifference => self.set_mode(MODES.into_iter().find(|m| m.command() == cmd).unwrap_or(CmpMode::SideBySide)),
            ToggleCameraInfo => self.show_exif = !self.show_exif,
            ToggleHistogram => self.show_hist = !self.show_hist,
            ToggleLink => self.sync = !self.sync,
            SwapAB => self.swap_ab(),
            AlignToA => self.toggle_alignment(core),
            TogglePixelValues => self.probe = !self.probe,
            LineProfile => {
                self.profile_mode = !self.profile_mode;
                if self.profile_mode {
                    self.rv.markup = false;
                    core.toasts.info("Line profile: drag across an edge; every pane shows the same line");
                } else {
                    self.profile = None;
                }
            }
            CycleInspect => {
                let gain_maps = !self.panes.iter().all(|p| self.inspect_cache.lacks_gain_map(&p.path));
                self.inspect = self.inspect.next_for(gain_maps);
                core.toasts.info(format!("Inspect view: {}. {}", self.inspect.label(), self.inspect.tip()));
            }
            ToggleMarkup => self.toggle_markup(core),
            ToggleReview => self.toggle_review(core),
            CompareHelp => self.help = !self.help,
            // With no mark to undo, Ctrl+Z puts back the last rating, flag or Keep.
            Undo if !self.rv.can_undo() => self.undo_tags = true,
            Undo => self.rv.undo(),
            Redo => self.rv.redo(),
            ExportReport => self.rv.open_export(),
            _ => return false,
        }
        true
    }

    /// A command from the command palette or a menu. True when it is one of
    /// Compare's, done or explained in a note when it does not apply here.
    pub fn run_command(&mut self, cmd: Command, core: &mut Core) -> bool {
        let own = Command::COMPARE.contains(&cmd);
        if !own && !keymap::commands_of(KeyMode::Compare).contains(&cmd) {
            return false;
        }
        let rects = self.layout(self.last_area);
        let ppp = core.ctx.pixels_per_point();
        if !self.exec(cmd, core, &rects, ppp) && own {
            let why = match cmd {
                _ if self.blind() => "Not during a blind test",
                Command::NextScene | Command::PrevScene | Command::ToggleScorecard => "This works on a scene of a project (Workbench)",
                Command::VoteLeft | Command::VoteRight | Command::VoteSame => "This works during a blind test",
                Command::JumpDivider => "This works in the Split view",
                _ => "Open two or more images first",
            };
            core.toasts.info(why);
        }
        true
    }

    // ------------------------------------------------------------ UI

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let p = core.pal;
        self.update_loading(core);
        self.poll_alignment(core);
        self.update_diff(ui.ctx());
        let ppp = ui.ctx().pixels_per_point();
        // Before the toolbar runs: its menus close themselves on Esc.
        self.popup_open = egui::Popup::is_any_open(ui.ctx());

        egui::Panel::top("compare-toolbar")
            .resizable(false)
            .exact_size(size::TOOLBAR)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| self.toolbar(ui, core));
        if self.link.is_some() {
            egui::Panel::top("compare-scene")
                .resizable(false)
                .exact_size(36.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 0)))
                .show(ui, |ui| self.scene_bar(ui, &p));
        }
        if self.blind() {
            self.show_exif = false;
            self.rv.panel = false;
            self.rv.markup = false;
        }

        // One right panel at a time: the Analysis panel opening closes Review.
        if core.analysis_open && !self.analysis_was_open {
            self.rv.panel = false;
        }
        self.analysis_was_open = core.analysis_open;
        if std::mem::take(&mut self.rv.restored) {
            core.toasts.info("Your review of these images was restored (marks and comments)");
        }
        self.rv.tick(ui.ctx(), &mut core.toasts);
        if self.rv.panel {
            let infos: Vec<review_ui::PaneInfo> = (0..self.panes.len()).map(|i| review_ui::PaneInfo { letter: LETTERS[i.min(3)], name: self.pane_label(i).unwrap_or_else(|| name_of(&self.panes[i].path)), color: self.pane_color(core, i) }).collect();
            // At most 40% of the window, like the Analysis panel.
            let max = (ui.ctx().content_rect().width() * 0.4).clamp(280.0, 560.0);
            let out = egui::Panel::right("compare-review")
                .resizable(true)
                .default_size(330.0_f32.min(max))
                .size_range(280.0..=max)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin { left: 12, right: 4, top: 10, bottom: 8 }))
                .show(ui, |ui| self.rv.panel(ui, &p, &infos))
                .inner;
            if out.close {
                self.rv.panel = false;
            }
            if let Some(k) = out.zoom_detail {
                self.zoom_to_detail(k, ppp);
            }
        }
        if self.rv.markup && self.markup_allowed() {
            let dims: Vec<Option<(u32, u32)>> = self.panes.iter().map(|p| p.dims()).collect();
            egui::Panel::top("compare-markup")
                .resizable(false)
                .exact_size(40.0)
                .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 0)))
                .show(ui, |ui| self.rv.strip(ui, &p, &dims));
        }

        let area = ui.available_rect_before_wrap();
        self.last_area = area;
        let bgc = core.settings.viewer_background;
        ui.painter().rect_filled(area, 0.0, Color32::from_rgb(bgc[0], bgc[1], bgc[2]));
        ui.painter().line_segment([area.left_top(), area.right_top()], Stroke::new(1.0, p.border));
        let rects = self.layout(area);
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| area.contains(*p));
        self.hover_pane = pointer.and_then(|pos| match self.mode {
            _ if !self.pair_mode() => pane_at(&rects, pos),
            CmpMode::Split => Some(if pos.x < area.min.x + area.width() * self.split { 0 } else { 1 }),
            _ => None,
        });
        self.keys(ui, core, &rects, ppp);
        if self.exit {
            return false;
        }
        let rects = self.layout(area);
        if rects.len() != self.panes.len() {
            return true;
        }
        if let Some(fov) = self.pending_fov.take() {
            // Linked panes all follow A to the shared field of view.
            self.sync = true;
            self.zoom_to_region_exact(0, fov, ppp);
        }

        if self.pair_mode() {
            self.pair_view(ui, core, area, &rects, ppp);
        } else {
            self.side_by_side(ui, core, &rects, ppp);
        }
        if self.help {
            self.help_overlay(ui, area);
        }
        self.delete_dialog(ui.ctx(), core);
        let panes = &self.panes;
        let project_branding = self.link.as_ref().and_then(|l| l.branding.as_ref());
        self.rv.export_dialog(ui.ctx(), &p, &mut core.settings.report_branding, project_branding, || {
            panes
                .iter()
                .enumerate()
                .map(|(i, pn)| review_ui::ExportSource { letter: LETTERS[i.min(3)], path: pn.path.clone(), image: pn.loaded.as_ref().map(|l| l.decoded.image.clone()), file_size: pn.loaded.as_ref().map(|l| l.file_size).unwrap_or(0) })
                .collect()
        });
        if std::mem::take(&mut self.analyze) {
            core.analysis_request = Some(crate::analysis_ui::PanelRequest::Open);
        }
        !self.exit
    }

    fn delete_dialog(&mut self, ctx: &egui::Context, core: &mut Core) {
        let Some(i) = self.confirm_delete else { return };
        let Some((path, permanent)) = self.panes.get(i).map(|p| (p.path.clone(), p.no_bin)) else {
            self.confirm_delete = None;
            return;
        };
        // As in the browser: ask when the setting says so, and always when the
        // file cannot go to the Recycle Bin (network and removable drives).
        if !core.settings.confirm_delete && !permanent {
            self.confirm_delete = None;
            self.delete_pane(core, i);
            return;
        }
        let p = core.pal;
        let mut decision = None;
        widgets::modal(ctx, egui::Id::NULL.with("cmp-delete"), 400.0, |ui| {
            widgets::modal_title(ui, if permanent { "Delete permanently?" } else { "Move to Recycle Bin?" });
            ui.label(RichText::new(format!("{}  {}", LETTERS[i.min(3)], name_of(&path))).color(p.text));
            ui.add_space(4.0);
            widgets::modal_body(
                ui,
                if permanent {
                    "The image is removed from the comparison. This drive has no Recycle Bin, so this cannot be undone."
                } else {
                    "The image is removed from the comparison. You can restore it from the Recycle Bin."
                },
            );
            if let Some(r) = widgets::dialog_buttons(ui, &p, "Cancel", if permanent { "Delete permanently" } else { "Move to Recycle Bin" }, widgets::ButtonKind::Danger, true) {
                decision = Some(r);
            }
            // The Recycle Bin can undo it, so Enter confirms (as Delete does in
            // Explorer); a permanent delete takes the button.
            if !permanent && ui.input(|i| i.key_pressed(Key::Enter)) {
                decision = Some(true);
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
        });
        match decision {
            Some(true) => {
                self.confirm_delete = None;
                self.delete_pane(core, i);
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    fn view_input(&mut self, ui: &Ui, core: &mut Core, resp: &egui::Response, src: usize, rects: &[Rect], ppp: f32, geos: &[Option<PaneGeo>]) {
        // Line profile: the left button draws the line (Space or the middle button pans).
        if self.profile_mode && !ui.input(|i| i.key_down(Key::Space)) {
            if let (Some(g), Some(pos)) = (geos.get(src).and_then(|g| g.as_ref()), ui.input(|i| i.pointer.interact_pos())) {
                let px = super::review::to_image(g.dest, g.dims, pos);
                let rel = [(px[0] / g.dims.0.max(1) as f32).clamp(0.0, 1.0), (px[1] / g.dims.1.max(1) as f32).clamp(0.0, 1.0)];
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    self.profile = Some((rel, rel));
                } else if resp.dragged_by(egui::PointerButton::Primary) {
                    if let Some(l) = &mut self.profile {
                        l.1 = rel;
                    }
                }
            }
            if resp.dragged_by(egui::PointerButton::Primary) || resp.drag_started_by(egui::PointerButton::Primary) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                return;
            }
        }
        // Marking up: the left button draws (Space or the middle button pans).
        let marking = self.rv.markup && self.markup_allowed() && !ui.input(|i| i.key_down(Key::Space));
        let hover = if self.pair_mode() { self.hover_pane } else { Some(src) };
        let draws = marking && self.rv.input(ui, resp, hover, geos);
        let selecting = !marking || self.rv.tool == review_ui::Tool::Select;
        if resp.double_clicked() && selecting {
            self.fit_all();
        } else if resp.clicked() {
            self.selected = src;
        }
        if (resp.dragged_by(egui::PointerButton::Primary) && !draws) || resp.dragged_by(egui::PointerButton::Middle) {
            let d = resp.drag_delta();
            if d != Vec2::ZERO {
                self.pan(src, rects, ppp, d);
            }
        }
        if resp.hovered() {
            let wd = ui.input(super::wheel_delta);
            if wd != 0.0 {
                let factor = (1.0 + 0.0015 * wd.abs()).min(1.6);
                let factor = if wd > 0.0 { factor } else { 1.0 / factor };
                let anchor = ui.input(|i| i.pointer.hover_pos());
                self.zoom_step(src, rects, ppp, factor, anchor);
            }
            // Touch / gesture pinch only: egui's zoom_delta() also contains the
            // (smoothed) ctrl+wheel, which the wheel branch already handled.
            let pinch = ui.input(|i| super::viewer::scroll_input(i).pinch);
            if wd == 0.0 && (pinch - 1.0).abs() > 1e-4 {
                let anchor = ui.input(|i| i.pointer.hover_pos());
                self.zoom_step(src, rects, ppp, pinch, anchor);
            }
        }
        if resp.dragged() && !draws {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        // The pane the menu was opened on (in Split: the one under the
        // pointer then), kept while the pointer moves into the menu.
        if resp.secondary_clicked() {
            self.menu_pane = self.hover_pane.unwrap_or(src);
        }
        let menu_pane = self.menu_pane;
        if !self.blind() {
            resp.context_menu(|ui| self.pane_menu(ui, core, menu_pane));
        }
    }

    /// Right-click menu of the panes.
    fn pane_menu(&mut self, ui: &mut Ui, core: &mut Core, pane: usize) {
        ui.set_min_width(240.0);
        let item = |ui: &mut Ui, icon: &str, label: &str, key: &str| {
            let r = widgets::menu_item(ui, icon, label, key).clicked();
            if r {
                ui.close();
            }
            r
        };
        let toggle = |ui: &mut Ui, on: bool, icon: &str, label: &str, key: &str| {
            let r = widgets::menu_check(ui, on, icon, label, key).clicked();
            if r {
                ui.close();
            }
            r
        };
        let analysis = Command::ToggleAnalysis;
        if item(ui, analysis.icon(), "Analyze...", &analysis.shortcut()) {
            self.analyze = true;
        }
        ui.separator();
        if item(ui, I::ARROWS_IN, "Fit", &key_text(Command::ZoomFit)) {
            self.fit_all();
        }
        if self.panes.len() == 2 && item(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B", &key_text(Command::SwapAB)) {
            self.swap_ab();
        }
        if toggle(ui, self.sync, I::LINK, "Link zoom & pan", &key_text(Command::ToggleLink)) {
            self.toggle_sync();
        }
        if toggle(ui, self.show_hist, I::CHART_BAR, "Histograms", &key_text(Command::ToggleHistogram)) {
            self.show_hist = !self.show_hist;
        }
        if self.show_hist {
            let mode = self.rv.hist_mode();
            widgets::submenu(ui, I::CHART_BAR, "Histogram shows", |ui| {
                for m in HistMode::ALL {
                    if widgets::menu_toggle(ui, mode == m, m.label(), "").clicked() {
                        self.rv.set_hist_mode(m);
                        ui.close();
                    }
                }
            });
        }
        if toggle(ui, self.show_exif, I::CAMERA, "Camera info strip", &key_text(Command::ToggleCameraInfo)) {
            self.show_exif = !self.show_exif;
        }
        ui.separator();
        if self.markup_allowed() && toggle(ui, self.rv.markup, I::PENCIL_SIMPLE_LINE, "Mark up", &key_text(Command::ToggleMarkup)) {
            self.toggle_markup(core);
        }
        if toggle(ui, self.rv.panel, I::CHAT_TEXT, "Review panel", &key_text(Command::ToggleReview)) {
            self.toggle_review(core);
        }
        if item(ui, I::EXPORT, "Export report...", &key_text(Command::ExportReport)) {
            self.rv.open_export();
        }
        // Culling on the pane that was right-clicked.
        if pane < self.panes.len() {
            ui.separator();
            let letter = LETTERS[pane.min(3)];
            if self.panes.len() >= 2 && item(ui, I::CHECK_CIRCLE, &format!("Keep {letter}, reject the others"), &key_text(Command::KeepOnly)) {
                self.selected = pane;
                self.keep_only(core, pane);
            }
            let flagged = core.tags.get(&self.panes[pane].path).flagged;
            if toggle(ui, flagged, I::FLAG, &format!("Flag {letter}"), &key_text(Command::ToggleFlag)) {
                self.toggle_flag(core, pane);
            }
            if item(ui, I::TRASH, &self.delete_label(pane), &key_text(Command::Delete)) {
                self.confirm_delete = Some(pane);
            }
        }
        ui.separator();
        if item(ui, I::ARROW_LEFT, "Back", "Esc") {
            self.exit = true;
        }
    }

    fn paint_image(&mut self, ui: &Ui, core: &Core, i: usize, rect: Rect, clip: Rect, rects: &[Rect], ppp: f32, fit_ref: f32) -> Option<f32> {
        let p = core.pal;
        let (z, c, d) = self.effective(i, rects, ppp, fit_ref)?;
        let dest = dest_rect(rect, ppp, d, z, c);
        // Panes A to D for the Analysis panel's overlays.
        let slot = crate::analysis_ui::canvas::Slot::from_index(i);
        if let Some(slot) = slot {
            crate::analysis_ui::canvas::publish(ui.ctx(), slot, dest, d, clip);
        }
        let painter = ui.painter().with_clip_rect(clip);
        let pane = &mut self.panes[i];
        if pane.loaded.as_ref().is_some_and(|l| l.decoded.has_alpha) && core.settings.checkerboard_alpha {
            render::paint_checkerboard(&painter, dest.intersect(clip), 10.0, p.dark);
        }
        let nearest = !core.settings.smooth_zoom_in && z >= 2.0;
        if let Some(g) = &mut pane.gpu {
            g.paint(ui.ctx(), &painter, dest, clip, z, nearest, Color32::WHITE);
        }
        Some(z)
    }

    fn placeholder(&self, ui: &Ui, core: &Core, i: usize, rect: Rect) {
        let p = core.pal;
        let pane = &self.panes[i];
        if let Some(err) = &pane.error {
            ui.painter().text(rect.center() - Vec2::new(0.0, 26.0), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(40.0), p.faint);
            ui.painter().text(rect.center() + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, name_of(&pane.path), FontId::proportional(14.0), p.text);
            let g = ui.painter().layout(err.clone(), FontId::proportional(12.0), p.muted, (rect.width() - 40.0).clamp(40.0, 520.0));
            ui.painter().galley(rect.center() + Vec2::new(-g.size().x / 2.0, 30.0), g, p.muted);
        } else if pane.gpu.is_none() {
            let r = Rect::from_center_size(rect.center(), Vec2::splat(28.0));
            egui::Spinner::new().size(28.0).paint_at(ui, r);
        }
    }

    /// Font of the caption pills.
    fn pill_font() -> FontId {
        FontId::proportional(12.0)
    }

    /// Width of a caption pill showing all of `text` (and a letter badge).
    fn pill_width(ui: &Ui, text: &str, badge: bool) -> f32 {
        ui.painter().layout_no_wrap(text.to_string(), Self::pill_font(), Color32::WHITE).size().x + 20.0 + if badge { 22.0 } else { 0.0 }
    }

    /// A caption pill at most `max_w` points wide, so captions never run
    /// into a neighbour's: trailing "  ·  " parts are left out first (the
    /// file name stays), then the name is shortened in the middle with
    /// "...", so its end (where compared names differ) stays readable.
    fn pill(ui: &Ui, pos: Pos2, anchor: Align2, text: &str, badge: Option<(&str, Color32)>, max_w: f32) -> Rect {
        let font = Self::pill_font();
        let badge_w = if badge.is_some() { 22.0 } else { 0.0 };
        let layout = |t: String| ui.painter().layout_no_wrap(t, font.clone(), Color32::from_white_alpha(235));
        let room = (max_w - 20.0 - badge_w).max(0.0);
        let parts: Vec<&str> = text.split(CAPTION_SEP).collect();
        let fitting = (1..=parts.len()).rev().map(|n| layout(parts[..n].join(CAPTION_SEP))).find(|g| g.size().x <= room);
        let g = fitting.unwrap_or_else(|| layout(middle_ellipsis(ui, parts[0], &font, room)));
        let size = Vec2::new(g.size().x + 20.0 + badge_w, 24.0);
        let r = anchor.anchor_size(pos, size);
        ui.painter().rect_filled(r, CornerRadius::same(12), Color32::from_black_alpha(150));
        let mut x = r.min.x + 10.0;
        if let Some((b, col)) = badge {
            let br = Rect::from_center_size(Pos2::new(r.min.x + 14.0, r.center().y), Vec2::splat(18.0));
            ui.painter().rect_filled(br, CornerRadius::same(9), col);
            ui.painter().text(br.center(), Align2::CENTER_CENTER, b, super::theme::semibold(11.0), theme::text_on(col));
            x += badge_w - 2.0;
        }
        ui.painter().galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g, Color32::WHITE);
        r
    }

    /// The device name of pane `i` in a project scene ("Left" / "Right" in a blind test).
    fn pane_label(&self, i: usize) -> Option<String> {
        let link = self.link.as_ref()?;
        if link.blind {
            return Some(if i == 0 { "Left".into() } else { "Right".into() });
        }
        link.labels.get(i).cloned()
    }

    fn pane_color(&self, _core: &Core, i: usize) -> Color32 {
        match &self.link {
            Some(l) if l.blind => Color32::from_rgb(110, 116, 128),
            Some(l) => l.colors.get(i).map(|c| Color32::from_rgb(c[0], c[1], c[2])).unwrap_or_else(|| Self::badge_color(i)),
            None => Self::badge_color(i),
        }
    }

    /// The scene bar of a project compare: scene name and position, previous
    /// and next, the scorecard toggle; in a blind test the vote buttons.
    fn scene_bar(&mut self, ui: &mut Ui, p: &super::theme::Palette) {
        let Some(link) = self.link.clone() else { return };
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if link.blind {
                ui.label(RichText::new(format!("{}  Blind test", I::EYE_SLASH)).font(text::body_strong()).color(p.text))
                    .on_hover_text("Names, files and camera data are hidden and the sides are random. Pick the image that looks better overall.");
                if !link.progress.is_empty() {
                    ui.label(RichText::new(&link.progress).size(text::SMALL).color(p.muted));
                }
                ui.add_space(12.0);
                let vote_tip = |t: &str, c: Command| keymap::tip(t, KeyMode::Blind, c);
                if widgets::secondary_button(ui, &format!("{}  Left is better", I::ARROW_LEFT)).on_hover_text(vote_tip("The left image looks better", Command::VoteLeft)).clicked() {
                    self.link_requests.push(LinkRequest::Vote(0));
                }
                if widgets::secondary_button(ui, "About the same").on_hover_text(vote_tip("No clear difference", Command::VoteSame)).clicked() {
                    self.link_requests.push(LinkRequest::Vote(2));
                }
                if widgets::secondary_button(ui, &format!("Right is better  {}", I::ARROW_RIGHT)).on_hover_text(vote_tip("The right image looks better", Command::VoteRight)).clicked() {
                    self.link_requests.push(LinkRequest::Vote(1));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::secondary_button(ui, "End test").on_hover_text("Stop the blind test and see the ranking").clicked() {
                        self.link_requests.push(LinkRequest::EndBlind);
                    }
                });
                return;
            }
            if widgets::icon_button(ui, I::CARET_LEFT, &tip("Previous scene", Command::PrevScene), false).clicked() {
                self.link_requests.push(LinkRequest::Step(-1));
            }
            ui.label(RichText::new(&link.name).font(text::body_strong()).color(p.text));
            ui.label(RichText::new(format!("Scene {} of {}", link.scene + 1, link.count.max(1))).size(text::SMALL).color(p.muted));
            if widgets::icon_button(ui, I::CARET_RIGHT, &tip("Next scene", Command::NextScene), false).clicked() {
                self.link_requests.push(LinkRequest::Step(1));
            }
            ui.add_space(8.0);
            let mut done = link.done;
            if widgets::checkbox(ui, &mut done, "Reviewed").on_hover_text("Mark this scene as reviewed; the project overview shows which scenes are left").changed() {
                self.link_requests.push(LinkRequest::SetDone(done));
                if let Some(l) = &mut self.link {
                    l.done = done;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::labeled_icon_button(ui, I::LIST_CHECKS, "Scorecard", &tip("Show or hide the scorecard: verdicts and attribute scores per device", Command::ToggleScorecard), self.scorecard).clicked() {
                    self.link_requests.push(LinkRequest::ToggleScorecard);
                }
            });
        });
    }

    fn caption_text(&self, i: usize, zoom: Option<f32>) -> String {
        let pane = &self.panes[i];
        if self.blind() {
            return match zoom {
                Some(z) => format!("{}{CAPTION_SEP}{:.0}%", self.pane_label(i).unwrap_or_default(), z * 100.0),
                None => self.pane_label(i).unwrap_or_default(),
            };
        }
        // "aligned" stays in the first part: later parts drop out when narrow.
        let aligned = if pane.showing_aligned { " (aligned)" } else { "" };
        let mut parts = vec![self.pane_label(i).map(|l| format!("{l}{aligned}{CAPTION_SEP}{}", name_of(&pane.path))).unwrap_or_else(|| format!("{}{aligned}", name_of(&pane.path)))];
        if let Some((w, h)) = pane.dims() {
            parts.push(format!("{w} × {h}"));
        }
        if let Some(l) = &pane.loaded {
            parts.push(fmt_size(l.file_size));
        }
        if pane.showing_aligned {
            if let Some(Ok(r)) = &pane.align {
                parts.push(format!("aligned to A: {}", confidence_word(r.confidence)));
            }
        }
        if let Some(z) = zoom {
            parts.push(format!("{:.0}%", z * 100.0));
        }
        parts.join(CAPTION_SEP)
    }

    fn badge_color(i: usize) -> Color32 {
        theme::PANE_COLORS[i % 4]
    }

    /// Where each pane's image is drawn (`clips` per pane).
    fn geos(&self, rects: &[Rect], clips: &[Rect], ppp: f32, fit_ref: f32) -> Vec<Option<PaneGeo>> {
        (0..self.panes.len())
            .map(|i| {
                let (z, c, d) = self.effective(i, rects, ppp, fit_ref)?;
                Some(PaneGeo { dest: dest_rect(rects[i], ppp, d, z, c), dims: d, clip: *clips.get(i)? })
            })
            .collect()
    }

    fn side_by_side(&mut self, ui: &mut Ui, core: &mut Core, rects: &[Rect], ppp: f32) {
        let p = core.pal;
        let fit_ref = self.fit_ref(rects, ppp);
        let geos = self.geos(rects, rects, ppp, fit_ref);
        let mut remove = None;
        for i in 0..self.panes.len() {
            let rect = rects[i];
            let resp = ui.interact(rect, ui.scope_id().with(("cmp-pane", i)), Sense::click_and_drag());
            let zoom = self.paint_image(ui, core, i, rect, rect, rects, ppp, fit_ref);
            if zoom.is_none() {
                self.placeholder(ui, core, i, rect);
            }
            if self.inspect != super::inspect_view::InspectMode::Off {
                if let Some(g) = &geos[i] {
                    let pane = &self.panes[i];
                    let path = pane.path.clone();
                    if self.inspect.is_gain() {
                        // The gain map fits the original image only, not the aligned one.
                        let img = pane.loaded.as_ref().filter(|_| !pane.showing_aligned).map(|l| l.decoded.image.clone());
                        if let Some(img) = img {
                            self.inspect_cache.paint_gain(ui, &path, &img, self.inspect, self.hdr_headroom, g.dest, g.clip);
                        }
                    } else {
                        let img = if pane.showing_aligned { pane.aligned.clone() } else { pane.loaded.as_ref().map(|l| l.decoded.image.clone()) };
                        if let Some(img) = img {
                            let key = if pane.showing_aligned { pane.path.with_extension("aligned") } else { pane.path.clone() };
                            self.inspect_cache.paint(ui, &key, &img, self.inspect, g.dest, g.clip);
                        }
                        // Also tells the O cycle whether the panes have gain maps.
                        let _ = self.inspect_cache.gain_info(ui.ctx(), &path);
                    }
                }
            }
            if let Some(g) = geos[i].as_ref().filter(|_| !self.panes[i].showing_aligned) {
                self.rv.paint(ui, i, g);
            }
            self.view_input(ui, core, &resp, i, rects, ppp, &geos);
            // Caption pill. While the pointer is on the pane, "Keep A" joins
            // its row (left of the remove button), but only when the whole
            // caption still fits beside it: it never pushes the name out.
            let text = self.caption_text(i, zoom);
            let reserve = if self.panes.len() > 2 { 34.0 } else { 0.0 };
            let room = rect.width() - 20.0 - reserve;
            let keep_w = Self::keep_size(ui, i).x + 8.0;
            let show_keep = self.shows_keep(i, rect) && Self::pill_width(ui, &text, true) + keep_w <= room;
            let caption = Self::pill(ui, rect.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &text, Some((LETTERS[i], self.pane_color(core, i))), room);
            if self.inspect.is_gain() {
                use super::inspect_view::GainInfo;
                let info = if self.panes[i].showing_aligned {
                    GainInfo::Missing(lumen_core::inspect::gainmap::NoGainMap { reason: "The gain map views show the original image: turn off Align to A.".into(), announced: true })
                } else {
                    let path = self.panes[i].path.clone();
                    self.inspect_cache.gain_info(ui.ctx(), &path)
                };
                let at = Pos2::new(caption.min.x, caption.max.y + 8.0);
                let id = ui.scope_id().with(("cmp-gain-legend", i));
                super::inspect_view::gain_legend(ui, id, at, rect.width() - 20.0, &info, self.inspect, &mut self.hdr_headroom);
            }
            if self.show_hist {
                let hr = if self.show_exif { Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.max.y - 30.0)) } else { rect };
                self.paint_hist(ui, core, i, hr);
            }
            self.pane_overlays(ui, core, i, rect, caption, reserve, show_keep);
            if self.panes.len() > 2 {
                let br = Rect::from_min_size(Pos2::new(rect.max.x - 34.0, rect.min.y + 8.0), Vec2::splat(26.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(br));
                child.painter().rect_filled(br, CornerRadius::same(13), Color32::from_black_alpha(140));
                child.visuals_mut().override_text_color = Some(Color32::WHITE);
                if widgets::icon_button_sized(&mut child, I::X, "Remove from comparison", false, 26.0, 14.0).clicked() {
                    remove = Some(i);
                }
            }
            if self.selected == i && self.panes.len() > 1 {
                ui.painter().rect_stroke(rect.shrink(1.0), CornerRadius::ZERO, Stroke::new(2.0, p.accent.gamma_multiply(0.8)), egui::StrokeKind::Inside);
            }
        }
        if self.probe {
            self.paint_probe(ui, &geos);
        }
        // Thin separators on the panes' inner edges, in the 1 pt gaps (a
        // centred last row of a grid gets them on both sides).
        let full = self.last_area;
        let line = Stroke::new(1.0, p.border);
        for r in rects {
            let e = r.expand(0.5);
            if r.min.x > full.min.x + 0.5 {
                ui.painter().line_segment([e.left_top(), e.left_bottom()], line);
            }
            if r.max.x < full.max.x - 0.5 {
                ui.painter().line_segment([e.right_top(), e.right_bottom()], line);
            }
            if r.min.y > full.min.y + 0.5 {
                ui.painter().line_segment([e.left_top(), e.right_top()], line);
            }
            if r.max.y < full.max.y - 0.5 {
                ui.painter().line_segment([e.left_bottom(), e.right_bottom()], line);
            }
        }
        if self.profile_mode {
            self.paint_profile(ui, core, &geos);
        }
        // The selected pane's navigator, clear of what is in its corner.
        if let Some(&r) = rects.get(self.selected.min(self.panes.len().saturating_sub(1))) {
            let avoid = self.corner_overlays(r);
            self.navigator(ui, core, rects, ppp, &avoid);
        }
        if let Some(i) = remove {
            self.remove_pane(i);
        }
    }

    /// What a pane drawn in `rect` shows in its bottom-right corner (the
    /// histogram, see `paint_hist`, and the camera strip), for the navigator
    /// to keep clear of.
    fn corner_overlays(&self, rect: Rect) -> Vec<Rect> {
        let mut v = Vec::new();
        let hr = if self.show_exif { Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.max.y - 30.0)) } else { rect };
        if self.show_hist {
            v.push(Rect::from_min_max(hr.max - Vec2::new(226.0, 134.0), hr.max - Vec2::splat(8.0)));
        }
        if self.show_exif {
            v.push(Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - 26.0), rect.max));
        }
        v
    }

    /// The zoom navigator of the pane the zoom keys act on (A in Split and
    /// Difference) while its image is larger than the pane: an overview with
    /// the part on screen framed, to drag; linked panes follow it. `avoid`:
    /// overlays it keeps clear of. Not while marking up or drawing a line
    /// profile, where a drag in the corner draws.
    fn navigator(&mut self, ui: &mut Ui, core: &Core, rects: &[Rect], ppp: f32, avoid: &[Rect]) {
        if !core.settings.zoom_navigator || self.profile_mode || (self.rv.markup && self.markup_allowed()) {
            return;
        }
        let i = if self.pair_mode() { 0 } else { self.selected.min(self.panes.len().saturating_sub(1)) };
        let fit_ref = self.fit_ref(rects, ppp);
        let (Some((z, c, d)), Some(&pane)) = (self.effective(i, rects, ppp, fit_ref), rects.get(i)) else { return };
        let dest = dest_rect(pane, ppp, d, z, c);
        if dest.width() <= pane.width() + 1.0 && dest.height() <= pane.height() + 1.0 {
            return;
        }
        // Difference shows |A - B| in A's place.
        let tex = if self.pair_mode() && self.mode == CmpMode::Difference { self.diff.gpu.as_ref().and_then(|(_, g)| g.overview()) } else { self.panes[i].gpu.as_ref().and_then(|g| g.overview()) };
        let Some(tex) = tex else { return };
        let on = pane.intersect(dest);
        let shown = Rect::from_min_max(((on.min - dest.min) / dest.size()).to_pos2(), ((on.max - dest.min) / dest.size()).to_pos2());
        let Some(r) = widgets::navigator_rect(pane, d.0 as f32 / d.1.max(1) as f32, avoid) else { return };
        let id = ui.scope_id().with("cmp-navigator");
        let out = widgets::navigator(ui, id, r, tex, shown);
        if out.visible {
            crate::analysis_ui::canvas::occlude(ui.ctx(), r.expand(4.0));
        }
        if let Some(f) = out.pan_to {
            let basis = self.basis;
            let target = Vec2::new(f.x * d.0 as f32, f.y * d.1 as f32);
            self.apply(i, |_| from_effective(basis, d, pane.size() * ppp, z, target));
        }
    }

    /// The pixel inspector: the pointer's spot in the pane under it, the same
    /// spot (same fraction of the image) in every other pane, each with its
    /// RGB, hex, CIE L*a*b* and luma.
    fn paint_probe(&self, ui: &Ui, geos: &[Option<PaneGeo>]) {
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
        let Some(gh) = geos.iter().find_map(|g| g.as_ref().filter(|g| g.clip.contains(pos))) else { return };
        let px = super::review::to_image(gh.dest, gh.dims, pos);
        let rel = [px[0] / gh.dims.0.max(1) as f32, px[1] / gh.dims.1.max(1) as f32];
        if !(0.0..1.0).contains(&rel[0]) || !(0.0..1.0).contains(&rel[1]) {
            return;
        }
        for (i, g) in geos.iter().enumerate() {
            let Some(g) = g else { continue };
            let pane = &self.panes[i];
            let img = if pane.showing_aligned { pane.aligned.clone() } else { pane.loaded.as_ref().map(|l| l.decoded.image.clone()) };
            let Some(img) = img else { continue };
            let (x, y) = ((rel[0] * img.width() as f32) as u32, (rel[1] * img.height() as f32) as u32);
            let Some(c) = img.get_pixel_checked(x, y).map(|p| p.0) else { continue };
            let sp = super::review::to_screen(g.dest, g.dims, [rel[0] * g.dims.0 as f32, rel[1] * g.dims.1 as f32]);
            let painter = ui.painter().with_clip_rect(g.clip);
            for (col, w) in [(Color32::from_black_alpha(200), 3.0), (Color32::WHITE, 1.2)] {
                painter.line_segment([sp - Vec2::new(10.0, 0.0), sp - Vec2::new(3.0, 0.0)], Stroke::new(w, col));
                painter.line_segment([sp + Vec2::new(3.0, 0.0), sp + Vec2::new(10.0, 0.0)], Stroke::new(w, col));
                painter.line_segment([sp - Vec2::new(0.0, 10.0), sp - Vec2::new(0.0, 3.0)], Stroke::new(w, col));
                painter.line_segment([sp + Vec2::new(0.0, 3.0), sp + Vec2::new(0.0, 10.0)], Stroke::new(w, col));
            }
            let lab = lumen_analysis::color::srgb8_to_lab([c[0], c[1], c[2]]);
            let luma = 0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32;
            let text = format!(
                "x {x}  y {y}   RGB {} {} {}   #{:02X}{:02X}{:02X}   L* {:.1}  a* {:.1}  b* {:.1}   Y {:.0}{}",
                c[0], c[1], c[2], c[0], c[1], c[2], lab[0], lab[1], lab[2], luma,
                if c[3] < 255 { format!("   alpha {}", c[3]) } else { String::new() }
            );
            let font = FontId::monospace(11.5);
            let gal = painter.layout_no_wrap(text, font, Color32::WHITE);
            let r = Rect::from_min_size(Pos2::new(g.clip.min.x + 10.0, g.clip.max.y - 34.0 - gal.size().y), gal.size() + Vec2::new(36.0, 8.0));
            painter.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(190));
            let sw = Rect::from_min_size(r.min + Vec2::new(6.0, 4.0), Vec2::splat(gal.size().y));
            painter.rect_filled(sw, CornerRadius::same(2), Color32::from_rgb(c[0], c[1], c[2]));
            painter.rect_stroke(sw, CornerRadius::same(2), Stroke::new(1.0, Color32::from_white_alpha(120)), egui::StrokeKind::Inside);
            painter.galley(r.min + Vec2::new(12.0 + gal.size().y, 4.0), gal, Color32::WHITE);
        }
    }

    /// Samples of luma (0..255) along the profile line in pane `i`'s image,
    /// with the line's length in that image's pixels.
    fn profile_samples(&self, i: usize, line: ([f32; 2], [f32; 2])) -> Option<(Vec<f32>, f32)> {
        let pane = &self.panes[i];
        let img = if pane.showing_aligned { pane.aligned.clone() } else { pane.loaded.as_ref().map(|l| l.decoded.image.clone()) }?;
        let (w, h) = (img.width() as f32, img.height() as f32);
        let (a, b) = ([line.0[0] * w, line.0[1] * h], [line.1[0] * w, line.1[1] * h]);
        let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        if len < 2.0 {
            return None;
        }
        let n = (len.ceil() as usize).clamp(2, 1200);
        let samples = (0..n)
            .map(|k| {
                let t = k as f32 / (n - 1) as f32;
                let (x, y) = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
                let (x, y) = ((x as u32).min(img.width() - 1), (y as u32).min(img.height() - 1));
                let p = img.get_pixel(x, y).0;
                0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32
            })
            .collect();
        Some((samples, len))
    }

    /// The line on every pane and the profiles of all panes in one plot.
    fn paint_profile(&self, ui: &Ui, core: &Core, geos: &[Option<PaneGeo>]) {
        let area = self.last_area;
        let Some(line) = self.profile else {
            let r = Rect::from_center_size(Pos2::new(area.center().x, area.max.y - 90.0), Vec2::new(420.0, 30.0));
            ui.painter().rect_filled(r, CornerRadius::same(8), Color32::from_black_alpha(170));
            let leave = key_text(Command::LineProfile);
            let t = if leave.is_empty() { "Line profile: drag across an edge".to_string() } else { format!("Line profile: drag across an edge ({leave} to leave)") };
            ui.painter().text(r.center(), Align2::CENTER_CENTER, t, FontId::proportional(12.5), Color32::WHITE);
            return;
        };
        for (i, g) in geos.iter().enumerate() {
            let Some(g) = g else { continue };
            let to = |r: [f32; 2]| super::review::to_screen(g.dest, g.dims, [r[0] * g.dims.0 as f32, r[1] * g.dims.1 as f32]);
            let painter = ui.painter().with_clip_rect(g.clip);
            let (a, b) = (to(line.0), to(line.1));
            painter.line_segment([a, b], Stroke::new(3.0, Color32::from_black_alpha(180)));
            painter.line_segment([a, b], Stroke::new(1.5, self.pane_color(core, i)));
            painter.circle_filled(a, 3.5, Color32::WHITE);
        }
        let data: Vec<(usize, Vec<f32>, f32)> = (0..self.panes.len()).filter_map(|i| self.profile_samples(i, line).map(|(s, l)| (i, s, l))).collect();
        if data.is_empty() {
            return;
        }
        // One plot for every pane, so it is centred on the whole compare area
        // (painted after the pane dividers, which then never cross it).
        let plot = Rect::from_center_size(Pos2::new(area.center().x, area.max.y - 159.0), Vec2::new((area.width() - 32.0).min(760.0), 186.0));
        let painter = ui.painter();
        painter.rect_filled(plot, CornerRadius::same(10), Color32::from_black_alpha(225));
        let inner = Rect::from_min_max(plot.min + Vec2::new(14.0, 30.0), plot.max - Vec2::new(14.0, 26.0));
        painter.text(plot.min + Vec2::new(14.0, 8.0), Align2::LEFT_TOP, "Brightness along the line", super::theme::semibold(12.0), Color32::WHITE);
        for frac in [0.0f32, 0.5, 1.0] {
            let y = inner.max.y - frac * inner.height();
            painter.line_segment([Pos2::new(inner.min.x, y), Pos2::new(inner.max.x, y)], Stroke::new(1.0, Color32::from_white_alpha(28)));
        }
        let mut legend = Vec::new();
        for (i, samples, len) in &data {
            let col = self.pane_color(core, *i);
            let n = samples.len();
            let pts: Vec<Pos2> = samples.iter().enumerate().map(|(k, v)| Pos2::new(inner.min.x + inner.width() * k as f32 / (n - 1).max(1) as f32, inner.max.y - inner.height() * (v / 255.0))).collect();
            painter.add(egui::Shape::line(pts, Stroke::new(1.6, col)));
            let width = edge_width(samples).map(|w| w * len / (n - 1).max(1) as f32);
            legend.push((col, format!("{}  {}", LETTERS[(*i).min(3)], match width {
                Some(w) => format!("edge {w:.1} px"),
                None => "no clear edge".to_string(),
            })));
        }
        let mut x = plot.max.x - 14.0;
        for (col, t) in legend.iter().rev() {
            let g = painter.layout_no_wrap(t.clone(), FontId::proportional(11.5), *col);
            x -= g.size().x;
            painter.galley(Pos2::new(x, plot.min.y + 8.0), g, *col);
            x -= 14.0;
        }
        let leave = key_text(Command::LineProfile);
        let note = format!("Edge: distance from 10% to 90% of the brightness step across the line (smaller is sharper).{}", if leave.is_empty() { String::new() } else { format!(" {leave} to leave.") });
        painter.text(Pos2::new(plot.min.x + 14.0, plot.max.y - 8.0), Align2::LEFT_BOTTOM, note, FontId::proportional(10.5), Color32::from_white_alpha(200));
    }

    /// The pane's "Keep" button shows on the pane under the pointer only
    /// (never in a blind test: no culling there).
    fn shows_keep(&self, i: usize, rect: Rect) -> bool {
        self.hover_pane == Some(i) && self.panes.len() >= 2 && rect.width() > 260.0 && !self.popup_open && !self.blind()
    }

    /// Size of pane `i`'s "Keep" button.
    fn keep_size(ui: &Ui, i: usize) -> Vec2 {
        let g = ui.painter().layout_no_wrap(keep_text(i), FontId::proportional(12.5), Color32::WHITE);
        Vec2::new(12.0 + size::ICON + 6.0 + g.size().x + 12.0, size::CONTROL)
    }

    /// Culling overlays for pane `i` drawn inside `rect`: reject frame, rating /
    /// flag / reject status, the optional EXIF strip and, with `keep`, the
    /// "Keep" button in the caption row (`caption` is the caption pill,
    /// `reserve` the points kept free at the row's right end for the remove
    /// button).
    #[allow(clippy::too_many_arguments)]
    fn pane_overlays(&mut self, ui: &mut Ui, core: &mut Core, i: usize, rect: Rect, caption: Rect, reserve: f32, keep: bool) {
        if i >= self.panes.len() || !rect.is_positive() || self.blind() {
            return;
        }
        let path = self.panes[i].path.clone();
        let tag = core.tags.get(&path);
        let rejected = tag.label == REJECT_LABEL;
        let red = Color32::from_rgb(235, 87, 87);
        let painter = ui.painter().with_clip_rect(rect);
        if rejected {
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_black_alpha(70));
            // Inset so the selection outline does not hide it.
            painter.rect_stroke(rect.shrink(3.5), CornerRadius::ZERO, Stroke::new(3.0, red), egui::StrokeKind::Inside);
        }
        let mut bottom = rect.max.y - 10.0;
        if self.show_exif {
            let p2 = path.clone();
            let line = self.panes[i].exif.get(&path, ui.ctx(), move || crate::meta::read_exif(&p2)).map(|e| e.as_ref().map(exif_line).unwrap_or_default());
            let fresh = self.panes[i].exif.is_current(&path);
            let text = match line {
                Some(l) if fresh && !l.is_empty() => l,
                Some(_) if fresh => "No camera data".to_string(),
                _ => "Reading EXIF...".to_string(),
            };
            let strip = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - 26.0), rect.max);
            painter.rect_filled(strip, CornerRadius::ZERO, Color32::from_black_alpha(175));
            painter.text(Pos2::new(strip.min.x + 10.0, strip.center().y), Align2::LEFT_CENTER, format!("{}  {text}", I::CAMERA), FontId::proportional(12.0), Color32::from_white_alpha(235));
            bottom = strip.min.y - 8.0;
        }
        // Status pill.
        let mut parts: Vec<(String, Color32)> = Vec::new();
        if rejected {
            parts.push((format!("{} REJECTED", I::X_CIRCLE), red));
        }
        if tag.flagged {
            parts.push((format!("{} Flagged", I::FLAG), Color32::from_rgb(80, 200, 120)));
        }
        if tag.rating > 0 {
            parts.push((format!("{} {}", I::STAR, tag.rating), Color32::from_rgb(242, 201, 76)));
        }
        if tag.label > 0 && !rejected {
            let c = crate::tags::LABEL_COLORS[tag.label as usize % 6];
            parts.push((crate::tags::LABEL_NAMES[tag.label as usize % 6].to_string(), Color32::from_rgb(c[0], c[1], c[2])));
        }
        if !parts.is_empty() {
            let font = super::theme::semibold(12.5);
            let galleys: Vec<_> = parts.into_iter().map(|(t, c)| painter.layout_no_wrap(t, font.clone(), c)).collect();
            let w: f32 = galleys.iter().map(|g| g.size().x).sum::<f32>() + 14.0 * (galleys.len() as f32 - 1.0) + 22.0;
            let r = Rect::from_min_size(Pos2::new(rect.min.x + 10.0, bottom - 26.0), Vec2::new(w, 26.0));
            painter.rect_filled(r, CornerRadius::same(13), Color32::from_black_alpha(170));
            let mut x = r.min.x + 11.0;
            for g in galleys {
                let gw = g.size().x;
                painter.galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g, Color32::WHITE);
                x += gw + 14.0;
            }
        }
        // "Keep A" only on the pane under the pointer: a neutral pill in the
        // caption row, so it never covers the image or competes with it (the
        // toolbar's Keep is the filled one). It goes at the row's right end,
        // or left of a caption that is there (Split's B), and stays away
        // when neither spot is clear of the caption.
        if keep {
            let p = core.pal;
            let size = Self::keep_size(ui, i);
            let gap = 8.0;
            let clear = |x: f32| x >= rect.min.x + 10.0 && (x + size.x + gap <= caption.min.x || x >= caption.max.x + gap);
            let x = [rect.max.x - 10.0 - reserve - size.x, caption.min.x - gap - size.x].into_iter().find(|&x| clear(x));
            if let Some(x) = x {
                let br = Rect::from_min_size(Pos2::new(x, caption.center().y - size.y / 2.0), size);
                let resp = ui.interact(br, ui.scope_id().with(("cmp-keep", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let painter = ui.painter().with_clip_rect(rect);
                let r = CornerRadius::same((size::CONTROL / 2.0) as u8);
                painter.rect(br, r, if resp.hovered() { theme::mix(p.raised, p.text, 0.08) } else { p.overlay }, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                painter.text(Pos2::new(br.min.x + 12.0 + size::ICON / 2.0, br.center().y), Align2::CENTER_CENTER, I::CHECK_CIRCLE, FontId::proportional(size::ICON), p.ok);
                let g = painter.layout_no_wrap(keep_text(i), FontId::proportional(12.5), p.text);
                painter.galley(Pos2::new(br.min.x + 12.0 + size::ICON + 6.0, br.center().y - g.size().y / 2.0), g, p.text);
                if resp.on_hover_text(tip(&format!("Flag {} and mark the other images as rejected with the red label", LETTERS[i]), Command::KeepOnly)).clicked() {
                    self.selected = i;
                    self.keep_only(core, i);
                }
            }
        }
    }

    /// The histogram overlay of pane `i` in the bottom-right of `rect`, with
    /// its Luminance / RGB / Both switch above it.
    fn paint_hist(&mut self, ui: &Ui, core: &Core, i: usize, rect: Rect) {
        let r = Rect::from_min_size(Pos2::new(rect.max.x - 222.0, rect.max.y - 104.0), Vec2::new(210.0, 92.0));
        if r.min.x < rect.min.x + 4.0 {
            return;
        }
        let mode = self.rv.hist_mode();
        match &self.panes[i].hist {
            Some(h) => {
                let chips = Rect::from_min_max(Pos2::new(r.min.x - 4.0, r.min.y - 30.0), Pos2::new(r.max.x + 4.0, r.min.y - 4.0));
                let bg = Rect::from_min_max(chips.min, r.max + Vec2::splat(4.0));
                ui.painter().rect_filled(bg, CornerRadius::same(8), Color32::from_black_alpha(150));
                h.paint_mode(ui.painter(), r, true, mode);
                let tips = ["Luminance: the brightness of each pixel", "RGB: the red, green and blue channels", "Both: brightness behind the three channels"];
                let w = (chips.width() - 8.0) / 3.0;
                for (k, m) in HistMode::ALL.into_iter().enumerate() {
                    let cr = Rect::from_min_size(Pos2::new(chips.min.x + 4.0 + k as f32 * w, chips.min.y + 4.0), Vec2::new(w - 2.0, chips.height() - 6.0));
                    let resp = ui.interact(cr, ui.scope_id().with(("cmp-hist-mode", i, k)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    let on = m == mode;
                    let fill = if on { core.pal.accent } else if resp.hovered() { Color32::from_white_alpha(30) } else { Color32::TRANSPARENT };
                    ui.painter().rect_filled(cr, CornerRadius::same(5), fill);
                    let fg = if on { theme::text_on(core.pal.accent) } else { Color32::from_white_alpha(220) };
                    ui.painter().text(cr.center(), Align2::CENTER_CENTER, m.label(), FontId::proportional(11.5), fg);
                    if resp.on_hover_text(format!("Histogram: {}", tips[k])).clicked() {
                        self.rv.set_hist_mode(m);
                    }
                }
            }
            None if self.panes[i].gpu.is_some() => {
                egui::Spinner::new().size(16.0).paint_at(ui, Rect::from_center_size(r.center(), Vec2::splat(16.0)));
            }
            None => {}
        }
    }

    fn pair_view(&mut self, ui: &mut Ui, core: &mut Core, area: Rect, rects: &[Rect], ppp: f32) {
        let p = core.pal;
        let fit_ref = self.fit_ref(rects, ppp);
        let resp = ui.interact(area, ui.scope_id().with("cmp-pair"), Sense::click_and_drag());
        match self.mode {
            CmpMode::Split => {
                let x = area.min.x + area.width() * self.split;
                let left = Rect::from_min_max(area.min, Pos2::new(x, area.max.y));
                let right = Rect::from_min_max(Pos2::new(x, area.min.y), area.max);
                let za = self.paint_image(ui, core, 0, area, left, rects, ppp, fit_ref);
                let zb = self.paint_image(ui, core, 1, area, right, rects, ppp, fit_ref);
                if za.is_none() {
                    self.placeholder(ui, core, 0, Rect::from_min_max(area.min, Pos2::new(area.center().x, area.max.y)));
                }
                if zb.is_none() {
                    self.placeholder(ui, core, 1, Rect::from_min_max(Pos2::new(area.center().x, area.min.y), area.max));
                }
                let geos = self.geos(rects, &[left, right], ppp, fit_ref);
                for (i, g) in geos.iter().enumerate() {
                    if let Some(g) = g {
                        self.rv.paint(ui, i, g);
                    }
                }
                // Divider + handle (registered after the view so it wins the drag).
                let handle = Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(14.0, area.height()));
                let hresp = ui.interact(handle, ui.scope_id().with("cmp-split-handle"), Sense::drag()).on_hover_text("Drag to move the divider between A and B  (Tab jumps it to one side)");
                if hresp.hovered() || hresp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if hresp.dragged() {
                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                        self.split = ((pos.x - area.min.x) / area.width()).clamp(0.0, 1.0);
                    }
                }
                let col = if hresp.hovered() || hresp.dragged() { p.accent } else { Color32::from_white_alpha(220) };
                ui.painter().line_segment([Pos2::new(x, area.min.y), Pos2::new(x, area.max.y)], Stroke::new(2.0, col));
                let knob = Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(26.0, 40.0));
                ui.painter().rect_filled(knob, CornerRadius::same(13), Color32::from_black_alpha(170));
                ui.painter().rect_stroke(knob, CornerRadius::same(13), Stroke::new(1.5, col), egui::StrokeKind::Inside);
                ui.painter().text(knob.center(), Align2::CENTER_CENTER, I::ARROWS_HORIZONTAL, FontId::proportional(15.0), Color32::WHITE);
                let ta = self.caption_text(0, za);
                let tb = self.caption_text(1, zb);
                // Each caption keeps to its half, wherever the divider is.
                let half = (area.width() - 30.0) / 2.0;
                let cap_a = Self::pill(ui, area.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &ta, Some(("A", self.pane_color(core, 0))), half);
                let cap_b = Self::pill(ui, Pos2::new(area.max.x - 10.0, area.min.y + 10.0), Align2::RIGHT_TOP, &tb, Some(("B", self.pane_color(core, 1))), half);
                if self.show_hist {
                    let ha = if self.show_exif { Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - 30.0)) } else { area };
                    self.paint_hist(ui, core, 1, ha);
                    let left_hist = Rect::from_min_max(ha.min, Pos2::new(ha.min.x + 240.0, ha.max.y));
                    self.paint_hist(ui, core, 0, left_hist);
                }
                let (keep_a, keep_b) = (self.shows_keep(0, left), self.shows_keep(1, right));
                self.pane_overlays(ui, core, 0, left, cap_a, 0.0, keep_a);
                self.pane_overlays(ui, core, 1, right, cap_b, 0.0, keep_b);
                if !hresp.dragged() {
                    self.view_input(ui, core, &resp, 0, rects, ppp, &geos);
                }
                let avoid = self.corner_overlays(area);
                self.navigator(ui, core, rects, ppp, &avoid);
            }
            CmpMode::Difference => {
                let mut zshow = None;
                if let (Some((z, c, d)), Some((_, g))) = (self.effective(0, rects, ppp, fit_ref), &mut self.diff.gpu) {
                    let dest = dest_rect(area, ppp, d, z, c);
                    // The difference is drawn in A's geometry: the Analysis
                    // overlays (ROI, heatmap, AI boxes) line up with it as A.
                    crate::analysis_ui::canvas::publish(ui.ctx(), crate::analysis_ui::canvas::Slot::A, dest, d, area);
                    let painter = ui.painter().with_clip_rect(area);
                    // The diff may be computed at a reduced level: scale the zoom accordingly.
                    let scale = d.0 as f32 / g.width.max(1) as f32;
                    g.paint(ui.ctx(), &painter, dest, area, z * scale, z >= 2.0, Color32::WHITE);
                    zshow = Some(z);
                }
                let busy = self.diff.busy.is_some();
                if self.diff.gpu.is_none() {
                    if self.panes.iter().any(|p| p.error.is_some()) {
                        let i = if self.panes[0].error.is_some() { 0 } else { 1 };
                        self.placeholder(ui, core, i, area);
                    } else {
                        egui::Spinner::new().size(28.0).paint_at(ui, Rect::from_center_size(area.center(), Vec2::splat(28.0)));
                        ui.painter().text(area.center() + Vec2::new(0.0, 30.0), Align2::CENTER_TOP, "Computing difference...", FontId::proportional(13.0), p.muted);
                    }
                } else if busy {
                    egui::Spinner::new().size(16.0).paint_at(ui, Rect::from_center_size(area.right_top() + Vec2::new(-22.0, 22.0), Vec2::splat(16.0)));
                }
                let mut text = format!("|A - B|  ×{:.0}", self.amp);
                if let Some(z) = zshow {
                    text.push_str(&format!("  ·  {:.0}%", z * 100.0));
                }
                Self::pill(ui, area.min + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, &text, None, area.width() - 20.0);
                let mut avoid = Vec::new();
                if let Some(s) = self.diff.stats {
                    let psnr = if s.psnr.is_infinite() { "∞ (identical)".to_string() } else { format!("{:.2} dB", s.psnr) };
                    let mut st = format!("PSNR {psnr}  ·  mean Δ {:.2}  ·  max Δ {}  ·  {:.2}% pixels differ", s.mean, s.max, s.changed_pct);
                    if let Some(n) = &self.diff.note {
                        st.push_str(&format!("  ·  {n}"));
                    }
                    avoid.push(Self::pill(ui, Pos2::new(area.center().x, area.max.y - 14.0), Align2::CENTER_BOTTOM, &st, None, area.width() - 20.0));
                }
                let src = 0;
                self.view_input(ui, core, &resp, src, rects, ppp, &[]);
                self.navigator(ui, core, rects, ppp, &avoid);
            }
            CmpMode::SideBySide => {}
        }
    }

    /// What fits in a toolbar of `avail` points: the image count goes first,
    /// then the button names (Sync, Swap, Mark up, Review), Difference's
    /// "Amplify", the title and the "?" button, then the mode names, then
    /// parts move into the "..." menu in [`BarPart`] order (back, the mode
    /// buttons, Fit and the Analysis button always stay).
    fn bar_plan(&self, ui: &Ui, avail: f32) -> BarPlan {
        let s = ui.spacing().item_spacing.x;
        let pad = ui.spacing().button_padding.x * 2.0;
        let text_w = |t: &str, f: FontId| ui.painter().layout_no_wrap(t.to_string(), f, Color32::WHITE).size().x;
        let body = egui::TextStyle::Button.resolve(ui.style());
        let icon = 30.0 + s;
        let sep = 9.0 + s;
        let pair = self.panes.len() == 2;
        // An icon button, or with `labels` the icon and its name.
        let button = |label: &str, labels: bool| if labels { widgets::labeled_icon_button_width(ui, label) + s } else { icon };
        // One segmented control: equal segments as wide as the widest label.
        let widest = MODES.iter().map(|m| text_w(&mode_label(*m, true), body.clone())).fold(0.0, f32::max);
        let n_modes = MODES.len() as f32;
        let modes_full = if pair { n_modes * (widest + 24.0) + 4.0 + s } else { 0.0 };
        let modes_compact = if pair { n_modes * (text_w(I::COLUMNS, body.clone()) + 24.0) + 4.0 + s } else { 0.0 };
        let title = text_w("Compare", text::title()) + s;
        let count = text_w(&format!("{} images", self.panes.len()), FontId::proportional(text::SMALL)) + s;
        let blind = self.blind();
        let part = |b: BarPart, plan: &BarPlan| -> f32 {
            let labels = plan.icon_labels;
            match b {
                BarPart::Help => icon,
                BarPart::Views => sep + widgets::labeled_menu_button_width(ui, SHOW_LABEL) + s,
                BarPart::Basis if !self.basis_matters() => 0.0,
                BarPart::Basis => 126.0 + s,
                BarPart::Sync => sep + button(SYNC_LABEL, labels),
                BarPart::Cull => {
                    let keep = if self.panes.len() >= 2 { text_w(&format!("{}  Keep A", I::CHECK_CIRCLE), body.clone()) + pad + s } else { 0.0 };
                    sep + keep + if self.panes.is_empty() { 0.0 } else { 2.0 * icon }
                }
                BarPart::Swap => {
                    if pair {
                        button(SWAP_LABEL, labels)
                    } else {
                        0.0
                    }
                }
                BarPart::Zoom => 2.0 * icon + ZOOM_LABEL_W + s,
                BarPart::Review => sep + button(MARKUP_LABEL, labels) + button(REVIEW_LABEL, labels),
                BarPart::Extras => match self.mode {
                    CmpMode::Difference if pair => sep + if plan.amp_label { text_w("Amplify", FontId::proportional(text::SMALL)) + s } else { 0.0 } + AMP_RAIL_W + 4.0 + AMP_VALUE_W + s,
                    CmpMode::Split if pair => sep + text_w("Divider 100%", FontId::proportional(12.0)) + s,
                    _ => 0.0,
                },
            }
        };
        // Back, Fit, Analysis and "..." always stay.
        let fixed = icon + 4.0 + sep + sep + icon + sep + icon + icon;
        let modes_menu = text_w(&format!("{}  {}", I::COLUMNS, I::CARET_DOWN), body.clone()) + pad + s;
        let omitted = if blind { vec![BarPart::Cull, BarPart::Swap, BarPart::Review] } else { Vec::new() };
        let mut plan = BarPlan { count: true, title: true, labels: true, icon_labels: true, amp_label: true, modes_menu: false, hidden: Vec::new(), omitted };
        let width = |p: &BarPlan| -> f32 {
            let mut w = fixed + if p.modes_menu { modes_menu } else if p.labels { modes_full } else { modes_compact };
            w += if p.title { title } else { 0.0 } + if p.count { count } else { 0.0 };
            w + BarPart::ALL.iter().filter(|b| p.shown(**b)).map(|b| part(*b, p)).sum::<f32>()
        };
        let hideable: Vec<BarPart> = BarPart::ALL.iter().copied().filter(|b| *b != BarPart::Help && !plan.omitted.contains(b)).collect();
        for step in 0..7 + hideable.len() {
            if width(&plan) <= avail {
                break;
            }
            match step {
                0 => plan.count = false,
                1 => plan.icon_labels = false,
                // Difference's "Amplify" goes before the title, so the bar
                // keeps its left part when the mode changes.
                2 => plan.amp_label = false,
                // The title and "?" (the ? key and the "..." menu have it
                // too) go before the mode names.
                3 => plan.title = false,
                4 => plan.hidden.push(BarPart::Help),
                5 => plan.labels = false,
                n if n - 6 < hideable.len() => plan.hidden.push(hideable[n - 6]),
                _ => plan.modes_menu = true,
            }
        }
        // What went first may fit again once later steps made room (the mode
        // names became icons, a part moved into "...").
        if plan.hidden.contains(&BarPart::Help) {
            plan.hidden.retain(|b| *b != BarPart::Help);
            if width(&plan) > avail {
                plan.hidden.push(BarPart::Help);
            }
        }
        if !plan.title {
            plan.title = true;
            plan.title = width(&plan) <= avail;
        }
        if plan.title && !plan.amp_label {
            plan.amp_label = true;
            plan.amp_label = width(&plan) <= avail;
        }
        if plan.title && !plan.icon_labels {
            plan.icon_labels = true;
            plan.icon_labels = width(&plan) <= avail;
        }
        if plan.icon_labels && !plan.count {
            plan.count = true;
            plan.count = width(&plan) <= avail;
        }
        plan
    }

    fn toggle_sync(&mut self) {
        self.sync = !self.sync;
        if self.sync {
            let v = self.views[self.selected.min(self.views.len() - 1)];
            for w in &mut self.views {
                *w = v;
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let plan = self.bar_plan(ui, ui.available_width());
            let shown = |b: BarPart| plan.shown(b);
            let blind = self.blind();
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back  (Esc)", false).clicked() {
                self.exit = true;
            }
            ui.add_space(4.0);
            if plan.title {
                ui.label(RichText::new("Compare").font(text::title()).color(p.text));
            }
            if plan.count {
                ui.label(RichText::new(format!("{} images", self.panes.len())).size(text::SMALL).color(p.muted));
            }
            let pair = self.panes.len() == 2;
            if pair {
                widgets::separator_v(ui);
            }
            // The view modes need exactly two images; with three or four there is
            // only side by side, so no control is shown.
            if pair && plan.modes_menu {
                let r = ui.menu_button(format!("{}  {}", self.mode.icon(), I::CARET_DOWN), |ui| {
                    for m in MODES {
                        if widgets::menu_toggle(ui, self.mode == m, m.label(), &m.key()).on_hover_text(mode_tip(m)).clicked() {
                            self.set_mode(m);
                            ui.close();
                        }
                    }
                });
                r.response.on_hover_text(format!("View: {} (click to change)", self.mode.label()));
            } else if pair {
                let labels: Vec<String> = MODES.iter().map(|m| mode_label(*m, plan.labels)).collect();
                let labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                let tips: Vec<String> = MODES.iter().map(|m| mode_tip(*m)).collect();
                let tips: Vec<&str> = tips.iter().map(|s| s.as_str()).collect();
                let mut idx = MODES.iter().position(|m| *m == self.mode).unwrap_or(0);
                if widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) {
                    self.set_mode(MODES[idx]);
                }
            }
            if shown(BarPart::Sync) {
                widgets::separator_v(ui);
                let (icon, t) = if self.sync { (I::LINK, tip("Zoom & pan synchronized", Command::ToggleLink)) } else { (I::LINK_BREAK, tip("Zoom & pan independent", Command::ToggleLink)) };
                if bar_button(ui, icon, SYNC_LABEL, &t, self.sync, plan.icon_labels).clicked() {
                    self.toggle_sync();
                }
            }
            if shown(BarPart::Basis) && self.basis_matters() {
                let mut basis = self.basis;
                widgets::combo("cmp-basis")
                    .width(118.0)
                    .selected_text(match basis {
                        Basis::Relative => "Match framing",
                        Basis::Pixel => "Match pixels",
                    })
                    .show_ui(ui, |ui| Self::basis_choices(ui, &mut basis))
                    .response
                    .on_hover_text("How linked panes line up: the same field of view (framing) or the same pixel scale");
                self.set_basis(basis);
            }
            widgets::separator_v(ui);
            let rects = self.layout(self.last_area);
            let ppp = ui.ctx().pixels_per_point();
            let src = self.selected.min(self.panes.len().saturating_sub(1));
            if shown(BarPart::Zoom) {
                if widgets::icon_button(ui, I::MAGNIFYING_GLASS_MINUS, &tip("Zoom out", Command::ZoomOut), false).clicked() && !rects.is_empty() {
                    self.zoom_step(src, &rects, ppp, 0.8, None);
                }
                let z = if rects.is_empty() { None } else { self.effective(src, &rects, ppp, self.fit_ref(&rects, ppp)).map(|e| e.0) };
                let zl = ui.add_sized([ZOOM_LABEL_W, size::COMPACT], egui::Button::new(RichText::new(z.map(|z| format!("{:.0}%", z * 100.0)).unwrap_or_else(|| " - ".into())).size(12.0)).frame(false));
                if zl.on_hover_text(tip("Zoom of the selected image. Click for 100% (actual pixels)", Command::ZoomActual)).clicked() && !rects.is_empty() {
                    self.actual_size(&rects, ppp);
                }
                if widgets::icon_button(ui, I::MAGNIFYING_GLASS_PLUS, &tip("Zoom in", Command::ZoomIn), false).clicked() && !rects.is_empty() {
                    self.zoom_step(src, &rects, ppp, 1.25, None);
                }
            }
            let fitted = self.views.iter().all(|v| v.fit);
            let fit_keys = key_text(Command::ZoomFit);
            let fit_tip = if fit_keys.is_empty() { "Fit the whole image in each pane  (double-click)".to_string() } else { format!("Fit the whole image in each pane  ({fit_keys} or double-click)") };
            if widgets::icon_button(ui, I::ARROWS_IN, &fit_tip, fitted).clicked() {
                self.fit_all();
            }
            if shown(BarPart::Views) {
                widgets::separator_v(ui);
                // Camera data and file names stay hidden in a blind test.
                let tip = if blind {
                    "Show: histograms, pixel values, inspect views and how the panes are laid out"
                } else {
                    "Show: histograms, camera info, pixel values, inspect views, how the panes are laid out, and the Metadata window"
                };
                let open_id = ui.scope_id().with("cmp-show-open");
                let was_open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
                let r = widgets::labeled_menu_button(ui, I::STACK, SHOW_LABEL, tip, was_open);
                let open = egui::Popup::menu(&r).show(|ui| self.show_menu(ui)).is_some();
                ui.data_mut(|d| d.insert_temp(open_id, open));
            }
            // Culling on the selected pane (keys act on the pane under the pointer).
            let sel = self.selected.min(self.panes.len().saturating_sub(1));
            let letter = LETTERS[sel.min(3)];
            if shown(BarPart::Cull) {
                widgets::separator_v(ui);
                if self.panes.len() >= 2 {
                    // The one filled call to action of the surface.
                    let r = ui.scope(|ui| {
                        ui.spacing_mut().button_padding.x = 10.0;
                        widgets::success_button(ui, &p, &format!("{}  Keep {letter}", I::CHECK_CIRCLE))
                    });
                    if r.inner.on_hover_text(tip(&format!("Keep {letter}: flag it and mark the other images as rejected (red label)"), Command::KeepOnly)).clicked() {
                        self.keep_only(core, sel);
                    }
                }
                if !self.panes.is_empty() {
                    let flagged = core.tags.get(&self.panes[sel].path).flagged;
                    if widgets::icon_button(ui, I::FLAG, &tip(&format!("Flag {letter}"), Command::ToggleFlag), flagged).clicked() {
                        self.toggle_flag(core, sel);
                    }
                    if widgets::icon_button(ui, I::TRASH, &tip(&self.delete_label(sel), Command::Delete), false).clicked() {
                        self.confirm_delete = Some(sel);
                    }
                }
            }
            if shown(BarPart::Swap) && pair && bar_button(ui, I::ARROWS_LEFT_RIGHT, SWAP_LABEL, &tip("Swap A and B", Command::SwapAB), false, plan.icon_labels).clicked() {
                self.swap_ab();
            }
            // Mode-specific controls.
            if shown(BarPart::Extras) && pair && self.mode != CmpMode::SideBySide {
                widgets::separator_v(ui);
                self.mode_controls(ui, &p, plan.amp_label);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if shown(BarPart::Help) && widgets::icon_button(ui, I::QUESTION, &tip("Shortcuts", Command::CompareHelp), self.help).clicked() {
                    self.help = !self.help;
                }
                ui.menu_button(RichText::new(I::DOTS_THREE).size(17.0), |ui| self.overflow_menu(ui, core, &plan, sel)).response.on_hover_text("More: export a report and the tools that do not fit here");
                if shown(BarPart::Review) {
                    if bar_button(ui, I::CHAT_TEXT, REVIEW_LABEL, &tip("Review panel: comments, notes and report export", Command::ToggleReview), self.rv.panel, plan.icon_labels).clicked() {
                        self.toggle_review(core);
                    }
                    let t = tip(if self.markup_allowed() { "Mark up: draw boxes, circles, arrows and details on the images" } else { "Mark up works in Side by side and Split" }, Command::ToggleMarkup);
                    if bar_button(ui, I::PENCIL_SIMPLE_LINE, MARKUP_LABEL, &t, self.rv.markup && self.markup_allowed(), plan.icon_labels).clicked() {
                        self.toggle_markup(core);
                    }
                }
                crate::analysis_ui::toolbar_button(ui, core);
                widgets::separator_v(ui);
            });
        });
    }

    /// The Show menu: what is drawn over the images, how Side by side lays
    /// out the panes, and the Metadata window.
    fn show_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(250.0);
        let blind = self.blind();
        let toggle = |ui: &mut Ui, on: bool, icon: &str, label: &str, key: &str, tip: &str| {
            let r = widgets::menu_check(ui, on, icon, label, key).on_hover_text(tip).clicked();
            if r {
                ui.close();
            }
            r
        };
        if toggle(ui, self.show_hist, I::CHART_BAR, "Histograms", &key_text(Command::ToggleHistogram), "A histogram in the corner of every pane") {
            self.show_hist = !self.show_hist;
        }
        // Camera data and file names stay hidden in a blind test.
        if !blind && toggle(ui, self.show_exif, I::CAMERA, "Camera info strip", &key_text(Command::ToggleCameraInfo), "Camera, lens, exposure, aperture, ISO and focal length under every image") {
            self.show_exif = !self.show_exif;
        }
        if toggle(ui, self.probe, I::EYEDROPPER, "Pixel values", &key_text(Command::TogglePixelValues), "RGB, hex, L*a*b* and luma under the pointer, at the same spot in every pane") {
            self.probe = !self.probe;
        }
        widgets::submenu(ui, I::EYE, "Inspect view", |ui| {
            for m in super::inspect_view::InspectMode::ALL {
                if widgets::menu_toggle(ui, self.inspect == m, m.label(), "").on_hover_text(m.tip()).clicked() {
                    self.inspect = m;
                    ui.close();
                }
            }
        });
        if !self.pair_mode() && self.panes.len() >= 2 {
            ui.separator();
            menu_caption(ui, "Layout");
            let auto = self.auto_layout(self.last_area).kind();
            for l in PaneLayout::ALL {
                if l == PaneLayout::Grid && self.panes.len() < 3 {
                    continue;
                }
                let label = if l == PaneLayout::Auto { format!("Auto ({})", auto.label().to_lowercase()) } else { l.label().to_string() };
                if widgets::menu_check(ui, self.arrange == l, l.icon(), &label, "").on_hover_text(l.tip()).clicked() {
                    self.arrange = l;
                    ui.close();
                }
            }
        }
        if !blind {
            ui.separator();
            let cmd = crate::ui::commands::Command::ShowMetadata;
            let tip = "Metadata of every image side by side: capture features (HDR, night, zoom, lens), every EXIF tag, JPEG quality, color profile, XMP. Opens on the tab you used last";
            if widgets::menu_item(ui, cmd.icon(), "Metadata...", &cmd.shortcut()).on_hover_text(tip).clicked() {
                self.info_request = Some(InfoRequest::Metadata);
                ui.close();
            }
        }
    }

    fn basis_choices(ui: &mut Ui, basis: &mut Basis) {
        ui.selectable_value(basis, Basis::Relative, "Match framing").on_hover_text("Same field of view: images of different sizes show the same part of the scene");
        ui.selectable_value(basis, Basis::Pixel, "Match pixels").on_hover_text("Same pixel scale: 100% in one pane is 100% in all");
    }

    /// The difference gain (with its "Amplify" word when `label`) or the
    /// divider position.
    fn mode_controls(&mut self, ui: &mut Ui, p: &super::theme::Palette, label: bool) {
        match self.mode {
            CmpMode::Difference => {
                let tip = "Amplify: multiplies the difference so small changes become visible (does not change the numbers)";
                if label {
                    ui.label(RichText::new("Amplify").size(text::SMALL).color(p.muted)).on_hover_text(tip);
                }
                ui.add(widgets::Slider::new(&mut self.amp, 1.0..=50.0).logarithmic(true).prefix("×").fixed_decimals(0).default_value(1.0).width(AMP_RAIL_W).value_width(AMP_VALUE_W)).on_hover_text(tip);
            }
            CmpMode::Split => {
                let jump = key_text(Command::JumpDivider);
                let t = if jump.is_empty() { "Where the divider is; drag it on the image".to_string() } else { format!("Where the divider is; drag it on the image  ({jump} jumps it to one side)") };
                ui.label(RichText::new(format!("Divider {:.0}%", self.split * 100.0)).size(text::SMALL).color(p.muted)).on_hover_text(t);
            }
            CmpMode::SideBySide => {}
        }
    }

    /// The toolbar parts that did not fit, as menu entries.
    fn overflow_menu(&mut self, ui: &mut Ui, core: &mut Core, plan: &BarPlan, sel: usize) {
        ui.set_min_width(240.0);
        let p = core.pal;
        let pair = self.panes.len() == 2;
        let letter = LETTERS[sel.min(3)];
        // `on`: an on / off setting (check mark while on); `None`: a plain command.
        let item = |ui: &mut Ui, icon: &str, label: &str, key: &str, on: Option<bool>| match on {
            Some(on) => widgets::menu_check(ui, on, icon, label, key).clicked(),
            None => widgets::menu_item(ui, icon, label, key).clicked(),
        };
        let busy = self.align_job.is_some();
        let label = if busy { "Aligning..." } else { "Align to A (match field of view)" };
        if widgets::menu_check(ui, self.align_on, I::FRAME_CORNERS, label, &key_text(Command::AlignToA))
            .on_hover_text("Find the same scene content in every image and warp B-D onto A, so phones with different lenses and crops line up pixel for pixel; every pane then zooms to the view they all share. The difference view uses the aligned images. Mark up works on the originals.")
            .clicked()
        {
            self.toggle_alignment(core);
            ui.close();
        }
        // No report of a blind test: it would show the files.
        if !self.blind() {
            ui.separator();
            let exporting = self.rv.exporting();
            let r = ui.add_enabled_ui(!exporting, |ui| widgets::menu_item(ui, I::EXPORT, if exporting { "Exporting report..." } else { "Export report..." }, &key_text(Command::ExportReport))).inner;
            if r.on_hover_text("Save the review (images with marks, comments, details) as PowerPoint, PDF or a web page").clicked() {
                self.rv.open_export();
                ui.close();
            }
        }
        for part in BarPart::ALL.iter().rev().filter(|b| plan.hidden.contains(b)) {
            let sep = |ui: &mut Ui| {
                ui.separator();
            };
            match part {
                BarPart::Review => {
                    sep(ui);
                    if item(ui, I::PENCIL_SIMPLE_LINE, "Mark up", &key_text(Command::ToggleMarkup), Some(self.rv.markup)) {
                        self.toggle_markup(core);
                    }
                    if item(ui, I::CHAT_TEXT, "Review panel", &key_text(Command::ToggleReview), Some(self.rv.panel)) {
                        self.toggle_review(core);
                    }
                }
                BarPart::Sync => {
                    if item(ui, I::LINK, "Link zoom & pan", &key_text(Command::ToggleLink), Some(self.sync)) {
                        self.toggle_sync();
                    }
                }
                BarPart::Basis if self.basis_matters() => {
                    let mut basis = self.basis;
                    widgets::submenu(ui, I::FRAME_CORNERS, "Framing", |ui| {
                        if widgets::menu_toggle(ui, basis == Basis::Relative, "Match framing", "").on_hover_text("Same relative position/zoom: images of different sizes stay aligned").clicked() {
                            basis = Basis::Relative;
                        }
                        if widgets::menu_toggle(ui, basis == Basis::Pixel, "Match pixels", "").on_hover_text("Same pixel scale: 100% in one pane is 100% in all").clicked() {
                            basis = Basis::Pixel;
                        }
                    });
                    self.set_basis(basis);
                }
                BarPart::Views => {
                    widgets::submenu(ui, I::STACK, SHOW_LABEL, |ui| self.show_menu(ui));
                }
                BarPart::Cull if !self.panes.is_empty() => {
                    sep(ui);
                    if self.panes.len() >= 2 && item(ui, I::CHECK_CIRCLE, &format!("Keep {letter}, reject the others"), &key_text(Command::KeepOnly), None) {
                        self.keep_only(core, sel);
                    }
                    let flagged = core.tags.get(&self.panes[sel].path).flagged;
                    if item(ui, I::FLAG, &format!("Flag {letter}"), &key_text(Command::ToggleFlag), Some(flagged)) {
                        self.toggle_flag(core, sel);
                    }
                    if item(ui, I::TRASH, &self.delete_label(sel), &key_text(Command::Delete), None) {
                        self.confirm_delete = Some(sel);
                        ui.close();
                    }
                }
                BarPart::Swap if pair => {
                    if item(ui, I::ARROWS_LEFT_RIGHT, "Swap A and B", &key_text(Command::SwapAB), None) {
                        self.swap_ab();
                    }
                }
                BarPart::Extras if pair && self.mode != CmpMode::SideBySide => {
                    sep(ui);
                    ui.horizontal(|ui| self.mode_controls(ui, &p, true));
                }
                BarPart::Help => {
                    if item(ui, I::QUESTION, "Shortcuts", &key_text(Command::CompareHelp), Some(self.help)) {
                        self.help = !self.help;
                    }
                }
                BarPart::Zoom => {
                    let rects = self.layout(self.last_area);
                    let ppp = ui.ctx().pixels_per_point();
                    let src = self.selected.min(self.panes.len().saturating_sub(1));
                    if !rects.is_empty() {
                        if item(ui, I::MAGNIFYING_GLASS_PLUS, "Zoom in", &key_text(Command::ZoomIn), None) {
                            self.zoom_step(src, &rects, ppp, 1.25, None);
                        }
                        if item(ui, I::MAGNIFYING_GLASS_MINUS, "Zoom out", &key_text(Command::ZoomOut), None) {
                            self.zoom_step(src, &rects, ppp, 0.8, None);
                        }
                        if item(ui, I::ARROWS_OUT, "Actual size (100%)", &key_text(Command::ZoomActual), None) {
                            self.actual_size(&rects, ppp);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn help_overlay(&self, ui: &Ui, area: Rect) {
        let rows = help_rows(&keymap::active(), self.link.as_ref().map(|l| l.blind));
        let font = FontId::proportional(13.0);
        let width = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x;
        let key_w = rows.iter().map(|(k, _)| width(k)).fold(0.0, f32::max).clamp(90.0, 220.0);
        let text_w = rows.iter().map(|(_, v)| width(v)).fold(0.0, f32::max);
        let col_w = 22.0 + key_w + 18.0 + text_w + 22.0;
        // Two columns when one would not fit the height (and two fit the width).
        let one_col_h = 60.0 + rows.len() as f32 * 24.0;
        let cols = if one_col_h > area.height() - 16.0 && 2.0 * col_w <= area.width() - 16.0 { 2 } else { 1 };
        let per_col = rows.len().div_ceil(cols);
        let r = Rect::from_center_size(area.center(), Vec2::new(col_w * cols as f32, 60.0 + per_col as f32 * 24.0).min(area.size() - Vec2::splat(8.0)));
        let painter = ui.painter().with_clip_rect(r);
        painter.rect_filled(r, CornerRadius::same(14), Color32::from_rgba_unmultiplied(18, 19, 22, 240));
        painter.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, Color32::from_white_alpha(26)), egui::StrokeKind::Inside);
        painter.text(r.min + Vec2::new(22.0, 24.0), Align2::LEFT_CENTER, "Compare shortcuts", super::theme::semibold(15.0), Color32::WHITE);
        for (i, (k, v)) in rows.iter().enumerate() {
            let x = r.min.x + (i / per_col) as f32 * col_w;
            let y = r.min.y + 56.0 + (i % per_col) as f32 * 24.0;
            painter.text(Pos2::new(x + 22.0, y), Align2::LEFT_CENTER, k, font.clone(), Color32::from_gray(150));
            painter.text(Pos2::new(x + 22.0 + key_w + 18.0, y), Align2::LEFT_CENTER, v, font.clone(), Color32::from_gray(235));
        }
    }
}

/// The rows of Compare's shortcuts overlay (keys, what they do) from the
/// keymap; rows of unbound commands are left out. `blind`: `None` outside a
/// project, `Some(true)` in a blind test.
fn help_rows(km: &keymap::Keymap, blind: Option<bool>) -> Vec<(String, String)> {
    use Command::*;
    let first = |c: Command| km.keys_for(KeyMode::Compare, c).first().map(keymap::format_shortcut);
    // The first key of each command, when all of them have one.
    let joined = |cmds: &[Command], sep: &str| -> String { cmds.iter().map(|c| first(*c)).collect::<Option<Vec<String>>>().map(|v| v.join(sep)).unwrap_or_default() };
    let one = |c: Command| keymap::mode_text(km, KeyMode::Compare, c);
    let rates: Vec<String> = (0..=5).filter_map(|n| first(Rate(n))).collect();
    let rate_keys = if rates == ["0", "1", "2", "3", "4", "5"] { "0 - 5".to_string() } else { rates.join(" ") };
    let fit = one(ZoomFit);
    let in_blind = blind == Some(true);
    let mut rows: Vec<(String, &str)> = Vec::new();
    if in_blind {
        let side = |c: Command| keymap::mode_text(km, KeyMode::Blind, c);
        let (l, r) = (side(VoteLeft), side(VoteRight));
        if !l.is_empty() && !r.is_empty() {
            rows.push((format!("{l}  /  {r}"), "The left / right image is better"));
        }
        rows.push((side(VoteSame), "About the same"));
    } else if blind.is_some() {
        rows.push((joined(&[PrevScene, NextScene], " / "), "Previous / next scene of the project"));
        rows.push((one(ToggleScorecard), "Scorecard: verdicts and scores per device"));
    }
    rows.push(("Wheel / pinch".into(), "Zoom at cursor (all panes when linked)"));
    rows.push(("Drag".into(), "Pan (mark up: draw; Space+drag pans)"));
    rows.push((if fit.is_empty() { "Double-click".into() } else { format!("Double-click  or  {fit}") }, "Fit"));
    rows.push((one(ZoomActual), "100% (selected pane)"));
    rows.push((joined(&[ZoomIn, ZoomOut], " / "), "Zoom in / out"));
    if !in_blind {
        rows.push((joined(&[PanLeft, PanRight, PanUp, PanDown], " "), "Pan"));
    }
    rows.push((joined(&[ViewSideBySide, ViewSplit, ViewDifference], "  "), "Side by side · Split · Difference"));
    rows.push((one(JumpDivider), "Split: jump the divider"));
    if !in_blind {
        rows.push((one(ToggleMarkup), "Mark up: boxes, arrows, pen, details"));
        rows.push((one(ToggleReview), "Review panel: comments and notes"));
        rows.push((joined(&[Undo, Redo], "  /  "), "Undo / redo marks"));
        rows.push(("Del".into(), "Delete the selected mark"));
        rows.push((one(ExportReport), "Export report (PowerPoint, PDF, web)"));
        rows.push((one(SwapAB), "Swap A and B"));
    }
    rows.push((one(AlignToA), "Align to A: match the field of view"));
    rows.push((one(TogglePixelValues), "Pixel values at the same spot in every pane"));
    rows.push((one(CycleInspect), "Inspect view: clipping, false color, peaking, channels"));
    if !in_blind {
        rows.push((one(LineProfile), "Line profile: brightness along a line, edge width"));
    }
    rows.push((one(ToggleLink), "Link / unlink zoom & pan"));
    if !in_blind {
        rows.push((rate_keys, "Rate the image under the pointer"));
        rows.push((one(ToggleFlag), "Flag / unflag it"));
        rows.push((one(KeepOnly), "Keep it, reject the others (red label)"));
        rows.push((one(Delete), "Move it to the Recycle Bin (no mark selected)"));
        rows.push((one(ToggleCameraInfo), "Camera info strip"));
    }
    rows.push((one(ToggleHistogram), "Histograms"));
    rows.push((one(CompareHelp), "This help"));
    rows.push(("Esc".into(), "Back"));
    rows.into_iter().filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k, v.to_string())).collect()
}

impl Drop for Compare {
    /// Leaving compare writes the review at once.
    fn drop(&mut self) {
        self.rv.save_now();
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 40.0), Vec2::new(1201.0, 801.0))
    }

    /// The rects of `n` panes in `l` (not Auto).
    fn rects_of(n: usize, l: PaneLayout, a: Rect) -> Vec<Rect> {
        grid_rects(n, l.grid(n).unwrap(), a, 1.0)
    }

    #[test]
    fn layout_two_three_four() {
        let a = area();
        assert_eq!(rects_of(1, PaneLayout::Columns, a), vec![a]);
        let r2 = rects_of(2, PaneLayout::Columns, a);
        assert_eq!(r2.len(), 2);
        assert!((r2[0].width() - 600.0).abs() < 1e-3 && (r2[1].width() - 600.0).abs() < 1e-3);
        assert_eq!(r2[0].min, a.min);
        assert_eq!(r2[1].max, a.max);
        assert!((r2[1].min.x - r2[0].max.x - 1.0).abs() < 1e-3);
        let r3 = rects_of(3, PaneLayout::Columns, a);
        assert_eq!(r3.len(), 3);
        for r in &r3 {
            assert!((r.width() - 399.6667).abs() < 1e-2);
            assert_eq!(r.height(), a.height());
        }
        assert!((r3[2].max.x - a.max.x).abs() < 1e-3);
        let r4 = rects_of(4, PaneLayout::Grid, a);
        assert_eq!(r4.len(), 4);
        assert_eq!(r4[0].min, a.min);
        assert_eq!(r4[3].max, a.max);
        assert!((r4[0].height() - 400.0).abs() < 1e-3);
        assert!((r4[2].min.y - r4[0].max.y - 1.0).abs() < 1e-3);
        assert!((r4[1].min.y - r4[0].min.y).abs() < 1e-6);
        // No overlap.
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    let x = r4[i].intersect(r4[j]);
                    assert!(x.width() <= 0.0 || x.height() <= 0.0);
                }
            }
        }
        // Rows: full width, stacked with the gap between.
        let rows = rects_of(3, PaneLayout::Rows, a);
        assert!(rows.iter().all(|r| r.min.x == a.min.x && r.max.x == a.max.x));
        assert!((rows[1].min.y - rows[0].max.y - 1.0).abs() < 1e-3 && (rows[2].max.y - a.max.y).abs() < 1e-3);
    }

    #[test]
    fn three_in_a_grid_centre_the_last_one() {
        let a = area();
        let r = rects_of(3, PaneLayout::Grid, a);
        assert_eq!(PaneLayout::Grid.grid(3), Some(Grid { cols: 2, rows: 2 }));
        assert_eq!(r.len(), 3);
        // Two on top, the third as big, centred below.
        assert!((r[0].size() - r[2].size()).length() < 1e-3);
        assert!((r[2].center().x - a.center().x).abs() < 1e-3);
        assert!((r[2].min.y - r[0].max.y - 1.0).abs() < 1e-3 && (r[2].max.y - a.max.y).abs() < 1e-3);
        // A grid of two is one row.
        assert_eq!(PaneLayout::Grid.grid(2), Some(Grid { cols: 2, rows: 1 }));
        assert_eq!(PaneLayout::Auto.grid(3), None);
    }

    #[test]
    fn auto_layout_shows_the_images_largest() {
        // The compare area of a 1280 x 800 window (below the toolbar).
        let area = Vec2::new(1280.0, 756.0);
        let landscape = 4.0 / 3.0;
        let portrait = 3.0 / 4.0;
        // Two landscape images: side by side.
        assert_eq!(auto_grid(&[landscape; 2], area, 1.0).kind(), PaneLayout::Columns);
        // Three landscape images: a 2 x 2 grid (31% zoom instead of 27% for 1600 px wide images).
        assert_eq!(auto_grid(&[landscape; 3], area, 1.0), Grid { cols: 2, rows: 2 });
        // Four landscape images: a grid; four portrait ones: one row.
        assert_eq!(auto_grid(&[landscape; 4], area, 1.0).kind(), PaneLayout::Grid);
        assert_eq!(auto_grid(&[portrait; 4], area, 1.0), Grid { cols: 4, rows: 1 });
        // Wide panoramas in a tall area: one above the other.
        assert_eq!(auto_grid(&[3.0, 3.0], Vec2::new(800.0, 1000.0), 1.0).kind(), PaneLayout::Rows);
        // Square images in a square area: a near tie keeps one row.
        assert_eq!(auto_grid(&[1.0; 2], Vec2::new(1000.0, 1000.0), 1.0).kind(), PaneLayout::Columns);
        // An image still loading (no size yet) counts as 4:3; one image is one cell.
        assert_eq!(auto_grid(&[f32::NAN; 3], area, 1.0).kind(), PaneLayout::Grid);
        assert_eq!(auto_grid(&[landscape], area, 1.0), Grid { cols: 1, rows: 1 });
    }

    #[test]
    fn relative_sync_keeps_images_aligned() {
        let pane = Vec2::new(1000.0, 800.0);
        let small = (1000, 500);
        let big = (4000, 2000);
        let v = View::fitted(Basis::Relative);
        let (zs, cs) = pane_transform(&v, Basis::Relative, small, pane, 1.0);
        let (zb, cb) = pane_transform(&v, Basis::Relative, big, pane, 1.0);
        assert!((zs - 1.0).abs() < 1e-6 && (zb - 0.25).abs() < 1e-6);
        assert_eq!(cs, Vec2::new(500.0, 250.0));
        assert_eq!(cb, Vec2::new(2000.0, 1000.0));
        // Zoom at an anchor on the small one; apply the same view to both.
        let anchor = Vec2::new(200.0, -100.0);
        let v2 = zoom_at(&v, Basis::Relative, small, pane, 1.0, 2.0, anchor);
        let (zs2, cs2) = pane_transform(&v2, Basis::Relative, small, pane, 1.0);
        let (zb2, cb2) = pane_transform(&v2, Basis::Relative, big, pane, 1.0);
        assert!((zs2 - 2.0).abs() < 1e-5);
        assert!((zb2 - 0.5).abs() < 1e-5);
        // The anchor maps to the same image point before and after (in the source pane).
        let before = cs + anchor / zs;
        let after = cs2 + anchor / zs2;
        assert!((before - after).length() < 1e-3);
        // Both panes show the same normalized point at their centre.
        assert!((cs2.x / 1000.0 - cb2.x / 4000.0).abs() < 1e-5);
        assert!((cs2.y / 500.0 - cb2.y / 2000.0).abs() < 1e-5);
    }

    #[test]
    fn pixel_sync_uses_same_scale() {
        let pane = Vec2::new(1000.0, 800.0);
        let a = (1000, 500);
        let b = (4000, 2000);
        let fit_ref = fit_of(a, pane).min(fit_of(b, pane));
        assert!((fit_ref - 0.25).abs() < 1e-6);
        let v = View::fitted(Basis::Pixel);
        assert_eq!(pane_transform(&v, Basis::Pixel, a, pane, fit_ref).0, fit_ref);
        let v2 = zoom_at(&v, Basis::Pixel, a, pane, fit_ref, 4.0, Vec2::ZERO);
        let (za, ca) = pane_transform(&v2, Basis::Pixel, a, pane, fit_ref);
        let (zb, cb) = pane_transform(&v2, Basis::Pixel, b, pane, fit_ref);
        assert!((za - 1.0).abs() < 1e-5 && (zb - 1.0).abs() < 1e-5);
        assert_eq!(ca, Vec2::new(500.0, 250.0));
        assert_eq!(cb, Vec2::new(2000.0, 1000.0));
        // Pan moves both by the same number of image pixels.
        let v3 = pan_by(&v2, Basis::Pixel, a, pane, fit_ref, Vec2::new(100.0, 0.0));
        let (_, ca3) = pane_transform(&v3, Basis::Pixel, a, pane, fit_ref);
        let (_, cb3) = pane_transform(&v3, Basis::Pixel, b, pane, fit_ref);
        assert!((ca3.x - 400.0).abs() < 1e-3 && (cb3.x - 1900.0).abs() < 1e-3);
    }

    #[test]
    fn pan_is_clamped() {
        let pane = Vec2::new(500.0, 500.0);
        let v = pan_by(&View::fitted(Basis::Relative), Basis::Relative, (100, 100), pane, 1.0, Vec2::new(1e6, -1e6));
        assert_eq!(v.center, Vec2::new(0.0, 1.0));
    }

    #[test]
    fn dest_rect_centres_image() {
        let pane = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        let d = dest_rect(pane, 1.0, (1000, 500), 1.0, Vec2::new(500.0, 250.0));
        assert_eq!(d, Rect::from_min_size(Pos2::new(0.0, 150.0), Vec2::new(1000.0, 500.0)));
        let d2 = dest_rect(pane, 2.0, (1000, 500), 1.0, Vec2::new(500.0, 250.0));
        assert_eq!(d2.size(), Vec2::new(500.0, 250.0));
    }

    fn solid(w: u32, h: u32, v: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(v))
    }

    #[test]
    fn difference_of_identical_is_zero() {
        let a = RgbaImage::from_fn(64, 33, |x, y| image::Rgba([(x * 3) as u8, (y * 7) as u8, 200, 255]));
        let (d, s) = difference(&a, &a);
        assert!(d.pixels().all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] == 255));
        assert_eq!(s.mse, 0.0);
        assert!(s.psnr.is_infinite());
        assert_eq!(s.max, 0);
        assert_eq!(s.changed_pct, 0.0);
    }

    #[test]
    fn difference_stats_and_psnr() {
        let a = solid(100, 50, [100, 100, 100, 255]);
        let b = solid(100, 50, [110, 90, 100, 255]);
        let (d, s) = difference(&a, &b);
        assert_eq!(d.get_pixel(3, 4).0, [10, 10, 0, 255]);
        assert!((s.mean - 20.0 / 3.0).abs() < 1e-9);
        assert!((s.mse - 200.0 / 3.0).abs() < 1e-9);
        let expect = 10.0 * (65025.0f64 / (200.0 / 3.0)).log10();
        assert!((s.psnr - expect).abs() < 1e-9);
        assert_eq!(s.max, 10);
        assert!((s.changed_pct - 100.0).abs() < 1e-9);
        // Half the image different.
        let mut c = a.clone();
        for y in 0..25 {
            for x in 0..100 {
                c.put_pixel(x, y, image::Rgba([0, 100, 100, 255]));
            }
        }
        let (_, s2) = difference(&a, &c);
        assert!((s2.changed_pct - 50.0).abs() < 1e-9);
        assert_eq!(s2.max, 100);
        assert!((psnr_from_mse(65025.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn keep_reject_flags_one_and_labels_the_rest() {
        use crate::tags::Tag;
        let tags = [
            Tag { rating: 3, flagged: false, label: REJECT_LABEL },
            Tag { rating: 0, flagged: true, label: 0 },
            Tag { rating: 5, flagged: false, label: 4 },
        ];
        let out = keep_reject(&tags, 0);
        assert_eq!(out[0], Tag { rating: 3, flagged: true, label: 0 });
        assert_eq!(out[1], Tag { rating: 0, flagged: false, label: REJECT_LABEL });
        assert_eq!(out[2], Tag { rating: 5, flagged: false, label: REJECT_LABEL });
        // A non-red label on the kept image survives.
        let out = keep_reject(&tags, 2);
        assert_eq!(out[2], Tag { rating: 5, flagged: true, label: 4 });
        assert!(out[0].label == REJECT_LABEL && !out[1].flagged);
    }

    #[test]
    fn exif_line_joins_known_fields() {
        let mut e = crate::meta::ExifSummary::default();
        assert_eq!(exif_line(&e), "");
        e.camera = Some("Canon EOS R5".into());
        e.exposure = Some("1/250 s".into());
        e.aperture = Some("f/4.0".into());
        e.iso = Some("400".into());
        e.focal = Some("50 mm".into());
        assert_eq!(exif_line(&e), "Canon EOS R5  ·  1/250 s  ·  f/4.0  ·  ISO 400  ·  50 mm");
        e.iso = Some("ISO 800".into());
        e.lens = Some("RF24-70mm".into());
        assert_eq!(exif_line(&e), "Canon EOS R5  ·  RF24-70mm  ·  1/250 s  ·  f/4.0  ·  ISO 800  ·  50 mm");
    }

    #[test]
    fn pane_at_finds_the_pane() {
        let rects = rects_of(4, PaneLayout::Grid, area());
        assert_eq!(pane_at(&rects, Pos2::new(10.0, 50.0)), Some(0));
        assert_eq!(pane_at(&rects, Pos2::new(1190.0, 50.0)), Some(1));
        assert_eq!(pane_at(&rects, Pos2::new(10.0, 830.0)), Some(2));
        assert_eq!(pane_at(&rects, Pos2::new(1190.0, 830.0)), Some(3));
        assert_eq!(pane_at(&rects, Pos2::new(10.0, 10.0)), None);
    }

    #[test]
    fn starts_side_by_side_linked_with_same_framing() {
        assert_eq!(START, (CmpMode::SideBySide, true, Basis::Relative));
        assert_eq!(MODES, [CmpMode::SideBySide, CmpMode::Split, CmpMode::Difference]);
    }

    #[test]
    fn edge_width_measures_the_rise() {
        // A ramp from 20 to 220 over 10 samples (10%-90% = 8 samples).
        let s: Vec<f32> = (0..40).map(|i| if i < 15 { 20.0 } else if i >= 25 { 220.0 } else { 20.0 + 20.0 * (i - 15) as f32 }).collect();
        let w = edge_width(&s).unwrap();
        assert!((w - 8.0).abs() < 1.6, "{w}");
        let falling: Vec<f32> = s.iter().rev().copied().collect();
        assert!((edge_width(&falling).unwrap() - w).abs() < 0.5);
        assert_eq!(edge_width(&[100.0; 30]), None);
        // A sharper step is narrower.
        let sharp: Vec<f32> = (0..40).map(|i| if i < 20 { 20.0 } else { 220.0 }).collect();
        assert!(edge_width(&sharp).unwrap() < w);
    }

    #[test]
    fn amplify_saturates() {
        let mut d = solid(4, 4, [10, 100, 0, 255]);
        d.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        let a = amplify(&d, 4.0);
        assert_eq!(a.get_pixel(1, 1).0, [40, 255, 0, 255]);
        assert_eq!(a.get_pixel(0, 0).0, [4, 8, 12, 255]);
        assert_eq!(amplify(&d, 1.0), d);
    }

    #[test]
    fn help_follows_the_keymap() {
        use crate::ui::keymap::{parse_shortcut, Keymap, Preset};
        let row = |rows: &[(String, String)], what: &str| rows.iter().find(|(_, v)| v == what).map(|(k, _)| k.clone());
        let km = Keymap::preset(Preset::Lumen);
        let rows = help_rows(&km, None);
        // The keys the overlay always listed.
        assert_eq!(row(&rows, "Side by side · Split · Difference").as_deref(), Some("B  W  D"));
        assert_eq!(row(&rows, "Rate the image under the pointer").as_deref(), Some("0 - 5"));
        assert_eq!(row(&rows, "Flag / unflag it").as_deref(), Some("Q / `"));
        assert_eq!(row(&rows, "Undo / redo marks").as_deref(), Some("Ctrl+Z  /  Ctrl+Y"));
        assert_eq!(row(&rows, "Fit").as_deref(), Some("Double-click  or  /"));
        assert_eq!(row(&rows, "Swap A and B").as_deref(), Some("X"));
        assert!(row(&rows, "Previous / next scene of the project").is_none());
        // A remapped key shows its new key; an unbound command has no row.
        let mut km2 = km.clone();
        km2.set_keys(KeyMode::Compare, Command::SwapAB, vec![parse_shortcut("Y").unwrap()]);
        km2.set_keys(KeyMode::Compare, Command::AlignToA, vec![]);
        let rows = help_rows(&km2, None);
        assert_eq!(row(&rows, "Swap A and B").as_deref(), Some("Y"));
        assert!(row(&rows, "Align to A: match the field of view").is_none());
        // Project scenes and blind tests.
        let rows = help_rows(&km, Some(false));
        assert_eq!(row(&rows, "Previous / next scene of the project").as_deref(), Some("PgUp / PgDn"));
        let rows = help_rows(&km, Some(true));
        assert_eq!(row(&rows, "The left / right image is better").as_deref(), Some("Left  /  Right"));
        assert_eq!(row(&rows, "About the same").as_deref(), Some("Down / Space"));
        assert!(row(&rows, "Swap A and B").is_none() && row(&rows, "Pan").is_none());
        assert!(rows.iter().all(|(k, _)| !k.is_empty()));
    }

    #[test]
    fn nothing_but_votes_and_viewing_in_a_blind_test() {
        for c in [Command::Rate(3), Command::ToggleFlag, Command::KeepOnly, Command::Delete, Command::ToggleMarkup, Command::ToggleReview, Command::ToggleCameraInfo, Command::SwapAB, Command::LineProfile, Command::ExportReport] {
            assert!(blind_off(c), "{c:?}");
        }
        for c in [Command::ZoomIn, Command::ToggleHistogram, Command::TogglePixelValues, Command::CycleInspect, Command::AlignToA, Command::VoteLeft, Command::NextScene, Command::CompareHelp] {
            assert!(!blind_off(c), "{c:?}");
        }
    }
}
