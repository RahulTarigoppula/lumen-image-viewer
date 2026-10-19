//! On-disk result cache: blake3 keys over everything that affects a result,
//! one JSON file per key in a caller-provided directory, LRU eviction by
//! total size (file modification time is the recency stamp; hits refresh it).

use crate::frame::Roi;
use crate::report::FrameRange;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Default cache size limit (200 MB).
pub const DEFAULT_MAX_BYTES: u64 = 200 << 20;

/// 32-byte blake3 cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(pub [u8; 32]);

impl CacheKey {
    pub fn builder() -> KeyBuilder {
        KeyBuilder::default()
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Builds a key from tagged, length-prefixed fields (so field boundaries
/// cannot alias).
#[derive(Default)]
pub struct KeyBuilder {
    h: blake3::Hasher,
}

impl KeyBuilder {
    fn field(mut self, tag: &str, bytes: &[u8]) -> Self {
        self.h.update(tag.as_bytes());
        self.h.update(&(bytes.len() as u64).to_le_bytes());
        self.h.update(bytes);
        self
    }

    /// A source file: canonical path, size and modification time.
    pub fn file(self, tag: &str, path: &Path) -> Self {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let meta = std::fs::metadata(path).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        self.field(tag, canon.to_string_lossy().to_lowercase().as_bytes()).field("size", &size.to_le_bytes()).field("mtime", &mtime.to_le_bytes())
    }

    pub fn metric(self, id: &str, version: u32) -> Self {
        self.field("metric", id.as_bytes()).field("ver", &version.to_le_bytes())
    }

    /// Parameters as JSON (e.g. `MetricOptions`).
    pub fn params<T: Serialize>(self, p: &T) -> Self {
        let s = serde_json::to_vec(p).unwrap_or_default();
        self.field("params", &s)
    }

    pub fn roi(self, roi: Option<Roi>) -> Self {
        let b = match roi {
            Some(r) => [r.x, r.y, r.w, r.h].iter().flat_map(|v| v.to_le_bytes()).collect(),
            None => Vec::new(),
        };
        self.field("roi", &b)
    }

    pub fn range(self, r: Option<FrameRange>) -> Self {
        let b = match r {
            Some(r) => [r.start, r.end, r.stride].iter().flat_map(|v| v.to_le_bytes()).collect(),
            None => Vec::new(),
        };
        self.field("range", &b)
    }

    /// Any other identity (e.g. B's file key, "rescaled", frame offset).
    pub fn extra(self, tag: &str, bytes: &[u8]) -> Self {
        self.field(tag, bytes)
    }

    pub fn finish(self) -> CacheKey {
        CacheKey(*self.h.finalize().as_bytes())
    }
}

/// JSON files `<dir>/<key>.json` with an LRU size limit.
pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
}

impl DiskCache {
    /// `dir` is created on first write (e.g. `cache_dir()/analysis`).
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        DiskCache { dir: dir.into(), max_bytes }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{}.json", key.hex()))
    }

    /// The cached value, refreshing its recency. Unreadable entries are removed.
    pub fn get<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<T> {
        let p = self.path(key);
        let bytes = std::fs::read(&p).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(v) => {
                if let Ok(f) = std::fs::File::options().write(true).open(&p) {
                    let _ = f.set_modified(SystemTime::now());
                }
                Some(v)
            }
            Err(_) => {
                let _ = std::fs::remove_file(&p);
                None
            }
        }
    }

    /// Stores `value` (atomic write), then evicts least recently used entries
    /// until the cache fits its limit.
    pub fn put<T: Serialize>(&self, key: &CacheKey, value: &T) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
        crate::report::write_atomic(&self.path(key), &bytes)?;
        self.evict(Some(key));
        Ok(())
    }

    pub fn remove(&self, key: &CacheKey) {
        let _ = std::fs::remove_file(self.path(key));
    }

    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        rd.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((e.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
            })
            .collect()
    }

    /// Total bytes of cache entries.
    pub fn total_size(&self) -> u64 {
        self.entries().iter().map(|e| e.1).sum()
    }

    /// Deletes oldest entries until the total is within the limit, never `keep`.
    pub fn evict(&self, keep: Option<&CacheKey>) {
        let mut e = self.entries();
        let mut total: u64 = e.iter().map(|x| x.1).sum();
        if total <= self.max_bytes {
            return;
        }
        let keep = keep.map(|k| self.path(k));
        e.sort_by_key(|x| x.2);
        for (p, len, _) in e {
            if total <= self.max_bytes {
                break;
            }
            if Some(&p) == keep.as_ref() {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
            }
        }
    }

    /// Removes every entry (Settings > Privacy "Clear analysis history").
    pub fn clear(&self) {
        for (p, ..) in self.entries() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_differ_by_field() {
        let a = CacheKey::builder().metric("psnr", 1).roi(None).finish();
        let b = CacheKey::builder().metric("psnr", 2).roi(None).finish();
        let c = CacheKey::builder().metric("psnr", 1).roi(Some(Roi::new(0, 0, 8, 8))).finish();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, CacheKey::builder().metric("psnr", 1).roi(None).finish());
        assert_eq!(a.hex().len(), 64);
    }

    #[test]
    fn put_get_and_lru_eviction() {
        let dir = std::env::temp_dir().join(format!("lumen-analysis-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DiskCache::new(&dir, 4500);
        let blob = |n: u8| vec![n; 1000];
        let k: Vec<CacheKey> = (0..3u8).map(|i| CacheKey::builder().extra("i", &[i]).finish()).collect();
        cache.put(&k[0], &blob(0)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        cache.put(&k[1], &blob(1)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Touch k0 so k1 becomes the least recently used.
        assert_eq!(cache.get::<Vec<u8>>(&k[0]).unwrap(), blob(0));
        std::thread::sleep(std::time::Duration::from_millis(30));
        cache.put(&k[2], &blob(2)).unwrap();
        assert!(cache.total_size() <= 4500, "{}", cache.total_size());
        assert!(cache.get::<Vec<u8>>(&k[2]).is_some());
        assert!(cache.get::<Vec<u8>>(&k[0]).is_some());
        assert!(cache.get::<Vec<u8>>(&k[1]).is_none(), "LRU entry evicted");
        cache.clear();
        assert_eq!(cache.total_size(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
