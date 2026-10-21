//! The program's log: one line per event, UTC times, rotated at 1 MB (one old file kept).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn init(path: PathBuf) {
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

pub fn write(msg: &str) {
    let guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    append(guard.as_ref(), msg);
}

/// As `write`, but gives up at once if another thread is writing (it may be the hung one).
pub fn try_write(msg: &str) {
    if let Ok(guard) = FILE.try_lock() {
        append(guard.as_ref(), msg);
    }
}

fn append(path: Option<&PathBuf>, msg: &str) {
    let Some(path) = path else { return };
    if fs::metadata(path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            f,
            "{} UTC {msg}",
            crate::units::fmt_time(crate::units::now_ms())
        );
    }
}
