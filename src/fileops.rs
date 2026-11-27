//! File-system operations and Windows shell integration.

use anyhow::{anyhow, Context, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

/// Size and last-write time of a file, to notice that it was changed since
/// they were read (by another program, or by Lumen itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl FileStamp {
    /// The stamp of the file at `path` now (None when it is not a readable file).
    pub fn of(path: &Path) -> Option<FileStamp> {
        let m = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        Some(FileStamp { len: m.len(), modified: m.modified().ok() })
    }

    /// The file still has the size and time read earlier; a time that could
    /// not be read on either side compares the size alone.
    pub fn matches(&self, len: u64, modified: Option<SystemTime>) -> bool {
        self.len == len && (self.modified.is_none() || modified.is_none() || self.modified == modified)
    }
}

/// Pick a non-existing destination path by appending " (2)", " (3)", ...
pub fn unique_path(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let parent = target.parent().unwrap_or(Path::new("."));
    let stem = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 2..100_000 {
        let p = parent.join(format!("{stem} ({i}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    target.to_path_buf()
}

/// Where Explorer puts a copy made in the same folder: "photo - Copy.jpg",
/// then "photo - Copy (2).jpg", ... (a folder keeps its whole name:
/// "Trip.2024 - Copy"). Never an existing name.
pub fn copy_path(target: &Path, is_dir: bool) -> PathBuf {
    let parent = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let (stem, ext) = match (is_dir, target.file_stem(), target.extension()) {
        (false, Some(s), Some(e)) => (s.to_string_lossy().to_string(), format!(".{}", e.to_string_lossy())),
        _ => (name, String::new()),
    };
    for n in 1..100_000 {
        let p = parent.join(copy_name(&stem, &ext, n));
        if !p.exists() {
            return p;
        }
    }
    unique_path(target)
}

/// The `n`th copy's name: "a - Copy.jpg", "a - Copy (2).jpg", ...
fn copy_name(stem: &str, ext: &str, n: u32) -> String {
    if n <= 1 {
        format!("{stem} - Copy{ext}")
    } else {
        format!("{stem} - Copy ({n}){ext}")
    }
}

pub fn delete_to_trash(paths: &[PathBuf]) -> Result<()> {
    trash::delete_all(paths).map_err(|e| anyhow!("Could not move to Recycle Bin: {e}"))
}

/// What deleting with [`recycle`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // for the delete messages (app.rs, compare.rs, tools.rs)
pub enum TrashOutcome {
    /// Everything went to the Recycle Bin.
    Recycled,
    /// At least one file was on a network or removable drive, which has no
    /// Recycle Bin: those files are deleted for good.
    DeletedPermanently,
}

/// [`delete_to_trash`], also telling whether the files really went to the
/// Recycle Bin, so the message can say when they were deleted permanently.
#[allow(dead_code)] // for the delete messages (app.rs, compare.rs, tools.rs)
pub fn recycle(paths: &[PathBuf]) -> Result<TrashOutcome> {
    let permanent = is_permanent_delete(paths);
    delete_to_trash(paths)?;
    Ok(if permanent { TrashOutcome::DeletedPermanently } else { TrashOutcome::Recycled })
}

/// True when deleting `paths` cannot use the Recycle Bin for all of them:
/// network shares and removable drives have none, so the files would be
/// deleted permanently (the delete confirmation should say so).
#[allow(dead_code)] // for the delete confirmations (app.rs, compare.rs, tools.rs)
pub fn is_permanent_delete(paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.clone());
        match volume_of(&p) {
            Volume::Share => true,
            Volume::Drive(root) => matches!(drive_type(&root), DRIVE_REMOVABLE | DRIVE_REMOTE),
            Volume::Unknown => false,
        }
    })
}

/// GetDriveTypeW results (WinBase.h) for drives without a Recycle Bin.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_REMOTE: u32 = 4;

/// The volume a path lies on, as far as the Recycle Bin is concerned.
#[derive(Debug, PartialEq, Eq)]
enum Volume {
    /// A drive letter, as its root ("C:\").
    Drive(String),
    /// A network share (\\server\share\...).
    Share,
    Unknown,
}

fn volume_of(path: &Path) -> Volume {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Volume::Drive(format!("{}:\\", (d as char).to_ascii_uppercase())),
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => Volume::Share,
            _ => Volume::Unknown,
        },
        _ => Volume::Unknown,
    }
}

#[cfg(windows)]
fn drive_type(root: &str) -> u32 {
    // SAFETY: a valid NUL-terminated wide string for the duration of the call.
    unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(&windows::core::HSTRING::from(root)) }
}

#[cfg(not(windows))]
fn drive_type(_root: &str) -> u32 {
    0
}

/// Copy or move files into `dest_dir`. Returns (source, destination) pairs that succeeded.
/// A name that already exists in `dest_dir` gets a number appended, so nothing is
/// ever overwritten and every move can be undone. A copy into the file's own
/// folder is named like Explorer's ("photo - Copy.jpg").
///
/// A move renames; only a move to another drive copies and then deletes the
/// original. Any other rename failure (a file open in another program, access
/// denied) is reported and nothing is copied. A move whose copy succeeded but
/// whose original could not be deleted still counts as done (the copy is the
/// moved file) and also gets a line in the errors.
pub fn transfer(paths: &[PathBuf], dest_dir: &Path, do_move: bool) -> (Vec<(PathBuf, PathBuf)>, Vec<String>) {
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for src in paths {
        let Some(name) = src.file_name() else { continue };
        // A folder copied or moved into itself (or a sub-folder) would recurse forever.
        if src.is_dir() && is_within(dest_dir, src) {
            errors.push(format!("{}: cannot copy or move a folder into itself", src.display()));
            continue;
        }
        let dst = dest_dir.join(name);
        if dst == *src && do_move {
            continue;
        }
        let own_folder = src.parent().is_some_and(|p| same_path(p, dest_dir));
        let dst = if own_folder && !do_move { copy_path(&dst, src.is_dir()) } else { unique_path(&dst) };
        let res = if do_move { move_any(src, &dst) } else { copy_into_place(src, &dst).map(|()| Moved::Done) };
        match res {
            Ok(Moved::Done) => done.push((src.clone(), dst)),
            Ok(Moved::OriginalKept(e)) => {
                errors.push(format!("{}: copied, but the original could not be deleted ({e})", src.display()));
                done.push((src.clone(), dst));
            }
            Err(e) => errors.push(format!("{}: {e}", src.display())),
        }
    }
    (done, errors)
}

/// Move (or rename) one file or folder to an exact destination, never overwriting.
pub fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(anyhow!("{} already exists", dst.display()));
    }
    if let Some(parent) = dst.parent() {
        if !parent.is_dir() {
            return Err(anyhow!("folder {} no longer exists", parent.display()));
        }
    }
    match move_any(src, dst).with_context(|| format!("{}", src.display()))? {
        Moved::Done => Ok(()),
        // The file is at its destination: that is what counts for the caller.
        Moved::OriginalKept(e) => {
            log::warn!("{} was copied to {}, but the original could not be deleted: {e}", src.display(), dst.display());
            Ok(())
        }
    }
}

/// How [`move_any`] got a file or folder to its destination.
enum Moved {
    Done,
    /// Copied to another drive, but the original could not be deleted.
    OriginalKept(io::Error),
}

/// Rename `src` to `dst`; across drives, copy it and delete the original.
fn move_any(src: &Path, dst: &Path) -> io::Result<Moved> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(Moved::Done),
        Err(e) if is_cross_device(&e) => {
            copy_into_place(src, dst)?;
            Ok(match remove_any(src) {
                Ok(()) => Moved::Done,
                Err(e) => Moved::OriginalKept(e),
            })
        }
        // A locked or protected file: copying it would leave two copies (or,
        // for a folder, a half-deleted original).
        Err(e) => Err(e),
    }
}

/// The rename failed only because the destination is on another drive.
fn is_cross_device(e: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows, EXDEV elsewhere.
    e.raw_os_error() == Some(if cfg!(windows) { 17 } else { 18 })
}

/// `path` is `dir` or lies below it (case-insensitive, as on NTFS).
pub fn is_within(path: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (p, d) = (norm(path), norm(dir));
    p == d || p.starts_with(&format!("{d}\\"))
}

/// The same path, ignoring case and a trailing separator (as on NTFS).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

/// `path` spelled the way it is stored on disk: drive letter in upper case and
/// each existing component in its real case ("c:\photos\2024" ->
/// "C:\Photos\2024"). Windows paths are case-insensitive, but the folder tree,
/// the back / forward history and the title compare paths exactly, so a typed
/// path must match the one listed. Components that cannot be found are kept as
/// given.
pub fn true_case(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => out.push(p.as_os_str().to_string_lossy().to_uppercase()),
                _ => out.push(p.as_os_str()),
            },
            Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                let wild = name.to_string_lossy().contains(['*', '?']);
                let real = if wild { None } else { disk_name(&out.join(name)) };
                out.push(real.as_deref().unwrap_or(name));
            }
        }
    }
    out
}

/// The name of the existing file or folder `path` as stored on disk.
#[cfg(windows)]
fn disk_name(path: &Path) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `data` is a valid out-parameter; the search handle is closed right away.
    let h = unsafe { FindFirstFileW(&windows::core::HSTRING::from(path.as_os_str()), &mut data) }.ok()?;
    let _ = unsafe { FindClose(h) };
    let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
    (len > 0).then(|| std::ffi::OsString::from_wide(&data.cFileName[..len]))
}

#[cfg(not(windows))]
fn disk_name(_path: &Path) -> Option<std::ffi::OsString> {
    None
}

// ------------------------------------------------------------------ copying

/// Set when Lumen quits while a copy is still running: the file being copied
/// stops (CopyFileExW polls this flag and deletes its partial file) and no
/// further copy starts.
static CANCEL_COPIES: AtomicI32 = AtomicI32::new(0);
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);
/// Staging folders of the copies in progress (see [`copy_into_place`]).
static STAGING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn staging() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    STAGING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stop the copy in progress and refuse new ones (Lumen is quitting).
pub fn cancel_copies() {
    CANCEL_COPIES.store(1, Ordering::SeqCst);
}

pub fn copies_cancelled() -> bool {
    CANCEL_COPIES.load(Ordering::SeqCst) != 0
}

/// Delete what copies still running have written so far (best effort: a
/// file still held open stays, inside its hidden staging folder). Returns
/// how many staging folders are left.
pub fn remove_partial_copies() -> usize {
    let mut st = staging();
    st.retain(|s| std::fs::remove_dir_all(s).is_err() && s.exists());
    st.len()
}

/// Copy `src` (a file or a whole folder) to `dst`, which must not exist yet.
/// The copy is written inside a hidden staging folder next to `dst` and only
/// renamed to `dst` once complete, so a copy that fails or is cut short when
/// Lumen quits never leaves a partial file under the real name.
fn copy_into_place(src: &Path, dst: &Path) -> io::Result<()> {
    if copies_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped because Lumen is closing"));
    }
    let (Some(parent), Some(name)) = (dst.parent(), dst.file_name()) else {
        return Err(io::Error::other("invalid destination"));
    };
    let stage = parent.join(format!(".lumen-copy-{}-{}", std::process::id(), STAGE_SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir(&stage)?;
    set_hidden(&stage);
    staging().push(stage.clone());
    let inner = stage.join(name);
    let res = copy_any(src, &inner).and_then(|()| rename_no_replace(&inner, dst));
    let _ = std::fs::remove_dir_all(&stage);
    staging().retain(|s| s != &stage);
    res
}

fn copy_any(src: &Path, dst: &Path) -> io::Result<()> {
    if copies_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped because Lumen is closing"));
    }
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_any(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        copy_file(src, dst)
    }
}

/// Copy one file (attributes and times included, as `std::fs::copy`); it
/// stops early, removing the partial file, once [`cancel_copies`] is called.
#[cfg(windows)]
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{CopyFileExW, COPY_FILE_FAIL_IF_EXISTS};
    let cancel = CANCEL_COPIES.as_ptr() as *mut windows::core::BOOL;
    // SAFETY: valid NUL-terminated wide paths for the call's duration; the
    // cancel flag is a static i32 (BOOL) that outlives the call.
    unsafe { CopyFileExW(&win_path(src), &win_path(dst), None, None, Some(cancel), COPY_FILE_FAIL_IF_EXISTS) }.map_err(io_error)
}

#[cfg(not(windows))]
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

/// Rename `from` to `to` on the same drive, failing (instead of replacing)
/// when `to` exists.
#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
    // SAFETY: valid NUL-terminated wide paths for the call's duration.
    unsafe { MoveFileExW(&win_path(from), &win_path(to), MOVE_FILE_FLAGS(0)) }.map_err(io_error)
}

#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "destination exists"));
    }
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn set_hidden(path: &Path) {
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    // SAFETY: a valid NUL-terminated wide path for the call's duration.
    let _ = unsafe { SetFileAttributesW(&win_path(path), FILE_ATTRIBUTE_HIDDEN) };
}

#[cfg(not(windows))]
fn set_hidden(_path: &Path) {}

/// `path` for a Win32 file call: long paths get the `\\?\` prefix (the
/// standard library adds it on its own; the raw API stops at MAX_PATH).
#[cfg(windows)]
fn win_path(path: &Path) -> windows::core::HSTRING {
    let s = path.as_os_str();
    if s.len() < 248 || s.to_string_lossy().starts_with(r"\\?\") {
        return windows::core::HSTRING::from(s);
    }
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = full.to_string_lossy().replace('/', "\\");
    let verbatim = match text.strip_prefix(r"\\") {
        Some(unc) => format!(r"\\?\UNC\{unc}"),
        None => format!(r"\\?\{text}"),
    };
    windows::core::HSTRING::from(verbatim.as_str())
}

/// A Win32 error carried in a windows-rs error, as an `io::Error` with its
/// plain error code (so `raw_os_error` and the message are the usual ones).
#[cfg(windows)]
fn io_error(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e.to_string())
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(anyhow!("Name cannot be empty"));
    }
    if new_name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')) {
        return Err(anyhow!("A file name can't contain any of: < > : \" / \\ | ? *"));
    }
    let target = path.with_file_name(new_name);
    if target == path {
        return Ok(target);
    }
    // Allow case-only renames on case-insensitive file systems.
    let case_only = target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if target.exists() && !case_only {
        return Err(anyhow!("\"{new_name}\" already exists"));
    }
    std::fs::rename(path, &target).with_context(|| format!("Rename failed for {}", path.display()))?;
    Ok(target)
}

pub fn new_folder(parent: &Path, name: &str) -> Result<PathBuf> {
    let p = unique_path(&parent.join(name.trim()));
    std::fs::create_dir(&p)?;
    Ok(p)
}

pub fn show_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

pub fn open_with_default(path: &Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

pub fn open_with(exe: &Path, path: &Path) -> Result<()> {
    std::process::Command::new(exe).arg(path).spawn()?;
    Ok(())
}

/// Show the Windows "Open with" chooser.
pub fn open_with_dialog(path: &Path) {
    let _ = std::process::Command::new("rundll32.exe")
        .arg("shell32.dll,OpenAs_RunDLL")
        .arg(path)
        .spawn();
}

pub fn properties_dialog(path: &Path) {
    #[cfg(windows)]
    unsafe {
        use windows::core::{w, HSTRING};
        use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};
        let file = HSTRING::from(path.as_os_str());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            lpVerb: w!("properties"),
            lpFile: windows::core::PCWSTR(file.as_ptr()),
            nShow: 1,
            ..Default::default()
        };
        let _ = ShellExecuteExW(&mut info);
    }
}

/// Set the desktop wallpaper. `style`: 0 fill, 1 fit, 2 stretch, 3 tile, 4 center, 5 span.
pub fn set_wallpaper(image_path: &Path, style: u8) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Registry::*;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let (ws, tile) = match style {
            0 => ("10", "0"),
            1 => ("6", "0"),
            2 => ("2", "0"),
            3 => ("0", "1"),
            4 => ("0", "0"),
            _ => ("22", "0"),
        };
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, windows::core::w!("Control Panel\\Desktop"), Some(0), KEY_SET_VALUE, &mut key).is_ok() {
            for (name, val) in [("WallpaperStyle", ws), ("TileWallpaper", tile)] {
                let wide: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let _ = RegSetValueExW(key, &windows::core::HSTRING::from(name), Some(0), REG_SZ, Some(bytes));
            }
            let _ = RegCloseKey(key);
        }
        let mut wide: Vec<u16> = image_path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_mut_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| anyhow!("Could not set wallpaper: {e}"))?;
    }
    Ok(())
}

pub fn copy_image_to_clipboard(img: &image::RgbaImage) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_image(arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::Borrowed(img.as_raw()),
    })?;
    Ok(())
}

pub fn paste_image_from_clipboard() -> Result<image::RgbaImage> {
    let mut cb = arboard::Clipboard::new()?;
    let data = cb.get_image().map_err(|_| anyhow!("The clipboard doesn't contain an image"))?;
    image::RgbaImage::from_raw(data.width as u32, data.height as u32, data.bytes.into_owned())
        .ok_or_else(|| anyhow!("Invalid clipboard image"))
}

pub fn copy_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

// ------------------------------------------------------------------ files on the clipboard

/// The main window: the clipboard's owner when Lumen puts files there.
static MAIN_WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Remember the main window's handle (once, at start).
pub fn set_main_window(hwnd: isize) {
    MAIN_WINDOW.store(hwnd, Ordering::Relaxed);
}

/// DROPEFFECT values: what a drop or a paste does ("Preferred DropEffect" on
/// the clipboard, the cursor of a drag).
pub const EFFECT_NONE: u32 = 0;
pub const EFFECT_COPY: u32 = 1;
pub const EFFECT_MOVE: u32 = 2;
pub const EFFECT_LINK: u32 = 4;

/// Files on the clipboard, as Explorer puts them there.
pub struct ClipboardFiles {
    pub files: Vec<PathBuf>,
    /// They were cut (Ctrl+X): pasting moves them.
    pub cut: bool,
}

/// A "Preferred DropEffect" of move and nothing else means the files were cut
/// (Explorer writes 2 for Cut, 5 = copy + link for Copy).
pub fn is_cut(effect: Option<u32>) -> bool {
    effect.is_some_and(|e| e & (EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK) == EFFECT_MOVE)
}

/// Copy files to the clipboard so they can be pasted in Explorer or another folder.
pub fn copy_files_to_clipboard(paths: &[PathBuf]) -> Result<()> {
    set_clipboard_files(paths, false).map(|_| ())
}

/// Cut files (Ctrl+X): pasting them in Explorer or in Lumen moves them.
/// Returns the clipboard's sequence number after the cut ([`clipboard_seq`]).
pub fn cut_files_to_clipboard(paths: &[PathBuf]) -> Result<u32> {
    set_clipboard_files(paths, true)
}

/// The files on the clipboard (copied or cut in Explorer or Lumen).
pub fn paste_files_from_clipboard() -> Result<ClipboardFiles> {
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Ole::CF_HDROP;
    let fmt = preferred_effect_format();
    let _open = ClipboardOpen::new()?;
    // SAFETY: the clipboard is open; its data stays valid until it is closed.
    let h = unsafe { GetClipboardData(CF_HDROP.0 as u32) }.map_err(|_| anyhow!("The clipboard doesn't contain files"))?;
    let files = hdrop_files(windows::Win32::UI::Shell::HDROP(h.0));
    Ok(ClipboardFiles { files, cut: is_cut(clipboard_u32(fmt)) })
}

/// The clipboard's sequence number: it changes whenever anything new is put
/// on the clipboard (or it is emptied). A cheap call.
pub fn clipboard_seq() -> u32 {
    // SAFETY: no arguments, no clipboard access.
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// After files were moved: a cut of exactly these files on the clipboard is
/// done with, so the clipboard is emptied, as Explorer does after pasting a cut.
pub fn clear_cut_after_move(moved: &[PathBuf]) {
    use windows::Win32::System::DataExchange::{EmptyClipboard, IsClipboardFormatAvailable};
    let fmt = preferred_effect_format();
    // SAFETY: a plain query; the clipboard is not opened.
    if fmt == 0 || moved.is_empty() || unsafe { IsClipboardFormatAvailable(fmt) }.is_err() {
        return;
    }
    let Ok(clip) = paste_files_from_clipboard() else { return };
    if !clip.cut || clip.files.is_empty() {
        return;
    }
    let gone: std::collections::HashSet<String> = moved.iter().map(|p| path_key(p)).collect();
    if clip.files.iter().all(|f| gone.contains(&path_key(f))) {
        if let Ok(_open) = ClipboardOpen::new() {
            // SAFETY: the clipboard is open.
            let _ = unsafe { EmptyClipboard() };
        }
    }
}

/// A path compared the way NTFS does: case and a trailing separator ignored.
fn path_key(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Put `paths` on the clipboard the way Explorer does: the file list
/// (CF_HDROP) and "Preferred DropEffect", which tells a paste to move the
/// files after a cut and to copy them otherwise.
fn set_clipboard_files(paths: &[PathBuf], cut: bool) -> Result<u32> {
    use windows::Win32::System::DataExchange::EmptyClipboard;
    use windows::Win32::System::Ole::CF_HDROP;
    if paths.is_empty() {
        return Err(anyhow!("No files to put on the clipboard"));
    }
    let fmt = preferred_effect_format();
    {
        let _open = ClipboardOpen::new()?;
        // SAFETY: the clipboard is open.
        unsafe { EmptyClipboard() }.map_err(|e| anyhow!("Could not use the clipboard: {e}"))?;
        set_clipboard_bytes(CF_HDROP.0 as u32, &hdrop_bytes(paths))?;
        if fmt != 0 {
            let effect = if cut { EFFECT_MOVE } else { EFFECT_COPY | EFFECT_LINK };
            set_clipboard_bytes(fmt, &effect.to_le_bytes())?;
        }
    }
    Ok(clipboard_seq())
}

/// The registered "Preferred DropEffect" clipboard format (0 if unavailable).
fn preferred_effect_format() -> u32 {
    // SAFETY: a static NUL-terminated wide string.
    unsafe { windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect")) }
}

/// The clipboard, open until this is dropped.
struct ClipboardOpen;

impl ClipboardOpen {
    fn new() -> Result<Self> {
        use windows::Win32::Foundation::HWND;
        let raw = MAIN_WINDOW.load(Ordering::Relaxed);
        let owner = (raw != 0).then(|| HWND(raw as _));
        // Another program may hold it open for a moment.
        for attempt in 0..10u64 {
            // SAFETY: the owner (if known) is Lumen's main window, alive while the app runs.
            if unsafe { windows::Win32::System::DataExchange::OpenClipboard(owner) }.is_ok() {
                return Ok(ClipboardOpen);
            }
            std::thread::sleep(std::time::Duration::from_millis(5 + attempt * 5));
        }
        Err(anyhow!("The clipboard is in use by another program; try again"))
    }
}

impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        // SAFETY: opened in `new`.
        let _ = unsafe { windows::Win32::System::DataExchange::CloseClipboard() };
    }
}

/// Put `bytes` on the open clipboard as `format` (the clipboard owns the memory then).
fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::SetClipboardData;
    let h = global_bytes(bytes)?;
    // SAFETY: `h` is a movable global block; once set, the clipboard owns it.
    if let Err(e) = unsafe { SetClipboardData(format, Some(HANDLE(h.0))) } {
        let _ = unsafe { GlobalFree(Some(h)) };
        return Err(anyhow!("Could not put the files on the clipboard: {e}"));
    }
    Ok(())
}

/// A 4-byte value of the open clipboard (None when it has none).
fn clipboard_u32(format: u32) -> Option<u32> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    if format == 0 {
        return None;
    }
    // SAFETY: the clipboard is open (by the caller); the block is locked while read.
    unsafe {
        let h = HGLOBAL(GetClipboardData(format).ok()?.0);
        if GlobalSize(h) < 4 {
            return None;
        }
        let p = GlobalLock(h) as *const [u8; 4];
        if p.is_null() {
            return None;
        }
        let v = u32::from_le_bytes(std::ptr::read_unaligned(p));
        let _ = GlobalUnlock(h);
        Some(v)
    }
}

/// A movable global memory block holding `bytes`.
fn global_bytes(bytes: &[u8]) -> Result<windows::Win32::Foundation::HGLOBAL> {
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
    // SAFETY: the block is at least `bytes.len()` long and locked while written.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(anyhow!("Out of memory"));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

/// A CF_HDROP block: the 20-byte DROPFILES header, then each path as a wide
/// string ended by a NUL, and one more NUL after the last.
fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(&20u32.to_le_bytes()); // pFiles: the list starts after the header
    v.extend_from_slice(&[0u8; 8]); // pt
    v.extend_from_slice(&0u32.to_le_bytes()); // fNC
    v.extend_from_slice(&1u32.to_le_bytes()); // fWide: UTF-16 paths
    for p in paths {
        for u in p.as_os_str().encode_wide().chain(std::iter::once(0)) {
            v.extend_from_slice(&u.to_le_bytes());
        }
    }
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

/// The paths in a CF_HDROP block (clipboard or drag and drop).
pub fn hdrop_files(h: windows::Win32::UI::Shell::HDROP) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::UI::Shell::DragQueryFileW;
    // SAFETY: `h` is a valid HDROP, owned by the caller for the duration of the call.
    unsafe {
        let n = DragQueryFileW(h, u32::MAX, None);
        (0..n)
            .filter_map(|i| {
                let len = DragQueryFileW(h, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = (DragQueryFileW(h, i, Some(buf.as_mut_slice())) as usize).min(len);
                (got > 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..got])))
            })
            .collect()
    }
}

/// Logical drives ("C:\", "D:\", ...).
pub fn drives() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    unsafe {
        let mask = windows::Win32::Storage::FileSystem::GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) != 0 {
                out.push(PathBuf::from(format!("{}:\\", (b'A' + i as u8) as char)));
            }
        }
    }
    out
}

pub fn volume_label(drive: &Path) -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = windows::core::HSTRING::from(drive.as_os_str());
        let mut name = [0u16; 261];
        if GetVolumeInformationW(&root, Some(&mut name), None, None, None, None).is_ok() {
            let len = name.iter().position(|&c| c == 0).unwrap_or(0);
            let s = String::from_utf16_lossy(&name[..len]);
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Lowercased known folders for the sidebar.
pub fn quick_access() -> Vec<(String, PathBuf, &'static str)> {
    use crate::ui::icons as I;
    let mut v = Vec::new();
    let mut push = |name: &str, p: Option<PathBuf>, icon: &'static str| {
        if let Some(p) = p {
            if p.exists() {
                v.push((name.to_string(), p, icon));
            }
        }
    };
    push("Pictures", dirs::picture_dir(), I::IMAGES);
    push("Desktop", dirs::desktop_dir(), I::DESKTOP);
    push("Downloads", dirs::download_dir(), I::DOWNLOAD_SIMPLE);
    push("Documents", dirs::document_dir(), I::FILE_TEXT);
    push("Videos", dirs::video_dir(), I::FILM_STRIP);
    push("Home", dirs::home_dir(), I::HOUSE);
    v
}

/// Result of dragging files out of Lumen onto another program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    Cancelled,
    Copied,
    /// The target moved the files (e.g. Shift-drag onto an Explorer folder).
    Moved,
    Linked,
}

/// The shell's modal drag loop swallows the button-up: tell the window (and so
/// winit / egui) that the left button is no longer held.
fn release_mouse_button(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_LBUTTONUP};
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            let _ = ScreenToClient(hwnd, &mut pt);
        }
        let lp = ((pt.y as u32 & 0xFFFF) << 16) | (pt.x as u32 & 0xFFFF);
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(lp as isize));
    }
}

/// Start a Windows shell drag-and-drop of `files` (all in one folder), so they can be
/// dropped on Explorer, the desktop, a mail client, an editor ... Blocks until the drop
/// ends (the shell runs its own modal loop). Uses the shell's own data object (the same
/// one Explorer uses, CF_HDROP + shell IDList formats) and the default drop source.
pub fn drag_out(files: &[PathBuf]) -> Result<DragOutcome> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{ILFindLastID, ILFree, SHCreateDataObject, SHDoDragDrop, SHParseDisplayName};

    let Some(first) = files.first() else { return Ok(DragOutcome::Cancelled) };
    let parent = first.parent().ok_or_else(|| anyhow!("no parent folder"))?;
    let files: Vec<&PathBuf> = files.iter().filter(|f| f.parent() == Some(parent)).collect();

    unsafe {
        let parse = |p: &Path| -> Result<*mut ITEMIDLIST> {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(&HSTRING::from(p.as_os_str()), None, &mut pidl, 0, None).with_context(|| format!("{}", p.display()))?;
            Ok(pidl)
        };
        // Full PIDLs of the files; the parent PIDL is the first one minus its last id.
        let mut full: Vec<*mut ITEMIDLIST> = Vec::new();
        let result = (|| -> Result<DragOutcome> {
            for f in &files {
                full.push(parse(f)?);
            }
            let folder = parse(parent)?;
            let children: Vec<*const ITEMIDLIST> = full.iter().map(|p| ILFindLastID(*p) as *const ITEMIDLIST).collect();
            let data: windows::core::Result<IDataObject> = SHCreateDataObject(Some(folder as *const _), Some(&children), None);
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    ILFree(Some(folder as *const _));
                    return Err(anyhow!("could not start dragging: {e}"));
                }
            };
            let hwnd = GetActiveWindow();
            let hwnd = if hwnd.0.is_null() { None } else { Some(hwnd) };
            let effect = SHDoDragDrop(hwnd, &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
            ILFree(Some(folder as *const _));
            if let Some(h) = hwnd {
                release_mouse_button(h);
            }
            let effect = effect.map_err(|e| anyhow!("drag and drop failed: {e}"))?;
            Ok(if effect.0 & DROPEFFECT_MOVE.0 != 0 {
                DragOutcome::Moved
            } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
                DragOutcome::Copied
            } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
                DragOutcome::Linked
            } else {
                DragOutcome::Cancelled
            })
        })();
        for p in full {
            ILFree(Some(p as *const _));
        }
        // DRAGDROP_S_CANCEL / DRAGDROP_S_DROP are success codes; only real failures are Err.
        result
    }
}

/// Take every path in `gone` out of `list` in one pass, leaving the current
/// item (`index`) where removing them one by one would (in any order): the
/// first remaining item after it, else the last remaining one before it.
/// Returns the new index.
pub fn remove_listed(list: &mut Vec<PathBuf>, index: usize, gone: &std::collections::HashSet<&Path>) -> usize {
    if gone.is_empty() {
        return index;
    }
    let keep: Vec<bool> = list.iter().map(|p| !gone.contains(p.as_path())).collect();
    let n = keep.len();
    let target = if index >= n {
        None
    } else if keep[index] {
        Some(index)
    } else {
        (index + 1..n).find(|&i| keep[i]).or_else(|| (0..index).rev().find(|&i| keep[i]))
    };
    let new_index = target.map(|t| keep[..t].iter().filter(|k| **k).count()).unwrap_or(index);
    let mut i = 0;
    list.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    new_index.min(list.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_many_matches_removing_one_by_one() {
        // What the viewer's remove_path does for one file.
        fn one_by_one(list: &mut Vec<PathBuf>, mut index: usize, gone: &[PathBuf]) -> usize {
            for g in gone {
                if let Some(pos) = list.iter().position(|p| p == g) {
                    list.remove(pos);
                    if index > pos || index >= list.len() {
                        index = index.saturating_sub(1).min(list.len().saturating_sub(1));
                    }
                }
            }
            index
        }
        for n in 0..7usize {
            let all: Vec<PathBuf> = (0..n).map(|i| PathBuf::from(format!(r"C:\x\{i}.jpg"))).collect();
            for mask in 0..(1u32 << n) {
                let gone: Vec<PathBuf> = all.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, p)| p.clone()).collect();
                for index in 0..n.max(1) {
                    let mut a = all.clone();
                    let set: std::collections::HashSet<&Path> = gone.iter().map(|p| p.as_path()).collect();
                    let ia = remove_listed(&mut a, index, &set);
                    for order in [gone.clone(), gone.iter().rev().cloned().collect()] {
                        let mut b = all.clone();
                        let ib = one_by_one(&mut b, index, &order);
                        assert_eq!(a, b);
                        assert_eq!(ia, ib, "n {n} mask {mask:b} index {index}");
                    }
                }
            }
        }
    }

    #[test]
    fn true_case_restores_the_spelling_on_disk() {
        let base = std::env::temp_dir().join(format!("lumen-true-case-{}", std::process::id()));
        let dir = base.join("MixedCase Folder");
        std::fs::create_dir_all(&dir).unwrap();
        let typed = PathBuf::from(base.to_string_lossy().to_lowercase()).join("mixedcase folder");
        let fixed = true_case(&typed);
        assert_eq!(fixed.file_name().unwrap(), "MixedCase Folder");
        assert!(fixed.to_string_lossy().starts_with(|c: char| c.is_ascii_uppercase()));
        // Missing components are kept as typed.
        assert_eq!(true_case(&typed.join("not there")).file_name().unwrap(), "not there");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn copies_files_and_folders_without_leftovers() {
        let base = std::env::temp_dir().join(format!("lumen-transfer-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dest) = (base.join("src"), base.join("dest"));
        std::fs::create_dir_all(src.join("folder").join("inner")).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("a.jpg"), b"AAAA").unwrap();
        std::fs::write(src.join("folder").join("inner").join("b.png"), b"BB").unwrap();
        std::fs::write(dest.join("a.jpg"), b"taken").unwrap();
        let (done, errs) = transfer(&[src.join("a.jpg"), src.join("folder")], &dest, false);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(done.len(), 2);
        // The taken name got a number; nothing was replaced and no staging folder is left.
        assert_eq!(names_in(&dest), vec!["a (2).jpg", "a.jpg", "folder"]);
        assert_eq!(std::fs::read(dest.join("a.jpg")).unwrap(), b"taken");
        assert_eq!(std::fs::read(dest.join("a (2).jpg")).unwrap(), b"AAAA");
        assert_eq!(std::fs::read(dest.join("folder").join("inner").join("b.png")).unwrap(), b"BB");
        assert!(src.join("a.jpg").exists(), "a copy keeps the original");
        // A folder copy that fails half-way leaves nothing under the real name.
        assert!(copy_into_place(&src.join("missing"), &dest.join("missing")).is_err());
        assert_eq!(names_in(&dest).len(), 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copies_in_their_own_folder_are_named_like_explorer() {
        let base = std::env::temp_dir().join(format!("lumen-copy-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Trip.2024")).unwrap();
        std::fs::write(base.join("a.jpg"), b"A").unwrap();
        std::fs::write(base.join("README"), b"R").unwrap();
        assert_eq!(copy_path(&base.join("a.jpg"), false), base.join("a - Copy.jpg"));
        assert_eq!(copy_path(&base.join("README"), false), base.join("README - Copy"));
        assert_eq!(copy_path(&base.join("Trip.2024"), true), base.join("Trip.2024 - Copy"));
        // Pasting twice, and a folder, into the folder they are in.
        for _ in 0..2 {
            let (done, errs) = transfer(&[base.join("a.jpg"), base.join("Trip.2024")], &base, false);
            assert!(errs.is_empty(), "{errs:?}");
            assert_eq!(done.len(), 2);
        }
        assert_eq!(names_in(&base), vec!["README", "Trip.2024", "Trip.2024 - Copy", "Trip.2024 - Copy (2)", "a - Copy (2).jpg", "a - Copy.jpg", "a.jpg"]);
        assert_eq!(std::fs::read(base.join("a.jpg")).unwrap(), b"A", "the original is untouched");
        // Moving into the own folder changes nothing.
        let (done, errs) = transfer(&[base.join("a.jpg")], &base, true);
        assert!(done.is_empty() && errs.is_empty());
        assert!(base.join("a.jpg").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn clipboard_file_lists_round_trip() {
        use windows::Win32::UI::Shell::{DROPFILES, HDROP};
        assert_eq!(std::mem::size_of::<DROPFILES>(), 20, "the header written by hdrop_bytes");
        let paths = vec![PathBuf::from(r"C:\Photos\a.jpg"), PathBuf::from("D:\\Fotos 2024\\caf\u{e9} b.png"), PathBuf::from(r"\\server\share\c")];
        let h = global_bytes(&hdrop_bytes(&paths)).unwrap();
        assert_eq!(hdrop_files(HDROP(h.0)), paths);
        // SAFETY: allocated above and not handed to anyone.
        let _ = unsafe { windows::Win32::Foundation::GlobalFree(Some(h)) };
        // Explorer: 2 for Cut, 5 (copy + link) for Copy.
        assert!(is_cut(Some(EFFECT_MOVE)));
        assert!(!is_cut(Some(EFFECT_COPY | EFFECT_LINK)));
        assert!(!is_cut(Some(EFFECT_COPY | EFFECT_MOVE)));
        assert!(!is_cut(Some(EFFECT_NONE)));
        assert!(!is_cut(None));
    }

    #[test]
    fn move_renames_and_reports_other_failures_without_copying() {
        let base = std::env::temp_dir().join(format!("lumen-transfer-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dest) = (base.join("src"), base.join("dest"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("a.jpg"), b"A").unwrap();
        let (done, errs) = transfer(&[src.join("a.jpg")], &dest, true);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(done, vec![(src.join("a.jpg"), dest.join("a.jpg"))]);
        assert!(!src.join("a.jpg").exists());
        // A file held open without delete sharing cannot be renamed: the move
        // is reported as failed and no copy is made.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::write(src.join("locked.jpg"), b"L").unwrap();
            let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(src.join("locked.jpg")).unwrap();
            let (done, errs) = transfer(&[src.join("locked.jpg")], &dest, true);
            assert!(done.is_empty());
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert!(!dest.join("locked.jpg").exists(), "no copy of a locked file");
            drop(held);
            assert!(src.join("locked.jpg").exists());
        }
        assert!(is_cross_device(&io::Error::from_raw_os_error(if cfg!(windows) { 17 } else { 18 })));
        assert!(!is_cross_device(&io::Error::from_raw_os_error(5)));
        assert!(!is_cross_device(&io::Error::from_raw_os_error(32)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(windows)]
    fn volumes_without_a_recycle_bin() {
        assert_eq!(volume_of(Path::new(r"c:\photos\a.jpg")), Volume::Drive(r"C:\".into()));
        assert_eq!(volume_of(Path::new(r"\\?\D:\a.jpg")), Volume::Drive(r"D:\".into()));
        assert_eq!(volume_of(Path::new(r"\\server\share\a.jpg")), Volume::Share);
        assert_eq!(volume_of(Path::new(r"\\?\UNC\server\share\a.jpg")), Volume::Share);
        assert_eq!(volume_of(Path::new("a.jpg")), Volume::Unknown);
        assert!(is_permanent_delete(&[PathBuf::from(r"\\server\share\a.jpg")]));
        // The temp folder is on a fixed drive, which has a Recycle Bin.
        assert!(!is_permanent_delete(&[std::env::temp_dir().join("a.jpg")]));
        assert!(!is_permanent_delete(&[]));
    }

    #[test]
    fn file_stamps_notice_changes() {
        let dir = std::env::temp_dir().join(format!("lumen-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        std::fs::write(&f, b"one").unwrap();
        let s = FileStamp::of(&f).unwrap();
        assert_eq!(s.len, 3);
        assert!(s.matches(3, s.modified));
        assert!(!s.matches(4, s.modified));
        assert!(s.matches(3, None), "no time: size only");
        let later = s.modified.map(|t| t + std::time::Duration::from_secs(5));
        assert!(!s.matches(3, later));
        assert_eq!(FileStamp::of(&dir), None, "folders have no stamp");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
