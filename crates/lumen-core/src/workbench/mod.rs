//! The image quality workbench: a project groups captures of the same scenes
//! from several devices (phones, cameras, builds), holds the team's verdicts
//! (attribute scorecards, Win / Par / Lose against a reference device, blind
//! votes) and is saved as `lumen-project.json` beside the captures.
//!
//! Paths are stored relative to the project file, so the folder can move or
//! live on a network drive. Nothing here knows about the UI.

pub mod detect;
pub mod measure;
pub mod merge;
pub mod rank;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The project file's name inside the captures folder.
pub const PROJECT_FILE: &str = "lumen-project.json";

/// Current file format version.
pub const FORMAT: u32 = 1;

/// Attributes of a camera scorecard (the usual list of camera test protocols).
pub const DEFAULT_ATTRIBUTES: [&str; 8] = ["Exposure", "Color", "Detail", "Noise", "Artifacts", "Focus", "Dynamic range", "Overall"];

/// Artifact tags for marks, with a short description for hover text.
pub const ARTIFACT_TAGS: [(&str, &str); 20] = [
    ("Noise", "Grain or color speckle, mostly in dark or flat areas"),
    ("Blur", "Soft detail: focus miss, motion or heavy noise reduction"),
    ("Over-sharpening", "Halos and crunchy edges from too much sharpening"),
    ("Texture loss", "Smeared fine texture (grass, hair, fabric) from noise reduction"),
    ("Halo", "Bright or dark rim along high-contrast edges"),
    ("Ghosting", "Double edges from merging several frames (HDR, night mode)"),
    ("Flare", "Veiling glare or bright spots from a light source"),
    ("Clipped highlights", "Bright areas burnt to white, detail lost"),
    ("Crushed shadows", "Dark areas blocked to black, detail lost"),
    ("White balance", "Wrong overall color temperature or tint"),
    ("Color shift", "Wrong hue of an object or skin tone"),
    ("Color fringing", "Purple or green edges (chromatic aberration)"),
    ("Banding", "Visible steps in smooth gradients such as sky"),
    ("Moire", "False patterns on fine repeating detail"),
    ("Ringing", "Ripples next to edges, often from compression"),
    ("Compression", "Blocks or mosquito noise from strong compression"),
    ("Distortion", "Bent straight lines or stretched faces at the edges"),
    ("Demosaic error", "Zipper or maze patterns on fine detail"),
    ("Motion artifact", "Judder, jello or rolling shutter skew in moving scenes"),
    ("Other", "Anything else worth flagging"),
];

/// Device colors (the same order as the compare pane badges).
pub const DEVICE_COLORS: [[u8; 3]; 8] = [[47, 111, 235], [234, 120, 32], [34, 160, 92], [142, 84, 214], [214, 64, 96], [0, 150, 170], [150, 120, 40], [96, 104, 120]];

/// How serious a flagged artifact is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    #[default]
    Minor,
    Moderate,
    Severe,
}

impl Severity {
    pub const ALL: [Severity; 3] = [Severity::Minor, Severity::Moderate, Severity::Severe];
    pub fn label(self) -> &'static str {
        match self {
            Severity::Minor => "Minor",
            Severity::Moderate => "Moderate",
            Severity::Severe => "Severe",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Severity::Minor => "Visible when looking for it at 100%",
            Severity::Moderate => "Noticeable at normal viewing size",
            Severity::Severe => "Obvious at a glance; a user would complain",
        }
    }
}

/// Scorecard scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scale {
    /// 1 (bad) to 5 (excellent) per device.
    #[default]
    Absolute5,
    /// -3 (much worse) to +3 (much better) than the reference device.
    Relative3,
}

impl Scale {
    pub fn range(self) -> (i32, i32) {
        match self {
            Scale::Absolute5 => (1, 5),
            Scale::Relative3 => (-3, 3),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Scale::Absolute5 => "1 to 5",
            Scale::Relative3 => "-3 to +3 vs reference",
        }
    }
    /// What a score means, for hover text.
    pub fn describe(self, v: i32) -> &'static str {
        match (self, v) {
            (Scale::Absolute5, 1) => "Bad: unacceptable",
            (Scale::Absolute5, 2) => "Poor: clearly visible problems",
            (Scale::Absolute5, 3) => "Fair: acceptable with flaws",
            (Scale::Absolute5, 4) => "Good: minor flaws",
            (Scale::Absolute5, 5) => "Excellent: no visible flaws",
            (Scale::Relative3, -3) => "Much worse than the reference",
            (Scale::Relative3, -2) => "Worse than the reference",
            (Scale::Relative3, -1) => "Slightly worse than the reference",
            (Scale::Relative3, 0) => "Same as the reference",
            (Scale::Relative3, 1) => "Slightly better than the reference",
            (Scale::Relative3, 2) => "Better than the reference",
            (Scale::Relative3, 3) => "Much better than the reference",
            _ => "",
        }
    }
}

/// The verdict of one device on one scene against the reference device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Verdict {
    Win,
    Par,
    Lose,
}

impl Verdict {
    pub const ALL: [Verdict; 3] = [Verdict::Win, Verdict::Par, Verdict::Lose];
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Win => "Win",
            Verdict::Par => "Par",
            Verdict::Lose => "Lose",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Verdict::Win => "Better than the reference device on this scene",
            Verdict::Par => "About the same as the reference device",
            Verdict::Lose => "Worse than the reference device on this scene",
        }
    }
    pub fn color(self) -> [u8; 3] {
        match self {
            Verdict::Win => [34, 160, 92],
            Verdict::Par => [140, 146, 158],
            Verdict::Lose => [214, 64, 64],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    /// The name detection found (folder or file name part); the key.
    pub name: String,
    /// What to call it in the UI and reports; empty = `name`.
    pub alias: String,
}

impl Device {
    pub fn display(&self) -> &str {
        if self.alias.trim().is_empty() {
            &self.name
        } else {
            self.alias.trim()
        }
    }
}

/// One capture of a scene.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shot {
    pub device: String,
    /// Relative to the project folder with `/` separators, or absolute.
    pub path: String,
}

/// One device's scorecard on one scene.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceScore {
    pub device: String,
    /// Attribute -> score on the project's scale.
    pub attrs: BTreeMap<String, i32>,
    pub verdict: Option<Verdict>,
    pub note: String,
}

impl DeviceScore {
    pub fn is_empty(&self) -> bool {
        self.attrs.is_empty() && self.verdict.is_none() && self.note.trim().is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scene {
    pub name: String,
    pub shots: Vec<Shot>,
    pub scores: Vec<DeviceScore>,
    pub comment: String,
    /// The reviewer marked the scene as reviewed.
    pub done: bool,
    /// Objective measurements per device (see [`measure`]).
    pub measures: Vec<DeviceMeasure>,
}

/// Headline values of the measured metrics for one device on one scene.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceMeasure {
    pub device: String,
    /// Metric id -> headline value.
    pub values: BTreeMap<String, f64>,
}

impl Scene {
    pub fn shot(&self, device: &str) -> Option<&Shot> {
        self.shots.iter().find(|s| s.device == device)
    }
    pub fn score(&self, device: &str) -> Option<&DeviceScore> {
        self.scores.iter().find(|s| s.device == device)
    }
    /// The scorecard of `device`, created when missing.
    pub fn score_mut(&mut self, device: &str) -> &mut DeviceScore {
        let k = match self.scores.iter().position(|s| s.device == device) {
            Some(k) => k,
            None => {
                self.scores.push(DeviceScore { device: device.to_string(), ..Default::default() });
                self.scores.len() - 1
            }
        };
        &mut self.scores[k]
    }
    pub fn measure(&self, device: &str) -> Option<&DeviceMeasure> {
        self.measures.iter().find(|m| m.device == device)
    }

    pub fn has_scores(&self) -> bool {
        self.scores.iter().any(|s| !s.is_empty())
    }
}

/// One blind vote: which of two devices looked better on a scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub scene: String,
    pub a: String,
    pub b: String,
    pub choice: Choice,
    #[serde(default)]
    pub rater: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    A,
    B,
    Tie,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub format: u32,
    pub name: String,
    pub devices: Vec<Device>,
    /// The device the others are judged against (golden reference).
    pub reference: Option<String>,
    pub scale: Scale,
    pub attributes: Vec<String>,
    pub scenes: Vec<Scene>,
    pub votes: Vec<Vote>,
    /// How the devices were found, for the record.
    pub detected_by: String,
    /// The previous round's project folder (relative to this project's
    /// folder when possible) for regression tracking.
    pub previous: Option<String>,
}

impl Default for Project {
    fn default() -> Self {
        Project {
            format: FORMAT,
            name: String::new(),
            devices: Vec::new(),
            reference: None,
            scale: Scale::default(),
            attributes: DEFAULT_ATTRIBUTES.iter().map(|s| s.to_string()).collect(),
            scenes: Vec::new(),
            votes: Vec::new(),
            detected_by: String::new(),
            previous: None,
        }
    }
}

/// A loaded project: the data plus where it lives.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub project: Project,
    /// Folder of the project file; relative paths start here.
    pub root: PathBuf,
}

impl Project {
    /// A project from a detection result.
    pub fn from_detection(name: &str, d: &detect::Detection) -> Project {
        Project {
            name: name.to_string(),
            devices: d.devices.iter().map(|n| Device { name: n.clone(), alias: String::new() }).collect(),
            scenes: d
                .scenes
                .iter()
                .map(|s| Scene { name: s.name.clone(), shots: s.shots.iter().map(|(dev, p)| Shot { device: dev.clone(), path: p.clone() }).collect(), ..Default::default() })
                .collect(),
            detected_by: d.method.describe().to_string(),
            ..Default::default()
        }
    }

    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }

    /// Display name of a device key.
    pub fn display<'a>(&'a self, name: &'a str) -> &'a str {
        self.device(name).map(|d| d.display()).unwrap_or(name)
    }

    pub fn device_index(&self, name: &str) -> Option<usize> {
        self.devices.iter().position(|d| d.name == name)
    }

    pub fn device_color(&self, name: &str) -> [u8; 3] {
        DEVICE_COLORS[self.device_index(name).unwrap_or(DEVICE_COLORS.len() - 1) % DEVICE_COLORS.len()]
    }

    pub fn scene_index(&self, name: &str) -> Option<usize> {
        self.scenes.iter().position(|s| s.name == name)
    }

    /// Scene cells with no capture: (scene index, device).
    pub fn missing(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (i, s) in self.scenes.iter().enumerate() {
            for d in &self.devices {
                if s.shot(&d.name).is_none() {
                    out.push((i, d.name.clone()));
                }
            }
        }
        out
    }

    /// Scene cells with more than one capture: (scene index, device, count).
    pub fn duplicates(&self) -> Vec<(usize, String, usize)> {
        let mut out = Vec::new();
        for (i, s) in self.scenes.iter().enumerate() {
            for d in &self.devices {
                let n = s.shots.iter().filter(|x| x.device == d.name).count();
                if n > 1 {
                    out.push((i, d.name.clone(), n));
                }
            }
        }
        out
    }

    /// Keep scores in range after a scale change and drop scores of
    /// attributes that no longer exist.
    pub fn normalize(&mut self) {
        let (lo, hi) = self.scale.range();
        let attrs = self.attributes.clone();
        for s in &mut self.scenes {
            for sc in &mut s.scores {
                sc.attrs.retain(|k, _| attrs.contains(k));
                for v in sc.attrs.values_mut() {
                    *v = (*v).clamp(lo, hi);
                }
            }
            s.scores.retain(|sc| !sc.is_empty());
        }
        if let Some(r) = &self.reference {
            if self.device(r).is_none() {
                self.reference = None;
            }
        }
    }

    /// Tally per device over all scenes.
    pub fn tally(&self) -> Vec<DeviceTally> {
        self.devices
            .iter()
            .map(|d| {
                let mut t = DeviceTally { device: d.name.clone(), ..Default::default() };
                let mut sums: BTreeMap<String, (f64, usize)> = BTreeMap::new();
                for s in &self.scenes {
                    if s.shot(&d.name).is_some() {
                        t.scenes += 1;
                    }
                    let Some(sc) = s.score(&d.name) else { continue };
                    match sc.verdict {
                        Some(Verdict::Win) => t.win += 1,
                        Some(Verdict::Par) => t.par += 1,
                        Some(Verdict::Lose) => t.lose += 1,
                        None => {}
                    }
                    if !sc.attrs.is_empty() {
                        t.scored += 1;
                    }
                    for (k, v) in &sc.attrs {
                        let e = sums.entry(k.clone()).or_default();
                        e.0 += *v as f64;
                        e.1 += 1;
                    }
                }
                t.mean = self.attributes.iter().filter_map(|a| sums.get(a).map(|(s, n)| (a.clone(), s / *n as f64))).collect();
                t
            })
            .collect()
    }

    /// Metric ids that have a value somewhere in the project, in display order.
    pub fn measured_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = measure::MEASURES.iter().map(|(id, _)| id.to_string()).filter(|id| self.scenes.iter().any(|s| s.measures.iter().any(|m| m.values.contains_key(id)))).collect();
        // Ids from a newer build last.
        for s in &self.scenes {
            for m in &s.measures {
                for k in m.values.keys() {
                    if !ids.contains(k) {
                        ids.push(k.clone());
                    }
                }
            }
        }
        ids
    }

    /// Mean measured value per device and metric: (device, [(metric id, mean)]).
    pub fn measure_means(&self) -> Vec<(String, Vec<(String, f64)>)> {
        let ids = self.measured_ids();
        self.devices
            .iter()
            .map(|d| {
                let means = ids
                    .iter()
                    .filter_map(|id| {
                        let vals: Vec<f64> = self.scenes.iter().filter_map(|s| s.measure(&d.name)?.values.get(id).copied()).collect();
                        (!vals.is_empty()).then(|| (id.clone(), vals.iter().sum::<f64>() / vals.len() as f64))
                    })
                    .collect();
                (d.name.clone(), means)
            })
            .collect()
    }

    /// Captures not measured yet: (scene index, device).
    pub fn unmeasured(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (i, s) in self.scenes.iter().enumerate() {
            let mut seen: Vec<&str> = Vec::new();
            for sh in &s.shots {
                if seen.contains(&sh.device.as_str()) {
                    continue;
                }
                seen.push(&sh.device);
                if s.measure(&sh.device).is_none_or(|m| m.values.is_empty()) {
                    out.push((i, sh.device.clone()));
                }
            }
        }
        out
    }

    /// Short plain-language headline per device ("PhoneX: 5 wins, 2 par,
    /// 1 loss against A56 over 8 scenes").
    pub fn headline(&self) -> Vec<String> {
        let reference = self.reference.clone();
        self.tally()
            .into_iter()
            .filter(|t| Some(&t.device) != reference.as_ref())
            .filter(|t| t.win + t.par + t.lose > 0)
            .map(|t| {
                let n = t.win + t.par + t.lose;
                let against = reference.as_deref().map(|r| format!(" against {}", self.display(r))).unwrap_or_default();
                format!(
                    "{}: {} win{}, {} par, {} loss{}{} over {} scene{}",
                    self.display(&t.device),
                    t.win,
                    if t.win == 1 { "" } else { "s" },
                    t.par,
                    t.lose,
                    if t.lose == 1 { "" } else { "es" },
                    against,
                    n,
                    if n == 1 { "" } else { "s" }
                )
            })
            .collect()
    }

    /// Scores, verdicts and notes as CSV (one row per scene and device).
    pub fn scores_csv(&self) -> String {
        let mut out = String::new();
        let mut head = vec!["Scene".to_string(), "Device".to_string(), "File".to_string()];
        head.extend(self.attributes.iter().cloned());
        head.extend(["Verdict".to_string(), "Note".to_string(), "Scene comment".to_string()]);
        let measured = self.measured_ids();
        head.extend(measured.iter().map(|id| format!("Measured: {}", measure::name_of(id))));
        out.push_str(&csv_row(&head));
        for s in &self.scenes {
            for d in &self.devices {
                let Some(shot) = s.shot(&d.name) else { continue };
                let sc = s.score(&d.name);
                let mut row = vec![s.name.clone(), d.display().to_string(), shot.path.clone()];
                for a in &self.attributes {
                    row.push(sc.and_then(|x| x.attrs.get(a)).map(|v| v.to_string()).unwrap_or_default());
                }
                row.push(sc.and_then(|x| x.verdict).map(|v| v.label().to_string()).unwrap_or_default());
                row.push(sc.map(|x| x.note.clone()).unwrap_or_default());
                row.push(s.comment.clone());
                for id in &measured {
                    row.push(s.measure(&d.name).and_then(|m| m.values.get(id)).map(|v| format!("{v:.4}")).unwrap_or_default());
                }
                out.push_str(&csv_row(&row));
            }
        }
        out
    }
}

/// Totals for one device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceTally {
    pub device: String,
    /// Scenes with a capture of this device.
    pub scenes: usize,
    /// Scenes with at least one attribute score.
    pub scored: usize,
    pub win: usize,
    pub par: usize,
    pub lose: usize,
    /// Mean score per attribute (attributes with no score are absent).
    pub mean: Vec<(String, f64)>,
}

/// One CSV line (RFC 4180 quoting) ending in CRLF.
pub fn csv_row(cells: &[String]) -> String {
    let mut s = cells
        .iter()
        .map(|c| {
            // A leading = + - @ would be read as a formula by spreadsheets.
            let c = if c.starts_with(['=', '+', '@']) || (c.starts_with('-') && c.parse::<f64>().is_err()) { format!("'{c}") } else { c.clone() };
            if c.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    s.push_str("\r\n");
    s
}

impl Workspace {
    pub fn file(&self) -> PathBuf {
        self.root.join(PROJECT_FILE)
    }

    /// Absolute path of a stored path.
    pub fn abs(&self, stored: &str) -> PathBuf {
        let p = Path::new(stored);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            let mut out = self.root.clone();
            for part in stored.split('/') {
                out.push(part);
            }
            out
        }
    }

    /// The stored form of a path: relative when inside the project folder.
    pub fn stored(&self, path: &Path) -> String {
        detect::relative(&self.root, path).unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    /// Absolute paths of a scene's captures in device order: (device, path).
    pub fn scene_files(&self, scene: usize) -> Vec<(String, PathBuf)> {
        let Some(s) = self.project.scenes.get(scene) else { return Vec::new() };
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for d in &self.project.devices {
            for sh in s.shots.iter().filter(|x| x.device == d.name) {
                out.push((d.name.clone(), self.abs(&sh.path)));
            }
        }
        // Shots of devices the project no longer lists come last.
        for sh in &s.shots {
            if self.project.device(&sh.device).is_none() {
                out.push((sh.device.clone(), self.abs(&sh.path)));
            }
        }
        out
    }

    /// The scene (and device) a file belongs to.
    pub fn locate(&self, path: &Path) -> Option<(usize, String)> {
        let key = self.stored(path).to_lowercase();
        for (i, s) in self.project.scenes.iter().enumerate() {
            for sh in &s.shots {
                if sh.path.to_lowercase() == key || self.abs(&sh.path).to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy()) {
                    return Some((i, sh.device.clone()));
                }
            }
        }
        None
    }

    /// Detect devices and scenes in `root` and make a new (unsaved) project.
    pub fn detect(root: &Path, exif: &dyn Fn(&Path) -> (Option<String>, Option<i64>)) -> Workspace {
        let files = detect::scan(root, 3, 20_000);
        let infos: Vec<detect::FileInfo> = files
            .iter()
            .filter_map(|p| {
                let rel = detect::relative(root, p)?;
                Some(detect::FileInfo { rel, model: None, taken: None })
            })
            .collect();
        let mut d = detect::detect(&infos);
        // Camera models only matter when the names gave nothing.
        if d.method == detect::Method::Single && infos.len() > 1 {
            let with_exif: Vec<detect::FileInfo> = infos
                .iter()
                .map(|f| {
                    let (model, taken) = exif(&root.join(&f.rel));
                    detect::FileInfo { rel: f.rel.clone(), model, taken }
                })
                .collect();
            d = detect::detect(&with_exif);
        }
        let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Project".into());
        Workspace { project: Project::from_detection(&name, &d), root: root.to_path_buf() }
    }

    /// Load `<root>/lumen-project.json`.
    pub fn load(root: &Path) -> Result<Workspace> {
        let file = root.join(PROJECT_FILE);
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut project: Project = serde_json::from_str(&text).with_context(|| format!("{} is not a Lumen project", file.display()))?;
        anyhow::ensure!(project.format <= FORMAT, "{} was made by a newer Lumen; update Lumen to open it", file.display());
        project.format = FORMAT;
        Ok(Workspace { project, root: root.to_path_buf() })
    }

    /// Write the project file atomically.
    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.project)?;
        crate::encode::write_atomic(&self.file(), &bytes)
    }

    /// Modification time of the file on disk (to notice edits by others).
    pub fn disk_stamp(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.file()).ok()?.modified().ok()
    }
}

/// The folder holding a project file for `path`: the path itself (when it is
/// a folder) or one of its parents, up to `levels` up.
pub fn find_root(path: &Path, levels: usize) -> Option<PathBuf> {
    let mut cur = if path.is_dir() { Some(path.to_path_buf()) } else { path.parent().map(Path::to_path_buf) };
    for _ in 0..=levels {
        let d = cur?;
        if d.join(PROJECT_FILE).is_file() {
            return Some(d);
        }
        cur = d.parent().map(Path::to_path_buf);
    }
    None
}

/// One score that changed between two rounds.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreChange {
    pub scene: String,
    pub device: String,
    pub attribute: String,
    pub before: i32,
    pub after: i32,
}

/// What changed for one device since the previous round.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceDelta {
    pub device: String,
    /// Mean change per attribute over scenes scored in both rounds.
    pub mean: Vec<(String, f64)>,
    pub wins_before: usize,
    pub wins_after: usize,
    pub losses_before: usize,
    pub losses_after: usize,
}

/// Round-over-round comparison: scenes and devices are matched by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoundDiff {
    pub devices: Vec<DeviceDelta>,
    /// Scores that went down by `threshold` or more, worst first.
    pub regressions: Vec<ScoreChange>,
    /// Scores that went up by `threshold` or more, best first.
    pub improvements: Vec<ScoreChange>,
    /// Scenes present in both rounds.
    pub shared_scenes: usize,
}

/// Compare `cur` with the previous round `prev`. Only scenes and devices
/// with the same names in both are compared; a change of at least
/// `threshold` points counts as a regression or improvement.
pub fn compare_rounds(cur: &Project, prev: &Project, threshold: i32) -> RoundDiff {
    let mut diff = RoundDiff::default();
    let mut sums: BTreeMap<(String, String), (f64, usize)> = BTreeMap::new();
    for s in &cur.scenes {
        let Some(o) = prev.scenes.iter().find(|o| o.name == s.name) else { continue };
        diff.shared_scenes += 1;
        for sc in &s.scores {
            let Some(old) = o.score(&sc.device) else { continue };
            for (attr, &after) in &sc.attrs {
                let Some(&before) = old.attrs.get(attr) else { continue };
                let e = sums.entry((sc.device.clone(), attr.clone())).or_default();
                e.0 += (after - before) as f64;
                e.1 += 1;
                let ch = ScoreChange { scene: s.name.clone(), device: sc.device.clone(), attribute: attr.clone(), before, after };
                if after - before <= -threshold {
                    diff.regressions.push(ch);
                } else if after - before >= threshold {
                    diff.improvements.push(ch);
                }
            }
        }
    }
    diff.regressions.sort_by_key(|c| (c.after - c.before, c.scene.clone()));
    diff.improvements.sort_by_key(|c| (c.before - c.after, c.scene.clone()));
    let (tc, tp) = (cur.tally(), prev.tally());
    for d in &cur.devices {
        let now = tc.iter().find(|t| t.device == d.name);
        let before = tp.iter().find(|t| t.device == d.name);
        let mean: Vec<(String, f64)> = cur.attributes.iter().filter_map(|a| sums.get(&(d.name.clone(), a.clone())).map(|(s, n)| (a.clone(), s / *n as f64))).collect();
        if before.is_none() && mean.is_empty() {
            continue;
        }
        diff.devices.push(DeviceDelta {
            device: d.name.clone(),
            mean,
            wins_before: before.map(|t| t.win).unwrap_or(0),
            wins_after: now.map(|t| t.win).unwrap_or(0),
            losses_before: before.map(|t| t.lose).unwrap_or(0),
            losses_after: now.map(|t| t.lose).unwrap_or(0),
        });
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_are_compared_by_name() {
        let mut prev = sample();
        let mut cur = sample();
        prev.scenes[1].score_mut("X").attrs.insert("Noise".into(), 4);
        prev.scenes[1].score_mut("X").attrs.insert("Color".into(), 2);
        prev.scenes[0].score_mut("X").verdict = Some(Verdict::Lose);
        cur.scenes[1].score_mut("X").attrs.insert("Noise".into(), 2);
        cur.scenes[1].score_mut("X").attrs.insert("Color".into(), 4);
        cur.scenes[0].score_mut("X").verdict = Some(Verdict::Win);
        // A scene only in this round is ignored.
        cur.scenes.push(Scene { name: "new".into(), ..Default::default() });
        let d = compare_rounds(&cur, &prev, 1);
        assert_eq!(d.shared_scenes, 2);
        assert_eq!(d.regressions.len(), 1);
        assert_eq!((d.regressions[0].attribute.as_str(), d.regressions[0].before, d.regressions[0].after), ("Noise", 4, 2));
        assert_eq!(d.improvements.len(), 1);
        let x = d.devices.iter().find(|t| t.device == "X").unwrap();
        assert_eq!(x.mean, vec![("Color".to_string(), 2.0), ("Noise".to_string(), -2.0)]);
        assert_eq!((x.wins_before, x.wins_after, x.losses_before, x.losses_after), (0, 1, 1, 0));
        // A higher threshold hides small changes.
        assert!(compare_rounds(&cur, &prev, 3).regressions.is_empty());
    }

    fn sample() -> Project {
        let d = detect::detect(&[
            detect::FileInfo::new("A56/night.jpg"),
            detect::FileInfo::new("A56/day.jpg"),
            detect::FileInfo::new("X/night.jpg"),
            detect::FileInfo::new("X/day.jpg"),
            detect::FileInfo::new("Y/night.jpg"),
        ]);
        Project::from_detection("Round 1", &d)
    }

    #[test]
    fn from_detection_and_completeness() {
        let p = sample();
        assert_eq!(p.devices.len(), 3);
        assert_eq!(p.scenes.len(), 2);
        assert_eq!(p.missing(), vec![(0, "Y".to_string())]);
        assert!(p.duplicates().is_empty());
        assert_eq!(p.attributes.len(), DEFAULT_ATTRIBUTES.len());
    }

    #[test]
    fn tally_and_headline() {
        let mut p = sample();
        p.reference = Some("A56".into());
        p.devices[1].alias = "Phone X".into();
        for (i, v) in [(0, Verdict::Win), (1, Verdict::Lose)] {
            let sc = p.scenes[i].score_mut("X");
            sc.verdict = Some(v);
            sc.attrs.insert("Noise".into(), 4 - i as i32);
        }
        let t = p.tally();
        let x = t.iter().find(|t| t.device == "X").unwrap();
        assert_eq!((x.win, x.par, x.lose, x.scored, x.scenes), (1, 0, 1, 2, 2));
        assert_eq!(x.mean, vec![("Noise".to_string(), 3.5)]);
        assert_eq!(p.headline(), vec!["Phone X: 1 win, 0 par, 1 loss against A56 over 2 scenes".to_string()]);
    }

    #[test]
    fn normalize_clamps_and_drops() {
        let mut p = sample();
        p.scenes[0].score_mut("X").attrs.insert("Noise".into(), 5);
        p.scenes[0].score_mut("X").attrs.insert("Gone".into(), 2);
        p.scale = Scale::Relative3;
        p.reference = Some("nobody".into());
        p.normalize();
        assert_eq!(p.scenes[0].score("X").unwrap().attrs.get("Noise"), Some(&3));
        assert!(!p.scenes[0].score("X").unwrap().attrs.contains_key("Gone"));
        assert_eq!(p.reference, None);
    }

    #[test]
    fn csv_quotes_and_guards_formulas() {
        assert_eq!(csv_row(&["a,b".into(), "say \"hi\"".into(), "=1+2".into(), "-2".into(), "-x".into()]), "\"a,b\",\"say \"\"hi\"\"\",'=1+2,-2,'-x\r\n");
        let mut p = sample();
        p.scenes[1].score_mut("A56").attrs.insert("Color".into(), 4);
        let csv = p.scores_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert!(lines[0].starts_with("Scene,Device,File,Exposure,Color"));
        // 2 scenes: night has 3 devices, day has 2.
        assert_eq!(lines.len(), 1 + 5);
        assert!(csv.contains("night,A56,A56/night.jpg,,4") || csv.contains(",A56,A56/night.jpg,,4"));
    }

    #[test]
    fn save_load_and_paths() {
        let root = std::env::temp_dir().join(format!("lumen-wb-{}", std::process::id()));
        std::fs::create_dir_all(root.join("A56")).unwrap();
        std::fs::write(root.join("A56").join("night.jpg"), b"x").unwrap();
        let ws = Workspace { project: sample(), root: root.clone() };
        assert_eq!(ws.abs("A56/night.jpg"), root.join("A56").join("night.jpg"));
        assert_eq!(ws.stored(&root.join("A56").join("night.jpg")), "A56/night.jpg");
        assert_eq!(ws.locate(&root.join("X").join("day.jpg")), Some((0, "X".to_string())));
        let files = ws.scene_files(1);
        assert_eq!(files.len(), 3);
        assert_eq!(files[2].0, "Y");
        ws.save().unwrap();
        assert_eq!(find_root(&root.join("A56").join("night.jpg"), 2), Some(root.clone()));
        let back = Workspace::load(&root).unwrap();
        assert_eq!(back.project, ws.project);
        // Unknown fields and missing ones are fine.
        std::fs::write(root.join(PROJECT_FILE), br#"{"format":1,"name":"x","future":true}"#).unwrap();
        let min = Workspace::load(&root).unwrap();
        assert_eq!(min.project.name, "x");
        assert_eq!(min.project.attributes.len(), DEFAULT_ATTRIBUTES.len());
        std::fs::write(root.join(PROJECT_FILE), br#"{"format":99}"#).unwrap();
        assert!(Workspace::load(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
