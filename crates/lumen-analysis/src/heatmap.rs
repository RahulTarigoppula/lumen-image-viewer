//! Heatmaps: one scalar map type for every metric that has a map (diff,
//! SSIM, Delta-E, clipping, no-reference tile maps), colormap LUTs,
//! range modes, worst-value-preserving downsampling and PNG / float TIFF export.

use crate::colormaps;
use crate::metrics::Unit;
use crate::stats;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Display downsampling target (longest side) for overlays.
pub const MAX_DISPLAY_SIDE: u32 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Colormap {
    #[default]
    Magma,
    Viridis,
    Turbo,
    Gray,
    /// Diverging blue (negative) / white / red (positive), for signed maps.
    RdBu,
}

impl Colormap {
    pub const ALL: [Colormap; 5] = [Colormap::Magma, Colormap::Viridis, Colormap::Turbo, Colormap::Gray, Colormap::RdBu];

    pub fn name(self) -> &'static str {
        match self {
            Colormap::Magma => "Magma",
            Colormap::Viridis => "Viridis",
            Colormap::Turbo => "Turbo",
            Colormap::Gray => "Gray",
            Colormap::RdBu => "Red-blue",
        }
    }

    /// The 256-entry RGBA lookup table (opaque).
    pub fn lut(self) -> [[u8; 4]; 256] {
        let rgb: &[[u8; 3]; 256] = match self {
            Colormap::Magma => &colormaps::MAGMA,
            Colormap::Viridis => &colormaps::VIRIDIS,
            Colormap::Turbo => &colormaps::TURBO,
            Colormap::RdBu => &colormaps::RDBU,
            Colormap::Gray => return std::array::from_fn(|i| [i as u8, i as u8, i as u8, 255]),
        };
        std::array::from_fn(|i| [rgb[i][0], rgb[i][1], rgb[i][2], 255])
    }

    /// Color at `t` in 0..1.
    pub fn color(self, t: f32) -> [u8; 4] {
        self.lut()[(t.clamp(0.0, 1.0) * 255.0 + 0.5) as usize]
    }
}

/// How values are reduced when a map is downsampled: the worst value of each
/// block wins so hotspots are not averaged away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pool {
    /// Higher is worse (difference, Delta-E, masks).
    #[default]
    Max,
    /// Lower is worse (SSIM).
    Min,
    /// Signed maps: largest magnitude, sign kept.
    MaxAbs,
}

/// Value range used for coloring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeMode {
    /// Maps with an ideal value ([`Heatmap::min_span`] > 0): from the ideal
    /// end of the fixed range to the 99th percentile (1st for SSIM), at least
    /// `min_span` wide, so near-identical images read as nearly clean while
    /// small differences stay visible. Other maps: 1st to 99th percentile.
    /// Signed maps are symmetric around 0.
    #[default]
    Auto,
    /// The metric's default range ([`Heatmap::fixed_range`]).
    Fixed,
    User { lo: f32, hi: f32 },
}

/// Coloring options for [`Heatmap::colorize`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorizeOptions {
    pub colormap: Colormap,
    pub range: RangeMode,
    /// Map high values to the low end of the colormap (SSIM: low is bad).
    pub invert: bool,
    /// Only pixels at or beyond this value are drawn (beyond means below it
    /// when `invert` is set); others are transparent.
    pub threshold: Option<f32>,
    /// Alpha of drawn pixels.
    pub alpha: u8,
}

/// A scalar map over the image (or ROI). Cell `(i, j)` covers image pixels
/// `x0 + i * cell_w .. x0 + (i + 1) * cell_w` (same for y); NaN means "no value".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Heatmap {
    pub width: u32,
    pub height: u32,
    #[serde(skip)]
    pub data: Vec<f32>,
    pub label: String,
    pub unit: Option<Unit>,
    /// Metric-specific default range (e.g. Delta-E 0..10, SSIM 0.7..1).
    pub fixed_range: (f32, f32),
    /// Smallest span of the auto range, anchored at the ideal end of
    /// `fixed_range` (0: no ideal value, plain percentiles).
    #[serde(default)]
    pub min_span: f32,
    pub colormap: Colormap,
    pub invert: bool,
    pub pool: Pool,
    pub x0: f32,
    pub y0: f32,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Heatmap {
    /// A per-pixel map (cell size 1) starting at `(x0, y0)`.
    pub fn new(width: usize, height: usize, data: Vec<f32>, label: impl Into<String>) -> Self {
        debug_assert_eq!(data.len(), width * height);
        Heatmap {
            width: width as u32,
            height: height as u32,
            data,
            label: label.into(),
            unit: None,
            fixed_range: (0.0, 1.0),
            min_span: 0.0,
            colormap: Colormap::Magma,
            invert: false,
            pool: Pool::Max,
            x0: 0.0,
            y0: 0.0,
            cell_w: 1.0,
            cell_h: 1.0,
        }
    }

    pub fn with_unit(mut self, u: Unit) -> Self {
        self.unit = Some(u);
        self
    }

    pub fn with_range(mut self, lo: f32, hi: f32) -> Self {
        self.fixed_range = (lo, hi);
        self
    }

    /// Anchors the auto range at the ideal end of the fixed range, at least
    /// `span` wide (see [`RangeMode::Auto`]).
    pub fn with_min_span(mut self, span: f32) -> Self {
        self.min_span = span;
        self
    }

    pub fn with_style(mut self, colormap: Colormap, invert: bool, pool: Pool) -> Self {
        self.colormap = colormap;
        self.invert = invert;
        self.pool = pool;
        self
    }

    pub fn with_origin(mut self, x0: f32, y0: f32) -> Self {
        self.x0 = x0;
        self.y0 = y0;
        self
    }

    pub fn with_cell(mut self, w: f32, h: f32) -> Self {
        self.cell_w = w;
        self.cell_h = h;
        self
    }

    /// Default options for this map (its colormap, inversion, auto range, opaque).
    pub fn default_options(&self) -> ColorizeOptions {
        ColorizeOptions { colormap: self.colormap, range: RangeMode::Auto, invert: self.invert, threshold: None, alpha: 255 }
    }

    /// Value at an image coordinate (pixels, same space as `x0`/`y0`).
    pub fn value_at(&self, x: f32, y: f32) -> Option<f32> {
        let i = ((x - self.x0) / self.cell_w).floor();
        let j = ((y - self.y0) / self.cell_h).floor();
        if i < 0.0 || j < 0.0 || i >= self.width as f32 || j >= self.height as f32 {
            return None;
        }
        let v = self.data[j as usize * self.width as usize + i as usize];
        (!v.is_nan()).then_some(v)
    }

    /// Image-space rectangle (x, y, w, h) covered by the map.
    pub fn extent(&self) -> (f32, f32, f32, f32) {
        (self.x0, self.y0, self.width as f32 * self.cell_w, self.height as f32 * self.cell_h)
    }

    /// Resolves a range mode to (lo, hi).
    pub fn range(&self, mode: RangeMode) -> (f32, f32) {
        match mode {
            RangeMode::Fixed => self.fixed_range,
            RangeMode::User { lo, hi } => (lo, hi),
            RangeMode::Auto => {
                let (mn, mx) = self.data.iter().filter(|v| v.is_finite()).fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &v| (a.min(v), b.max(v)));
                if !mn.is_finite() {
                    return self.fixed_range;
                }
                if mx <= mn && self.min_span <= 0.0 && self.pool != Pool::MaxAbs {
                    return (mn, mn + 1e-6);
                }
                let s = stats::summarize(&self.data, mn, mx, 4096);
                let (lo, hi) = (s.percentile(1.0) as f32, s.percentile(99.0) as f32);
                let span = self.min_span;
                if self.pool == Pool::MaxAbs {
                    let m = lo.abs().max(hi.abs()).max(span).max(1e-6);
                    (-m, m)
                } else if span > 0.0 {
                    // Anchored at the ideal value: 0 for errors, the top for SSIM.
                    match self.pool {
                        Pool::Min => {
                            let top = self.fixed_range.1;
                            (lo.min(top - span), top)
                        }
                        _ => {
                            let bottom = self.fixed_range.0;
                            (bottom, hi.max(bottom + span))
                        }
                    }
                } else if hi > lo {
                    (lo, hi)
                } else {
                    (mn, mx)
                }
            }
        }
    }

    /// RGBA8 pixels, `width * height * 4` bytes; NaN and thresholded-out cells are transparent.
    pub fn colorize(&self, o: &ColorizeOptions) -> Vec<u8> {
        let (lo, hi) = self.range(o.range);
        let lut = o.colormap.lut();
        let span = hi - lo;
        let inv = if span.abs() > 0.0 { 1.0 / span } else { 0.0 };
        let mut out = vec![0u8; self.data.len() * 4];
        for (px, &v) in out.as_chunks_mut::<4>().0.iter_mut().zip(&self.data) {
            if v.is_nan() {
                continue;
            }
            if let Some(t) = o.threshold {
                let keep = if o.invert { v <= t } else { v >= t };
                if !keep {
                    continue;
                }
            }
            let mut t = ((v - lo) * inv).clamp(0.0, 1.0);
            if o.invert {
                t = 1.0 - t;
            }
            let c = lut[(t * 255.0 + 0.5) as usize];
            px.copy_from_slice(&[c[0], c[1], c[2], o.alpha]);
        }
        out
    }

    /// Reduces the map so its longest side is at most `max_side`, pooling
    /// blocks by [`Heatmap::pool`]. Returns a clone when already small enough.
    pub fn downsample(&self, max_side: u32) -> Heatmap {
        let side = self.width.max(self.height);
        if side <= max_side || max_side == 0 {
            return self.clone();
        }
        let f = side.div_ceil(max_side) as usize;
        let (w, h) = (self.width as usize, self.height as usize);
        let (w2, h2) = (w.div_ceil(f), h.div_ceil(f));
        let mut out = vec![f32::NAN; w2 * h2];
        use rayon::prelude::*;
        out.par_chunks_mut(w2).enumerate().for_each(|(j, row)| {
            for (i, o) in row.iter_mut().enumerate() {
                let mut acc = f32::NAN;
                for y in j * f..((j + 1) * f).min(h) {
                    for &v in &self.data[y * w + i * f..y * w + ((i + 1) * f).min(w)] {
                        if v.is_nan() {
                            continue;
                        }
                        acc = if acc.is_nan() {
                            v
                        } else {
                            match self.pool {
                                Pool::Max => acc.max(v),
                                Pool::Min => acc.min(v),
                                Pool::MaxAbs => {
                                    if v.abs() > acc.abs() {
                                        v
                                    } else {
                                        acc
                                    }
                                }
                            }
                        };
                    }
                }
                *o = acc;
            }
        });
        Heatmap {
            width: w2 as u32,
            height: h2 as u32,
            data: out,
            label: self.label.clone(),
            unit: self.unit,
            fixed_range: self.fixed_range,
            min_span: self.min_span,
            colormap: self.colormap,
            invert: self.invert,
            pool: self.pool,
            x0: self.x0,
            y0: self.y0,
            cell_w: self.cell_w * f as f32,
            cell_h: self.cell_h * f as f32,
        }
    }

    /// The map at display size (longest side at most [`MAX_DISPLAY_SIDE`]).
    pub fn for_display(&self) -> Heatmap {
        self.downsample(MAX_DISPLAY_SIDE)
    }

    /// Writes the colorized map as an 8-bit RGBA PNG (atomic: temp file, then rename).
    pub fn write_png(&self, path: &Path, o: &ColorizeOptions) -> std::io::Result<()> {
        let rgba = self.colorize(o);
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, self.width, self.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().map_err(std::io::Error::other)?;
            w.write_image_data(&rgba).map_err(std::io::Error::other)?;
            w.finish().map_err(std::io::Error::other)?;
        }
        crate::report::write_atomic(path, &buf)
    }

    /// Writes the raw values as a single-channel 32-bit float TIFF (NaN kept).
    pub fn write_tiff(&self, path: &Path) -> std::io::Result<()> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut enc = tiff::encoder::TiffEncoder::new(&mut buf).map_err(std::io::Error::other)?;
            enc.write_image::<tiff::encoder::colortype::Gray32Float>(self.width, self.height, &self.data).map_err(std::io::Error::other)?;
        }
        crate::report::write_atomic(path, buf.get_ref())
    }
}

/// Legend colors for a colormap: `n` RGBA entries from low to high.
pub fn legend(colormap: Colormap, n: usize, invert: bool) -> Vec<[u8; 4]> {
    (0..n)
        .map(|i| {
            let t = if n > 1 { i as f32 / (n - 1) as f32 } else { 0.0 };
            colormap.color(if invert { 1.0 - t } else { t })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_keeps_hotspot() {
        let mut data = vec![0.0f32; 10_000 * 10];
        data[5 * 10_000 + 7777] = 9.0;
        let m = Heatmap::new(10_000, 10, data, "t");
        let d = m.downsample(4096);
        assert!(d.width <= 4096 && d.height >= 1);
        assert_eq!(d.data.iter().cloned().fold(f32::MIN, f32::max), 9.0);
        assert_eq!(d.cell_w, 3.0);
        // The hotspot is still found at its image position.
        assert_eq!(d.value_at(7777.0, 5.0), Some(9.0));
        let mut s = Heatmap::new(4, 1, vec![0.9, 0.2, 0.95, 1.0], "s");
        s.pool = Pool::Min;
        assert_eq!(s.downsample(2).data, vec![0.2, 0.95]);
    }

    #[test]
    fn colorize_ranges_and_threshold() {
        let m = Heatmap::new(3, 1, vec![0.0, 0.5, f32::NAN], "t").with_range(0.0, 1.0);
        let o = ColorizeOptions { colormap: Colormap::Gray, range: RangeMode::Fixed, invert: false, threshold: None, alpha: 200 };
        let px = m.colorize(&o);
        assert_eq!(&px[0..4], &[0, 0, 0, 200]);
        assert_eq!(&px[4..8], &[128, 128, 128, 200]);
        assert_eq!(px[11], 0, "NaN is transparent");
        let t = m.colorize(&ColorizeOptions { threshold: Some(0.4), ..o });
        assert_eq!(t[3], 0);
        assert_eq!(t[7], 200);
        let signed = Heatmap::new(3, 1, vec![-2.0, 0.5, 1.0], "s").with_style(Colormap::RdBu, false, Pool::MaxAbs);
        let (lo, hi) = signed.range(RangeMode::Auto);
        assert!(lo < 0.0 && (lo + hi).abs() < 1e-6);
        assert_eq!(Colormap::Magma.lut()[0], [0, 0, 4, 255]);
    }

    #[test]
    fn auto_range_is_anchored_at_the_ideal_value() {
        // Near-identical SSIM: 0.999 everywhere but one pixel. The range keeps
        // its minimum span, so the map reads as nearly clean, not saturated.
        let mut v = vec![0.999f32; 1000];
        v[0] = 0.99;
        let ssim = Heatmap::new(1000, 1, v, "s").with_range(0.7, 1.0).with_min_span(0.1).with_style(Colormap::Magma, true, Pool::Min);
        let (lo, hi) = ssim.range(RangeMode::Auto);
        assert!((lo - 0.9).abs() < 1e-6 && hi == 1.0, "{lo} {hi}");
        // A really bad map stretches to its data.
        let bad = Heatmap::new(4, 1, vec![0.2, 0.5, 0.8, 1.0], "s").with_range(0.7, 1.0).with_min_span(0.1).with_style(Colormap::Magma, true, Pool::Min);
        let (lo, hi) = bad.range(RangeMode::Auto);
        assert!(lo < 0.3 && hi == 1.0);
        // Identical images: all zero difference stays at the clean end.
        let same = Heatmap::new(4, 1, vec![0.0; 4], "d").with_range(0.0, 64.0).with_min_span(16.0);
        assert_eq!(same.range(RangeMode::Auto), (0.0, 16.0));
        assert_eq!(&same.colorize(&same.default_options())[0..3], &Colormap::Magma.lut()[0][0..3]);
        let big = Heatmap::new(3, 1, vec![0.0, 10.0, 50.0], "d").with_range(0.0, 64.0).with_min_span(16.0);
        assert_eq!(big.range(RangeMode::Auto).0, 0.0);
        assert!(big.range(RangeMode::Auto).1 > 40.0);
    }
}
