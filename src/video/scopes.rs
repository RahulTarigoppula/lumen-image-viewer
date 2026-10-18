//! Video scopes computed on the CPU from the displayed RGBA frame: luma
//! waveform, RGB parade, vectorscope and histogram (as images for display).

use image::RgbaImage;
use rayon::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Waveform,
    Parade,
    Vectorscope,
    Histogram,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::Waveform, Scope::Parade, Scope::Vectorscope, Scope::Histogram];
    pub fn label(self) -> &'static str {
        match self {
            Scope::Waveform => "Waveform",
            Scope::Parade => "RGB parade",
            Scope::Vectorscope => "Vectorscope",
            Scope::Histogram => "Histogram",
        }
    }
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 54 + g as u32 * 183 + b as u32 * 19) >> 8) as u8
}

/// Downsample columns so scopes stay cheap on 4K frames.
fn sample_grid(w: u32, h: u32) -> (u32, u32) {
    let sx = (w / 512).max(1);
    let sy = (h / 288).max(1);
    (sx, sy)
}

fn intensity(count: u32, scale: f32) -> u8 {
    if count == 0 {
        0
    } else {
        ((count as f32 * scale).sqrt().min(1.0) * 255.0) as u8
    }
}

pub fn render(scope: Scope, frame: &RgbaImage) -> RgbaImage {
    match scope {
        Scope::Waveform => waveform(frame, false),
        Scope::Parade => waveform(frame, true),
        Scope::Vectorscope => vectorscope(frame),
        Scope::Histogram => histogram(frame),
    }
}

fn waveform(frame: &RgbaImage, parade: bool) -> RgbaImage {
    let (w, h) = frame.dimensions();
    let (sx, sy) = sample_grid(w, h);
    let cols = (w / sx).max(1) as usize;
    const LEVELS: usize = 256;
    let channels = if parade { 3 } else { 1 };
    // bins[ch][col][level]
    let bins: Vec<Vec<u32>> = (0..channels)
        .into_par_iter()
        .map(|ch| {
            let mut b = vec![0u32; cols * LEVELS];
            let raw = frame.as_raw();
            let mut y = 0;
            while y < h {
                for c in 0..cols {
                    let x = c as u32 * sx;
                    let i = ((y * w + x) * 4) as usize;
                    let v = if parade { raw[i + ch] } else { luma(raw[i], raw[i + 1], raw[i + 2]) };
                    b[c * LEVELS + v as usize] += 1;
                }
                y += sy;
            }
            b
        })
        .collect();
    let out_w = (cols * channels) as u32;
    let mut out = RgbaImage::from_pixel(out_w, LEVELS as u32, image::Rgba([0, 0, 0, 255]));
    let rows = (h / sy).max(1) as f32;
    let scale = 12.0 / rows;
    for (ch, b) in bins.iter().enumerate() {
        let tint: [u8; 3] = if !parade {
            [200, 255, 200]
        } else {
            [[255, 70, 70], [70, 255, 110], [90, 140, 255]][ch]
        };
        for c in 0..cols {
            for lv in 0..LEVELS {
                let k = intensity(b[c * LEVELS + lv], scale);
                if k > 0 {
                    let px = out.get_pixel_mut((ch * cols + c) as u32, (LEVELS - 1 - lv) as u32);
                    px.0 = [
                        (tint[0] as u32 * k as u32 / 255) as u8,
                        (tint[1] as u32 * k as u32 / 255) as u8,
                        (tint[2] as u32 * k as u32 / 255) as u8,
                        255,
                    ];
                }
            }
        }
    }
    // Graticule lines at 0/25/50/75/100%.
    for pct in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
        let y = ((1.0 - pct) * (LEVELS - 1) as f32) as u32;
        for x in (0..out_w).step_by(3) {
            let p = out.get_pixel_mut(x, y);
            p.0[0] = p.0[0].max(70);
            p.0[1] = p.0[1].max(70);
            p.0[2] = p.0[2].max(70);
        }
    }
    out
}

fn vectorscope(frame: &RgbaImage) -> RgbaImage {
    const S: usize = 256;
    let (w, h) = frame.dimensions();
    let (sx, sy) = sample_grid(w, h);
    let mut bins = vec![0u32; S * S];
    let raw = frame.as_raw();
    let mut y = 0;
    let mut n = 0u32;
    while y < h {
        let mut x = 0;
        while x < w {
            let i = ((y * w + x) * 4) as usize;
            let (r, g, b) = (raw[i] as f32, raw[i + 1] as f32, raw[i + 2] as f32);
            // BT.709 Cb/Cr in [-128,128)
            let cb = -0.1146 * r - 0.3854 * g + 0.5 * b;
            let cr = 0.5 * r - 0.4542 * g - 0.0458 * b;
            let u = ((cb + 128.0).clamp(0.0, 255.0)) as usize;
            let v = ((128.0 - cr).clamp(0.0, 255.0)) as usize;
            bins[v * S + u] += 1;
            n += 1;
            x += sx;
        }
        y += sy;
    }
    let mut out = RgbaImage::from_pixel(S as u32, S as u32, image::Rgba([0, 0, 0, 255]));
    let scale = 400.0 / n.max(1) as f32;
    for v in 0..S {
        for u in 0..S {
            let k = intensity(bins[v * S + u], scale);
            if k > 0 {
                // Color each point by its hue for readability.
                let cb = u as f32 - 128.0;
                let cr = 128.0 - v as f32;
                let r = (128.0 + 1.5748 * cr).clamp(40.0, 255.0);
                let g = (128.0 - 0.1873 * cb - 0.4681 * cr).clamp(40.0, 255.0);
                let b = (128.0 + 1.8556 * cb).clamp(40.0, 255.0);
                out.put_pixel(u as u32, v as u32, image::Rgba([(r * k as f32 / 255.0) as u8, (g * k as f32 / 255.0) as u8, (b * k as f32 / 255.0) as u8, 255]));
            }
        }
    }
    // Graticule: outer circle, center cross, skin-tone line (~123°).
    let c = S as f32 / 2.0;
    for t in 0..720 {
        let a = t as f32 / 720.0 * std::f32::consts::TAU;
        let (x, y) = (c + a.cos() * (c - 2.0), c + a.sin() * (c - 2.0));
        let p = out.get_pixel_mut(x as u32, y as u32);
        p.0 = [80, 80, 80, 255];
    }
    for i in 0..S as u32 {
        out.get_pixel_mut(i, c as u32).0 = [50, 50, 50, 255];
        out.get_pixel_mut(c as u32, i).0 = [50, 50, 50, 255];
    }
    let skin = 123f32.to_radians();
    for r in 0..(c as i32 - 4) {
        let x = c + skin.cos() * r as f32;
        let y = c - skin.sin() * r as f32;
        out.get_pixel_mut(x as u32, y as u32).0 = [200, 150, 110, 255];
    }
    // 75% color targets (R, Mg, B, Cy, G, Yl)
    for (r, g, b) in [(191.0, 0.0, 0.0), (191.0, 0.0, 191.0), (0.0, 0.0, 191.0), (0.0, 191.0, 191.0), (0.0, 191.0, 0.0), (191.0, 191.0, 0.0)] {
        let cb: f32 = -0.1146 * r - 0.3854 * g + 0.5 * b;
        let cr: f32 = 0.5 * r - 0.4542 * g - 0.0458 * b;
        let (x, y) = ((cb + 128.0) as i32, (128.0 - cr) as i32);
        for dx in -3..=3 {
            for dy in -3..=3 {
                if dx == -3 || dx == 3 || dy == -3 || dy == 3 {
                    let (px, py) = ((x + dx).clamp(0, 255) as u32, (y + dy).clamp(0, 255) as u32);
                    out.get_pixel_mut(px, py).0 = [160, 160, 160, 255];
                }
            }
        }
    }
    out
}

fn histogram(frame: &RgbaImage) -> RgbaImage {
    let (w, h) = frame.dimensions();
    let (sx, sy) = sample_grid(w, h);
    let mut hist = [[0u32; 256]; 4];
    let raw = frame.as_raw();
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let i = ((y * w + x) * 4) as usize;
            hist[0][raw[i] as usize] += 1;
            hist[1][raw[i + 1] as usize] += 1;
            hist[2][raw[i + 2] as usize] += 1;
            hist[3][luma(raw[i], raw[i + 1], raw[i + 2]) as usize] += 1;
            x += sx;
        }
        y += sy;
    }
    let max = (0..4).flat_map(|c| hist[c][1..255].iter().copied()).max().unwrap_or(1).max(1) as f32;
    let (ow, oh) = (256u32, 140u32);
    let mut out = RgbaImage::from_pixel(ow, oh, image::Rgba([0, 0, 0, 255]));
    let colors = [[255u32, 70, 70], [70, 230, 110], [90, 140, 255], [220, 220, 220]];
    for x in 0..256usize {
        for (c, color) in colors.iter().enumerate() {
            let hh = ((hist[c][x] as f32 / max).sqrt().min(1.0) * (oh - 1) as f32) as u32;
            for yy in 0..hh {
                let p = out.get_pixel_mut(x as u32, oh - 1 - yy);
                for k in 0..3 {
                    p.0[k] = (p.0[k] as u32 + color[k] / 3).min(255) as u8;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scopes_render_sizes() {
        let img = RgbaImage::from_fn(640, 360, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]));
        for s in Scope::ALL {
            let out = render(s, &img);
            assert!(out.width() > 0 && out.height() > 0);
        }
        assert_eq!(render(Scope::Parade, &img).width() % 3, 0);
    }
}
