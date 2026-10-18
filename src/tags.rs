//! Local ratings, flags (FastStone-style "tags") and color labels. Stored in a
//! private JSON file in the app data folder - never written into your images.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tag {
    pub rating: u8,
    pub flagged: bool,
    pub label: u8, // 0 = none, 1..=5 = red, yellow, green, blue, purple
}

impl Tag {
    pub fn is_empty(&self) -> bool {
        self.rating == 0 && !self.flagged && self.label == 0
    }
}

pub const LABEL_COLORS: [[u8; 3]; 6] = [
    [0, 0, 0],
    [235, 87, 87],
    [242, 201, 76],
    [39, 174, 96],
    [47, 128, 237],
    [155, 81, 224],
];
pub const LABEL_NAMES: [&str; 6] = ["None", "Red", "Yellow", "Green", "Blue", "Purple"];

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TagStore {
    tags: HashMap<String, Tag>,
    #[serde(skip)]
    dirty: bool,
}

fn key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

impl TagStore {
    fn file() -> PathBuf {
        crate::settings::data_dir().join("tags.json")
    }

    pub fn load() -> Self {
        let path = Self::file();
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(e) => {
                // Keep the unreadable file so the next save does not destroy the ratings.
                log::warn!("tags.json is unreadable ({e}); starting with no tags");
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

    pub fn get(&self, path: &Path) -> Tag {
        self.tags.get(&key(path)).copied().unwrap_or_default()
    }

    pub fn update(&mut self, path: &Path, f: impl FnOnce(&mut Tag)) {
        let k = key(path);
        let mut t = self.tags.get(&k).copied().unwrap_or_default();
        f(&mut t);
        if t.is_empty() {
            self.tags.remove(&k);
        } else {
            self.tags.insert(k, t);
        }
        self.dirty = true;
    }

    /// Keep tags attached when a file is renamed or moved inside Lumen.
    /// A moved or renamed folder carries the tags of everything inside it.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        self.moved_many(&[(from.to_path_buf(), to.to_path_buf())]);
    }

    /// Several moves done together (a batch rename): all old keys are taken out
    /// before any new one is written, so chains like 1->2, 2->3 stay correct.
    pub fn moved_many(&mut self, moves: &[(PathBuf, PathBuf)]) {
        let mut out: Vec<(String, Tag)> = Vec::new();
        for (from, to) in moves {
            let (kf, kt) = (key(from), key(to));
            if kf == kt {
                continue;
            }
            if let Some(t) = self.tags.remove(&kf) {
                out.push((kt.clone(), t));
            }
            let prefix = format!("{}\\", kf.trim_end_matches(['\\', '/']));
            let inside: Vec<String> = self.tags.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            let to_prefix = format!("{}\\", kt.trim_end_matches(['\\', '/']));
            for k in inside {
                if let Some(t) = self.tags.remove(&k) {
                    out.push((format!("{to_prefix}{}", &k[prefix.len()..]), t));
                }
            }
        }
        if !out.is_empty() {
            self.dirty = true;
        }
        self.tags.extend(out);
    }

    pub fn copied(&mut self, from: &Path, to: &Path) {
        if let Some(t) = self.tags.get(&key(from)).copied() {
            self.tags.insert(key(to), t);
            self.dirty = true;
        }
    }

    pub fn removed(&mut self, path: &Path) {
        if self.tags.remove(&key(path)).is_some() {
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_move_rekeys_everything_inside() {
        let mut s = TagStore::default();
        s.update(Path::new(r"C:\Photos\Trip\a.jpg"), |t| t.rating = 3);
        s.update(Path::new(r"C:\Photos\Trip\sub\b.jpg"), |t| t.flagged = true);
        s.update(Path::new(r"C:\Photos\Trip2\c.jpg"), |t| t.rating = 5);
        s.update(Path::new(r"C:\Photos\Trip"), |t| t.label = 2);
        s.moved(Path::new(r"c:\photos\TRIP"), Path::new(r"D:\Archive\Trip 2020"));
        assert_eq!(s.get(Path::new(r"D:\Archive\Trip 2020\a.jpg")).rating, 3);
        assert!(s.get(Path::new(r"d:\archive\trip 2020\SUB\b.jpg")).flagged);
        assert_eq!(s.get(Path::new(r"D:\Archive\Trip 2020")).label, 2);
        assert_eq!(s.get(Path::new(r"C:\Photos\Trip\a.jpg")).rating, 0);
        // A sibling folder sharing the name prefix is untouched.
        assert_eq!(s.get(Path::new(r"C:\Photos\Trip2\c.jpg")).rating, 5);
        // A plain file rename still works.
        s.moved(Path::new(r"C:\Photos\Trip2\c.jpg"), Path::new(r"C:\Photos\Trip2\d.jpg"));
        assert_eq!(s.get(Path::new(r"C:\Photos\Trip2\d.jpg")).rating, 5);
    }

    #[test]
    fn chained_batch_rename_keeps_each_tag() {
        let mut s = TagStore::default();
        s.update(Path::new(r"C:\x\1.jpg"), |t| t.rating = 1);
        s.update(Path::new(r"C:\x\2.jpg"), |t| t.rating = 2);
        s.moved_many(&[(PathBuf::from(r"C:\x\1.jpg"), PathBuf::from(r"C:\x\2.jpg")), (PathBuf::from(r"C:\x\2.jpg"), PathBuf::from(r"C:\x\3.jpg"))]);
        assert_eq!(s.get(Path::new(r"C:\x\1.jpg")).rating, 0);
        assert_eq!(s.get(Path::new(r"C:\x\2.jpg")).rating, 1);
        assert_eq!(s.get(Path::new(r"C:\x\3.jpg")).rating, 2);
    }

    #[test]
    fn tolerant_fields() {
        let s: TagStore = serde_json::from_str(r#"{"tags":{"c:\\a.jpg":{"rating":2}}}"#).unwrap();
        assert_eq!(s.get(Path::new(r"C:\a.jpg")).rating, 2);
        let s: TagStore = serde_json::from_str("{}").unwrap();
        assert!(s.tags.is_empty());
    }
}
