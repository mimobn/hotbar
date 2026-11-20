//! Tiny logger: stderr, plus a log file when running as the daemon.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Start writing to `<data dir>/hotbar.log` (truncated when over 1 MiB).
pub fn to_file() {
    let dir = crate::config::data_dir();
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("hotbar.log");
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > 1 << 20)
        .unwrap_or(false);
    let f = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(path);
    if let Ok(f) = f {
        *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
}

pub fn write(msg: &str) {
    let secs = crate::store::now_secs();
    let line = format!("[{secs}] {msg}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(f) = FILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::logging::write(&format!($($t)*)) };
}
