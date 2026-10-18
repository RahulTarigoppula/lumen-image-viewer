//! GPU presentation of (possibly gigantic) images.
//!
//! An image is kept as a CPU mip pyramid (level 0 = full resolution, each next
//! level half the size). Only the tiles of the level matching the current zoom
//! that are actually visible get uploaded, so a 200-megapixel photo opens and
//! pans as smoothly as a small one. While a tile is uploading, the coarsest
//! level (always resident, a single small texture) is drawn in its place.

use egui::{Color32, ColorImage, Pos2, Rect, TextureHandle, TextureOptions, Vec2};
use image::RgbaImage;
use std::sync::Arc;
use std::time::Instant;

pub const TILE: u32 = 1024;
/// Texels of neighbouring image content copied around every tile texture.
/// Linear filtering (and the first mip level that trilinear sampling blends in
/// between pyramid levels) then reads real neighbour pixels at tile edges
/// instead of clamping to the tile's own border, which removed visible seams
/// at 30-70% zoom. The level chosen for a zoom keeps the GPU LOD below 1, so
/// 2 texels at mip 0 (= 1 texel at mip 1) are enough.
pub const GUTTER: u32 = 2;
/// The coarsest level is at most this big so it fits in one texture.
pub const TOP_MAX: u32 = 1024;

/// Build the halving pyramid for `img` (level 0 is `img` itself).
pub fn build_pyramid(img: Arc<RgbaImage>) -> Vec<Arc<RgbaImage>> {
    let mut levels = vec![img];
    loop {
        let last = levels.last().unwrap();
        if last.width().max(last.height()) <= TOP_MAX {
            break;
        }
        let next = crate::imaging::half(last);
        levels.push(Arc::new(next));
    }
    levels
}

struct Level {
    cols: u32,
    rows: u32,
    tiles: Vec<Option<TextureHandle>>,
    last_used: Vec<u64>,
}

pub struct GpuImage {
    pub width: u32,
    pub height: u32,
    pyramid: Vec<Arc<RgbaImage>>,
    levels: Vec<Level>,
    top: Option<TextureHandle>,
    nearest_level0: bool,
    frame_counter: u64,
    name: String,
}

fn push_row(pixels: &mut Vec<Color32>, row: &[u8]) {
    // Opaque rows (the common case) need no premultiplication: copy bytes directly.
    if row.iter().skip(3).step_by(4).all(|&a| a == 255) {
        pixels.extend_from_slice(bytemuck::cast_slice::<u8, Color32>(row));
    } else {
        pixels.extend(row.chunks_exact(4).map(|p| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])));
    }
}

/// Copy the `w`x`h` region at (`x0`, `y0`) plus `g` texels on every side. Gutter
/// texels outside the image repeat the nearest edge pixel (like ClampToEdge).
fn to_color_image_gutter(img: &RgbaImage, x0: u32, y0: u32, w: u32, h: u32, g: u32) -> ColorImage {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let stride = iw as usize * 4;
    let raw = img.as_raw();
    let (ow, oh) = (w + 2 * g, h + 2 * g);
    let mut pixels: Vec<Color32> = Vec::with_capacity(ow as usize * oh as usize);
    // Columns [xa, xb) are inside the image; the rest of the row is edge-clamped.
    let xa = (x0 as i64 - g as i64).max(0);
    let xb = (x0 as i64 + w as i64 + g as i64).min(iw);
    let left = (xa - (x0 as i64 - g as i64)) as usize;
    let right = ((x0 as i64 + w as i64 + g as i64) - xb) as usize;
    for oy in 0..oh as i64 {
        let y = (y0 as i64 - g as i64 + oy).clamp(0, ih - 1) as usize;
        let row = &raw[y * stride..(y + 1) * stride];
        let px = |x: i64| {
            let p = &row[x as usize * 4..x as usize * 4 + 4];
            Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])
        };
        pixels.extend(std::iter::repeat_n(px(xa), left));
        push_row(&mut pixels, &row[xa as usize * 4..xb as usize * 4]);
        pixels.extend(std::iter::repeat_n(px(xb - 1), right));
    }
    ColorImage::new([ow as usize, oh as usize], pixels)
}

fn to_color_image(img: &RgbaImage, x0: u32, y0: u32, w: u32, h: u32) -> ColorImage {
    to_color_image_gutter(img, x0, y0, w, h, 0)
}

pub fn color_image_from_rgba(img: &RgbaImage) -> ColorImage {
    to_color_image(img, 0, 0, img.width(), img.height())
}

/// UV rectangle of the real tile content inside a texture of `w`x`h` texels
/// surrounded by a `g`-texel gutter.
pub fn gutter_uv(w: u32, h: u32, g: u32) -> Rect {
    let (tw, th) = ((w + 2 * g) as f32, (h + 2 * g) as f32);
    Rect::from_min_max(Pos2::new(g as f32 / tw, g as f32 / th), Pos2::new((g + w) as f32 / tw, (g + h) as f32 / th))
}

fn tex_options(nearest: bool) -> TextureOptions {
    TextureOptions {
        magnification: if nearest { egui::TextureFilter::Nearest } else { egui::TextureFilter::Linear },
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    }
}

impl GpuImage {
    pub fn new(ctx: &egui::Context, name: &str, pyramid: Vec<Arc<RgbaImage>>) -> Self {
        let width = pyramid[0].width();
        let height = pyramid[0].height();
        let levels = pyramid
            .iter()
            .map(|img| {
                let cols = img.width().div_ceil(TILE);
                let rows = img.height().div_ceil(TILE);
                let n = (cols * rows) as usize;
                Level { cols, rows, tiles: vec![None; n], last_used: vec![0; n] }
            })
            .collect();
        let top_img = pyramid.last().unwrap();
        let top = Some(ctx.load_texture(
            format!("{name}#top"),
            color_image_from_rgba(top_img),
            tex_options(false),
        ));
        GpuImage { width, height, pyramid, levels, top, nearest_level0: false, frame_counter: 0, name: name.to_string() }
    }

    pub fn level_count(&self) -> usize {
        self.pyramid.len()
    }

    pub fn full(&self) -> &Arc<RgbaImage> {
        &self.pyramid[0]
    }

    /// Choose the pyramid level for a zoom factor (physical px per image px).
    fn level_for_zoom(&self, zoom: f32) -> usize {
        if zoom >= 1.0 {
            return 0;
        }
        let l = (1.0 / zoom).log2().floor() as i32;
        (l.max(0) as usize).min(self.pyramid.len() - 1)
    }

    /// Paint the image. `dest` is the screen rect (in points) the whole image maps to;
    /// `clip` is the visible area; `zoom` is in physical pixels per image pixel.
    pub fn paint(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        dest: Rect,
        clip: Rect,
        zoom: f32,
        nearest: bool,
        tint: Color32,
    ) -> bool {
        self.frame_counter += 1;
        if nearest != self.nearest_level0 {
            self.nearest_level0 = nearest;
            for t in self.levels[0].tiles.iter_mut() {
                *t = None;
            }
        }
        let li = self.level_for_zoom(zoom);
        let started = Instant::now();
        let budget = std::time::Duration::from_millis(12);
        let mut pending = false;

        let level_img = self.pyramid[li].clone();
        let (lw, lh) = (level_img.width(), level_img.height());
        let sx = dest.width() / lw as f32;
        let sy = dest.height() / lh as f32;
        let visible = dest.intersect(clip);
        if visible.width() <= 0.0 || visible.height() <= 0.0 {
            return false;
        }
        // Visible range in level pixel coordinates.
        let vx0 = (((visible.min.x - dest.min.x) / sx).floor().max(0.0)) as u32;
        let vy0 = (((visible.min.y - dest.min.y) / sy).floor().max(0.0)) as u32;
        let vx1 = (((visible.max.x - dest.min.x) / sx).ceil() as u32).min(lw);
        let vy1 = (((visible.max.y - dest.min.y) / sy).ceil() as u32).min(lh);
        let level = &mut self.levels[li];
        let c0 = vx0 / TILE;
        let r0 = vy0 / TILE;
        let c1 = vx1.saturating_sub(1) / TILE;
        let r1 = vy1.saturating_sub(1) / TILE;

        // Center-out ordering so the middle of the screen sharpens first.
        let mut order: Vec<(u32, u32)> = Vec::new();
        for r in r0..=r1.min(level.rows - 1) {
            for c in c0..=c1.min(level.cols - 1) {
                order.push((c, r));
            }
        }
        let cc = (c0 + c1) as f32 / 2.0;
        let cr = (r0 + r1) as f32 / 2.0;
        order.sort_by(|a, b| {
            let da = (a.0 as f32 - cc).powi(2) + (a.1 as f32 - cr).powi(2);
            let db = (b.0 as f32 - cc).powi(2) + (b.1 as f32 - cr).powi(2);
            da.partial_cmp(&db).unwrap()
        });

        let opts = tex_options(li == 0 && nearest);
        let top_tex = self.top.as_ref().map(|t| t.id());
        let mut meshes: Vec<(egui::TextureId, Rect, Rect)> = Vec::new();
        for (c, r) in order {
            let idx = (r * level.cols + c) as usize;
            let tx = c * TILE;
            let ty = r * TILE;
            let tw = TILE.min(lw - tx);
            let th = TILE.min(lh - ty);
            let screen = Rect::from_min_max(
                Pos2::new(dest.min.x + tx as f32 * sx, dest.min.y + ty as f32 * sy),
                Pos2::new(dest.min.x + (tx + tw) as f32 * sx, dest.min.y + (ty + th) as f32 * sy),
            );
            if level.tiles[idx].is_none() && (started.elapsed() < budget || meshes.is_empty() && li == self.pyramid.len() - 1) {
                let ci = to_color_image_gutter(&level_img, tx, ty, tw, th, GUTTER);
                level.tiles[idx] = Some(ctx.load_texture(format!("{}#{li}/{idx}", self.name), ci, opts));
            }
            level.last_used[idx] = self.frame_counter;
            match &level.tiles[idx] {
                Some(t) => meshes.push((t.id(), screen, gutter_uv(tw, th, GUTTER))),
                None => {
                    pending = true;
                    if let Some(top) = top_tex {
                        // UV of this tile within the whole image.
                        let uv = Rect::from_min_max(
                            Pos2::new(tx as f32 / lw as f32, ty as f32 / lh as f32),
                            Pos2::new((tx + tw) as f32 / lw as f32, (ty + th) as f32 / lh as f32),
                        );
                        meshes.push((top, screen, uv));
                    }
                }
            }
        }
        for (tex, rect, uv) in meshes {
            let mut mesh = egui::Mesh::with_texture(tex);
            mesh.add_rect_with_uv(rect, uv, tint);
            painter.add(egui::Shape::mesh(mesh));
        }
        self.evict(li);
        if pending {
            ctx.request_repaint();
        }
        pending
    }

    /// Free tiles of other levels and tiles not drawn recently to bound VRAM use.
    fn evict(&mut self, current: usize) {
        let now = self.frame_counter;
        let last = self.levels.len() - 1;
        for (li, level) in self.levels.iter_mut().enumerate() {
            if li == last {
                continue;
            }
            for (t, used) in level.tiles.iter_mut().zip(level.last_used.iter()) {
                if t.is_some() && (li != current && now - *used > 120 || now - *used > 600) {
                    *t = None;
                }
            }
        }
    }

    /// Paint a small preview using only the always-resident top level.
    pub fn paint_top(&self, painter: &egui::Painter, dest: Rect, tint: Color32) {
        if let Some(top) = &self.top {
            let mut mesh = egui::Mesh::with_texture(top.id());
            mesh.add_rect_with_uv(dest, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
            painter.add(egui::Shape::mesh(mesh));
        }
    }

    pub fn top_texture(&self) -> Option<&TextureHandle> {
        self.top.as_ref()
    }
}

/// Draw a checkerboard (for transparent images) inside `rect`.
pub fn paint_checkerboard(painter: &egui::Painter, rect: Rect, cell: f32, dark: bool) {
    let (a, b) = if dark {
        (Color32::from_gray(48), Color32::from_gray(64))
    } else {
        (Color32::from_gray(204), Color32::from_gray(240))
    };
    painter.rect_filled(rect, 0.0, a);
    let clip = painter.clip_rect().intersect(rect);
    if clip.width() <= 0.0 {
        return;
    }
    let x0 = ((clip.min.x - rect.min.x) / cell).floor() as i64;
    let y0 = ((clip.min.y - rect.min.y) / cell).floor() as i64;
    let x1 = ((clip.max.x - rect.min.x) / cell).ceil() as i64;
    let y1 = ((clip.max.y - rect.min.y) / cell).ceil() as i64;
    if (x1 - x0) * (y1 - y0) > 40_000 {
        return; // too many cells; plain background is fine
    }
    let mut mesh = egui::Mesh::default();
    for y in y0..y1 {
        for x in x0..x1 {
            if (x + y) % 2 == 0 {
                continue;
            }
            let r = Rect::from_min_size(
                Pos2::new(rect.min.x + x as f32 * cell, rect.min.y + y as f32 * cell),
                Vec2::splat(cell),
            )
            .intersect(rect);
            mesh.add_colored_rect(r, b);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 7, 255]))
    }

    #[test]
    fn gutter_copies_neighbours_inside_the_image() {
        let img = numbered(40, 30);
        let ci = to_color_image_gutter(&img, 10, 8, 12, 9, 2);
        assert_eq!(ci.size, [16, 13]);
        // Texel (0,0) is image pixel (8,6); the content starts at (2,2) = (10,8).
        assert_eq!(ci.pixels[0], Color32::from_rgb(8, 6, 7));
        assert_eq!(ci.pixels[2 * 16 + 2], Color32::from_rgb(10, 8, 7));
        // Last texel = image (10+12+1, 8+9+1).
        assert_eq!(*ci.pixels.last().unwrap(), Color32::from_rgb(23, 18, 7));
    }

    #[test]
    fn gutter_clamps_at_image_borders() {
        let img = numbered(5, 4);
        let ci = to_color_image_gutter(&img, 0, 0, 5, 4, 2);
        assert_eq!(ci.size, [9, 8]);
        assert_eq!(ci.pixels.len(), 72);
        for oy in 0..8usize {
            for ox in 0..9usize {
                let x = (ox as i32 - 2).clamp(0, 4) as u8;
                let y = (oy as i32 - 2).clamp(0, 3) as u8;
                assert_eq!(ci.pixels[oy * 9 + ox], Color32::from_rgb(x, y, 7), "at {ox},{oy}");
            }
        }
        // Without a gutter it is a plain copy.
        let plain = to_color_image(&img, 1, 1, 3, 2);
        assert_eq!(plain.size, [3, 2]);
        assert_eq!(plain.pixels[0], Color32::from_rgb(1, 1, 7));
    }

    #[test]
    fn gutter_keeps_premultiplication() {
        let mut img = numbered(4, 4);
        img.put_pixel(0, 0, image::Rgba([200, 100, 50, 128]));
        let ci = to_color_image_gutter(&img, 0, 0, 2, 2, 1);
        let expect = Color32::from_rgba_unmultiplied(200, 100, 50, 128);
        assert_eq!(ci.pixels[0], expect); // clamped corner
        assert_eq!(ci.pixels[4 + 1], expect);
    }

    #[test]
    fn gutter_uv_maps_content() {
        let uv = gutter_uv(1024, 1000, 2);
        assert!((uv.min.x - 2.0 / 1028.0).abs() < 1e-7);
        assert!((uv.max.x - 1026.0 / 1028.0).abs() < 1e-7);
        assert!((uv.min.y - 2.0 / 1004.0).abs() < 1e-7);
        assert!((uv.max.y - 1002.0 / 1004.0).abs() < 1e-7);
        assert_eq!(gutter_uv(10, 10, 0), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
    }
}
