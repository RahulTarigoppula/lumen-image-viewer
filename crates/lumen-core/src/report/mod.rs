//! Review reports: a compare session (images with their marks, comments,
//! detail crops and metrics) laid out as slides and written as PowerPoint
//! (`.pptx`), PDF or a self-contained HTML page.
//!
//! The flow is: the app builds a [`Report`] (pixels already annotated and
//! encoded with [`Picture::from_rgba`]), [`layout`] turns it into
//! [`Slide`]s of positioned boxes in points (a 960 x 540 pt 16:9 page), and
//! each writer renders those boxes. PPTX and PDF share the layout, so both
//! look the same; HTML is a flowing page built from the report itself.
//!
//! Company branding ([`Branding`], resolved into a [`Brand`]) adds a logo,
//! a company name, an accent color and a footer line to every page and a
//! white cover page; without it reports keep Lumen's own look.

pub mod brand;
pub mod deck;
pub mod html;
pub mod pdf;
pub mod pptx;

pub use brand::{Brand, Branding};

use anyhow::Result;
use image::RgbaImage;
use std::path::Path;

/// Page size in points (16:9, the PowerPoint default of 13.333 x 7.5 in).
pub const PAGE_W: f32 = 960.0;
pub const PAGE_H: f32 = 540.0;

/// Output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Pptx,
    Pdf,
    Html,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Pptx, Format::Pdf, Format::Html];
    pub fn label(self) -> &'static str {
        match self {
            Format::Pptx => "PowerPoint (.pptx)",
            Format::Pdf => "PDF document (.pdf)",
            Format::Html => "Web page (.html)",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Format::Pptx => "pptx",
            Format::Pdf => "pdf",
            Format::Html => "html",
        }
    }
    /// What the format is good for, for the export dialog.
    pub fn hint(self) -> &'static str {
        match self {
            Format::Pptx => "Editable slides: present, restyle or merge into another deck.",
            Format::Pdf => "Fixed pages that look the same everywhere: share or archive.",
            Format::Html => "One file that opens in any browser: email or attach to a ticket.",
        }
    }
}

/// Which slides to include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    /// Title page with the report name, date and author.
    pub title: bool,
    /// All images side by side on one page with the overall comments.
    pub overview: bool,
    /// One page per image: the image large, its comment and numbered notes.
    pub per_image: bool,
    /// One page per detail crop (the same region from every image).
    pub details: bool,
    /// A table of the measured numbers per image.
    pub metrics: bool,
}

impl Default for Sections {
    fn default() -> Self {
        Sections { title: true, overview: true, per_image: true, details: true, metrics: true }
    }
}

/// An encoded JPEG with its size.
#[derive(Clone, Debug)]
pub struct Picture {
    pub jpeg: std::sync::Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

impl Picture {
    /// Encode `img` (alpha flattened on white) as a JPEG no larger than
    /// `max_side` pixels on its long side.
    pub fn from_rgba(img: &RgbaImage, max_side: u32) -> Result<Picture> {
        Self::from_rgba_quality(img, max_side, 88)
    }

    /// [`Picture::from_rgba`] at JPEG `quality` (1-100).
    pub fn from_rgba_quality(img: &RgbaImage, max_side: u32, quality: u8) -> Result<Picture> {
        let (w, h) = img.dimensions();
        anyhow::ensure!(w > 0 && h > 0, "empty image");
        let scaled;
        let src = if w.max(h) > max_side {
            scaled = crate::imaging::resize_to_fit(img, max_side, max_side);
            &scaled
        } else {
            img
        };
        let (w, h) = src.dimensions();
        let rgb = crate::encode::flatten_rgb(src, [255, 255, 255]);
        let mut out = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)?;
        Ok(Picture { jpeg: std::sync::Arc::new(out), w, h })
    }
    pub fn aspect(&self) -> f32 {
        self.w.max(1) as f32 / self.h.max(1) as f32
    }
}

/// One compared image.
#[derive(Clone, Debug)]
pub struct Item {
    /// "A", "B", ...
    pub label: String,
    /// File name.
    pub name: String,
    /// "4000 × 3000 · 3.2 MB · Canon EOS R5".
    pub details: String,
    /// The image with its marks drawn in.
    pub picture: Picture,
    /// The reviewer's comment on this image.
    pub comment: String,
    /// Notes for the numbered marks, in order (mark 1 is `notes[0]`).
    pub notes: Vec<String>,
}

/// One detail crop: the same region cut from every image.
#[derive(Clone, Debug)]
pub struct Detail {
    pub title: String,
    pub comment: String,
    /// (image label, crop).
    pub crops: Vec<(String, Picture)>,
}

/// A table of numbers: `header[0]` names the row label column.
#[derive(Clone, Debug, Default)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// One line under the table explaining how to read it.
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub title: String,
    pub author: String,
    /// Shown on the title page, e.g. "1 November 2026".
    pub date: String,
    /// The overall verdict / comments on the comparison.
    pub summary: String,
    pub items: Vec<Item>,
    pub details: Vec<Detail>,
    pub metrics: Option<Table>,
    pub sections: Sections,
    /// Company branding (default: Lumen's look).
    pub brand: Brand,
}

/// Accent colours per image label, as RGB.
pub const LABEL_COLORS: [[u8; 3]; 4] = [[47, 111, 235], [234, 120, 32], [34, 160, 92], [142, 84, 214]];
pub const INK: [u8; 3] = [32, 36, 44];
pub const MUTED: [u8; 3] = [104, 112, 124];
pub const RULE: [u8; 3] = [222, 226, 232];
pub const PANEL: [u8; 3] = [245, 247, 250];
pub const ACCENT: [u8; 3] = [47, 111, 235];

pub fn label_color(i: usize) -> [u8; 3] {
    LABEL_COLORS[i % LABEL_COLORS.len()]
}

// ------------------------------------------------------------------ layout

/// A rectangle in points from the page's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Box2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Box2 {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Box2 {
        Box2 { x, y, w: w.max(0.0), h: h.max(0.0) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug)]
pub struct TextEl {
    pub bx: Box2,
    pub text: String,
    pub size: f32,
    pub bold: bool,
    pub color: [u8; 3],
    pub align: Align,
}

#[derive(Clone, Debug)]
pub enum El {
    Fill { bx: Box2, color: [u8; 3] },
    /// An outline only.
    Frame { bx: Box2, color: [u8; 3], width: f32 },
    Image { bx: Box2, pic: Picture },
    Text(TextEl),
    /// A table; `head` fills the header row.
    Table { bx: Box2, table: Table, head: [u8; 3] },
}

#[derive(Clone, Debug, Default)]
pub struct Slide {
    pub els: Vec<El>,
    /// Speaker notes (PowerPoint only).
    pub notes: String,
}

const M: f32 = 36.0; // page margin
const TITLE_H: f32 = 44.0;
const BODY_TOP: f32 = M + TITLE_H + 12.0;
const FOOT_H: f32 = 20.0;

/// Approximate width of `s` in points at `size` (Helvetica metrics), used to
/// wrap and fit text the same way in every writer.
pub fn text_width(s: &str, size: f32, bold: bool) -> f32 {
    let w: u32 = s.chars().map(|c| helv_width(c) as u32).sum();
    w as f32 / 1000.0 * size * if bold { 1.06 } else { 1.0 }
}

fn helv_width(c: char) -> u16 {
    match c {
        ' ' | '!' | ',' | '.' | '/' | ':' | ';' | '[' | ']' | '\\' | 'f' | 't' | 'I' => 278,
        'i' | 'j' | 'l' => 222,
        '\'' => 191,
        '"' => 355,
        '-' | '(' | ')' | 'r' | '`' => 333,
        '*' => 389,
        '+' | '=' | '<' | '>' | '~' | '×' => 584,
        '%' => 889,
        '&' | 'A' | 'B' | 'E' | 'K' | 'P' | 'S' | 'V' | 'X' | 'Y' => 667,
        '@' => 1015,
        'm' | 'M' => 833,
        'w' | 'C' | 'D' | 'H' | 'N' | 'R' | 'U' => 722,
        'W' => 944,
        'F' | 'T' | 'Z' => 611,
        'G' | 'O' | 'Q' => 778,
        'J' | 'c' | 'k' | 's' | 'v' | 'x' | 'y' | 'z' => 500,
        'L' => 556,
        '·' | '•' => 350,
        '\u{2014}' => 1000,
        '\u{2013}' => 556,
        _ if c.is_ascii() => 556,
        _ => 600,
    }
}

/// Split `text` into lines no wider than `width` points (paragraph breaks kept).
pub fn wrap(text: &str, size: f32, bold: bool, width: f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let cand = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if text_width(&cand, size, bold) <= width || line.is_empty() {
                line = cand;
                // A single word wider than the box: hard-break it.
                while text_width(&line, size, bold) > width && line.chars().count() > 1 {
                    let mut cut = line.chars().count() - 1;
                    while cut > 1 && text_width(&line.chars().take(cut).collect::<String>(), size, bold) > width {
                        cut -= 1;
                    }
                    out.push(line.chars().take(cut).collect());
                    line = line.chars().skip(cut).collect();
                }
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        out.push(line);
    }
    out
}

/// Line height for `size`.
pub fn leading(size: f32) -> f32 {
    size * 1.25
}

/// The largest font size from `sizes` (descending) at which `text` fits the
/// box; the smallest otherwise, and the text is cut with an ellipsis.
fn fit_text(text: &str, bx: Box2, sizes: &[f32], bold: bool) -> (f32, String) {
    for &s in sizes {
        let lines = wrap(text, s, bold, bx.w);
        if lines.len() as f32 * leading(s) <= bx.h {
            return (s, text.to_string());
        }
    }
    let s = *sizes.last().unwrap_or(&10.0);
    let max_lines = ((bx.h / leading(s)).floor() as usize).max(1);
    let mut lines = wrap(text, s, bold, bx.w);
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(l) = lines.last_mut() {
            while !l.is_empty() && text_width(&format!("{l}\u{2026}"), s, bold) > bx.w {
                l.pop();
            }
            l.push('\u{2026}');
        }
    }
    (s, lines.join("\n"))
}

fn text(bx: Box2, t: impl Into<String>, size: f32, bold: bool, color: [u8; 3]) -> El {
    El::Text(TextEl { bx, text: t.into(), size, bold, color, align: Align::Left })
}

fn fitted(bx: Box2, t: &str, sizes: &[f32], bold: bool, color: [u8; 3]) -> El {
    let (size, t) = fit_text(t, bx, sizes, bold);
    text(bx, t, size, bold, color)
}

/// Fit an image of aspect `a` centred inside `bx`.
pub fn contain(bx: Box2, a: f32) -> Box2 {
    let (mut w, mut h) = (bx.w, bx.w / a.max(1e-3));
    if h > bx.h {
        h = bx.h;
        w = h * a;
    }
    Box2::new(bx.x + (bx.w - w) / 2.0, bx.y + (bx.h - h) / 2.0, w, h)
}

/// Best grid (cols, rows) for `n` pictures of aspect ratios `aspects` inside
/// a `w` x `h` area: the one that shows the images largest.
pub fn best_grid(aspects: &[f32], w: f32, h: f32, gap: f32) -> (usize, usize) {
    let n = aspects.len().max(1);
    let mut best = (n, 1, -1.0f32);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = (h - gap * (rows as f32 - 1.0)) / rows as f32;
        // The smallest image decides: everyone should stay readable.
        let area = aspects
            .iter()
            .map(|&a| {
                let b = contain(Box2::new(0.0, 0.0, cw, ch), a);
                b.w * b.h
            })
            .fold(f32::MAX, f32::min);
        if area > best.2 {
            best = (cols, rows, area);
        }
    }
    (best.0, best.1)
}

/// Slot of the logo at the right end of a page's title row.
pub const HEAD_LOGO: Box2 = Box2 { x: PAGE_W - M - 130.0, y: M + 5.0, w: 130.0, h: TITLE_H - 10.0 };

/// A content page's header: the accent bar at the left edge, the kicker
/// ("SCENE 2 OF 9"), the title and a rule; the logo at the right of the
/// title, which makes room for it.
pub(crate) fn header(s: &mut Slide, b: &Brand, title: &str, kicker: &str) {
    s.els.push(El::Fill { bx: Box2::new(0.0, 0.0, 6.0, PAGE_H), color: b.accent });
    if !kicker.is_empty() {
        s.els.push(text(Box2::new(M, M - 14.0, PAGE_W - 2.0 * M, 14.0), kicker.to_uppercase(), 9.0, true, b.accent));
    }
    let mut title_w = PAGE_W - 2.0 * M;
    if let Some(pic) = &b.logo {
        let lb = brand::logo_box(HEAD_LOGO, pic.aspect(), true);
        s.els.push(El::Image { bx: lb, pic: pic.clone() });
        title_w = lb.x - M - 16.0;
    }
    s.els.push(fitted(Box2::new(M, M, title_w, TITLE_H), title, &[26.0, 22.0, 18.0], true, INK));
    s.els.push(El::Fill { bx: Box2::new(M, M + TITLE_H + 2.0, PAGE_W - 2.0 * M, 1.0), color: RULE });
}

/// A content page's footer: company and report title at the left, the
/// footer line ("Confidential") in the middle, the page number at the right.
/// Every part stays on one line, shortened with an ellipsis when long.
pub(crate) fn page_footer(s: &mut Slide, b: &Brand, title: &str, page: usize) {
    let y = PAGE_H - M + 6.0;
    let num = page.to_string();
    let w = text_width(&num, 8.0, false);
    s.els.push(text(Box2::new(PAGE_W - M - w - 2.0, y, w + 4.0, FOOT_H), num, 8.0, false, MUTED));
    let line = leading(8.0);
    let foot = b.footer.trim();
    let mid_w = if foot.is_empty() { 0.0 } else { (text_width(foot, 8.0, true) + 4.0).min(PAGE_W * 0.36) };
    if mid_w > 0.0 {
        let (size, t) = fit_text(foot, Box2::new(0.0, 0.0, mid_w, line), &[8.0, 7.0], true);
        s.els.push(El::Text(TextEl { bx: Box2::new((PAGE_W - mid_w) / 2.0, y, mid_w, line), text: t, size, bold: true, color: INK, align: Align::Center }));
    }
    let left_w = if mid_w > 0.0 { (PAGE_W - mid_w) / 2.0 - M - 16.0 } else { PAGE_W / 2.0 };
    let left = [b.company.as_str(), title.trim()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ·  ");
    s.els.push(fitted(Box2::new(M, y, left_w, line), &left, &[8.0, 7.0], false, MUTED));
}

fn footer(s: &mut Slide, r: &Report, page: usize) {
    let left = if r.title.is_empty() { "Lumen review" } else { r.title.as_str() };
    page_footer(s, &r.brand, left, page);
}

/// What a cover page says.
#[derive(Clone, Debug, Default)]
pub struct CoverInfo {
    pub title: String,
    /// The project's name, shown when it is not the title already.
    pub project: String,
    /// "3 devices · 12 scenes", "2 images compared".
    pub stats: String,
    /// The devices (or images) compared, with their colors.
    pub items: Vec<(String, [u8; 3])>,
    pub date: String,
    pub author: String,
}

/// Device chips in rows from (`x0`, `y`) up to `x1`, at most `max_rows`
/// rows; when not all fit, a last chip says how many more there are.
pub(crate) fn chip_rows(items: &[(String, [u8; 3])], x0: f32, y: f32, x1: f32, size: f32, max_rows: usize) -> Vec<(Box2, String, [u8; 3])> {
    let h = leading(size) + 4.0;
    let max_w = 240.0f32.min(x1 - x0).max(h);
    let width = |t: &str| (text_width(t, size, true) + 14.0).min(max_w).max(h);
    // The first `shown` items and the "+N more" chip, if they fit the rows.
    let place = |shown: usize| -> Option<Vec<(Box2, String, [u8; 3])>> {
        let more = items.len() - shown;
        let extra = (more > 0).then(|| (format!("+{more} more"), MUTED));
        let mut out = Vec::new();
        let (mut x, mut row) = (x0, 0usize);
        for (t, c) in items[..shown].iter().cloned().chain(extra) {
            let w = width(&t);
            if x > x0 && x + w > x1 {
                row += 1;
                x = x0;
            }
            if row >= max_rows {
                return None;
            }
            out.push((Box2::new(x, y + row as f32 * (h + 6.0), w, h), t, c));
            x += w + 8.0;
        }
        Some(out)
    };
    (0..=items.len()).rev().find_map(place).unwrap_or_default()
}

/// A colored chip with a name in white.
pub(crate) fn push_chip(s: &mut Slide, bx: Box2, name: &str, color: [u8; 3], size: f32) {
    s.els.push(El::Fill { bx, color });
    s.els.push(fitted(Box2::new(bx.x + 7.0, bx.y + 2.0, (bx.w - 10.0).max(1.0), leading(size)), name, &[size, size * 0.85], true, [255, 255, 255]));
}

/// The branded cover: a white page with the accent band at the left, the
/// logo and the company name at the top, then the title, the project, what
/// was compared, the date and the author, and the footer line at the bottom.
/// Logos of any shape and long names stay inside their slots.
pub fn branded_cover(c: &CoverInfo, b: &Brand) -> Slide {
    let mut s = Slide::default();
    let (x0, x1) = (72.0, PAGE_W - 72.0);
    let w = x1 - x0;
    s.els.push(El::Fill { bx: Box2::new(0.0, 0.0, 16.0, PAGE_H), color: b.accent });
    // Top row: the logo, a thin rule, the company name.
    let top = Box2::new(x0, 52.0, w, 64.0);
    let mut name_x = x0;
    if let Some(pic) = &b.logo {
        let lb = brand::logo_box(Box2::new(x0, top.y, 260.0, top.h), pic.aspect(), false);
        s.els.push(El::Image { bx: lb, pic: pic.clone() });
        name_x = lb.x + lb.w + 18.0;
        if !b.company.is_empty() {
            s.els.push(El::Fill { bx: Box2::new(name_x, top.y + 12.0, 1.0, top.h - 24.0), color: RULE });
            name_x += 18.0;
        }
    }
    if !b.company.is_empty() {
        let (size, color) = if b.logo.is_some() { (16.0, INK) } else { (18.0, b.accent) };
        s.els.push(fitted(Box2::new(name_x, top.y + 12.0, x1 - name_x, top.h - 24.0), &b.company, &[size, size * 0.85, 12.0, 10.0], true, color));
    }
    s.els.push(El::Fill { bx: Box2::new(x0, 196.0, 64.0, 4.0), color: b.accent });
    s.els.push(fitted(Box2::new(x0, 212.0, w, 96.0), &c.title, &[40.0, 34.0, 28.0, 22.0], true, INK));
    let mut y = 316.0;
    let project = c.project.trim();
    if !project.is_empty() && project != c.title.trim() {
        s.els.push(fitted(Box2::new(x0, y, w, 20.0), &format!("Project: {project}"), &[13.0, 11.0], false, MUTED));
        y += 22.0;
    }
    if !c.stats.is_empty() {
        s.els.push(fitted(Box2::new(x0, y, w, 20.0), &c.stats, &[14.0, 12.0], false, MUTED));
        y += 26.0;
    }
    for (bx, name, color) in chip_rows(&c.items, x0, y, x1, 11.0, 2) {
        push_chip(&mut s, bx, &name, color, 11.0);
    }
    let by = [c.date.as_str(), c.author.as_str()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ·  ");
    if !by.is_empty() {
        s.els.push(fitted(Box2::new(x0, 436.0, w, 18.0), &by, &[12.0, 10.0], false, INK));
    }
    s.els.push(El::Fill { bx: Box2::new(x0, PAGE_H - 60.0, w, 0.75), color: RULE });
    if !b.footer.is_empty() {
        s.els.push(fitted(Box2::new(x0, PAGE_H - 50.0, w, 16.0), &b.footer, &[10.0, 9.0], true, INK));
    }
    s
}

/// The footer line on the dark (unbranded) cover, at the bottom right.
pub(crate) fn dark_cover_footer(s: &mut Slide, b: &Brand) {
    if !b.footer.is_empty() {
        let (size, t) = fit_text(&b.footer, Box2::new(0.0, 0.0, PAGE_W / 2.0 - 72.0, 14.0), &[9.0, 8.0], true);
        s.els.push(El::Text(TextEl { bx: Box2::new(PAGE_W / 2.0, PAGE_H - 48.0, PAGE_W / 2.0 - 72.0, 14.0), text: t, size, bold: true, color: [200, 208, 220], align: Align::Right }));
    }
}

/// The coloured "A" badge and file name above an image.
fn caption(s: &mut Slide, i: usize, item: &Item, bx: Box2, size: f32) {
    let badge = size * 1.6;
    s.els.push(El::Fill { bx: Box2::new(bx.x, bx.y, badge, badge), color: label_color(i) });
    let lw = text_width(&item.label, size, true);
    s.els.push(El::Text(TextEl {
        bx: Box2::new(bx.x, bx.y + (badge - leading(size)) / 2.0, badge, leading(size)),
        text: item.label.clone(),
        size,
        bold: true,
        color: [255, 255, 255],
        align: if lw < badge { Align::Center } else { Align::Left },
    }));
    let nb = Box2::new(bx.x + badge + 6.0, bx.y + (badge - leading(size)) / 2.0, (bx.w - badge - 6.0).max(10.0), leading(size));
    s.els.push(fitted(nb, &item.name, &[size, size * 0.85], true, INK));
}

/// Body area of a content page.
fn body() -> Box2 {
    Box2::new(M, BODY_TOP, PAGE_W - 2.0 * M, PAGE_H - BODY_TOP - M - 4.0)
}

/// Lay the report out as pages.
pub fn layout(r: &Report) -> Vec<Slide> {
    let mut slides: Vec<Slide> = Vec::new();
    let n = r.items.len();
    let title = if r.title.trim().is_empty() { "Image comparison".to_string() } else { r.title.clone() };

    let brand = &r.brand;
    let sub = format!("{} image{} compared", n, if n == 1 { "" } else { "s" });
    if r.sections.title && brand.branded() {
        let items = r.items.iter().enumerate().map(|(i, it)| (format!("{}  {}", it.label, it.name), label_color(i))).collect();
        let c = CoverInfo { title: title.clone(), project: String::new(), stats: sub, items, date: r.date.clone(), author: r.author.clone() };
        slides.push(branded_cover(&c, brand));
    } else if r.sections.title {
        let mut s = Slide::default();
        s.els.push(El::Fill { bx: Box2::new(0.0, 0.0, PAGE_W, PAGE_H), color: [24, 30, 42] });
        s.els.push(El::Fill { bx: Box2::new(72.0, 214.0, 64.0, 4.0), color: brand.accent });
        s.els.push(fitted(Box2::new(72.0, 230.0, PAGE_W - 144.0, 92.0), &title, &[40.0, 34.0, 28.0], true, [255, 255, 255]));
        let names: Vec<String> = r.items.iter().map(|it| format!("{}  {}", it.label, it.name)).collect();
        s.els.push(text(Box2::new(72.0, 330.0, PAGE_W - 144.0, 22.0), sub, 16.0, false, [200, 208, 220]));
        s.els.push(fitted(Box2::new(72.0, 356.0, PAGE_W - 144.0, 60.0), &names.join("\n"), &[11.0, 9.0], false, [160, 170, 186]));
        let by = [r.date.as_str(), r.author.as_str()].iter().filter(|t| !t.is_empty()).cloned().collect::<Vec<_>>().join("  ·  ");
        if !by.is_empty() {
            s.els.push(text(Box2::new(72.0, 440.0, PAGE_W - 144.0, 18.0), by, 12.0, false, [200, 208, 220]));
        }
        s.els.push(text(Box2::new(72.0, PAGE_H - 48.0, 300.0, 14.0), "Made with Lumen", 9.0, false, [120, 130, 146]));
        dark_cover_footer(&mut s, brand);
        slides.push(s);
    }

    if r.sections.overview && n > 0 {
        let mut s = Slide::default();
        header(&mut s, brand, &title, "Side by side");
        let b = body();
        let summary_h = if r.summary.trim().is_empty() { 0.0 } else { (b.h * 0.26).clamp(60.0, 120.0) };
        let cap = 20.0;
        let area = Box2::new(b.x, b.y, b.w, b.h - summary_h - if summary_h > 0.0 { 12.0 } else { 0.0 });
        let aspects: Vec<f32> = r.items.iter().map(|i| i.picture.aspect()).collect();
        let gap = 14.0;
        let (cols, rows) = best_grid(&aspects, area.w, area.h - cap * 1.0, gap);
        let cw = (area.w - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = (area.h - gap * (rows as f32 - 1.0)) / rows as f32;
        for (i, it) in r.items.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let cell = Box2::new(area.x + cx as f32 * (cw + gap), area.y + cy as f32 * (ch + gap), cw, ch);
            let ib = contain(Box2::new(cell.x, cell.y + cap + 4.0, cell.w, cell.h - cap - 4.0), it.picture.aspect());
            // The caption sits right above its picture.
            caption(&mut s, i, it, Box2::new(ib.x, ib.y - cap - 4.0, ib.w.max(120.0).min(cell.w), cap), 10.0);
            s.els.push(El::Image { bx: ib, pic: it.picture.clone() });
            s.els.push(El::Frame { bx: ib, color: RULE, width: 0.75 });
        }
        if summary_h > 0.0 {
            let sb = Box2::new(b.x, b.y + b.h - summary_h, b.w, summary_h);
            s.els.push(El::Fill { bx: sb, color: PANEL });
            s.els.push(El::Fill { bx: Box2::new(sb.x, sb.y, 3.0, sb.h), color: brand.accent });
            s.els.push(text(Box2::new(sb.x + 14.0, sb.y + 8.0, 200.0, 14.0), "SUMMARY", 9.0, true, brand.accent));
            s.els.push(fitted(Box2::new(sb.x + 14.0, sb.y + 24.0, sb.w - 28.0, sb.h - 30.0), &r.summary, &[13.0, 11.0, 9.0], false, INK));
        }
        s.notes = r.summary.clone();
        slides.push(s);
    } else if !r.summary.trim().is_empty() && n == 0 {
        let mut s = Slide::default();
        header(&mut s, brand, &title, "Summary");
        s.els.push(fitted(body(), &r.summary, &[16.0, 13.0, 11.0], false, INK));
        slides.push(s);
    }

    if r.sections.per_image {
        for (i, it) in r.items.iter().enumerate() {
            let mut s = Slide::default();
            header(&mut s, brand, &format!("{}  ·  {}", it.label, it.name), &format!("Image {} of {}", i + 1, n));
            let b = body();
            let has_text = !it.comment.trim().is_empty() || !it.notes.is_empty() || !it.details.is_empty();
            let side_w = if has_text { (b.w * 0.32).max(220.0) } else { 0.0 };
            let ia = Box2::new(b.x, b.y, b.w - side_w - if has_text { 18.0 } else { 0.0 }, b.h);
            let ib = contain(ia, it.picture.aspect());
            s.els.push(El::Image { bx: ib, pic: it.picture.clone() });
            s.els.push(El::Frame { bx: ib, color: RULE, width: 0.75 });
            if has_text {
                let sx = b.x + b.w - side_w;
                s.els.push(El::Fill { bx: Box2::new(sx, b.y, side_w, b.h), color: PANEL });
                s.els.push(El::Fill { bx: Box2::new(sx, b.y, 3.0, b.h), color: label_color(i) });
                let mut y = b.y + 10.0;
                let tw = side_w - 26.0;
                if !it.details.is_empty() {
                    let lines = wrap(&it.details, 9.0, false, tw).len().min(3) as f32;
                    s.els.push(fitted(Box2::new(sx + 14.0, y, tw, lines * leading(9.0)), &it.details, &[9.0], false, MUTED));
                    y += lines * leading(9.0) + 10.0;
                }
                let mut body_text = String::new();
                if !it.comment.trim().is_empty() {
                    body_text.push_str(it.comment.trim());
                }
                if !it.notes.is_empty() {
                    if !body_text.is_empty() {
                        body_text.push_str("\n\n");
                    }
                    for (k, note) in it.notes.iter().enumerate() {
                        let note = if note.trim().is_empty() { "(marked)" } else { note.trim() };
                        body_text.push_str(&format!("{}.  {}\n", k + 1, note));
                    }
                }
                let label = if it.notes.is_empty() { "COMMENTS" } else { "COMMENTS AND MARKS" };
                s.els.push(text(Box2::new(sx + 14.0, y, tw, 14.0), label, 9.0, true, label_color(i)));
                y += 18.0;
                s.els.push(fitted(Box2::new(sx + 14.0, y, tw, b.y + b.h - y - 8.0), body_text.trim_end(), &[12.0, 11.0, 10.0, 9.0], false, INK));
            }
            s.notes = it.comment.clone();
            slides.push(s);
        }
    }

    if r.sections.details {
        for (k, d) in r.details.iter().enumerate() {
            let mut s = Slide::default();
            let t = if d.title.trim().is_empty() { format!("Detail {}", k + 1) } else { d.title.clone() };
            header(&mut s, brand, &t, &format!("Detail {} of {}", k + 1, r.details.len()));
            let b = body();
            let ch_comment = if d.comment.trim().is_empty() { 0.0 } else { 70.0 };
            let area = Box2::new(b.x, b.y, b.w, b.h - ch_comment - if ch_comment > 0.0 { 10.0 } else { 0.0 });
            let aspects: Vec<f32> = d.crops.iter().map(|(_, p)| p.aspect()).collect();
            let gap = 14.0;
            let cap = 20.0;
            let (cols, rows) = best_grid(&aspects, area.w, area.h - cap, gap);
            let cw = (area.w - gap * (cols as f32 - 1.0)) / cols as f32;
            let chh = (area.h - gap * (rows as f32 - 1.0)) / rows as f32;
            for (i, (label, pic)) in d.crops.iter().enumerate() {
                let (cx, cy) = (i % cols, i / cols);
                let cell = Box2::new(area.x + cx as f32 * (cw + gap), area.y + cy as f32 * (chh + gap), cw, chh);
                let idx = r.items.iter().position(|it| &it.label == label).unwrap_or(i);
                let name = r.items.get(idx).map(|it| it.name.clone()).unwrap_or_default();
                let pseudo = Item { label: label.clone(), name, details: String::new(), picture: pic.clone(), comment: String::new(), notes: vec![] };
                let ib = contain(Box2::new(cell.x, cell.y + cap + 4.0, cell.w, cell.h - cap - 4.0), pic.aspect());
                caption(&mut s, idx, &pseudo, Box2::new(ib.x, ib.y - cap - 4.0, ib.w.max(120.0).min(cell.w), cap), 10.0);
                s.els.push(El::Image { bx: ib, pic: pic.clone() });
                s.els.push(El::Frame { bx: ib, color: label_color(idx), width: 1.5 });
            }
            if ch_comment > 0.0 {
                let sb = Box2::new(b.x, b.y + b.h - ch_comment, b.w, ch_comment);
                s.els.push(El::Fill { bx: sb, color: PANEL });
                s.els.push(El::Fill { bx: Box2::new(sb.x, sb.y, 3.0, sb.h), color: brand.accent });
                s.els.push(fitted(Box2::new(sb.x + 14.0, sb.y + 8.0, sb.w - 28.0, sb.h - 14.0), &d.comment, &[13.0, 11.0, 9.0], false, INK));
            }
            s.notes = d.comment.clone();
            slides.push(s);
        }
    }

    if r.sections.metrics {
        if let Some(t) = r.metrics.as_ref().filter(|t| !t.rows.is_empty()) {
            // Long tables continue on further pages.
            let per_page = 14usize;
            let pages = t.rows.len().div_ceil(per_page);
            for p in 0..pages {
                let mut s = Slide::default();
                let kicker = if pages > 1 { format!("Measurements {} of {}", p + 1, pages) } else { "Measurements".to_string() };
                header(&mut s, brand, "Measured quality", &kicker);
                let b = body();
                let rows: Vec<Vec<String>> = t.rows[p * per_page..((p + 1) * per_page).min(t.rows.len())].to_vec();
                let row_h = 24.0;
                let th = row_h * (rows.len() as f32 + 1.0);
                let note_h = if t.note.is_empty() { 0.0 } else { 40.0 };
                let tb = Box2::new(b.x, b.y, b.w, th.min(b.h - note_h));
                s.els.push(El::Table { bx: tb, table: Table { header: t.header.clone(), rows, note: String::new() }, head: brand.accent });
                if note_h > 0.0 {
                    s.els.push(fitted(Box2::new(b.x, tb.y + tb.h + 10.0, b.w, note_h - 10.0), &t.note, &[10.0, 9.0], false, MUTED));
                }
                slides.push(s);
            }
        }
    }

    if slides.is_empty() {
        let mut s = Slide::default();
        header(&mut s, brand, &title, "");
        slides.push(s);
    }
    let first_content = usize::from(r.sections.title);
    for (k, s) in slides.iter_mut().enumerate().skip(first_content) {
        footer(s, r, k + 1);
    }
    slides
}

/// Write `r` to `path` in `fmt` (atomically: a failed write leaves no file).
pub fn write(r: &Report, fmt: Format, path: &Path) -> Result<()> {
    let bytes = match fmt {
        Format::Pptx => pptx::render(r)?,
        Format::Pdf => pdf::render(r)?,
        Format::Html => html::render(r).into_bytes(),
    };
    crate::encode::write_atomic(path, &bytes)
}

pub(crate) fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // Control characters are not allowed in XML 1.0.
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => {}
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample(n: usize) -> Report {
        let items = (0..n)
            .map(|i| {
                let img = RgbaImage::from_fn(320, 200 + i as u32 * 20, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, (i * 60) as u8, 255]));
                Item {
                    label: ["A", "B", "C", "D"][i % 4].into(),
                    name: format!("IMG_{i:04}.jpg"),
                    details: "320 × 200 · 41 KB".into(),
                    picture: Picture::from_rgba(&img, 2000).unwrap(),
                    comment: "Edges look soft & the sky has banding <visible> at 100%.".into(),
                    notes: vec!["Halo around the roof".into(), String::new()],
                }
            })
            .collect::<Vec<_>>();
        let crops = items.iter().map(|it| (it.label.clone(), it.picture.clone())).collect();
        Report {
            title: "Camera A vs B, low light".into(),
            author: "Rahul".into(),
            date: "1 November 2026".into(),
            summary: "B keeps more detail in the shadows; A is noisier but sharper.".into(),
            items,
            details: vec![Detail { title: "Roof edge".into(), comment: "Halo visible in A".into(), crops }],
            metrics: Some(Table {
                header: vec!["Metric".into(), "A".into(), "B".into()],
                rows: vec![vec!["Sharpness".into(), "Good".into(), "Soft".into()], vec!["Noise".into(), "1.2".into(), "0.8".into()]],
                note: "Higher sharpness is better; lower noise is better.".into(),
            }),
            sections: Sections::default(),
            brand: Brand::default(),
        }
    }

    /// Every element's box on the page.
    pub fn boxes(s: &Slide) -> Vec<Box2> {
        s.els
            .iter()
            .map(|e| match e {
                El::Fill { bx, .. } | El::Frame { bx, .. } | El::Image { bx, .. } | El::Table { bx, .. } => *bx,
                El::Text(t) => t.bx,
            })
            .collect()
    }

    pub fn on_page(b: Box2) -> bool {
        b.x >= -0.01 && b.y >= -0.01 && b.x + b.w <= PAGE_W + 0.01 && b.y + b.h <= PAGE_H + 0.01
    }

    pub fn overlaps(a: Box2, b: Box2) -> bool {
        a.x < b.x + b.w - 0.01 && b.x < a.x + a.w - 0.01 && a.y < b.y + b.h - 0.01 && b.y < a.y + a.h - 0.01
    }

    /// A logo of `w` x `h` pixels.
    pub fn logo(w: u32, h: u32) -> Picture {
        Picture::from_rgba(&RgbaImage::from_pixel(w, h, image::Rgba([20, 60, 120, 255])), 900).unwrap()
    }

    pub fn acme(logo_px: Option<(u32, u32)>) -> Brand {
        Brand { company: "Acme Camera Lab".into(), logo: logo_px.map(|(w, h)| logo(w, h)), accent: [200, 30, 60], footer: "Confidential".into() }
    }

    #[test]
    fn unbranded_report_keeps_the_lumen_look() {
        let slides = layout(&sample(2));
        // The dark title page, Lumen blue, no logo anywhere.
        assert!(matches!(slides[0].els[0], El::Fill { color: [24, 30, 42], .. }));
        assert!(slides.iter().flat_map(|s| &s.els).any(|e| matches!(e, El::Fill { color: ACCENT, .. })));
        assert!(!slides.iter().flat_map(|s| &s.els).any(|e| matches!(e, El::Fill { color: [200, 30, 60], .. })));
        let images: usize = slides.iter().flat_map(|s| &s.els).filter(|e| matches!(e, El::Image { .. })).count();
        let mut r = sample(2);
        r.brand = acme(Some((400, 100)));
        let branded = layout(&r);
        assert_eq!(branded.len(), slides.len());
        // One logo on the cover and one per content page.
        let logos: usize = branded.iter().flat_map(|s| &s.els).filter(|e| matches!(e, El::Image { .. })).count();
        assert_eq!(logos, images + branded.len());
        assert!(!matches!(branded[0].els[0], El::Fill { color: [24, 30, 42], .. }));
        // The footer line is on every content page; tables use the accent.
        for s in &branded[1..] {
            assert!(s.els.iter().any(|e| matches!(e, El::Text(t) if t.text == "Confidential" && t.align == Align::Center)));
            assert!(s.els.iter().any(|e| matches!(e, El::Text(t) if t.text.starts_with("Acme Camera Lab  ·  "))));
        }
        assert!(branded.iter().flat_map(|s| &s.els).any(|e| matches!(e, El::Table { head: [200, 30, 60], .. })));
    }

    #[test]
    fn branded_cover_is_tidy_at_any_logo_shape_and_with_long_names() {
        let long = "Night street, wet asphalt and neon signs: the longest scene name anyone ever typed into a camera comparison project title box".to_string();
        let devices: Vec<(String, [u8; 3])> = (0..14).map(|i| (format!("Galaxy A{i}6 Ultra Pro Max build 2026.{i} (night mode on)"), label_color(i))).collect();
        for logo_px in [None, Some((1600, 400)), Some((400, 400)), Some((120, 900)), Some((3000, 60)), Some((1, 1))] {
            for company in ["", "Acme", "The Very Long Name Imaging Quality Engineering and Camera Tuning Department of Example Corporation Worldwide"] {
                let b = Brand { company: company.into(), logo: logo_px.map(|(w, h)| logo(w, h)), accent: [10, 120, 90], footer: "Confidential - for internal review only, do not forward outside the company".into() };
                let c = CoverInfo { title: long.clone(), project: "Round 3 (night)".into(), stats: "14 devices · 40 scenes".into(), items: devices.clone(), date: "27 November 2026".into(), author: "R. Tarigoppula".into() };
                let s = branded_cover(&c, &b);
                let boxes = boxes(&s);
                for bx in &boxes {
                    assert!(on_page(*bx), "{logo_px:?} {company}: {bx:?}");
                }
                // The logo stays in its slot and nothing written covers it.
                let logo_box = s.els.iter().find_map(|e| if let El::Image { bx, .. } = e { Some(*bx) } else { None });
                if let Some(lb) = logo_box {
                    assert!(lb.x >= 72.0 - 0.01 && lb.x + lb.w <= 332.01 && lb.y >= 51.99 && lb.y + lb.h <= 116.01, "{lb:?}");
                    for e in &s.els {
                        if let El::Text(t) = e {
                            assert!(!overlaps(t.bx, lb), "{:?} over the logo", t.text);
                        }
                    }
                }
                // Texts do not overlap each other.
                let texts: Vec<Box2> = s.els.iter().filter_map(|e| if let El::Text(t) = e { Some(t.bx) } else { None }).collect();
                for (i, a) in texts.iter().enumerate() {
                    for b in &texts[i + 1..] {
                        assert!(!overlaps(*a, *b) || chip_text(&s, *a) || chip_text(&s, *b), "{a:?} {b:?}");
                    }
                }
                // Long text is shortened, not spilled: wrapped as the writers
                // wrap it, every line fits its box.
                for e in &s.els {
                    if let El::Text(t) = e {
                        let lines = wrap(&t.text, t.size, t.bold, t.bx.w);
                        assert!(lines.len() as f32 * leading(t.size) <= t.bx.h + 0.5, "{:?} in {:?}", t.text, t.bx);
                        for l in &lines {
                            assert!(text_width(l, t.size, t.bold) <= t.bx.w + 0.5, "{l:?}");
                        }
                    }
                }
            }
        }
    }

    /// The text inside a chip (it sits on the chip's fill, not on other text).
    fn chip_text(s: &Slide, t: Box2) -> bool {
        s.els.iter().any(|e| matches!(e, El::Fill { bx, .. } if bx.x <= t.x && t.x + t.w <= bx.x + bx.w + 0.01 && bx.y <= t.y && bx.h < 30.0))
    }

    #[test]
    fn chips_wrap_and_say_how_many_more() {
        let items: Vec<(String, [u8; 3])> = (0..30).map(|i| (format!("Device number {i}"), label_color(i))).collect();
        let chips = chip_rows(&items, 72.0, 360.0, 888.0, 11.0, 2);
        let rows: Vec<f32> = chips.iter().map(|c| c.0.y).fold(Vec::new(), |mut v, y| {
            if !v.contains(&y) {
                v.push(y);
            }
            v
        });
        assert_eq!(rows.len(), 2);
        let last = chips.last().unwrap();
        assert_eq!(last.1, format!("+{} more", 30 - (chips.len() - 1)));
        for c in &chips {
            assert!(c.0.x >= 72.0 && c.0.x + c.0.w <= 888.01, "{c:?}");
        }
        // Few devices: all shown, no "more" chip.
        let few = chip_rows(&items[..3], 72.0, 360.0, 888.0, 11.0, 2);
        assert_eq!(few.len(), 3);
        assert!(few.iter().all(|c| !c.1.contains("more")));
        assert!(chip_rows(&[], 72.0, 360.0, 888.0, 11.0, 2).is_empty());
    }

    #[test]
    fn header_makes_room_for_any_logo_and_footer_fits() {
        for (w, h) in [(2000, 100), (100, 2000), (500, 500)] {
            let b = acme(Some((w, h)));
            let mut s = Slide::default();
            header(&mut s, &b, &"W".repeat(200), "Scene 1 of 3");
            page_footer(&mut s, &b, &"Long report title ".repeat(20), 12);
            let lb = s.els.iter().find_map(|e| if let El::Image { bx, .. } = e { Some(*bx) } else { None }).unwrap();
            assert!(lb.x >= HEAD_LOGO.x - 0.01 && lb.x + lb.w <= HEAD_LOGO.x + HEAD_LOGO.w + 0.01 && lb.y >= HEAD_LOGO.y - 0.01 && lb.y + lb.h <= HEAD_LOGO.y + HEAD_LOGO.h + 0.01);
            for e in &s.els {
                if let El::Text(t) = e {
                    assert!(!overlaps(t.bx, lb), "{:?}", t.text);
                    assert!(on_page(t.bx));
                    assert!(t.text.lines().count() == 1 || t.bx.y < M + TITLE_H, "footer text on one line: {:?}", t.text);
                }
            }
        }
    }

    #[test]
    fn wrap_breaks_long_text() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", 12.0, false, 80.0);
        assert!(lines.len() > 2);
        for l in &lines {
            assert!(text_width(l, 12.0, false) <= 80.0 + 1e-3, "{l}");
        }
        assert_eq!(wrap("a\nb", 12.0, false, 100.0), vec!["a", "b"]);
        // A word longer than the box is hard-broken.
        let long = wrap("WWWWWWWWWWWWWWWWWWWW", 12.0, false, 50.0);
        assert!(long.len() > 1);
    }

    #[test]
    fn grid_prefers_rows_for_wide_images() {
        // Four wide images on a wide page: 2 x 2 beats 4 x 1 or 1 x 4.
        assert_eq!(best_grid(&[1.5; 4], 888.0, 380.0, 14.0), (2, 2));
        assert_eq!(best_grid(&[1.5; 2], 888.0, 380.0, 14.0), (2, 1));
        // Tall images sit in one row.
        assert_eq!(best_grid(&[0.6; 3], 888.0, 380.0, 14.0), (3, 1));
    }

    #[test]
    fn layout_has_expected_pages_and_stays_on_page() {
        let r = sample(3);
        let slides = layout(&r);
        // title + overview + 3 per image + 1 detail + 1 metrics
        assert_eq!(slides.len(), 7);
        for s in &slides {
            for e in &s.els {
                let b = match e {
                    El::Fill { bx, .. } | El::Frame { bx, .. } | El::Image { bx, .. } | El::Table { bx, .. } => *bx,
                    El::Text(t) => t.bx,
                };
                assert!(b.x >= -0.01 && b.y >= -0.01 && b.x + b.w <= PAGE_W + 0.01 && b.y + b.h <= PAGE_H + 0.01, "{b:?}");
            }
        }
        let mut only = r.clone();
        only.sections = Sections { title: false, overview: true, per_image: false, details: false, metrics: false };
        assert_eq!(layout(&only).len(), 1);
    }

    #[test]
    fn writers_produce_files() {
        let r = sample(4);
        let dir = std::env::temp_dir().join(format!("lumen-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in Format::ALL {
            let p = dir.join(format!("r.{}", f.ext()));
            write(&r, f, &p).unwrap();
            assert!(std::fs::metadata(&p).unwrap().len() > 1000);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod sample_out {
    /// `LUMEN_REPORT_OUT=<dir> cargo test -p lumen-core write_samples` writes
    /// sample reports for checking in PowerPoint, a PDF reader and a browser.
    #[test]
    fn write_samples() {
        let Some(dir) = std::env::var_os("LUMEN_REPORT_OUT") else { return };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [2usize, 4] {
            let r = super::tests::sample(n);
            for f in super::Format::ALL {
                super::write(&r, f, &dir.join(format!("sample{n}.{}", f.ext()))).unwrap();
            }
        }
    }
}
