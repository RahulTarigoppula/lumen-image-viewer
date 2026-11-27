//! Small helper for "compute this off the UI thread, show the result when
//! ready" values such as histograms and EXIF panels.

use crossbeam_channel::Receiver;

/// Run the calling worker thread just below normal priority, so decoding and
/// thumbnail work never delays a frame of the UI thread (scrolling, zooming
/// and stepping stay smooth while all cores are busy).
pub fn background_priority() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        // SAFETY: changes only the calling thread's priority.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
}

/// Run `job`, turning a panic into `None`. A panic must not leave a
/// `rayon::spawn` job: rayon aborts the whole app then.
pub fn catch<T>(job: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).ok()
}

/// Holds the latest value for a key and recomputes it in the background when
/// the key changes. The previous value stays visible until the new one lands,
/// which avoids flicker while stepping through images.
pub struct BgSlot<K: PartialEq + Clone + Send + 'static, V: Send + 'static> {
    value: Option<(K, V)>,
    /// `None` arrives when the job failed (panicked).
    pending: Option<(K, Receiver<Option<V>>)>,
    /// The key whose job failed: not tried again until the key changes.
    failed: Option<K>,
}

impl<K: PartialEq + Clone + Send + 'static, V: Send + 'static> Default for BgSlot<K, V> {
    fn default() -> Self {
        BgSlot { value: None, pending: None, failed: None }
    }
}

impl<K: PartialEq + Clone + Send + 'static, V: Send + 'static> BgSlot<K, V> {
    /// Return the value for `key` (or the stale previous one while computing).
    /// `job` runs on the rayon pool only when `key` is new. A job that panics
    /// gives no value (and is not run again for that key).
    pub fn get(&mut self, key: &K, ctx: &egui::Context, job: impl FnOnce() -> V + Send + 'static) -> Option<&V> {
        if let Some((k, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(Some(v)) => {
                    self.value = Some((k.clone(), v));
                    self.pending = None;
                }
                Ok(None) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // Not the previous key's value either: show nothing.
                    self.failed = Some(k.clone());
                    self.value = None;
                    self.pending = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        let fresh = self.value.as_ref().map(|(k, _)| k == key).unwrap_or(false);
        let in_flight = self.pending.as_ref().map(|(k, _)| k == key).unwrap_or(false);
        let failed = self.failed.as_ref() == Some(key);
        if !fresh && !in_flight && !failed {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx = ctx.clone();
            rayon::spawn(move || {
                let _ = tx.send(catch(job));
                ctx.request_repaint();
            });
            self.pending = Some((key.clone(), rx));
        }
        self.value.as_ref().map(|(_, v)| v)
    }

    /// True when the shown value matches `key`.
    pub fn is_current(&self, key: &K) -> bool {
        self.value.as_ref().map(|(k, _)| k == key).unwrap_or(false)
    }

    /// The value for exactly `key`, never a stale one (see [`BgSlot::get`]).
    pub fn current(&self, key: &K) -> Option<&V> {
        self.value.as_ref().filter(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Log a panic that escaped a job on the global rayon pool instead of
/// aborting the app (rayon's default).
pub fn rayon_panic(p: Box<dyn std::any::Any + Send>) {
    let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
    log::error!("a background job failed: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_panicking_job_gives_no_value_and_runs_once() {
        let ctx = egui::Context::default();
        let mut slot: BgSlot<u32, u32> = BgSlot::default();
        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..1000 {
            let r = runs.clone();
            let got = slot.get(&1, &ctx, move || {
                r.fetch_add(1, Ordering::SeqCst);
                panic!("decoder bug");
            });
            assert!(got.is_none());
            if slot.failed.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(slot.failed, Some(1));
        assert!(slot.get(&1, &ctx, || 9).is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a failed key is not run again");
        // Another key works.
        let mut v = None;
        for _ in 0..1000 {
            v = slot.get(&2, &ctx, || 7).copied();
            if v.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(v, Some(7));
        assert_eq!(catch(|| 3), Some(3));
        assert_eq!(catch(|| -> u32 { panic!("x") }), None);
    }
}
