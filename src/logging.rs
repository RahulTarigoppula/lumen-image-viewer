//! Diagnostics written next to the settings (`settings::data_dir()`):
//! - `lumen.log`: warnings and errors from Lumen and its libraries (eframe,
//!   egui, winit, glow). Rotated to `lumen.old.log` at about 1 MB. Set
//!   `LUMEN_LOG=info` (or `debug`) for more detail.
//! - `crash.log`: panics on the UI thread (with a backtrace) and native
//!   crashes (exception code, address, module). Appended, kept under 512 KB.
//!
//! Panics on worker threads are caught by the pools (a bad file shows an
//! error), so they go to `lumen.log` as warnings instead of `crash.log`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// `lumen.log` is moved to `lumen.old.log` when it grows past this.
pub const LOG_LIMIT: u64 = 1024 * 1024;
/// `crash.log` is moved to `crash.old.log` when it grows past this.
const CRASH_LIMIT: u64 = 512 * 1024;

struct Sink {
    path: PathBuf,
    old: PathBuf,
    limit: u64,
    file: Option<File>,
    size: u64,
    /// The last line written and how often it repeated since (a warning
    /// raised every frame must not fill the disk or stall the UI on IO).
    last: String,
    repeats: u64,
}

impl Sink {
    fn new(path: PathBuf, old: PathBuf, limit: u64) -> Self {
        Sink { path, old, limit, file: None, size: 0, last: String::new(), repeats: 0 }
    }

    fn open(&mut self) -> Option<&mut File> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let f = OpenOptions::new().create(true).append(true).open(&self.path).ok()?;
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
        }
        self.file.as_mut()
    }

    fn rotate_if_needed(&mut self) {
        if self.size < self.limit {
            return;
        }
        self.file = None;
        let _ = std::fs::remove_file(&self.old);
        let _ = std::fs::rename(&self.path, &self.old);
        self.size = 0;
    }

    fn write_raw(&mut self, text: &str) {
        if self.open().is_none() {
            return;
        }
        self.rotate_if_needed();
        if let Some(f) = self.open() {
            if f.write_all(text.as_bytes()).is_ok() {
                let _ = f.flush();
                self.size += text.len() as u64;
            }
        }
    }

    /// Write one message; `key` identifies it for collapsing repeats.
    fn write(&mut self, key: &str, line: &str) {
        if key == self.last {
            self.repeats += 1;
            // Note a flood now and then instead of every time.
            if self.repeats.is_power_of_two() && self.repeats >= 16 {
                let note = format!("{} ... repeated {} times\n", stamp(), self.repeats);
                self.write_raw(&note);
            }
            return;
        }
        if self.repeats > 0 {
            let note = format!("{} ... last message repeated {} times\n", stamp(), self.repeats);
            self.write_raw(&note);
        }
        self.last = key.to_string();
        self.repeats = 0;
        self.write_raw(line);
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn thread_name() -> String {
    let t = std::thread::current();
    t.name().map(str::to_string).unwrap_or_else(|| format!("{:?}", t.id()))
}

struct Logger {
    level: log::LevelFilter,
    sink: Mutex<Sink>,
}

impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let msg = record.args().to_string();
        let line = format!("{} {:<5} [{}] ({}) {msg}\n", stamp(), record.level(), record.target(), thread_name());
        let key = format!("{}|{}|{msg}", record.level(), record.target());
        // A poisoned lock (a panic while logging) just drops the line.
        if let Ok(mut s) = self.sink.lock() {
            s.write(&key, &line);
        }
    }

    fn flush(&self) {
        if let Ok(mut s) = self.sink.lock() {
            if let Some(f) = s.file.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

fn level_from_env() -> log::LevelFilter {
    match std::env::var("LUMEN_LOG").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("error") => log::LevelFilter::Error,
        Ok("info") => log::LevelFilter::Info,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        Ok("off") => log::LevelFilter::Off,
        _ => log::LevelFilter::Warn,
    }
}

static CRASH: Mutex<Option<Sink>> = Mutex::new(None);

/// Install the file logger, the panic hook and the native crash handler.
/// Call once, first thing in `main`.
pub fn init() {
    let dir = crate::settings::data_dir();
    let level = level_from_env();
    let logger = Logger { level, sink: Mutex::new(Sink::new(dir.join("lumen.log"), dir.join("lumen.old.log"), LOG_LIMIT)) };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    if let Ok(mut c) = CRASH.lock() {
        *c = Some(Sink::new(dir.join("crash.log"), dir.join("crash.old.log"), CRASH_LIMIT));
    }
    std::panic::set_hook(Box::new(panic_hook));
    #[cfg(windows)]
    native::install(&dir.join("crash.log"));
}

fn panic_hook(info: &std::panic::PanicHookInfo) {
    let thread = thread_name();
    let ui_thread = std::thread::current().name() == Some("main");
    if ui_thread {
        // The UI thread is not caught anywhere: this ends the app.
        let msg = format!("{} panic on thread '{thread}' (Lumen {}): {info}\n{}\n\n", stamp(), env!("CARGO_PKG_VERSION"), std::backtrace::Backtrace::force_capture());
        write_crash(&msg);
        log::error!("panic on the UI thread: {info}");
        eprintln!("{msg}");
    } else {
        // Workers run under catch_unwind; the pool reports the error.
        log::warn!("panic on thread '{thread}' (caught): {info}");
        eprintln!("panic on thread '{thread}': {info}");
    }
}

/// Append to `crash.log`.
pub fn write_crash(text: &str) {
    if let Ok(mut c) = CRASH.lock() {
        if let Some(s) = c.as_mut() {
            s.write_raw(text);
        }
    }
}

/// Paths of the log files, for the README and the About box.
#[allow(dead_code)]
pub fn log_path() -> PathBuf {
    crate::settings::data_dir().join("lumen.log")
}

#[cfg(windows)]
mod native {
    //! Native crashes (access violations in a driver, stack overflow, ...)
    //! never reach the panic hook. The top-level exception filter writes one
    //! line to `crash.log` and lets Windows continue (Windows Error Reporting
    //! still sees the crash). It formats into a stack buffer and calls only
    //! kernel32: the heap may be what is broken.

    use std::fmt::Write as _;
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HMODULE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, WriteFile, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_ALWAYS};
    use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS};
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};
    use windows::Win32::System::SystemInformation::GetLocalTime;

    static PATH: OnceLock<Vec<u16>> = OnceLock::new();

    pub fn install(path: &std::path::Path) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let _ = PATH.set(wide);
        // SAFETY: registers a function pointer; no other state.
        unsafe {
            SetUnhandledExceptionFilter(Some(filter));
        }
    }

    /// Fixed-size text buffer; overlong text is cut.
    pub(super) struct StackBuf {
        pub buf: [u8; 768],
        pub len: usize,
    }

    impl std::fmt::Write for StackBuf {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let n = s.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }

    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        let Some(path) = PATH.get() else { return EXCEPTION_CONTINUE_SEARCH };
        let mut out = StackBuf { buf: [0; 768], len: 0 };
        // SAFETY: the system passes valid exception pointers to the filter.
        let (code, addr) = unsafe {
            match info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
                Some(r) => (r.ExceptionCode.0 as u32, r.ExceptionAddress as usize),
                None => (0, 0),
            }
        };
        let t = unsafe { GetLocalTime() };
        let _ = write!(
            out,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} native crash (Lumen {}): exception 0x{code:08X} at 0x{addr:X}",
            t.wYear,
            t.wMonth,
            t.wDay,
            t.wHour,
            t.wMinute,
            t.wSecond,
            t.wMilliseconds,
            env!("CARGO_PKG_VERSION")
        );
        let mut module = HMODULE::default();
        // SAFETY: FROM_ADDRESS treats the "name" as an address inside a module.
        let found = unsafe {
            GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, PCWSTR(addr as *const u16), &mut module)
        };
        if found.is_ok() && !module.is_invalid() {
            let mut name = [0u16; 260];
            let n = unsafe { GetModuleFileNameW(Some(module), &mut name) } as usize;
            let name = &name[..n.min(260)];
            let file = name.rsplit(|&c| c == b'\\' as u16).next().unwrap_or(name);
            let _ = out.write_str(" in ");
            for c in char::decode_utf16(file.iter().copied()) {
                let _ = out.write_char(c.unwrap_or('?'));
            }
            let _ = write!(out, "+0x{:X}", addr.wrapping_sub(module.0 as usize));
        }
        let _ = out.write_str("\r\n\r\n");
        // SAFETY: plain file append with a NUL-terminated path.
        unsafe {
            if let Ok(h) = CreateFileW(PCWSTR(path.as_ptr()), FILE_APPEND_DATA.0, FILE_SHARE_READ, None, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, None) {
                let _ = WriteFile(h, Some(&out.buf[..out.len]), None, None);
                let _ = CloseHandle(h);
            }
        }
        EXCEPTION_CONTINUE_SEARCH
    }
}

#[cfg(test)]
pub(crate) fn test_sink(path: &std::path::Path, limit: u64) -> impl FnMut(&str, &str) {
    let mut s = Sink::new(path.to_path_buf(), path.with_extension("old.log"), limit);
    move |key, line| s.write(key, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lumen-log-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rotates_at_the_limit() {
        let d = temp("rotate");
        let path = d.join("lumen.log");
        let mut w = test_sink(&path, 100);
        for i in 0..30 {
            w(&format!("k{i}"), &format!("line number {i:04}\n"));
        }
        let cur = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(d.join("lumen.old.log")).unwrap().len();
        assert!(cur <= 100 + 20, "current log stays near the limit: {cur}");
        assert!(old >= 100 && old <= 120, "old log holds the previous chunk: {old}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("line number 0029\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn collapses_repeats() {
        let d = temp("repeat");
        let path = d.join("lumen.log");
        let mut w = test_sink(&path, 1 << 20);
        for _ in 0..1000 {
            w("same", "GL error\n");
        }
        w("other", "done\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("GL error").count(), 1);
        assert!(text.lines().count() < 12, "{text}");
        assert!(text.contains("repeated 999 times"));
        assert!(text.ends_with("done\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn stack_buffer_cuts_long_text() {
        use std::fmt::Write as _;
        let mut b = native::StackBuf { buf: [0; 768], len: 0 };
        for _ in 0..100 {
            let _ = b.write_str("0123456789");
        }
        assert_eq!(b.len, 768);
    }
}
