//! Thumbnail service: prioritised multi-threaded generation, a persistent disk
//! cache (so re-opening a folder of 10,000 photos is instant) and an LRU of GPU
//! textures.

use crate::decode;
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::TextureHandle;
use lru::LruCache;
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Thumbnails are generated at this size and scaled down for display.
pub const THUMB_PX: u32 = 384;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

impl ThumbKey {
    pub fn new(path: &Path, size: u64, modified: Option<SystemTime>) -> Self {
        let mtime = modified
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ThumbKey { path: path.to_path_buf(), size, mtime }
    }
    fn hash_name(&self) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.path.to_string_lossy().to_lowercase().hash(&mut h);
        self.size.hash(&mut h);
        self.mtime.hash(&mut h);
        THUMB_PX.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

pub struct ThumbInfo {
    pub tex: TextureHandle,
    /// Original image dimensions when known.
    pub source_dims: Option<(u32, u32)>,
}

pub enum Thumb<'a> {
    Ready(&'a ThumbInfo),
    Pending,
    Failed,
}

struct Job {
    key: ThumbKey,
    /// Request sequence number (higher = asked for more recently). Shared
    /// with the service's `pending` entry so a re-request bumps it without
    /// searching the queue or taking its lock.
    seq: Arc<AtomicU64>,
    /// Request generation: a result whose generation is no longer the pending
    /// one (the path was forgotten meanwhile) is dropped.
    gen: u64,
}

struct Shared {
    jobs: Mutex<Vec<Job>>,
    cv: Condvar,
    disk_cache: AtomicBool,
    /// Requests older than this sequence number are stale (scrolled away).
    min_seq: AtomicU64,
}

/// Pixels ready for upload (converted on the worker) and the source size.
type ThumbResult = (ThumbKey, u64, Result<(egui::ColorImage, Option<(u32, u32)>), String>);

/// At most this many finished thumbnails become textures per frame: the GPU
/// upload happens after the frame is built, so a time budget cannot see it.
const UPLOADS_PER_FRAME: usize = 8;

/// GPU textures, least recently used out first, but never one drawn in this or
/// the previous frame; the cap grows to twice what the last frame drew, so a
/// screen with more cells than `base_cap` does not evict its own thumbnails
/// (and reload them, forever).
struct TexCache<K: Hash + Eq, V> {
    lru: LruCache<K, (V, u64)>,
    base_cap: usize,
    frame: u64,
    used_now: usize,
    used_last: usize,
}

impl<K: Hash + Eq, V> TexCache<K, V> {
    fn new(base_cap: usize) -> Self {
        TexCache { lru: LruCache::unbounded(), base_cap, frame: 1, used_now: 0, used_last: 0 }
    }

    fn begin_frame(&mut self) {
        self.used_last = self.used_now;
        self.used_now = 0;
        self.frame += 1;
    }

    fn cap(&self) -> usize {
        self.base_cap.max(2 * self.used_last).max(2 * self.used_now)
    }

    fn contains(&self, k: &K) -> bool {
        self.lru.contains(k)
    }

    /// The entry for `k`, marked as drawn this frame.
    fn get(&mut self, k: &K) -> Option<&V> {
        let frame = self.frame;
        let e = self.lru.get_mut(k)?;
        if e.1 != frame {
            e.1 = frame;
            self.used_now += 1;
        }
        Some(&e.0)
    }

    fn put(&mut self, k: K, v: V) {
        self.lru.put(k, (v, self.frame));
        let cap = self.cap();
        while self.lru.len() > cap {
            match self.lru.peek_lru() {
                // Everything left was drawn just now (or arrived this frame).
                Some((_, (_, f))) if *f + 1 >= self.frame => break,
                Some(_) => {
                    self.lru.pop_lru();
                }
                None => break,
            }
        }
    }

    fn pop(&mut self, k: &K) -> Option<V> {
        self.lru.pop(k).map(|e| e.0)
    }

    fn keys(&self) -> impl Iterator<Item = &K> {
        self.lru.iter().map(|(k, _)| k)
    }

    fn clear(&mut self) {
        self.lru.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lru.len()
    }
}

pub struct ThumbService {
    shared: Arc<Shared>,
    rx: Receiver<ThumbResult>,
    textures: TexCache<ThumbKey, ThumbInfo>,
    /// Requested and not finished: the generation of the request and its
    /// sequence number (shared with the queued job).
    pending: HashMap<ThumbKey, (u64, Arc<AtomicU64>)>,
    failed: HashSet<ThumbKey>,
    seq: u64,
    gen: u64,
    ctx: egui::Context,
    cache_dir: PathBuf,
}

impl ThumbService {
    pub fn new(ctx: egui::Context, threads: usize, cache_dir: PathBuf) -> Self {
        let shared = Arc::new(Shared {
            jobs: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            disk_cache: AtomicBool::new(true),
            min_seq: AtomicU64::new(0),
        });
        let _ = std::fs::create_dir_all(&cache_dir);
        {
            // Keep the persistent cache bounded: prune oldest files beyond 1.5 GB,
            // off-thread and after startup, so listing a big cache folder does not
            // compete with the first folder and its thumbnails for the disk.
            let dir = cache_dir.clone();
            let _ = std::thread::Builder::new().name("lumen-thumb-prune".into()).spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(15));
                prune_disk_cache(&dir, 1536 << 20);
            });
        }
        let (tx, rx) = unbounded();
        for i in 0..threads.max(1) {
            let s = shared.clone();
            let tx = tx.clone();
            let c = ctx.clone();
            let dir = cache_dir.clone();
            std::thread::Builder::new()
                .name(format!("lumen-thumb-{i}"))
                .spawn(move || worker(s, tx, c, dir))
                .expect("spawn thumb worker");
        }
        ThumbService {
            shared,
            rx,
            // ~400 x (384 px RGBA + mips) is about 300 MB of VRAM: plenty for most grids
            // (the cache grows when more are on screen at once).
            textures: TexCache::new(400),
            pending: HashMap::new(),
            failed: HashSet::new(),
            seq: 0,
            gen: 0,
            ctx,
            cache_dir,
        }
    }

    pub fn set_disk_cache(&self, on: bool) {
        self.shared.disk_cache.store(on, Ordering::Relaxed);
    }

    /// Call once per frame before requesting thumbnails.
    pub fn begin_frame(&mut self) {
        self.textures.begin_frame();
        // Upload finished thumbnails (bounded per frame to keep scrolling smooth).
        let started = std::time::Instant::now();
        let mut uploads = 0;
        while let Ok((key, gen, res)) = self.rx.try_recv() {
            // A result for a request that was forgotten while it ran (the file was
            // moved, renamed or rewritten meanwhile) says nothing about the file now.
            if self.pending.get(&key).map(|p| p.0) != Some(gen) {
                continue;
            }
            self.pending.remove(&key);
            match res {
                Ok((ci, dims)) => {
                    uploads += 1;
                    let tex = self.ctx.load_texture(
                        format!("thumb:{}", key.path.display()),
                        ci,
                        egui::TextureOptions {
                            magnification: egui::TextureFilter::Linear,
                            minification: egui::TextureFilter::Linear,
                            wrap_mode: egui::TextureWrapMode::ClampToEdge,
                            mipmap_mode: Some(egui::TextureFilter::Linear),
                        },
                    );
                    self.textures.put(key, ThumbInfo { tex, source_dims: dims });
                }
                Err(e) if e == "stale" => {}
                Err(_) => {
                    self.failed.insert(key);
                }
            }
            if uploads >= UPLOADS_PER_FRAME || started.elapsed().as_millis() > 6 {
                self.ctx.request_repaint();
                break;
            }
        }
        crate::devhook::prof("thumb_upload", started);
        // Mark everything requested before the previous frame as stale; workers
        // skip stale jobs, so fast scrolling never builds a backlog.
        self.shared.min_seq.store(self.seq.saturating_sub(4000), Ordering::Relaxed);
    }

    /// Get (or request) a thumbnail. Call for visible items, most important first.
    pub fn get(&mut self, key: &ThumbKey) -> Thumb<'_> {
        if self.textures.contains(key) {
            return Thumb::Ready(self.textures.get(key).unwrap());
        }
        if self.failed.contains(key) {
            return Thumb::Failed;
        }
        self.seq += 1;
        if let Some((_, seq)) = self.pending.get(key) {
            // Still queued (or running): make it the newest request again.
            seq.store(self.seq, Ordering::Relaxed);
            return Thumb::Pending;
        }
        self.gen += 1;
        let seq = Arc::new(AtomicU64::new(self.seq));
        self.pending.insert(key.clone(), (self.gen, seq.clone()));
        self.shared.jobs.lock().push(Job { key: key.clone(), seq, gen: self.gen });
        self.shared.cv.notify_one();
        Thumb::Pending
    }

    /// Mark currently queued jobs as low-priority leftovers (call when changing folder).
    pub fn cancel_all(&mut self) {
        let mut jobs = self.shared.jobs.lock();
        for j in jobs.drain(..) {
            self.pending.remove(&j.key);
        }
    }

    /// Drop all thumbnail textures so they are rebuilt (settings that change pixels).
    pub fn clear_textures(&mut self) {
        self.cancel_all();
        self.textures.clear();
        self.pending.clear();
        self.failed.clear();
    }

    /// The file at `path` changed, appeared or went away: drop its texture, cached
    /// file, failure mark and any request in flight, so the next request starts over.
    /// File operations call this for both the old and the new path.
    pub fn forget(&mut self, path: &Path) {
        self.forget_many(&HashSet::from([path]));
    }

    /// [`forget`](Self::forget) for many files at once (one pass over the cache).
    pub fn forget_many(&mut self, paths: &HashSet<&Path>) {
        if paths.is_empty() {
            return;
        }
        let keys: Vec<ThumbKey> = self.textures.keys().filter(|k| paths.contains(k.path.as_path())).cloned().collect();
        for k in keys {
            self.textures.pop(&k);
            let _ = std::fs::remove_file(self.cache_dir.join(k.hash_name()));
        }
        self.failed.retain(|k| !paths.contains(k.path.as_path()));
        self.pending.retain(|k, _| !paths.contains(k.path.as_path()));
        self.shared.jobs.lock().retain(|j| !paths.contains(j.key.path.as_path()));
    }

    /// Retry every thumbnail that failed in `dir` (Refresh): a file that could not be
    /// read a moment ago (still being written, moved away and back) may be fine now.
    pub fn retry_failed_in(&mut self, dir: &Path) {
        self.failed.retain(|k| k.path.parent() != Some(dir));
    }

    pub fn clear_disk_cache(&self) -> std::io::Result<u64> {
        let mut freed = 0;
        for e in std::fs::read_dir(&self.cache_dir)?.flatten() {
            if let Ok(m) = e.metadata() {
                freed += m.len();
            }
            let _ = std::fs::remove_file(e.path());
        }
        Ok(freed)
    }

    pub fn disk_cache_size(&self) -> u64 {
        std::fs::read_dir(&self.cache_dir)
            .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }
}

fn worker(s: Arc<Shared>, tx: Sender<ThumbResult>, ctx: egui::Context, dir: PathBuf) {
    crate::bg::background_priority();
    loop {
        let job = {
            let mut jobs = s.jobs.lock();
            loop {
                if let Some((i, _)) = jobs.iter().enumerate().max_by_key(|(_, j)| j.seq.load(Ordering::Relaxed)) {
                    break jobs.swap_remove(i);
                }
                s.cv.wait(&mut jobs);
            }
        };
        if job.seq.load(Ordering::Relaxed) < s.min_seq.load(Ordering::Relaxed) {
            let _ = tx.send((job.key, job.gen, Err("stale".into())));
            continue;
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            make_thumb(&job.key, &s, &dir).map(|(img, dims)| (crate::render::color_image_from_rgba(&img), dims))
        }))
        .unwrap_or_else(|_| Err("panic".into()));
        let _ = tx.send((job.key, job.gen, res));
        ctx.request_repaint();
    }
}

fn make_thumb(key: &ThumbKey, s: &Shared, dir: &Path) -> Result<(image::RgbaImage, Option<(u32, u32)>), String> {
    let use_disk = s.disk_cache.load(Ordering::Relaxed);
    let file = dir.join(key.hash_name());
    if use_disk {
        if let Ok(bytes) = std::fs::read(&file) {
            if let Some((img, dims)) = read_cached(&bytes) {
                return Ok((img, dims));
            }
        }
    }
    // The source size comes from the same decode: no second open just to probe it.
    let (img, dims) = decode::decode_thumbnail_sized(&key.path, THUMB_PX).map_err(|e| e.to_string())?;
    if use_disk {
        let _ = write_cached(&file, &img, dims);
    }
    Ok((img, dims))
}

// Cache file layout: b"LMT1" | src_w u32 | src_h u32 | encoded image (JPEG or PNG).
fn read_cached(bytes: &[u8]) -> Option<(image::RgbaImage, Option<(u32, u32)>)> {
    if bytes.len() < 12 || &bytes[0..4] != b"LMT1" {
        return None;
    }
    let w = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let img = image::load_from_memory(&bytes[12..]).ok()?.into_rgba8();
    Some((img, if w > 0 { Some((w, h)) } else { None }))
}

fn write_cached(file: &Path, img: &image::RgbaImage, dims: Option<(u32, u32)>) -> anyhow::Result<()> {
    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"LMT1");
    let (w, h) = dims.unwrap_or((0, 0));
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    let has_alpha = img.pixels().any(|p| p.0[3] != 255);
    let mut cur = std::io::Cursor::new(&mut out);
    cur.set_position(12);
    if has_alpha {
        image::DynamicImage::ImageRgba8(img.clone()).write_to(&mut cur, image::ImageFormat::Png)?;
    } else {
        let rgb = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 88);
        rgb.write_with_encoder(enc)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service with no worker threads, so requests stay queued.
    fn idle_service() -> ThumbService {
        let shared = Arc::new(Shared { jobs: Mutex::new(Vec::new()), cv: Condvar::new(), disk_cache: AtomicBool::new(false), min_seq: AtomicU64::new(0) });
        let (_tx, rx) = unbounded();
        ThumbService {
            shared,
            rx,
            textures: TexCache::new(400),
            pending: HashMap::new(),
            failed: HashSet::new(),
            seq: 0,
            gen: 0,
            ctx: egui::Context::default(),
            cache_dir: std::env::temp_dir(),
        }
    }

    fn job_seq(j: &Job) -> u64 {
        j.seq.load(Ordering::Relaxed)
    }

    fn key(i: usize) -> ThumbKey {
        ThumbKey::new(Path::new(&format!(r"C:\Users\someone\Pictures\Holiday 2024\IMG_{i:05}.jpg")), 3_000_000 + i as u64, None)
    }

    #[test]
    fn rerequest_bumps_priority_of_queued_job() {
        let mut s = idle_service();
        for i in 0..10 {
            assert!(matches!(s.get(&key(i)), Thumb::Pending));
        }
        // Asking again for an older one makes it the newest request.
        s.get(&key(2));
        let jobs = s.shared.jobs.lock();
        assert_eq!(jobs.len(), 10);
        let newest = jobs.iter().max_by_key(|j| job_seq(j)).unwrap();
        assert!(newest.key == key(2));
    }

    #[test]
    fn textures_on_screen_are_never_evicted() {
        let mut c: TexCache<usize, ()> = TexCache::new(4);
        // Ten cells on screen, more than the base cap: every frame draws all of them.
        for frame in 0..5 {
            c.begin_frame();
            for i in 0..10 {
                if c.get(&i).is_none() {
                    c.put(i, ());
                }
            }
            if frame > 0 {
                assert_eq!(c.len(), 10, "frame {frame}");
            }
        }
        for i in 0..10 {
            assert!(c.contains(&i));
        }
        assert!(c.cap() >= 20);
        // Scrolled on: ten new cells, the old ones go (oldest first) once unused.
        for _ in 0..3 {
            c.begin_frame();
            for i in 10..20 {
                if c.get(&i).is_none() {
                    c.put(i, ());
                }
            }
        }
        assert!((10..20).all(|i| c.contains(&i)));
        c.begin_frame();
        c.put(99, ());
        assert!(c.len() <= c.cap().max(11), "{} > {}", c.len(), c.cap());
        // With few on screen the cache shrinks back to the base cap.
        for _ in 0..3 {
            c.begin_frame();
            c.get(&99);
        }
        c.put(100, ());
        assert!(c.len() <= 4, "{}", c.len());
        assert!(c.contains(&99) && c.contains(&100));
    }

    #[test]
    fn forget_many_drops_queued_and_failed() {
        let mut s = idle_service();
        for i in 0..6 {
            s.get(&key(i));
        }
        s.failed.insert(key(7));
        let (a, b, c) = (key(1).path, key(4).path, key(7).path);
        s.forget_many(&HashSet::from([a.as_path(), b.as_path(), c.as_path()]));
        let jobs = s.shared.jobs.lock();
        assert_eq!(jobs.len(), 4);
        assert!(jobs.iter().all(|j| j.key != key(1) && j.key != key(4)));
        drop(jobs);
        assert!(s.failed.is_empty() && s.pending.len() == 4);
    }

    /// Frame cost of re-requesting visible thumbnails while a long queue is
    /// waiting (fast scrolling through a 10,000-file folder).
    /// `cargo test --release -p lumen --no-default-features -- --ignored --nocapture bench_perf_`
    #[test]
    #[ignore]
    fn bench_perf_thumb_requests() {
        let mut s = idle_service();
        let keys: Vec<ThumbKey> = (0..4000).map(key).collect();
        for k in &keys {
            s.get(k);
        }
        let frames = 200;
        let start = std::time::Instant::now();
        for f in 0..frames {
            s.begin_frame();
            // 60 visible cells, a window that moves through the queue.
            let base = (f * 17) % (keys.len() - 60);
            for k in &keys[base..base + 60] {
                std::hint::black_box(matches!(s.get(k), Thumb::Pending));
            }
        }
        eprintln!("60 pending thumbnail requests with 4000 queued: {:.3} ms per frame", start.elapsed().as_secs_f64() * 1000.0 / frames as f64);
    }
}

/// Delete the oldest cache files until the folder is under `limit` bytes.
pub fn prune_disk_cache(dir: &Path, limit: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().unwrap_or(UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= limit {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}
