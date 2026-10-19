//! Duplicate detection: exact (size + content hash) and visually similar
//! (128-bit difference hash compared by Hamming distance).

use image::RgbaImage;
use std::collections::HashMap;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 128-bit difference hash: the high 64 bits compare horizontal neighbours
/// (a 9x8 grayscale grid), the low 64 bits vertical ones (8x9). Both
/// directions are needed: a picture whose detail runs top to bottom (a
/// vertical gradient, a horizon) has almost nothing to compare across.
#[allow(dead_code)]
pub fn dhash(img: &RgbaImage) -> u128 {
    dhash_detail(img).0
}

/// Images whose hash grids have less detail than this (mean absolute
/// neighbour difference, 0..255, in the more detailed direction) are too flat
/// for the hash to mean anything: their bits are dominated by noise, so they
/// never take part in similarity matching.
pub const MIN_DETAIL: f32 = 2.0;

/// Number of bits in the hash.
pub const HASH_BITS: u32 = 128;

/// Difference hash plus the amount of detail it was computed from.
pub fn dhash_detail(img: &RgbaImage) -> (u128, f32) {
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
    // One direction: a (w x h) grid compared along (dx, dy).
    let half = |w: u32, h: u32, dx: u32, dy: u32| -> (u64, u32) {
        let small = image::imageops::resize(&gray, w, h, image::imageops::FilterType::Triangle);
        let (mut bits, mut diff) = (0u64, 0u32);
        for y in 0..h - dy {
            for x in 0..w - dx {
                let a = small.get_pixel(x, y).0[0];
                let b = small.get_pixel(x + dx, y + dy).0[0];
                bits = (bits << 1) | (a > b) as u64;
                diff += (a as i32 - b as i32).unsigned_abs();
            }
        }
        (bits, diff)
    };
    let (row, row_diff) = half(9, 8, 1, 0);
    let (col, col_diff) = half(8, 9, 0, 1);
    (((row as u128) << 64) | col as u128, row_diff.max(col_diff) as f32 / 64.0)
}

pub fn hamming(a: u128, b: u128) -> u32 {
    (a ^ b).count_ones()
}

/// Similarity percentage for a Hamming distance between two hashes.
pub fn similarity_pct(distance: u32) -> u32 {
    100 - (distance.min(HASH_BITS) * 100 + HASH_BITS / 2) / HASH_BITS
}

/// Hamming threshold for a similarity percentage.
pub fn threshold_for_pct(pct: u32) -> u32 {
    (100 - pct.min(100)) * HASH_BITS / 100
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

/// 64-bit hash of decoded pixels plus dimensions, for finding files that
/// differ in bytes (metadata, compression) but not in a single pixel.
pub fn pixel_hash(img: &RgbaImage) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u32(img.width());
    h.write_u32(img.height());
    h.write(img.as_raw());
    h.finish()
}

#[derive(Clone, Debug)]
pub struct ScanItem {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// EXIF capture date (Unix seconds).
    pub taken: Option<i64>,
    pub dims: Option<(u32, u32)>,
    pub content: Option<u64>,
    /// Hash of the decoded pixels, computed only for likely matches (see `pixel_hash`).
    pub pixels: Option<u64>,
    pub dhash: Option<u128>,
    /// Detail behind the dHash (see `MIN_DETAIL`).
    pub detail: f32,
}

impl ScanItem {
    pub fn new(path: PathBuf, size: u64, modified: Option<SystemTime>) -> Self {
        ScanItem { path, size, modified, taken: None, dims: None, content: None, pixels: None, dhash: None, detail: 0.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// Exact copies: byte-identical files, or files with identical pixels.
    Identical,
    /// Pictures that look alike but are not exact copies.
    Similar,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub kind: GroupKind,
    /// Indices into the scanned items. In a similar group each set of exact
    /// copies is represented by one file (see `linked`).
    pub members: Vec<usize>,
    /// Similar only, parallel to `members`: the identical group a member
    /// stands for. Its copies are listed (and marked) in that group.
    pub linked: Vec<Option<usize>>,
    /// Lowest similarity between any two members (Similar only).
    pub min_similarity: u32,
    /// Identical only: every member has the same bytes, not just the same pixels.
    pub same_bytes: bool,
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

    /// Criteria in the order they decide, the rule's own first. After it come
    /// resolution, a name that does not look like a copy, the date (the oldest
    /// file is the likely original) and file size last: an edited or annotated
    /// copy is often bigger, so size must never outrank the others.
    pub fn chain(self) -> &'static [Criterion] {
        use Criterion as C;
        match self {
            KeepRule::Resolution => &[C::Resolution, C::NotACopy, C::Older, C::Larger, C::ShorterName, C::NameOrder],
            KeepRule::Largest => &[C::Larger, C::Resolution, C::NotACopy, C::Older, C::ShorterName, C::NameOrder],
            KeepRule::Newest => &[C::Newer, C::Resolution, C::NotACopy, C::Larger, C::ShorterName, C::NameOrder],
            KeepRule::Oldest => &[C::Older, C::Resolution, C::NotACopy, C::Larger, C::ShorterName, C::NameOrder],
        }
    }
}

/// One step of the keep tie-break chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Criterion {
    Resolution,
    Larger,
    Newer,
    Older,
    NotACopy,
    ShorterName,
    NameOrder,
}

impl Criterion {
    /// Why the kept file won, for the group header.
    pub fn reason(self) -> &'static str {
        match self {
            Criterion::Resolution => "Kept: highest resolution",
            Criterion::Larger => "Kept: largest file",
            Criterion::Newer => "Kept: newest",
            Criterion::Older => "Kept: oldest original",
            Criterion::NotACopy => "Kept: not named as a copy",
            Criterion::ShorterName => "Kept: shortest name",
            Criterion::NameOrder => "Kept: first by name",
        }
    }

    /// Greater when `a` is the better file to keep.
    fn compare(self, items: &[ScanItem], a: usize, b: usize) -> std::cmp::Ordering {
        let (ia, ib) = (&items[a], &items[b]);
        let px = |it: &ScanItem| it.dims.map(|(w, h)| w as u64 * h as u64).unwrap_or(0);
        match self {
            Criterion::Resolution => px(ia).cmp(&px(ib)),
            Criterion::Larger => ia.size.cmp(&ib.size),
            Criterion::Newer => date_cmp(ia, ib),
            Criterion::Older => date_cmp(ib, ia),
            Criterion::NotACopy => copy_penalty(&ib.path).cmp(&copy_penalty(&ia.path)),
            Criterion::ShorterName => name_len(&ib.path).cmp(&name_len(&ia.path)),
            Criterion::NameOrder => crate::catalog::natural_cmp(&ib.path.to_string_lossy(), &ia.path.to_string_lossy()),
        }
    }
}

/// Compare dates: the capture date when the two files have different ones,
/// otherwise the modified time (an unknown time compares equal).
fn date_cmp(a: &ScanItem, b: &ScanItem) -> std::cmp::Ordering {
    match (a.taken, b.taken) {
        (Some(x), Some(y)) if x != y => x.cmp(&y),
        _ => match (a.modified, b.modified) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => std::cmp::Ordering::Equal,
        },
    }
}

/// The file to keep in a group and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    /// Index into the group's `members`.
    pub index: usize,
    /// The criterion that settled the closest contest.
    pub reason: Criterion,
    /// The keep rule alone beat every other file and the winner has more
    /// pixels than each of them (pictures of the same size that only look
    /// alike are edits, crops or re-encodes: a person has to choose). Only a
    /// decisive pick may be applied to a similar group in bulk.
    pub decisive: bool,
}

/// Choose the file to keep among `members` with `rule` and its tie-break chain.
pub fn pick(items: &[ScanItem], members: &[usize], rule: KeepRule) -> Pick {
    use std::cmp::Ordering;
    let chain = rule.chain();
    // The first step of the chain that separates two files.
    let decide = |a: usize, b: usize| -> (Ordering, usize) {
        for (step, c) in chain.iter().enumerate() {
            let o = c.compare(items, a, b);
            if o != Ordering::Equal {
                return (o, step);
            }
        }
        (Ordering::Equal, chain.len() - 1)
    };
    let mut best = 0;
    for k in 1..members.len() {
        if decide(members[k], members[best]).0 == Ordering::Greater {
            best = k;
        }
    }
    let Some(&b) = members.get(best) else {
        return Pick { index: 0, reason: chain[0], decisive: true };
    };
    // The deepest step needed to beat any other file is the reason.
    let deepest = members.iter().enumerate().filter(|&(k, _)| k != best).map(|(_, &m)| decide(b, m).1).max().unwrap_or(0);
    let sharpest = members.iter().enumerate().all(|(k, &m)| k == best || Criterion::Resolution.compare(items, b, m) == Ordering::Greater);
    Pick { index: best, reason: chain[deepest], decisive: deepest == 0 && sharpest }
}

/// Index (into `members`) of the file to keep in a group.
pub fn best_in_group(items: &[ScanItem], members: &[usize], rule: KeepRule) -> usize {
    pick(items, members, rule).index
}

/// In similar groups, show for each linked set of exact copies the file that
/// its identical group keeps under `rule`, so both groups agree on the survivor.
pub fn refresh_representatives(groups: &mut [Group], items: &[ScanItem], rule: KeepRule) {
    let keepers: Vec<Option<usize>> = groups.iter().map(|g| (g.kind == GroupKind::Identical && !g.members.is_empty()).then(|| g.members[best_in_group(items, &g.members, rule)])).collect();
    for g in groups.iter_mut() {
        for (k, l) in g.linked.iter().enumerate() {
            if let (Some(Some(keep)), Some(m)) = (l.map(|gi| keepers.get(gi).copied().flatten()), g.members.get_mut(k)) {
                *m = keep;
            }
        }
    }
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

/// BK-tree over hashes with Hamming distance, for fast radius queries.
pub struct BkTree {
    /// (hash, payload, children as (distance, node index))
    nodes: Vec<(u128, usize, Vec<(u32, usize)>)>,
}

impl BkTree {
    pub fn new() -> Self {
        BkTree { nodes: Vec::new() }
    }

    pub fn insert(&mut self, hash: u128, payload: usize) {
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
    pub fn query(&self, hash: u128, radius: u32, out: &mut Vec<usize>) {
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

/// Group scanned items. Exact copies (byte-identical files, or files whose
/// decoded pixels are identical) always form their own Identical groups. With
/// `similar` on, visually similar images are clustered with complete linkage:
/// every member of a similar group is within `threshold` Hamming distance (and
/// a similar aspect ratio) of every other member, so near-duplicates never
/// chain into one big group. A set of exact copies takes part in a similar
/// group through one representative linked to its Identical group, so an
/// edited relative never decides which of the exact copies survives. Flat,
/// low-detail images never match by look.
pub fn group(items: &[ScanItem], similar: bool, threshold: u32) -> Vec<Group> {
    // 1. Exact copies: units of files that share bytes or decoded pixels.
    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut by_content: HashMap<(u64, u64), usize> = HashMap::new();
    let mut by_pixels: HashMap<u64, usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        let firsts = [it.content.map(|c| *by_content.entry((it.size, c)).or_insert(i)), it.pixels.map(|p| *by_pixels.entry(p).or_insert(i))];
        for j in firsts.into_iter().flatten() {
            let (a, b) = (root(&mut parent, i), root(&mut parent, j));
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut units: Vec<Vec<usize>> = Vec::new();
    let mut unit_of: HashMap<usize, usize> = HashMap::new();
    for i in 0..items.len() {
        let r = root(&mut parent, i);
        match unit_of.get(&r) {
            Some(&u) => units[u].push(i),
            None => {
                unit_of.insert(r, units.len());
                units.push(vec![i]);
            }
        }
    }
    // Every member of a unit looks the same; hash by one that has a dHash.
    let reps: Vec<usize> = units.iter().map(|u| u.iter().copied().find(|&i| items[i].dhash.is_some()).unwrap_or(u[0])).collect();
    // 2. Similar: cluster units by their representative's hash.
    let mut clusters: Vec<Vec<usize>> = Vec::new(); // unit indices
    if similar {
        let usable: Vec<usize> = (0..units.len()).filter(|&u| items[reps[u]].dhash.is_some() && items[reps[u]].detail >= MIN_DETAIL).collect();
        let mut tree = BkTree::new();
        for &u in &usable {
            tree.insert(items[reps[u]].dhash.unwrap_or(0), u);
        }
        // Deterministic order: by path.
        let mut order = usable.clone();
        order.sort_by(|&a, &b| crate::catalog::natural_cmp(&items[reps[a]].path.to_string_lossy(), &items[reps[b]].path.to_string_lossy()));
        let mut assigned = vec![false; units.len()];
        let mut cands = Vec::new();
        for &u in &order {
            if assigned[u] {
                continue;
            }
            assigned[u] = true;
            let hu = items[reps[u]].dhash.unwrap_or(0);
            cands.clear();
            tree.query(hu, threshold, &mut cands);
            cands.retain(|&c| !assigned[c]);
            // Closest first, ties by path, for stable groups.
            cands.sort_by(|&a, &b| {
                let (ia, ib) = (&items[reps[a]], &items[reps[b]]);
                hamming(hu, ia.dhash.unwrap_or(0)).cmp(&hamming(hu, ib.dhash.unwrap_or(0))).then_with(|| crate::catalog::natural_cmp(&ia.path.to_string_lossy(), &ib.path.to_string_lossy()))
            });
            let mut members = vec![u];
            for &c in &cands {
                let ic = &items[reps[c]];
                let fits = members.iter().all(|&m| {
                    let im = &items[reps[m]];
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
    let path_cmp = |a: &usize, b: &usize| crate::catalog::natural_cmp(&items[*a].path.to_string_lossy(), &items[*b].path.to_string_lossy());
    let wasted = |members: &[usize]| -> u64 {
        let total: u64 = members.iter().map(|&i| items[i].size).sum();
        total - members.iter().map(|&i| items[i].size).max().unwrap_or(0)
    };
    // 3. Identical groups first, most reclaimable space first.
    let mut identical: Vec<(usize, Vec<usize>)> = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.len() >= 2)
        .map(|(u, unit)| {
            let mut members = unit.clone();
            members.sort_by(path_cmp);
            (u, members)
        })
        .collect();
    identical.sort_by(|a, b| wasted(&b.1).cmp(&wasted(&a.1)));
    let mut group_of_unit = vec![None; units.len()];
    let mut groups: Vec<Group> = Vec::new();
    for (u, members) in identical {
        group_of_unit[u] = Some(groups.len());
        let key = |i: usize| (items[i].size, items[i].content);
        let same_bytes = members.iter().all(|&i| items[i].content.is_some() && key(i) == key(members[0]));
        groups.push(Group { kind: GroupKind::Identical, members, linked: Vec::new(), min_similarity: 100, same_bytes });
    }
    // 4. Similar groups: one file per unit, linked to the unit's Identical group.
    let mut similar_groups: Vec<Group> = clusters
        .into_iter()
        .map(|c| {
            let mut shown: Vec<(usize, Option<usize>)> = c.iter().map(|&u| (units[u].iter().copied().min_by(|a, b| path_cmp(a, b)).unwrap_or(reps[u]), group_of_unit[u])).collect();
            shown.sort_by(|a, b| path_cmp(&a.0, &b.0));
            let hashes: Vec<u128> = c.iter().filter_map(|&u| items[reps[u]].dhash).collect();
            let mut min_similarity = 100;
            for a in 0..hashes.len() {
                for b in a + 1..hashes.len() {
                    min_similarity = min_similarity.min(similarity_pct(hamming(hashes[a], hashes[b])));
                }
            }
            Group { kind: GroupKind::Similar, members: shown.iter().map(|s| s.0).collect(), linked: shown.iter().map(|s| s.1).collect(), min_similarity, same_bytes: false }
        })
        .collect();
    similar_groups.sort_by(|a, b| wasted(&b.members).cmp(&wasted(&a.members)));
    groups.extend(similar_groups);
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

    /// Top-to-bottom gradient from `top` to `bottom` gray with faint rings,
    /// like a sky or a studio backdrop: almost no detail from left to right.
    fn vertical_gradient(w: u32, h: u32, top: f32, bottom: f32, seed: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            let mut l = top + (bottom - top) * v;
            let (cx, cy) = (0.3 + 0.1 * seed as f32, 0.5);
            let r = ((u - cx).powi(2) + (v - cy).powi(2)).sqrt();
            if (r * 40.0).fract() < 0.05 {
                l += 6.0;
            }
            let g = l.clamp(0.0, 255.0) as u8;
            image::Rgba([g, g.saturating_add(10), g / 2, 255])
        })
    }

    fn fingerprinted(name: &str, img: &RgbaImage, size: u64) -> ScanItem {
        let (h, detail) = dhash_detail(img);
        ScanItem { dims: Some(img.dimensions()), content: Some(size), dhash: Some(h), detail, ..ScanItem::new(PathBuf::from(name), size, None) }
    }

    #[test]
    fn vertical_gradients_and_their_resized_copies_match() {
        let g = vertical_gradient(800, 600, 210.0, 50.0, 0);
        let half = image::imageops::resize(&g, 400, 300, image::imageops::FilterType::Triangle);
        let gentle = vertical_gradient(800, 600, 90.0, 150.0, 1);
        let gentle_small = image::imageops::resize(&gentle, 200, 150, image::imageops::FilterType::Lanczos3);
        // Nothing to compare across a row, plenty down a column.
        let (h, detail) = dhash_detail(&g);
        assert!(detail >= MIN_DETAIL, "{detail}");
        assert!(dhash_detail(&gentle).1 >= MIN_DETAIL);
        assert!((h as u64).count_ones() > 48, "{h:032x}");
        assert!(hamming(h, dhash(&half)) <= 4, "{}", hamming(h, dhash(&half)));
        assert!(hamming(dhash(&gentle), dhash(&gentle_small)) <= 4);
        // Opposite directions are different pictures.
        assert!(hamming(h, dhash(&gentle)) >= 40, "{}", hamming(h, dhash(&gentle)));
        let items = vec![
            fingerprinted("g.png", &g, 1000),
            fingerprinted("g_half.png", &half, 400),
            fingerprinted("s.png", &gentle, 900),
            fingerprinted("s_small.png", &gentle_small, 100),
            fingerprinted("flat.png", &RgbaImage::from_pixel(80, 60, image::Rgba([90, 90, 90, 255])), 50),
        ];
        let groups = group(&items, true, threshold_for_pct(90));
        let mut found: Vec<Vec<usize>> = groups.iter().map(|g| g.members.clone()).collect();
        found.sort();
        assert_eq!(found, vec![vec![0, 1], vec![2, 3]], "{groups:?}");
        assert!(groups.iter().all(|g| g.kind == GroupKind::Similar));
    }

    #[test]
    fn similarity_conversion() {
        assert_eq!(similarity_pct(0), 100);
        assert_eq!(similarity_pct(HASH_BITS), 0);
        assert_eq!(similarity_pct(64), 50);
        assert_eq!(threshold_for_pct(100), 0);
        assert_eq!(threshold_for_pct(90), 12);
        assert!(similarity_pct(threshold_for_pct(85)) >= 85);
    }

    fn item(name: &str, size: u64, content: Option<u64>, dhash: Option<u128>, dims: (u32, u32)) -> ScanItem {
        ScanItem { dims: Some(dims), content, dhash, detail: 20.0, ..ScanItem::new(PathBuf::from(name), size, None) }
    }

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }

    #[test]
    fn similar_groups_use_complete_linkage() {
        // A burst: each frame differs from the next by 4 bits, so single
        // linkage would chain all of them; complete linkage must not.
        let mut items = Vec::new();
        let mut h = 0u128;
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
        let hashes: Vec<u128> = (0..300)
            .map(|_| {
                v ^= v << 13;
                v ^= v >> 7;
                v ^= v << 17;
                if v % 3 == 0 {
                    (v & 0xFFFF) as u128
                } else {
                    ((v as u128) << 64) | v.rotate_left(29) as u128
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
            item("d.jpg", 100, Some(3), Some(!0xFF00u128), (10, 10)),
        ];
        let exact = group(&items, false, 6);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].kind, GroupKind::Identical);
        assert_eq!(exact[0].members, vec![0, 1]);
        // The exact pair keeps its own group; the similar group lists it once.
        let sim = group(&items, true, 6);
        assert_eq!(sim.len(), 2);
        assert_eq!(sim[0].kind, GroupKind::Identical);
        assert_eq!(sim[0].members, vec![0, 1]);
        assert_eq!(sim[1].kind, GroupKind::Similar);
        assert_eq!(sim[1].members, vec![0, 2]);
        assert_eq!(sim[1].linked, vec![Some(0), None]);
        let p = pick(&items, &sim[1].members, KeepRule::Resolution);
        assert_eq!((p.index, p.reason, p.decisive), (1, Criterion::Resolution, true));
        assert_eq!(best_in_group(&items, &sim[1].members, KeepRule::Largest), 1);
    }

    /// The reported case: two byte-identical originals and an annotated,
    /// bigger, newer edit of the same size.
    fn originals_and_edit() -> Vec<ScanItem> {
        let mut items = vec![
            item("clone.jpg", 195_656, Some(7), Some(0xF0F0_F0F0), (1200, 800)),
            item("draw.jpg", 328_713, Some(8), Some(0xF0F0_F0F7), (1200, 800)),
            item("dup_of_05.jpg", 195_656, Some(7), Some(0xF0F0_F0F0), (1200, 800)),
        ];
        items[0].modified = at(1_000);
        items[1].modified = at(1_060);
        items[2].modified = at(2_000);
        items
    }

    #[test]
    fn exact_copies_never_lose_to_an_edited_relative() {
        let mut items = originals_and_edit();
        let mut groups = group(&items, true, 6);
        assert_eq!(groups.len(), 2, "{groups:?}");
        let (ident, sim) = (&groups[0], &groups[1]);
        assert_eq!((ident.kind, ident.same_bytes), (GroupKind::Identical, true));
        assert_eq!(ident.members, vec![0, 2]);
        // The identical pair keeps the older file.
        let p = pick(&items, &ident.members, KeepRule::Resolution);
        assert_eq!((ident.members[p.index], p.reason), (0, Criterion::Older));
        // The edit is only a similar relative of the pair's keeper.
        assert_eq!(sim.kind, GroupKind::Similar);
        assert_eq!(sim.members, vec![0, 1]);
        assert_eq!(sim.linked, vec![Some(0), None]);
        // Same resolution: the original still wins over the bigger edit, but
        // the choice is not clear enough to mark anything automatically.
        let p = pick(&items, &sim.members, KeepRule::Resolution);
        assert_eq!((sim.members[p.index], p.reason, p.decisive), (0, Criterion::Older, false));
        // "Largest file" keeps the bigger edit, but a pick between pictures of
        // the same resolution is never clear enough for bulk marking.
        let p = pick(&items, &sim.members, KeepRule::Largest);
        assert_eq!((sim.members[p.index], p.decisive), (1, false));
        // With "Newest" the identical group keeps dup_of_05 and the similar
        // group shows that same copy.
        refresh_representatives(&mut groups, &items, KeepRule::Newest);
        assert_eq!(groups[1].members, vec![2, 1]);
        // A capture date takes precedence over modified times.
        items[0].taken = Some(500);
        items[2].taken = Some(400);
        assert_eq!(groups[0].members[best_in_group(&items, &groups[0].members, KeepRule::Resolution)], 2);
    }

    #[test]
    fn identical_pixels_form_identical_groups() {
        // Same pixels, different bytes (metadata stripped, re-saved PNG).
        let mut items = vec![item("a.jpg", 100, Some(1), Some(0xAB), (10, 10)), item("a_noexif.jpg", 90, Some(2), Some(0xAB), (10, 10)), item("b.png", 300, Some(3), Some(0xAB), (10, 10))];
        items[0].pixels = Some(42);
        items[1].pixels = Some(42);
        items[2].pixels = Some(43);
        let groups = group(&items, true, 6);
        assert_eq!(groups[0].kind, GroupKind::Identical);
        assert_eq!(groups[0].members, vec![0, 1]);
        assert!(!groups[0].same_bytes);
        assert_eq!(groups[1].kind, GroupKind::Similar);
        assert_eq!(groups[1].members, vec![0, 2]);
        // Without similarity matching only the exact copies are found.
        let groups = group(&items, false, 6);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![0, 1]);
    }

    #[test]
    fn tie_break_order() {
        let base = |name: &str| ScanItem { dims: Some((100, 100)), modified: at(5_000), ..ScanItem::new(PathBuf::from(name), 1_000, None) };
        let both = |a: ScanItem, b: ScanItem| {
            let items = vec![a, b];
            let p = pick(&items, &[0, 1], KeepRule::Resolution);
            (p.index, p.reason)
        };
        // 1. Resolution beats everything after it.
        let mut hi = base("x - Copy.jpg");
        hi.dims = Some((200, 200));
        hi.modified = at(9_000);
        hi.size = 10;
        assert_eq!(both(base("x.jpg"), hi), (1, Criterion::Resolution));
        // 2. A name that is not a copy beats an older date and a bigger file.
        let mut copy = base("x (2).jpg");
        copy.modified = at(1);
        copy.size = 9_000;
        assert_eq!(both(copy, base("x.jpg")), (1, Criterion::NotACopy));
        // 3. The older file beats a bigger one.
        let mut big = base("a.jpg");
        big.size = 9_000;
        let mut old = base("b.jpg");
        old.modified = at(1);
        assert_eq!(both(big, old), (1, Criterion::Older));
        // 4. File size decides only when everything else ties.
        let mut big = base("a.jpg");
        big.size = 9_000;
        assert_eq!(both(base("b.jpg"), big), (1, Criterion::Larger));
        // 5. Then the shorter name, then name order.
        assert_eq!(both(base("photo1.jpg"), base("p1.jpg")), (1, Criterion::ShorterName));
        assert_eq!(both(base("b.jpg"), base("a.jpg")), (1, Criterion::NameOrder));
        // The rule comes first: "Largest file" keeps the bigger copy, but
        // only a winner with more pixels than every other file is decisive.
        let mut big = base("x (2).jpg");
        big.size = 9_000;
        let items = vec![base("x.jpg"), big.clone()];
        let p = pick(&items, &[0, 1], KeepRule::Largest);
        assert_eq!((p.index, p.reason, p.decisive), (1, Criterion::Larger, false));
        big.dims = Some((120, 120));
        let items = vec![base("x.jpg"), big];
        let p = pick(&items, &[0, 1], KeepRule::Largest);
        assert_eq!((p.index, p.decisive), (1, true));
        // A rule that keeps a smaller picture is never decisive.
        let mut small_big = base("s.jpg");
        small_big.dims = Some((50, 50));
        small_big.size = 9_000;
        let items = vec![base("x.jpg"), small_big];
        let p = pick(&items, &[0, 1], KeepRule::Largest);
        assert_eq!((p.index, p.decisive), (1, false));
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
