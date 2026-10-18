//! Folder listing, sorting and filtering for the browser.

use crate::formats;
use crate::settings::SortKey;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    Image,
    Video,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub ext: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub hidden: bool,
}

impl Entry {
    pub fn from_path(path: &Path) -> Option<Entry> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self::from_meta(path.to_path_buf(), &meta))
    }

    pub fn from_meta(path: PathBuf, meta: &std::fs::Metadata) -> Entry {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let ext = formats::ext_of(&path);
        let kind = if meta.is_dir() {
            EntryKind::Folder
        } else if crate::video::is_video_ext(&ext) {
            EntryKind::Video
        } else {
            EntryKind::Image
        };
        #[cfg(windows)]
        let hidden = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x2 != 0 || name.starts_with('.')
        };
        #[cfg(not(windows))]
        let hidden = name.starts_with('.');
        Entry {
            path,
            name,
            ext,
            kind,
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            hidden,
        }
    }

    pub fn is_folder(&self) -> bool {
        self.kind == EntryKind::Folder
    }
    pub fn is_media(&self) -> bool {
        self.kind != EntryKind::Folder
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaFilter {
    All,
    Images,
    Videos,
}

/// List a folder: subfolders plus supported image/video files.
pub fn list_folder(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let path = item.path();
        let Ok(meta) = item.metadata() else { continue };
        let is_dir = meta.is_dir();
        if !is_dir {
            let ext = formats::ext_of(&path);
            if !formats::is_image_ext(&ext) && !crate::video::is_video_ext(&ext) {
                continue;
            }
        }
        // Follow directory symlinks/junctions for the "is dir" test.
        let e = Entry::from_meta(path, &meta);
        if e.hidden && !show_hidden {
            continue;
        }
        out.push(e);
    }
    Ok(out)
}

/// Recursively collect media files under `dir` (used by slideshow / batch "include subfolders").
pub fn list_recursive(dir: &Path, limit: usize) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for item in rd.flatten() {
            let path = item.path();
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if formats::is_image_path(&path) {
                out.push(Entry::from_meta(path, &meta));
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Natural ordering: "img2" < "img10", case-insensitive.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                        na.push(c);
                        ai.next();
                    }
                    let mut nb = String::new();
                    while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                        nb.push(c);
                        bi.next();
                    }
                    let ta = na.trim_start_matches('0');
                    let tb = nb.trim_start_matches('0');
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let la = ca.to_lowercase().next().unwrap_or(ca);
                    let lb = cb.to_lowercase().next().unwrap_or(cb);
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

pub struct SortContext<'a> {
    pub dims: &'a dyn Fn(&Path) -> Option<(u32, u32)>,
    pub taken: &'a dyn Fn(&Path) -> Option<i64>,
    pub rating: &'a dyn Fn(&Path) -> u8,
}

pub fn sort_entries(entries: &mut [Entry], key: SortKey, desc: bool, folders_first: bool, cx: &SortContext) {
    use std::cmp::Ordering;
    entries.sort_by(|a, b| {
        if folders_first && a.is_folder() != b.is_folder() {
            return if a.is_folder() { Ordering::Less } else { Ordering::Greater };
        }
        let ord = match key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Created => a.created.cmp(&b.created),
            SortKey::Type => a.ext.cmp(&b.ext),
            SortKey::Dimensions => {
                let pa = (cx.dims)(&a.path).map(|(w, h)| w as u64 * h as u64);
                let pb = (cx.dims)(&b.path).map(|(w, h)| w as u64 * h as u64);
                pa.cmp(&pb)
            }
            SortKey::DateTaken => {
                let ta = (cx.taken)(&a.path).or_else(|| a.modified.map(systime_secs));
                let tb = (cx.taken)(&b.path).or_else(|| b.modified.map(systime_secs));
                ta.cmp(&tb)
            }
            SortKey::Rating => (cx.rating)(&a.path).cmp(&(cx.rating)(&b.path)),
        }
        .then_with(|| natural_cmp(&a.name, &b.name));
        if desc { ord.reverse() } else { ord }
    });
}

pub fn systime_secs(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Simple fuzzy-ish filter: every whitespace-separated term must appear in the name.
pub fn matches_filter(name: &str, filter: &str) -> bool {
    if filter.trim().is_empty() {
        return true;
    }
    let lname = name.to_lowercase();
    filter
        .to_lowercase()
        .split_whitespace()
        .all(|term| {
            if let Some(ext) = term.strip_prefix('.') {
                lname.ends_with(&format!(".{ext}"))
            } else {
                lname.contains(term)
            }
        })
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn natural_sort() {
        let mut v = vec!["img10.jpg", "img2.jpg", "IMG1.jpg", "img02.jpg", "a.png"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a.png", "IMG1.jpg", "img2.jpg", "img02.jpg", "img10.jpg"]);
    }
    #[test]
    fn filter_terms() {
        assert!(matches_filter("Holiday_Beach_01.JPG", "beach .jpg"));
        assert!(!matches_filter("Holiday_Beach_01.JPG", "beach .png"));
        assert!(matches_filter("x", "  "));
    }
    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(300 * 1024 * 1024), "300 MB");
    }
}
