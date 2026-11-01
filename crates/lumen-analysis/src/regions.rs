//! Prominent problem areas of one image from its no-reference tile maps:
//! soft detail (sharpness), blocking, banding and clipped highlights or
//! shadows. Each map's cells are marked bad against the image's own
//! statistics, joined into connected areas, merged when close, and only
//! areas large and strong enough to matter are kept (at most
//! [`MAX_REGIONS`]). Small specks and mild effects are left out on purpose:
//! the boxes are for what a viewer would notice.

use crate::heatmap::Heatmap;
use serde::{Deserialize, Serialize};

/// Most regions reported for one image.
pub const MAX_REGIONS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegionKind {
    Blurry,
    Blocky,
    Banding,
    ClippedHighlights,
    ClippedShadows,
}

impl RegionKind {
    /// Short name for a box tag or list row.
    pub fn title(self) -> &'static str {
        match self {
            RegionKind::Blurry => "Blurry area",
            RegionKind::Blocky => "Blocky compression",
            RegionKind::Banding => "Banding in gradient",
            RegionKind::ClippedHighlights => "Clipped highlights",
            RegionKind::ClippedShadows => "Crushed shadows",
        }
    }

    /// What the user sees there, in plain words.
    pub fn explain(self) -> &'static str {
        match self {
            RegionKind::Blurry => "fine detail is much softer here than in the rest of the image (out of focus, motion or smoothing)",
            RegionKind::Blocky => "square 8 × 8 blocks from strong compression are visible",
            RegionKind::Banding => "a smooth gradient shows steps or contour lines",
            RegionKind::ClippedHighlights => "pure white with no detail left",
            RegionKind::ClippedShadows => "pure black with no detail left",
        }
    }

    /// The metric whose map found it.
    pub fn metric(self) -> &'static str {
        match self {
            RegionKind::Blurry => "sharpness.laplacian",
            RegionKind::Blocky => "blockiness",
            RegionKind::Banding => "banding",
            RegionKind::ClippedHighlights | RegionKind::ClippedShadows => "exposure",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RegionLevel {
    Moderate,
    Severe,
}

/// One problem area.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub kind: RegionKind,
    pub level: RegionLevel,
    /// Image pixels: x0, y0, x1, y1.
    pub rect: [f32; 4],
    /// Share of the image the area's bad cells cover (0..100).
    pub area_pct: f32,
    /// How strong the effect is in the area, 0..1.
    pub strength: f32,
}

impl Region {
    /// "Blurry area: fine detail is much softer here ... (6% of the image)".
    pub fn description(&self) -> String {
        let size = if self.area_pct >= 50.0 { "most of the image".to_string() } else if self.area_pct >= 1.0 { format!("{:.0}% of the image", self.area_pct) } else { "a small area".to_string() };
        let strong = if self.level == RegionLevel::Severe { "Strong: " } else { "" };
        format!("{strong}{} ({size})", self.kind.explain())
    }

    fn score(&self) -> f32 {
        self.area_pct.max(0.1).sqrt() * (0.5 + self.strength) * if self.level == RegionLevel::Severe { 1.5 } else { 1.0 }
    }
}

/// Bad cells of one kind on a grid over the image.
struct Grid {
    w: usize,
    h: usize,
    x0: f32,
    y0: f32,
    cw: f32,
    ch: f32,
    /// Strength 0..1 per cell; 0 is fine.
    bad: Vec<f32>,
}

fn percentile(mut v: Vec<f32>, p: f32) -> Option<f32> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let i = ((v.len() - 1) as f32 * p / 100.0).round() as usize;
    Some(v[i.min(v.len() - 1)])
}

fn grid_like(m: &Heatmap, bad: Vec<f32>) -> Grid {
    Grid { w: m.width as usize, h: m.height as usize, x0: m.x0, y0: m.y0, cw: m.cell_w, ch: m.cell_h, bad }
}

/// Soft detail: the Laplacian variance (high frequencies) against the
/// gradient energy (Tenengrad) of each tile. Blur lowers the first far more
/// than the second, so the ratio finds soft areas without calling flat sky
/// or walls blurry (they have little of either). Relative to the image's
/// sharp tiles: a picture that is soft everywhere has no "blurry area".
fn blur_grid(lap: &Heatmap, ten: &Heatmap) -> Option<Grid> {
    if lap.width != ten.width || lap.height != ten.height || lap.data.len() != ten.data.len() {
        return None;
    }
    let t90 = percentile(ten.data.clone(), 90.0)?;
    let structured = |t: f32| t.is_finite() && t > (0.03 * t90).max(4.0);
    let ratios: Vec<f32> = lap.data.iter().zip(&ten.data).map(|(&l, &t)| if structured(t) && l.is_finite() { l / t } else { f32::NAN }).collect();
    let n = ratios.iter().filter(|r| r.is_finite()).count();
    if n < 8 {
        return None;
    }
    let reference = percentile(ratios.clone(), 75.0)?;
    if reference <= 0.0 {
        return None;
    }
    let bad = ratios.iter().map(|&r| if r.is_finite() && r < 0.3 * reference { (1.0 - r / (0.3 * reference)).clamp(0.2, 1.0) } else { 0.0 }).collect();
    Some(grid_like(lap, bad))
}

/// Tiles over a fixed limit, strength growing to 1 at `full`.
fn limit_grid(m: &Heatmap, limit: f32, full: f32) -> Grid {
    let bad = m.data.iter().map(|&v| if v.is_finite() && v >= limit { ((v - limit) / (full - limit) * 0.8 + 0.2).clamp(0.2, 1.0) } else { 0.0 }).collect();
    grid_like(m, bad)
}

/// Clipping from the per-pixel map (1 highlight, -1 shadow, NaN neither),
/// pooled into cells; a cell is bad when most of it is clipped.
fn clip_grid(m: &Heatmap, highlights: bool) -> Option<Grid> {
    let (w, h) = (m.width as usize, m.height as usize);
    if w == 0 || h == 0 || m.data.len() != w * h {
        return None;
    }
    let cell = (w.max(h) / 64).clamp(8, 64);
    let (gw, gh) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut count = vec![(0u32, 0u32); gw * gh];
    let want = if highlights { 1.0 } else { -1.0 };
    for y in 0..h {
        let row = &m.data[y * w..(y + 1) * w];
        let gy = y / cell;
        for (x, &v) in row.iter().enumerate() {
            let c = &mut count[gy * gw + x / cell];
            c.1 += 1;
            c.0 += (v == want) as u32;
        }
    }
    let bad = count.iter().map(|&(n, of)| {
        let f = if of == 0 { 0.0 } else { n as f32 / of as f32 };
        if f >= 0.6 { f } else { 0.0 }
    });
    let c = cell as f32 * m.cell_w;
    Some(Grid { w: gw, h: gh, x0: m.x0, y0: m.y0, cw: c, ch: cell as f32 * m.cell_h, bad: bad.collect() })
}

/// A connected area of bad cells: cell bbox, cell count, mean strength.
struct Blob {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    cells: usize,
    strength: f32,
}

fn blobs(g: &Grid) -> Vec<Blob> {
    let mut seen = vec![false; g.bad.len()];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for start in 0..g.bad.len() {
        if seen[start] || g.bad[start] <= 0.0 {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let (sx, sy) = (start % g.w, start / g.w);
        let mut b = Blob { x0: sx, y0: sy, x1: sx, y1: sy, cells: 0, strength: 0.0 };
        while let Some(i) = stack.pop() {
            let (x, y) = (i % g.w, i / g.w);
            b.cells += 1;
            b.strength += g.bad[i];
            b.x0 = b.x0.min(x);
            b.x1 = b.x1.max(x);
            b.y0 = b.y0.min(y);
            b.y1 = b.y1.max(y);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= g.w as i64 || ny >= g.h as i64 {
                        continue;
                    }
                    let j = ny as usize * g.w + nx as usize;
                    if !seen[j] && g.bad[j] > 0.0 {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        b.strength /= b.cells as f32;
        out.push(b);
    }
    out
}

/// Joins blobs whose boxes touch or are one cell apart (a soft area split
/// by one sharp tile is one area).
fn merge(mut v: Vec<Blob>) -> Vec<Blob> {
    let near = |a: &Blob, b: &Blob| a.x0 <= b.x1 + 2 && b.x0 <= a.x1 + 2 && a.y0 <= b.y1 + 2 && b.y0 <= a.y1 + 2;
    loop {
        let mut joined = false;
        'outer: for i in 0..v.len() {
            for j in i + 1..v.len() {
                if near(&v[i], &v[j]) {
                    let b = v.swap_remove(j);
                    let a = &mut v[i];
                    let n = (a.cells + b.cells) as f32;
                    a.strength = (a.strength * a.cells as f32 + b.strength * b.cells as f32) / n;
                    a.cells += b.cells;
                    a.x0 = a.x0.min(b.x0);
                    a.y0 = a.y0.min(b.y0);
                    a.x1 = a.x1.max(b.x1);
                    a.y1 = a.y1.max(b.y1);
                    joined = true;
                    break 'outer;
                }
            }
        }
        if !joined {
            return v;
        }
    }
}

/// Areas of one kind that matter: at least `min_pct` of the image.
fn regions_of(g: &Grid, kind: RegionKind, width: u32, height: u32, min_pct: f32, severe_strength: f32) -> Vec<Region> {
    let (iw, ih) = (width.max(1) as f32, height.max(1) as f32);
    let cell_area = (g.cw * g.ch).min(iw * ih);
    merge(blobs(g))
        .into_iter()
        .filter_map(|b| {
            let rect = [
                (g.x0 + b.x0 as f32 * g.cw).clamp(0.0, iw),
                (g.y0 + b.y0 as f32 * g.ch).clamp(0.0, ih),
                (g.x0 + (b.x1 + 1) as f32 * g.cw).clamp(0.0, iw),
                (g.y0 + (b.y1 + 1) as f32 * g.ch).clamp(0.0, ih),
            ];
            let area_pct = (100.0 * b.cells as f32 * cell_area / (iw * ih)).min(100.0);
            if area_pct < min_pct || b.cells < 2 || rect[2] <= rect[0] || rect[3] <= rect[1] {
                return None;
            }
            let level = if b.strength >= severe_strength { RegionLevel::Severe } else { RegionLevel::Moderate };
            Some(Region { kind, level, rect, area_pct, strength: b.strength })
        })
        .collect()
}

/// The prominent problem areas of an image `width` × `height` from its
/// no-reference maps, by metric id: `sharpness.laplacian` with
/// `sharpness.tenengrad` (soft areas need both), `blockiness`, `banding`
/// and `exposure`. Strongest first, at most [`MAX_REGIONS`]; none when
/// nothing stands out.
pub fn find_regions(maps: &[(&str, &Heatmap)], width: u32, height: u32) -> Vec<Region> {
    let map = |id: &str| maps.iter().find(|m| m.0 == id).map(|m| m.1).filter(|m| !m.data.is_empty());
    let mut out = Vec::new();
    if let (Some(lap), Some(ten)) = (map("sharpness.laplacian"), map("sharpness.tenengrad")) {
        if let Some(g) = blur_grid(lap, ten) {
            out.extend(regions_of(&g, RegionKind::Blurry, width, height, 2.0, 0.7));
        }
    }
    if let Some(m) = map("blockiness") {
        out.extend(regions_of(&limit_grid(m, 0.35, 1.0), RegionKind::Blocky, width, height, 1.5, 0.6));
    }
    if let Some(m) = map("banding") {
        out.extend(regions_of(&limit_grid(m, 10.0, 35.0), RegionKind::Banding, width, height, 1.0, 0.6));
    }
    if let Some(m) = map("exposure") {
        for (hi, kind) in [(true, RegionKind::ClippedHighlights), (false, RegionKind::ClippedShadows)] {
            if let Some(g) = clip_grid(m, hi) {
                out.extend(regions_of(&g, kind, width, height, 0.75, 0.95));
            }
        }
    }
    out.sort_by(|a, b| b.score().total_cmp(&a.score()));
    out.truncate(MAX_REGIONS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiles(w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> Heatmap {
        let data = (0..w * h).map(|i| f(i % w, i / w)).collect();
        Heatmap::new(w, h, data, "t").with_cell(64.0, 64.0)
    }

    #[test]
    fn blocky_tiles_join_into_one_box() {
        // 10 x 8 tiles of 64 px; a 3 x 2 blocky patch plus one stray tile.
        let m = tiles(10, 8, |x, y| if (6..9).contains(&x) && (1..3).contains(&y) || (x, y) == (0, 7) { 0.8 } else { 0.05 });
        let r = find_regions(&[("blockiness", &m)], 640, 512);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].kind, RegionKind::Blocky);
        assert_eq!(r[0].rect, [384.0, 64.0, 576.0, 192.0]);
        assert!(r[0].description().contains("blocks"));
    }

    #[test]
    fn close_patches_merge_and_count_is_capped() {
        // Two banded patches one tile apart are one area.
        let m = tiles(12, 4, |x, y| if y < 2 && (x < 2 || (3..5).contains(&x)) { 30.0 } else { f32::NAN });
        let r = find_regions(&[("banding", &m)], 768, 256);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].rect[2], 320.0);
        // Many separate areas: only the strongest few.
        let m = tiles(40, 40, |x, y| if x % 4 < 2 && y % 4 < 2 { 0.9 } else { 0.0 });
        let r = find_regions(&[("blockiness", &m)], 2560, 2560);
        assert!(r.len() <= MAX_REGIONS);
    }

    #[test]
    fn clipped_sky_is_one_highlight_area() {
        let (w, h) = (256usize, 128usize);
        let data = (0..w * h).map(|i| if i / w < 40 { 1.0 } else if i / w > 120 && i % w < 4 { -1.0 } else { f32::NAN }).collect();
        let m = Heatmap::new(w, h, data, "clip");
        let r = find_regions(&[("exposure", &m)], w as u32, h as u32);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].kind, RegionKind::ClippedHighlights);
        assert!(r[0].rect[1] == 0.0 && r[0].rect[3] <= 48.0, "{:?}", r[0].rect);
    }

    #[test]
    fn nothing_without_maps_or_problems() {
        assert!(find_regions(&[], 100, 100).is_empty());
        let m = tiles(8, 8, |_, _| 0.02);
        assert!(find_regions(&[("blockiness", &m)], 512, 512).is_empty());
    }
}
