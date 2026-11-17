//! Blind comparison: picking the next trial and turning pairwise votes into
//! a ranking.
//!
//! Votes are scaled with the Bradley-Terry model (the standard for pairwise
//! comparison data, as in ITU-T P.910 paired comparison and pwcmp): every
//! device gets a strength `p` so that P(i beats j) = p_i / (p_i + p_j).
//! Strengths are fitted with Hunter's MM iterations; a tie counts as half a
//! win for each side. Scores are reported on a 0-100 scale (the chance of
//! beating an average device) with a 90% interval from a bootstrap over the
//! votes.

use super::{Choice, Vote};
use std::collections::BTreeMap;

/// Ranking of one device.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub device: String,
    /// Chance (0-100) of being preferred over an average device.
    pub score: f64,
    /// 90% interval of `score`.
    pub low: f64,
    pub high: f64,
    pub wins: f64,
    pub votes: usize,
}

/// Fit Bradley-Terry strengths for `devices` from `votes` (votes naming
/// other devices are ignored). Returns log-strengths centred on 0.
pub fn fit(devices: &[String], votes: &[&Vote]) -> Vec<f64> {
    let n = devices.len();
    if n == 0 {
        return Vec::new();
    }
    let idx: BTreeMap<&str, usize> = devices.iter().enumerate().map(|(i, d)| (d.as_str(), i)).collect();
    // w[i]: wins of i (ties count half); games[i][j]: comparisons of i and j.
    let mut w = vec![0.0f64; n];
    let mut games = vec![vec![0.0f64; n]; n];
    for v in votes {
        let (Some(&a), Some(&b)) = (idx.get(v.a.as_str()), idx.get(v.b.as_str())) else { continue };
        if a == b {
            continue;
        }
        games[a][b] += 1.0;
        games[b][a] += 1.0;
        match v.choice {
            Choice::A => w[a] += 1.0,
            Choice::B => w[b] += 1.0,
            Choice::Tie => {
                w[a] += 0.5;
                w[b] += 0.5;
            }
        }
    }
    // A small prior (one tie against a virtual average device) keeps devices
    // that won or lost everything finite.
    let prior = 0.5;
    let mut p = vec![1.0f64; n];
    for _ in 0..500 {
        let mut next = vec![0.0f64; n];
        let mut change = 0.0f64;
        for i in 0..n {
            let mut denom = 2.0 * prior / (p[i] + 1.0);
            for j in 0..n {
                if games[i][j] > 0.0 {
                    denom += games[i][j] / (p[i] + p[j]);
                }
            }
            next[i] = if denom > 0.0 { (w[i] + prior) / denom } else { 1.0 };
        }
        // Normalize to geometric mean 1.
        let g = (next.iter().map(|x| x.max(1e-12).ln()).sum::<f64>() / n as f64).exp();
        for i in 0..n {
            next[i] /= g;
            change = change.max((next[i] - p[i]).abs());
        }
        p = next;
        if change < 1e-9 {
            break;
        }
    }
    p.iter().map(|x| x.max(1e-12).ln()).collect()
}

/// Chance (0-100) that a device of log-strength `s` beats one of strength 0.
pub fn to_score(s: f64) -> f64 {
    100.0 / (1.0 + (-s).exp())
}

/// Small deterministic generator (xorshift64*), so results repeat.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// Uniform in 0..n (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

/// Rank `devices` from all votes, best first.
pub fn rank(devices: &[String], votes: &[Vote]) -> Vec<Ranked> {
    let all: Vec<&Vote> = votes.iter().collect();
    let s = fit(devices, &all);
    // Bootstrap: refit on votes drawn with replacement.
    let rounds = if all.is_empty() { 0 } else { 200 };
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); devices.len()];
    let mut rng = Rng::new(0x5eed_1234_abcd_0001 ^ all.len() as u64);
    for _ in 0..rounds {
        let pick: Vec<&Vote> = (0..all.len()).map(|_| all[rng.below(all.len())]).collect();
        for (i, v) in fit(devices, &pick).into_iter().enumerate() {
            samples[i].push(to_score(v));
        }
    }
    let mut out: Vec<Ranked> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let mut xs = samples[i].clone();
            xs.sort_by(f64::total_cmp);
            let score = to_score(s[i]);
            let (low, high) = if xs.is_empty() { (score, score) } else { (xs[xs.len() * 5 / 100], xs[(xs.len() * 95 / 100).min(xs.len() - 1)]) };
            let mut wins = 0.0;
            let mut n = 0;
            for v in votes {
                let side = if &v.a == d {
                    Some(Choice::A)
                } else if &v.b == d {
                    Some(Choice::B)
                } else {
                    None
                };
                if let Some(side) = side {
                    n += 1;
                    if v.choice == side {
                        wins += 1.0;
                    } else if v.choice == Choice::Tie {
                        wins += 0.5;
                    }
                }
            }
            Ranked { device: d.clone(), score, low: low.min(score), high: high.max(score), wins, votes: n }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// One blind trial: a scene and two devices, `left` shown on the left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    pub scene: usize,
    pub left: String,
    pub right: String,
}

/// The next trial: the (scene, device pair) with the fewest votes so far,
/// random among equals, sides random. `available[s]` lists the devices with
/// a capture in scene `s`; `scene_names[s]` is its name in the votes.
pub fn next_trial(available: &[Vec<String>], scene_names: &[String], votes: &[Vote], rng: &mut Rng) -> Option<Trial> {
    let mut counts: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for v in votes {
        let (x, y) = if v.a <= v.b { (v.a.clone(), v.b.clone()) } else { (v.b.clone(), v.a.clone()) };
        *counts.entry((v.scene.clone(), x, y)).or_default() += 1;
    }
    let mut best: Vec<(usize, String, String)> = Vec::new();
    let mut best_n = usize::MAX;
    for (s, devs) in available.iter().enumerate() {
        let name = scene_names.get(s).cloned().unwrap_or_default();
        for i in 0..devs.len() {
            for j in i + 1..devs.len() {
                let (x, y) = if devs[i] <= devs[j] { (devs[i].clone(), devs[j].clone()) } else { (devs[j].clone(), devs[i].clone()) };
                let n = counts.get(&(name.clone(), x.clone(), y.clone())).copied().unwrap_or(0);
                if n < best_n {
                    best_n = n;
                    best.clear();
                }
                if n == best_n {
                    best.push((s, x, y));
                }
            }
        }
    }
    if best.is_empty() {
        return None;
    }
    let (scene, x, y) = best.swap_remove(rng.below(best.len()));
    let (left, right) = if rng.below(2) == 0 { (x, y) } else { (y, x) };
    Some(Trial { scene, left, right })
}

/// How often a rater gave opposite answers to the same scene and pair
/// (0..1; lower is more consistent). `None` with no repeated pairs.
pub fn inconsistency(votes: &[Vote], rater: &str) -> Option<f64> {
    let mut by_pair: BTreeMap<(String, String, String), Vec<i8>> = BTreeMap::new();
    for v in votes.iter().filter(|v| v.rater == rater) {
        // +1 = the alphabetically first device won.
        let (x, y, sign) = if v.a <= v.b { (v.a.clone(), v.b.clone(), 1i8) } else { (v.b.clone(), v.a.clone(), -1i8) };
        let r = match v.choice {
            Choice::A => sign,
            Choice::B => -sign,
            Choice::Tie => 0,
        };
        by_pair.entry((v.scene.clone(), x, y)).or_default().push(r);
    }
    let mut pairs = 0usize;
    let mut flips = 0usize;
    for rs in by_pair.values().filter(|r| r.len() >= 2) {
        pairs += 1;
        if rs.iter().any(|&r| r > 0) && rs.iter().any(|&r| r < 0) {
            flips += 1;
        }
    }
    (pairs > 0).then(|| flips as f64 / pairs as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(a: &str, b: &str, c: Choice) -> Vote {
        Vote { scene: "s".into(), a: a.into(), b: b.into(), choice: c, rater: "r".into() }
    }

    #[test]
    fn stronger_device_ranks_first() {
        let devs: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let mut votes = Vec::new();
        for _ in 0..8 {
            votes.push(vote("A", "B", Choice::A));
            votes.push(vote("B", "C", Choice::A));
            votes.push(vote("A", "C", Choice::A));
        }
        votes.push(vote("A", "B", Choice::B));
        votes.push(vote("C", "B", Choice::Tie));
        let r = rank(&devs, &votes);
        assert_eq!(r.iter().map(|x| x.device.as_str()).collect::<Vec<_>>(), vec!["A", "B", "C"]);
        assert!(r[0].score > 60.0 && r[2].score < 40.0, "{r:?}");
        for x in &r {
            assert!(x.low <= x.score && x.score <= x.high);
        }
        assert_eq!(r[0].votes, 17);
        assert_eq!(r[0].wins, 16.0);
    }

    #[test]
    fn no_votes_is_even() {
        let devs: Vec<String> = vec!["A".into(), "B".into()];
        let r = rank(&devs, &[]);
        assert!((r[0].score - 50.0).abs() < 1e-6 && (r[1].score - 50.0).abs() < 1e-6);
    }

    #[test]
    fn all_wins_stay_finite() {
        let devs: Vec<String> = vec!["A".into(), "B".into()];
        let votes: Vec<Vote> = (0..5).map(|_| vote("A", "B", Choice::A)).collect();
        let s = fit(&devs, &votes.iter().collect::<Vec<_>>());
        assert!(s.iter().all(|x| x.is_finite()));
        assert!(s[0] > s[1]);
    }

    #[test]
    fn trials_cover_least_voted_pairs() {
        let avail = vec![vec!["A".to_string(), "B".to_string(), "C".to_string()]];
        let names = vec!["s".to_string()];
        let mut rng = Rng::new(7);
        let mut votes = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let t = next_trial(&avail, &names, &votes, &mut rng).unwrap();
            let mut pair = [t.left.clone(), t.right.clone()];
            pair.sort();
            assert!(seen.insert(pair.clone()), "pair repeated before all were shown");
            votes.push(vote(&t.left, &t.right, Choice::A));
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(next_trial(&[vec!["A".into()]], &names, &[], &mut rng), None);
    }

    #[test]
    fn inconsistency_counts_flips() {
        let votes = vec![vote("A", "B", Choice::A), vote("B", "A", Choice::A), vote("A", "C", Choice::A), vote("C", "A", Choice::B)];
        // A-B flipped (A then B won); A-C consistent.
        assert_eq!(inconsistency(&votes, "r"), Some(0.5));
        assert_eq!(inconsistency(&votes, "nobody"), None);
    }
}
