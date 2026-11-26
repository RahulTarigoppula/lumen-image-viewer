//! Last-used settings of the edit tools (and of Save as), kept in `tools.json`
//! in the data folder: a tool opens with the values it was last applied with,
//! and Reset goes back to its defaults. A missing or unreadable file means
//! defaults; a write goes to a temporary file that then replaces the old one.
//!
//! Only what the user chooses is kept, never what comes from the image (the
//! pixel size in Resize, the source size...): those fields are `#[serde(skip)]`.

use super::EditTool;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// The file in the data folder.
fn file() -> PathBuf {
    crate::settings::data_dir().join("tools.json")
}

/// Every entry of the file at `path`; none when it is missing or unreadable.
pub fn load(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Map::new() };
    match serde_json::from_str(&text) {
        Ok(Value::Object(all)) => all,
        _ => {
            log::warn!("{} is unreadable; the tools open with their defaults", path.display());
            Map::new()
        }
    }
}

/// Set `key` to `value` in the file at `path`, keeping the other entries
/// (an unreadable file is replaced).
pub fn store(path: &Path, key: &str, value: Value) -> std::io::Result<()> {
    let mut all = load(path);
    if all.get(key) == Some(&value) {
        return Ok(());
    }
    all.insert(key.to_string(), value);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(all))?;
    // Write a temporary file first so a crash mid-write never leaves half a file.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The value remembered under `key` (in the data folder).
pub fn get(key: &str) -> Option<Value> {
    load(&file()).remove(key)
}

/// Remember `value` under `key` (in the data folder); a failure is only logged.
pub fn set(key: &str, value: Value) {
    if let Err(e) = store(&file(), key, value) {
        log::warn!("could not save the last-used tool settings: {e}");
    }
}

/// Give a newly made tool the settings it was last applied with, from `all`.
pub fn recall_from(tool: &mut dyn EditTool, all: &Map<String, Value>) {
    if let Some(v) = tool.memory_key().and_then(|k| all.get(k)) {
        tool.recall(v);
    }
}

/// Give a newly made tool the settings it was last applied with.
pub fn recall_tool(tool: &mut dyn EditTool) {
    if tool.memory_key().is_some() {
        recall_from(tool, &load(&file()));
    }
}

/// Remember the settings of a tool that was just applied.
pub fn remember_tool(tool: &dyn EditTool) {
    if let (Some(key), Some(v)) = (tool.memory_key(), tool.remember()) {
        set(key, v);
    }
}

/// `t` as JSON (its fields that are not `#[serde(skip)]`).
pub fn to_value<T: Serialize>(t: &T) -> Option<Value> {
    serde_json::to_value(t).ok()
}

/// A `T` read from `v`: missing fields keep `T`'s defaults (with
/// `#[serde(default)]`); `None` when a value has the wrong type.
pub fn from_value<T: DeserializeOwned>(v: &Value) -> Option<T> {
    <T as Deserialize>::deserialize(v).ok()
}

/// Keep a recalled number inside the range its control allows (the file may
/// have been edited by hand).
pub fn clamp(v: &mut f32, range: RangeInclusive<f32>) {
    *v = if v.is_nan() { *range.start() } else { v.clamp(*range.start(), *range.end()) };
}

/// A tool whose remembered settings are its serialized fields; what comes from
/// the image and passing UI state are `#[serde(skip)]`. Its [`EditTool`]
/// memory methods come from [`remember_methods`].
pub trait Remember: Serialize + DeserializeOwned + Default {
    /// Name in `tools.json`.
    const KEY: &'static str;
    /// Called after the remembered fields are read into a new tool: keep them
    /// inside what the controls allow and rebuild what depends on them.
    fn recalled(&mut self) {}
    /// What Reset makes of the tool: the defaults, unless it keeps something
    /// (the Watermark logo).
    fn reset_to(&self) -> Self {
        Self::default()
    }
}

/// Read `v` into a newly made tool, keeping its defaults when `v` does not fit.
pub fn recall_into<T: Remember>(tool: &mut T, v: &Value) {
    if let Some(mut t) = from_value::<T>(v) {
        t.recalled();
        *tool = t;
    }
}

/// Whether the remembered settings of `tool` differ from what Reset gives
/// (Reset then has something to do although nothing changed since it opened).
pub fn differs_from_defaults<T: Remember>(tool: &T) -> bool {
    to_value(tool) != to_value(&tool.reset_to())
}

/// The [`EditTool`] memory methods (`memory_key`, `remember`, `recall`,
/// `differs_from_defaults`) of a tool that implements [`Remember`].
macro_rules! remember_methods {
    () => {
        fn memory_key(&self) -> Option<&'static str> {
            Some(<Self as $crate::edit::memory::Remember>::KEY)
        }
        fn remember(&self) -> Option<::serde_json::Value> {
            $crate::edit::memory::to_value(self)
        }
        fn recall(&mut self, v: &::serde_json::Value) {
            $crate::edit::memory::recall_into(self, v)
        }
        fn differs_from_defaults(&self) -> bool {
            $crate::edit::memory::differs_from_defaults(self)
        }
    };
}
pub(crate) use remember_methods;

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A file in the temp folder that is removed when the test ends.
    pub struct TempFile(pub PathBuf);
    impl TempFile {
        pub fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("lumen-test-{}-{name}.json", std::process::id()));
            let _ = std::fs::remove_file(&p);
            TempFile(p)
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn store_keeps_other_entries() {
        let f = TempFile::new("store");
        assert!(load(&f.0).is_empty(), "missing file: nothing remembered");
        store(&f.0, "a", serde_json::json!({"x": 1})).unwrap();
        store(&f.0, "b", serde_json::json!([1, 2])).unwrap();
        store(&f.0, "a", serde_json::json!({"x": 2})).unwrap();
        let all = load(&f.0);
        assert_eq!(all.len(), 2);
        assert_eq!(all["a"]["x"], 2);
        assert_eq!(all["b"][1], 2);
        // No temporary file is left behind.
        let tmp = f.0.with_extension(format!("json.{}.tmp", std::process::id()));
        assert!(!tmp.exists());
    }

    #[test]
    fn unreadable_file_is_empty_and_replaced() {
        let f = TempFile::new("corrupt");
        for junk in ["{ not json", "[1, 2, 3]", "", "\u{0}\u{1}"] {
            std::fs::write(&f.0, junk).unwrap();
            assert!(load(&f.0).is_empty(), "{junk:?}");
        }
        store(&f.0, "k", Value::Bool(true)).unwrap();
        assert_eq!(load(&f.0)["k"], Value::Bool(true));
    }

    #[test]
    fn clamp_limits_and_handles_nan() {
        let mut v = 500.0;
        clamp(&mut v, -100.0..=100.0);
        assert_eq!(v, 100.0);
        v = f32::NAN;
        clamp(&mut v, 1.0..=4.0);
        assert_eq!(v, 1.0);
        v = f32::NEG_INFINITY;
        clamp(&mut v, 0.0..=1.0);
        assert_eq!(v, 0.0);
    }
}
