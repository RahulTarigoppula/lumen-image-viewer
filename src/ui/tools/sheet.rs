//! Contact sheet composition: layout math, thumbnail placement and text
//! rendering (ab_glyph with the Windows UI font).

use crate::formats::SaveFormat;
use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SheetSettings {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub spacing: u32,
    pub margin: u32,
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub crop_fill: bool,
    pub cap_name: bool,
    pub cap_dims: bool,
    pub cap_date: bool,
    pub cap_size: bool,
    pub caption_px: f32,
    pub title: String,
    pub title_px: f32,
    pub page_numbers: bool,
    pub format: SaveFormat,
    pub jpeg_quality: u8,
    pub out_dir: Option<PathBuf>,
    pub base_name: String,
}

impl Default for SheetSettings {
    fn default() -> Self {
        SheetSettings {
            cols: 5,
            rows: 6,
            cell_w: 300,
            cell_h: 225,
            spacing: 18,
            margin: 40,
            bg: [255, 255, 255],
            fg: [40, 42, 48],
            crop_fill: false,
            cap_name: true,
            cap_dims: true,
            cap_date: false,
            cap_size: false,
            caption_px: 15.0,
            title: String::new(),
            title_px: 30.0,
            page_numbers: true,
            format: SaveFormat::Jpeg,
            jpeg_quality: 92,
            out_dir: None,
            base_name: "Contact sheet".into(),
        }
    }
}

impl SheetSettings {
    pub fn caption_lines(&self) -> u32 {
        let first = self.cap_name as u32;
        let second = (self.cap_dims || self.cap_date || self.cap_size) as u32;
        first + second
    }
    pub fn line_h(&self) -> u32 {
        (self.caption_px * 1.35).ceil() as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub header_h: u32,
    pub caption_h: u32,
    pub per_page: usize,
    pub pages: usize,
}

pub fn layout(s: &SheetSettings, n_files: usize) -> Layout {
    let cols = s.cols.max(1);
    let rows = s.rows.max(1);
    let per_page = (cols * rows) as usize;
    let pages = n_files.div_ceil(per_page).max(1);
    let caption_h = if s.caption_lines() > 0 { 6 + s.caption_lines() * s.line_h() } else { 0 };
    let header_h = if s.title.trim().is_empty() && !s.page_numbers { 0 } else { (s.title_px.max(s.caption_px) * 1.6).ceil() as u32 + s.spacing };
    let width = s.margin * 2 + cols * s.cell_w + (cols - 1) * s.spacing;
    let height = s.margin * 2 + header_h + rows * (s.cell_h + caption_h) + (rows - 1) * s.spacing;
    Layout { width, height, header_h, caption_h, per_page, pages }
}

/// Top-left corner of the image cell for `slot` (0-based within a page).
pub fn cell_origin(s: &SheetSettings, l: &Layout, slot: usize) -> (u32, u32) {
    let cols = s.cols.max(1) as usize;
    let (c, r) = ((slot % cols) as u32, (slot / cols) as u32);
    let x = s.margin + c * (s.cell_w + s.spacing);
    let y = s.margin + l.header_h + r * (s.cell_h + l.caption_h + s.spacing);
    (x, y)
}

/// Per-file data needed to draw a cell.
#[derive(Clone)]
pub struct SheetItem {
    pub path: PathBuf,
    pub dims: Option<(u32, u32)>,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl SheetItem {
    pub fn from_path(p: &Path) -> SheetItem {
        let meta = std::fs::metadata(p).ok();
        SheetItem {
            path: p.to_path_buf(),
            dims: crate::decode::probe_dimensions(p),
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta.and_then(|m| m.modified().ok()),
        }
    }
    fn details(&self, s: &SheetSettings) -> String {
        let mut parts = Vec::new();
        if s.cap_dims {
            if let Some((w, h)) = self.dims {
                parts.push(format!("{w} × {h}"));
            }
        }
        if s.cap_date {
            parts.push(crate::catalog::format_time(self.modified));
        }
        if s.cap_size {
            parts.push(crate::catalog::human_size(self.size));
        }
        parts.join("  ·  ")
    }
}

// ------------------------------------------------------------------- fonts

pub struct Fonts {
    pub regular: Option<FontArc>,
    pub bold: Option<FontArc>,
}

fn load_font(names: &[&str]) -> Option<FontArc> {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    names.iter().find_map(|n| std::fs::read(Path::new(&dir).join("Fonts").join(n)).ok().and_then(|b| FontArc::try_from_vec(b).ok()))
}

pub fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| {
        let regular = load_font(&["segoeui.ttf", "arial.ttf", "tahoma.ttf"]);
        let bold = load_font(&["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"]).or_else(|| regular.clone());
        Fonts { regular, bold }
    })
}

/// Width in pixels of `text` at `px`.
pub fn text_width(font: &FontArc, text: &str, px: f32) -> f32 {
    let f = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            w += f.kern(p, id);
        }
        w += f.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Shorten `text` with an ellipsis so it fits `max_w`.
pub fn ellipsize(font: &FontArc, text: &str, px: f32, max_w: f32) -> String {
    if text_width(font, text, px) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "...";
        if text_width(font, &s, px) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "..."
}

/// Draw `text` with its top-left at (`x`, `y`). Returns the drawn width.
pub fn draw_text(img: &mut RgbaImage, font: &FontArc, text: &str, x: f32, y: f32, px: f32, color: [u8; 3]) -> f32 {
    let f = font.as_scaled(PxScale::from(px));
    let baseline = y + f.ascent();
    let mut caret = x;
    let mut prev = None;
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            caret += f.kern(p, id);
        }
        let glyph = id.with_scale_and_position(px, point(caret, baseline));
        caret += f.h_advance(id);
        prev = Some(id);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let b = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                let px_ = b.min.x as i32 + gx as i32;
                let py_ = b.min.y as i32 + gy as i32;
                if px_ < 0 || py_ < 0 || px_ >= iw || py_ >= ih {
                    return;
                }
                let a = cov.clamp(0.0, 1.0);
                let dst = img.get_pixel_mut(px_ as u32, py_ as u32);
                for i in 0..3 {
                    dst.0[i] = (dst.0[i] as f32 * (1.0 - a) + color[i] as f32 * a + 0.5) as u8;
                }
            });
        }
    }
    caret - x
}

// --------------------------------------------------------------- rendering

/// Scale a thumbnail to the cell: fit inside, or cover and center-crop.
pub fn fit_to_cell(src: &RgbaImage, cw: u32, ch: u32, crop: bool) -> RgbaImage {
    let (w, h) = src.dimensions();
    if w == 0 || h == 0 {
        return RgbaImage::new(1, 1);
    }
    let sx = cw as f64 / w as f64;
    let sy = ch as f64 / h as f64;
    let s = if crop { sx.max(sy) } else { sx.min(sy) };
    let nw = ((w as f64 * s).round() as u32).max(1);
    let nh = ((h as f64 * s).round() as u32).max(1);
    let scaled = crate::imaging::resize_exact(src, nw, nh, crate::imaging::Filter::Lanczos3);
    if crop && (nw > cw || nh > ch) {
        let x = (nw.saturating_sub(cw)) / 2;
        let y = (nh.saturating_sub(ch)) / 2;
        image::imageops::crop_imm(&scaled, x, y, cw.min(nw), ch.min(nh)).to_image()
    } else {
        scaled
    }
}

/// Alpha-blend `src` onto `dst` at (x, y).
fn blit(dst: &mut RgbaImage, src: &RgbaImage, x: u32, y: u32) {
    for (sx, sy, p) in src.enumerate_pixels() {
        let (dx, dy) = (x + sx, y + sy);
        if dx >= dst.width() || dy >= dst.height() {
            continue;
        }
        let a = p.0[3] as u32;
        let d = dst.get_pixel_mut(dx, dy);
        if a == 255 {
            *d = Rgba([p.0[0], p.0[1], p.0[2], 255]);
        } else if a > 0 {
            for i in 0..3 {
                d.0[i] = ((p.0[i] as u32 * a + d.0[i] as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

/// Render one page. `thumbs[i]` belongs to `items[i]` (None = failed to load).
pub fn render_page(s: &SheetSettings, l: &Layout, items: &[SheetItem], thumbs: &[Option<RgbaImage>], page: usize) -> RgbaImage {
    let bg = Rgba([s.bg[0], s.bg[1], s.bg[2], 255]);
    let mut img = RgbaImage::from_pixel(l.width, l.height, bg);
    let fonts = fonts();
    let muted = [
        ((s.fg[0] as u16 + s.bg[0] as u16 * 2) / 3) as u8,
        ((s.fg[1] as u16 + s.bg[1] as u16 * 2) / 3) as u8,
        ((s.fg[2] as u16 + s.bg[2] as u16 * 2) / 3) as u8,
    ];
    let placeholder = [
        ((s.fg[0] as u16 + s.bg[0] as u16 * 9) / 10) as u8,
        ((s.fg[1] as u16 + s.bg[1] as u16 * 9) / 10) as u8,
        ((s.fg[2] as u16 + s.bg[2] as u16 * 9) / 10) as u8,
    ];
    // Header
    if l.header_h > 0 {
        let y = s.margin as f32;
        if let Some(bold) = &fonts.bold {
            if !s.title.trim().is_empty() {
                let t = ellipsize(bold, s.title.trim(), s.title_px, l.width as f32 * 0.7);
                draw_text(&mut img, bold, &t, s.margin as f32, y, s.title_px, s.fg);
            }
        }
        if s.page_numbers {
            if let Some(reg) = &fonts.regular {
                let t = format!("Page {} of {}", page + 1, l.pages);
                let w = text_width(reg, &t, s.caption_px);
                let ty = y + (s.title_px - s.caption_px).max(0.0) * 0.6;
                draw_text(&mut img, reg, &t, l.width as f32 - s.margin as f32 - w, ty, s.caption_px, muted);
            }
        }
    }
    for (slot, item) in items.iter().enumerate() {
        let (x, y) = cell_origin(s, l, slot);
        match thumbs.get(slot).and_then(|t| t.as_ref()) {
            Some(t) => {
                let ox = x + (s.cell_w.saturating_sub(t.width())) / 2;
                let oy = y + (s.cell_h.saturating_sub(t.height())) / 2;
                blit(&mut img, t, ox, oy);
            }
            None => {
                let ph = RgbaImage::from_pixel(s.cell_w, s.cell_h, Rgba([placeholder[0], placeholder[1], placeholder[2], 255]));
                blit(&mut img, &ph, x, y);
            }
        }
        if let Some(reg) = &fonts.regular {
            let max_w = s.cell_w as f32;
            let mut ty = (y + s.cell_h + 6) as f32;
            let mut line = |img: &mut RgbaImage, text: &str, color: [u8; 3]| {
                let t = ellipsize(reg, text, s.caption_px, max_w);
                let w = text_width(reg, &t, s.caption_px);
                draw_text(img, reg, &t, x as f32 + (max_w - w) / 2.0, ty, s.caption_px, color);
                ty += s.line_h() as f32;
            };
            if s.cap_name {
                line(&mut img, &crate::ui::batch::file_name(&item.path), s.fg);
            }
            let d = item.details(s);
            if !d.is_empty() {
                line(&mut img, &d, muted);
            }
        }
    }
    img
}

/// Thumbnail for a cell, decoded at the size the cell needs.
pub fn cell_thumb(path: &Path, s: &SheetSettings) -> Option<RgbaImage> {
    let need = if s.crop_fill { (s.cell_w.max(s.cell_h) as f32 * 1.6) as u32 } else { s.cell_w.max(s.cell_h) };
    let t = crate::decode::decode_thumbnail(path, need.max(64)).ok()?;
    Some(fit_to_cell(&t, s.cell_w, s.cell_h, s.crop_fill))
}

/// Output paths for every page.
pub fn page_paths(s: &SheetSettings, dir: &Path, pages: usize) -> Vec<PathBuf> {
    let base = crate::ui::batch::logic::sanitize_file_stem(if s.base_name.trim().is_empty() { "Contact sheet" } else { s.base_name.trim() });
    (0..pages)
        .map(|i| {
            let name = if pages == 1 { format!("{base}.{}", s.format.ext()) } else { format!("{base} {:02}.{}", i + 1, s.format.ext()) };
            dir.join(name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_math() {
        let s = SheetSettings { cols: 3, rows: 2, cell_w: 100, cell_h: 80, spacing: 10, margin: 20, title: String::new(), page_numbers: false, cap_name: false, cap_dims: false, ..Default::default() };
        let l = layout(&s, 13);
        assert_eq!(l.per_page, 6);
        assert_eq!(l.pages, 3);
        assert_eq!(l.width, 20 * 2 + 300 + 20);
        assert_eq!(l.height, 20 * 2 + 160 + 10);
        assert_eq!(cell_origin(&s, &l, 0), (20, 20));
        assert_eq!(cell_origin(&s, &l, 4), (20 + 110, 20 + 90));
        assert_eq!(layout(&s, 0).pages, 1);
    }

    #[test]
    fn fit_and_crop() {
        let src = RgbaImage::new(400, 200);
        assert_eq!(fit_to_cell(&src, 100, 100, false).dimensions(), (100, 50));
        assert_eq!(fit_to_cell(&src, 100, 100, true).dimensions(), (100, 100));
    }

    #[test]
    fn renders_page_with_text() {
        let s = SheetSettings { cols: 2, rows: 1, cell_w: 60, cell_h: 40, title: "Test".into(), ..Default::default() };
        let l = layout(&s, 2);
        let items = vec![
            SheetItem { path: PathBuf::from("a.jpg"), dims: Some((6, 4)), size: 10, modified: None },
            SheetItem { path: PathBuf::from("b.jpg"), dims: None, size: 10, modified: None },
        ];
        let red = RgbaImage::from_pixel(60, 40, Rgba([255, 0, 0, 255]));
        let page = render_page(&s, &l, &items, &[Some(red), None], 0);
        assert_eq!(page.dimensions(), (l.width, l.height));
        let (x, y) = cell_origin(&s, &l, 0);
        assert_eq!(page.get_pixel(x + 30, y + 20).0, [255, 0, 0, 255]);
        if fonts().regular.is_some() {
            // Some caption pixels must differ from the background.
            let cap_y = y + s.cell_h + 6;
            let inked = (cap_y..cap_y + s.line_h()).any(|yy| (x..x + s.cell_w).any(|xx| page.get_pixel(xx, yy).0 != [255, 255, 255, 255]));
            assert!(inked);
        }
    }
}
