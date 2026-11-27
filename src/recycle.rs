//! Undo for deletes: find what Lumen moved to the Recycle Bin and put it back.
//!
//! The Recycle Bin is read through the shell: each entry carries the folder it
//! was deleted from, its name and when it was deleted. Every deleted path is
//! matched to the newest entry with its folder and name that was deleted no
//! earlier than the delete itself ([`match_entries`], a pure function), and
//! that entry is moved back with `IFileOperation` (no dialogs).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// An entry of the Recycle Bin.
#[derive(Clone, Debug, PartialEq)]
pub struct BinEntry {
    /// Where the shell keeps it (`C:\$Recycle.Bin\<user>\$R....jpg`): the
    /// stored file keeps the original extension.
    pub stored: PathBuf,
    /// Its name as the Recycle Bin shows it (without the extension when
    /// Explorer hides the extensions of known file types).
    pub name: String,
    /// The folder it was deleted from.
    pub from: PathBuf,
    /// When it was deleted (None when the shell did not say).
    pub deleted: Option<SystemTime>,
}

/// A deleted file or folder to look for.
#[derive(Clone, Debug)]
pub struct Wanted {
    pub path: PathBuf,
    /// Its folder as spelled on disk, when that differs from `path` (through a
    /// junction or a substituted drive): the Recycle Bin records that one.
    pub real_folder: Option<PathBuf>,
}

/// Entries deleted this much before the delete started still count (the
/// clocks of Lumen and of the shell are read at slightly different moments).
const SLACK: Duration = Duration::from_secs(2);

/// For each of `wanted` (deleted at `since`), the index in `bin` of the entry
/// that is it: deleted from the same folder under the same name, at or after
/// `since` (less [`SLACK`]), the newest such entry. Each entry goes to one
/// path only; a path without an entry gets None.
pub fn match_entries(wanted: &[Wanted], since: SystemTime, bin: &[BinEntry]) -> Vec<Option<usize>> {
    let earliest = since.checked_sub(SLACK).unwrap_or(since);
    let mut used = vec![false; bin.len()];
    let mut out = Vec::with_capacity(wanted.len());
    for w in wanted {
        let (Some(folder), Some(name)) = (w.path.parent(), w.path.file_name()) else {
            out.push(None);
            continue;
        };
        let name = name.to_string_lossy();
        let mut best: Option<usize> = None;
        for (i, e) in bin.iter().enumerate() {
            if used[i] || e.deleted.is_some_and(|t| t < earliest) || !same_name(e, &name) {
                continue;
            }
            if !same_folder(&e.from, folder) && !w.real_folder.as_deref().is_some_and(|r| same_folder(&e.from, r)) {
                continue;
            }
            // The newest; an entry without a date only when nothing else fits.
            if best.is_none_or(|b| e.deleted >= bin[b].deleted) {
                best = Some(i);
            }
        }
        if let Some(b) = best {
            used[b] = true;
        }
        out.push(best);
    }
    out
}

/// `entry` is the file or folder called `name`.
fn same_name(entry: &BinEntry, name: &str) -> bool {
    let shown = entry.name.to_lowercase();
    if shown == name.to_lowercase() {
        return true;
    }
    // Known extensions hidden: the bin shows "photo", the stored "$R12AB34.jpg"
    // keeps the ".jpg" of "photo.jpg".
    let p = Path::new(name);
    match (p.file_stem(), p.extension(), entry.stored.extension()) {
        (Some(stem), Some(ext), Some(kept)) => shown == stem.to_string_lossy().to_lowercase() && ext.to_string_lossy().to_lowercase() == kept.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

/// The same folder, ignoring case, separators, a trailing backslash and the
/// `\\?\` prefix.
fn same_folder(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

fn norm(p: &Path) -> String {
    let s = plain(p).to_string_lossy().replace('/', "\\");
    s.trim_end_matches('\\').to_lowercase()
}

/// `p` without the `\\?\` prefix that `canonicalize` adds.
fn plain(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// `path`'s folder as spelled on disk (long names, through junctions), when
/// that is different. A folder that is gone is spelled from its nearest
/// existing parent.
fn real_folder(path: &Path) -> Option<PathBuf> {
    let folder = path.parent()?;
    let mut existing = folder;
    let mut gone = Vec::new();
    let mut real = loop {
        match std::fs::canonicalize(existing) {
            Ok(r) => break plain(&r),
            Err(_) => {
                gone.push(existing.file_name()?);
                existing = existing.parent()?;
            }
        }
    };
    for part in gone.iter().rev() {
        real.push(part);
    }
    (!same_folder(&real, folder)).then_some(real)
}

/// A FILETIME (100 ns steps since 1601) as a system time.
fn from_filetime(ticks: u64) -> Option<SystemTime> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix.checked_mul(100)?))
}

/// What [`restore`] did.
#[derive(Debug, Default)]
pub struct Restored {
    /// (original path, where it is now): a different name only when the
    /// original one was taken.
    pub done: Vec<(PathBuf, PathBuf)>,
    /// Paths that could not be put back, with the reason in plain words.
    pub failed: Vec<(PathBuf, String)>,
}

/// Put `paths`, moved to the Recycle Bin at `since`, back where they were. A
/// name that is taken now gets a number added; a folder that is gone is made
/// again. The shell calls run on a thread of their own (they need COM in a
/// single-threaded apartment); `progress(done, total)` follows the work.
pub fn restore(paths: &[PathBuf], since: SystemTime, progress: &(dyn Fn(usize, usize) + Sync)) -> Restored {
    let ran = std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("lumen-restore".into())
            .spawn_scoped(s, || {
                use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
                // SAFETY: COM is set up for this thread only and released below,
                // after every COM object of `restore_now` has been dropped.
                let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let out = restore_now(paths, since, progress);
                if init.is_ok() {
                    unsafe { CoUninitialize() };
                }
                out
            })
            .map_err(|e| e.to_string())
            .and_then(|h| h.join().map_err(|_| "it stopped unexpectedly".to_string()))
    });
    ran.unwrap_or_else(|why| Restored { done: Vec::new(), failed: paths.iter().map(|p| (p.clone(), format!("the Recycle Bin could not be read ({why})"))).collect() })
}

fn restore_now(paths: &[PathBuf], since: SystemTime, progress: &(dyn Fn(usize, usize) + Sync)) -> Restored {
    let mut out = Restored::default();
    let total = paths.len();
    progress(0, total);
    let bin = match list_bin() {
        Ok(b) => b,
        Err(e) => {
            out.failed = paths.iter().map(|p| (p.clone(), format!("the Recycle Bin could not be read ({})", e.message()))).collect();
            return out;
        }
    };
    let wanted: Vec<Wanted> = paths.iter().map(|p| Wanted { path: p.clone(), real_folder: real_folder(p) }).collect();
    let found = match_entries(&wanted, since, &bin);
    for (i, (path, hit)) in paths.iter().zip(found).enumerate() {
        progress(i, total);
        match hit {
            Some(hit) => match put_back(&bin[hit].stored, path) {
                Ok(now) => out.done.push((path.clone(), now)),
                Err(e) => out.failed.push((path.clone(), e)),
            },
            None => out.failed.push((path.clone(), "it is no longer in the Recycle Bin".into())),
        }
    }
    progress(total, total);
    out
}

/// Move the Recycle Bin entry stored at `stored` back to `original` (or, when
/// that name is taken, the same name with a number). Returns where it is now.
fn put_back(stored: &Path, original: &Path) -> Result<PathBuf, String> {
    let Some(folder) = original.parent() else { return Err("it has no folder".into()) };
    if !folder.is_dir() {
        std::fs::create_dir_all(folder).map_err(|e| format!("its folder could not be made again ({e})"))?;
    }
    let target = crate::fileops::unique_path(original);
    let Some(name) = target.file_name() else { return Err("it has no name".into()) };
    move_item(stored, folder, name).map_err(|e| e.message().to_string())?;
    if target.exists() {
        Ok(target)
    } else {
        Err("Windows did not move it back".into())
    }
}

/// Move one item into `folder` under `name`, without any dialog.
fn move_item(stored: &Path, folder: &Path, name: &OsStr) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName, FOFX_EARLYFAILURE, FOF_NO_UI, FOF_RENAMEONCOLLISION};
    // SAFETY: plain COM calls on this thread's apartment; every string lives
    // until the call that reads it returns.
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        // No dialogs; never replace a file that took the name meanwhile.
        op.SetOperationFlags(FOF_NO_UI | FOF_RENAMEONCOLLISION | FOFX_EARLYFAILURE)?;
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(stored.as_os_str()), None)?;
        let dest: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(folder.as_os_str()), None)?;
        op.MoveItem(&item, &dest, &HSTRING::from(name), None)?;
        op.PerformOperations()?;
        if op.GetAnyOperationsAborted()?.as_bool() {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_ABORT, "the move was stopped"));
        }
    }
    Ok(())
}

/// Every entry of the Recycle Bin that the shell can describe (others are skipped).
fn list_bin() -> windows::core::Result<Vec<BinEntry>> {
    use windows::Win32::UI::Shell::{BHID_EnumItems, FOLDERID_RecycleBinFolder, IEnumShellItems, IShellItem, SHGetKnownFolderItem, KF_FLAG_DEFAULT};
    // SAFETY: COM calls on this thread's apartment; `got` outlives the call
    // that writes it.
    unsafe {
        let bin: IShellItem = SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = bin.BindToHandler(None, &BHID_EnumItems)?;
        let mut out = Vec::new();
        loop {
            let mut got = 0u32;
            let mut one = [None];
            items.Next(&mut one, Some(&mut got as *mut u32))?;
            let Some(item) = one[0].take().filter(|_| got > 0) else { break };
            if let Some(e) = describe(&item) {
                out.push(e);
            }
        }
        Ok(out)
    }
}

/// One Recycle Bin item: where it is stored, its name, where and when it was deleted.
fn describe(item: &windows::Win32::UI::Shell::IShellItem) -> Option<BinEntry> {
    use windows::core::Interface;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::UI::Shell::{IShellItem2, PID_DISPLACED_DATE, PID_DISPLACED_FROM, PSGUID_DISPLACED, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_PARENTRELATIVE};
    // System.Recycle.DeletedFrom and System.Recycle.DateDeleted.
    const DELETED_FROM: PROPERTYKEY = PROPERTYKEY { fmtid: PSGUID_DISPLACED, pid: PID_DISPLACED_FROM };
    const DATE_DELETED: PROPERTYKEY = PROPERTYKEY { fmtid: PSGUID_DISPLACED, pid: PID_DISPLACED_DATE };
    // SAFETY: the strings come from the shell (CoTaskMem) and are freed by `take`.
    unsafe {
        let stored = take(item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).ok()?);
        let name = take(item.GetDisplayName(SIGDN_PARENTRELATIVE).ok()?);
        let item2: IShellItem2 = item.cast().ok()?;
        let from = take(item2.GetString(&DELETED_FROM).ok()?);
        let deleted = item2.GetFileTime(&DATE_DELETED).ok().and_then(|t| from_filetime(((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64));
        Some(BinEntry { stored: PathBuf::from(stored), name: name.to_string_lossy().into_owned(), from: PathBuf::from(from), deleted })
    }
}

/// A shell-allocated string, freed.
unsafe fn take(s: windows::core::PWSTR) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    if s.is_null() {
        return std::ffi::OsString::new();
    }
    // SAFETY: `s` is a NUL-terminated string from CoTaskMemAlloc, freed once here.
    unsafe {
        let out = std::ffi::OsString::from_wide(s.as_wide());
        windows::Win32::System::Com::CoTaskMemFree(Some(s.0 as *const std::ffi::c_void));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn entry(stored: &str, name: &str, from: &str, deleted: Option<u64>) -> BinEntry {
        BinEntry { stored: PathBuf::from(stored), name: name.into(), from: PathBuf::from(from), deleted: deleted.map(at) }
    }

    fn want(p: &str) -> Wanted {
        Wanted { path: PathBuf::from(p), real_folder: None }
    }

    #[test]
    fn picks_the_newest_entry_of_the_same_file_deleted_since() {
        let bin = vec![
            entry(r"C:\$Recycle.Bin\S-1\$RAAAAAA.jpg", "a.jpg", r"C:\Photos", Some(5)),
            entry(r"C:\$Recycle.Bin\S-1\$RBBBBBB.jpg", "a.jpg", r"C:\Photos", Some(100)),
            entry(r"C:\$Recycle.Bin\S-1\$RCCCCCC.jpg", "a.jpg", r"C:\Other", Some(101)),
            entry(r"C:\$Recycle.Bin\S-1\$RDDDDDD.jpg", "a.jpg", r"C:\Photos", Some(99)),
            entry(r"C:\$Recycle.Bin\S-1\$REEEEEE.png", "b.png", r"c:\photos\", Some(100)),
        ];
        let got = match_entries(&[want(r"C:\Photos\a.jpg"), want(r"C:\Photos\B.PNG"), want(r"C:\Photos\c.jpg")], at(100), &bin);
        // a.jpg: the newest in C:\Photos (not the one from C:\Other); b.png: case
        // and a trailing backslash do not matter; c.jpg is not there.
        assert_eq!(got, vec![Some(1), Some(4), None]);
        // An entry deleted before the delete (beyond the slack) is an older delete.
        assert_eq!(match_entries(&[want(r"C:\Photos\a.jpg")], at(200), &bin), vec![None]);
        // Within the slack it counts.
        assert_eq!(match_entries(&[want(r"C:\Photos\a.jpg")], at(101), &bin), vec![Some(1)]);
    }

    #[test]
    fn each_entry_is_used_once() {
        let bin = vec![entry(r"C:\$Recycle.Bin\S-1\$R1.jpg", "a.jpg", r"C:\P", Some(10)), entry(r"C:\$Recycle.Bin\S-1\$R2.jpg", "a.jpg", r"C:\P", Some(11))];
        // The same path twice (deleted, made again, deleted again): newest first.
        assert_eq!(match_entries(&[want(r"C:\P\a.jpg"), want(r"C:\P\a.jpg"), want(r"C:\P\a.jpg")], at(9), &bin), vec![Some(1), Some(0), None]);
    }

    #[test]
    fn hidden_extensions_and_undated_entries() {
        let bin = vec![
            entry(r"C:\$Recycle.Bin\S-1\$R1.png", "photo", r"C:\P", Some(10)),
            entry(r"C:\$Recycle.Bin\S-1\$R2.jpg", "photo", r"C:\P", Some(10)),
            entry(r"C:\$Recycle.Bin\S-1\$R3", "My folder", r"C:\P", None),
        ];
        // "photo" with the extension hidden: the stored file's extension decides.
        assert_eq!(match_entries(&[want(r"C:\P\photo.jpg"), want(r"C:\P\photo.png"), want(r"C:\P\photo.gif")], at(10), &bin), vec![Some(1), Some(0), None]);
        // No date: still found (there is nothing newer).
        assert_eq!(match_entries(&[want(r"C:\P\My folder")], at(10), &bin), vec![Some(2)]);
        // A dated entry wins over an undated one.
        let mut two = bin.clone();
        two.push(entry(r"C:\$Recycle.Bin\S-1\$R4", "My folder", r"C:\P", Some(12)));
        assert_eq!(match_entries(&[want(r"C:\P\My folder")], at(10), &two), vec![Some(3)]);
    }

    #[test]
    fn folders_compare_as_spelled_on_disk() {
        let bin = vec![entry(r"D:\$Recycle.Bin\S-1\$R1.jpg", "a.jpg", r"D:\Real\Photos", Some(10))];
        // Deleted through another spelling (a junction or subst drive).
        let w = Wanted { path: PathBuf::from(r"X:\Photos\a.jpg"), real_folder: Some(PathBuf::from(r"D:\Real\Photos")) };
        assert_eq!(match_entries(&[w], at(10), &bin), vec![Some(0)]);
        assert_eq!(match_entries(&[want(r"X:\Photos\a.jpg")], at(10), &bin), vec![None]);
        assert!(same_folder(Path::new(r"\\?\C:\A\b"), Path::new(r"c:/a/B/")));
        assert!(same_folder(Path::new(r"\\?\UNC\server\share\x"), Path::new(r"\\server\share\x")));
        assert_eq!(match_entries(&[want(r"a.jpg"), want("")], at(10), &bin), vec![None, None]);
    }

    #[test]
    fn filetimes() {
        assert_eq!(from_filetime(116_444_736_000_000_000), Some(SystemTime::UNIX_EPOCH));
        assert_eq!(from_filetime(116_444_736_000_000_000 + 15_000_000), Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1500)));
        assert_eq!(from_filetime(5), None);
    }

    /// The real Recycle Bin: delete, restore, a taken name, a missing folder.
    #[test]
    fn restores_from_the_recycle_bin() {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let base = std::env::temp_dir().join(format!("lumen-restore-{}-{stamp}", std::process::id()));
        let dir = base.join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.jpg"), dir.join("b.png"), dir.join("never deleted.jpg"));
        std::fs::write(&a, b"AAAA").unwrap();
        std::fs::write(&b, b"BB").unwrap();
        let since = SystemTime::now();
        if crate::fileops::is_permanent_delete(&[a.clone()]) {
            return; // no Recycle Bin for the temp folder here
        }
        crate::fileops::recycle(&[a.clone(), b.clone()]).unwrap();
        assert!(!a.exists() && !b.exists());
        // Another a.jpg took the name meanwhile.
        std::fs::write(&a, b"new").unwrap();
        let steps = std::sync::Mutex::new(Vec::new());
        let out = restore(&[a.clone(), b.clone(), c.clone()], since, &|d: usize, t: usize| steps.lock().unwrap().push((d, t)));
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert_eq!(out.failed[0].0, c);
        assert_eq!(out.done, vec![(a.clone(), dir.join("a (2).jpg")), (b.clone(), b.clone())]);
        assert_eq!(std::fs::read(dir.join("a (2).jpg")).unwrap(), b"AAAA");
        assert_eq!(std::fs::read(&a).unwrap(), b"new");
        assert_eq!(std::fs::read(&b).unwrap(), b"BB");
        assert_eq!(steps.lock().unwrap().last(), Some(&(3, 3)));
        // A second restore finds nothing: the entries left the bin.
        let again = restore(&[b.clone()], since, &|_: usize, _: usize| {});
        assert!(again.done.is_empty() && again.failed.len() == 1);
        // The folder is gone by the time of the undo: it is made again.
        let lone = base.join("gone").join("x.jpg");
        std::fs::create_dir_all(lone.parent().unwrap()).unwrap();
        std::fs::write(&lone, b"X").unwrap();
        let since = SystemTime::now();
        crate::fileops::recycle(&[lone.clone()]).unwrap();
        std::fs::remove_dir(lone.parent().unwrap()).unwrap();
        let out = restore(&[lone.clone()], since, &|_: usize, _: usize| {});
        assert_eq!(out.done, vec![(lone.clone(), lone.clone())], "{:?}", out.failed);
        assert_eq!(std::fs::read(&lone).unwrap(), b"X");
        let _ = std::fs::remove_dir_all(&base);
    }
}
