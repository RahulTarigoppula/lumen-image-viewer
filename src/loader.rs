//! Background loading of full-resolution images with a memory-bounded cache and
//! neighbour preloading so stepping through a folder is instant.

use crate::decode::{self, DecodeOptions, Decoded};
use crate::render::build_pyramid;
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// A preload (priority > 0) of a path evicted for the memory budget less than
/// this long ago is ignored, so images larger than the budget cannot cause a
/// decode / evict / re-request loop.
pub const EVICT_COOLDOWN: Duration = Duration::from_millis(4000);

pub struct LoadedImage {
    pub decoded: Decoded,
    pub pyramid: Vec<Arc<image::RgbaImage>>,
    pub load_ms: u128,
    pub file_size: u64,
    pub modified: Option<SystemTime>,
}

impl LoadedImage {
    pub fn bytes(&self) -> u64 {
        let mut b: u64 = self.pyramid.iter().map(|l| l.as_raw().len() as u64).sum();
        if let Some(frames) = &self.decoded.frames {
            b += frames.iter().map(|f| f.image.as_raw().len() as u64).sum::<u64>();
        }
        b
    }
}

pub enum LoadState {
    Loading,
    Preview(Arc<image::RgbaImage>),
    Ready(Arc<LoadedImage>),
    Failed(String),
}

struct Job {
    path: PathBuf,
    priority: u32, // lower = more urgent
    preview_side: Option<u32>,
    /// Generation of the cache entry this job fills; results for an older
    /// generation (the entry was invalidated while decoding) are dropped.
    generation: u64,
}

enum Msg {
    Preview(PathBuf, u64, image::RgbaImage),
    Done(PathBuf, u64, Result<LoadedImage, String>),
}

struct Entry {
    state: LoadState,
    touched: Instant,
    generation: u64,
    /// Priority of the latest request (0 = on screen).
    priority: u32,
}

struct Queue {
    jobs: Mutex<Vec<Job>>,
    cv: Condvar,
}

pub struct Loader {
    queue: Arc<Queue>,
    rx: Receiver<Msg>,
    entries: HashMap<PathBuf, Entry>,
    next_generation: u64,
    pub budget_bytes: u64,
    pub opts: Arc<Mutex<DecodeOptions>>,
    /// The last set passed to `retain_queued` (what the active view wants:
    /// the image on screen first, then its neighbours). Never evicted ahead
    /// of other entries.
    keep: Vec<PathBuf>,
    /// Paths evicted for the budget, and when.
    evicted: HashMap<PathBuf, Instant>,
}

impl Loader {
    pub fn new(ctx: egui::Context, threads: usize) -> Self {
        let queue = Arc::new(Queue { jobs: Mutex::new(Vec::new()), cv: Condvar::new() });
        let (tx, rx) = unbounded();
        let opts = Arc::new(Mutex::new(DecodeOptions::default()));
        for i in 0..threads.max(1) {
            let q = queue.clone();
            let tx: Sender<Msg> = tx.clone();
            let ctx = ctx.clone();
            let opts = opts.clone();
            std::thread::Builder::new()
                .name(format!("lumen-load-{i}"))
                .spawn(move || worker(q, tx, ctx, opts))
                .expect("spawn loader");
        }
        Loader { queue, rx, entries: HashMap::new(), next_generation: 1, budget_bytes: 1536 << 20, opts, keep: Vec::new(), evicted: HashMap::new() }
    }

    /// Request `path`. `priority` 0 = the image on screen.
    pub fn request(&mut self, path: &Path, priority: u32, preview_side: Option<u32>) {
        if let Some(e) = self.entries.get_mut(path) {
            // Already loading/loaded/failed (use `invalidate` to retry); bump priority if queued.
            // Called every frame for the image on screen, so only touch the queue lock
            // while the job can still be waiting in it.
            e.touched = Instant::now();
            e.priority = priority;
            if matches!(e.state, LoadState::Loading) {
                let mut jobs = self.queue.jobs.lock();
                if let Some(j) = jobs.iter_mut().find(|j| j.path == path) {
                    j.priority = j.priority.min(priority);
                }
            }
            return;
        }
        if priority > 0 && self.recently_evicted(path) {
            return;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Loading, touched: Instant::now(), generation, priority });
        let mut jobs = self.queue.jobs.lock();
        jobs.push(Job { path: path.to_path_buf(), priority, preview_side, generation });
        self.queue.cv.notify_one();
    }

    /// Whether `path` was evicted for the memory budget within [`EVICT_COOLDOWN`].
    pub fn recently_evicted(&self, path: &Path) -> bool {
        self.evicted.get(path).is_some_and(|t| t.elapsed() < EVICT_COOLDOWN)
    }

    /// Bytes held by `path` once decoded (None while loading / not cached).
    pub fn bytes_of(&self, path: &Path) -> Option<u64> {
        match self.entries.get(path) {
            Some(Entry { state: LoadState::Ready(i), .. }) => Some(i.bytes()),
            _ => None,
        }
    }

    /// Drop queued (not yet started) jobs except those listed. `keep` is also
    /// remembered as the protected set for the memory budget (most important
    /// first: the image on screen, then its neighbours).
    pub fn retain_queued(&mut self, keep: &[PathBuf]) {
        if self.keep.as_slice() != keep {
            self.keep = keep.to_vec();
        }
        let mut jobs = self.queue.jobs.lock();
        if jobs.iter().all(|j| keep.contains(&j.path)) {
            return; // common case (every frame): nothing to drop, no allocation
        }
        let removed: Vec<(PathBuf, u64)> = jobs.iter().filter(|j| !keep.contains(&j.path)).map(|j| (j.path.clone(), j.generation)).collect();
        jobs.retain(|j| keep.contains(&j.path));
        drop(jobs);
        for (p, g) in removed {
            if matches!(self.entries.get(&p), Some(e) if matches!(e.state, LoadState::Loading) && e.generation == g) {
                self.entries.remove(&p);
            }
        }
    }

    pub fn state(&self, path: &Path) -> Option<&LoadState> {
        self.entries.get(path).map(|e| &e.state)
    }

    pub fn get_ready(&self, path: &Path) -> Option<Arc<LoadedImage>> {
        match self.entries.get(path) {
            Some(Entry { state: LoadState::Ready(img), .. }) => Some(img.clone()),
            _ => None,
        }
    }

    /// Forget `path` so the next `request` decodes it again. A decode of the old
    /// version that is still running is ignored when it finishes.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.remove(path);
        self.queue.jobs.lock().retain(|j| j.path != path);
    }

    /// Drop every decoded image except `keep` (e.g. after the color management setting changed).
    pub fn clear_except(&mut self, keep: &[PathBuf]) {
        self.entries.retain(|p, _| keep.contains(p));
        self.queue.jobs.lock().clear();
    }

    /// Insert an already-decoded image. Only tests seed the cache this way; the
    /// app always decodes through `request`.
    #[cfg(test)]
    pub fn insert(&mut self, path: &Path, img: LoadedImage) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Ready(Arc::new(img)), touched: Instant::now(), generation, priority: u32::MAX });
    }

    /// Drain finished jobs. Returns paths whose state changed.
    pub fn poll(&mut self, protect: &[PathBuf]) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Preview(p, g, img) => {
                    if let Some(e) = self.entries.get_mut(&p) {
                        if e.generation == g && matches!(e.state, LoadState::Loading) {
                            e.state = LoadState::Preview(Arc::new(img));
                            changed.push(p);
                        }
                    }
                }
                Msg::Done(p, g, res) => {
                    // Ignore results for entries that were dropped or invalidated meanwhile.
                    if let Some(e) = self.entries.get_mut(&p) {
                        if e.generation == g {
                            e.state = match res {
                                Ok(img) => LoadState::Ready(Arc::new(img)),
                                Err(err) => LoadState::Failed(err),
                            };
                            e.touched = Instant::now();
                            changed.push(p);
                        }
                    }
                }
            }
        }
        if !changed.is_empty() {
            self.enforce_budget(protect);
        }
        changed
    }

    /// Evict least recently used entries until the cache fits the budget.
    /// Never evicted: `protect` and kept paths last requested at priority 0
    /// (on screen). Other kept paths (preloaded neighbours) go only after
    /// every unprotected entry, least urgent first.
    fn enforce_budget(&mut self, protect: &[PathBuf]) {
        let mut total: u64 = self
            .entries
            .values()
            .map(|e| if let LoadState::Ready(i) = &e.state { i.bytes() } else { 0 })
            .sum();
        if total <= self.budget_bytes {
            return;
        }
        let mut order: Vec<PathBuf> = Vec::new();
        for p in protect.iter().chain(self.keep.iter()) {
            if !order.contains(p) {
                order.push(p.clone());
            }
        }
        let mut ready: Vec<(PathBuf, Instant, u64)> = self
            .entries
            .iter()
            .filter_map(|(p, e)| match &e.state {
                LoadState::Ready(i) if !order.contains(p) => Some((p.clone(), e.touched, i.bytes())),
                _ => None,
            })
            .collect();
        ready.sort_by_key(|(_, t, _)| *t);
        // Then the kept neighbours, least urgent first.
        let mut neighbours: Vec<(u32, PathBuf, u64)> = order
            .iter()
            .filter(|p| !protect.contains(p))
            .filter_map(|p| match self.entries.get(p) {
                Some(Entry { state: LoadState::Ready(i), priority, .. }) if *priority > 0 => Some((*priority, p.clone(), i.bytes())),
                _ => None,
            })
            .collect();
        neighbours.sort_by_key(|n| std::cmp::Reverse(n.0));
        let now = Instant::now();
        ready.extend(neighbours.into_iter().map(|(_, p, b)| (p, now, b)));
        self.evicted.retain(|_, t| now.duration_since(*t) < EVICT_COOLDOWN);
        for (p, _, b) in ready {
            if total <= self.budget_bytes {
                break;
            }
            self.entries.remove(&p);
            self.evicted.insert(p, now);
            total -= b;
        }
    }
}

fn worker(q: Arc<Queue>, tx: Sender<Msg>, ctx: egui::Context, opts: Arc<Mutex<DecodeOptions>>) {
    loop {
        let job = {
            let mut jobs = q.jobs.lock();
            while jobs.is_empty() {
                q.cv.wait(&mut jobs);
            }
            // Most urgent first; among equals, the most recently requested.
            let mut best = 0;
            for (i, j) in jobs.iter().enumerate() {
                if j.priority <= jobs[best].priority {
                    best = i;
                }
            }
            jobs.remove(best)
        };
        let started = Instant::now();
        let meta = std::fs::metadata(&job.path).ok();
        let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        // Instant low-res first paint for big files on screen.
        if let Some(side) = job.preview_side {
            if file_size > 6 << 20 {
                if let Ok(prev) = decode::decode_thumbnail(&job.path, side) {
                    let _ = tx.send(Msg::Preview(job.path.clone(), job.generation, prev));
                    ctx.request_repaint();
                }
            }
        }
        let o = *opts.lock();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode::decode_file(&job.path, o)));
        let res = match res {
            Ok(Ok(decoded)) => {
                let pyramid = build_pyramid(decoded.image.clone());
                Ok(LoadedImage {
                    decoded,
                    pyramid,
                    load_ms: started.elapsed().as_millis(),
                    file_size,
                    modified: meta.and_then(|m| m.modified().ok()),
                })
            }
            Ok(Err(e)) => Err(decode::friendly_error(&e)),
            Err(_) => Err("The decoder crashed on this file (it may be corrupt).".into()),
        };
        let _ = tx.send(Msg::Done(job.path, job.generation, res));
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(side: u32) -> LoadedImage {
        let rgba = image::RgbaImage::new(side, side);
        let pyramid = vec![Arc::new(rgba.clone())];
        LoadedImage { decoded: Decoded::from_rgba(rgba, "test"), pyramid, load_ms: 0, file_size: 0, modified: None }
    }

    #[test]
    fn budget_protects_keep_set_and_cools_down_evicted() {
        let mut l = Loader::new(egui::Context::default(), 1);
        // 10x10 RGBA = 400 bytes each; room for 2.5 of them.
        l.budget_bytes = 1000;
        let p = |n: &str| PathBuf::from(format!("Z:/lumen-test/{n}.png"));
        let (cur, next, prev, old) = (p("cur"), p("next"), p("prev"), p("old"));
        for q in [&old, &prev, &next, &cur] {
            l.insert(q, img(10));
        }
        // What the viewer does every frame: request with priorities, then keep.
        l.request(&cur, 0, None);
        l.request(&next, 1, None);
        l.request(&prev, 2, None);
        l.retain_queued(&[cur.clone(), next.clone(), prev.clone()]);
        l.enforce_budget(std::slice::from_ref(&cur));
        // Unprotected first, then the least urgent neighbour; never the current.
        assert!(l.get_ready(&old).is_none());
        assert!(l.get_ready(&prev).is_none());
        assert!(l.get_ready(&cur).is_some());
        assert!(l.get_ready(&next).is_some());
        // Evicted neighbours are not re-queued right away (no decode loop) ...
        assert!(l.recently_evicted(&prev));
        l.request(&prev, 2, None);
        assert!(l.state(&prev).is_none());
        // ... but an explicit on-screen request always goes through.
        l.request(&prev, 0, None);
        assert!(matches!(l.state(&prev), Some(LoadState::Loading)));
    }

    #[test]
    fn explicit_protect_list_is_never_evicted() {
        let mut l = Loader::new(egui::Context::default(), 1);
        l.budget_bytes = 100; // smaller than any single image
        let (a, b) = (PathBuf::from("Z:/lumen-test/a.png"), PathBuf::from("Z:/lumen-test/b.png"));
        l.insert(&a, img(10));
        l.insert(&b, img(10));
        l.enforce_budget(&[a.clone(), b.clone()]);
        assert!(l.get_ready(&a).is_some() && l.get_ready(&b).is_some());
        assert_eq!(l.bytes_of(&a), Some(400));
    }
}
