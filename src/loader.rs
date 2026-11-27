//! Background loading of full-resolution images with a memory-bounded cache and
//! neighbour preloading so stepping through a folder is instant.

use crate::decode::{self, DecodeOptions, Decoded};
use crate::render::build_pyramid;
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// Set when the image is no longer wanted: the decode stops at its next stage.
    cancel: Arc<AtomicBool>,
    /// The latest priority asked for (0 = on screen), shared with the entry, so a
    /// running decode knows whether it is still a preload.
    live: Arc<AtomicU32>,
}

enum Msg {
    Preview(PathBuf, u64, image::RgbaImage),
    /// The decoded image; true when more pages follow (`Pages`).
    Done(PathBuf, u64, Result<LoadedImage, String>, bool),
    /// All pages of a multi-page TIFF whose first page came with `Done`
    /// (None: they could not be decoded).
    Pages(PathBuf, u64, Option<Arc<Vec<decode::AnimFrame>>>),
}

struct Entry {
    state: LoadState,
    touched: Instant,
    generation: u64,
    /// Priority of the latest request (0 = on screen).
    priority: u32,
    /// Shared with the job decoding this entry (see `Job`).
    cancel: Arc<AtomicBool>,
    live: Arc<AtomicU32>,
    /// Ready with its first page; the other pages are still being decoded.
    pages_pending: bool,
}

impl Entry {
    fn loading(&self) -> bool {
        matches!(self.state, LoadState::Loading | LoadState::Preview(_))
    }

    /// Its job is still running (decoding the image, or its other pages).
    fn busy(&self) -> bool {
        self.loading() || self.pages_pending
    }
}

struct QueueState {
    jobs: Vec<Job>,
    /// Latest priorities of the jobs being decoded right now.
    running: Vec<Arc<AtomicU32>>,
}

struct Queue {
    jobs: Mutex<QueueState>,
    cv: Condvar,
    workers: usize,
}

/// The job a free worker should take: the most urgent (among equals the most
/// recently requested), but a preload (priority > 0) only while another worker
/// stays free for the image on screen. `busy_preloads` counts running decodes
/// that are preloads now.
fn pick_job(priorities: impl Iterator<Item = u32>, busy_preloads: usize, workers: usize) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for (i, p) in priorities.enumerate() {
        if best.is_none_or(|(_, b)| p <= b) {
            best = Some((i, p));
        }
    }
    let (i, p) = best?;
    if p > 0 && busy_preloads >= workers.saturating_sub(1).max(1) {
        return None;
    }
    Some(i)
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
        let queue = Arc::new(Queue { jobs: Mutex::new(QueueState { jobs: Vec::new(), running: Vec::new() }), cv: Condvar::new(), workers: threads.max(1) });
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
            e.live.store(priority, Ordering::Relaxed);
            if matches!(e.state, LoadState::Loading) {
                let mut q = self.queue.jobs.lock();
                if let Some(j) = q.jobs.iter_mut().find(|j| j.path == path) {
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
        let (cancel, live) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU32::new(priority)));
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Loading, touched: Instant::now(), generation, priority, cancel: cancel.clone(), live: live.clone(), pages_pending: false });
        let mut q = self.queue.jobs.lock();
        q.jobs.push(Job { path: path.to_path_buf(), priority, preview_side, generation, cancel, live });
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

    /// Drop jobs except those listed: queued ones at once, decodes already
    /// running at their next stage (holding an arrow key skips through images,
    /// and the one on screen must not wait behind the ones skipped). `keep` is
    /// also remembered as the protected set for the memory budget (most
    /// important first: the image on screen, then its neighbours).
    pub fn retain_queued(&mut self, keep: &[PathBuf]) {
        if self.keep.as_slice() != keep {
            self.keep = keep.to_vec();
        }
        let mut q = self.queue.jobs.lock();
        let queued_ok = q.jobs.iter().all(|j| keep.contains(&j.path));
        let running_ok = self.entries.iter().all(|(p, e)| !e.busy() || keep.contains(p));
        if queued_ok && running_ok {
            return; // common case (every frame): nothing to drop, no allocation
        }
        q.jobs.retain(|j| keep.contains(&j.path));
        drop(q);
        // Queued or running, an unwanted load goes: a late result is dropped
        // (its generation is gone) and a later request starts over. So does an
        // image whose other pages are still being decoded.
        self.entries.retain(|p, e| {
            let unwanted = e.busy() && !keep.contains(p);
            if unwanted {
                e.cancel.store(true, Ordering::Relaxed);
            }
            !unwanted
        });
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
        if let Some(e) = self.entries.remove(path) {
            e.cancel.store(true, Ordering::Relaxed);
        }
        self.queue.jobs.lock().jobs.retain(|j| j.path != path);
    }

    /// Drop every decoded image except `keep` (e.g. after the color management setting changed).
    pub fn clear_except(&mut self, keep: &[PathBuf]) {
        self.entries.retain(|p, e| {
            let kept = keep.contains(p);
            if !kept {
                e.cancel.store(true, Ordering::Relaxed);
            }
            kept
        });
        self.queue.jobs.lock().jobs.clear();
    }

    /// Insert an already-decoded image. Only tests seed the cache this way; the
    /// app always decodes through `request`.
    #[cfg(test)]
    pub fn insert(&mut self, path: &Path, img: LoadedImage) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Ready(Arc::new(img)), touched: Instant::now(), generation, priority: u32::MAX, cancel: Default::default(), live: Default::default(), pages_pending: false });
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
                Msg::Done(p, g, res, more) => {
                    // Ignore results for entries that were dropped or invalidated meanwhile.
                    if let Some(e) = self.entries.get_mut(&p) {
                        if e.generation == g {
                            e.pages_pending = more && res.is_ok();
                            e.state = match res {
                                Ok(img) => LoadState::Ready(Arc::new(img)),
                                Err(err) => LoadState::Failed(err),
                            };
                            e.touched = Instant::now();
                            changed.push(p);
                        }
                    }
                }
                Msg::Pages(p, g, frames) => {
                    if let Some(e) = self.entries.get_mut(&p) {
                        if e.generation == g {
                            e.pages_pending = false;
                            // The same image with its pages (the pyramid is shared, so
                            // the viewer keeps its textures).
                            let paged = match (frames, &e.state) {
                                (Some(frames), LoadState::Ready(img)) => {
                                    let mut decoded = img.decoded.clone();
                                    decoded.frames = Some(frames);
                                    Some(LoadedImage { decoded, pyramid: img.pyramid.clone(), load_ms: img.load_ms, file_size: img.file_size, modified: img.modified })
                                }
                                _ => None,
                            };
                            if let Some(paged) = paged {
                                e.state = LoadState::Ready(Arc::new(paged));
                                changed.push(p);
                            }
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
            // Its other pages, if still being decoded, are not wanted either.
            if let Some(e) = self.entries.remove(&p) {
                e.cancel.store(true, Ordering::Relaxed);
            }
            self.evicted.insert(p, now);
            total -= b;
        }
    }
}

fn worker(q: Arc<Queue>, tx: Sender<Msg>, ctx: egui::Context, opts: Arc<Mutex<DecodeOptions>>) {
    crate::bg::background_priority();
    loop {
        let job = {
            let mut st = q.jobs.lock();
            loop {
                let busy = st.running.iter().filter(|p| p.load(Ordering::Relaxed) > 0).count();
                if let Some(i) = pick_job(st.jobs.iter().map(|j| j.priority), busy, q.workers) {
                    let job = st.jobs.remove(i);
                    st.running.push(job.live.clone());
                    break job;
                }
                q.cv.wait(&mut st);
            }
        };
        let live = job.live.clone();
        if let Some(msg) = run_job(job, &opts, &tx, &ctx) {
            let _ = tx.send(msg);
            ctx.request_repaint();
        }
        let mut st = q.jobs.lock();
        if let Some(i) = st.running.iter().position(|p| Arc::ptr_eq(p, &live)) {
            st.running.swap_remove(i);
        }
        drop(st);
        // A preload may have been waiting for this worker.
        q.cv.notify_one();
    }
}

/// Decode one job. None when it was cancelled (nobody wants the result).
fn run_job(job: Job, opts: &Mutex<DecodeOptions>, tx: &Sender<Msg>, ctx: &egui::Context) -> Option<Msg> {
    let wanted = || !job.cancel.load(Ordering::Relaxed);
    if !wanted() {
        return None;
    }
    let started = Instant::now();
    let meta = std::fs::metadata(&job.path).ok();
    let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    // Instant low-res first paint for big files on screen, where that is much
    // cheaper than the full decode (otherwise it would only delay it).
    if let Some(side) = job.preview_side {
        if file_size > 6 << 20 {
            if let Some(prev) = decode::decode_preview(&job.path, side) {
                let _ = tx.send(Msg::Preview(job.path.clone(), job.generation, prev));
                ctx.request_repaint();
            }
            if !wanted() {
                return None;
            }
        }
    }
    let o = *opts.lock();
    // A multi-page TIFF: its first page now, the others below.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode::with_cancel(&job.cancel, || decode::decode_first_page(&job.path, o))));
    if !wanted() {
        return None;
    }
    let res = match res {
        Ok(Ok(decoded)) => {
            // A panic here must not end the worker (the image would stay on Loading).
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_pyramid(decoded.image.clone()))) {
                Ok(pyramid) if wanted() => Ok(LoadedImage {
                    decoded,
                    pyramid,
                    load_ms: started.elapsed().as_millis(),
                    file_size,
                    modified: meta.and_then(|m| m.modified().ok()),
                }),
                Ok(_) => return None,
                Err(_) => Err("Lumen could not prepare this image for display.".into()),
            }
        }
        Ok(Err(e)) => Err(decode::friendly_error(&e)),
        Err(_) => Err("The decoder crashed on this file (it may be corrupt).".into()),
    };
    let first_page = res.as_ref().ok().filter(|l| decode::has_more_pages(&l.decoded)).map(|l| l.decoded.clone());
    let Some(first_page) = first_page else {
        return Some(Msg::Done(job.path, job.generation, res, false));
    };
    // The first page goes on screen while the other pages are decoded (each
    // one only while the image is still wanted).
    let _ = tx.send(Msg::Done(job.path.clone(), job.generation, res, true));
    ctx.request_repaint();
    let pages = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode::with_cancel(&job.cancel, || decode::decode_more_pages(&job.path, &first_page, o.color_manage))));
    if !wanted() {
        return None;
    }
    Some(Msg::Pages(job.path, job.generation, pages.ok().flatten()))
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
    fn a_worker_stays_free_for_the_image_on_screen() {
        // Two workers, one busy with a preload: the next preload waits ...
        assert_eq!(pick_job([2, 1].into_iter(), 1, 2), None);
        // ... but the image on screen is taken at once, even behind preloads.
        assert_eq!(pick_job([2, 0, 1].into_iter(), 1, 2), Some(1));
        // Nothing running: the most urgent preload, the newest among equals.
        assert_eq!(pick_job([2, 1, 1].into_iter(), 0, 2), Some(2));
        // One worker only: preloads still run.
        assert_eq!(pick_job([1].into_iter(), 0, 1), Some(0));
        assert_eq!(pick_job([1].into_iter(), 1, 1), None);
        assert_eq!(pick_job(std::iter::empty(), 0, 4), None);
        // Four workers: up to three preloads at once.
        assert_eq!(pick_job([3].into_iter(), 2, 4), Some(0));
        assert_eq!(pick_job([3].into_iter(), 3, 4), None);
    }

    #[test]
    fn unwanted_loads_are_cancelled() {
        // The only worker sees a preload running, so it takes no preload and
        // the queue can be inspected.
        let mut l = Loader::new(egui::Context::default(), 1);
        l.queue.jobs.lock().running.push(Arc::new(AtomicU32::new(1)));
        let p = |n: &str| PathBuf::from(format!("Z:/lumen-test/{n}.png"));
        let (a, b, c) = (p("a"), p("b"), p("c"));
        l.request(&a, 1, None);
        l.request(&b, 2, None);
        let cancel_a = l.entries[&a].cancel.clone();
        // b is being decoded (taken off the queue by a worker).
        let running_b = {
            let mut q = l.queue.jobs.lock();
            let i = q.jobs.iter().position(|j| j.path == b).unwrap();
            q.jobs.remove(i)
        };
        // Moved on: only c is wanted now (c is not requested, so no worker runs).
        l.retain_queued(std::slice::from_ref(&c));
        assert!(cancel_a.load(Ordering::Relaxed), "queued job cancelled");
        assert!(running_b.cancel.load(Ordering::Relaxed), "running job told to stop");
        assert!(l.state(&a).is_none() && l.state(&b).is_none());
        assert!(l.queue.jobs.lock().jobs.is_empty());
        // Asking again starts a fresh load.
        l.request(&b, 1, None);
        assert!(!l.entries[&b].cancel.load(Ordering::Relaxed));
        assert_eq!(l.queue.jobs.lock().jobs.iter().filter(|j| j.path == b).count(), 1);
        // Kept loads are left alone.
        l.retain_queued(std::slice::from_ref(&b));
        assert!(matches!(l.state(&b), Some(LoadState::Loading)));
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
