//! Writes of Lumen's own data files (settings, tags, custom orders). Each file
//! is replaced atomically with its new content flushed to disk first, so a
//! crash or power cut leaves the old or the new file, never an empty one; and
//! the periodic saves happen on a background thread, not the UI thread.

use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Write {
    path: PathBuf,
    bytes: Vec<u8>,
    /// Set when the write fails, so the owner saves again later.
    failed: Option<Arc<AtomicBool>>,
}

struct State {
    queue: VecDeque<Write>,
    /// A write is in progress on the writer thread.
    busy: bool,
    started: bool,
}

static STATE: Mutex<State> = parking_lot::const_mutex(State { queue: VecDeque::new(), busy: false, started: false });
static CHANGED: Condvar = Condvar::new();

/// Write `bytes` to `path` now (parent folders are created), atomically and durably.
pub fn write_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    crate::encode::write_atomic(path, bytes)
}

/// Queue `bytes` to be written to `path` on the background writer thread. A
/// write still waiting for the same file is replaced: only the newest content
/// matters. `failed` is set if the write fails.
pub fn write_later(path: PathBuf, bytes: Vec<u8>, failed: Option<&Arc<AtomicBool>>) {
    let failed = failed.cloned();
    let mut st = STATE.lock();
    if !st.started {
        let spawned = std::thread::Builder::new().name("lumen-save".into()).spawn(writer);
        if spawned.is_err() {
            drop(st);
            if write_file(&path, &bytes).is_err() {
                if let Some(f) = failed {
                    f.store(true, Ordering::Relaxed);
                }
            }
            return;
        }
        st.started = true;
    }
    match st.queue.iter_mut().find(|w| w.path == path) {
        Some(w) => {
            w.bytes = bytes;
            w.failed = failed;
        }
        None => st.queue.push_back(Write { path, bytes, failed }),
    }
    drop(st);
    CHANGED.notify_all();
}

/// Wait until every queued write has finished (call before a final save).
pub fn flush() {
    let mut st = STATE.lock();
    while st.busy || !st.queue.is_empty() {
        CHANGED.wait(&mut st);
    }
}

/// Write `bytes` to `path` on this thread, after any queued writes (so an older
/// queued content can never land after this one).
pub fn write_now(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    flush();
    write_file(path, bytes)
}

fn writer() {
    loop {
        let w = {
            let mut st = STATE.lock();
            loop {
                if let Some(w) = st.queue.pop_front() {
                    st.busy = true;
                    break w;
                }
                CHANGED.wait(&mut st);
            }
        };
        if let Err(e) = write_file(&w.path, &w.bytes) {
            log::warn!("could not save {}: {e:#}", w.path.display());
            if let Some(f) = &w.failed {
                f.store(true, Ordering::Relaxed);
            }
        }
        STATE.lock().busy = false;
        CHANGED.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_writes_land_and_flush_waits() {
        let d = std::env::temp_dir().join(format!("lumen-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let p = d.join("sub").join("tags.json");
        let failed = Arc::new(AtomicBool::new(false));
        for i in 0..20 {
            write_later(p.clone(), format!("{{\"n\":{i}}}").into_bytes(), Some(&failed));
        }
        flush();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"n\":19}", "the newest content wins");
        assert!(!failed.load(Ordering::Relaxed));
        write_now(&p, b"final").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"final");
        // A file that cannot be written reports it.
        let blocked = d.join("blocked");
        std::fs::create_dir_all(blocked.join("x.json")).unwrap();
        write_later(blocked.join("x.json"), b"{}".to_vec(), Some(&failed));
        flush();
        assert!(failed.load(Ordering::Relaxed));
        let names: Vec<String> = std::fs::read_dir(d.join("sub")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["tags.json".to_string()], "no temp files left");
        let _ = std::fs::remove_dir_all(&d);
    }
}
