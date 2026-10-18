//! ICO / CUR: pick the largest entry (then the deepest), PNG or BMP encoded.
//! The image crate alone prefers bit depth over size and rejects CUR files.

use super::Decoded;
use anyhow::{anyhow, bail, Result};

struct Entry {
    w: u32,
    h: u32,
    bpp: u16,
    len: usize,
    off: usize,
}

pub fn is_ico(b: &[u8]) -> bool {
    b.len() >= 6 && b[0] == 0 && b[1] == 0 && matches!(b[2], 1 | 2) && b[3] == 0 && u16::from_le_bytes([b[4], b[5]]) > 0
}

fn entries(b: &[u8]) -> Result<Vec<Entry>> {
    if !is_ico(b) {
        bail!("Not an icon file");
    }
    let n = u16::from_le_bytes([b[4], b[5]]) as usize;
    let is_cur = b[2] == 2;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let e = 6 + i * 16;
        let Some(d) = b.get(e..e + 16) else { break };
        let w = if d[0] == 0 { 256 } else { d[0] as u32 };
        let h = if d[1] == 0 { 256 } else { d[1] as u32 };
        // For CUR files planes/bpp hold the hotspot.
        let bpp = if is_cur { 0 } else { u16::from_le_bytes([d[6], d[7]]) };
        let len = u32::from_le_bytes([d[8], d[9], d[10], d[11]]) as usize;
        let off = u32::from_le_bytes([d[12], d[13], d[14], d[15]]) as usize;
        if off.checked_add(len).map(|end| end <= b.len()).unwrap_or(false) && len > 0 {
            v.push(Entry { w, h, bpp, len, off });
        }
    }
    if v.is_empty() {
        bail!("Icon file has no readable images");
    }
    Ok(v)
}

pub fn probe(b: &[u8]) -> Option<(u32, u32)> {
    let e = entries(b).ok()?;
    let best = e.iter().max_by_key(|e| (e.w * e.h, e.bpp))?;
    Some((best.w, best.h))
}

pub fn decode(b: &[u8]) -> Result<Decoded> {
    let list = entries(b)?;
    let count = list.len();
    let kind = if b[2] == 2 { "CUR" } else { "ICO" };
    // Try entries from largest to smallest until one decodes.
    let mut order: Vec<&Entry> = list.iter().collect();
    order.sort_by_key(|e| std::cmp::Reverse((e.w * e.h, e.bpp)));
    let mut last_err = anyhow!("Icon file has no readable images");
    for e in order {
        let data = &b[e.off..e.off + e.len];
        let res = if data.starts_with(b"\x89PNG") {
            image::load_from_memory_with_format(data, image::ImageFormat::Png).map_err(anyhow::Error::from)
        } else {
            // Wrap the single BMP entry in a one-image ICO for the image crate.
            let mut ico = Vec::with_capacity(22 + data.len());
            ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
            ico.push(if e.w >= 256 { 0 } else { e.w as u8 });
            ico.push(if e.h >= 256 { 0 } else { e.h as u8 });
            ico.extend_from_slice(&[0, 0, 1, 0]);
            ico.extend_from_slice(&e.bpp.max(1).min(32).to_le_bytes());
            ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
            ico.extend_from_slice(&22u32.to_le_bytes());
            ico.extend_from_slice(data);
            image::load_from_memory_with_format(&ico, image::ImageFormat::Ico).map_err(anyhow::Error::from)
        };
        match res {
            Ok(img) => {
                let rgba = img.into_rgba8();
                let mut d = Decoded::from_rgba(rgba, kind);
                d.decoder = "Native";
                d.color_info = if e.bpp > 0 { format!("{} bpp", e.bpp) } else { "RGBA 8-bit".into() };
                d.frame_count = count;
                if count > 1 {
                    d.format = format!("{kind} ({count} sizes)");
                }
                return Ok(d);
            }
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(c));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    pub fn build(kind: u8, imgs: &[(u32, u32, u16, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0, 0, kind, 0];
        v.extend_from_slice(&(imgs.len() as u16).to_le_bytes());
        let mut off = 6 + imgs.len() * 16;
        for (w, h, bpp, data) in imgs {
            v.push(if *w >= 256 { 0 } else { *w as u8 });
            v.push(if *h >= 256 { 0 } else { *h as u8 });
            v.extend_from_slice(&[0, 0, 1, 0]);
            v.extend_from_slice(&bpp.to_le_bytes());
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(&(off as u32).to_le_bytes());
            off += data.len();
        }
        for (_, _, _, d) in imgs {
            v.extend_from_slice(d);
        }
        v
    }

    #[test]
    fn picks_largest_even_if_shallower() {
        let f = build(1, &[(16, 16, 32, png(16, 16, [255, 0, 0, 255])), (48, 48, 8, png(48, 48, [0, 255, 0, 255]))]);
        let d = decode(&f).unwrap();
        assert_eq!(d.image.dimensions(), (48, 48));
        assert_eq!(d.frame_count, 2);
        assert_eq!(probe(&f), Some((48, 48)));
    }

    #[test]
    fn cursor_files_decode() {
        let f = build(2, &[(32, 32, 7, png(32, 32, [0, 0, 255, 128]))]);
        let d = decode(&f).unwrap();
        assert_eq!(d.format, "CUR");
        assert_eq!(d.image.get_pixel(0, 0).0, [0, 0, 255, 128]);
        assert!(d.has_alpha);
    }

    #[test]
    fn corrupt_entries() {
        let mut f = build(1, &[(16, 16, 32, png(16, 16, [1, 2, 3, 255]))]);
        f.truncate(40);
        assert!(decode(&f).is_err());
        assert!(decode(&[0, 0, 1, 0, 0, 0]).is_err());
    }
}
