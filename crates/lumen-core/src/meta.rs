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
/// HEIF, WebP). A TIFF is its own EXIF container, and kamadak-exif reads the
/// whole file for it (gigabytes for a big scan, over a network share too):
/// only the directories and the values they point to are read instead.
pub fn exif_from_file(path: &Path) -> Option<exif::Exif> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    (&mut file).take(4).read_to_end(&mut head).ok()?;
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        file.seek(std::io::SeekFrom::Start(0)).ok()?;
        if let Ok(Some(tiff)) = compact_tiff(&mut file, TIFF_EXIF_CAP) {
            if let Ok(exif) = exif::Reader::new().read_raw(tiff) {
                return Some(exif);
            }
        }
    }
    // Other containers are scanned only as far as their EXIF block; unusual
    // TIFFs (damaged, or with huge tags) are read whole as before.
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()
}

/// Most bytes of TIFF directories and tag values [`compact_tiff`] copies.
const TIFF_EXIF_CAP: usize = 16 << 20;

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
/// is not read. Ok(None) when the file is not a plain TIFF or anything is
/// unusual (truncated, an odd pointer, more than `cap` bytes): the caller then
/// lets kamadak-exif read the whole file, so the result is always the same.
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
    let mut c = TiffCompactor { le, out: Vec::with_capacity(64 * 1024), cap };
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

struct TiffCompactor {
    le: bool,
    out: Vec<u8>,
    cap: usize,
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

    fn put_u32(&mut self, at: usize, v: u32) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.out[at..at + 4].copy_from_slice(&b);
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

    /// Copy the main directory chain starting at `ofs`; the copy's first offset.
    fn copy_chain<R: std::io::Read + std::io::Seek>(&mut self, r: &mut RangeReader<'_, R>, mut ofs: u64) -> Option<u32> {
        let mut first = None;
        let mut prev_next: Option<usize> = None;
        let mut n = 0;
        while ofs != 0 {
            // kamadak-exif refuses more than 8 directories: let it see that itself.
            if n >= 8 {
                return None;
            }
            let (at, next_at, next) = self.copy_ifd(r, ofs, IfdKind::Tiff)?;
            match prev_next {
                Some(p) => self.put_u32(p, at),
                None => first = Some(at),
            }
            prev_next = Some(next_at);
            ofs = next as u64;
            n += 1;
        }
        first
    }

    /// Copy one directory at `ofs` in the source and everything it points to.
    /// Returns its offset in the copy, where its next-directory offset sits
    /// (written as 0) and the source's next-directory offset.
    fn copy_ifd<R: std::io::Read + std::io::Seek>(&mut self, r: &mut RangeReader<'_, R>, ofs: u64, kind: IfdKind) -> Option<(u32, usize, u32)> {
        let count = self.u16_at(&r.read(ofs, 2)?) as usize;
        let mut table = r.read(ofs, 2 + 12 * count + 4)?;
        let next = self.u32_at(&table[2 + 12 * count..]);
        let len = table.len();
        table[len - 4..].fill(0);
        let at = self.append(&table)?;
        for i in 0..count {
            let e = at as usize + 2 + 12 * i;
            let entry = self.out[e..e + 12].to_vec();
            let tag = self.u16_at(&entry[0..2]);
            let typ = self.u16_at(&entry[2..4]);
            let cnt = self.u32_at(&entry[4..8]);
            let child = match (kind, tag) {
                (IfdKind::Tiff, 0x8769) => Some(IfdKind::Exif),
                (IfdKind::Tiff, 0x8825) => Some(IfdKind::Gps),
                (IfdKind::Exif, 0xA005) => Some(IfdKind::Interop),
                _ => None,
            };
            if let Some(child) = child {
                // A pointer is one LONG; anything else is left to kamadak-exif.
                if typ != 4 || cnt != 1 {
                    return None;
                }
                let (child_at, _, child_next) = self.copy_ifd(r, self.u32_at(&entry[8..12]) as u64, child)?;
                if child_next != 0 {
                    return None;
                }
                self.put_u32(e + 8, child_at);
                continue;
            }
            let unit: u64 = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => 0,
            };
            let vlen = unit * cnt as u64;
            if vlen > 4 {
                if vlen > self.cap as u64 {
                    return None;
                }
                let value = r.read(self.u32_at(&entry[8..12]) as u64, vlen as usize)?;
                let vat = self.append(&value)?;
                self.put_u32(e + 8, vat);
            }
        }
        Some((at, at as usize + 2 + 12 * count, next))
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
        let meta = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| QuickMeta {
            dims: crate::decode::probe_dimensions(&p),
            // Only the date: summarizing every tag as text is several times slower.
            taken: read_taken(&p),
        }))
        .unwrap_or_default();
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
    fn unusual_tiffs_are_left_to_the_full_read() {
        let good = tiff_with_ifds_at_end(100);
        // A value pointing past the end of the file.
        let mut bad = good.clone();
        let ifd0 = u32::from_le_bytes(bad[4..8].try_into().unwrap()) as usize;
        patch(&mut bad, ifd0 + 2 + 8, u32::MAX - 16);
        assert!(compact_tiff(&mut std::io::Cursor::new(bad.clone()), TIFF_EXIF_CAP).unwrap().is_none());
        assert!(exif::Reader::new().read_raw(bad).is_err(), "kamadak-exif refuses it too");
        // More than the cap.
        assert!(compact_tiff(&mut std::io::Cursor::new(good.clone()), 64).unwrap().is_none());
        // Not a TIFF.
        assert!(compact_tiff(&mut std::io::Cursor::new(b"\xFF\xD8\xFF\xE0 not a tiff".to_vec()), TIFF_EXIF_CAP).unwrap().is_none());
        // No directories at all.
        let empty = b"II*\0\0\0\0\0".to_vec();
        let c = compact_tiff(&mut std::io::Cursor::new(empty), TIFF_EXIF_CAP).unwrap().unwrap();
        assert_eq!(exif::Reader::new().read_raw(c).unwrap().fields().count(), 0);
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
