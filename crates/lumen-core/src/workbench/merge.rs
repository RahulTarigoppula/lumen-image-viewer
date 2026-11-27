//! Three-way merge of a project two people changed at the same time (the
//! project file lives on a shared folder): what each side changed since the
//! version both started from (`base`) is kept; a value both sides changed to
//! different things is a conflict the user has to settle.
//!
//! Scenes, scorecards and measurements are matched by name (scene, device,
//! attribute), so one person scoring a scene while another scores the next
//! one merges cleanly. Blind votes are only ever added: both sides' new votes
//! are kept.

use super::{Device, DeviceMeasure, DeviceScore, Project, Scene, Vote};
use std::collections::{BTreeMap, BTreeSet};

/// One value: the side that changed it wins; `None` when both changed it to
/// different values.
fn pick<T: PartialEq + Clone>(base: &T, mine: &T, theirs: &T) -> Option<T> {
    if mine == base || mine == theirs {
        Some(theirs.clone())
    } else if theirs == base {
        Some(mine.clone())
    } else {
        None
    }
}

/// Merge `mine` and `theirs`, both changed from `base`. `None` when both
/// changed the same value differently.
pub fn merge(base: &Project, mine: &Project, theirs: &Project) -> Option<Project> {
    let mut p = Project {
        format: theirs.format.max(mine.format),
        name: pick(&base.name, &mine.name, &theirs.name)?,
        devices: merge_devices(&base.devices, &mine.devices, &theirs.devices)?,
        reference: pick(&base.reference, &mine.reference, &theirs.reference)?,
        scale: pick(&base.scale, &mine.scale, &theirs.scale)?,
        attributes: pick(&base.attributes, &mine.attributes, &theirs.attributes)?,
        scenes: merge_scenes(&base.scenes, &mine.scenes, &theirs.scenes)?,
        votes: merge_votes(&base.votes, &mine.votes, &theirs.votes)?,
        detected_by: pick(&base.detected_by, &mine.detected_by, &theirs.detected_by)?,
        previous: pick(&base.previous, &mine.previous, &theirs.previous)?,
    };
    // A scale or attribute change on one side and new scores on the other.
    p.normalize();
    Some(p)
}

fn merge_devices(b: &Vec<Device>, m: &Vec<Device>, t: &Vec<Device>) -> Option<Vec<Device>> {
    let names = |v: &[Device]| v.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
    if names(b) == names(m) && names(m) == names(t) {
        b.iter().zip(m).zip(t).map(|((b, m), t)| Some(Device { name: t.name.clone(), alias: pick(&b.alias, &m.alias, &t.alias)? })).collect()
    } else {
        pick(b, m, t)
    }
}

/// Votes are appended: both sides' new ones are kept, theirs first.
fn merge_votes(b: &Vec<Vote>, m: &Vec<Vote>, t: &Vec<Vote>) -> Option<Vec<Vote>> {
    if m.starts_with(b) && t.starts_with(b) {
        let mut out = t.clone();
        out.extend_from_slice(&m[b.len()..]);
        Some(out)
    } else {
        pick(b, m, t)
    }
}

fn merge_scenes(b: &Vec<Scene>, m: &Vec<Scene>, t: &Vec<Scene>) -> Option<Vec<Scene>> {
    let names = |v: &[Scene]| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
    let (nb, nm, nt) = (names(b), names(m), names(t));
    let unique = |n: &[String]| n.iter().collect::<BTreeSet<_>>().len() == n.len();
    if !(unique(&nb) && unique(&nm) && unique(&nt)) {
        return pick(b, m, t);
    }
    // The scene list itself: the side that found the scenes again decides it.
    let order = if nm == nb {
        nt
    } else if nt == nb || nt == nm {
        nm
    } else {
        return None;
    };
    let find = |v: &[Scene], n: &str| -> Option<Scene> { v.iter().find(|s| s.name == n).cloned() };
    // A scene dropped by one side goes only when the other did not change it.
    for s in b.iter().filter(|s| !order.contains(&s.name)) {
        if find(m, &s.name).is_some_and(|x| &x != s) || find(t, &s.name).is_some_and(|x| &x != s) {
            return None;
        }
    }
    order
        .iter()
        .map(|n| match (find(m, n), find(t, n)) {
            (Some(x), Some(y)) => merge_scene(&find(b, n).unwrap_or_else(|| Scene { name: n.clone(), ..Default::default() }), &x, &y),
            // New on one side.
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        })
        .collect()
}

fn merge_scene(b: &Scene, m: &Scene, t: &Scene) -> Option<Scene> {
    let empty_score = |d: &str| DeviceScore { device: d.to_string(), ..Default::default() };
    let empty_measure = |d: &str| DeviceMeasure { device: d.to_string(), ..Default::default() };
    let scores = merge_keyed(&b.scores, &m.scores, &t.scores, |s| &s.device, empty_score, merge_score)?;
    let measures = merge_keyed(&b.measures, &m.measures, &t.measures, |s| &s.device, empty_measure, |b, m, t| pick(b, m, t))?;
    Some(Scene {
        name: t.name.clone(),
        shots: pick(&b.shots, &m.shots, &t.shots)?,
        scores: scores.into_iter().filter(|s| !s.is_empty()).collect(),
        comment: pick(&b.comment, &m.comment, &t.comment)?,
        done: pick(&b.done, &m.done, &t.done)?,
        measures: measures.into_iter().filter(|x| !x.values.is_empty()).collect(),
    })
}

fn merge_score(b: &DeviceScore, m: &DeviceScore, t: &DeviceScore) -> Option<DeviceScore> {
    let keys: BTreeSet<&String> = b.attrs.keys().chain(m.attrs.keys()).chain(t.attrs.keys()).collect();
    let mut attrs = BTreeMap::new();
    for k in keys {
        if let Some(v) = pick(&b.attrs.get(k), &m.attrs.get(k), &t.attrs.get(k))? {
            attrs.insert(k.clone(), *v);
        }
    }
    Some(DeviceScore { device: t.device.clone(), attrs, verdict: pick(&b.verdict, &m.verdict, &t.verdict)?, note: pick(&b.note, &m.note, &t.note)? })
}

/// Records with a key (a device): each key merged on its own, a missing
/// record taken as `empty(key)`. Theirs' order first, then new ones of mine.
fn merge_keyed<T: Clone + PartialEq>(b: &[T], m: &[T], t: &[T], key: impl Fn(&T) -> &String, empty: impl Fn(&str) -> T, each: impl Fn(&T, &T, &T) -> Option<T>) -> Option<Vec<T>> {
    let mut keys: Vec<&String> = Vec::new();
    for x in t.iter().chain(m).chain(b) {
        if !keys.contains(&key(x)) {
            keys.push(key(x));
        }
    }
    let get = |v: &[T], k: &str| v.iter().find(|x| key(*x) == k).cloned().unwrap_or_else(|| empty(k));
    keys.into_iter().map(|k| each(&get(b, k), &get(m, k), &get(t, k))).collect()
}

#[cfg(test)]
mod tests {
    use super::super::{detect, Choice, Verdict};
    use super::*;

    fn sample() -> Project {
        let d = detect::detect(&[
            detect::FileInfo::new("A56/night.jpg"),
            detect::FileInfo::new("A56/day.jpg"),
            detect::FileInfo::new("X/night.jpg"),
            detect::FileInfo::new("X/day.jpg"),
        ]);
        let mut p = Project::from_detection("Round", &d);
        assert_eq!((p.devices.len(), p.scenes.len()), (2, 2));
        p.reference = Some("A56".into());
        p
    }

    fn vote(scene: &str, choice: Choice) -> Vote {
        Vote { scene: scene.into(), a: "A56".into(), b: "X".into(), choice, rater: "r".into() }
    }

    #[test]
    fn both_raters_votes_are_kept() {
        let mut base = sample();
        base.votes = vec![vote("day", Choice::A)];
        let (mut mine, mut theirs) = (base.clone(), base.clone());
        mine.votes.push(vote("night", Choice::B));
        theirs.votes.push(vote("day", Choice::Tie));
        theirs.votes.push(vote("night", Choice::A));
        let m = merge(&base, &mine, &theirs).unwrap();
        assert_eq!(m.votes.len(), 4);
        assert_eq!(m.votes[1..3], theirs.votes[1..]);
        assert_eq!(m.votes[3], mine.votes[1]);
        // Nothing changed here: theirs as it is.
        assert_eq!(merge(&base, &base, &theirs).unwrap(), theirs);
        assert_eq!(merge(&base, &mine, &base).unwrap(), mine);
    }

    #[test]
    fn scores_merge_per_scene_device_and_attribute() {
        let base = sample();
        let (mut mine, mut theirs) = (base.clone(), base.clone());
        // Different scenes, and different attributes of one device.
        mine.scenes[0].score_mut("X").attrs.insert("Noise".into(), 4);
        mine.scenes[0].comment = "flare".into();
        theirs.scenes[1].score_mut("X").verdict = Some(Verdict::Win);
        theirs.scenes[0].score_mut("X").attrs.insert("Color".into(), 2);
        let x = base.device_index("X").unwrap();
        theirs.devices[x].alias = "Phone X".into();
        let m = merge(&base, &mine, &theirs).unwrap();
        let x0 = m.scenes[0].score("X").unwrap();
        assert_eq!((x0.attrs.get("Noise"), x0.attrs.get("Color")), (Some(&4), Some(&2)));
        assert_eq!(m.scenes[0].comment, "flare");
        assert_eq!(m.scenes[1].score("X").unwrap().verdict, Some(Verdict::Win));
        assert_eq!(m.display("X"), "Phone X");
        // The same score set to two values: a conflict.
        theirs.scenes[0].score_mut("X").attrs.insert("Noise".into(), 1);
        assert!(merge(&base, &mine, &theirs).is_none());
        // ... but the same value on both sides is not.
        theirs.scenes[0].score_mut("X").attrs.insert("Noise".into(), 4);
        assert!(merge(&base, &mine, &theirs).is_some());
    }

    #[test]
    fn scenes_found_again_on_one_side() {
        let base = sample();
        let (mut mine, mut theirs) = (base.clone(), base.clone());
        // Theirs found a new scene; mine commented on an old one.
        theirs.scenes.push(Scene { name: "dusk".into(), ..Default::default() });
        mine.scenes[1].comment = "soft".into();
        let m = merge(&base, &mine, &theirs).unwrap();
        let names: Vec<&str> = m.scenes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [base.scenes[0].name.as_str(), base.scenes[1].name.as_str(), "dusk"]);
        assert_eq!(m.scenes[1].comment, "soft");
        // Theirs dropped a scene mine changed: a conflict.
        let mut theirs = base.clone();
        theirs.scenes.remove(1);
        assert!(merge(&base, &mine, &theirs).is_none());
        // ... a scene nobody changed just goes.
        mine.scenes[1].comment.clear();
        mine.scenes[0].done = true;
        let m = merge(&base, &mine, &theirs).unwrap();
        assert_eq!(m.scenes.len(), 1);
        assert!(m.scenes[0].done);
    }
}
