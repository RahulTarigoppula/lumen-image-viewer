//! Borrowed frame views, regions of interest and the per-frame derived cache.
//!
//! Metrics never see egui or FFmpeg types. The app wraps its decoded pixels
//! (display RGBA8 for images, native YUV planes for video) in a [`FrameView`]
//! without copying; a [`FrameCtx`] adds lazily computed derived images (luma
//! `f32`, linear RGB, CIE Lab) that every metric in one run shares.

use crate::color;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Integer sample type of a plane (`u8` or `u16`). Kernels are generic over
/// it so the hot loops run on contiguous typed slices with no per-sample match.
pub trait Sample: Copy + Default + PartialEq + Send + Sync + Into<u32> + 'static {
    fn to_f32(self) -> f32;
    fn to_i32(self) -> i32;
}

impl Sample for u8 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
}

impl Sample for u16 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FrameError {
    /// The buffer is shorter than `stride * (height - 1) + width` samples.
    BufferTooSmall { needed: usize, got: usize },
    /// Width or height is zero, or the stride is below the row width.
    BadGeometry(String),
    /// Chroma planes do not match the luma plane by a power-of-two subsampling.
    BadSubsampling(String),
    /// The ROI does not intersect the frame.
    EmptyRoi,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BufferTooSmall { needed, got } => write!(f, "pixel buffer too small ({got} samples, need {needed})"),
            FrameError::BadGeometry(s) => write!(f, "bad frame geometry: {s}"),
            FrameError::BadSubsampling(s) => write!(f, "bad chroma subsampling: {s}"),
            FrameError::EmptyRoi => write!(f, "the region of interest is outside the frame"),
        }
    }
}

impl std::error::Error for FrameError {}

/// One plane of samples, borrowed. `stride` is in samples (not bytes).
#[derive(Clone, Copy, Debug)]
pub struct Plane<'a, T> {
    pub data: &'a [T],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl<'a, T: Copy> Plane<'a, T> {
    pub fn new(data: &'a [T], width: usize, height: usize, stride: usize) -> Result<Self, FrameError> {
        if width == 0 || height == 0 || stride < width {
            return Err(FrameError::BadGeometry(format!("{width}x{height} stride {stride}")));
        }
        let needed = stride * (height - 1) + width;
        if data.len() < needed {
            return Err(FrameError::BufferTooSmall { needed, got: data.len() });
        }
        Ok(Plane { data, width, height, stride })
    }

    /// A plane whose rows are packed back to back (`stride == width`).
    pub fn packed(data: &'a [T], width: usize, height: usize) -> Result<Self, FrameError> {
        Self::new(data, width, height, width)
    }

    #[inline(always)]
    pub fn row(&self, y: usize) -> &'a [T] {
        let s = y * self.stride;
        &self.data[s..s + self.width]
    }

    /// Strided sub-view, no copy. The rectangle must lie inside the plane.
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Self {
        assert!(x + w <= self.width && y + h <= self.height && w > 0 && h > 0, "crop outside plane");
        let start = y * self.stride + x;
        let end = (y + h - 1) * self.stride + x + w;
        Plane { data: &self.data[start..end], width: w, height: h, stride: self.stride }
    }

    pub fn len(&self) -> usize {
        self.width * self.height
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A plane of `u8` or `u16` samples. Dispatch on the variant happens once per
/// plane, never per sample.
#[derive(Clone, Copy, Debug)]
pub enum PlaneView<'a> {
    U8(Plane<'a, u8>),
    U16(Plane<'a, u16>),
}

impl<'a> PlaneView<'a> {
    pub fn width(&self) -> usize {
        match self {
            PlaneView::U8(p) => p.width,
            PlaneView::U16(p) => p.width,
        }
    }

    pub fn height(&self) -> usize {
        match self {
            PlaneView::U8(p) => p.height,
            PlaneView::U16(p) => p.height,
        }
    }

    pub fn len(&self) -> usize {
        self.width() * self.height()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_u16(&self) -> bool {
        matches!(self, PlaneView::U16(_))
    }

    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Self {
        match self {
            PlaneView::U8(p) => PlaneView::U8(p.crop(x, y, w, h)),
            PlaneView::U16(p) => PlaneView::U16(p.crop(x, y, w, h)),
        }
    }

    /// Samples as packed `f32` multiplied by `scale` (e.g. `1 / max` to normalize).
    pub fn to_f32(&self, scale: f32) -> Vec<f32> {
        fn conv<T: Sample>(p: &Plane<'_, T>, scale: f32) -> Vec<f32> {
            let mut out = vec![0f32; p.len()];
            out.par_chunks_mut(p.width).enumerate().for_each(|(y, o)| {
                for (d, s) in o.iter_mut().zip(p.row(y)) {
                    *d = s.to_f32() * scale;
                }
            });
            out
        }
        match self {
            PlaneView::U8(p) => conv(p, scale),
            PlaneView::U16(p) => conv(p, scale),
        }
    }

    /// Row `y` as `f32` times `scale`, written into `out[..width]`.
    pub fn row_f32(&self, y: usize, scale: f32, out: &mut [f32]) {
        match self {
            PlaneView::U8(p) => {
                for (d, s) in out.iter_mut().zip(p.row(y)) {
                    *d = *s as f32 * scale;
                }
            }
            PlaneView::U16(p) => {
                for (d, s) in out.iter_mut().zip(p.row(y)) {
                    *d = *s as f32 * scale;
                }
            }
        }
    }
}

/// YCbCr matrix coefficients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Matrix {
    Bt601,
    #[default]
    Bt709,
    Bt2020,
}

impl Matrix {
    /// (Kr, Kb).
    pub fn kr_kb(self) -> (f32, f32) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// Code value range of YUV samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Range {
    /// Y 16..235, C 16..240 (scaled by bit depth).
    #[default]
    Limited,
    Full,
}

/// Transfer function of the encoded values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Transfer {
    /// sRGB / SDR display gamma (also used for BT.709/BT.1886 video).
    #[default]
    Srgb,
    /// Already linear light.
    Linear,
    /// SMPTE ST 2084; linear 1.0 = 203 nits (reference white).
    Pq,
    /// ARIB STD-B67; linear 1.0 = HLG reference white (75% signal), no OOTF.
    Hlg,
}

/// Chroma layout, for display and for ROI snapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Subsampling {
    Gray,
    S444,
    S422,
    S420,
    S440,
    S411,
    S410,
    Other(u8, u8),
}

impl Subsampling {
    pub fn from_shift(sx: u32, sy: u32) -> Self {
        match (sx, sy) {
            (0, 0) => Subsampling::S444,
            (1, 0) => Subsampling::S422,
            (1, 1) => Subsampling::S420,
            (0, 1) => Subsampling::S440,
            (2, 0) => Subsampling::S411,
            (2, 1) => Subsampling::S410,
            (a, b) => Subsampling::Other(a as u8, b as u8),
        }
    }
}

/// Planar YUV (or gray) frame at native resolution.
#[derive(Clone, Copy, Debug)]
pub struct YuvView<'a> {
    pub y: PlaneView<'a>,
    /// Cb and Cr; `None` for gray.
    pub uv: Option<(PlaneView<'a>, PlaneView<'a>)>,
    /// Bits per sample (8, 10, 12, 16). Samples of 9 bits and more are `u16`.
    pub bit_depth: u32,
    pub matrix: Matrix,
    pub range: Range,
    pub transfer: Transfer,
    /// log2 chroma subsampling (x, y).
    shift: (u32, u32),
}

impl<'a> YuvView<'a> {
    /// Checks that all planes share a sample type and that chroma is the luma
    /// size divided (rounding up) by 1, 2 or 4 in each direction.
    pub fn new(y: PlaneView<'a>, uv: Option<(PlaneView<'a>, PlaneView<'a>)>, bit_depth: u32) -> Result<Self, FrameError> {
        if !(1..=16).contains(&bit_depth) || (bit_depth > 8 && !y.is_u16()) {
            return Err(FrameError::BadGeometry(format!("bit depth {bit_depth} does not fit the sample type")));
        }
        let mut shift = (0, 0);
        if let Some((u, v)) = &uv {
            if u.is_u16() != y.is_u16() || v.is_u16() != y.is_u16() {
                return Err(FrameError::BadSubsampling("planes have different sample types".into()));
            }
            if u.width() != v.width() || u.height() != v.height() {
                return Err(FrameError::BadSubsampling("Cb and Cr differ in size".into()));
            }
            let find = |full: usize, sub: usize| (0..=2u32).find(|s| full.div_ceil(1 << s) == sub);
            match (find(y.width(), u.width()), find(y.height(), u.height())) {
                (Some(sx), Some(sy)) => shift = (sx, sy),
                _ => {
                    return Err(FrameError::BadSubsampling(format!(
                        "chroma {}x{} for luma {}x{}",
                        u.width(),
                        u.height(),
                        y.width(),
                        y.height()
                    )))
                }
            }
        }
        Ok(YuvView { y, uv, bit_depth, matrix: Matrix::default(), range: Range::default(), transfer: Transfer::default(), shift })
    }

    pub fn with_color(mut self, matrix: Matrix, range: Range, transfer: Transfer) -> Self {
        self.matrix = matrix;
        self.range = range;
        self.transfer = transfer;
        self
    }

    pub fn chroma_shift(&self) -> (u32, u32) {
        self.shift
    }

    pub fn subsampling(&self) -> Subsampling {
        if self.uv.is_none() {
            Subsampling::Gray
        } else {
            Subsampling::from_shift(self.shift.0, self.shift.1)
        }
    }

    pub fn max_value(&self) -> u32 {
        ((1u64 << self.bit_depth) - 1) as u32
    }

    /// Planes in Y, Cb, Cr order.
    pub fn planes(&self) -> Vec<PlaneView<'a>> {
        let mut v = vec![self.y];
        if let Some((u, w)) = self.uv {
            v.push(u);
            v.push(w);
        }
        v
    }
}

/// Interleaved RGB(A) samples.
#[derive(Clone, Copy, Debug)]
pub enum RgbData<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    /// Nominal range 0..1 (values outside are kept for linear/HDR data).
    F32(&'a [f32]),
}

/// Interleaved RGB or RGBA frame. Alpha, when present, is ignored.
#[derive(Clone, Copy, Debug)]
pub struct RgbView<'a> {
    pub data: RgbData<'a>,
    pub width: usize,
    pub height: usize,
    /// Row pitch in samples (not pixels, not bytes).
    pub stride: usize,
    /// 3 (RGB) or 4 (RGBA).
    pub channels: usize,
    pub transfer: Transfer,
}

impl<'a> RgbView<'a> {
    pub fn new(data: RgbData<'a>, width: usize, height: usize, stride: usize, channels: usize) -> Result<Self, FrameError> {
        if width == 0 || height == 0 || !(channels == 3 || channels == 4) || stride < width * channels {
            return Err(FrameError::BadGeometry(format!("{width}x{height}x{channels} stride {stride}")));
        }
        let len = match data {
            RgbData::U8(d) => d.len(),
            RgbData::U16(d) => d.len(),
            RgbData::F32(d) => d.len(),
        };
        let needed = stride * (height - 1) + width * channels;
        if len < needed {
            return Err(FrameError::BufferTooSmall { needed, got: len });
        }
        Ok(RgbView { data, width, height, stride, channels, transfer: Transfer::Srgb })
    }

    /// Packed 8-bit RGBA (what `image::RgbaImage` holds), sRGB.
    pub fn rgba8(data: &'a [u8], width: usize, height: usize) -> Result<Self, FrameError> {
        Self::new(RgbData::U8(data), width, height, width * 4, 4)
    }

    /// Packed 8-bit RGB, sRGB.
    pub fn rgb8(data: &'a [u8], width: usize, height: usize) -> Result<Self, FrameError> {
        Self::new(RgbData::U8(data), width, height, width * 3, 3)
    }

    pub fn with_transfer(mut self, t: Transfer) -> Self {
        self.transfer = t;
        self
    }

    fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Self {
        let c = self.channels;
        let start = y * self.stride + x * c;
        let end = (y + h - 1) * self.stride + (x + w) * c;
        let data = match self.data {
            RgbData::U8(d) => RgbData::U8(&d[start..end]),
            RgbData::U16(d) => RgbData::U16(&d[start..end]),
            RgbData::F32(d) => RgbData::F32(&d[start..end]),
        };
        RgbView { data, width: w, height: h, ..*self }
    }

    /// Full-scale code value of one channel (255, 65535, or 1 for float).
    pub fn max_value(&self) -> f32 {
        match self.data {
            RgbData::U8(_) => 255.0,
            RgbData::U16(_) => 65535.0,
            RgbData::F32(_) => 1.0,
        }
    }
}

/// A frame for analysis: native YUV planes or interleaved RGB.
#[derive(Clone, Copy, Debug)]
pub enum FrameView<'a> {
    Yuv(YuvView<'a>),
    Rgb(RgbView<'a>),
}

impl<'a> FrameView<'a> {
    pub fn width(&self) -> usize {
        match self {
            FrameView::Yuv(v) => v.y.width(),
            FrameView::Rgb(v) => v.width,
        }
    }

    pub fn height(&self) -> usize {
        match self {
            FrameView::Yuv(v) => v.y.height(),
            FrameView::Rgb(v) => v.height,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width(), self.height())
    }

    /// Native sample levels of the luma/first channel (255 for 8-bit).
    pub fn levels(&self) -> f32 {
        match self {
            FrameView::Yuv(v) => v.max_value() as f32,
            FrameView::Rgb(v) => match v.data {
                RgbData::F32(_) => 255.0,
                _ => v.max_value(),
            },
        }
    }

    /// Bytes held by the pixel data this view covers (for memory budgets).
    pub fn byte_size(&self) -> usize {
        match self {
            FrameView::Yuv(v) => {
                let bps = if v.y.is_u16() { 2 } else { 1 };
                v.planes().iter().map(|p| p.len() * bps).sum()
            }
            FrameView::Rgb(v) => {
                let bps = match v.data {
                    RgbData::U8(_) => 1,
                    RgbData::U16(_) => 2,
                    RgbData::F32(_) => 4,
                };
                v.width * v.height * v.channels * bps
            }
        }
    }

    /// True when both views have the same kind, size, sample type and chroma layout,
    /// so native-plane metrics can compare them.
    pub fn same_layout(&self, other: &FrameView<'_>) -> bool {
        match (self, other) {
            (FrameView::Yuv(a), FrameView::Yuv(b)) => {
                a.bit_depth == b.bit_depth
                    && a.y.is_u16() == b.y.is_u16()
                    && a.uv.is_some() == b.uv.is_some()
                    && a.planes().iter().zip(b.planes()).all(|(p, q)| p.width() == q.width() && p.height() == q.height())
            }
            (FrameView::Rgb(a), FrameView::Rgb(b)) => {
                a.width == b.width
                    && a.height == b.height
                    && std::mem::discriminant(&a.data) == std::mem::discriminant(&b.data)
            }
            _ => false,
        }
    }

    /// Strided sub-view for `roi` (no copy). For subsampled YUV the ROI is
    /// snapped outward to the chroma grid first; the returned ROI is the one
    /// actually used, in luma pixel coordinates.
    pub fn crop(&self, roi: Roi) -> Result<(FrameView<'a>, Roi), FrameError> {
        let (w, h) = self.size();
        let roi = roi.clamp(w as u32, h as u32).ok_or(FrameError::EmptyRoi)?;
        match self {
            FrameView::Rgb(v) => Ok((FrameView::Rgb(v.crop(roi.x as usize, roi.y as usize, roi.w as usize, roi.h as usize)), roi)),
            FrameView::Yuv(v) => {
                let (sx, sy) = v.shift;
                let r = roi.snap(sx, sy, w as u32, h as u32);
                let y = v.y.crop(r.x as usize, r.y as usize, r.w as usize, r.h as usize);
                let uv = v.uv.map(|(u, q)| {
                    let (cx, cy, cw, ch) = r.chroma(sx, sy, u.width() as u32, u.height() as u32);
                    let c = |p: PlaneView<'a>| p.crop(cx as usize, cy as usize, cw as usize, ch as usize);
                    (c(u), c(q))
                });
                Ok((FrameView::Yuv(YuvView { y, uv, ..*v }), r))
            }
        }
    }
}

/// Region of interest in luma / full-resolution pixel coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Roi {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Roi { x, y, w, h }
    }

    pub fn full(w: u32, h: u32) -> Self {
        Roi { x: 0, y: 0, w, h }
    }

    /// Intersection with a `w` x `h` frame; `None` when empty.
    pub fn clamp(self, w: u32, h: u32) -> Option<Roi> {
        let x0 = self.x.min(w);
        let y0 = self.y.min(h);
        let x1 = self.x.saturating_add(self.w).min(w);
        let y1 = self.y.saturating_add(self.h).min(h);
        (x1 > x0 && y1 > y0).then(|| Roi { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }

    /// Grows the ROI outward to multiples of `2^sx` x `2^sy` (an edge at the
    /// frame border stays at the border even when the frame size is odd).
    pub fn snap(self, sx: u32, sy: u32, w: u32, h: u32) -> Roi {
        let down = |v: u32, s: u32| v >> s << s;
        let up = |v: u32, s: u32, lim: u32| (v.div_ceil(1 << s) << s).min(lim);
        let x0 = down(self.x, sx);
        let y0 = down(self.y, sy);
        let x1 = up(self.x + self.w, sx, w);
        let y1 = up(self.y + self.h, sy, h);
        Roi { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// Chroma rectangle (x, y, w, h) of a snapped ROI, limited to the chroma plane size.
    pub fn chroma(self, sx: u32, sy: u32, cw: u32, ch: u32) -> (u32, u32, u32, u32) {
        let x0 = self.x >> sx;
        let y0 = self.y >> sy;
        let x1 = (self.x + self.w).div_ceil(1 << sx).min(cw);
        let y1 = (self.y + self.h).div_ceil(1 << sy).min(ch);
        (x0, y0, x1 - x0, y1 - y0)
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// Packed single-channel `f32` image.
#[derive(Clone, Debug, Default)]
pub struct PlaneF32 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
    /// Native code levels the normalized values came from (255 for 8-bit),
    /// so `v * levels` recovers code values.
    pub levels: f32,
}

impl PlaneF32 {
    #[inline(always)]
    pub fn row(&self, y: usize) -> &[f32] {
        &self.data[y * self.width..(y + 1) * self.width]
    }
}

/// CIE L*a*b* (D65) in structure-of-arrays layout.
#[derive(Clone, Debug, Default)]
pub struct LabImage {
    pub width: usize,
    pub height: usize,
    pub l: Vec<f32>,
    pub a: Vec<f32>,
    pub b: Vec<f32>,
}

/// Linear-light RGB, interleaved, nominal white 1.0.
#[derive(Clone, Debug, Default)]
pub struct RgbF32 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 3]>,
}

/// Derived inputs a metric can ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InputKind {
    /// The view itself (typed planes / channels).
    NativePlanes,
    /// Luma `f32` normalized to 0..1 (YUV: Y code value / max; RGB: BT.709 Y').
    Luma,
    /// Linear-light RGB.
    LinearRgb,
    /// CIE Lab D65.
    Lab,
}

/// A frame plus its lazily computed derived images, shared by all metrics in a run.
pub struct FrameCtx<'a> {
    view: FrameView<'a>,
    luma: OnceLock<PlaneF32>,
    lab: OnceLock<LabImage>,
    linear: OnceLock<RgbF32>,
}

impl<'a> FrameCtx<'a> {
    pub fn new(view: FrameView<'a>) -> Self {
        FrameCtx { view, luma: OnceLock::new(), lab: OnceLock::new(), linear: OnceLock::new() }
    }

    pub fn view(&self) -> &FrameView<'a> {
        &self.view
    }

    pub fn width(&self) -> usize {
        self.view.width()
    }

    pub fn height(&self) -> usize {
        self.view.height()
    }

    /// Computes the derived inputs in `kinds` now. Runners call this before
    /// running metrics in parallel, so no rayon worker ever blocks inside a
    /// lazy initializer that another worker is computing.
    pub fn prepare(&self, kinds: &[InputKind]) {
        for k in kinds {
            match k {
                InputKind::NativePlanes => {}
                InputKind::Luma => {
                    self.luma();
                }
                InputKind::LinearRgb => {
                    self.linear();
                }
                InputKind::Lab => {
                    self.lab();
                }
            }
        }
    }

    pub fn luma(&self) -> &PlaneF32 {
        self.luma.get_or_init(|| compute_luma(&self.view))
    }

    pub fn lab(&self) -> &LabImage {
        self.lab.get_or_init(|| compute_lab(&self.view))
    }

    pub fn linear(&self) -> &RgbF32 {
        self.linear.get_or_init(|| compute_linear(&self.view))
    }

    /// Bytes a fully populated derived cache needs for a `w` x `h` frame.
    pub fn derived_bytes(w: usize, h: usize, kinds: &[InputKind]) -> usize {
        let px = w * h;
        kinds
            .iter()
            .map(|k| match k {
                InputKind::NativePlanes => 0,
                InputKind::Luma => px * 4,
                InputKind::LinearRgb | InputKind::Lab => px * 12,
            })
            .sum()
    }
}

/// Converts rows of any [`FrameView`] to encoded (non-linear) RGB `f32` 0..1.
/// Keeps its scratch buffers between rows; create one per worker.
pub struct RowConverter {
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
    cb: Vec<f32>,
    cr: Vec<f32>,
}

impl RowConverter {
    pub fn new(width: usize) -> Self {
        RowConverter { r: vec![0.0; width], g: vec![0.0; width], b: vec![0.0; width], cb: vec![0.0; width], cr: vec![0.0; width] }
    }

    /// Fills `self.r/g/b[..width]` with row `y`, clamped to 0..1 for integer
    /// sources (float RGB is passed through).
    pub fn encoded(&mut self, view: &FrameView<'_>, y: usize) {
        let w = view.width();
        match view {
            FrameView::Rgb(v) => match v.data {
                RgbData::U8(d) => deinterleave(&d[y * v.stride..], v.channels, w, 1.0 / 255.0, &mut self.r, &mut self.g, &mut self.b),
                RgbData::U16(d) => deinterleave(&d[y * v.stride..], v.channels, w, 1.0 / 65535.0, &mut self.r, &mut self.g, &mut self.b),
                RgbData::F32(d) => deinterleave(&d[y * v.stride..], v.channels, w, 1.0, &mut self.r, &mut self.g, &mut self.b),
            },
            FrameView::Yuv(v) => {
                let depth_scale = (1u32 << v.bit_depth.saturating_sub(8)) as f32;
                let max = v.max_value() as f32;
                let (y_off, y_scale, c_scale) = match v.range {
                    Range::Limited => (16.0 * depth_scale, 1.0 / (219.0 * depth_scale), 1.0 / (224.0 * depth_scale)),
                    Range::Full => (0.0, 1.0 / max, 1.0 / max),
                };
                let mid = (1u32 << (v.bit_depth - 1)) as f32;
                // Luma into r (temporarily), chroma upsampled by replication.
                v.y.row_f32(y, 1.0, &mut self.r);
                for l in &mut self.r[..w] {
                    *l = (*l - y_off) * y_scale;
                }
                match v.uv {
                    None => {
                        let (r, rest) = (&self.r, (&mut self.g, &mut self.b));
                        rest.0[..w].copy_from_slice(&r[..w]);
                        rest.1[..w].copy_from_slice(&r[..w]);
                        for c in [&mut self.r, &mut self.g, &mut self.b] {
                            for x in &mut c[..w] {
                                *x = x.clamp(0.0, 1.0);
                            }
                        }
                    }
                    Some((u, q)) => {
                        let (sx, sy) = v.shift;
                        let cy = y >> sy;
                        upsample_row(&u, cy, sx, w, mid, c_scale, &mut self.cb, &mut self.g);
                        upsample_row(&q, cy, sx, w, mid, c_scale, &mut self.cr, &mut self.g);
                        let (kr, kb) = v.matrix.kr_kb();
                        let kg = 1.0 - kr - kb;
                        let (cr_r, cb_b) = (2.0 * (1.0 - kr), 2.0 * (1.0 - kb));
                        let (cb_g, cr_g) = (cb_b * kb / kg, cr_r * kr / kg);
                        for x in 0..w {
                            let l = self.r[x];
                            let cb = self.cb[x];
                            let cr = self.cr[x];
                            self.r[x] = (l + cr_r * cr).clamp(0.0, 1.0);
                            self.g[x] = (l - cb_g * cb - cr_g * cr).clamp(0.0, 1.0);
                            self.b[x] = (l + cb_b * cb).clamp(0.0, 1.0);
                        }
                    }
                }
            }
        }
    }

    /// Fills `self.r/g/b` with linear-light RGB for row `y`.
    pub fn linear(&mut self, view: &FrameView<'_>, y: usize) {
        let w = view.width();
        let transfer = match view {
            FrameView::Rgb(v) => {
                if let (RgbData::U8(d), Transfer::Srgb) = (v.data, v.transfer) {
                    // Exact 256-entry LUT straight from the bytes.
                    let lut = color::srgb_u8_lut();
                    let row = &d[y * v.stride..];
                    let c = v.channels;
                    for x in 0..w {
                        self.r[x] = lut[row[x * c] as usize];
                        self.g[x] = lut[row[x * c + 1] as usize];
                        self.b[x] = lut[row[x * c + 2] as usize];
                    }
                    return;
                }
                v.transfer
            }
            FrameView::Yuv(v) => v.transfer,
        };
        self.encoded(view, y);
        for c in [&mut self.r, &mut self.g, &mut self.b] {
            color::eotf_in_place(&mut c[..w], transfer);
        }
    }
}

fn deinterleave<T: Copy + ToF32>(row: &[T], ch: usize, w: usize, scale: f32, r: &mut [f32], g: &mut [f32], b: &mut [f32]) {
    let row = &row[..w * ch];
    if ch == 4 {
        for (((px, r), g), b) in row.as_chunks::<4>().0.iter().zip(r.iter_mut()).zip(g.iter_mut()).zip(b.iter_mut()) {
            *r = px[0].f() * scale;
            *g = px[1].f() * scale;
            *b = px[2].f() * scale;
        }
    } else {
        for (((px, r), g), b) in row.as_chunks::<3>().0.iter().zip(r.iter_mut()).zip(g.iter_mut()).zip(b.iter_mut()) {
            *r = px[0].f() * scale;
            *g = px[1].f() * scale;
            *b = px[2].f() * scale;
        }
    }
}

/// Lossless-enough conversion of a sample to `f32`.
pub trait ToF32 {
    fn f(self) -> f32;
}
impl ToF32 for u8 {
    #[inline(always)]
    fn f(self) -> f32 {
        self as f32
    }
}
impl ToF32 for u16 {
    #[inline(always)]
    fn f(self) -> f32 {
        self as f32
    }
}
impl ToF32 for f32 {
    #[inline(always)]
    fn f(self) -> f32 {
        self
    }
}

/// Chroma row `cy` centered and scaled, replicated horizontally by `2^sx`.
#[allow(clippy::too_many_arguments)]
fn upsample_row(p: &PlaneView<'_>, cy: usize, sx: u32, w: usize, mid: f32, scale: f32, out: &mut [f32], tmp: &mut [f32]) {
    let cw = p.width();
    p.row_f32(cy, 1.0, tmp);
    if sx == 0 {
        for x in 0..w {
            out[x] = (tmp[x] - mid) * scale;
        }
    } else {
        for x in 0..w {
            out[x] = (tmp[(x >> sx).min(cw - 1)] - mid) * scale;
        }
    }
}

fn compute_luma(view: &FrameView<'_>) -> PlaneF32 {
    let (w, h) = view.size();
    match view {
        FrameView::Yuv(v) => PlaneF32 { width: w, height: h, data: v.y.to_f32(1.0 / v.max_value() as f32), levels: v.max_value() as f32 },
        FrameView::Rgb(_) => {
            let mut data = vec![0f32; w * h];
            data.par_chunks_mut(w).enumerate().for_each_init(
                || RowConverter::new(w),
                |conv, (y, out)| {
                    conv.encoded(view, y);
                    let (kr, kb) = Matrix::Bt709.kr_kb();
                    let kg = 1.0 - kr - kb;
                    for x in 0..w {
                        out[x] = kr * conv.r[x] + kg * conv.g[x] + kb * conv.b[x];
                    }
                },
            );
            PlaneF32 { width: w, height: h, data, levels: view.levels() }
        }
    }
}

/// RGB to XYZ matrix for the view's primaries (BT.2020 when the YUV matrix
/// says so, otherwise BT.709/sRGB).
fn xyz_matrix(view: &FrameView<'_>) -> &'static [[f32; 3]; 3] {
    match view {
        FrameView::Yuv(v) if v.matrix == Matrix::Bt2020 => &color::BT2020_TO_XYZ,
        _ => &color::SRGB_TO_XYZ,
    }
}

fn compute_lab(view: &FrameView<'_>) -> LabImage {
    let (w, h) = view.size();
    let m = xyz_matrix(view);
    let mut l = vec![0f32; w * h];
    let mut a = vec![0f32; w * h];
    let mut b = vec![0f32; w * h];
    l.par_chunks_mut(w).zip(a.par_chunks_mut(w)).zip(b.par_chunks_mut(w)).enumerate().for_each_init(
        || RowConverter::new(w),
        |conv, (y, ((lo, ao), bo))| {
            conv.linear(view, y);
            color::linear_to_lab_row(m, &conv.r[..w], &conv.g[..w], &conv.b[..w], lo, ao, bo);
        },
    );
    LabImage { width: w, height: h, l, a, b }
}

fn compute_linear(view: &FrameView<'_>) -> RgbF32 {
    let (w, h) = view.size();
    let mut data = vec![[0f32; 3]; w * h];
    data.par_chunks_mut(w).enumerate().for_each_init(
        || RowConverter::new(w),
        |conv, (y, out)| {
            conv.linear(view, y);
            for x in 0..w {
                out[x] = [conv.r[x], conv.g[x], conv.b[x]];
            }
        },
    );
    RgbF32 { width: w, height: h, data }
}

/// Owned pixel storage, for jobs that outlive the caller's borrow and for
/// downsampled previews.
#[derive(Clone, Debug)]
pub enum OwnedFrame {
    Rgb { data: OwnedRgb, width: usize, height: usize, channels: usize, transfer: Transfer },
    Yuv { planes: Vec<OwnedPlane>, bit_depth: u32, matrix: Matrix, range: Range, transfer: Transfer },
}

#[derive(Clone, Debug)]
pub enum OwnedRgb {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

#[derive(Clone, Debug)]
pub enum OwnedPlane {
    U8 { data: Vec<u8>, width: usize, height: usize },
    U16 { data: Vec<u16>, width: usize, height: usize },
}

impl OwnedPlane {
    fn view(&self) -> PlaneView<'_> {
        match self {
            OwnedPlane::U8 { data, width, height } => PlaneView::U8(Plane { data, width: *width, height: *height, stride: *width }),
            OwnedPlane::U16 { data, width, height } => PlaneView::U16(Plane { data, width: *width, height: *height, stride: *width }),
        }
    }

    fn from_view(p: &PlaneView<'_>) -> Self {
        fn copy<T: Copy>(p: &Plane<'_, T>) -> Vec<T> {
            let mut v = Vec::with_capacity(p.len());
            for y in 0..p.height {
                v.extend_from_slice(p.row(y));
            }
            v
        }
        match p {
            PlaneView::U8(q) => OwnedPlane::U8 { data: copy(q), width: q.width, height: q.height },
            PlaneView::U16(q) => OwnedPlane::U16 { data: copy(q), width: q.width, height: q.height },
        }
    }

    /// 2x2 box average (odd edges replicate), rounded.
    fn half(p: &PlaneView<'_>) -> Self {
        fn half<T: Sample>(p: &Plane<'_, T>, mk: impl Fn(u32) -> T + Sync) -> (Vec<T>, usize, usize) {
            let (w2, h2) = (p.width.div_ceil(2), p.height.div_ceil(2));
            let mut out = vec![T::default(); w2 * h2];
            out.par_chunks_mut(w2).enumerate().for_each(|(y, o)| {
                let r0 = p.row(2 * y);
                let r1 = p.row((2 * y + 1).min(p.height - 1));
                for (x, d) in o.iter_mut().enumerate() {
                    let x1 = (2 * x + 1).min(p.width - 1);
                    let s = r0[2 * x].into() + r0[x1].into() + r1[2 * x].into() + r1[x1].into();
                    *d = mk((s + 2) / 4);
                }
            });
            (out, w2, h2)
        }
        match p {
            PlaneView::U8(q) => {
                let (data, width, height) = half(q, |v| v as u8);
                OwnedPlane::U8 { data, width, height }
            }
            PlaneView::U16(q) => {
                let (data, width, height) = half(q, |v| v as u16);
                OwnedPlane::U16 { data, width, height }
            }
        }
    }
}

impl OwnedFrame {
    /// Packed 8-bit sRGB RGBA (e.g. the bytes of an `image::RgbaImage`).
    pub fn rgba8(width: usize, height: usize, data: Vec<u8>) -> Self {
        OwnedFrame::Rgb { data: OwnedRgb::U8(data), width, height, channels: 4, transfer: Transfer::Srgb }
    }

    /// Packed planar YUV planes (each `width * height` samples of its own size).
    pub fn yuv(planes: Vec<OwnedPlane>, bit_depth: u32) -> Self {
        OwnedFrame::Yuv { planes, bit_depth, matrix: Matrix::default(), range: Range::default(), transfer: Transfer::default() }
    }

    /// A view of the owned pixels. Panics if the stored geometry is invalid
    /// (use [`OwnedFrame::try_view`] to check).
    pub fn view(&self) -> FrameView<'_> {
        self.try_view().expect("invalid OwnedFrame geometry")
    }

    pub fn try_view(&self) -> Result<FrameView<'_>, FrameError> {
        match self {
            OwnedFrame::Rgb { data, width, height, channels, transfer } => {
                let d = match data {
                    OwnedRgb::U8(v) => RgbData::U8(v),
                    OwnedRgb::U16(v) => RgbData::U16(v),
                    OwnedRgb::F32(v) => RgbData::F32(v),
                };
                Ok(FrameView::Rgb(RgbView::new(d, *width, *height, width * channels, *channels)?.with_transfer(*transfer)))
            }
            OwnedFrame::Yuv { planes, bit_depth, matrix, range, transfer } => {
                let y = planes.first().ok_or_else(|| FrameError::BadGeometry("no planes".into()))?.view();
                let uv = match planes.len() {
                    1 => None,
                    3 => Some((planes[1].view(), planes[2].view())),
                    n => return Err(FrameError::BadGeometry(format!("{n} planes"))),
                };
                Ok(FrameView::Yuv(YuvView::new(y, uv, *bit_depth)?.with_color(*matrix, *range, *transfer)))
            }
        }
    }

    /// Deep copy of a view (packs strided rows).
    pub fn from_view(v: &FrameView<'_>) -> Self {
        match v {
            FrameView::Yuv(y) => OwnedFrame::Yuv {
                planes: y.planes().iter().map(OwnedPlane::from_view).collect(),
                bit_depth: y.bit_depth,
                matrix: y.matrix,
                range: y.range,
                transfer: y.transfer,
            },
            FrameView::Rgb(r) => {
                let rowlen = r.width * r.channels;
                fn copy<T: Copy>(d: &[T], stride: usize, rowlen: usize, h: usize) -> Vec<T> {
                    let mut v = Vec::with_capacity(rowlen * h);
                    for y in 0..h {
                        v.extend_from_slice(&d[y * stride..y * stride + rowlen]);
                    }
                    v
                }
                let data = match r.data {
                    RgbData::U8(d) => OwnedRgb::U8(copy(d, r.stride, rowlen, r.height)),
                    RgbData::U16(d) => OwnedRgb::U16(copy(d, r.stride, rowlen, r.height)),
                    RgbData::F32(d) => OwnedRgb::F32(copy(d, r.stride, rowlen, r.height)),
                };
                OwnedFrame::Rgb { data, width: r.width, height: r.height, channels: r.channels, transfer: r.transfer }
            }
        }
    }

    /// Half-size copy (2x2 box filter, 1/4 area) for preview runs of heavy metrics.
    pub fn downsample2(v: &FrameView<'_>) -> Self {
        match v {
            FrameView::Yuv(y) => OwnedFrame::Yuv {
                planes: y.planes().iter().map(OwnedPlane::half).collect(),
                bit_depth: y.bit_depth,
                matrix: y.matrix,
                range: y.range,
                transfer: y.transfer,
            },
            FrameView::Rgb(r) => {
                let (w2, h2) = (r.width.div_ceil(2), r.height.div_ceil(2));
                let c = r.channels;
                fn half<T: Copy + ToF32 + Default + Send + Sync>(d: &[T], r: &RgbView<'_>, w2: usize, h2: usize, mk: impl Fn(f32) -> T + Sync) -> Vec<T> {
                    let c = r.channels;
                    let mut out = vec![T::default(); w2 * h2 * c];
                    out.par_chunks_mut(w2 * c).enumerate().for_each(|(y, o)| {
                        let r0 = &d[2 * y * r.stride..];
                        let r1 = &d[(2 * y + 1).min(r.height - 1) * r.stride..];
                        for x in 0..w2 {
                            let x0 = 2 * x * c;
                            let x1 = (2 * x + 1).min(r.width - 1) * c;
                            for k in 0..c {
                                let s = r0[x0 + k].f() + r0[x1 + k].f() + r1[x0 + k].f() + r1[x1 + k].f();
                                o[x * c + k] = mk(s * 0.25);
                            }
                        }
                    });
                    out
                }
                let data = match r.data {
                    RgbData::U8(d) => OwnedRgb::U8(half(d, r, w2, h2, |v| (v + 0.5) as u8)),
                    RgbData::U16(d) => OwnedRgb::U16(half(d, r, w2, h2, |v| (v + 0.5) as u16)),
                    RgbData::F32(d) => OwnedRgb::F32(half(d, r, w2, h2, |v| v)),
                };
                OwnedFrame::Rgb { data, width: w2, height: h2, channels: c, transfer: r.transfer }
            }
        }
    }
}

/// Anything that can lend a [`FrameView`]; jobs take frames through this so
/// the app can pass its own decoded buffers (e.g. `Arc<YuvFrame>`) without copying.
pub trait FrameSource: Send + Sync {
    fn view(&self) -> FrameView<'_>;
}

impl FrameSource for OwnedFrame {
    fn view(&self) -> FrameView<'_> {
        OwnedFrame::view(self)
    }
}

impl<T: FrameSource + ?Sized> FrameSource for std::sync::Arc<T> {
    fn view(&self) -> FrameView<'_> {
        (**self).view()
    }
}

impl<T: FrameSource + ?Sized> FrameSource for Box<T> {
    fn view(&self) -> FrameView<'_> {
        (**self).view()
    }
}
