//! Finding devices and scenes in a folder of captures.
//!
//! Camera teams store captures in a few common ways; all are recognized:
//!
//! - One folder per device: `Round1/GalaxyA56/night_01.jpg`,
//!   `Round1/PhoneX/night_01.jpg`. Scenes are matched by file name (with the
//!   device name removed, so `night_01_A56.jpg` and `night_01_X.jpg` match);
//!   when the names have nothing in common (camera numbering such as
//!   `IMG_1234.jpg`), the n-th capture of every device forms scene n.
//! - All files in one folder, device and scene in the file name:
//!   `night_01_A56.jpg`, `A56-night_01.jpg`, `night 01 (PhoneX).jpg`. The
//!   name part that takes a few values, each repeated across scenes, is the
//!   device.
//! - All files in one folder from different cameras with camera numbering:
//!   the EXIF camera model is the device and the n-th capture (by capture
//!   time) of every camera forms scene n.
//!
//! Everything here works on relative paths (`/` separated) so it is easy to
//! test; [`scan`] lists a real folder.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// How the devices were found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// One folder per device, scenes matched by file name.
    FoldersByName,
    /// One folder per device, scenes matched by capture order.
    FoldersByOrder,
    /// Device and scene in the file name.
    FileNames,
    /// EXIF camera model, scenes by capture order.
    CameraModel,
    /// Nothing recognizable: every file is its own scene of one device.
    Single,
}

impl Method {
    pub fn describe(self) -> &'static str {
        match self {
            Method::FoldersByName => "one folder per device; scenes matched by file name",
            Method::FoldersByOrder => "one folder per device; scenes matched by capture order",
            Method::FileNames => "device and scene taken from the file names",
            Method::CameraModel => "device from the camera model; scenes matched by capture time order",
            Method::Single => "no devices recognized: one scene per file",
        }
    }
}

/// A scene found by detection: its name and one file per device (a device
/// can have more than one file when the names collide; the UI shows that).
#[derive(Clone, Debug, PartialEq)]
pub struct SceneDraft {
    pub name: String,
    /// (device, relative path).
    pub shots: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub method: Method,
    pub devices: Vec<String>,
    pub scenes: Vec<SceneDraft>,
    /// Plain-language remarks ("PhoneX has 2 captures fewer than the others").
    pub notes: Vec<String>,
}

/// What detection may know about one file besides its path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileInfo {
    /// Relative path with `/` separators.
    pub rel: String,
    /// EXIF camera model ("SM-A566B"), if known.
    pub model: Option<String>,
    /// Capture time (seconds, any epoch), if known.
    pub taken: Option<i64>,
}

impl FileInfo {
    pub fn new(rel: impl Into<String>) -> FileInfo {
        FileInfo { rel: rel.into(), ..Default::default() }
    }
}

fn split_rel(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

fn stem_of(file: &str) -> &str {
    match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    }
}

/// File name parts: split at `_`, `-`, space, `.`, brackets.
pub fn tokens(stem: &str) -> Vec<String> {
    stem.split(|c: char| c == '_' || c == '-' || c == ' ' || c == '.' || c == '(' || c == ')' || c == '[' || c == ']')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// A scene key from a file stem: lower case, the device name's parts removed.
fn scene_key(stem: &str, device: &str) -> String {
    let dev: Vec<String> = tokens(device).iter().map(|t| t.to_lowercase()).collect();
    let dev_joined = dev.join("");
    let parts: Vec<String> = tokens(stem)
        .into_iter()
        .map(|t| t.to_lowercase())
        .filter(|t| !dev.contains(t) && *t != dev_joined)
        .collect();
    if parts.is_empty() {
        stem.to_lowercase()
    } else {
        parts.join("_")
    }
}

/// A readable scene name from the file stem of its first capture.
fn scene_title(stem: &str, device: &str) -> String {
    let dev: Vec<String> = tokens(device).iter().map(|t| t.to_lowercase()).collect();
    let dev_joined = dev.join("");
    let parts: Vec<String> = tokens(stem).into_iter().filter(|t| !dev.contains(&t.to_lowercase()) && t.to_lowercase() != dev_joined).collect();
    if parts.is_empty() {
        stem.to_string()
    } else {
        parts.join("_")
    }
}

fn natural_sort(v: &mut [String]) {
    v.sort_by(|a, b| crate::catalog::natural_cmp(a, b));
}

/// Short, unique device names for folders: the last folder name, or the last
/// two joined when the last names repeat (`R1/A56`, `R2/A56`).
fn device_names(dirs: &[String]) -> Vec<String> {
    let last: Vec<String> = dirs.iter().map(|d| split_rel(d).1.to_string()).collect();
    let unique = last.iter().collect::<BTreeSet<_>>().len() == last.len();
    if unique {
        return last;
    }
    dirs.iter()
        .map(|d| {
            let parts: Vec<&str> = d.split('/').collect();
            let n = parts.len();
            if n >= 2 {
                format!("{} {}", parts[n - 2], parts[n - 1])
            } else {
                d.clone()
            }
        })
        .collect()
}

fn by_order(files: &[&FileInfo]) -> Vec<String> {
    let mut v: Vec<&FileInfo> = files.to_vec();
    // Capture time when every file has one, else natural name order.
    if v.iter().all(|f| f.taken.is_some()) {
        v.sort_by(|a, b| a.taken.cmp(&b.taken).then_with(|| crate::catalog::natural_cmp(&a.rel, &b.rel)));
    } else {
        v.sort_by(|a, b| crate::catalog::natural_cmp(&a.rel, &b.rel));
    }
    v.into_iter().map(|f| f.rel.clone()).collect()
}

fn order_scenes(devices: &[String], per_device: &[Vec<String>], notes: &mut Vec<String>) -> Vec<SceneDraft> {
    let max = per_device.iter().map(|v| v.len()).max().unwrap_or(0);
    let min = per_device.iter().map(|v| v.len()).min().unwrap_or(0);
    if max != min {
        for (d, v) in devices.iter().zip(per_device) {
            if v.len() < max {
                notes.push(format!("{d} has {} capture{} fewer than the others: check the pairing of the last scenes", max - v.len(), if max - v.len() == 1 { "" } else { "s" }));
            }
        }
    }
    (0..max)
        .map(|k| SceneDraft {
            name: format!("Scene {:02}", k + 1),
            shots: devices.iter().zip(per_device).filter_map(|(d, v)| v.get(k).map(|r| (d.clone(), r.clone()))).collect(),
        })
        .collect()
}

/// Group files that are in the same folder; folders with media only.
fn folder_groups(files: &[FileInfo]) -> BTreeMap<String, Vec<&FileInfo>> {
    let mut m: BTreeMap<String, Vec<&FileInfo>> = BTreeMap::new();
    for f in files {
        m.entry(split_rel(&f.rel).0.to_string()).or_default().push(f);
    }
    m
}

fn detect_folders(groups: &BTreeMap<String, Vec<&FileInfo>>) -> Option<Detection> {
    // Leaf folders only, and each needs at least one file.
    let dirs: Vec<String> = groups.keys().filter(|d| !d.is_empty()).cloned().collect();
    if dirs.len() < 2 || dirs.len() > 12 {
        return None;
    }
    let devices = device_names(&dirs);
    let mut notes = Vec::new();
    // Scene keys per device.
    let mut keyed: Vec<BTreeMap<String, Vec<String>>> = Vec::new();
    for (d, dev) in dirs.iter().zip(&devices) {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in &groups[d] {
            let stem = stem_of(split_rel(&f.rel).1);
            m.entry(scene_key(stem, dev)).or_default().push(f.rel.clone());
        }
        keyed.push(m);
    }
    let mut count: HashMap<&String, usize> = HashMap::new();
    for m in &keyed {
        for k in m.keys() {
            *count.entry(k).or_default() += 1;
        }
    }
    let total_keys = count.len().max(1);
    let shared = count.values().filter(|&&c| c >= 2).count();
    if shared * 2 >= total_keys {
        // Matched by name. Scene order: natural order of the keys.
        let mut keys: Vec<String> = count.keys().map(|k| (*k).clone()).collect();
        natural_sort(&mut keys);
        let mut scenes = Vec::new();
        for k in keys {
            let mut shots = Vec::new();
            let mut title = None;
            for (dev, m) in devices.iter().zip(&keyed) {
                if let Some(files) = m.get(&k) {
                    let mut files = files.clone();
                    natural_sort(&mut files);
                    if title.is_none() {
                        title = files.first().map(|f| scene_title(stem_of(split_rel(f).1), dev));
                    }
                    for f in files {
                        shots.push((dev.clone(), f));
                    }
                }
            }
            scenes.push(SceneDraft { name: title.unwrap_or(k), shots });
        }
        let lonely = scenes.iter().filter(|s| s.shots.len() < 2).count();
        if lonely > 0 {
            notes.push(format!("{lonely} scene{} found on one device only", if lonely == 1 { " was" } else { "s were" }));
        }
        return Some(Detection { method: Method::FoldersByName, devices, scenes, notes });
    }
    let per_device: Vec<Vec<String>> = dirs.iter().map(|d| by_order(&groups[d])).collect();
    let scenes = order_scenes(&devices, &per_device, &mut notes);
    Some(Detection { method: Method::FoldersByOrder, devices, scenes, notes })
}

/// A candidate device position in the file name: counted from the start
/// (`Some(i)` = token i) or from the end (`None` + index from the end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pos {
    Start(usize),
    End(usize),
}

fn token_at(toks: &[String], p: Pos) -> Option<&String> {
    match p {
        Pos::Start(i) => toks.get(i),
        Pos::End(i) => toks.len().checked_sub(i + 1).and_then(|k| toks.get(k)),
    }
}

fn without(toks: &[String], p: Pos) -> String {
    let skip = match p {
        Pos::Start(i) => i,
        Pos::End(i) => toks.len().saturating_sub(i + 1),
    };
    toks.iter().enumerate().filter(|(k, _)| *k != skip).map(|(_, t)| t.to_lowercase()).collect::<Vec<_>>().join("_")
}

fn detect_names(files: &[&FileInfo]) -> Option<Detection> {
    if files.len() < 2 {
        return None;
    }
    let toks: Vec<Vec<String>> = files.iter().map(|f| tokens(stem_of(split_rel(&f.rel).1))).collect();
    let max_len = toks.iter().map(|t| t.len()).max().unwrap_or(0);
    // Best split: most scenes with several devices, then fewest devices.
    let mut best: Option<(usize, usize, Pos)> = None;
    for i in 0..max_len.min(6) {
        for p in [Pos::Start(i), Pos::End(i)] {
            let mut values: BTreeMap<String, usize> = BTreeMap::new();
            let mut ok = true;
            for t in &toks {
                match token_at(t, p) {
                    Some(v) if t.len() >= 2 => *values.entry(v.to_lowercase()).or_default() += 1,
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || values.len() < 2 || values.len() > 8 {
                continue;
            }
            // Pure numbers are frame or scene counters, not devices.
            if values.keys().all(|v| v.chars().all(|c| c.is_ascii_digit())) {
                continue;
            }
            // Scenes: the name without that part. A good split has scenes
            // with several devices and no device twice.
            let mut scenes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut dup = false;
            for t in &toks {
                let dev = token_at(t, p).map(|v| v.to_lowercase()).unwrap_or_default();
                if !scenes.entry(without(t, p)).or_default().insert(dev) {
                    dup = true;
                }
            }
            if dup {
                continue;
            }
            let multi = scenes.values().filter(|d| d.len() >= 2).count();
            if multi == 0 || multi * 2 < scenes.len() {
                continue;
            }
            let nv = values.len();
            if best.is_none_or(|(b, bn, _)| multi > b || (multi == b && nv < bn)) {
                best = Some((multi, nv, p));
            }
        }
    }
    let (_, _, p) = best?;
    // Device display names keep the spelling of their first occurrence.
    let mut dev_name: BTreeMap<String, String> = BTreeMap::new();
    let mut scenes: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut titles: BTreeMap<String, String> = BTreeMap::new();
    for (f, t) in files.iter().zip(&toks) {
        let raw = token_at(t, p).cloned().unwrap_or_default();
        let name = dev_name.entry(raw.to_lowercase()).or_insert(raw).clone();
        let key = without(t, p);
        titles.entry(key.clone()).or_insert_with(|| {
            let skip = match p {
                Pos::Start(i) => i,
                Pos::End(i) => t.len().saturating_sub(i + 1),
            };
            t.iter().enumerate().filter(|(k, _)| *k != skip).map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("_")
        });
        scenes.entry(key).or_default().push((name, f.rel.clone()));
    }
    let mut devices: Vec<String> = dev_name.into_values().collect();
    natural_sort(&mut devices);
    let mut keys: Vec<String> = scenes.keys().cloned().collect();
    natural_sort(&mut keys);
    let mut notes = Vec::new();
    let out: Vec<SceneDraft> = keys
        .into_iter()
        .map(|k| {
            let mut shots = scenes.remove(&k).unwrap_or_default();
            shots.sort_by_key(|(d, _)| devices.iter().position(|x| x == d).unwrap_or(usize::MAX));
            SceneDraft { name: titles.get(&k).cloned().unwrap_or(k), shots }
        })
        .collect();
    let lonely = out.iter().filter(|s| s.shots.len() < 2).count();
    if lonely > 0 {
        notes.push(format!("{lonely} scene{} found on one device only", if lonely == 1 { " was" } else { "s were" }));
    }
    Some(Detection { method: Method::FileNames, devices, scenes: out, notes })
}

fn detect_models(files: &[&FileInfo]) -> Option<Detection> {
    let mut by_model: BTreeMap<String, Vec<&FileInfo>> = BTreeMap::new();
    for f in files {
        by_model.entry(f.model.clone()?.trim().to_string()).or_default().push(f);
    }
    if by_model.len() < 2 || by_model.len() > 8 {
        return None;
    }
    let devices: Vec<String> = by_model.keys().cloned().collect();
    let per_device: Vec<Vec<String>> = by_model.values().map(|v| by_order(v)).collect();
    let mut notes = Vec::new();
    let scenes = order_scenes(&devices, &per_device, &mut notes);
    Some(Detection { method: Method::CameraModel, devices, scenes, notes })
}

/// Find devices and scenes in a list of files (relative paths).
pub fn detect(files: &[FileInfo]) -> Detection {
    let groups = folder_groups(files);
    let non_root = groups.keys().filter(|d| !d.is_empty()).count();
    if non_root >= 2 {
        if let Some(d) = detect_folders(&groups) {
            return d;
        }
    }
    // One folder: the names, then the camera models.
    let flat: Vec<&FileInfo> = files.iter().collect();
    if let Some(d) = detect_names(&flat) {
        return d;
    }
    if let Some(d) = detect_models(&flat) {
        return d;
    }
    let mut rels: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
    natural_sort(&mut rels);
    let device = "Camera".to_string();
    Detection {
        method: Method::Single,
        devices: vec![device.clone()],
        scenes: rels.into_iter().map(|r| SceneDraft { name: stem_of(split_rel(&r).1).to_string(), shots: vec![(device.clone(), r)] }).collect(),
        notes: vec!["No devices were recognized. Put each device's captures in its own folder, or add the device to the file names (night_01_PhoneA.jpg).".into()],
    }
}

/// Media files under `root` (at most `depth` folders deep, at most `limit`
/// files) as relative paths; the project file's own folder layout.
pub fn scan(root: &Path, depth: usize, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((d, level)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for item in entries {
            let path = item.path();
            let name = item.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = item.file_type() else { continue };
            if ft.is_dir() {
                if level < depth {
                    stack.push((path, level + 1));
                }
            } else {
                let ext = crate::formats::ext_of(&path);
                if crate::formats::is_image_ext(&ext) || crate::video::is_video_ext(&ext) {
                    out.push(path);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// `path` relative to `root` with `/` separators, if it is inside `root`.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infos(v: &[&str]) -> Vec<FileInfo> {
        v.iter().map(|s| FileInfo::new(*s)).collect()
    }

    #[test]
    fn folders_matched_by_name() {
        let d = detect(&infos(&["R1/A56/night_01.jpg", "R1/A56/night_02.jpg", "R1/A56/portrait.jpg", "R1/PhoneX/night_01.jpg", "R1/PhoneX/night_02.jpg", "R1/PhoneX/portrait.jpg"]));
        assert_eq!(d.method, Method::FoldersByName);
        assert_eq!(d.devices, vec!["A56", "PhoneX"]);
        assert_eq!(d.scenes.len(), 3);
        assert_eq!(d.scenes[0].name, "night_01");
        assert_eq!(d.scenes[0].shots, vec![("A56".to_string(), "R1/A56/night_01.jpg".to_string()), ("PhoneX".to_string(), "R1/PhoneX/night_01.jpg".to_string())]);
        assert!(d.notes.is_empty());
    }

    #[test]
    fn device_name_in_file_is_ignored_for_matching() {
        let d = detect(&infos(&["A56/night_01_A56.jpg", "A56/day_A56.jpg", "X/night_01_X.jpg", "X/day_X.jpg"]));
        assert_eq!(d.method, Method::FoldersByName);
        assert_eq!(d.scenes.len(), 2);
        assert_eq!(d.scenes[0].name, "day");
        assert_eq!(d.scenes[1].name, "night_01");
        assert_eq!(d.scenes[1].shots.len(), 2);
    }

    #[test]
    fn folders_matched_by_order() {
        let mut f = infos(&["A/IMG_0003.jpg", "A/IMG_0001.jpg", "B/20261101_101010.jpg", "B/20261101_101500.jpg", "B/20261101_102000.jpg"]);
        f.push(FileInfo::new("A/IMG_0010.jpg"));
        let d = detect(&f);
        assert_eq!(d.method, Method::FoldersByOrder);
        assert_eq!(d.scenes.len(), 3);
        assert_eq!(d.scenes[0].shots[0].1, "A/IMG_0001.jpg");
        assert_eq!(d.scenes[2].shots[0].1, "A/IMG_0010.jpg");
        assert_eq!(d.scenes[2].shots[1].1, "B/20261101_102000.jpg");
        assert!(d.notes.is_empty());
    }

    #[test]
    fn uneven_counts_are_reported() {
        let d = detect(&infos(&["A/IMG_1.jpg", "A/IMG_2.jpg", "B/DSC_7.jpg"]));
        assert_eq!(d.method, Method::FoldersByOrder);
        assert_eq!(d.scenes.len(), 2);
        assert_eq!(d.scenes[1].shots.len(), 1);
        assert_eq!(d.notes.len(), 1);
        assert!(d.notes[0].starts_with("B has 1 capture fewer"));
    }

    #[test]
    fn repeated_leaf_names_get_parent_prefix() {
        assert_eq!(device_names(&["R1/A56".into(), "R2/A56".into()]), vec!["R1 A56", "R2 A56"]);
        assert_eq!(device_names(&["A56".into(), "X".into()]), vec!["A56", "X"]);
    }

    #[test]
    fn device_suffix_in_flat_folder() {
        let d = detect(&infos(&["night_01_A56.jpg", "night_01_X.jpg", "night_02_A56.jpg", "night_02_X.jpg", "food_A56.jpg", "food_X.jpg"]));
        assert_eq!(d.method, Method::FileNames);
        assert_eq!(d.devices, vec!["A56", "X"]);
        assert_eq!(d.scenes.len(), 3);
        assert_eq!(d.scenes[0].name, "food");
        assert_eq!(d.scenes[1].name, "night_01");
        assert_eq!(d.scenes[1].shots[1], ("X".to_string(), "night_01_X.jpg".to_string()));
    }

    #[test]
    fn device_prefix_in_flat_folder() {
        let d = detect(&infos(&["A56-night-1.jpg", "PhoneX-night-1.jpg", "A56-night-2.jpg", "PhoneX-night-2.jpg"]));
        assert_eq!(d.method, Method::FileNames);
        assert_eq!(d.devices, vec!["A56", "PhoneX"]);
        assert_eq!(d.scenes.len(), 2);
        assert_eq!(d.scenes[0].name, "night_1");
    }

    #[test]
    fn counters_are_not_devices() {
        // Only the trailing number varies: no devices.
        let d = detect(&infos(&["shot_1.jpg", "shot_2.jpg", "shot_3.jpg"]));
        assert_eq!(d.method, Method::Single);
        assert_eq!(d.scenes.len(), 3);
    }

    #[test]
    fn camera_models_from_exif() {
        let f = vec![
            FileInfo { rel: "IMG_0002.jpg".into(), model: Some("SM-A566B".into()), taken: Some(200) },
            FileInfo { rel: "IMG_0001.jpg".into(), model: Some("SM-A566B".into()), taken: Some(100) },
            FileInfo { rel: "PXL_1.jpg".into(), model: Some("Pixel 9".into()), taken: Some(105) },
            FileInfo { rel: "PXL_2.jpg".into(), model: Some("Pixel 9".into()), taken: Some(206) },
        ];
        let d = detect(&f);
        assert_eq!(d.method, Method::CameraModel);
        assert_eq!(d.devices, vec!["Pixel 9", "SM-A566B"]);
        assert_eq!(d.scenes[0].shots, vec![("Pixel 9".to_string(), "PXL_1.jpg".to_string()), ("SM-A566B".to_string(), "IMG_0001.jpg".to_string())]);
    }

    #[test]
    fn relative_paths() {
        let root = Path::new("C:/caps");
        assert_eq!(relative(root, Path::new("C:/caps/A/x.jpg")).as_deref(), Some("A/x.jpg"));
        assert_eq!(relative(root, Path::new("D:/other/x.jpg")), None);
    }

    #[test]
    fn tokens_split_on_separators() {
        assert_eq!(tokens("night 01 (PhoneX)"), vec!["night", "01", "PhoneX"]);
        assert_eq!(scene_key("night_01_A56", "A56"), "night_01");
        assert_eq!(scene_key("A56", "A56"), "a56");
    }
}
