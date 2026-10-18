//! ZSoft PCX (PC Paintbrush): 1/2/4/8-bit palettes, 4-plane EGA and 24/32-bit RGB(A).

use super::{check_alloc, Decoded};
use anyhow::{bail, Result};
use image::RgbaImage;

pub fn is_pcx(b: &[u8]) -> bool {
    b.len() >= 128 && b[0] == 0x0A && b[1] <= 5 && b[2] <= 1 && matches!(b[3], 1 | 2 | 4 | 8)
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn probe(b: &[u8]) -> Option<(u32, u32)> {
    if !is_pcx(b) {
        return None;
    }
    let (x0, y0, x1, y1) = (le16(b, 4), le16(b, 6), le16(b, 8), le16(b, 10));
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some(((x1 - x0) as u32 + 1, (y1 - y0) as u32 + 1))
}

pub fn decode(b: &[u8]) -> Result<Decoded> {
    if !is_pcx(b) {
        bail!("Not a PCX file");
    }
    let Some((w, h)) = probe(b) else { bail!("Invalid PCX dimensions") };
    let bpp = b[3] as usize;
    let planes = b[65] as usize;
    let bpl = le16(b, 66) as usize;
    let rle = b[2] == 1;
    if planes == 0 || planes > 4 || bpl == 0 || bpl * 8 < w as usize * bpp {
        bail!("Unsupported or corrupt PCX header");
    }
    check_alloc(w, h)?;
    let line = bpl * planes;
    // Decode all scanlines (tolerating truncation: missing data stays black).
    let total = line * h as usize;
    let mut data = vec![0u8; total];
    let src = &b[128..];
    let (mut i, mut o) = (0usize, 0usize);
    if rle {
        while o < total && i < src.len() {
            let v = src[i];
            i += 1;
            if v >= 0xC0 {
                let n = (v & 0x3F) as usize;
                let Some(&val) = src.get(i) else { break };
                i += 1;
                let n = n.min(total - o);
                data[o..o + n].fill(val);
                o += n;
            } else {
                data[o] = v;
                o += 1;
            }
        }
    } else {
        let n = src.len().min(total);
        data[..n].copy_from_slice(&src[..n]);
        o = n;
        i = n;
    }
    if o < line {
        bail!("PCX file is truncated");
    }

    let header_pal: Vec<[u8; 3]> = (0..16).map(|k| [b[16 + k * 3], b[17 + k * 3], b[18 + k * 3]]).collect();
    // 256-color palette lives in the last 769 bytes (0x0C marker).
    let vga_pal: Option<Vec<[u8; 3]>> = if bpp == 8 && planes == 1 && b.len() >= 128 + 769 && b[b.len() - 769] == 0x0C {
        let p = &b[b.len() - 768..];
        Some((0..256).map(|k| [p[k * 3], p[k * 3 + 1], p[k * 3 + 2]]).collect())
    } else {
        None
    };
    let _ = i;

    let mut img = RgbaImage::new(w, h);
    let mut info = String::new();
    for y in 0..h as usize {
        let row = &data[y * line..(y + 1) * line];
        for x in 0..w as usize {
            let px = match (bpp, planes) {
                (8, 3) | (8, 4) => {
                    let a = if planes == 4 { row[3 * bpl + x] } else { 255 };
                    [row[x], row[bpl + x], row[2 * bpl + x], a]
                }
                (8, 1) => {
                    let idx = row[x] as usize;
                    match &vga_pal {
                        Some(p) => [p[idx][0], p[idx][1], p[idx][2], 255],
                        None => [idx as u8, idx as u8, idx as u8, 255],
                    }
                }
                (1, 1) => {
                    let bit = (row[x / 8] >> (7 - x % 8)) & 1;
                    let v = if bit == 1 { 255 } else { 0 };
                    [v, v, v, 255]
                }
                (1, p) => {
                    let mut idx = 0usize;
                    for k in 0..p {
                        idx |= (((row[k * bpl + x / 8] >> (7 - x % 8)) & 1) as usize) << k;
                    }
                    let c = header_pal[idx & 15];
                    [c[0], c[1], c[2], 255]
                }
                (2, 1) | (4, 1) => {
                    let per = 8 / bpp;
                    let byte = row[x / per];
                    let shift = 8 - bpp * (x % per + 1);
                    let idx = ((byte >> shift) as usize) & ((1 << bpp) - 1);
                    let c = header_pal[idx & 15];
                    [c[0], c[1], c[2], 255]
                }
                _ => bail!("Unsupported PCX layout ({bpp} bits x {planes} planes)"),
            };
            img.put_pixel(x as u32, y as u32, image::Rgba(px));
        }
    }
    if info.is_empty() {
        info = match (bpp, planes) {
            (8, 3) => "RGB 8-bit".into(),
            (8, 4) => "RGBA 8-bit".into(),
            (8, 1) if vga_pal.is_some() => "Indexed 8-bit".into(),
            (8, 1) => "Grayscale 8-bit".into(),
            (1, 1) => "Monochrome".into(),
            _ => format!("Indexed {}-bit", bpp * planes),
        };
    }
    let mut d = Decoded::from_rgba(img, "PCX");
    d.color_info = info;
    Ok(d)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn rle_line(out: &mut Vec<u8>, line: &[u8]) {
        let mut i = 0;
        while i < line.len() {
            let v = line[i];
            let mut n = 1;
            while i + n < line.len() && line[i + n] == v && n < 63 {
                n += 1;
            }
            if n > 1 || v >= 0xC0 {
                out.push(0xC0 | n as u8);
            }
            out.push(v);
            i += n;
        }
    }

    pub fn encode(w: u16, h: u16, bpp: u8, planes: u8, bpl: u16, rows: &[Vec<u8>], vga: Option<&[[u8; 3]; 256]>) -> Vec<u8> {
        let mut v = vec![0u8; 128];
        v[0] = 0x0A;
        v[1] = 5;
        v[2] = 1;
        v[3] = bpp;
        v[8..10].copy_from_slice(&(w - 1).to_le_bytes());
        v[10..12].copy_from_slice(&(h - 1).to_le_bytes());
        v[65] = planes;
        v[66..68].copy_from_slice(&bpl.to_le_bytes());
        for r in rows {
            rle_line(&mut v, r);
        }
        if let Some(p) = vga {
            v.push(0x0C);
            for c in p {
                v.extend_from_slice(c);
            }
        }
        v
    }

    #[test]
    fn rgb24_and_palette() {
        // 3x2 RGB, planar per line, bytes-per-line padded to 4.
        let rows: Vec<Vec<u8>> = (0..2u8)
            .map(|y| {
                let mut r = Vec::new();
                for c in 0..3u8 {
                    r.extend_from_slice(&[c * 50 + y, c * 50 + y, 200, 0]);
                }
                r
            })
            .collect();
        let f = encode(3, 2, 8, 3, 4, &rows, None);
        let d = decode(&f).unwrap();
        assert_eq!(d.image.dimensions(), (3, 2));
        assert_eq!(d.image.get_pixel(0, 1).0, [1, 51, 101, 255]);
        assert_eq!(d.image.get_pixel(2, 0).0, [200, 200, 200, 255]);

        let mut pal = [[0u8; 3]; 256];
        pal[7] = [10, 20, 30];
        let f = encode(2, 2, 8, 1, 2, &[vec![7, 7], vec![0, 7]], Some(&pal));
        let d = decode(&f).unwrap();
        assert_eq!(d.image.get_pixel(1, 1).0, [10, 20, 30, 255]);
        assert_eq!(d.image.get_pixel(0, 1).0, [0, 0, 0, 255]);
        assert!(probe(&f) == Some((2, 2)));
    }

    #[test]
    fn mono_and_corrupt() {
        let f = encode(8, 1, 1, 1, 2, &[vec![0b1010_0000, 0]], None);
        let d = decode(&f).unwrap();
        assert_eq!(d.image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(d.image.get_pixel(1, 0).0, [0, 0, 0, 255]);
        assert!(decode(&f[..100]).is_err());
        assert!(decode(&[]).is_err());
        let mut bad = f.clone();
        bad[66] = 0;
        bad[67] = 0;
        assert!(decode(&bad).is_err());
    }
}
