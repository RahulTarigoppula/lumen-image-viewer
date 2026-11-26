//! Custom file order per folder (FastStone-style "drag thumbnails to arrange").
//! Stored as file names in a private JSON file in the app data folder, like the
//! tags: images are never renamed or touched.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// At most this many folders keep a custom order; the oldest are dropped first.
const MAX_FOLDERS: usize = 2000;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderStore {
    /// Lower-cased folder path -> file names in display order.
    folders: HashMap<String, Vec<String>>,
    /// Lower-cased folder paths, least recently changed first (for the cap).
    recent: Vec<String>,
    /// Folders whose arrangement is kept but not shown (another sort was picked there).
    inactive: std::collections::HashSet<String>,
    #[serde(skip)]
    dirty: bool,
}

fn key(folder: &Path) -> String {
    folder.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()
}

impl OrderStore {
    fn file() -> PathBuf {
        crate::settings::data_dir().join("orders.json")
    }

    pub fn load() -> Self {
        let path = Self::file();
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("orders.json is unreadable ({e}); starting with no custom orders");
                crate::settings::quarantine_bad_file(&path);
                Self::default()
            }
        }
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let path = Self::file();
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                self.dirty = false;
            }
        }
    }

    /// The saved order of `folder`, if any.
    pub fn get(&self, folder: &Path) -> Option<&[String]> {
        self.folders.get(&key(folder)).map(|v| v.as_slice())
    }

    pub fn has(&self, folder: &Path) -> bool {
        self.folders.contains_key(&key(folder))
    }

    /// True when `folder` has an arrangement and it is the folder's sort.
    pub fn is_active(&self, folder: &Path) -> bool {
        let k = key(folder);
        self.folders.contains_key(&k) && !self.inactive.contains(&k)
    }

    /// Show (or stop showing) the folder's arrangement; it is kept either way.
    pub fn set_active(&mut self, folder: &Path, on: bool) {
        let k = key(folder);
        let changed = if on { self.inactive.remove(&k) } else { self.folders.contains_key(&k) && self.inactive.insert(k) };
        self.dirty |= changed;
    }

    /// Position of each saved name in `folder` (lower-cased name -> index).
    pub fn positions(&self, folder: &Path) -> HashMap<String, usize> {
        self.get(folder).map(|v| v.iter().enumerate().map(|(i, n)| (n.to_lowercase(), i)).collect()).unwrap_or_default()
    }

    pub fn set(&mut self, folder: &Path, names: Vec<String>) {
        let k = key(folder);
        self.recent.retain(|r| r != &k);
        self.recent.push(k.clone());
        self.inactive.remove(&k);
        self.folders.insert(k, names);
        while self.recent.len() > MAX_FOLDERS {
            let old = self.recent.remove(0);
            self.folders.remove(&old);
            self.inactive.remove(&old);
        }
        self.dirty = true;
    }

    pub fn clear(&mut self, folder: &Path) {
        let k = key(folder);
        if self.folders.remove(&k).is_some() {
            self.recent.retain(|r| r != &k);
            self.inactive.remove(&k);
            self.dirty = true;
        }
    }

    /// Follow a rename inside the same folder so the file keeps its place.
    pub fn rename(&mut self, from: &Path, to: &Path) {
        self.rename_many(&[(from.to_path_buf(), to.to_path_buf())]);
    }

    /// Follow several renames at once (a batch rename may swap names, so they are
    /// applied together, not one after the other). Moves to another folder are
    /// ignored: the file simply leaves this folder's order.
    pub fn rename_many(&mut self, moves: &[(PathBuf, PathBuf)]) {
        let mut by_dir: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (from, to) in moves {
            let (Some(dir), Some(old), Some(new)) = (from.parent(), from.file_name(), to.file_name()) else { continue };
            if to.parent() != Some(dir) {
                continue;
            }
            by_dir.entry(key(dir)).or_default().insert(old.to_string_lossy().to_lowercase(), new.to_string_lossy().into_owned());
        }
        for (dir, map) in by_dir {
            if let Some(v) = self.folders.get_mut(&dir) {
                for n in v.iter_mut() {
                    if let Some(new) = map.get(&n.to_lowercase()) {
                        *n = new.clone();
                        self.dirty = true;
                    }
                }
            }
        }
    }
}

/// `order` with the `moving` names taken out and put back together, in their
/// current relative order, just before `target` (or just after it when
/// `after`). A `target` of `None`, or one that is itself moving, puts them at
/// the end. Names compare without case (Windows file names).
pub fn move_names(order: &[String], moving: &[String], target: Option<&str>, after: bool) -> Vec<String> {
    let lower = |s: &str| s.to_lowercase();
    let set: std::collections::HashSet<String> = moving.iter().map(|m| lower(m)).collect();
    let moved: Vec<String> = order.iter().filter(|n| set.contains(&lower(n))).cloned().collect();
    let mut rest: Vec<String> = order.iter().filter(|n| !set.contains(&lower(n))).cloned().collect();
    let at = target
        .filter(|t| !set.contains(&lower(t)))
        .and_then(|t| rest.iter().position(|n| lower(n) == lower(t)))
        .map(|i| if after { i + 1 } else { i })
        .unwrap_or(rest.len());
    rest.splice(at..at, moved);
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn move_before_and_after() {
        let o = v(&["a", "b", "c", "d", "e"]);
        assert_eq!(move_names(&o, &v(&["d"]), Some("b"), false), v(&["a", "d", "b", "c", "e"]));
        assert_eq!(move_names(&o, &v(&["a"]), Some("c"), true), v(&["b", "c", "a", "d", "e"]));
        // Several files keep their relative order, whatever order they were picked in.
        assert_eq!(move_names(&o, &v(&["e", "b"]), Some("a"), false), v(&["b", "e", "a", "c", "d"]));
        // Onto one of the moving files, or nowhere: to the end.
        assert_eq!(move_names(&o, &v(&["a", "b"]), Some("b"), false), v(&["c", "d", "e", "a", "b"]));
        assert_eq!(move_names(&o, &v(&["c"]), None, false), v(&["a", "b", "d", "e", "c"]));
        // Case-insensitive names.
        assert_eq!(move_names(&o, &v(&["C"]), Some("A"), false), v(&["c", "a", "b", "d", "e"]));
    }

    #[test]
    fn store_set_get_rename_and_cap() {
        let mut s = OrderStore::default();
        let dir = Path::new(r"C:\Photos\Trip");
        s.set(dir, v(&["b.jpg", "a.jpg"]));
        assert_eq!(s.get(Path::new(r"c:\photos\trip\")).unwrap(), &v(&["b.jpg", "a.jpg"])[..]);
        assert_eq!(s.positions(dir).get("a.jpg"), Some(&1));
        s.rename(&dir.join("A.JPG"), &dir.join("z.jpg"));
        assert_eq!(s.get(dir).unwrap(), &v(&["b.jpg", "z.jpg"])[..]);
        // A move to another folder leaves the order alone.
        s.rename(&dir.join("b.jpg"), Path::new(r"C:\Other\b.jpg"));
        assert_eq!(s.get(dir).unwrap()[0], "b.jpg");
        // A swap applied together keeps both places.
        s.rename_many(&[(dir.join("b.jpg"), dir.join("z.jpg")), (dir.join("z.jpg"), dir.join("b.jpg"))]);
        assert_eq!(s.get(dir).unwrap(), &v(&["z.jpg", "b.jpg"])[..]);
        assert!(s.is_active(dir));
        s.set_active(dir, false);
        assert!(s.has(dir) && !s.is_active(dir));
        s.set(dir, v(&["b.jpg", "z.jpg"]));
        assert!(s.is_active(dir), "arranging again shows the order");
        s.clear(dir);
        assert!(!s.has(dir) && !s.is_active(dir));
        for i in 0..MAX_FOLDERS + 5 {
            s.set(Path::new(&format!(r"C:\f{i}")), v(&["x"]));
        }
        assert_eq!(s.folders.len(), MAX_FOLDERS);
        assert!(!s.has(Path::new(r"C:\f0")));
        assert!(s.has(Path::new(&format!(r"C:\f{}", MAX_FOLDERS + 4))));
    }
}
