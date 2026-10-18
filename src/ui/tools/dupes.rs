//! Duplicate detection: exact (size + content hash) and visually similar
//! (64-bit difference hash compared by Hamming distance).

use image::RgbaImage;
use std::collections::HashMap;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 64-bit difference hash: shrink to 9×8 grayscale and compare horizontal neighbours.
#[allow(dead_code)]
pub fn dhash(img: &RgbaImage) -> u64 {
    dhash_detail(img).0
}

/// Images whose hash grid has less detail than this (mean absolute neighbour
/// difference, 0..255) are too flat for the hash to mean anything: their bits
/// are dominated by noise, so they never take part in similarity matching.
pub const MIN_DETAIL: f32 = 2.0;

/// Difference hash plus the amount of detail it was computed from.
pub fn dhash_detail(img: &RgbaImage) -> (u64, f32) {
    if img.width() == 0 || img.height() == 0 {
        return (0, 0.0);
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
    let mut diff = 0u32;
    for y in 0..8 {
        for x in 0..8 {
            let l = small.get_pixel(x, y).0[0];
            let r = small.get_pixel(x + 1, y).0[0];
            h = (h << 1) | (l > r) as u64;
            diff += (l as i32 - r as i32).unsigned_abs();
        }
    }
    (h, diff as f32 / 64.0)
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
    /// Detail behind the dHash (see `MIN_DETAIL`).
    pub detail: f32,
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
    /// Lowest similarity between any two members (Similar only).
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
    use std::cmp::Ordering;
    let mut best = 0;
    for (k, &i) in members.iter().enumerate().skip(1) {
        let b = members[best];
        let primary = match rule {
            KeepRule::Resolution => (px(i), items[i].size).cmp(&(px(b), items[b].size)),
            KeepRule::Largest => (items[i].size, px(i)).cmp(&(items[b].size, px(b))),
            KeepRule::Newest => t(i).cmp(&t(b)),
            KeepRule::Oldest => t(b).cmp(&t(i)),
        };
        // On a tie prefer the "original" name over "x - Copy", "Copy of x" or "x (2)",
        // then the shorter name.
        let better = match primary {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => (copy_penalty(&items[i].path), name_len(&items[i].path)) < (copy_penalty(&items[b].path), name_len(&items[b].path)),
        };
        if better {
            best = k;
        }
    }
    best
}

fn name_len(p: &std::path::Path) -> usize {
    p.file_name().map(|n| n.len()).unwrap_or(usize::MAX)
}

/// 1 when a file name looks like a copy made by Explorer or a download manager.
pub fn copy_penalty(p: &std::path::Path) -> u8 {
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let copyish = stem.ends_with(" - copy")
        || stem.contains(" - copy (")
        || stem.starts_with("copy of ")
        || stem.ends_with(" copy")
        || (stem.ends_with(')') && stem.rfind(" (").map(|i| stem[i + 2..stem.len() - 1].chars().all(|c| c.is_ascii_digit())).unwrap_or(false));
    copyish as u8
}

/// BK-tree over 64-bit hashes with Hamming distance, for fast radius queries.
pub struct BkTree {
    /// (hash, payload, children as (distance, node index))
    nodes: Vec<(u64, usize, Vec<(u32, usize)>)>,
}

impl BkTree {
    pub fn new() -> Self {
        BkTree { nodes: Vec::new() }
    }

    pub fn insert(&mut self, hash: u64, payload: usize) {
        if self.nodes.is_empty() {
            self.nodes.push((hash, payload, Vec::new()));
            return;
        }
        let mut cur = 0;
        loop {
            let d = hamming(self.nodes[cur].0, hash);
            match self.nodes[cur].2.iter().find(|(cd, _)| *cd == d) {
                Some(&(_, next)) => cur = next,
                None => {
                    let idx = self.nodes.len();
                    self.nodes.push((hash, payload, Vec::new()));
                    self.nodes[cur].2.push((d, idx));
                    return;
                }
            }
        }
    }

    /// Payloads of every hash within `radius` of `hash`.
    pub fn query(&self, hash: u64, radius: u32, out: &mut Vec<usize>) {
        if self.nodes.is_empty() {
            return;
        }
        let mut stack = vec![0usize];
        while let Some(n) = stack.pop() {
            let (h, payload, children) = &self.nodes[n];
            let d = hamming(*h, hash);
            if d <= radius {
                out.push(*payload);
            }
            let (lo, hi) = (d.saturating_sub(radius), d + radius);
            for &(cd, c) in children {
                if cd >= lo && cd <= hi {
                    stack.push(c);
                }
            }
        }
    }
}

impl Default for BkTree {
    fn default() -> Self {
        Self::new()
    }
}

/// True when two images have a similar aspect ratio (within ~6%), or when
/// either size is unknown.
pub fn similar_aspect(a: Option<(u32, u32)>, b: Option<(u32, u32)>) -> bool {
    match (a, b) {
        (Some((aw, ah)), Some((bw, bh))) if aw > 0 && ah > 0 && bw > 0 && bh > 0 => {
            let ra = aw as f64 / ah as f64;
            let rb = bw as f64 / bh as f64;
            (ra / rb).ln().abs() < 0.06
        }
        _ => true,
    }
}

/// Group scanned items. Byte-identical files always form groups. With
/// `similar` on, visually similar images are clustered with complete linkage:
/// every member of a similar group is within `threshold` Hamming distance (and
/// a similar aspect ratio) of every other member, so near-duplicates never
/// chain into one big group. Flat, low-detail images never match by look.
pub fn group(items: &[ScanItem], similar: bool, threshold: u32) -> Vec<Group> {
    // 1. Identical content: units of one or more byte-identical files.
    let mut units: Vec<Vec<usize>> = Vec::new();
    let mut by_content: HashMap<(u64, u64), usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        match it.content {
            Some(c) => match by_content.get(&(it.size, c)) {
                Some(&u) => units[u].push(i),
                None => {
                    by_content.insert((it.size, c), units.len());
                    units.push(vec![i]);
                }
            },
            None => units.push(vec![i]),
        }
    }
    let rep = |u: &Vec<usize>| u[0];
    // 2. Similar: cluster units by their representative's hash.
    let mut clusters: Vec<Vec<usize>> = Vec::new(); // unit indices
    if similar {
        let usable: Vec<usize> = (0..units.len()).filter(|&u| items[rep(&units[u])].dhash.is_some() && items[rep(&units[u])].detail >= MIN_DETAIL).collect();
        let mut tree = BkTree::new();
        for &u in &usable {
            tree.insert(items[rep(&units[u])].dhash.unwrap_or(0), u);
        }
        // Deterministic order: by path.
        let mut order = usable.clone();
        order.sort_by(|&a, &b| crate::catalog::natural_cmp(&items[rep(&units[a])].path.to_string_lossy(), &items[rep(&units[b])].path.to_string_lossy()));
        let mut assigned = vec![false; units.len()];
        let mut cands = Vec::new();
        for &u in &order {
            if assigned[u] {
                continue;
            }
            assigned[u] = true;
            let hu = items[rep(&units[u])].dhash.unwrap_or(0);
            cands.clear();
            tree.query(hu, threshold, &mut cands);
            cands.retain(|&c| !assigned[c]);
            // Closest first, ties by path, for stable groups.
            cands.sort_by(|&a, &b| {
                let (ia, ib) = (&items[rep(&units[a])], &items[rep(&units[b])]);
                hamming(hu, ia.dhash.unwrap_or(0)).cmp(&hamming(hu, ib.dhash.unwrap_or(0))).then_with(|| crate::catalog::natural_cmp(&ia.path.to_string_lossy(), &ib.path.to_string_lossy()))
            });
            let mut members = vec![u];
            for &c in &cands {
                let ic = &items[rep(&units[c])];
                let fits = members.iter().all(|&m| {
                    let im = &items[rep(&units[m])];
                    hamming(im.dhash.unwrap_or(0), ic.dhash.unwrap_or(0)) <= threshold && similar_aspect(im.dims, ic.dims)
                });
                if fits {
                    members.push(c);
                    assigned[c] = true;
                }
            }
            if members.len() >= 2 {
                clusters.push(members);
            }
        }
    }
    let mut in_cluster = vec![false; units.len()];
    for c in &clusters {
        for &u in c {
            in_cluster[u] = true;
        }
    }
    let sort_members = |members: &mut Vec<usize>| {
        members.sort_by(|&a, &b| crate::catalog::natural_cmp(&items[a].path.to_string_lossy(), &items[b].path.to_string_lossy()));
    };
    let mut groups: Vec<Group> = Vec::new();
    for (u, unit) in units.iter().enumerate() {
        if !in_cluster[u] && unit.len() >= 2 {
            let mut members = unit.clone();
            sort_members(&mut members);
            groups.push(Group { kind: GroupKind::Identical, members, min_similarity: 100 });
        }
    }
    for c in clusters {
        let mut members: Vec<usize> = c.iter().flat_map(|&u| units[u].iter().copied()).collect();
        sort_members(&mut members);
        let hashes: Vec<u64> = members.iter().filter_map(|&i| items[i].dhash).collect();
        let mut min_similarity = 100;
        for a in 0..hashes.len() {
            for b in a + 1..hashes.len() {
                min_similarity = min_similarity.min(similarity_pct(hamming(hashes[a], hashes[b])));
            }
        }
        groups.push(Group { kind: GroupKind::Similar, members, min_similarity });
    }
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
        assert!(dhash_detail(&a).1 >= MIN_DETAIL);
        let flat = RgbaImage::from_pixel(64, 64, image::Rgba([90, 90, 90, 255]));
        assert!(dhash_detail(&flat).1 < MIN_DETAIL);
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
        ScanItem { path: PathBuf::from(name), size, modified: None, dims: Some(dims), content, dhash, detail: 20.0 }
    }

    #[test]
    fn similar_groups_use_complete_linkage() {
        // A burst: each frame differs from the next by 4 bits, so single
        // linkage would chain all of them; complete linkage must not.
        let mut items = Vec::new();
        let mut h = 0u64;
        for i in 0..6 {
            items.push(item(&format!("burst{i}.jpg"), 100 + i, Some(i), Some(h), (40, 30)));
            h ^= 0xF << (i * 4);
        }
        let groups = group(&items, true, 6);
        for g in &groups {
            for &a in &g.members {
                for &b in &g.members {
                    assert!(hamming(items[a].dhash.unwrap(), items[b].dhash.unwrap()) <= 6, "{:?}", g.members);
                }
            }
            assert!(g.min_similarity >= similarity_pct(6));
        }
        assert!(groups.iter().all(|g| g.members.len() <= 2), "{groups:?}");
        // Different aspect ratios never match.
        let items = vec![item("a.jpg", 1, Some(1), Some(7), (40, 30)), item("b.jpg", 2, Some(2), Some(7), (30, 40))];
        assert!(group(&items, true, 6).is_empty());
        // Flat images never match by look, but identical copies still do.
        let mut flat = vec![item("f1.png", 5, Some(9), Some(0), (10, 10)), item("f2.png", 6, Some(8), Some(0), (10, 10)), item("f3.png", 6, Some(8), Some(0), (10, 10))];
        for f in &mut flat {
            f.detail = 0.3;
        }
        let g = group(&flat, true, 6);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GroupKind::Identical);
        assert_eq!(g[0].members, vec![1, 2]);
    }

    #[test]
    fn bk_tree_matches_brute_force() {
        let mut v = 0x9E37_79B9_7F4A_7C15u64;
        let hashes: Vec<u64> = (0..300)
            .map(|_| {
                v ^= v << 13;
                v ^= v >> 7;
                v ^= v << 17;
                if v % 3 == 0 {
                    v & 0xFFFF
                } else {
                    v
                }
            })
            .collect();
        let mut t = BkTree::new();
        for (i, &h) in hashes.iter().enumerate() {
            t.insert(h, i);
        }
        for &q in hashes.iter().take(40) {
            let mut got = Vec::new();
            t.query(q, 10, &mut got);
            got.sort();
            let want: Vec<usize> = (0..hashes.len()).filter(|&i| hamming(hashes[i], q) <= 10).collect();
            assert_eq!(got, want);
        }
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

#[cfg(test)]
mod copy_name_tests {
    use super::copy_penalty;
    use std::path::Path;
    #[test]
    fn detects_copy_names() {
        assert_eq!(copy_penalty(Path::new("photo_01 - Copy.jpg")), 1);
        assert_eq!(copy_penalty(Path::new("Copy of photo.jpg")), 1);
        assert_eq!(copy_penalty(Path::new("photo (2).jpg")), 1);
        assert_eq!(copy_penalty(Path::new("photo_01.jpg")), 0);
        assert_eq!(copy_penalty(Path::new("party (birthday).jpg")), 0);
    }
}
