//! Shared helpers for the integration tests: deterministic synthetic images,
//! distortions, PGM loading and frame builders.
#![allow(dead_code)]

use lumen_analysis::frame::{FrameCtx, FrameView, OwnedFrame, OwnedPlane};
use lumen_analysis::job::CancelToken;
use lumen_analysis::metrics::{self, MetricOptions, MetricResult};

/// xorshift64* PRNG (deterministic, no dependency).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-12);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Textured gray test image (gradient, sinusoids, edges), values 0..255.
pub fn texture(w: usize, h: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut v = vec![0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64, y as f64);
            v[y * w + x] = 60.0 + 100.0 * fx / w as f64 + 30.0 * (fx / 4.0).sin() * (fy / 6.0).cos() + if (x / 16 + y / 16) % 2 == 0 { 20.0 } else { -20.0 };
        }
    }
    for _ in 0..(w * h / 2000).max(4) {
        let (x0, y0) = ((rng.uniform() * w as f64) as usize, (rng.uniform() * h as f64) as usize);
        let c = rng.uniform() * 255.0;
        for y in y0..(y0 + 7).min(h) {
            for x in x0..(x0 + 9).min(w) {
                v[y * w + x] = c;
            }
        }
    }
    v
}

pub fn quantize(v: &[f64]) -> Vec<u8> {
    v.iter().map(|p| p.round().clamp(0.0, 255.0) as u8).collect()
}

pub fn add_noise(v: &[f64], sigma: f64, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    v.iter().map(|p| p + sigma * rng.normal()).collect()
}

/// Separable Gaussian blur with clamped edges.
pub fn blur(v: &[f64], w: usize, h: usize, sigma: f64) -> Vec<f64> {
    let r = (3.0 * sigma).ceil() as i64;
    let k: Vec<f64> = (-r..=r).map(|i| (-0.5 * (i * i) as f64 / (sigma * sigma)).exp()).collect();
    let s: f64 = k.iter().sum();
    let k: Vec<f64> = k.iter().map(|x| x / s).collect();
    let mut t = vec![0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            t[y * w + x] = (-r..=r).map(|i| k[(i + r) as usize] * v[y * w + (x as i64 + i).clamp(0, w as i64 - 1) as usize]).sum();
        }
    }
    let mut o = vec![0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            o[y * w + x] = (-r..=r).map(|i| k[(i + r) as usize] * t[(y as i64 + i).clamp(0, h as i64 - 1) as usize * w + x]).sum();
        }
    }
    o
}

/// RGBA8 frame from three gray channels (alpha 255).
pub fn rgba(w: usize, h: usize, r: &[u8], g: &[u8], b: &[u8]) -> OwnedFrame {
    let mut d = Vec::with_capacity(w * h * 4);
    for i in 0..w * h {
        d.extend_from_slice(&[r[i], g[i], b[i], 255]);
    }
    OwnedFrame::rgba8(w, h, d)
}

pub fn gray_rgba(w: usize, h: usize, v: &[u8]) -> OwnedFrame {
    rgba(w, h, v, v, v)
}

/// Gray-only YUV frame (one 8-bit plane).
pub fn gray_yuv(w: usize, h: usize, v: Vec<u8>) -> OwnedFrame {
    OwnedFrame::yuv(vec![OwnedPlane::U8 { data: v, width: w, height: h }], 8)
}

/// 8-bit 4:2:0 frame: luma `y`, chroma planes made from `f(cx, cy)`.
pub fn yuv420(w: usize, h: usize, y: Vec<u8>, f: impl Fn(usize, usize) -> (u8, u8)) -> OwnedFrame {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
    for cy in 0..ch {
        for cx in 0..cw {
            let (a, b) = f(cx, cy);
            u.push(a);
            v.push(b);
        }
    }
    OwnedFrame::yuv(
        vec![
            OwnedPlane::U8 { data: y, width: w, height: h },
            OwnedPlane::U8 { data: u, width: cw, height: ch },
            OwnedPlane::U8 { data: v, width: cw, height: ch },
        ],
        8,
    )
}

/// Reads an 8-bit binary PGM (P5).
pub fn read_pgm(path: &std::path::Path) -> (usize, usize, Vec<u8>) {
    let d = std::fs::read(path).unwrap();
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while d[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !d[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&d[s..i]).to_string());
    }
    i += 1;
    assert_eq!(fields[0], "P5");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    (w, h, d[i..i + w * h].to_vec())
}

/// Runs one metric by id on views.
pub fn run(id: &str, a: &FrameView<'_>, b: Option<&FrameView<'_>>, o: &MetricOptions) -> MetricResult {
    let m = metrics::get(id).unwrap_or_else(|| panic!("no metric {id}"));
    let ca = FrameCtx::new(*a);
    let cb = b.map(|v| FrameCtx::new(*v));
    m.compute(&ca, cb.as_ref(), o, &CancelToken::new()).unwrap_or_else(|e| panic!("{id}: {e}"))
}

pub fn val(r: &MetricResult, key: &str) -> f64 {
    r.get(key).unwrap_or_else(|| panic!("{} has no value {key}: {:?}", r.id, r.values))
}
