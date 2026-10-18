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
    let dt_field = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTimeDigitized, In::PRIMARY))
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY));
    if let Some(f) = dt_field {
        if let exif::Value::Ascii(ref v) = f.value {
            if let Some(bytes) = v.first() {
                if let Ok(dt) = exif::DateTime::from_ascii(bytes) {
                    s.taken = Some(format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
                    ));
                    s.taken_ts = chrono::NaiveDate::from_ymd_opt(dt.year as i32, dt.month as u32, dt.day as u32)
                        .and_then(|d| d.and_hms_opt(dt.hour as u32, dt.minute as u32, dt.second as u32))
                        .map(|n| n.and_utc().timestamp());
                }
            }
        }
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
            val.push('…');
        }
        s.all.push((group.to_string(), f.tag.to_string(), val));
    }
    s
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
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, jrx) = unbounded::<PathBuf>();
        let (rtx, rx) = unbounded();
        for _ in 0..2 {
            let jrx = jrx.clone();
            let rtx = rtx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut n = 0u32;
                while let Ok(p) = jrx.recv() {
                    let dims = crate::decode::probe_dimensions(&p);
                    let taken = read_exif(&p).and_then(|e| e.taken_ts);
                    let _ = rtx.send((p, QuickMeta { dims, taken }));
                    n += 1;
                    if n % 32 == 0 || jrx.is_empty() {
                        ctx.request_repaint();
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
        if self.requested.insert(path.to_path_buf()) {
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
