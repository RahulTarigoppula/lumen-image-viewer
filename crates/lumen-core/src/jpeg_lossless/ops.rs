//! Moving coefficient blocks: crop, rotate and flip.

use super::{Jpeg, NATURAL};
use anyhow::{bail, Result};

/// A lossless geometric change: transpose first, then flip in the
/// transposed picture's frame. The eight combinations are the eight EXIF
/// orientations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Xform {
    pub(super) transpose: bool,
    pub(super) flip_h: bool,
    pub(super) flip_v: bool,
}

impl Xform {
    /// The change that turns a stored picture into how EXIF orientation `o`
    /// shows it (6: rotate 90 clockwise = transpose, then flip horizontally).
    pub(super) fn of_orientation(o: u8) -> Xform {
        let (transpose, flip_h, flip_v) = match o {
            2 => (false, true, false),
            3 => (false, true, true),
            4 => (false, false, true),
            5 => (true, false, false),
            6 => (true, true, false),
            7 => (true, true, true),
            8 => (true, false, true),
            _ => (false, false, false),
        };
        Xform { transpose, flip_h, flip_v }
    }

    fn is_identity(self) -> bool {
        !self.transpose && !self.flip_h && !self.flip_v
    }
}

/// For each output coefficient (zigzag order): the source coefficient and
/// whether its sign changes. Mirroring a block horizontally negates the odd
/// horizontal frequencies, vertically the odd vertical ones; transposing
/// swaps the two frequencies.
fn coef_map(x: Xform) -> [(u8, bool); 64] {
    let mut zigzag = [0u8; 64];
    for (k, &n) in NATURAL.iter().enumerate() {
        zigzag[n as usize] = k as u8;
    }
    let mut m = [(0u8, false); 64];
    for (k, &n) in NATURAL.iter().enumerate() {
        let (row, col) = (n as usize / 8, n as usize % 8);
        let neg = (x.flip_h && col % 2 == 1) != (x.flip_v && row % 2 == 1);
        let src = if x.transpose { col * 8 + row } else { n as usize };
        m[k] = (zigzag[src], neg);
    }
    m
}

/// Keep `w` x `h` stored pixels from (`x0`, `y0`), which must be on the MCU
/// grid. The right and bottom edges can be anywhere: the frame just ends
/// there (blocks past it stay, as padding).
pub(super) fn crop(j: &mut Jpeg, x0: u32, y0: u32, w: u32, h: u32) -> Result<()> {
    let (mw, mh) = j.imcu();
    if w == 0 || h == 0 || x0 % mw != 0 || y0 % mh != 0 || x0 + w > j.width || y0 + h > j.height {
        bail!("The crop area does not fit the picture");
    }
    let (mx, my) = (w.div_ceil(mw) as usize, h.div_ceil(mh) as usize);
    let (ox, oy) = ((x0 / mw) as usize, (y0 / mh) as usize);
    for c in &mut j.comps {
        let (bw, bh) = (mx * c.h, my * c.v);
        let (sx, sy) = (ox * c.h, oy * c.v);
        let mut out = vec![[0i16; 64]; bw * bh];
        let n = bw.min(c.bw.saturating_sub(sx));
        for by in 0..bh.min(c.bh.saturating_sub(sy)) {
            let src = (sy + by) * c.bw + sx;
            out[by * bw..by * bw + n].copy_from_slice(&c.blocks[src..src + n]);
        }
        c.blocks = out;
        c.bw = bw;
        c.bh = bh;
    }
    j.width = w;
    j.height = h;
    Ok(())
}

/// Apply `x` to the picture. Returns the pixels trimmed off the new width
/// and height: an edge that moves to the other side must be whole MCUs, so
/// a partial MCU column / row there is dropped (`jpegtran -trim`).
pub(super) fn transform(j: &mut Jpeg, x: Xform) -> Result<(u32, u32)> {
    if x.is_identity() {
        return Ok((0, 0));
    }
    let (mut w, mut h) = if x.transpose { (j.height, j.width) } else { (j.width, j.height) };
    // MCU size of the result: transposing swaps the sampling factors.
    let (mw, mh) = if x.transpose { (j.imcu().1, j.imcu().0) } else { j.imcu() };
    let trim = (if x.flip_h { w % mw } else { 0 }, if x.flip_v { h % mh } else { 0 });
    w -= trim.0;
    h -= trim.1;
    if w == 0 || h == 0 {
        bail!("This picture is too small to turn losslessly");
    }
    let (mx, my) = (w.div_ceil(mw) as usize, h.div_ceil(mh) as usize);
    let map = coef_map(x);
    for c in &mut j.comps {
        let (ch, cv) = if x.transpose { (c.v, c.h) } else { (c.h, c.v) };
        let (bw, bh) = (mx * ch, my * cv);
        let mut blocks = vec![[0i16; 64]; bw * bh];
        for by in 0..bh {
            for bx in 0..bw {
                let xx = if x.flip_h { bw - 1 - bx } else { bx };
                let yy = if x.flip_v { bh - 1 - by } else { by };
                let (sx, sy) = if x.transpose { (yy, xx) } else { (xx, yy) };
                if sx < c.bw && sy < c.bh {
                    let s = &c.blocks[sy * c.bw + sx];
                    for (d, &(k, neg)) in blocks[by * bw + bx].iter_mut().zip(&map) {
                        let v = s[k as usize];
                        *d = if neg { v.wrapping_neg() } else { v };
                    }
                }
            }
        }
        c.blocks = blocks;
        (c.h, c.v, c.bw, c.bh) = (ch, cv, bw, bh);
        if x.transpose {
            if let Some(q) = &mut c.q {
                let t = *q;
                for (d, &(k, _)) in q.iter_mut().zip(&map) {
                    *d = t[k as usize];
                }
            }
        }
    }
    j.width = w;
    j.height = h;
    Ok(trim)
}
