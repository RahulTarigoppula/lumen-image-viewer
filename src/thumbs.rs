//! Thumbnail service: prioritised multi-threaded generation, a persistent disk
//! cache (so re-opening a folder of 10,000 photos is instant) and an LRU of GPU
//! textures.

use crate::decode;
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::TextureHandle;
use lru::LruCache;
use parking_lot::{Condvar, Mutex};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
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
    seq: u64,
}

struct Shared {
    jobs: Mutex<Vec<Job>>,
    cv: Condvar,
    disk_cache: AtomicBool,
    /// Requests older than this sequence number are stale (scrolled away).
    min_seq: AtomicU64,
}

type ThumbResult = (ThumbKey, Result<(image::RgbaImage, Option<(u32, u32)>), String>);

pub struct ThumbService {
    shared: Arc<Shared>,
    rx: Receiver<ThumbResult>,
    textures: LruCache<ThumbKey, ThumbInfo>,
    pending: HashSet<ThumbKey>,
    failed: HashSet<ThumbKey>,
    seq: u64,
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
            // Keep the persistent cache bounded: prune oldest files beyond 1.5 GB, off-thread.
            let dir = cache_dir.clone();
            std::thread::spawn(move || prune_disk_cache(&dir, 1536 << 20));
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
            // ~400 x (384 px RGBA + mips) is about 300 MB of VRAM: plenty for any grid.
            textures: LruCache::new(NonZeroUsize::new(400).unwrap()),
            pending: HashSet::new(),
            failed: HashSet::new(),
            seq: 0,
            ctx,
            cache_dir,
        }
    }

    pub fn set_disk_cache(&self, on: bool) {
        self.shared.disk_cache.store(on, Ordering::Relaxed);
    }

    /// Call once per frame before requesting thumbnails.
    pub fn begin_frame(&mut self) {
        // Upload finished thumbnails (bounded per frame to keep scrolling smooth).
        let started = std::time::Instant::now();
        while let Ok((key, res)) = self.rx.try_recv() {
            self.pending.remove(&key);
            match res {
                Ok((img, dims)) => {
                    let ci = crate::render::color_image_from_rgba(&img);
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
            if started.elapsed().as_millis() > 6 {
                self.ctx.request_repaint();
                break;
            }
        }
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
        let mut jobs = self.shared.jobs.lock();
        if self.pending.contains(key) {
            if let Some(j) = jobs.iter_mut().find(|j| &j.key == key) {
                j.seq = self.seq;
            }
            return Thumb::Pending;
        }
        self.pending.insert(key.clone());
        jobs.push(Job { key: key.clone(), seq: self.seq });
        self.shared.cv.notify_one();
        Thumb::Pending
    }

    /// Peek without requesting.
    pub fn peek(&mut self, key: &ThumbKey) -> Option<&ThumbInfo> {
        self.textures.get(key)
    }

    /// Mark currently queued jobs as low-priority leftovers (call when changing folder).
    pub fn cancel_all(&mut self) {
        let mut jobs = self.shared.jobs.lock();
        for j in jobs.drain(..) {
            self.pending.remove(&j.key);
        }
    }

    pub fn forget(&mut self, path: &Path) {
        let keys: Vec<ThumbKey> = self.textures.iter().filter(|(k, _)| k.path == path).map(|(k, _)| k.clone()).collect();
        for k in keys {
            self.textures.pop(&k);
            let _ = std::fs::remove_file(self.cache_dir.join(k.hash_name()));
        }
        self.failed.retain(|k| k.path != path);
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
    loop {
        let job = {
            let mut jobs = s.jobs.lock();
            loop {
                if let Some((i, _)) = jobs.iter().enumerate().max_by_key(|(_, j)| j.seq) {
                    break jobs.swap_remove(i);
                }
                s.cv.wait(&mut jobs);
            }
        };
        if job.seq < s.min_seq.load(Ordering::Relaxed) {
            let _ = tx.send((job.key, Err("stale".into())));
            continue;
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| make_thumb(&job.key, &s, &dir)))
            .unwrap_or_else(|_| Err("panic".into()));
        let _ = tx.send((job.key, res));
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
    let img = decode::decode_thumbnail(&key.path, THUMB_PX).map_err(|e| e.to_string())?;
    let dims = decode::probe_dimensions(&key.path);
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
