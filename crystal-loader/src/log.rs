//! `crystal-loader.log` next to the executable: stages and errors only. Never
//! pass session values, pipe payloads or anything derived from them to `log!`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static SINK: Mutex<Option<File>> = Mutex::new(None);

const MAX_LOG_BYTES: u64 = 1 << 20;
pub const LOG_NAME: &str = "crystal-loader.log";

/// Opens the log next to the executable (falls back to %TEMP% if that folder is
/// read-only). Returns the path used.
pub fn init() -> Option<PathBuf> {
    let beside_exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(LOG_NAME)));
    let candidates = beside_exe.into_iter().chain(std::iter::once(std::env::temp_dir().join(LOG_NAME)));
    for path in candidates {
        if let Some(f) = open(&path) {
            *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
            return Some(path);
        }
    }
    None
}

fn open(path: &Path) -> Option<File> {
    // Keep one previous generation instead of growing without bound.
    if std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

pub fn write(msg: std::fmt::Arguments<'_>) {
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = guard.as_mut() else { return };
    // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    // Format first so one line uses one append write. Formatting directly into
    // File performs many writes, interleaving lines from simultaneous clients.
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{}] {}\n",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        t.wMilliseconds,
        std::process::id(),
        msg
    );
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(format_args!($($t)*)) };
}
