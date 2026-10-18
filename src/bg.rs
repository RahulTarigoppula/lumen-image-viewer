//! Small helper for "compute this off the UI thread, show the result when
//! ready" values such as histograms and EXIF panels.

use crossbeam_channel::Receiver;

/// Holds the latest value for a key and recomputes it in the background when
/// the key changes. The previous value stays visible until the new one lands,
/// which avoids flicker while stepping through images.
pub struct BgSlot<K: PartialEq + Clone + Send + 'static, V: Send + 'static> {
    value: Option<(K, V)>,
    pending: Option<(K, Receiver<V>)>,
}

impl<K: PartialEq + Clone + Send + 'static, V: Send + 'static> Default for BgSlot<K, V> {
    fn default() -> Self {
        BgSlot { value: None, pending: None }
    }
}

impl<K: PartialEq + Clone + Send + 'static, V: Send + 'static> BgSlot<K, V> {
    /// Return the value for `key` (or the stale previous one while computing).
    /// `job` runs on the rayon pool only when `key` is new.
    pub fn get(&mut self, key: &K, ctx: &egui::Context, job: impl FnOnce() -> V + Send + 'static) -> Option<&V> {
        if let Some((k, rx)) = &self.pending {
            if let Ok(v) = rx.try_recv() {
                self.value = Some((k.clone(), v));
                self.pending = None;
            }
        }
        let fresh = self.value.as_ref().map(|(k, _)| k == key).unwrap_or(false);
        let in_flight = self.pending.as_ref().map(|(k, _)| k == key).unwrap_or(false);
        if !fresh && !in_flight {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx = ctx.clone();
            rayon::spawn(move || {
                let _ = tx.send(job());
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
}
