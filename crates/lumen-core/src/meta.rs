//! EXIF / metadata extraction for the info panel and for sorting by date taken.

use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct ExifSummary {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub exposure: Option<String>,
    pub aperture: Option<String>,
    pub iso: Option<String>,
    pub focal: Option<String>,
    pub taken: Option<String>,
    pub taken_ts: Option<i64>,
    pub flash: Option<String>,
    pub gps: Option<(f64, f64)>,
    pub software: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub description: Option<String>,
    pub orientation: Option<u32>,
    /// All tags, grouped: (IFD group, tag name, value).
    pub all: Vec<(String, String, String)>,
}

fn clean(s: String) -> Option<String> {
    let t = s.trim().trim_matches('"').trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

pub fn read_exif(path: &Path) -> Option<ExifSummary> {
    let exif = exif_from_file(path)?;
    Some(summarize(&exif))
}

/// Parse the EXIF of the file at `path` (JPEG, TIFF and TIFF-based RAW, PNG,
/// HEIF, WebP). Only the file's EXIF block is read, never the image data:
/// kamadak-exif reads a whole TIFF (its own EXIF container: gigabytes for a big
/// scan, over a network share too) and scans a JPEG, PNG or WebP without EXIF
/// to its end, so those containers are walked here instead.
pub fn exif_from_file(path: &Path) -> Option<exif::Exif> {
    let file = std::fs::File::open(path).ok()?;
    exif_from_reader(&mut std::io::BufReader::with_capacity(16 * 1024, file))
}

/// [`exif_from_file`] for the image read by `r` (at its start).
fn exif_from_reader<R: std::io::Read + std::io::Seek>(r: &mut std::io::BufReader<R>) -> Option<exif::Exif> {
    use std::io::BufRead;
    let head = r.fill_buf().ok()?;
    let head: Vec<u8> = head[..head.len().min(12)].to_vec();
    let raw = if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        compact_tiff(r, TIFF_EXIF_CAP).ok().flatten()
    } else if head.starts_with(&[0xFF, 0xD8]) {
        jpeg_exif(r)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        png_exif(r)
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        webp_exif(r)
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        // HEIF / AVIF: kamadak-exif seeks past the boxes it does not need.
        return exif::Reader::new().read_from_container(r).ok();
    } else {
        None
    };
    exif::Reader::new().read_raw(raw?).ok()
}

/// Most bytes of TIFF directories and tag values [`compact_tiff`] copies, and
/// the largest EXIF block read from other containers.
const TIFF_EXIF_CAP: usize = 16 << 20;

/// Read `len` bytes (fewer at the end of the file: an error).
fn read_len<R: std::io::Read>(r: &mut R, len: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    // Not `vec![0; len]`: a damaged length must not allocate it up front.
    r.by_ref().take(len as u64).read_to_end(&mut buf).ok()?;
    (buf.len() == len).then_some(buf)
}

/// Skip `n` bytes forward.
fn skip<R: std::io::Read + std::io::Seek>(r: &mut std::io::BufReader<R>, n: u64) -> Option<()> {
    r.seek_relative(i64::try_from(n).ok()?).ok()
}

/// A JPEG's APP1 "Exif" segment, looked for up to the start of the image data
/// (the first SOS): kamadak-exif scans a JPEG without one to its end.
fn jpeg_exif<R: std::io::Read + std::io::Seek>(r: &mut std::io::BufReader<R>) -> Option<Vec<u8>> {
    use std::io::{BufRead, Read};
    let mut b = [0u8; 2];
    r.read_exact(&mut b).ok()?;
    // Segments before the image data: a few dozen at most.
    for _ in 0..1024 {
        // The next marker. Stray bytes before it are skipped (as kamadak-exif
        // does), within reason; then fill bytes, then the marker code.
        let mut stray = Vec::new();
        r.by_ref().take(64 * 1024).read_until(0xFF, &mut stray).ok()?;
        if stray.last() != Some(&0xFF) {
            return None;
        }
        let mut code = 0xFF;
        while code == 0xFF {
            r.read_exact(&mut b[..1]).ok()?;
            code = b[0];
        }
        match code {
            // Start of scan, end of image, a second start of image.
            0xDA | 0xD9 | 0xD8 => return None,
            // Markers without a length.
            0x00 | 0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        r.read_exact(&mut b).ok()?;
        let len = (u16::from_be_bytes(b) as usize).checked_sub(2)?;
        if code == 0xE1 && len >= 6 {
            let mut seg = read_len(r, len)?;
            if seg.starts_with(b"Exif\0\0") {
                seg.drain(..6);
                return Some(seg);
            }
        } else {
            skip(r, len as u64)?;
        }
    }
    None
}

/// The EXIF chunk's TIFF structure, without the "Exif\0\0" some writers put
/// in front of it (PNG eXIf, WebP EXIF).
fn strip_exif_id(mut v: Vec<u8>) -> Vec<u8> {
    if v.starts_with(b"Exif\0\0") {
        v.drain(..6);
    }
    v
}

/// A PNG's eXIf chunk, looked for up to the image data (the first IDAT; the
/// PNG specification puts it before).
fn png_exif<R: std::io::Read + std::io::Seek>(r: &mut std::io::BufReader<R>) -> Option<Vec<u8>> {
    use std::io::Read;
    skip(r, 8)?;
    loop {
        let mut h = [0u8; 8];
        r.read_exact(&mut h).ok()?;
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        match &h[4..8] {
            b"eXIf" if len <= TIFF_EXIF_CAP => return read_len(r, len).map(strip_exif_id),
            b"IDAT" | b"IEND" | b"eXIf" => return None,
            _ => skip(r, len as u64 + 4)?,
        }
    }
}

/// A WebP's EXIF chunk, found from the chunk headers (the image data between
/// them is skipped, the chunk usually comes after it).
fn webp_exif<R: std::io::Read + std::io::Seek>(r: &mut std::io::BufReader<R>) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut h = [0u8; 12];
    r.read_exact(&mut h).ok()?;
    let mut left = u32::from_le_bytes([h[4], h[5], h[6], h[7]]).checked_sub(4)? as u64;
    let mut first = true;
    while left >= 8 {
        let mut c = [0u8; 8];
        r.read_exact(&mut c).ok()?;
        let size = u32::from_le_bytes([c[4], c[5], c[6], c[7]]) as u64;
        let padded = size + (size & 1);
        left = left.checked_sub(8 + size)?.saturating_sub(size & 1);
        match &c[..4] {
            b"EXIF" if size as usize <= TIFF_EXIF_CAP => return read_len(r, size as usize).map(strip_exif_id),
            b"EXIF" => return None,
            // The extended header says whether there is an EXIF chunk (the
            // simple formats have none).
            b"VP8X" if first && size >= 1 => {
                let mut flags = [0u8; 1];
                r.read_exact(&mut flags).ok()?;
                if flags[0] & 0x08 == 0 {
                    return None;
                }
                skip(r, padded - 1)?;
            }
            _ if first => return None,
            _ => skip(r, padded)?,
        }
        first = false;
    }
    None
}

/// Reads ranges of a file, serving the ones inside its first block from memory
/// (directories and tag values usually sit right after the header).
struct RangeReader<'a, R> {
    src: &'a mut R,
    head: Vec<u8>,
}

impl<R: std::io::Read + std::io::Seek> RangeReader<'_, R> {
    fn read(&mut self, at: u64, len: usize) -> Option<Vec<u8>> {
        use std::io::{Read, Seek};
        let end = at.checked_add(len as u64)?;
        if end <= self.head.len() as u64 {
            return Some(self.head[at as usize..end as usize].to_vec());
        }
        self.src.seek(std::io::SeekFrom::Start(at)).ok()?;
        let mut buf = vec![0u8; len];
        self.src.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

/// A copy of what kamadak-exif parses in a TIFF (the chain of image directories,
/// the EXIF, GPS and interoperability directories, and the out-of-line values of
/// their tags) packed into a small TIFF with the offsets rewritten. Image data
/// is not read, and neither is anything that would take more than `cap` bytes:
/// what kamadak-exif would refuse or what cannot be read is left out (the
/// directories after the eighth, values over the cap, odd pointers, entries
/// pointing past the end of the file), so the file is never read whole.
/// Ok(None) when the file is not a TIFF or its first directory is unreadable.
fn compact_tiff<R: std::io::Read + std::io::Seek>(src: &mut R, cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut head = Vec::new();
    src.by_ref().take(256 * 1024).read_to_end(&mut head)?;
    if head.len() < 8 {
        return Ok(None);
    }
    let le = match &head[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return Ok(None),
    };
    let mut r = RangeReader { src, head };
    let mut c = TiffCompactor { le, out: Vec::with_capacity(64 * 1024), cap, dirs: 0 };
    c.out.extend_from_slice(&r.head[..4]);
    c.out.extend_from_slice(&[0; 4]);
    let first = c.u32_at(&r.head[4..8]);
    let first = if first == 0 { Some(0) } else { c.copy_chain(&mut r, first as u64) };
    match first {
        Some(at) => {
            c.put_u32(4, at);
            Ok(Some(c.out))
        }
        None => Ok(None),
    }
}

/// Directory contexts, as kamadak-exif follows them.
#[derive(Clone, Copy, PartialEq)]
enum IfdKind {
    Tiff,
    Exif,
    Gps,
    Interop,
}

/// kamadak-exif reads at most 8 directories in the main chain.
const TIFF_MAX_CHAIN: usize = 8;
/// Directories copied in all: the chain and their EXIF, GPS and
/// interoperability directories (more are repeated pointers).
const TIFF_MAX_DIRS: usize = 4 * TIFF_MAX_CHAIN;

struct TiffCompactor {
    le: bool,
    out: Vec<u8>,
    cap: usize,
    /// Directories copied so far.
    dirs: usize,
}

impl TiffCompactor {
    fn u16_at(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }

    fn u32_at(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }

    fn put_u16(&mut self, at: usize, v: u16) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.out[at..at + 2].copy_from_slice(&b);
    }

    fn put_u32(&mut self, at: usize, v: u32) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.out[at..at + 4].copy_from_slice(&b);
    }

    /// Whether `len` more bytes (word aligned) still fit the cap.
    fn fits(&self, len: u64) -> bool {
        (self.out.len() as u64 + 1).saturating_add(len) <= self.cap as u64
    }

    /// Append `bytes` (word aligned, as TIFF wants); its offset in the copy.
    fn append(&mut self, bytes: &[u8]) -> Option<u32> {
        if self.out.len() % 2 == 1 {
            self.out.push(0);
        }
        if self.out.len() + bytes.len() > self.cap {
            return None;
        }
        let at = self.out.len();
        self.out.extend_from_slice(bytes);
        u32::try_from(at).ok()
    }

    /// Copy the main directory chain starting at `ofs` (cut after the eighth
    /// directory, or before one that cannot be read); the copy's first offset.
    fn copy_chain<R: std::io::Read + std::io::Seek>(&mut self, r: &mut RangeReader<'_, R>, mut ofs: u64) -> Option<u32> {
        let mut first = None;
        let mut prev_next: Option<usize> = None;
        for _ in 0..TIFF_MAX_CHAIN {
            if ofs == 0 {
                break;
            }
            let Some((at, next_at, next)) = self.copy_ifd(r, ofs, IfdKind::Tiff) else { break };
            match prev_next {
                Some(p) => self.put_u32(p, at),
                None => first = Some(at),
            }
            prev_next = Some(next_at);
            ofs = next as u64;
        }
        first
    }

    /// Copy one directory at `ofs` in the source and everything it points to,
    /// leaving out the entries that cannot be copied. Returns its offset in the
    /// copy, where its next-directory offset sits (written as 0) and the
    /// source's next-directory offset.
    fn copy_ifd<R: std::io::Read + std::io::Seek>(&mut self, r: &mut RangeReader<'_, R>, ofs: u64, kind: IfdKind) -> Option<(u32, usize, u32)> {
        if self.dirs >= TIFF_MAX_DIRS {
            return None;
        }
        self.dirs += 1;
        let count = self.u16_at(&r.read(ofs, 2)?) as usize;
        let len = 2 + 12 * count + 4;
        if !self.fits(len as u64) {
            return None;
        }
        let mut table = r.read(ofs, len)?;
        let next = self.u32_at(&table[2 + 12 * count..]);
        table[len - 4..].fill(0);
        let at = self.append(&table)? as usize;
        // Entries kept are moved up over the ones left out.
        let mut kept = 0;
        for i in 0..count {
            let e = at + 2 + 12 * i;
            let mut entry = [0u8; 12];
            entry.copy_from_slice(&self.out[e..e + 12]);
            if self.copy_entry(r, &mut entry, kind) {
                let k = at + 2 + 12 * kept;
                self.out[k..k + 12].copy_from_slice(&entry);
                kept += 1;
            }
        }
        let next_at = at + 2 + 12 * kept;
        self.put_u16(at, kept as u16);
        self.out[next_at..next_at + 4].fill(0);
        Some((at as u32, next_at, next))
    }

    /// Copy what the directory entry `entry` points to and point it at the
    /// copy. False when the entry is left out.
    fn copy_entry<R: std::io::Read + std::io::Seek>(&mut self, r: &mut RangeReader<'_, R>, entry: &mut [u8; 12], kind: IfdKind) -> bool {
        let tag = self.u16_at(&entry[0..2]);
        let typ = self.u16_at(&entry[2..4]);
        let cnt = self.u32_at(&entry[4..8]);
        let target = self.u32_at(&entry[8..12]);
        let child = match (kind, tag) {
            (IfdKind::Tiff, 0x8769) => Some(IfdKind::Exif),
            (IfdKind::Tiff, 0x8825) => Some(IfdKind::Gps),
            (IfdKind::Exif, 0xA005) => Some(IfdKind::Interop),
            _ => None,
        };
        let at = if let Some(child) = child {
            // A pointer is one LONG (kamadak-exif refuses anything else). The
            // copied directory has no next one (which kamadak-exif refuses too).
            if typ != 4 || cnt != 1 {
                return false;
            }
            match self.copy_ifd(r, target as u64, child) {
                Some((at, _, _)) => at,
                None => return false,
            }
        } else {
            let unit: u64 = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => 0,
            };
            let vlen = unit * cnt as u64;
            if vlen <= 4 {
                return true;
            }
            if !self.fits(vlen) {
                return false;
            }
            match r.read(target as u64, vlen as usize).and_then(|v| self.append(&v)) {
                Some(at) => at,
                None => return false,
            }
        };
        let b = if self.le { at.to_le_bytes() } else { at.to_be_bytes() };
        entry[8..12].copy_from_slice(&b);
        true
    }
}

pub fn read_exif_bytes(raw: &[u8]) -> Option<ExifSummary> {
    let exif = exif::Reader::new().read_raw(raw.to_vec()).ok()?;
    Some(summarize(&exif))
}

fn summarize(exif: &exif::Exif) -> ExifSummary {
    use exif::{In, Tag};
    let get = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY)
            .and_then(|f| clean(f.display_value().with_unit(exif).to_string()))
    };
    let mut s = ExifSummary::default();
    let make = get(Tag::Make);
    let model = get(Tag::Model);
    s.camera = match (make, model) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => Some(md),
        (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
        (a, b) => a.or(b),
    };
    s.lens = get(Tag::LensModel).or_else(|| get(Tag::LensMake));
    s.exposure = get(Tag::ExposureTime);
    s.aperture = get(Tag::FNumber);
    s.iso = get(Tag::PhotographicSensitivity).map(|v| format!("ISO {v}"));
    s.focal = get(Tag::FocalLength);
    if let Some(f35) = get(Tag::FocalLengthIn35mmFilm) {
        s.focal = Some(match s.focal.take() {
            Some(f) => format!("{f} ({f35} eq.)"),
            None => format!("{f35} (35mm eq.)"),
        });
    }
    s.flash = get(Tag::Flash);
    s.software = get(Tag::Software);
    s.artist = get(Tag::Artist);
    s.copyright = get(Tag::Copyright);
    s.description = get(Tag::ImageDescription);
    s.orientation = exif.get_field(Tag::Orientation, In::PRIMARY).and_then(|f| f.value.get_uint(0));
    if let Some((text, ts)) = taken(exif) {
        s.taken = Some(text);
        s.taken_ts = ts;
    }
    s.gps = gps(exif);
    for f in exif.fields() {
        let group = match f.ifd_num {
            In::PRIMARY => match f.tag.context() {
                exif::Context::Gps => "GPS",
                exif::Context::Exif => "EXIF",
                exif::Context::Interop => "Interop",
                _ => "Image",
            },
            _ => "Thumbnail",
        };
        if matches!(f.tag, Tag::MakerNote) {
            continue;
        }
        let mut val = f.display_value().with_unit(exif).to_string();
        if val.len() > 200 {
            val.truncate(200);
            val.push_str("...");
        }
        s.all.push((group.to_string(), f.tag.to_string(), val));
    }
    s
}

/// Capture date as display text and Unix seconds (DateTimeOriginal, then
/// DateTimeDigitized, then DateTime).
fn taken(exif: &exif::Exif) -> Option<(String, Option<i64>)> {
    use exif::{In, Tag};
    let f = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTimeDigitized, In::PRIMARY))
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))?;
    let exif::Value::Ascii(ref v) = f.value else { return None };
    let dt = exif::DateTime::from_ascii(v.first()?).ok()?;
    let text = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second);
    let ts = chrono::NaiveDate::from_ymd_opt(dt.year as i32, dt.month as u32, dt.day as u32)
        .and_then(|d| d.and_hms_opt(dt.hour as u32, dt.minute as u32, dt.second as u32))
        .map(|n| n.and_utc().timestamp());
    Some((text, ts))
}

/// Just the capture date of a file (Unix seconds), without summarizing every tag.
pub fn read_taken(path: &Path) -> Option<i64> {
    let exif = exif_from_file(path)?;
    taken(&exif)?.1
}

fn gps(exif: &exif::Exif) -> Option<(f64, f64)> {
    use exif::{In, Tag, Value};
    let coord = |tag: Tag, rtag: Tag| -> Option<f64> {
        let f = exif.get_field(tag, In::PRIMARY)?;
        let Value::Rational(ref v) = f.value else { return None };
        if v.len() < 3 {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let r = exif.get_field(rtag, In::PRIMARY)?.display_value().to_string();
        Some(if r.contains('S') || r.contains('W') { -deg } else { deg })
    };
    let lat = coord(Tag::GPSLatitude, Tag::GPSLatitudeRef)?;
    let lon = coord(Tag::GPSLongitude, Tag::GPSLongitudeRef)?;
    if lat.is_finite() && lon.is_finite() && !(lat == 0.0 && lon == 0.0) {
        Some((lat, lon))
    } else {
        None
    }
}

/// Background service that lazily fills in dimensions and capture dates
/// (needed for the details view and sorting by dimensions / date taken).
#[derive(Clone, Copy, Default)]
pub struct QuickMeta {
    pub dims: Option<(u32, u32)>,
    pub taken: Option<i64>,
}

/// The dimensions (as shown, after EXIF orientation) and capture date of the
/// file at `path`, reading its EXIF once for both.
pub fn quick_meta(path: &Path) -> QuickMeta {
    let exif = exif_from_file(path);
    let orientation = exif.as_ref().map_or(1, crate::decode::orientation_of);
    QuickMeta {
        dims: crate::decode::probe_dimensions_with_orientation(path, orientation),
        // Only the date: summarizing every tag as text is several times slower.
        taken: exif.as_ref().and_then(|e| taken(e)?.1),
    }
}

struct MetaJob {
    path: PathBuf,
    /// Request sequence number (higher = asked for more recently), shared with
    /// the service's `queued` entry so a re-request bumps it without the lock.
    seq: Arc<AtomicU64>,
}

struct MetaShared {
    jobs: Mutex<Vec<MetaJob>>,
    cv: Condvar,
    /// Jobs whose sequence number is at least this were asked for during the
    /// last two frames: they are on screen (see `MetaService::poll`).
    hot_from: AtomicU64,
    /// When a worker last asked for a repaint.
    last_repaint: Mutex<Instant>,
}

/// Results for files not on screen ask for a repaint at most this often.
const REPAINT_EVERY: Duration = Duration::from_millis(250);

pub struct MetaService {
    shared: Arc<MetaShared>,
    rx: Receiver<(PathBuf, QuickMeta)>,
    pub map: HashMap<PathBuf, QuickMeta>,
    requested: HashSet<PathBuf>,
    /// Requested and not finished: the sequence number shared with the job.
    queued: HashMap<PathBuf, Arc<AtomicU64>>,
    seq: u64,
    /// `seq` at the previous `poll`.
    polled_seq: u64,
}

/// Index of the most recently requested job.
fn newest_job(jobs: &[MetaJob]) -> Option<usize> {
    jobs.iter().enumerate().max_by_key(|(_, j)| j.seq.load(Ordering::Relaxed)).map(|(i, _)| i)
}

fn meta_worker(s: Arc<MetaShared>, tx: Sender<(PathBuf, QuickMeta)>, repaint: impl Fn()) {
    loop {
        let job = {
            let mut jobs = s.jobs.lock();
            loop {
                if let Some(i) = newest_job(&jobs) {
                    break jobs.swap_remove(i);
                }
                s.cv.wait(&mut jobs);
            }
        };
        let hot = job.seq.load(Ordering::Relaxed) >= s.hot_from.load(Ordering::Relaxed);
        let p = job.path;
        // A decoder or EXIF parser panicking on a hostile file must not stop
        // this worker for good (the rest would never be read).
        let meta = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| quick_meta(&p))).unwrap_or_default();
        if tx.send((p, meta)).is_err() {
            return;
        }
        // On-screen results and the last one repaint at once; the rest (a whole
        // folder read for sorting) a few times per second.
        let idle = s.jobs.lock().is_empty();
        let mut last = s.last_repaint.lock();
        if hot || idle || last.elapsed() >= REPAINT_EVERY {
            *last = Instant::now();
            drop(last);
            repaint();
        }
    }
}

impl MetaService {
    /// `repaint` is called from the worker threads when results are ready
    /// (the app passes a closure that requests a UI repaint).
    pub fn new(repaint: impl Fn() + Clone + Send + 'static) -> Self {
        let shared = Arc::new(MetaShared { jobs: Mutex::new(Vec::new()), cv: Condvar::new(), hot_from: AtomicU64::new(0), last_repaint: Mutex::new(Instant::now()) });
        let (rtx, rx) = unbounded();
        for i in 0..2 {
            let s = shared.clone();
            let rtx = rtx.clone();
            let repaint = repaint.clone();
            let _ = std::thread::Builder::new().name(format!("lumen-meta-{i}")).spawn(move || meta_worker(s, rtx, repaint));
        }
        MetaService { shared, rx, map: HashMap::new(), requested: HashSet::new(), queued: HashMap::new(), seq: 0, polled_seq: 0 }
    }

    /// Call once per frame: collects finished results (true when there were any).
    pub fn poll(&mut self) -> bool {
        // What was asked for since the previous frame began is on screen.
        self.shared.hot_from.store(self.polled_seq, Ordering::Relaxed);
        self.polled_seq = self.seq;
        let mut any = false;
        while let Ok((p, m)) = self.rx.try_recv() {
            self.queued.remove(&p);
            self.map.insert(p, m);
            any = true;
        }
        any
    }

    /// Read `path`'s size and date in the background. Files asked for most
    /// recently go first, so asking again every frame for what is on screen
    /// puts it ahead of a whole folder requested for sorting.
    pub fn request(&mut self, path: &Path) {
        self.seq += 1;
        if let Some(seq) = self.queued.get(path) {
            seq.store(self.seq, Ordering::Relaxed);
            return;
        }
        if self.requested.contains(path) {
            return;
        }
        self.requested.insert(path.to_path_buf());
        let seq = Arc::new(AtomicU64::new(self.seq));
        self.queued.insert(path.to_path_buf(), seq.clone());
        self.shared.jobs.lock().push(MetaJob { path: path.to_path_buf(), seq });
        self.shared.cv.notify_one();
    }

    pub fn get(&self, path: &Path) -> Option<QuickMeta> {
        self.map.get(path).copied()
    }

    pub fn pending(&self) -> usize {
        self.requested.len().saturating_sub(self.map.len())
    }

    pub fn forget(&mut self, path: &Path) {
        self.map.remove(path);
        self.requested.remove(path);
        self.queued.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type IfdEntry = (u16, u16, u32, Vec<u8>);

    /// A camera-like EXIF blob: `n0` ASCII tags in IFD0, then an EXIF IFD with
    /// DateTimeOriginal and `n1` rational tags.
    fn exif_blob(n0: u16, n1: u16) -> Vec<u8> {
        let ifd0: Vec<IfdEntry> = (0..n0).map(|i| (0xE000 + i, 2u16, 24u32, format!("Value number {i:>10}\0").into_bytes())).collect();
        let mut ifd1: Vec<IfdEntry> = vec![(0x9003, 2, 20, b"2021:06:15 10:20:30\0".to_vec())];
        ifd1.extend((0..n1).map(|i| (0xE100 + i, 5u16, 1u32, [(i as u32 + 1).to_le_bytes(), 7u32.to_le_bytes()].concat())));
        let size = |n: usize| 2 + 12 * n + 4;
        let ifd0_at = 8usize;
        let data0_at = ifd0_at + size(ifd0.len() + 1);
        let data0_len: usize = ifd0.iter().map(|e| e.3.len()).sum();
        let ifd1_at = data0_at + data0_len;
        let data1_at = ifd1_at + size(ifd1.len());
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&(ifd0_at as u32).to_le_bytes());
        let write_ifd = |v: &mut Vec<u8>, entries: &[IfdEntry], mut data_at: usize, exif_ptr: Option<u32>| {
            v.extend_from_slice(&((entries.len() + exif_ptr.is_some() as usize) as u16).to_le_bytes());
            for (tag, ty, n, payload) in entries {
                v.extend_from_slice(&tag.to_le_bytes());
                v.extend_from_slice(&ty.to_le_bytes());
                v.extend_from_slice(&n.to_le_bytes());
                v.extend_from_slice(&(data_at as u32).to_le_bytes());
                data_at += payload.len();
            }
            if let Some(p) = exif_ptr {
                v.extend_from_slice(&0x8769u16.to_le_bytes());
                v.extend_from_slice(&4u16.to_le_bytes());
                v.extend_from_slice(&1u32.to_le_bytes());
                v.extend_from_slice(&p.to_le_bytes());
            }
            v.extend_from_slice(&0u32.to_le_bytes());
            for e in entries {
                v.extend_from_slice(&e.3);
            }
        };
        write_ifd(&mut v, &ifd0, data0_at, Some(ifd1_at as u32));
        assert_eq!(v.len(), ifd1_at);
        write_ifd(&mut v, &ifd1, data1_at, None);
        v
    }

    fn jpeg_with_exif() -> Vec<u8> {
        let rgb = vec![120u8; 64 * 48 * 3];
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
        enc.add_exif_metadata(&exif_blob(40, 40)).unwrap();
        enc.encode(&rgb, 64, 48, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    }

    fn temp_jpeg(tag: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("lumen-meta-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.jpg");
        std::fs::write(&p, jpeg_with_exif()).unwrap();
        (d, p)
    }

    #[test]
    fn read_taken_matches_summary() {
        let (d, p) = temp_jpeg("taken");
        let s = read_exif(&p).unwrap();
        assert!(s.all.len() > 40, "{}", s.all.len());
        assert_eq!(s.taken.as_deref(), Some("2021-06-15 10:20:30"));
        assert!(s.taken_ts.is_some());
        assert_eq!(read_taken(&p), s.taken_ts);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Append a little-endian IFD with `entries` (tag, type, count, value) and
    /// their out-of-line values; returns its offset and where each entry's
    /// value field and the next-IFD offset are.
    fn put_ifd(v: &mut Vec<u8>, entries: &[(u16, u16, u32, Vec<u8>)]) -> (u32, Vec<usize>, usize) {
        let at = v.len();
        let n = entries.len();
        v.extend_from_slice(&(n as u16).to_le_bytes());
        let mut data_at = at + 2 + 12 * n + 4;
        let mut fields = Vec::new();
        for (tag, typ, cnt, val) in entries {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&typ.to_le_bytes());
            v.extend_from_slice(&cnt.to_le_bytes());
            fields.push(v.len());
            if val.len() <= 4 {
                let mut b = val.clone();
                b.resize(4, 0);
                v.extend_from_slice(&b);
            } else {
                v.extend_from_slice(&(data_at as u32).to_le_bytes());
                data_at += val.len();
            }
        }
        let next_at = v.len();
        v.extend_from_slice(&0u32.to_le_bytes());
        for (_, _, _, val) in entries {
            if val.len() > 4 {
                v.extend_from_slice(val);
            }
        }
        (at as u32, fields, next_at)
    }

    fn patch(v: &mut [u8], at: usize, value: u32) {
        v[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn rational(n: u32, d: u32) -> Vec<u8> {
        [n.to_le_bytes(), d.to_le_bytes()].concat()
    }

    /// A camera-like TIFF whose directories all sit after `junk` bytes of image
    /// data: IFD0 -> EXIF (-> interoperability) and GPS, then IFD1.
    fn tiff_with_ifds_at_end(junk: usize) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend((0..junk).map(|i| (i * 31) as u8));
        let (ifd0, f0, next0) = put_ifd(
            &mut v,
            &[
                (0x010F, 2, 6, b"Nikon\0".to_vec()),
                (0x0110, 2, 10, b"NIKON Z 6\0".to_vec()),
                (0x0111, 4, 3, [7u32.to_le_bytes(), 8u32.to_le_bytes(), 9u32.to_le_bytes()].concat()),
                (0x0112, 3, 1, 6u16.to_le_bytes().to_vec()),
                (0x0132, 2, 20, b"2020:01:02 03:04:05\0".to_vec()),
                (0x8769, 4, 1, vec![0; 4]),
                (0x8825, 4, 1, vec![0; 4]),
            ],
        );
        patch(&mut v, 4, ifd0);
        let (exif_at, fe, _) = put_ifd(
            &mut v,
            &[(0x829A, 5, 1, rational(1, 250)), (0x9003, 2, 20, b"2021:06:15 10:20:30\0".to_vec()), (0xA005, 4, 1, vec![0; 4])],
        );
        patch(&mut v, f0[5], exif_at);
        let (interop, _, _) = put_ifd(&mut v, &[(0x0001, 2, 4, b"R98\0".to_vec())]);
        patch(&mut v, fe[2], interop);
        let (gps, _, _) = put_ifd(&mut v, &[(0x0001, 2, 2, b"N\0".to_vec()), (0x0002, 5, 3, [rational(48, 1), rational(51, 1), rational(30, 1)].concat())]);
        patch(&mut v, f0[6], gps);
        let (ifd1, _, _) = put_ifd(&mut v, &[(0x0201, 4, 1, 1234u32.to_le_bytes().to_vec()), (0x0202, 4, 1, 99u32.to_le_bytes().to_vec())]);
        patch(&mut v, next0, ifd1);
        v
    }

    /// Counts the bytes read through it.
    struct Counting<R> {
        inner: R,
        read: usize,
    }

    impl<R: std::io::Read> std::io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    impl<R: std::io::Seek> std::io::Seek for Counting<R> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn fields(e: &exif::Exif) -> Vec<String> {
        e.fields().map(|f| format!("{} {} {}", f.ifd_num, f.tag, f.display_value())).collect()
    }

    #[test]
    fn tiff_exif_reads_only_the_directories() {
        let tiff = tiff_with_ifds_at_end(3 << 20);
        let full = exif::Reader::new().read_raw(tiff.clone()).unwrap();
        let mut src = Counting { inner: std::io::Cursor::new(tiff.clone()), read: 0 };
        let compact = compact_tiff(&mut src, TIFF_EXIF_CAP).unwrap().expect("plain TIFF");
        assert!(src.read < 300 * 1024, "read {} bytes", src.read);
        assert!(compact.len() < 1024, "{}", compact.len());
        let small = exif::Reader::new().read_raw(compact).unwrap();
        assert_eq!(fields(&full), fields(&small));
        assert!(fields(&full).len() >= 12, "{:?}", fields(&full));
        assert_eq!(taken(&small).unwrap().0, "2021-06-15 10:20:30");
        // The same through a file, as the metadata service reads it.
        let d = std::env::temp_dir().join(format!("lumen-meta-tiff-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.nef");
        std::fs::write(&p, &tiff).unwrap();
        assert_eq!(read_taken(&p), taken(&full).unwrap().1);
        assert_eq!(crate::decode::exif_orientation_from_file(&p), 6);
        assert_eq!(read_exif(&p).unwrap().camera.as_deref(), Some("NIKON Z 6"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unusual_tiffs_are_cut_instead_of_read_whole() {
        let compact = |v: &[u8], cap: usize| compact_tiff(&mut std::io::Cursor::new(v.to_vec()), cap).unwrap();
        let parse = |c: Vec<u8>| fields(&exif::Reader::new().read_raw(c).unwrap());
        let good = tiff_with_ifds_at_end(100);
        let all = parse(good.clone());
        let ifd0 = u32::from_le_bytes(good[4..8].try_into().unwrap()) as usize;
        // A value pointing past the end of the file: kamadak-exif refuses the
        // whole file, the copy leaves that entry (Make) out.
        let mut bad = good.clone();
        patch(&mut bad, ifd0 + 2 + 8, u32::MAX - 16);
        assert!(exif::Reader::new().read_raw(bad.clone()).is_err());
        let f = parse(compact(&bad, TIFF_EXIF_CAP).unwrap());
        assert_eq!(f.len(), all.len() - 1, "{f:?}");
        assert!(f.iter().all(|x| all.contains(x) && !x.contains("Make")), "{f:?}");
        // More than the cap: what fits is kept, the copy stays within the cap.
        let c = compact(&good, 200).unwrap();
        assert!(c.len() <= 200, "{}", c.len());
        let f = parse(c);
        assert!(!f.is_empty() && f.len() < all.len() && f.iter().all(|x| all.contains(x)), "{f:?}");
        // An odd EXIF pointer (two LONGs): the EXIF directory is left out.
        let mut odd = good.clone();
        patch(&mut odd, ifd0 + 2 + 12 * 5 + 4, 2);
        let f = parse(compact(&odd, TIFF_EXIF_CAP).unwrap());
        assert!(f.iter().all(|x| all.contains(x)) && !f.iter().any(|x| x.contains("DateTimeOriginal")), "{f:?}");
        assert!(f.iter().any(|x| x.contains("GPSLatitude")), "{f:?}");
        // Nine directories (kamadak-exif refuses more than eight): cut at eight.
        let mut nine = b"II*\0".to_vec();
        nine.extend_from_slice(&8u32.to_le_bytes());
        let mut prev_next = None;
        for i in 0..9u16 {
            let (at, _, next_at) = put_ifd(&mut nine, &[(0x0100, 3, 1, (100 + i).to_le_bytes().to_vec())]);
            if let Some(p) = prev_next {
                patch(&mut nine, p, at);
            }
            prev_next = Some(next_at);
        }
        assert!(exif::Reader::new().read_raw(nine.clone()).is_err());
        let f = parse(compact(&nine, TIFF_EXIF_CAP).unwrap());
        assert_eq!(f.len(), 8, "{f:?}");
        assert!(f[7].contains("107"), "{f:?}");
        // A directory chain pointing back at itself ends too.
        let mut looped = nine.clone();
        patch(&mut looped, 8 + 2 + 12, 8);
        assert_eq!(parse(compact(&looped, TIFF_EXIF_CAP).unwrap()).len(), 8);
        // Not a TIFF, or its first directory unreadable.
        assert!(compact(b"\xFF\xD8\xFF\xE0 not a tiff", TIFF_EXIF_CAP).is_none());
        assert!(compact(b"II*\0\x00\x10\x00\x00", TIFF_EXIF_CAP).is_none());
        // No directories at all.
        let c = compact(b"II*\0\0\0\0\0", TIFF_EXIF_CAP).unwrap();
        assert_eq!(exif::Reader::new().read_raw(c).unwrap().fields().count(), 0);
    }

    /// `exif_from_reader` over `data`, and how many bytes it read.
    fn exif_and_bytes_read(data: Vec<u8>) -> (Option<exif::Exif>, usize) {
        let mut r = std::io::BufReader::with_capacity(16 * 1024, Counting { inner: std::io::Cursor::new(data), read: 0 });
        let e = exif_from_reader(&mut r);
        (e, r.get_ref().read)
    }

    /// A `kind` chunk of a PNG (the CRC is not checked).
    fn png_chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        [&(data.len() as u32).to_be_bytes()[..], kind, data, &[0u8; 4][..]].concat()
    }

    /// A RIFF chunk of a WebP.
    fn riff_chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = [kind, &(data.len() as u32).to_le_bytes()[..], data].concat();
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn webp(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body = chunks.concat();
        [&b"RIFF"[..], &(body.len() as u32 + 4).to_le_bytes()[..], &b"WEBP"[..], &body[..]].concat()
    }

    #[test]
    fn containers_are_read_only_up_to_their_exif() {
        let blob = exif_blob(3, 3);
        let date = |e: Option<exif::Exif>| e.and_then(|e| taken(&e)).map(|t| t.0);
        // JPEG: the APP1 segment; without one, the scan stops at the image data
        // (4 MB of it here, which kamadak-exif reads through).
        assert_eq!(date(exif_and_bytes_read(jpeg_with_exif()).0).as_deref(), Some("2021-06-15 10:20:30"));
        let mut jpg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpg, 90).encode(&vec![120u8; 64 * 48 * 3], 64, 48, jpeg_encoder::ColorType::Rgb).unwrap();
        let eoi = jpg.split_off(jpg.len() - 2);
        jpg.extend_from_slice(&vec![0u8; 4 << 20]);
        jpg.extend_from_slice(&eoi);
        let (e, read) = exif_and_bytes_read(jpg);
        assert!(e.is_none());
        assert!(read <= 64 * 1024, "read {read} bytes");
        // PNG: the eXIf chunk before the image data (with or without the
        // "Exif" prefix some writers add); none: stop at the first IDAT.
        let sig = b"\x89PNG\r\n\x1a\n".to_vec();
        let ihdr = png_chunk(b"IHDR", &[0; 13]);
        let idat = png_chunk(b"IDAT", &vec![7; 4 << 20]);
        for data in [blob.clone(), [&b"Exif\0\0"[..], &blob[..]].concat()] {
            let png = [sig.clone(), ihdr.clone(), png_chunk(b"eXIf", &data), idat.clone(), png_chunk(b"IEND", &[])].concat();
            assert_eq!(date(exif_and_bytes_read(png).0).as_deref(), Some("2021-06-15 10:20:30"));
        }
        let (e, read) = exif_and_bytes_read([sig.clone(), ihdr.clone(), idat.clone(), png_chunk(b"eXIf", &blob), png_chunk(b"IEND", &[])].concat());
        assert!(e.is_none());
        assert!(read <= 64 * 1024, "read {read} bytes");
        // WebP: the EXIF chunk after the image data, found from the chunk
        // headers; none when the extended header says so or there is none.
        let vp8x = |flags: u8| riff_chunk(b"VP8X", &[flags, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let vp8 = riff_chunk(b"VP8 ", &vec![9; (4 << 20) + 1]);
        let (e, read) = exif_and_bytes_read(webp(&[vp8x(0x08), vp8.clone(), riff_chunk(b"EXIF", &blob)]));
        assert_eq!(date(e).as_deref(), Some("2021-06-15 10:20:30"));
        assert!(read <= 64 * 1024, "read {read} bytes");
        for file in [webp(&[vp8x(0), vp8.clone(), riff_chunk(b"EXIF", &blob)]), webp(&[vp8.clone(), riff_chunk(b"EXIF", &blob)])] {
            let (e, read) = exif_and_bytes_read(file);
            assert!(e.is_none());
            assert!(read <= 64 * 1024, "read {read} bytes");
        }
        // Anything else: nothing.
        assert!(exif_and_bytes_read(b"GIF89a not much".to_vec()).0.is_none());
        assert!(exif_and_bytes_read(Vec::new()).0.is_none());
    }

    fn idle_service() -> MetaService {
        let shared = Arc::new(MetaShared { jobs: Mutex::new(Vec::new()), cv: Condvar::new(), hot_from: AtomicU64::new(0), last_repaint: Mutex::new(Instant::now()) });
        let (_tx, rx) = unbounded();
        MetaService { shared, rx, map: HashMap::new(), requested: HashSet::new(), queued: HashMap::new(), seq: 0, polled_seq: 0 }
    }

    #[test]
    fn on_screen_requests_go_first() {
        let mut s = idle_service();
        let p = |i: usize| PathBuf::from(format!(r"C:\Photos\IMG_{i:04}.jpg"));
        // A whole folder for sorting ...
        for i in 0..100 {
            s.request(&p(i));
        }
        s.poll();
        s.poll();
        let hot = |s: &MetaService, i: usize| {
            let jobs = s.shared.jobs.lock();
            let j = jobs.iter().find(|j| j.path == p(i)).unwrap();
            j.seq.load(Ordering::Relaxed) >= s.shared.hot_from.load(Ordering::Relaxed)
        };
        assert!(!hot(&s, 3), "requested two frames ago");
        // ... then the rows on screen, asked for again every frame.
        s.request(&p(42));
        s.request(&p(7));
        {
            let jobs = s.shared.jobs.lock();
            assert_eq!(jobs.len(), 100, "re-requests do not queue twice");
            assert_eq!(jobs[newest_job(&jobs).unwrap()].path, p(7));
        }
        s.poll();
        assert!(hot(&s, 42) && hot(&s, 7));
        assert!(!hot(&s, 41));
        assert_eq!(s.pending(), 100);
    }

    /// `cargo test --release -p lumen-core -- --ignored --nocapture bench_perf_`
    #[test]
    #[ignore]
    fn bench_perf_meta() {
        let (d, p) = temp_jpeg("bench");
        let n = 500;
        let time = |f: &dyn Fn() -> Option<i64>| {
            let s = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(f());
            }
            s.elapsed().as_secs_f64() * 1e6 / n as f64
        };
        eprintln!("read_exif(..).taken_ts: {:.1} us per file", time(&|| read_exif(&p).and_then(|e| e.taken_ts)));
        eprintln!("read_taken: {:.1} us per file", time(&|| read_taken(&p)));
        eprintln!("probe_dimensions: {:.1} us per file", time(&|| crate::decode::probe_dimensions(&p).map(|d| d.0 as i64)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
