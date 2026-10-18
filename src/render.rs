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

fn to_color_image(img: &RgbaImage, x0: u32, y0: u32, w: u32, h: u32) -> ColorImage {
    let stride = img.width() as usize * 4;
    let raw = img.as_raw();
    let mut pixels = Vec::with_capacity((w * h) as usize);
    for y in y0..y0 + h {
        let start = y as usize * stride + x0 as usize * 4;
        let row = &raw[start..start + w as usize * 4];
        pixels.extend(
            row.chunks_exact(4)
                .map(|p| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])),
        );
    }
    ColorImage::new([w as usize, h as usize], pixels)
}

pub fn color_image_from_rgba(img: &RgbaImage) -> ColorImage {
    to_color_image(img, 0, 0, img.width(), img.height())
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
                let ci = to_color_image(&level_img, tx, ty, tw, th);
                level.tiles[idx] = Some(ctx.load_texture(format!("{}#{li}/{idx}", self.name), ci, opts));
            }
            level.last_used[idx] = self.frame_counter;
            match &level.tiles[idx] {
                Some(t) => meshes.push((t.id(), screen, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)))),
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
