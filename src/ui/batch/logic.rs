//! Pure (UI-free) logic shared by the batch windows: file-name templates,
//! output-size computation, rename validation and the two-phase rename.

use chrono::NaiveDateTime;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ templates

/// Values available to a file-name template.
#[derive(Clone, Default)]
pub struct TemplateCtx<'a> {
    /// File name without extension.
    pub name: &'a str,
    /// Original extension (without the dot).
    pub ext: &'a str,
    /// Counter value for `{n}`.
    pub n: i64,
    /// Default zero-padding for a bare `{n}`.
    pub pad: usize,
    pub modified: Option<NaiveDateTime>,
    pub taken: Option<NaiveDateTime>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Name of the containing folder.
    pub folder: &'a str,
}

/// Tokens shown as clickable chips in the UI: (token, description).
pub const TOKENS: &[(&str, &str)] = &[
    ("{name}", "Original file name"),
    ("{n}", "Counter"),
    ("{n:3}", "Counter padded to 3 digits"),
    ("{date}", "File modified date (YYYY-MM-DD)"),
    ("{taken}", "EXIF date taken (falls back to modified)"),
    ("{width}", "Image width"),
    ("{height}", "Image height"),
    ("{ext}", "Original extension"),
    ("{folder}", "Parent folder name"),
];

/// Whether `template` uses a token whose value needs the image to be probed.
pub fn needs_dims(template: &str) -> bool {
    template.contains("{width") || template.contains("{height")
}

pub fn needs_taken(template: &str) -> bool {
    template.contains("{taken")
}

fn fmt_date(dt: Option<NaiveDateTime>, arg: Option<&str>, default: &str) -> String {
    use std::fmt::Write;
    let Some(dt) = dt else { return String::new() };
    let f = arg.filter(|a| !a.is_empty()).unwrap_or(default);
    let mut s = String::new();
    // An invalid strftime pattern makes Display fail; fall back to the default.
    if write!(s, "{}", dt.format(f)).is_err() {
        s.clear();
        let _ = write!(s, "{}", dt.format(default));
    }
    s
}

/// Expand `{token}` / `{token:arg}` placeholders. Unknown tokens are kept verbatim.
///
/// * `{n}` / `{n:3}` - counter (optionally zero padded)
/// * `{date}` / `{date:%Y%m%d}` - modified date, optional strftime format
/// * `{taken}` / `{taken:%Y-%m-%d}` - EXIF date taken, falls back to modified
/// * `{name}` `{ext}` `{width}` `{height}` `{folder}`
pub fn expand(template: &str, cx: &TemplateCtx) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &after[..end];
        let (key, arg) = match token.split_once(':') {
            Some((k, a)) => (k, Some(a)),
            None => (token, None),
        };
        let value = match key.to_ascii_lowercase().as_str() {
            "name" => Some(cx.name.to_string()),
            "ext" => Some(cx.ext.to_string()),
            "folder" => Some(cx.folder.to_string()),
            "n" => {
                let pad = arg.and_then(|a| a.trim().parse::<usize>().ok()).unwrap_or(cx.pad).min(12);
                Some(if cx.n < 0 { format!("-{:0pad$}", -cx.n) } else { format!("{:0pad$}", cx.n) })
            }
            "date" => Some(fmt_date(cx.modified, arg, "%Y-%m-%d")),
            "taken" => Some(fmt_date(cx.taken.or(cx.modified), arg, "%Y-%m-%d")),
            "width" | "w" => Some(cx.width.map(|v| v.to_string()).unwrap_or_default()),
            "height" | "h" => Some(cx.height.map(|v| v.to_string()).unwrap_or_default()),
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Characters Windows does not allow in file names.
pub fn is_invalid_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32
}

/// Replace characters that are not allowed in Windows file names with `_` and
/// trim trailing dots/spaces.
pub fn sanitize_file_stem(s: &str) -> String {
    let mut out: String = s.chars().map(|c| if is_invalid_char(c) { '_' } else { c }).collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let trimmed = out.trim_start().to_string();
    if trimmed.is_empty() {
        "image".into()
    } else {
        trimmed
    }
}

/// Validate a complete file name (stem + extension). Returns a human readable reason.
pub fn invalid_name_reason(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("Name is empty".into());
    }
    if let Some(c) = name.chars().find(|&c| is_invalid_char(c)) {
        return Some(if (c as u32) < 32 { "Contains a control character".into() } else { format!("Contains invalid character  {c}") });
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Ends with a dot or space".into());
    }
    let stem = name.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let reserved = RESERVED.contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
    if reserved {
        return Some(format!("\"{stem}\" is a reserved name in Windows"));
    }
    if name.chars().count() > 255 {
        return Some("Name is longer than 255 characters".into());
    }
    None
}

// ------------------------------------------------------------ text transforms

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaseChange {
    Keep,
    Lower,
    Upper,
    Title,
}

impl CaseChange {
    pub const ALL: [CaseChange; 4] = [CaseChange::Keep, CaseChange::Lower, CaseChange::Upper, CaseChange::Title];
    pub fn label(self) -> &'static str {
        match self {
            CaseChange::Keep => "Keep",
            CaseChange::Lower => "lowercase",
            CaseChange::Upper => "UPPERCASE",
            CaseChange::Title => "Title Case",
        }
    }
    pub fn apply(self, s: &str) -> String {
        match self {
            CaseChange::Keep => s.to_string(),
            CaseChange::Lower => s.to_lowercase(),
            CaseChange::Upper => s.to_uppercase(),
            CaseChange::Title => {
                let mut out = String::with_capacity(s.len());
                let mut start = true;
                for c in s.chars() {
                    if c.is_alphanumeric() {
                        if start {
                            out.extend(c.to_uppercase());
                        } else {
                            out.extend(c.to_lowercase());
                        }
                        start = false;
                    } else {
                        out.push(c);
                        start = matches!(c, ' ' | '_' | '-' | '.' | '(' | '[');
                    }
                }
                out
            }
        }
    }
}

/// Replace every occurrence of `needle` in `hay` (optionally ignoring case).
pub fn replace_text(hay: &str, needle: &str, with: &str, ignore_case: bool) -> String {
    if needle.is_empty() {
        return hay.to_string();
    }
    if !ignore_case {
        return hay.replace(needle, with);
    }
    let needle_l: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    let chars: Vec<(usize, char)> = hay.char_indices().collect();
    let mut out = String::with_capacity(hay.len());
    let mut i = 0;
    while i < chars.len() {
        // Try to match the lowercase needle starting at char i.
        let mut j = i;
        let mut k = 0;
        while k < needle_l.len() && j < chars.len() {
            let lower: Vec<char> = chars[j].1.to_lowercase().collect();
            if needle_l[k..].starts_with(&lower) {
                k += lower.len();
                j += 1;
            } else {
                break;
            }
        }
        if k == needle_l.len() {
            out.push_str(with);
            i = j;
        } else {
            out.push(chars[i].1);
            i += 1;
        }
    }
    out
}

// --------------------------------------------------------------------- resize

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResizeMode {
    None,
    LongEdge,
    ShortEdge,
    Width,
    Height,
    Percent,
    Fit,
}

impl ResizeMode {
    pub const ALL: [ResizeMode; 7] =
        [ResizeMode::None, ResizeMode::LongEdge, ResizeMode::ShortEdge, ResizeMode::Width, ResizeMode::Height, ResizeMode::Percent, ResizeMode::Fit];
    pub fn label(self) -> &'static str {
        match self {
            ResizeMode::None => "Don't resize",
            ResizeMode::LongEdge => "Long edge",
            ResizeMode::ShortEdge => "Short edge",
            ResizeMode::Width => "Width",
            ResizeMode::Height => "Height",
            ResizeMode::Percent => "Percentage",
            ResizeMode::Fit => "Fit within box",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ResizeSpec {
    pub mode: ResizeMode,
    /// Edge length for LongEdge / ShortEdge / Width / Height, box width for Fit.
    pub a: u32,
    /// Box height for Fit.
    pub b: u32,
    pub percent: f32,
    pub no_upscale: bool,
}

impl Default for ResizeSpec {
    fn default() -> Self {
        ResizeSpec { mode: ResizeMode::None, a: 1920, b: 1080, percent: 50.0, no_upscale: true }
    }
}

/// Largest output side the batch tools will produce.
pub const MAX_OUTPUT_SIDE: u32 = 65_535;
/// Largest output area (one RGBA image this size is about 1.6 GB).
pub const MAX_OUTPUT_PIXELS: u64 = 400_000_000;

/// Refuse output sizes that would need an unreasonable amount of memory.
pub fn check_output_size(w: u32, h: u32) -> Result<(), String> {
    if w > MAX_OUTPUT_SIDE || h > MAX_OUTPUT_SIDE || w as u64 * h as u64 > MAX_OUTPUT_PIXELS {
        return Err(format!(
            "Output would be {w} x {h} px ({:.0} MP). The limit is {MAX_OUTPUT_SIDE} px per side and {} MP; choose a smaller size",
            w as f64 * h as f64 / 1e6,
            MAX_OUTPUT_PIXELS / 1_000_000
        ));
    }
    Ok(())
}

/// Compute the output size for an input of `w` x `h` (aspect ratio preserved).
pub fn output_size(w: u32, h: u32, spec: &ResizeSpec) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w, h);
    }
    let (wf, hf) = (w as f64, h as f64);
    let a = spec.a.max(1) as f64;
    let b = spec.b.max(1) as f64;
    let mut s = match spec.mode {
        ResizeMode::None => return (w, h),
        ResizeMode::LongEdge => a / wf.max(hf),
        ResizeMode::ShortEdge => a / wf.min(hf),
        ResizeMode::Width => a / wf,
        ResizeMode::Height => a / hf,
        ResizeMode::Percent => (spec.percent.max(0.1) as f64) / 100.0,
        ResizeMode::Fit => (a / wf).min(b / hf),
    };
    if spec.no_upscale {
        s = s.min(1.0);
    }
    if (s - 1.0).abs() < 1e-9 {
        return (w, h);
    }
    let nw = ((wf * s).round() as u32).max(1);
    let nh = ((hf * s).round() as u32).max(1);
    // Exact requested edge for edge-based modes (avoid off-by-one from rounding).
    match spec.mode {
        ResizeMode::Width => (spec.a.max(1), nh),
        ResizeMode::Height => (nw, spec.a.max(1)),
        _ => (nw, nh),
    }
}

// --------------------------------------------------------------------- rename

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameIssue {
    /// Two or more files would get the same name.
    Duplicate,
    /// A different file with that name already exists.
    Exists,
    Invalid(String),
}

impl RenameIssue {
    pub fn message(&self) -> String {
        match self {
            RenameIssue::Duplicate => "Duplicate name in this batch".into(),
            RenameIssue::Exists => "A file with this name already exists".into(),
            RenameIssue::Invalid(r) => r.clone(),
        }
    }
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Check every planned target. `exists` reports whether a path exists on disk.
/// Targets that collide with a file that is itself being renamed away are fine
/// (handled by the two-phase rename).
pub fn check_targets(sources: &[PathBuf], targets: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Vec<Option<RenameIssue>> {
    let source_keys: HashSet<String> = sources.iter().map(|p| path_key(p)).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in targets {
        *counts.entry(path_key(t)).or_default() += 1;
    }
    sources
        .iter()
        .zip(targets)
        .map(|(_, t)| {
            let name = t.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if let Some(r) = invalid_name_reason(&name) {
                return Some(RenameIssue::Invalid(r));
            }
            let key = path_key(t);
            if counts.get(&key).copied().unwrap_or(0) > 1 {
                return Some(RenameIssue::Duplicate);
            }
            if !source_keys.contains(&key) && exists(t) {
                return Some(RenameIssue::Exists);
            }
            None
        })
        .collect()
}

/// One step of a two-phase rename: `from` -> `tmp` -> `to`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenameStep {
    pub from: PathBuf,
    pub tmp: PathBuf,
    pub to: PathBuf,
}

/// Build a two-phase plan. Pairs whose source and target are byte-identical
/// are dropped; case-only renames are kept.
pub fn plan_two_phase(pairs: &[(PathBuf, PathBuf)], tag: &str) -> Vec<RenameStep> {
    pairs
        .iter()
        .filter(|(f, t)| f != t)
        .enumerate()
        .map(|(i, (from, to))| {
            let dir = from.parent().unwrap_or(Path::new("."));
            let tmp = dir.join(format!(".lumen-rename-{tag}-{i}.tmp"));
            RenameStep { from: from.clone(), tmp, to: to.clone() }
        })
        .collect()
}

pub struct RenameOutcome {
    pub done: Vec<(PathBuf, PathBuf)>,
    pub errors: Vec<String>,
}

/// Execute a two-phase rename plan. Phase 1 moves every source to a temporary
/// name (rolled back completely if any step fails), phase 2 moves temporaries
/// to their final names. `progress` is called with the number of completed steps.
#[allow(dead_code)]
pub fn execute_plan(plan: &[RenameStep], progress: impl FnMut(usize)) -> RenameOutcome {
    execute_plan_journaled(plan, None, progress)
}

/// Like `execute_plan`, but first writes the plan to `journal` so a crash or
/// power cut between the phases can be repaired with `recover_journal`. The
/// journal is removed once every file has its final (or original) name.
pub fn execute_plan_journaled(plan: &[RenameStep], journal: Option<&Path>, mut progress: impl FnMut(usize)) -> RenameOutcome {
    let mut errors = Vec::new();
    if let Some(j) = journal {
        if let Err(e) = write_journal(j, plan) {
            return RenameOutcome { done: Vec::new(), errors: vec![format!("Cannot write the rename journal ({e}); nothing was renamed")] };
        }
    }
    // Phase 1
    for (i, s) in plan.iter().enumerate() {
        if let Err(e) = std::fs::rename(&s.from, &s.tmp) {
            errors.push(format!("{}: {e}", s.from.display()));
            for r in plan[..i].iter().rev() {
                if let Err(e) = std::fs::rename(&r.tmp, &r.from) {
                    errors.push(format!("Could not restore {} (it is still named {}): {e}", r.from.display(), r.tmp.display()));
                }
            }
            // Keep the journal if something could not be restored, so the next
            // start can try again.
            if errors.len() == 1 {
                if let Some(j) = journal {
                    let _ = std::fs::remove_file(j);
                }
            }
            return RenameOutcome { done: Vec::new(), errors };
        }
        progress(i + 1);
    }
    // Phase 2
    let mut done = Vec::new();
    for (i, s) in plan.iter().enumerate() {
        let res = if s.to.exists() {
            Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "target already exists"))
        } else {
            std::fs::rename(&s.tmp, &s.to)
        };
        match res {
            Ok(()) => done.push((s.from.clone(), s.to.clone())),
            Err(e) => {
                errors.push(format!("{} -> {}: {e}", s.from.display(), s.to.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
                // Put it back under its original name (or a unique variant).
                let back = if s.from.exists() { crate::fileops::unique_path(&s.from) } else { s.from.clone() };
                match std::fs::rename(&s.tmp, &back) {
                    Ok(()) if back != s.from => done.push((s.from.clone(), back)),
                    Ok(()) => {}
                    Err(e) => errors.push(format!("Could not restore {} (it is still named {}): {e}", s.from.display(), s.tmp.display())),
                }
            }
        }
        progress(plan.len() + i + 1);
    }
    if let Some(j) = journal {
        if plan.iter().all(|s| !s.tmp.exists()) {
            let _ = std::fs::remove_file(j);
        }
    }
    RenameOutcome { done, errors }
}

/// Where the rename journal lives.
pub fn journal_path() -> PathBuf {
    crate::settings::data_dir().join("rename-journal.json")
}

pub fn write_journal(path: &Path, plan: &[RenameStep]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(plan).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Repair an interrupted rename from its journal: every file still carrying
/// a temporary name gets its final name (when free) or its original name back.
/// Returns human-readable messages (empty when there was nothing to do); the
/// journal is deleted once no temporary file is left.
pub fn recover_journal(path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let plan: Vec<RenameStep> = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return vec![format!("The rename journal was unreadable and was removed: {e}")];
        }
    };
    let mut msgs = Vec::new();
    let mut fixed = 0;
    for s in &plan {
        if !s.tmp.exists() {
            continue;
        }
        let target = if !s.to.exists() {
            s.to.clone()
        } else if !s.from.exists() {
            s.from.clone()
        } else {
            crate::fileops::unique_path(&s.from)
        };
        match std::fs::rename(&s.tmp, &target) {
            Ok(()) => fixed += 1,
            Err(e) => msgs.push(format!("Could not repair {} (still named {}): {e}", s.from.display(), s.tmp.display())),
        }
    }
    if fixed > 0 {
        msgs.insert(0, format!("Finished an interrupted batch rename: {fixed} file{} repaired", if fixed == 1 { "" } else { "s" }));
    }
    if plan.iter().all(|s| !s.tmp.exists()) {
        let _ = std::fs::remove_file(path);
    }
    msgs
}

// ------------------------------------------------------------ rename settings

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExtCase {
    Keep,
    Lower,
    Upper,
}

impl ExtCase {
    pub const ALL: [ExtCase; 3] = [ExtCase::Keep, ExtCase::Lower, ExtCase::Upper];
    pub fn label(self) -> &'static str {
        match self {
            ExtCase::Keep => "Keep",
            ExtCase::Lower => ".jpg",
            ExtCase::Upper => ".JPG",
        }
    }
    pub fn apply(self, ext: &str) -> String {
        match self {
            ExtCase::Keep => ext.to_string(),
            ExtCase::Lower => ext.to_lowercase(),
            ExtCase::Upper => ext.to_uppercase(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RenameSettings {
    pub template: String,
    pub start: i64,
    pub step: i64,
    /// Zero padding for a bare `{n}` (0 = automatic, based on the file count).
    pub pad: usize,
    pub find: String,
    pub replace: String,
    pub ignore_case: bool,
    pub case: CaseChange,
    pub ext_case: ExtCase,
}

impl Default for RenameSettings {
    fn default() -> Self {
        RenameSettings {
            template: "{name}".into(),
            start: 1,
            step: 1,
            pad: 0,
            find: String::new(),
            replace: String::new(),
            ignore_case: true,
            case: CaseChange::Keep,
            ext_case: ExtCase::Keep,
        }
    }
}

/// Per-file values used to build a new name.
#[derive(Clone, Default)]
pub struct NameInfo {
    pub stem: String,
    /// Extension as it is on disk (original case, no dot).
    pub ext: String,
    pub folder: String,
    pub modified: Option<NaiveDateTime>,
    pub taken: Option<NaiveDateTime>,
    pub dims: Option<(u32, u32)>,
}

impl NameInfo {
    pub fn from_path(p: &Path) -> NameInfo {
        NameInfo {
            stem: p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            ext: p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            folder: p.parent().and_then(|d| d.file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            ..Default::default()
        }
    }
}

/// Automatic counter padding: enough digits for the last number.
pub fn auto_pad(s: &RenameSettings, count: usize) -> usize {
    if s.pad > 0 {
        return s.pad;
    }
    let last = s.start + s.step * (count.saturating_sub(1) as i64);
    last.abs().max(s.start.abs()).max(1).to_string().len()
}

/// Build the new file name (with extension) for the `index`-th file.
pub fn new_file_name(s: &RenameSettings, info: &NameInfo, index: usize, pad: usize) -> String {
    let cx = TemplateCtx {
        name: &info.stem,
        ext: &info.ext,
        n: s.start + s.step * index as i64,
        pad,
        modified: info.modified,
        taken: info.taken,
        width: info.dims.map(|d| d.0),
        height: info.dims.map(|d| d.1),
        folder: &info.folder,
    };
    let mut stem = expand(&s.template, &cx);
    stem = replace_text(&stem, &s.find, &s.replace, s.ignore_case);
    stem = s.case.apply(&stem);
    let ext = s.ext_case.apply(&info.ext);
    if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    }
}

// --------------------------------------------------------------------- helpers

pub fn systime_to_naive(t: std::time::SystemTime) -> NaiveDateTime {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.naive_local()
}

pub fn ts_to_naive(ts: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.naive_utc())
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn interrupted_rename_is_repaired_from_the_journal() {
        let dir = std::env::temp_dir().join(format!("lumen-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();
        // Swap a <-> b, but "crash" after phase 1.
        let plan = plan_two_phase(&[(a.clone(), b.clone()), (b.clone(), a.clone())], "t");
        let journal = dir.join("journal.json");
        write_journal(&journal, &plan).unwrap();
        for s in &plan {
            std::fs::rename(&s.from, &s.tmp).unwrap();
        }
        assert!(!a.exists() && !b.exists());
        let msgs = recover_journal(&journal);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(std::fs::read(&a).unwrap(), b"B");
        assert_eq!(std::fs::read(&b).unwrap(), b"A");
        assert!(!journal.exists());
        assert!(recover_journal(&journal).is_empty());
        // A complete journaled run leaves no journal behind.
        let plan = plan_two_phase(&[(a.clone(), dir.join("c.jpg"))], "u");
        let out = execute_plan_journaled(&plan, Some(&journal), |_| {});
        assert!(out.errors.is_empty());
        assert!(!journal.exists());
        assert!(dir.join("c.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx<'a>() -> TemplateCtx<'a> {
        TemplateCtx {
            name: "IMG_0042",
            ext: "jpg",
            n: 7,
            pad: 1,
            modified: chrono::NaiveDate::from_ymd_opt(2024, 3, 9).and_then(|d| d.and_hms_opt(14, 5, 6)),
            taken: chrono::NaiveDate::from_ymd_opt(2023, 12, 25).and_then(|d| d.and_hms_opt(8, 0, 0)),
            width: Some(4000),
            height: Some(3000),
            folder: "Trip",
        }
    }

    #[test]
    fn template_tokens() {
        let c = cx();
        assert_eq!(expand("{name}", &c), "IMG_0042");
        assert_eq!(expand("{name}_{n}", &c), "IMG_0042_7");
        assert_eq!(expand("photo-{n:3}", &c), "photo-007");
        assert_eq!(expand("{date}", &c), "2024-03-09");
        assert_eq!(expand("{date:%Y%m%d_%H%M}", &c), "20240309_1405");
        assert_eq!(expand("{taken}", &c), "2023-12-25");
        assert_eq!(expand("{width}x{height}", &c), "4000x3000");
        assert_eq!(expand("{folder} {ext}", &c), "Trip jpg");
        assert_eq!(expand("{unknown} {", &c), "{unknown} {");
        assert_eq!(expand("plain", &c), "plain");
        assert_eq!(expand("{N:2}", &c), "07");
    }

    #[test]
    fn template_fallbacks() {
        let mut c = cx();
        c.taken = None;
        assert_eq!(expand("{taken}", &c), "2024-03-09");
        c.width = None;
        assert_eq!(expand("[{width}]", &c), "[]");
        c.pad = 4;
        assert_eq!(expand("{n}", &c), "0007");
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize_file_stem("a:b*c?"), "a_b_c_");
        assert_eq!(sanitize_file_stem("name. "), "name");
        assert_eq!(sanitize_file_stem("  "), "image");
        assert!(invalid_name_reason("ok.jpg").is_none());
        assert!(invalid_name_reason("bad|name.jpg").is_some());
        assert!(invalid_name_reason("CON.jpg").is_some());
        assert!(invalid_name_reason("com1.png").is_some());
        assert!(invalid_name_reason("COM0.png").is_none());
        assert!(invalid_name_reason("trailing.").is_some());
        assert!(invalid_name_reason("").is_some());
    }

    #[test]
    fn case_and_replace() {
        assert_eq!(CaseChange::Title.apply("my holiday_photo-01"), "My Holiday_Photo-01");
        assert_eq!(CaseChange::Upper.apply("abc"), "ABC");
        assert_eq!(replace_text("IMG_IMG", "img", "Pic", true), "Pic_Pic");
        assert_eq!(replace_text("IMG_img", "img", "Pic", false), "IMG_Pic");
        assert_eq!(replace_text("abc", "", "x", true), "abc");
        assert_eq!(replace_text("ÄbcÄ", "ä", "a", true), "abca");
    }

    #[test]
    fn resize_modes() {
        let spec = |mode, a, b| ResizeSpec { mode, a, b, percent: 50.0, no_upscale: true };
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::None, 10, 10)), (4000, 3000));
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::LongEdge, 2000, 0)), (2000, 1500));
        assert_eq!(output_size(3000, 4000, &spec(ResizeMode::LongEdge, 2000, 0)), (1500, 2000));
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::ShortEdge, 1500, 0)), (2000, 1500));
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::Width, 1000, 0)), (1000, 750));
        assert!(check_output_size(20_000, 20_000).is_ok());
        assert!(check_output_size(40_000, 40_000).is_err());
        assert!(check_output_size(65_536, 10).is_err());
        assert!(check_output_size(60_000, 6_000).is_ok());
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::Height, 600, 0)), (800, 600));
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::Percent, 0, 0)), (2000, 1500));
        assert_eq!(output_size(4000, 3000, &spec(ResizeMode::Fit, 1920, 1080)), (1440, 1080));
        assert_eq!(output_size(1000, 4000, &spec(ResizeMode::Fit, 1920, 1080)), (270, 1080));
        // No upscaling
        assert_eq!(output_size(800, 600, &spec(ResizeMode::LongEdge, 2000, 0)), (800, 600));
        assert_eq!(output_size(800, 600, &spec(ResizeMode::Width, 1600, 0)), (800, 600));
        // Upscaling allowed
        let mut s = spec(ResizeMode::Width, 1600, 0);
        s.no_upscale = false;
        assert_eq!(output_size(800, 600, &s), (1600, 1200));
        let mut s = spec(ResizeMode::Percent, 0, 0);
        s.percent = 200.0;
        s.no_upscale = false;
        assert_eq!(output_size(10, 5, &s), (20, 10));
        // Tiny results never collapse to zero.
        let mut s = spec(ResizeMode::Percent, 0, 0);
        s.percent = 1.0;
        assert_eq!(output_size(10, 10, &s), (1, 1));
    }

    #[test]
    fn rename_names() {
        let info = NameInfo { stem: "IMG_001".into(), ext: "JPG".into(), folder: "Trip".into(), dims: Some((10, 20)), ..Default::default() };
        let mut s = RenameSettings { template: "Holiday {n}".into(), start: 1, step: 5, ..Default::default() };
        let pad = auto_pad(&s, 30); // last = 146
        assert_eq!(pad, 3);
        assert_eq!(new_file_name(&s, &info, 0, pad), "Holiday 001.JPG");
        assert_eq!(new_file_name(&s, &info, 2, pad), "Holiday 011.JPG");
        s.ext_case = ExtCase::Lower;
        s.case = CaseChange::Lower;
        assert_eq!(new_file_name(&s, &info, 0, 1), "holiday 1.jpg");
        s.template = "{name}_{width}x{height}".into();
        s.find = "img_".into();
        s.replace = "photo-".into();
        s.case = CaseChange::Keep;
        assert_eq!(new_file_name(&s, &info, 0, 1), "photo-001_10x20.jpg");
        s.pad = 4;
        assert_eq!(auto_pad(&s, 3), 4);
    }

    #[test]
    fn rename_conflicts() {
        let d = PathBuf::from("C:\\x");
        let sources = vec![d.join("a.jpg"), d.join("b.jpg"), d.join("c.jpg"), d.join("d.jpg")];
        let targets = vec![d.join("b.jpg"), d.join("A.jpg"), d.join("same.jpg"), d.join("SAME.jpg")];
        let existing: HashSet<String> = ["c:\\x\\a.jpg", "c:\\x\\b.jpg", "c:\\x\\c.jpg", "c:\\x\\d.jpg"].iter().map(|s| s.to_string()).collect();
        let issues = check_targets(&sources, &targets, |p| existing.contains(&path_key(p)));
        // a->b and b->A form a swap: fine.
        assert_eq!(issues[0], None);
        assert_eq!(issues[1], None);
        assert_eq!(issues[2], Some(RenameIssue::Duplicate));
        assert_eq!(issues[3], Some(RenameIssue::Duplicate));

        let targets = vec![d.join("taken.jpg"), d.join("b?.jpg"), d.join("c.jpg"), d.join("d.jpg")];
        let issues = check_targets(&sources, &targets, |p| path_key(p) == "c:\\x\\taken.jpg");
        assert_eq!(issues[0], Some(RenameIssue::Exists));
        assert!(matches!(issues[1], Some(RenameIssue::Invalid(_))));
        assert_eq!(issues[2], None);
    }

    #[test]
    fn two_phase_swap_on_disk() {
        let dir = std::env::temp_dir().join(format!("lumen-rename-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        std::fs::write(&c, "C").unwrap();
        // Swap a<->b, case-only rename of c, and an identity pair that is dropped.
        let pairs = vec![(a.clone(), b.clone()), (b.clone(), a.clone()), (c.clone(), dir.join("C.txt")), (c.clone(), c.clone())];
        let plan = plan_two_phase(&pairs, "t");
        assert_eq!(plan.len(), 3);
        assert!(plan.iter().all(|s| s.tmp.parent() == Some(dir.as_path())));
        let mut steps = 0;
        let out = execute_plan(&plan, |n| steps = n);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.done.len(), 3);
        assert_eq!(steps, 6);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "A");
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(names.contains(&"C.txt".to_string()), "{names:?}");
        assert_eq!(names.len(), 3, "no temporaries left: {names:?}");

        // Undo = reversed pairs.
        let undo: Vec<(PathBuf, PathBuf)> = out.done.iter().map(|(f, t)| (t.clone(), f.clone())).collect();
        let out2 = execute_plan(&plan_two_phase(&undo, "u"), |_| {});
        assert!(out2.errors.is_empty());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn phase_one_failure_rolls_back() {
        let dir = std::env::temp_dir().join(format!("lumen-rename-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "A").unwrap();
        let missing = dir.join("missing.txt");
        let plan = plan_two_phase(&[(a.clone(), dir.join("x.txt")), (missing, dir.join("y.txt"))], "f");
        let out = execute_plan(&plan, |_| {});
        assert_eq!(out.errors.len(), 1);
        assert!(out.done.is_empty());
        assert!(a.exists(), "first file restored");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
