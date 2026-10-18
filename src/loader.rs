//! Background loading of full-resolution images with a memory-bounded cache and
//! neighbour preloading so stepping through a folder is instant.

use crate::decode::{self, DecodeOptions, Decoded};
use crate::render::build_pyramid;
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

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
        Loader { queue, rx, entries: HashMap::new(), next_generation: 1, budget_bytes: 1536 << 20, opts }
    }

    /// Request `path`. `priority` 0 = the image on screen.
    pub fn request(&mut self, path: &Path, priority: u32, preview_side: Option<u32>) {
        if let Some(e) = self.entries.get_mut(path) {
            // Already loading/loaded/failed (use `invalidate` to retry); bump priority if queued.
            // Called every frame for the image on screen, so only touch the queue lock
            // while the job can still be waiting in it.
            e.touched = Instant::now();
            if matches!(e.state, LoadState::Loading) {
                let mut jobs = self.queue.jobs.lock();
                if let Some(j) = jobs.iter_mut().find(|j| j.path == path) {
                    j.priority = j.priority.min(priority);
                }
            }
            return;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Loading, touched: Instant::now(), generation });
        let mut jobs = self.queue.jobs.lock();
        jobs.push(Job { path: path.to_path_buf(), priority, preview_side, generation });
        self.queue.cv.notify_one();
    }

    /// Drop queued (not yet started) jobs except those listed.
    pub fn retain_queued(&mut self, keep: &[PathBuf]) {
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

    pub fn clear(&mut self) {
        self.entries.clear();
        self.queue.jobs.lock().clear();
    }

    /// Insert an already-decoded image (e.g. pasted from clipboard / edited result).
    pub fn insert(&mut self, path: &Path, img: LoadedImage) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.entries.insert(path.to_path_buf(), Entry { state: LoadState::Ready(Arc::new(img)), touched: Instant::now(), generation });
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

    fn enforce_budget(&mut self, protect: &[PathBuf]) {
        let mut total: u64 = self
            .entries
            .values()
            .map(|e| if let LoadState::Ready(i) = &e.state { i.bytes() } else { 0 })
            .sum();
        if total <= self.budget_bytes {
            return;
        }
        let mut ready: Vec<(PathBuf, Instant, u64)> = self
            .entries
            .iter()
            .filter_map(|(p, e)| match &e.state {
                LoadState::Ready(i) if !protect.contains(p) => Some((p.clone(), e.touched, i.bytes())),
                _ => None,
            })
            .collect();
        ready.sort_by_key(|(_, t, _)| *t);
        for (p, _, b) in ready {
            if total <= self.budget_bytes {
                break;
            }
            self.entries.remove(&p);
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
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("The decoder crashed on this file (it may be corrupt).".into()),
        };
        let _ = tx.send(Msg::Done(job.path, job.generation, res));
        ctx.request_repaint();
    }
}
