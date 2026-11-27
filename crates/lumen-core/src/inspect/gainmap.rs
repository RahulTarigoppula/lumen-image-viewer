//! HDR gain maps in JPEG files: where the gain map image is, what its
//! metadata says, and the math that turns the photo into its HDR version.
//!
//! Three kinds of metadata are read:
//! - Ultra HDR / Adobe gain map: `hdrgm:` XMP in the gain map image.
//! - ISO 21496-1: a binary APP2 segment in the gain map image (newer Android
//!   and iOS versions write it next to, or instead of, the XMP).
//! - Apple's own JPEG gain map (`HDRGainMap:` XMP), whose headroom comes from
//!   two Apple maker note tags.
//!
//! The gain map image is found through the multi-picture (MPF) index, the
//! GContainer directory in the main image's XMP, or as a JPEG after the main
//! image. HEIC gain maps are not read: Windows' HEIC decoder does not expose
//! auxiliary images.
//!
//! The math (Ultra HDR specification; ISO 21496-1 uses the same model) for a
//! linear base value `e` and a gain map value `g` (0..1) of one channel:
//!
//! ```text
//! r         = g ^ (1 / gamma)
//! log_boost = min_log2 * (1 - r) + max_log2 * r
//! hdr       = (e + offset_base) * 2 ^ (log_boost * w) - offset_alt
//! w         = clamp((log2(H) - capacity_min) / (capacity_max - capacity_min), 0, 1)
//! ```
//!
//! where `H` is the screen headroom (how much brighter than SDR white the
//! screen can go) and `w` becomes `1 - w` when the base image is the HDR one.
//! Apple: `hdr = e * (1 + (headroom ^ w - 1) * srgb_to_linear(g))`.

use super::exifview::{self, AppleNote};
use super::{be16, be32, jpeg, xmp, Field, Group, Parts};
use crate::catalog::human_size;
use image::RgbaImage;
use lumen_analysis::color::{linear_to_srgb, srgb_to_linear, srgb_u8_lut};
use rayon::prelude::*;
use std::path::Path;

/// Which metadata describes the gain map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainMapKind {
    /// Adobe / Google Ultra HDR XMP (`hdrgm:` properties).
    UltraHdr,
    /// ISO 21496-1 binary metadata.
    Iso,
    /// Apple's JPEG gain map (headroom from the Apple maker note).
    Apple,
}

impl GainMapKind {
    /// Short name, e.g. for the views' legend.
    pub fn label(self) -> &'static str {
        match self {
            GainMapKind::UltraHdr => "Ultra HDR",
            GainMapKind::Iso => "ISO 21496-1",
            GainMapKind::Apple => "Apple",
        }
    }

    fn long(self) -> &'static str {
        match self {
            GainMapKind::UltraHdr => "Ultra HDR (Adobe gain map XMP)",
            GainMapKind::Iso => "ISO 21496-1 gain map",
            GainMapKind::Apple => "Apple HDR gain map",
        }
    }
}

/// Gain map metadata, per color channel (red, green, blue) where it can differ.
#[derive(Clone, Debug, PartialEq)]
pub struct GainMapMeta {
    pub kind: GainMapKind,
    /// log2 of the boost at gain map black (usually 0: no change).
    pub min_log2: [f32; 3],
    /// log2 of the boost at gain map white (2 = four times brighter).
    pub max_log2: [f32; 3],
    /// Gain map values are raised to 1 / gamma before use.
    pub gamma: [f32; 3],
    /// Added to the base image before the boost.
    pub offset_base: [f32; 3],
    /// Removed from the result after the boost.
    pub offset_alt: [f32; 3],
    /// log2 of the screen headroom where the map starts to apply ...
    pub capacity_min: f32,
    /// ... and where it applies fully.
    pub capacity_max: f32,
    /// The base image is the HDR version (the map makes the SDR one).
    pub base_is_hdr: bool,
    /// The metadata has separate values per color channel.
    pub per_channel: bool,
    /// Apple: the headroom is not stored, so a typical one is assumed.
    pub assumed: bool,
}

impl Default for GainMapMeta {
    fn default() -> Self {
        GainMapMeta {
            kind: GainMapKind::UltraHdr,
            min_log2: [0.0; 3],
            max_log2: [0.0; 3],
            gamma: [1.0; 3],
            offset_base: [1.0 / 64.0; 3],
            offset_alt: [1.0 / 64.0; 3],
            capacity_min: 0.0,
            capacity_max: 0.0,
            base_is_hdr: false,
            per_channel: false,
            assumed: false,
        }
    }
}

impl GainMapMeta {
    /// Largest boost (of any channel) as a factor, e.g. 4.0.
    pub fn max_boost(&self) -> f32 {
        self.max_log2.iter().fold(f32::MIN, |a, &b| a.max(b)).exp2()
    }

    /// Smallest boost (of any channel) as a factor, usually 1.0.
    pub fn min_boost(&self) -> f32 {
        self.min_log2.iter().fold(f32::MAX, |a, &b| a.min(b)).exp2()
    }

    /// The largest screen headroom worth showing (end of the headroom slider), at least 1.
    pub fn headroom_max(&self) -> f32 {
        let l = self.capacity_max.max(self.capacity_min).max(self.max_log2.iter().fold(0.0f32, |a, &b| a.max(b)));
        l.clamp(0.0, 10.0).exp2()
    }

    fn check(&self) -> Result<(), String> {
        let logs = || self.min_log2.iter().chain(&self.max_log2).chain([&self.capacity_min, &self.capacity_max]);
        let offsets = || self.offset_base.iter().chain(&self.offset_alt);
        if logs().chain(&self.gamma).chain(offsets()).any(|v| !v.is_finite()) {
            return Err("a value is not a number".into());
        }
        if self.gamma.iter().any(|&g| g <= 0.0) {
            return Err("the gamma is not above 0".into());
        }
        if (0..3).any(|c| self.max_log2[c] < self.min_log2[c]) {
            return Err("the largest boost is below the smallest".into());
        }
        if logs().any(|v| v.abs() > 16.0) || offsets().any(|v| v.abs() > 1.0) {
            return Err("the values are out of range".into());
        }
        Ok(())
    }
}

// ---- Math ----

/// How much of the gain map a screen with `headroom` (1 = SDR, 4 = four
/// times brighter than SDR white) uses: 0 (none) to 1 (all of it).
pub fn weight(m: &GainMapMeta, headroom: f32) -> f32 {
    let h = headroom.max(1.0).log2();
    let (lo, hi) = (m.capacity_min, m.capacity_max);
    let w = if hi > lo {
        ((h - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else if h > hi {
        1.0
    } else {
        0.0
    };
    if m.base_is_hdr { 1.0 - w } else { w }
}

/// The factor channel `c` of a pixel is multiplied by, for gain map value
/// `g` (0..1) and weight `w` (see [`weight`]).
pub fn factor(m: &GainMapMeta, c: usize, g: f32, w: f32) -> f32 {
    let c = c.min(2);
    let g = if g.is_nan() { 0.0 } else { g.clamp(0.0, 1.0) };
    match m.kind {
        GainMapKind::Apple => 1.0 + ((m.max_log2[c] * w).exp2() - 1.0) * srgb_to_linear(g),
        _ => {
            let gm = m.gamma[c];
            let r = if gm == 1.0 { g } else { g.powf(1.0 / gm) };
            ((m.min_log2[c] * (1.0 - r) + m.max_log2[c] * r) * w).exp2()
        }
    }
}

/// Linear HDR values of one pixel: `base` is the linear base image value
/// (1 = SDR white), `g` the gain map value of each channel (0..1; the same
/// three times for a one-channel map), `w` the weight from [`weight`].
pub fn apply(m: &GainMapMeta, base: [f32; 3], g: [f32; 3], w: f32) -> [f32; 3] {
    std::array::from_fn(|c| (base[c] + m.offset_base[c]) * factor(m, c, g[c], w) - m.offset_alt[c])
}

/// The tone curve of the HDR rendition view: a straight line that shows
/// `headroom` x SDR white as white, so SDR white shows at 1 / headroom (as on
/// an HDR screen running at its peak). Linear in, linear out (0..1).
pub fn tone(hdr: f32, headroom: f32) -> f32 {
    let v = hdr / headroom.max(1.0);
    if v > 0.0 { v.min(1.0) } else { 0.0 }
}

/// Apple's headroom from maker note tags 0x0021 and 0x0030 (Apple's
/// "Applying Apple HDR effect to your photos").
pub(crate) fn apple_headroom(maker33: f64, maker48: f64) -> f64 {
    let stops = if maker33 < 1.0 {
        if maker48 <= 0.01 { -20.0 * maker48 + 1.8 } else { -0.101 * maker48 + 1.601 }
    } else if maker48 <= 0.01 {
        -70.0 * maker48 + 3.0
    } else {
        -0.303 * maker48 + 2.303
    };
    stops.max(0.0).exp2()
}

// ---- Metadata ----

/// Ultra HDR XMP (`hdrgm:` properties of the gain map image). Ok(None) when
/// the required GainMapMax is absent.
fn parse_hdrgm(props: &[(String, String)]) -> Result<Option<GainMapMeta>, String> {
    let get = |name: &str| {
        let full = format!("hdrgm:{name}");
        let nested = format!(" / {full}");
        props.iter().find(|(k, _)| *k == full || k.ends_with(&nested)).map(|(_, v)| v.trim())
    };
    let Some(max) = get("GainMapMax") else { return Ok(None) };
    let triple = |name: &str, v: &str| -> Result<([f32; 3], bool), String> {
        let vals = v.split(',').map(|s| s.trim().parse::<f32>()).collect::<Result<Vec<f32>, _>>().map_err(|_| format!("hdrgm:{name} is not a number ({})", super::clip(v, 40)))?;
        match vals.as_slice() {
            [a] => Ok(([*a; 3], false)),
            [a, b, c] => Ok(([*a, *b, *c], true)),
            _ => Err(format!("hdrgm:{name} has {} values (1 or 3 expected)", vals.len())),
        }
    };
    let opt3 = |name: &str, default: f32| match get(name) {
        Some(v) => triple(name, v),
        None => Ok(([default; 3], false)),
    };
    let one = |name: &str, default: f32| match get(name) {
        Some(v) => v.parse::<f32>().map_err(|_| format!("hdrgm:{name} is not a number ({})", super::clip(v, 40))),
        None => Ok(default),
    };
    let (max_log2, p1) = triple("GainMapMax", max)?;
    let (min_log2, p2) = opt3("GainMapMin", 0.0)?;
    let (gamma, p3) = opt3("Gamma", 1.0)?;
    let (off_sdr, p4) = opt3("OffsetSDR", 1.0 / 64.0)?;
    let (off_hdr, p5) = opt3("OffsetHDR", 1.0 / 64.0)?;
    let cap_min = one("HDRCapacityMin", 0.0)?;
    let cap_max = one("HDRCapacityMax", max_log2.iter().fold(f32::MIN, |a, &b| a.max(b)))?;
    let base_is_hdr = get("BaseRenditionIsHDR").is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
    let (offset_base, offset_alt) = if base_is_hdr { (off_hdr, off_sdr) } else { (off_sdr, off_hdr) };
    let m = GainMapMeta {
        kind: GainMapKind::UltraHdr,
        min_log2,
        max_log2,
        gamma,
        offset_base,
        offset_alt,
        capacity_min: cap_min.min(cap_max),
        capacity_max: cap_min.max(cap_max),
        base_is_hdr,
        per_channel: p1 || p2 || p3 || p4 || p5,
        assumed: false,
    };
    m.check()?;
    Ok(Some(m))
}

/// Big-endian reader for the ISO 21496-1 payload.
struct Rd<'a> {
    d: &'a [u8],
    at: usize,
}

const ISO_SHORT: &str = "the ISO 21496-1 metadata is cut short";

impl Rd<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.d.get(self.at).ok_or(ISO_SHORT)?;
        self.at += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let v = be16(self.d, self.at).ok_or(ISO_SHORT)?;
        self.at += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, String> {
        let v = be32(self.d, self.at).ok_or(ISO_SHORT)?;
        self.at += 4;
        Ok(v)
    }
    fn s32(&mut self) -> Result<i32, String> {
        self.u32().map(|v| v as i32)
    }
}

/// ISO 21496-1 metadata (the payload after "urn:iso:std:iso:ts:21496:-1\0").
/// Ok(None) for the version-only segment of a main image.
///
/// Layout (big-endian, as libultrahdr writes it): u16 minimum version (0),
/// u16 writer version, u8 flags (0x80 three channels, 0x40 use the base color
/// space, 0x08 one common denominator, 0x04 the base image is HDR), then the
/// base and alternate headroom and per channel the gain map min, max, gamma,
/// base offset and alternate offset, as fractions (each with its own
/// denominator, or all sharing the one written first).
fn parse_iso(d: &[u8]) -> Result<Option<GainMapMeta>, String> {
    if d.len() <= 4 {
        return Ok(None);
    }
    let mut r = Rd { d, at: 0 };
    let min_version = r.u16()?;
    let _writer_version = r.u16()?;
    if min_version != 0 {
        return Err(format!("ISO 21496-1 metadata version {min_version} is not supported"));
    }
    let flags = r.u8()?;
    let channels = if flags & 0x80 != 0 { 3 } else { 1 };
    let frac = |n: f64, den: u32| -> Result<f32, String> {
        if den == 0 { Err("a fraction has a zero denominator".to_string()) } else { Ok((n / den as f64) as f32) }
    };
    let mut m = GainMapMeta { kind: GainMapKind::Iso, per_channel: channels == 3, ..Default::default() };
    let (base_h, alt_h);
    if flags & 0x08 != 0 {
        let den = r.u32()?;
        base_h = frac(r.u32()? as f64, den)?;
        alt_h = frac(r.u32()? as f64, den)?;
        for c in 0..channels {
            m.min_log2[c] = frac(r.s32()? as f64, den)?;
            m.max_log2[c] = frac(r.s32()? as f64, den)?;
            m.gamma[c] = frac(r.u32()? as f64, den)?;
            m.offset_base[c] = frac(r.s32()? as f64, den)?;
            m.offset_alt[c] = frac(r.s32()? as f64, den)?;
        }
    } else {
        let n = r.u32()?;
        base_h = frac(n as f64, r.u32()?)?;
        let n = r.u32()?;
        alt_h = frac(n as f64, r.u32()?)?;
        for c in 0..channels {
            let n = r.s32()?;
            m.min_log2[c] = frac(n as f64, r.u32()?)?;
            let n = r.s32()?;
            m.max_log2[c] = frac(n as f64, r.u32()?)?;
            let n = r.u32()?;
            m.gamma[c] = frac(n as f64, r.u32()?)?;
            let n = r.s32()?;
            m.offset_base[c] = frac(n as f64, r.u32()?)?;
            let n = r.s32()?;
            m.offset_alt[c] = frac(n as f64, r.u32()?)?;
        }
    }
    for c in channels..3 {
        m.min_log2[c] = m.min_log2[0];
        m.max_log2[c] = m.max_log2[0];
        m.gamma[c] = m.gamma[0];
        m.offset_base[c] = m.offset_base[0];
        m.offset_alt[c] = m.offset_alt[0];
    }
    m.base_is_hdr = flags & 0x04 != 0 || alt_h < base_h;
    m.capacity_min = base_h.min(alt_h);
    m.capacity_max = base_h.max(alt_h);
    m.check()?;
    Ok(Some(m))
}

/// Apple's gain map: boost up to the maker note headroom.
fn apple_meta(apple: &AppleNote) -> GainMapMeta {
    let (h, assumed) = match (apple.hdr_headroom, apple.hdr_gain) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => (apple_headroom(a, b), false),
        _ => (4.0, true),
    };
    let l = (h as f32).log2().clamp(0.0, 10.0);
    GainMapMeta {
        kind: GainMapKind::Apple,
        min_log2: [0.0; 3],
        max_log2: [l; 3],
        gamma: [1.0; 3],
        offset_base: [0.0; 3],
        offset_alt: [0.0; 3],
        capacity_min: 0.0,
        capacity_max: l,
        base_is_hdr: false,
        per_channel: false,
        assumed,
    }
}

// ---- Finding the gain map ----

/// What locating the gain map needs to know about the main image.
pub(crate) struct Primary<'a> {
    pub mpf: &'a [jpeg::MpImage],
    pub mpf_at: Option<usize>,
    /// End of the main JPEG (after its end marker).
    pub end: Option<usize>,
    pub items: &'a [Vec<(String, String)>],
    pub xmp: &'a [(String, String)],
    pub orientation: u8,
    pub apple: &'a AppleNote,
    /// The main image has an ISO 21496-1 segment.
    pub iso: bool,
}

impl Primary<'_> {
    /// Whether the main image says it has a gain map.
    pub fn announced(&self) -> bool {
        self.iso
            || self.xmp.iter().any(|(k, _)| k.contains("hdrgm:") || k.contains("HDRGainMap:"))
            || self.items.iter().any(|it| it.iter().any(|(k, v)| k.ends_with(":Semantic") && v.eq_ignore_ascii_case("GainMap")))
    }
}

/// A gain map found in a file.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub meta: GainMapMeta,
    /// The gain map JPEG's bytes in the file.
    pub start: usize,
    pub end: usize,
    /// Where it was found, e.g. "multi-picture image 2".
    pub source: String,
    /// Its entry in the MPF index (0-based).
    pub mpf_index: Option<usize>,
    /// Stored pixel size (before EXIF rotation).
    pub dims: Option<(u32, u32)>,
    /// One channel (gray) rather than three.
    pub gray: bool,
    /// EXIF orientation of the main image (the map is stored the same way).
    pub orientation: u8,
    /// Another kind of metadata for the same map that was not used.
    pub also: Option<GainMapKind>,
}

/// Why there is no gain map to show.
#[derive(Clone, Debug, PartialEq)]
pub struct NoGainMap {
    /// A sentence for people.
    pub reason: String,
    /// The file says it has a gain map (it is damaged or of a kind not read).
    pub announced: bool,
}

const NO_MAP: &str = "This photo has no HDR gain map.";

struct MapInfo {
    meta: GainMapMeta,
    also: Option<GainMapKind>,
    end: usize,
    dims: Option<(u32, u32)>,
    gray: bool,
}

/// Look at a JPEG starting at `start` (at most `len` bytes): where it ends,
/// and its gain map metadata when it has some.
fn examine(b: &[u8], start: usize, len: Option<usize>, pr: &Primary) -> (usize, Result<Option<MapInfo>, String>) {
    let lim = len.map_or(b.len(), |l| start.saturating_add(l).min(b.len()));
    let Some(sub) = b.get(start..lim).filter(|s| s.starts_with(&[0xFF, 0xD8, 0xFF])) else {
        return (start.saturating_add(3), Ok(None));
    };
    let mut q = Parts::default();
    jpeg::parse(sub, &mut q, &mut Vec::new());
    let end = start + q.image_end.unwrap_or(sub.len());
    let props = q.xmp.as_deref().map(|t| xmp::Doc::parse(t).properties()).unwrap_or_default();
    let iso = q.iso_gain_map_data.as_deref().map_or(Ok(None), parse_iso);
    let adobe = parse_hdrgm(&props);
    let apple = props.iter().any(|(k, _)| k.contains("HDRGainMap:"));
    let info = |meta: GainMapMeta, also: Option<GainMapKind>| MapInfo { meta, also, end, dims: q.dims, gray: q.components == 1 };
    let r = match (iso, adobe) {
        (Ok(Some(m)), x) => Ok(Some(info(m, matches!(x, Ok(Some(_))).then_some(GainMapKind::UltraHdr)))),
        (_, Ok(Some(m))) => Ok(Some(info(m, None))),
        (Err(e), _) | (_, Err(e)) => Err(e),
        (Ok(None), Ok(None)) if apple => Ok(Some(info(apple_meta(pr.apple), None))),
        _ => Ok(None),
    };
    (end, r)
}

/// Find the gain map of the JPEG `b` whose main image is described by `pr`.
pub(crate) fn locate_in(b: &[u8], pr: &Primary) -> Result<Found, NoGainMap> {
    let announced = pr.announced();
    let Some(end) = pr.end else {
        let reason = if announced { "The main image is cut short, so its HDR gain map cannot be found." } else { NO_MAP };
        return Err(NoGainMap { reason: reason.into(), announced });
    };
    // Candidates, most trusted first: (start, length, where, MPF index).
    let mut cands: Vec<(usize, Option<usize>, String, Option<usize>)> = Vec::new();
    if let Some(at) = pr.mpf_at {
        for (i, im) in pr.mpf.iter().enumerate().skip(1) {
            if im.offset > 0 && im.size > 0 {
                cands.push((at.saturating_add(im.offset as usize), Some(im.size as usize), format!("multi-picture image {}", i + 1), Some(i)));
            }
        }
    }
    let mut at = end;
    for item in pr.items {
        let get = |s: &str| item.iter().find(|(k, _)| k.ends_with(s)).map(|(_, v)| v.trim());
        let num = |s: &str| get(s).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        let (len, pad) = (num(":Length"), num(":Padding"));
        let sem = get(":Semantic").unwrap_or("");
        if sem.eq_ignore_ascii_case("Primary") {
            at = at.saturating_add(pad);
            continue;
        }
        if sem.eq_ignore_ascii_case("GainMap") && len > 0 {
            cands.push((at, Some(len), "the image the GContainer directory lists".into(), None));
        }
        at = at.saturating_add(len).saturating_add(pad);
    }
    let found = |x: MapInfo, start: usize, source: String, mpf_index: Option<usize>| Found {
        meta: x.meta,
        start,
        end: x.end,
        source,
        mpf_index,
        dims: x.dims,
        gray: x.gray,
        orientation: pr.orientation,
        also: x.also,
    };
    let mut err: Option<String> = None;
    let mut seen: Vec<usize> = Vec::new();
    for (start, len, source, idx) in cands {
        if seen.contains(&start) {
            continue;
        }
        seen.push(start);
        match examine(b, start, len, pr).1 {
            Ok(Some(x)) => return Ok(found(x, start, source, idx)),
            Err(e) => {
                err.get_or_insert(e);
            }
            Ok(None) => {}
        }
    }
    // Then the JPEGs after the main image (a few at most).
    let mut pos = end;
    for _ in 0..8 {
        let Some(k) = b.get(pos..).and_then(|s| super::find(s, &[0xFF, 0xD8, 0xFF])) else { break };
        let start = pos + k;
        let (next, r) = examine(b, start, None, pr);
        if !seen.contains(&start) {
            match r {
                Ok(Some(x)) => return Ok(found(x, start, "the image after the main picture".into(), None)),
                Err(e) => {
                    err.get_or_insert(e);
                }
                Ok(None) => {}
            }
        }
        pos = next.max(start + 3);
    }
    Err(match err {
        Some(e) => NoGainMap { reason: format!("The HDR gain map metadata cannot be used: {e}."), announced: true },
        None if announced => NoGainMap { reason: "The file says it has an HDR gain map, but no gain map image with its metadata was found.".into(), announced },
        None => NoGainMap { reason: NO_MAP.into(), announced },
    })
}

/// [`locate_in`] for a whole JPEG file in memory.
pub(crate) fn locate(b: &[u8]) -> Result<Found, NoGainMap> {
    if !b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(NoGainMap { reason: "Gain maps are read from JPEG files only.".into(), announced: false });
    }
    let mut p = Parts::default();
    let mut warn = Vec::new();
    jpeg::parse(b, &mut p, &mut warn);
    let exif = p.exif_raw.take().and_then(|raw| exifview::read_raw(raw, false, &mut warn));
    let orientation = exif.as_ref().and_then(|e| exifview::Ex(e).uint(exif::Tag::Orientation)).map_or(1, |o| o.clamp(1, 8) as u8);
    let apple = exif.as_ref().map(exifview::apple_note).unwrap_or_default();
    let mpf = p.mpf.as_deref().map(|m| jpeg::parse_mpf(m, &mut warn)).unwrap_or_default();
    let doc = p.xmp.as_deref().map(xmp::Doc::parse);
    let props = doc.as_ref().map(|d| d.properties()).unwrap_or_default();
    let items = doc.as_ref().map(|d| d.container_items()).unwrap_or_default();
    let pr = Primary { mpf: &mpf, mpf_at: p.mpf_at, end: p.image_end, items: &items, xmp: &props, orientation, apple: &apple, iso: p.iso_gain_map };
    locate_in(b, &pr)
}

/// Whether the start of a HEIF / AVIF file lists an HDR gain map (Apple's
/// auxiliary image or an ISO 21496-1 'tmap' item).
fn heif_announces_gain_map(head: &[u8]) -> bool {
    let head = &head[..head.len().min(64 * 1024)];
    head.get(4..8) == Some(&b"ftyp"[..]) && (super::find(head, b"hdrgainmap").is_some() || super::find(head, b"tmap").is_some())
}

// ---- The gain map image ----

/// A decoded gain map, ready for the Inspect views.
pub struct GainMap {
    pub meta: GainMapMeta,
    /// The gain map pixels, turned like the photo; gray for a one-channel map.
    pub image: RgbaImage,
    /// 1 (one map for all colors) or 3 (one per color channel).
    pub channels: u8,
    /// Where it is stored, e.g. "multi-picture image 2".
    pub source: String,
}

/// Largest file [`load`] reads.
const MAX_FILE: u64 = 512 * 1024 * 1024;

/// Find and decode the gain map of the image file at `path`. JPEG only.
pub fn load(path: &Path) -> Result<GainMap, NoGainMap> {
    use std::io::Read;
    let cannot = |e: std::io::Error| NoGainMap { reason: format!("Cannot read the file: {e}"), announced: false };
    let mut f = std::fs::File::open(path).map_err(cannot)?;
    let mut bytes = Vec::new();
    (&mut f).take(256 * 1024).read_to_end(&mut bytes).map_err(cannot)?;
    if !bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(if heif_announces_gain_map(&bytes) {
            NoGainMap { reason: "This file has an HDR gain map, but gain maps are read from JPEG files only (not HEIC or AVIF).".into(), announced: true }
        } else {
            NoGainMap { reason: NO_MAP.into(), announced: false }
        });
    }
    (&mut f).take(MAX_FILE).read_to_end(&mut bytes).map_err(cannot)?;
    load_bytes(&bytes)
}

/// [`load`] for a JPEG file in memory.
pub fn load_bytes(b: &[u8]) -> Result<GainMap, NoGainMap> {
    let f = locate(b)?;
    let bad = |why: String| NoGainMap { reason: format!("The HDR gain map image cannot be decoded. {why}"), announced: true };
    if let Some((w, h)) = f.dims {
        crate::decode::check_alloc(w, h).map_err(|e| bad(crate::decode::friendly_error(&e)))?;
    }
    let data = b.get(f.start..f.end.min(b.len())).unwrap_or_default();
    let img = crate::decode::guard("gain map", || image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).map_err(anyhow::Error::from)).map_err(|e| bad(crate::decode::friendly_error(&e)))?;
    if img.width() == 0 || img.height() == 0 {
        return Err(bad("It has no pixels.".into()));
    }
    let channels = if img.color().channel_count() >= 3 { 3 } else { 1 };
    let image = crate::imaging::orient(img.into_rgba8(), f.orientation);
    Ok(GainMap { meta: f.meta, image, channels, source: f.source })
}

/// A boost factor for people: "4", "3.48".
fn times(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl GainMap {
    /// Short lines describing the map, for the views' legend.
    pub fn legend(&self) -> Vec<String> {
        let m = &self.meta;
        let (w, h) = self.image.dimensions();
        let mut out = vec![format!("{}, {w} x {h}, {}", m.kind.label(), if self.channels == 1 { "one channel" } else { "three channels" })];
        if m.kind == GainMapKind::Apple {
            out.push(format!("Black: no change, white: x{} brighter{}", times(m.max_boost()), if m.assumed { " (assumed)" } else { "" }));
        } else {
            let per = if m.per_channel { " (brightest channel)" } else { "" };
            out.push(format!("Black: x{}, white: x{} brighter{per}", times(m.min_boost()), times(m.max_boost())));
            out.push(format!("Applies from x{} to x{} screen headroom", times(m.capacity_min.exp2()), times(m.capacity_max.exp2())));
        }
        if m.base_is_hdr {
            out.push("The main image is the HDR version".into());
        }
        out
    }
}

/// A gain map resampled to a base image's size, ready for [`render_hdr`].
pub struct Prepared {
    pub width: u32,
    pub height: u32,
    channels: u8,
    /// `channels` values per pixel: 0 = gain map black, 65535 = white.
    gain: Vec<u16>,
}

/// Resample `map` to `width` x `height` (bilinear, pixel centers aligned).
pub fn prepare(map: &GainMap, width: u32, height: u32) -> Prepared {
    let (gw, gh) = map.image.dimensions();
    let ch = if map.channels >= 3 { 3usize } else { 1 };
    let (w, h) = (width as usize, height as usize);
    let mut gain = vec![0u16; w * h * ch];
    if w > 0 && h > 0 && gw > 0 && gh > 0 {
        let axis = |n: usize, m: u32| -> Vec<(usize, usize, f32)> {
            (0..n)
                .map(|i| {
                    let s = ((i as f32 + 0.5) * m as f32 / n as f32 - 0.5).clamp(0.0, (m - 1) as f32);
                    let i0 = (s.floor() as usize).min(m as usize - 1);
                    (i0, (i0 + 1).min(m as usize - 1), s - i0 as f32)
                })
                .collect()
        };
        let (xs, ys) = (axis(w, gw), axis(h, gh));
        let src = map.image.as_raw();
        let stride = gw as usize * 4;
        gain.par_chunks_mut(w * ch).zip(ys.par_iter()).for_each(|(row, &(y0, y1, fy))| {
            for (x, &(x0, x1, fx)) in xs.iter().enumerate() {
                for c in 0..ch {
                    let px = |xx: usize, yy: usize| src[yy * stride + xx * 4 + c] as f32;
                    let top = px(x0, y0) + (px(x1, y0) - px(x0, y0)) * fx;
                    let bottom = px(x0, y1) + (px(x1, y1) - px(x0, y1)) * fx;
                    let v = top + (bottom - top) * fy;
                    row[x * ch + c] = (v * 257.0).round().clamp(0.0, 65535.0) as u16;
                }
            }
        });
    }
    Prepared { width, height, channels: ch as u8, gain }
}

/// Steps of the boost lookup table (over gain map values 0..1).
const LUT_STEPS: usize = 1024;

/// The "HDR rendition" view: `base` (8-bit sRGB) with the gain map applied
/// for a screen with `headroom`, shown with [`tone`]. `prep` must be the
/// map resampled to `base`'s size (otherwise `base` comes back unchanged).
pub fn render_hdr(base: &RgbaImage, prep: &Prepared, meta: &GainMapMeta, headroom: f32) -> RgbaImage {
    let (w, h) = base.dimensions();
    let ch = prep.channels as usize;
    if (w, h) != (prep.width, prep.height) || prep.gain.len() != w as usize * h as usize * ch || w == 0 || h == 0 {
        return base.clone();
    }
    let head = headroom.max(1.0);
    let wt = weight(meta, head);
    let lin = srgb_u8_lut();
    let boost: Vec<Vec<f32>> = (0..3).map(|c| (0..LUT_STEPS).map(|i| factor(meta, c, i as f32 / (LUT_STEPS - 1) as f32, wt)).collect()).collect();
    let enc: Vec<u8> = (0..4096).map(|i| (linear_to_srgb(i as f32 / 4095.0) * 255.0 + 0.5).clamp(0.0, 255.0) as u8).collect();
    let mut out = RgbaImage::new(w, h);
    let row = w as usize * 4;
    out.par_chunks_exact_mut(row).zip(base.as_raw().par_chunks_exact(row)).zip(prep.gain.par_chunks_exact(w as usize * ch)).for_each(|((o, i), g)| {
        for x in 0..w as usize {
            for c in 0..3 {
                let gv = g[x * ch + if ch == 1 { 0 } else { c }] as usize;
                let f = boost[c][(gv * (LUT_STEPS - 1) + 32767) / 65535];
                let hdr = (lin[i[x * 4 + c] as usize] + meta.offset_base[c]) * f - meta.offset_alt[c];
                o[x * 4 + c] = enc[(tone(hdr, head) * 4095.0 + 0.5) as usize];
            }
            o[x * 4 + 3] = i[x * 4 + 3];
        }
    });
    out
}

// ---- Inspection group ----

/// The "HDR gain map" group of the metadata window.
pub(crate) fn group(f: &Found) -> Group {
    let m = &f.meta;
    let mut v = Vec::new();
    let mut note = match m.kind {
        GainMapKind::UltraHdr => "Adobe / Google gain map XMP (hdrgm) in the gain map image, as Android's Ultra HDR writes it.".to_string(),
        GainMapKind::Iso => "ISO 21496-1 binary metadata in the gain map image (newer Android and iOS versions).".to_string(),
        GainMapKind::Apple => "Apple's HDRGainMap XMP; the headroom comes from the Apple maker note.".to_string(),
    };
    if let Some(a) = f.also {
        note.push_str(&format!(" The file also has {} metadata for the same map; the {} values are used.", a.label(), m.kind.label()));
    }
    v.push(Field::new("Format", m.kind.long(), note));
    let mut stored: String = f.source.chars().take(1).flat_map(char::to_uppercase).chain(f.source.chars().skip(1)).collect();
    if let Some((w, h)) = f.dims {
        stored.push_str(&format!(", {w} x {h}"));
    }
    stored.push_str(if f.gray { ", one channel (gray)" } else { ", three channels (color)" });
    stored.push_str(&format!(", {}", human_size(f.end.saturating_sub(f.start) as u64)));
    v.push(Field::new("Gain map image", stored, "A small extra JPEG stored after the main image. Brighter pixels mark the areas an HDR screen shows brighter."));
    let boost_note = "How much brighter the HDR version is than the normal photo, from the gain map's black (smallest boost) to its white (largest).";
    if m.kind == GainMapKind::Apple {
        let h = if m.assumed { format!("up to x{} (assumed: the Apple maker note has no headroom)", times(m.max_boost())) } else { format!("up to x{}", times(m.max_boost())) };
        v.push(Field::new("Boost", h, boost_note));
    } else {
        v.push(Field::new("Boost", format!("x{} to x{}", times(m.min_boost()), times(m.max_boost())), boost_note));
        if m.per_channel {
            let per = ["red", "green", "blue"].iter().enumerate().map(|(c, n)| format!("{n} x{}-{}", times(m.min_log2[c].exp2()), times(m.max_log2[c].exp2()))).collect::<Vec<_>>().join(", ");
            v.push(Field::new("Boost per channel", per, "Separate boost range for each color channel."));
        }
        let gamma = if m.gamma.iter().all(|&g| g == m.gamma[0]) { times(m.gamma[0]) } else { m.gamma.iter().map(|&g| times(g)).collect::<Vec<_>>().join(", ") };
        v.push(Field::new("Gamma", gamma, "Curve applied to the gain map values before use (1 = straight)."));
        v.push(Field::new(
            "Offsets",
            format!("base {:.4}, HDR {:.4}", m.offset_base[0], m.offset_alt[0]),
            "Small values added before the boost and removed after it, so dark pixels can be brightened too.",
        ));
        v.push(Field::new(
            "Screen headroom",
            format!("x{} to x{}", times(m.capacity_min.exp2()), times(m.capacity_max.exp2())),
            "Headroom: how much brighter than SDR white a screen can show. Below the first value the photo shows as stored; from the second on the full HDR version shows.",
        ));
    }
    v.push(Field::new(
        "Main image",
        if m.base_is_hdr { "HDR (the map makes the version for other screens)" } else { "SDR (the map brightens it for HDR screens)" },
        "Which version the main JPEG holds; the gain map turns it into the other one.",
    ));
    Group { name: "HDR gain map".into(), fields: v }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn meta(min: f32, max: f32, gamma: f32) -> GainMapMeta {
        GainMapMeta { min_log2: [min; 3], max_log2: [max; 3], gamma: [gamma; 3], capacity_min: 0.0, capacity_max: max, ..Default::default() }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn weight_follows_the_screen_headroom() {
        let m = meta(0.0, 2.0, 1.0);
        assert!(close(weight(&m, 1.0), 0.0));
        assert!(close(weight(&m, 2.0), 0.5));
        assert!(close(weight(&m, 4.0), 1.0));
        assert!(close(weight(&m, 16.0), 1.0));
        assert!(close(weight(&m, 0.5), 0.0), "below SDR counts as SDR");
        let hdr_base = GainMapMeta { base_is_hdr: true, ..m.clone() };
        assert!(close(weight(&hdr_base, 1.0), 1.0));
        assert!(close(weight(&hdr_base, 4.0), 0.0));
        // Starting above SDR: nothing until x2.
        let late = GainMapMeta { capacity_min: 1.0, ..m };
        assert!(close(weight(&late, 2.0), 0.0) && close(weight(&late, 4.0), 1.0));
    }

    #[test]
    fn factor_and_apply_match_the_spec() {
        let m = meta(0.0, 2.0, 1.0);
        assert!(close(factor(&m, 0, 0.0, 1.0), 1.0));
        assert!(close(factor(&m, 0, 1.0, 1.0), 4.0));
        assert!(close(factor(&m, 0, 0.5, 1.0), 2.0));
        assert!(close(factor(&m, 0, 1.0, 0.5), 2.0), "half the weight, half the stops");
        // Gamma 2: g = 0.25 is r = 0.5, one stop.
        assert!(close(factor(&meta(0.0, 2.0, 2.0), 1, 0.25, 1.0), 2.0));
        // A map that also darkens: min -1 (x0.5) at black.
        assert!(close(factor(&meta(-1.0, 1.0, 1.0), 2, 0.0, 1.0), 0.5));
        // (0.5 + 1/64) * 4 - 1/64 with the default offsets.
        let out = apply(&m, [0.5, 0.0, 1.0], [1.0, 1.0, 0.0], 1.0);
        assert!(close(out[0], 2.046_875), "{out:?}");
        assert!(close(out[1], 0.046_875), "{out:?}");
        assert!(close(out[2], 1.0), "{out:?}");
        // Out of range and NaN gain values clamp.
        assert!(close(factor(&m, 0, 7.0, 1.0), 4.0) && close(factor(&m, 0, f32::NAN, 1.0), 1.0));
    }

    #[test]
    fn apple_math() {
        assert!((apple_headroom(0.5, 0.005) - 1.7f64.exp2()).abs() < 1e-9);
        assert!((apple_headroom(0.5, 0.5) - (1.601f64 - 0.0505).exp2()).abs() < 1e-9);
        assert!((apple_headroom(1.2, 0.0) - 8.0).abs() < 1e-9);
        assert!((apple_headroom(1.2, 0.02) - (2.303f64 - 0.00606).exp2()).abs() < 1e-9);
        assert_eq!(apple_headroom(0.5, 50.0), 1.0, "never below SDR");
        let note = AppleNote { hdr_headroom: Some(1.2), hdr_gain: Some(0.0), ..Default::default() };
        let m = apple_meta(&note);
        assert!(!m.assumed && close(m.max_boost(), 8.0));
        // hdr = sdr * (1 + (8 - 1) * g_linear) at full weight.
        assert!(close(factor(&m, 0, 1.0, 1.0), 8.0));
        assert!(close(factor(&m, 0, 0.5, 1.0), 1.0 + 7.0 * srgb_to_linear(0.5)));
        assert!(close(factor(&m, 0, 1.0, 0.0), 1.0));
        assert!(apple_meta(&AppleNote::default()).assumed);
    }

    #[test]
    fn tone_maps_headroom_to_white() {
        assert!(close(tone(4.0, 4.0), 1.0));
        assert!(close(tone(1.0, 4.0), 0.25));
        assert!(close(tone(9.0, 4.0), 1.0));
        assert!(close(tone(-0.1, 4.0), 0.0) && close(tone(f32::NAN, 4.0), 0.0));
        assert!(close(tone(0.5, 0.5), 0.5), "headroom below 1 counts as 1");
    }

    #[test]
    fn hdrgm_xmp_values() {
        let p = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        let m = parse_hdrgm(&p(&[("hdrgm:Version", "1.0"), ("hdrgm:GainMapMax", "2"), ("hdrgm:HDRCapacityMax", "2")])).unwrap().unwrap();
        assert_eq!(m.max_log2, [2.0; 3]);
        assert_eq!((m.min_log2, m.gamma, m.offset_base), ([0.0; 3], [1.0; 3], [1.0 / 64.0; 3]));
        assert!(!m.per_channel && !m.base_is_hdr && close(m.headroom_max(), 4.0));
        let m = parse_hdrgm(&p(&[("hdrgm:GainMapMax", "1, 2, 3"), ("hdrgm:GainMapMin", "0"), ("hdrgm:OffsetSDR", "0"), ("hdrgm:BaseRenditionIsHDR", "True")])).unwrap().unwrap();
        assert!(m.per_channel && m.base_is_hdr);
        assert_eq!(m.max_log2, [1.0, 2.0, 3.0]);
        assert!(close(m.capacity_max, 3.0), "defaults to the largest boost");
        assert_eq!(m.offset_alt, [0.0; 3], "SDR offset is the alternate one when the base is HDR");
        assert_eq!(parse_hdrgm(&p(&[("hdrgm:Version", "1.0")])), Ok(None));
        assert!(parse_hdrgm(&p(&[("hdrgm:GainMapMax", "big")])).is_err());
        assert!(parse_hdrgm(&p(&[("hdrgm:GainMapMax", "1, 2")])).is_err());
        assert!(parse_hdrgm(&p(&[("hdrgm:GainMapMax", "1"), ("hdrgm:GainMapMin", "2")])).is_err());
        assert!(parse_hdrgm(&p(&[("hdrgm:GainMapMax", "1"), ("hdrgm:Gamma", "0")])).is_err());
    }

    /// ISO 21496-1 payload with one common denominator.
    pub(crate) fn iso_payload(multi: bool, backward: bool) -> Vec<u8> {
        let mut d = vec![0, 0, 0, 0];
        d.push(0x08 | if multi { 0x80 } else { 0 } | if backward { 0x04 } else { 0 });
        let den = 1000u32;
        d.extend(den.to_be_bytes());
        d.extend(0u32.to_be_bytes()); // base headroom log2 0
        d.extend(2000u32.to_be_bytes()); // alternate headroom log2 2
        for c in 0..if multi { 3i32 } else { 1 } {
            d.extend(0i32.to_be_bytes()); // min
            d.extend((2000 - 500 * c).to_be_bytes()); // max
            d.extend(1000u32.to_be_bytes()); // gamma 1
            d.extend(15i32.to_be_bytes()); // base offset 0.015
            d.extend(15i32.to_be_bytes()); // alternate offset
        }
        d
    }

    #[test]
    fn iso_binary_metadata() {
        let m = parse_iso(&iso_payload(false, false)).unwrap().unwrap();
        assert_eq!(m.kind, GainMapKind::Iso);
        assert_eq!(m.max_log2, [2.0; 3]);
        assert!(close(m.offset_base[2], 0.015) && close(m.capacity_max, 2.0) && !m.per_channel && !m.base_is_hdr);
        let m = parse_iso(&iso_payload(true, false)).unwrap().unwrap();
        assert!(m.per_channel);
        assert_eq!(m.max_log2, [2.0, 1.5, 1.0]);
        assert!(parse_iso(&iso_payload(false, true)).unwrap().unwrap().base_is_hdr);
        // Separate denominators.
        let mut d = vec![0, 0, 0, 0, 0];
        for (n, den) in [(0u32, 1u32), (1, 1)] {
            d.extend(n.to_be_bytes());
            d.extend(den.to_be_bytes());
        }
        for (n, den) in [(0i32, 1u32), (3, 2), (1, 1), (1, 64), (1, 64)] {
            d.extend(n.to_be_bytes());
            d.extend(den.to_be_bytes());
        }
        let m = parse_iso(&d).unwrap().unwrap();
        assert!(close(m.max_log2[0], 1.5) && close(m.offset_alt[1], 1.0 / 64.0) && close(m.capacity_max, 1.0));
        // Version only (main image), cut short, unknown version, zero denominator.
        assert_eq!(parse_iso(&[0, 0, 0, 0]), Ok(None));
        let full = iso_payload(false, false);
        assert!(parse_iso(&full[..full.len() - 3]).is_err());
        assert!(parse_iso(&[0, 1, 0, 0, 0, 0]).is_err());
        let mut zero = iso_payload(false, false);
        zero[5..9].copy_from_slice(&0u32.to_be_bytes());
        assert!(parse_iso(&zero).is_err());
    }

    fn gray_map(w: u32, h: u32, f: impl Fn(u32, u32) -> u8, m: GainMapMeta) -> GainMap {
        GainMap { meta: m, image: RgbaImage::from_fn(w, h, |x, y| { let v = f(x, y); image::Rgba([v, v, v, 255]) }), channels: 1, source: "test".into() }
    }

    #[test]
    fn prepare_is_bilinear_with_centers_aligned() {
        let map = gray_map(2, 1, |x, _| if x == 0 { 0 } else { 255 }, meta(0.0, 2.0, 1.0));
        let p = prepare(&map, 4, 2);
        assert_eq!((p.width, p.height, p.channels), (4, 2, 1));
        let row: Vec<u16> = p.gain[..4].to_vec();
        assert_eq!(row, vec![0, 16384, 49151, 65535]);
        assert_eq!(&p.gain[4..], &row[..], "rows repeat for a one-row map");
        let same = prepare(&map, 2, 1);
        assert_eq!(same.gain, vec![0, 65535]);
    }

    #[test]
    fn hdr_rendition_known_pixels() {
        // White and mid-gray base; the left column fully boosted (x4), the right not.
        let base = RgbaImage::from_fn(2, 2, |_, y| if y == 0 { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([188, 188, 188, 200]) });
        let map = gray_map(2, 2, |x, _| if x == 0 { 255 } else { 0 }, meta(0.0, 2.0, 1.0));
        let prep = prepare(&map, 2, 2);
        let out = render_hdr(&base, &prep, &map.meta, 4.0);
        let code = |lin: f32| (linear_to_srgb(lin) * 255.0 + 0.5) as u8;
        // Boosted white goes past the x4 peak: white.
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255, 255]);
        // Unboosted white is SDR white: a quarter of the peak.
        let quarter = code(0.25);
        assert!(out.get_pixel(1, 0).0[0].abs_diff(quarter) <= 1, "{:?} vs {quarter}", out.get_pixel(1, 0));
        // Mid gray (188 = about 0.5 linear) boosted x4 then shown at 1/4: about where it was.
        let g = out.get_pixel(0, 1).0;
        let want = code(tone((srgb_to_linear(188.0 / 255.0) + 1.0 / 64.0) * 4.0 - 1.0 / 64.0, 4.0));
        assert!(g[0].abs_diff(want) <= 1 && g[3] == 200, "{g:?} vs {want}");
        // At headroom 1 nothing is boosted and nothing scaled: the base comes back.
        let same = render_hdr(&base, &prep, &map.meta, 1.0);
        for (a, b) in same.pixels().zip(base.pixels()) {
            assert!(a.0.iter().zip(b.0).all(|(x, y)| x.abs_diff(y) <= 1), "{a:?} vs {b:?}");
        }
        // A mismatched size gives the base back, never panics.
        assert_eq!(render_hdr(&base, &prepare(&map, 3, 3), &map.meta, 4.0), base);
    }

    #[test]
    fn legend_lines() {
        let map = gray_map(8, 6, |_, _| 0, meta(0.0, 2.0, 1.0));
        let l = map.legend();
        assert_eq!(l[0], "Ultra HDR, 8 x 6, one channel");
        assert_eq!(l[1], "Black: x1, white: x4 brighter");
        assert_eq!(l[2], "Applies from x1 to x4 screen headroom");
        assert_eq!(times(3.4816), "3.48");
    }
}
