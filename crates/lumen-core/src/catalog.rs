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
            meta.file_attributes() & 0x2 != 0 || name.starts_with('.') || (meta.is_dir() && is_profile_noise(&path))
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

/// Folders of the user profile that Explorer never shows (it hides them
/// even when their Hidden attribute was cleared): treated as hidden.
const PROFILE_NOISE: &[&str] = &["AppData"];

/// True for a well-known noise folder directly in the user profile
/// (`%USERPROFILE%\AppData`). Dot-folders (`.cargo`, `.vscode`) are hidden by
/// their name already.
pub fn is_profile_noise(path: &Path) -> bool {
    std::env::var_os("USERPROFILE").is_some_and(|home| is_noise_in(path, Path::new(&home)))
}

fn is_noise_in(path: &Path, profile: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return false };
    let same = |a: &Path, b: &Path| a.to_string_lossy().trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']));
    same(parent, profile) && PROFILE_NOISE.iter().any(|n| name.to_string_lossy().eq_ignore_ascii_case(n))
}

/// List a folder: subfolders plus supported image/video files.
pub fn list_folder(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let path = item.path();
        let Ok(mut meta) = item.metadata() else { continue };
        // The link's own attributes: legacy junctions such as "Application
        // Data" or "My Documents" are hidden+system, their targets are not.
        #[cfg(windows)]
        let link_attrs = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes()
        };
        // DirEntry::metadata does not follow symlinks / junctions on Windows:
        // ask for the target so linked folders and files still show up.
        if meta.file_type().is_symlink() {
            match std::fs::metadata(&path) {
                Ok(m) => meta = m,
                Err(_) => continue,
            }
        }
        let is_dir = meta.is_dir();
        if !is_dir {
            let ext = formats::ext_of(&path);
            if !formats::is_image_ext(&ext) && !crate::video::is_video_ext(&ext) {
                continue;
            }
        }
        // Follow directory symlinks/junctions for the "is dir" test.
        #[allow(unused_mut)]
        let mut e = Entry::from_meta(path, &meta);
        #[cfg(windows)]
        {
            // Protected (hidden + system) items stay out even with Show hidden
            // files, as in Explorer ($Recycle.Bin, "Application Data", ...).
            use std::os::windows::fs::MetadataExt;
            if link_attrs & 0x6 == 0x6 || meta.file_attributes() & 0x6 == 0x6 {
                continue;
            }
            e.hidden |= link_attrs & 0x2 != 0;
        }
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
///
/// Works on the bytes with an ASCII fast path and no allocation (sorting a
/// 10,000-file folder makes over 100,000 comparisons on the UI thread).
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let (Some(&ca), Some(&cb)) = (ab.get(i), bb.get(j)) else {
            // Whichever name ran out first sorts first.
            return (i < ab.len()).cmp(&(j < bb.len()));
        };
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Index i and j stay on char boundaries: ASCII digits never occur
            // inside a multi-byte UTF-8 sequence.
            let (si, sj) = (i, j);
            while ab.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            while bb.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            let (na, nb) = (&ab[si..i], &bb[sj..j]);
            let ta = &na[na.iter().take_while(|&&c| c == b'0').count()..];
            let tb = &nb[nb.iter().take_while(|&&c| c == b'0').count()..];
            let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
            if ord != Ordering::Equal {
                return ord;
            }
        } else if ca.is_ascii() && cb.is_ascii() {
            let (la, lb) = (ca.to_ascii_lowercase(), cb.to_ascii_lowercase());
            if la != lb {
                return la.cmp(&lb);
            }
            i += 1;
            j += 1;
        } else {
            let xa = a[i..].chars().next().unwrap_or_default();
            let xb = b[j..].chars().next().unwrap_or_default();
            let la = xa.to_lowercase().next().unwrap_or(xa);
            let lb = xb.to_lowercase().next().unwrap_or(xb);
            if la != lb {
                return la.cmp(&lb);
            }
            i += xa.len_utf8();
            j += xb.len_utf8();
        }
    }
}

pub struct SortContext<'a> {
    pub dims: &'a dyn Fn(&Path) -> Option<(u32, u32)>,
    pub taken: &'a dyn Fn(&Path) -> Option<i64>,
    pub rating: &'a dyn Fn(&Path) -> u8,
    pub flagged: &'a dyn Fn(&Path) -> bool,
    /// Color label 0..=5 (0 = none).
    pub label: &'a dyn Fn(&Path) -> u8,
    /// Place in the folder's custom order (`SortKey::Custom`); files without
    /// one follow the arranged files, by name.
    pub custom: &'a dyn Fn(&Path) -> Option<usize>,
}

pub fn sort_entries(entries: &mut [Entry], key: SortKey, desc: bool, folders_first: bool, cx: &SortContext) {
    use std::cmp::Ordering;
    let folder_ord = |a: &Entry, b: &Entry| (folders_first && a.is_folder() != b.is_folder()).then(|| if a.is_folder() { Ordering::Less } else { Ordering::Greater });
    let finish = |a: &Entry, b: &Entry, ord: Ordering| {
        let ord = ord.then_with(|| natural_cmp(&a.name, &b.name));
        if desc { ord.reverse() } else { ord }
    };
    if key == SortKey::Custom {
        // An arranged order has no direction: Descending is ignored.
        let pos: Vec<usize> = entries.iter().map(|e| (cx.custom)(&e.path).unwrap_or(usize::MAX)).collect();
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by(|&i, &j| {
            let (a, b) = (&entries[i], &entries[j]);
            folder_ord(a, b).unwrap_or_else(|| pos[i].cmp(&pos[j]).then_with(|| natural_cmp(&a.name, &b.name)))
        });
        permute(entries, &order);
        return;
    }
    // Keys that need a lookup by path (metadata, tags) are read once per
    // entry, not twice per comparison.
    let looked_up: Option<Vec<Option<i128>>> = match key {
        SortKey::Dimensions => Some(entries.iter().map(|e| (cx.dims)(&e.path).map(|(w, h)| w as i128 * h as i128)).collect()),
        SortKey::DateTaken => Some(entries.iter().map(|e| (cx.taken)(&e.path).or_else(|| e.modified.map(systime_secs)).map(i128::from)).collect()),
        SortKey::Rating => Some(entries.iter().map(|e| Some((cx.rating)(&e.path) as i128)).collect()),
        // Ascending puts flagged files first, like a "picks" view.
        SortKey::Flagged => Some(entries.iter().map(|e| Some(if (cx.flagged)(&e.path) { 0 } else { 1 })).collect()),
        // Ascending: red, yellow, green, blue, purple, then unlabeled files.
        SortKey::ColorLabel => Some(
            entries
                .iter()
                .map(|e| {
                    let l = (cx.label)(&e.path);
                    Some(if l == 0 { u8::MAX as i128 } else { l as i128 })
                })
                .collect(),
        ),
        _ => None,
    };
    match looked_up {
        None => entries.sort_by(|a, b| {
            if let Some(o) = folder_ord(a, b) {
                return o;
            }
            let ord = match key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Created => a.created.cmp(&b.created),
                SortKey::Type => a.ext.cmp(&b.ext),
                _ => Ordering::Equal,
            };
            finish(a, b, ord)
        }),
        Some(keys) => {
            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by(|&i, &j| {
                let (a, b) = (&entries[i], &entries[j]);
                folder_ord(a, b).unwrap_or_else(|| finish(a, b, keys[i].cmp(&keys[j])))
            });
            permute(entries, &order);
        }
    }
}

/// Reorder `items` in place so position k holds the item that was at `order[k]`.
fn permute<T>(items: &mut [T], order: &[usize]) {
    let mut done = vec![false; items.len()];
    for start in 0..items.len() {
        if done[start] {
            continue;
        }
        let mut k = start;
        loop {
            done[k] = true;
            let from = order[k];
            if from == start {
                break;
            }
            items.swap(k, from);
            k = from;
        }
    }
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
        None => " - ".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_noise_is_appdata_in_the_profile_only() {
        let home = Path::new(r"C:\Users\pat");
        assert!(is_noise_in(Path::new(r"C:\Users\pat\AppData"), home));
        assert!(is_noise_in(Path::new(r"c:\users\PAT\appdata"), Path::new(r"C:\Users\pat\")));
        assert!(!is_noise_in(Path::new(r"C:\Users\pat\Pictures"), home));
        assert!(!is_noise_in(Path::new(r"D:\Work\AppData"), home));
        assert!(!is_noise_in(Path::new(r"C:\Users\pat\Pictures\AppData"), home));
    }
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
    /// `natural_cmp` before it was made allocation-free (the reference).
    fn old_natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
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

    /// `sort_entries` before keys were looked up once per entry (the reference).
    fn old_sort_entries(entries: &mut [Entry], key: SortKey, desc: bool, folders_first: bool, cx: &SortContext) {
        use std::cmp::Ordering;
        entries.sort_by(|a, b| {
            if folders_first && a.is_folder() != b.is_folder() {
                return if a.is_folder() { Ordering::Less } else { Ordering::Greater };
            }
            let ord = match key {
                SortKey::Name => old_natural_cmp(&a.name, &b.name),
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
                SortKey::Flagged => (cx.flagged)(&b.path).cmp(&(cx.flagged)(&a.path)),
                SortKey::ColorLabel => {
                    let k = |l: u8| if l == 0 { u8::MAX } else { l };
                    k((cx.label)(&a.path)).cmp(&k((cx.label)(&b.path)))
                }
                SortKey::Custom => std::cmp::Ordering::Equal,
            }
            .then_with(|| old_natural_cmp(&a.name, &b.name));
            if desc { ord.reverse() } else { ord }
        });
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    fn random_name(r: &mut Lcg) -> String {
        const PARTS: [&str; 22] = ["a", "B", "z", "Z", "0", "00", "1", "2", "9", "10", "007", "123456789012345678901234567890", "_", " ", ".", "-", "\u{e9}", "\u{c4}", "\u{df}", "\u{65e5}", "\u{130}", "IMG"];
        (0..r.next(7) + 1).map(|_| PARTS[r.next(PARTS.len())]).collect()
    }

    #[test]
    fn natural_cmp_matches_reference() {
        let mut r = Lcg(42);
        let names: Vec<String> = (0..600).map(|_| random_name(&mut r)).collect();
        for a in &names {
            for b in names.iter().step_by(7) {
                assert_eq!(natural_cmp(a, b), old_natural_cmp(a, b), "{a:?} vs {b:?}");
            }
        }
        assert_eq!(natural_cmp("", ""), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("", "a"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("a1", "a"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn sort_matches_reference() {
        use std::collections::HashMap;
        let mut r = Lcg(7);
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let entries: Vec<Entry> = (0..400)
            .map(|i| {
                let name = random_name(&mut r);
                let path = PathBuf::from(format!(r"C:\x\{i}\{name}"));
                let folder = r.next(6) == 0;
                Entry {
                    ext: ["jpg", "png", "JPG", ""][r.next(4)].into(),
                    path,
                    name,
                    kind: if folder { EntryKind::Folder } else { EntryKind::Image },
                    size: r.next(5) as u64,
                    modified: (r.next(5) > 0).then(|| t0 + std::time::Duration::from_secs(r.next(4) as u64)),
                    created: (r.next(5) > 0).then(|| t0 + std::time::Duration::from_secs(r.next(4) as u64)),
                    hidden: false,
                }
            })
            .collect();
        let meta: HashMap<PathBuf, (Option<(u32, u32)>, Option<i64>, u8, bool, u8)> = entries
            .iter()
            .map(|e| {
                let dims = (r.next(4) > 0).then(|| (r.next(3) as u32 * 1000, r.next(3) as u32 * 500));
                let taken = (r.next(3) > 0).then(|| 1_600_000_000 + r.next(4) as i64);
                (e.path.clone(), (dims, taken, r.next(6) as u8, r.next(2) == 0, r.next(6) as u8))
            })
            .collect();
        let dims = |p: &Path| meta[p].0;
        let taken = |p: &Path| meta[p].1;
        let rating = |p: &Path| meta[p].2;
        let flagged = |p: &Path| meta[p].3;
        let label = |p: &Path| meta[p].4;
        let cx = SortContext { dims: &dims, taken: &taken, rating: &rating, flagged: &flagged, label: &label, custom: &|_: &Path| None };
        let keys = [SortKey::Name, SortKey::Size, SortKey::Modified, SortKey::Created, SortKey::Type, SortKey::Dimensions, SortKey::DateTaken, SortKey::Rating, SortKey::Flagged, SortKey::ColorLabel];
        for key in keys {
            for desc in [false, true] {
                for folders_first in [false, true] {
                    let mut a = entries.clone();
                    let mut b = entries.clone();
                    sort_entries(&mut a, key, desc, folders_first, &cx);
                    old_sort_entries(&mut b, key, desc, folders_first, &cx);
                    let pa: Vec<&PathBuf> = a.iter().map(|e| &e.path).collect();
                    let pb: Vec<&PathBuf> = b.iter().map(|e| &e.path).collect();
                    assert_eq!(pa, pb, "{key:?} desc={desc} folders_first={folders_first}");
                }
            }
        }
    }

    #[test]
    fn custom_order_puts_arranged_files_first_and_ignores_direction() {
        let mk = |n: &str, folder: bool| Entry {
            ext: if folder { String::new() } else { "jpg".into() },
            path: PathBuf::from(format!(r"C:\d\{n}")),
            name: n.into(),
            kind: if folder { EntryKind::Folder } else { EntryKind::Image },
            size: 0,
            modified: None,
            created: None,
            hidden: false,
        };
        let mut e = vec![mk("a.jpg", false), mk("b.jpg", false), mk("sub", true), mk("c.jpg", false), mk("d.jpg", false)];
        let pos = |p: &Path| -> Option<usize> {
            match p.file_name()?.to_str()? {
                "c.jpg" => Some(0),
                "a.jpg" => Some(1),
                _ => None,
            }
        };
        let none = |_: &Path| -> Option<(u32, u32)> { None };
        let zero = |_: &Path| 0u8;
        let no = |_: &Path| false;
        let tk = |_: &Path| -> Option<i64> { None };
        let cx = SortContext { dims: &none, taken: &tk, rating: &zero, flagged: &no, label: &zero, custom: &pos };
        for desc in [false, true] {
            sort_entries(&mut e, SortKey::Custom, desc, true, &cx);
            let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
            assert_eq!(names, ["sub", "c.jpg", "a.jpg", "b.jpg", "d.jpg"]);
        }
    }

    #[test]
    fn permute_reorders() {
        let mut v = vec!['a', 'b', 'c', 'd', 'e'];
        permute(&mut v, &[3, 0, 4, 1, 2]);
        assert_eq!(v, vec!['d', 'a', 'e', 'b', 'c']);
    }

    fn entries_10k() -> Vec<Entry> {
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        (0..10_000u64)
            .map(|i| {
                let k = (i * 7919) % 10_000;
                let name = match i % 4 {
                    0 => format!("IMG_{k:04}.JPG"),
                    1 => format!("DSC{k}.jpg"),
                    2 => format!("Holiday 2024 - beach {k} (edit).png"),
                    _ => format!("20240{}{:02}_{k:06}.jpeg", k % 9 + 1, k % 28 + 1),
                };
                let path = PathBuf::from(format!(r"C:\Users\someone\Pictures\Trips\{name}"));
                Entry { ext: formats::ext_of(&path), path, name, kind: EntryKind::Image, size: k * 1000, modified: Some(t0 + std::time::Duration::from_secs(k * 37)), created: None, hidden: false }
            })
            .collect()
    }

    /// `cargo test --release -p lumen-core -- --ignored --nocapture bench_perf_`
    #[test]
    #[ignore]
    fn bench_perf_sort() {
        use std::collections::HashMap;
        let base = entries_10k();
        let taken: HashMap<PathBuf, i64> = base.iter().enumerate().filter(|(i, _)| i % 3 != 0).map(|(i, e)| (e.path.clone(), 1_600_000_000 + (i as i64 * 7907) % 100_000)).collect();
        let dims = |_: &Path| -> Option<(u32, u32)> { None };
        let tk = |p: &Path| taken.get(p).copied();
        let zero = |_: &Path| 0u8;
        let no = |_: &Path| false;
        let cx = SortContext { dims: &dims, taken: &tk, rating: &zero, flagged: &no, label: &zero, custom: &|_: &Path| None };
        let best = |f: &dyn Fn()| {
            (0..5)
                .map(|_| {
                    let s = std::time::Instant::now();
                    f();
                    s.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        for key in [SortKey::Name, SortKey::Modified, SortKey::DateTaken] {
            let t = best(&|| {
                let mut v = base.clone();
                sort_entries(&mut v, key, false, true, &cx);
                std::hint::black_box(v);
            });
            eprintln!("sort 10k by {key:?}: {t:.2} ms (includes a 10k clone)");
        }
        let t = best(&|| {
            std::hint::black_box(base.clone());
        });
        eprintln!("clone 10k entries: {t:.2} ms");
        let t = best(&|| {
            std::hint::black_box(base.iter().filter(|e| matches_filter(&e.name, "beach .png")).count());
        });
        eprintln!("filter 10k names: {t:.2} ms");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(300 * 1024 * 1024), "300 MB");
    }
}
