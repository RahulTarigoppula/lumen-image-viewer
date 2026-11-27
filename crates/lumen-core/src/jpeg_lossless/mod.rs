//! Lossless JPEG editing: crop, rotate and flip by moving the quantized DCT
//! coefficient blocks around instead of decoding and re-encoding the picture
//! (the way `jpegtran` works), so no quality is lost.
//!
//! A file is read into its coefficients (baseline or progressive, Huffman
//! coded, 8-bit, any sampling, 1 to 4 components), changed, and written back
//! as a baseline JPEG with the same quantization tables, component ids and
//! sampling, and optimized Huffman tables. APPn and COM segments (EXIF, ICC,
//! XMP, comments) stay in order; the EXIF pixel size and thumbnail follow the
//! new picture, and an HDR gain map (MPF secondary image) that would no
//! longer match is removed.

mod bits;
mod decode;
mod encode;
mod huffman;
mod meta;
mod ops;
#[cfg(test)]
mod tests;

use crate::edit::Transform;
use anyhow::{bail, Result};
use std::path::Path;

use ops::Xform;

/// Zigzag position -> natural (row-major) position in an 8 x 8 block.
const NATURAL: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60,
    61, 54, 47, 55, 62, 63,
];

/// One color component and its coefficient blocks.
#[derive(Clone)]
struct Component {
    id: u8,
    /// Sampling factors (1..=4; always 1 in a one-component picture).
    h: usize,
    v: usize,
    /// Quantization table slot named by the frame header.
    tq: usize,
    /// The quantization table (zigzag order) in effect for this component's
    /// first scan.
    q: Option<[u16; 64]>,
    /// Blocks per row and column, padded to whole MCUs.
    bw: usize,
    bh: usize,
    /// Quantized coefficients in zigzag order, blocks row by row.
    blocks: Vec<[i16; 64]>,
}

/// A JPEG as coefficients plus the segments kept as they are.
#[derive(Clone)]
struct Jpeg {
    width: u32,
    height: u32,
    comps: Vec<Component>,
    /// Restart interval (MCUs) of the first scan; 0 for none.
    restart: u16,
    progressive: bool,
    /// APPn and COM segments in file order: (marker, payload).
    segments: Vec<(u8, Vec<u8>)>,
    /// Bytes after the end-of-image marker (MPF images, motion photo video).
    trailer: Vec<u8>,
}

impl Jpeg {
    fn hmax(&self) -> usize {
        self.comps.iter().map(|c| c.h).max().unwrap_or(1)
    }

    fn vmax(&self) -> usize {
        self.comps.iter().map(|c| c.v).max().unwrap_or(1)
    }

    /// Pixels per MCU: the grid crops start on and flips keep whole.
    fn imcu(&self) -> (u32, u32) {
        (8 * self.hmax() as u32, 8 * self.vmax() as u32)
    }

    /// MCUs per row and column.
    fn mcus(&self) -> (usize, usize) {
        let (mw, mh) = self.imcu();
        (self.width.div_ceil(mw) as usize, self.height.div_ceil(mh) as usize)
    }

    /// Blocks of `c` that hold picture (what a scan of `c` alone codes).
    fn real_blocks(&self, c: &Component) -> (usize, usize) {
        let w = (self.width as usize * c.h).div_ceil(self.hmax());
        let h = (self.height as usize * c.v).div_ceil(self.vmax());
        (w.div_ceil(8), h.div_ceil(8))
    }

    /// EXIF orientation (1..=8; 1 without one).
    fn orientation(&self) -> u8 {
        self.segments
            .iter()
            .filter(|(m, _)| *m == 0xE1)
            .find_map(|(_, b)| meta::exif_tiff(b).and_then(meta::orientation))
            .filter(|o| (1..=8).contains(o))
            .map_or(1, |o| o as u8)
    }

    fn grid(&self) -> CropGrid {
        CropGrid { stored: (self.width, self.height), orientation: self.orientation(), step: self.imcu() }
    }
}

/// What a lossless crop needs to know about a JPEG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropGrid {
    /// Picture size as stored in the file (before the EXIF orientation).
    pub stored: (u32, u32),
    /// EXIF orientation (1..=8) the picture is shown with.
    pub orientation: u8,
    /// Block grid in stored pixels: 8 or 16 per side, from the chroma
    /// subsampling. A crop's stored left and top edges sit on it.
    pub step: (u32, u32),
}

/// How one displayed axis snaps to the block grid: the step, and whether the
/// grid is counted from the far side (then the right or bottom edge snaps,
/// otherwise the left or top edge). The other edge can be anywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisSnap {
    pub step: u32,
    pub from_end: bool,
}

impl CropGrid {
    /// Picture size as shown (after the EXIF orientation).
    pub fn display_size(&self) -> (u32, u32) {
        if self.orientation >= 5 {
            (self.stored.1, self.stored.0)
        } else {
            self.stored
        }
    }

    /// The snapping of the displayed x and y axes: the stored left and top
    /// edges end up on these sides of the shown picture.
    pub fn axes(&self) -> (AxisSnap, AxisSnap) {
        let (sx, sy) = self.step;
        let a = |step, from_end| AxisSnap { step, from_end };
        match self.orientation {
            2 => (a(sx, true), a(sy, false)),
            3 => (a(sx, true), a(sy, true)),
            4 => (a(sx, false), a(sy, true)),
            5 => (a(sy, false), a(sx, false)),
            6 => (a(sy, true), a(sx, false)),
            7 => (a(sy, true), a(sx, true)),
            8 => (a(sy, false), a(sx, true)),
            _ => (a(sx, false), a(sy, false)),
        }
    }

    /// `r` (x, y, w, h in displayed pixels) kept inside the picture, at least 1 px.
    fn clamp(&self, r: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let (dw, dh) = self.display_size();
        let x = r.0.min(dw.saturating_sub(1));
        let y = r.1.min(dh.saturating_sub(1));
        (x, y, r.2.min(dw - x).max(1), r.3.min(dh - y).max(1))
    }

    /// A rectangle (x, y, w, h) in displayed pixels as stored pixels.
    pub fn to_stored(&self, r: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let (sw, sh) = self.stored;
        let (x0, y0, x1, y1) = (r.0, r.1, r.0 + r.2, r.1 + r.3);
        // Stored x and y ranges (see `axes` for which displayed edge each is).
        let (a0, a1, b0, b1) = match self.orientation {
            2 => (sw.saturating_sub(x1), sw.saturating_sub(x0), y0, y1),
            3 => (sw.saturating_sub(x1), sw.saturating_sub(x0), sh.saturating_sub(y1), sh.saturating_sub(y0)),
            4 => (x0, x1, sh.saturating_sub(y1), sh.saturating_sub(y0)),
            5 => (y0, y1, x0, x1),
            6 => (y0, y1, sh.saturating_sub(x1), sh.saturating_sub(x0)),
            7 => (sw.saturating_sub(y1), sw.saturating_sub(y0), sh.saturating_sub(x1), sh.saturating_sub(x0)),
            8 => (sw.saturating_sub(y1), sw.saturating_sub(y0), x0, x1),
            _ => (x0, x1, y0, y1),
        };
        (a0, b0, a1 - a0, b1 - b0)
    }

    /// A rectangle (x, y, w, h) in stored pixels as displayed pixels.
    pub fn to_display(&self, r: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let (sw, sh) = self.stored;
        let (a0, b0, a1, b1) = (r.0, r.1, r.0 + r.2, r.1 + r.3);
        let (x0, x1, y0, y1) = match self.orientation {
            2 => (sw.saturating_sub(a1), sw.saturating_sub(a0), b0, b1),
            3 => (sw.saturating_sub(a1), sw.saturating_sub(a0), sh.saturating_sub(b1), sh.saturating_sub(b0)),
            4 => (a0, a1, sh.saturating_sub(b1), sh.saturating_sub(b0)),
            5 => (b0, b1, a0, a1),
            6 => (sh.saturating_sub(b1), sh.saturating_sub(b0), a0, a1),
            7 => (sh.saturating_sub(b1), sh.saturating_sub(b0), sw.saturating_sub(a1), sw.saturating_sub(a0)),
            8 => (b0, b1, sw.saturating_sub(a1), sw.saturating_sub(a0)),
            _ => (a0, a1, b0, b1),
        };
        (x0, y0, x1 - x0, y1 - y0)
    }

    /// The area a lossless crop of `r` (displayed pixels) keeps: the edges
    /// that sit on the block grid move out to it.
    pub fn snap(&self, r: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let s = self.to_stored(self.clamp(r));
        let (x0, y0) = (s.0 / self.step.0 * self.step.0, s.1 / self.step.1 * self.step.1);
        self.to_display((x0, y0, s.0 + s.2 - x0, s.1 + s.3 - y0))
    }
}

/// The result of a lossless edit.
#[derive(Clone, Debug)]
pub struct Edited {
    /// The new file (empty once written by [`crop_file`]).
    pub bytes: Vec<u8>,
    /// New picture size as stored (before the EXIF orientation).
    pub stored: (u32, u32),
    /// The area kept, in displayed pixels of the original (the whole new
    /// picture after a rotation).
    pub kept: (u32, u32, u32, u32),
    /// Pixels cut off the new width and height: a rotation or flip keeps only
    /// whole blocks at the edges that move.
    pub trimmed: (u32, u32),
    /// An HDR gain map was removed (it would no longer match the picture).
    pub gain_map_removed: bool,
}

/// The block grid and orientation of the JPEG `data`, read from its headers
/// only. Fails for JPEGs lossless editing cannot handle (arithmetic coding,
/// 12-bit, lossless or hierarchical).
pub fn probe(data: &[u8]) -> Result<CropGrid> {
    match decode::headers(data)? {
        Some(h) => Ok(h.grid()),
        None => bail!(decode::INCOMPLETE),
    }
}

/// [`probe`] for a file, reading only its start when the headers are there.
pub fn probe_file(path: &Path) -> Result<CropGrid> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    (&mut f).take(256 << 10).read_to_end(&mut buf)?;
    if let Some(h) = decode::headers(&buf)? {
        return Ok(h.grid());
    }
    f.read_to_end(&mut buf)?;
    probe(&buf)
}

/// Crop the JPEG `data` to `rect` (x, y, w, h in displayed pixels, after
/// the EXIF orientation) without re-encoding. The stored left and top edges
/// move out to the block grid ([`CropGrid::snap`]); the others stay.
pub fn crop(data: &[u8], rect: (u32, u32, u32, u32)) -> Result<Edited> {
    let mut j = decode::parse(data)?;
    let grid = j.grid();
    let s = grid.to_stored(grid.clamp(rect));
    let (x0, y0) = (s.0 / grid.step.0 * grid.step.0, s.1 / grid.step.1 * grid.step.1);
    let (w, h) = (s.0 + s.2 - x0, s.1 + s.3 - y0);
    ops::crop(&mut j, x0, y0, w, h)?;
    let gain_map_removed = meta::update(&mut j, false);
    Ok(Edited { bytes: encode::write(&j)?, stored: (w, h), kept: grid.to_display((x0, y0, w, h)), trimmed: (0, 0), gain_map_removed })
}

/// Rotate or flip the picture itself (not just its EXIF orientation) by `op`
/// as it is shown: the EXIF orientation is applied as well and reset to
/// normal. Edge blocks that cannot move whole are trimmed (`jpegtran -trim`).
pub fn rotate(data: &[u8], op: Transform) -> Result<Edited> {
    let mut j = decode::parse(data)?;
    let target = crate::encode::compose_orientation(j.orientation(), op);
    let trimmed = ops::transform(&mut j, Xform::of_orientation(target))?;
    let gain_map_removed = meta::update(&mut j, true);
    let (w, h) = (j.width, j.height);
    Ok(Edited { bytes: encode::write(&j)?, stored: (w, h), kept: (0, 0, w, h), trimmed, gain_map_removed })
}

/// [`crop`] the JPEG file at `path` in place (written atomically).
pub fn crop_file(path: &Path, rect: (u32, u32, u32, u32)) -> Result<Edited> {
    let data = std::fs::read(path)?;
    let mut e = crop(&data, rect)?;
    crate::encode::write_atomic(path, &e.bytes)?;
    e.bytes = Vec::new();
    Ok(e)
}
