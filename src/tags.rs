//! Local ratings, flags (FastStone-style "tags") and color labels. Stored in a
//! private JSON file in the app data folder - never written into your images.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
        std::fs::read_to_string(Self::file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
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
    pub fn moved(&mut self, from: &Path, to: &Path) {
        if let Some(t) = self.tags.remove(&key(from)) {
            self.tags.insert(key(to), t);
            self.dirty = true;
        }
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
