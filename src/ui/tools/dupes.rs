//! Duplicate detection: exact (size + content hash) and visually similar
//! (64-bit difference hash compared by Hamming distance).

use image::RgbaImage;
use std::collections::HashMap;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 64-bit difference hash: shrink to 9×8 grayscale and compare horizontal neighbours.
pub fn dhash(img: &RgbaImage) -> u64 {
    if img.width() == 0 || img.height() == 0 {
        return 0;
    }
    // Flatten alpha onto mid-gray so transparent regions hash consistently.
    let gray = image::GrayImage::from_fn(img.width(), img.height(), |x, y| {
        let p = img.get_pixel(x, y).0;
        let a = p[3] as f32 / 255.0;
        let l = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        image::Luma([(l * a + 128.0 * (1.0 - a)).round() as u8])
    });
    let small = image::imageops::resize(&gray, 9, 8, image::imageops::FilterType::Triangle);
    let mut h = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let l = small.get_pixel(x, y).0[0];
            let r = small.get_pixel(x + 1, y).0[0];
            h = (h << 1) | (l > r) as u64;
        }
    }
    h
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Similarity percentage for a Hamming distance on a 64-bit hash.
pub fn similarity_pct(distance: u32) -> u32 {
    100 - (distance.min(64) * 100 + 32) / 64
}

/// Hamming threshold for a similarity percentage.
pub fn threshold_for_pct(pct: u32) -> u32 {
    (100 - pct.min(100)) * 64 / 100
}

/// Fast 64-bit content hash (SipHash over the whole file plus its length).
pub fn file_hash(path: &Path) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
        total += n as u64;
    }
    h.write_u64(total);
    Ok(h.finish())
}

#[derive(Clone, Debug)]
pub struct ScanItem {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub dims: Option<(u32, u32)>,
    pub content: Option<u64>,
    pub dhash: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Identical,
    Similar,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub kind: GroupKind,
    /// Indices into the scanned items.
    pub members: Vec<usize>,
    /// Lowest similarity between the first member and the others (Similar only).
    pub min_similarity: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepRule {
    Resolution,
    Largest,
    Newest,
    Oldest,
}

impl KeepRule {
    pub const ALL: [KeepRule; 4] = [KeepRule::Resolution, KeepRule::Largest, KeepRule::Newest, KeepRule::Oldest];
    pub fn label(self) -> &'static str {
        match self {
            KeepRule::Resolution => "Highest resolution",
            KeepRule::Largest => "Largest file",
            KeepRule::Newest => "Newest",
            KeepRule::Oldest => "Oldest",
        }
    }
}

/// Index (into `members`) of the file to keep in a group.
pub fn best_in_group(items: &[ScanItem], members: &[usize], rule: KeepRule) -> usize {
    let px = |i: usize| items[i].dims.map(|(w, h)| w as u64 * h as u64).unwrap_or(0);
    let t = |i: usize| items[i].modified.unwrap_or(SystemTime::UNIX_EPOCH);
    let mut best = 0;
    for (k, &i) in members.iter().enumerate().skip(1) {
        let b = members[best];
        let better = match rule {
            KeepRule::Resolution => (px(i), items[i].size) > (px(b), items[b].size),
            KeepRule::Largest => (items[i].size, px(i)) > (items[b].size, px(b)),
            KeepRule::Newest => t(i) > t(b),
            KeepRule::Oldest => t(i) < t(b),
        };
        if better {
            best = k;
        }
    }
    best
}

struct Dsu(Vec<usize>);

impl Dsu {
    fn new(n: usize) -> Self {
        Dsu((0..n).collect())
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.0[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// Group scanned items. With `similar` off, only byte-identical files are grouped;
/// otherwise images within `threshold` Hamming distance are clustered too.
pub fn group(items: &[ScanItem], similar: bool, threshold: u32) -> Vec<Group> {
    let n = items.len();
    let mut dsu = Dsu::new(n);
    // Identical content.
    let mut by_content: HashMap<(u64, u64), usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if let Some(c) = it.content {
            match by_content.get(&(it.size, c)) {
                Some(&j) => dsu.union(i, j),
                None => {
                    by_content.insert((it.size, c), i);
                }
            }
        }
    }
    if similar {
        let hashed: Vec<(usize, u64)> = items.iter().enumerate().filter_map(|(i, it)| it.dhash.map(|h| (i, h))).collect();
        for a in 0..hashed.len() {
            let (ia, ha) = hashed[a];
            for &(ib, hb) in &hashed[a + 1..] {
                if hamming(ha, hb) <= threshold {
                    dsu.union(ia, ib);
                }
            }
        }
    }
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = dsu.find(i);
        clusters.entry(r).or_default().push(i);
    }
    let mut groups: Vec<Group> = clusters
        .into_values()
        .filter(|m| m.len() >= 2)
        .map(|mut members| {
            members.sort_by(|&a, &b| crate::catalog::natural_cmp(&items[a].path.to_string_lossy(), &items[b].path.to_string_lossy()));
            let first = &items[members[0]];
            let identical = members.iter().all(|&i| items[i].content.is_some() && items[i].content == first.content && items[i].size == first.size);
            let min_similarity = members
                .iter()
                .skip(1)
                .filter_map(|&i| Some(similarity_pct(hamming(first.dhash?, items[i].dhash?))))
                .min()
                .unwrap_or(100);
            Group { kind: if identical { GroupKind::Identical } else { GroupKind::Similar }, members, min_similarity }
        })
        .collect();
    let wasted = |g: &Group| -> u64 {
        let total: u64 = g.members.iter().map(|&i| items[i].size).sum();
        total - g.members.iter().map(|&i| items[i].size).max().unwrap_or(0)
    };
    groups.sort_by(|a, b| (a.kind != GroupKind::Identical).cmp(&(b.kind != GroupKind::Identical)).then(wasted(b).cmp(&wasted(a))));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32, invert: bool) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let v = ((x * 255 / (w - 1)) as u8).wrapping_add((y % 7) as u8);
            let v = if invert { 255 - v } else { v };
            image::Rgba([v, v / 2, 255 - v, 255])
        })
    }

    /// Deterministic pseudo-random 32 px blocks.
    fn blocks(w: u32, h: u32, seed: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let mut v = (x / 32) * 7919 + (y / 32) * 104729 + seed * 31337;
            v ^= v << 13;
            v ^= v >> 17;
            v ^= v << 5;
            let g = (v % 256) as u8;
            image::Rgba([g, g, g, 255])
        })
    }

    #[test]
    fn dhash_identical_and_different() {
        let a = gradient(256, 192, false);
        assert_eq!(hamming(dhash(&a), dhash(&a.clone())), 0);
        // A rescaled copy is near-identical.
        let small = image::imageops::resize(&a, 128, 96, image::imageops::FilterType::Triangle);
        assert!(hamming(dhash(&a), dhash(&small)) <= 4);
        // Mirrored gradient flips nearly every bit.
        let b = gradient(256, 192, true);
        assert!(hamming(dhash(&a), dhash(&b)) >= 40, "{}", hamming(dhash(&a), dhash(&b)));
        let c = blocks(256, 192, 1);
        let d = blocks(256, 192, 2);
        assert!(hamming(dhash(&a), dhash(&c)) >= 12, "{}", hamming(dhash(&a), dhash(&c)));
        assert!(hamming(dhash(&c), dhash(&d)) >= 12, "{}", hamming(dhash(&c), dhash(&d)));
        assert_eq!(hamming(dhash(&c), dhash(&blocks(256, 192, 1))), 0);
    }

    #[test]
    fn similarity_conversion() {
        assert_eq!(similarity_pct(0), 100);
        assert_eq!(similarity_pct(64), 0);
        assert_eq!(threshold_for_pct(100), 0);
        assert_eq!(threshold_for_pct(90), 6);
        assert!(similarity_pct(threshold_for_pct(85)) >= 85);
    }

    fn item(name: &str, size: u64, content: Option<u64>, dhash: Option<u64>, dims: (u32, u32)) -> ScanItem {
        ScanItem { path: PathBuf::from(name), size, modified: None, dims: Some(dims), content, dhash }
    }

    #[test]
    fn grouping() {
        let items = vec![
            item("a.jpg", 100, Some(1), Some(0xFF00), (10, 10)),
            item("b.jpg", 100, Some(1), Some(0xFF00), (10, 10)),
            item("c.jpg", 300, Some(2), Some(0xFF01), (40, 40)),
            item("d.jpg", 100, Some(3), Some(!0xFF00u64), (10, 10)),
        ];
        let exact = group(&items, false, 6);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].kind, GroupKind::Identical);
        assert_eq!(exact[0].members, vec![0, 1]);
        let sim = group(&items, true, 6);
        assert_eq!(sim.len(), 1);
        assert_eq!(sim[0].kind, GroupKind::Similar);
        assert_eq!(sim[0].members, vec![0, 1, 2]);
        assert_eq!(best_in_group(&items, &sim[0].members, KeepRule::Resolution), 2);
        assert_eq!(best_in_group(&items, &sim[0].members, KeepRule::Largest), 2);
    }

    #[test]
    fn content_hash() {
        let dir = std::env::temp_dir().join(format!("lumen-dup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        std::fs::write(&a, b"hello world").unwrap();
        std::fs::write(&b, b"hello world").unwrap();
        std::fs::write(&c, b"hello worle").unwrap();
        assert_eq!(file_hash(&a).unwrap(), file_hash(&b).unwrap());
        assert_ne!(file_hash(&a).unwrap(), file_hash(&c).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
