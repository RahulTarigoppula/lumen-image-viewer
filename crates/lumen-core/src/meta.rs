//! EXIF / metadata extraction for the info panel and for sorting by date taken.

use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    Some(summarize(&exif))
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
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
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

pub struct MetaService {
    tx: Sender<PathBuf>,
    rx: Receiver<(PathBuf, QuickMeta)>,
    pub map: HashMap<PathBuf, QuickMeta>,
    requested: HashSet<PathBuf>,
    _queue: Arc<Mutex<()>>,
}

impl MetaService {
    /// `repaint` is called from the worker threads when a batch of results is
    /// ready (the app passes a closure that requests a UI repaint).
    pub fn new(repaint: impl Fn() + Clone + Send + 'static) -> Self {
        let (tx, jrx) = unbounded::<PathBuf>();
        let (rtx, rx) = unbounded();
        for _ in 0..2 {
            let jrx = jrx.clone();
            let rtx = rtx.clone();
            let repaint = repaint.clone();
            std::thread::spawn(move || {
                let mut n = 0u32;
                while let Ok(p) = jrx.recv() {
                    let dims = crate::decode::probe_dimensions(&p);
                    let taken = read_exif(&p).and_then(|e| e.taken_ts);
                    let _ = rtx.send((p, QuickMeta { dims, taken }));
                    n += 1;
                    if n % 32 == 0 || jrx.is_empty() {
                        repaint();
                    }
                }
            });
        }
        MetaService { tx, rx, map: HashMap::new(), requested: HashSet::new(), _queue: Arc::new(Mutex::new(())) }
    }

    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok((p, m)) = self.rx.try_recv() {
            self.map.insert(p, m);
            any = true;
        }
        any
    }

    pub fn request(&mut self, path: &Path) {
        if !self.requested.contains(path) && self.requested.insert(path.to_path_buf()) {
            let _ = self.tx.send(path.to_path_buf());
        }
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
