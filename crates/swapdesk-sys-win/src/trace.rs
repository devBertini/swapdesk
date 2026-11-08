//! `TraceSink` (C3) writing to a file: Swapdesk's own logger (D14), no logging crate.
//!
//! One line per record: `<unix ms> <LEVEL> <target> <text>`; line breaks inside the text become spaces so a record
//! never spans lines. A record above the configured level is dropped. When the file would pass the size cap it is
//! renamed to `<name>.1` (replacing the previous `.1`) and a fresh file starts: at most one old file is kept.
//! Thread-safe (one mutex around the file). The file's DACL is the folder's (C2.3: the C5 folder's); a write error is
//! swallowed because a logger must not take the service down, and the next record retries.
//!
//! The caller guarantees `what` holds no never-logged value (C2.3); this type does not inspect it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use swapdesk_platform::{ErrorKind, Level, PlatformError, Result, TraceSink};

/// Default size cap: 2 MiB.
pub const DEFAULT_CAP_BYTES: u64 = 2 * 1024 * 1024;

struct Inner {
    file: Option<File>,
    size: u64,
}

/// Appends trace lines to `<dir>/<file_name>`.
pub struct FileTrace {
    path: PathBuf,
    rotated: PathBuf,
    cap: u64,
    max_level: Level,
    inner: Mutex<Inner>,
}

fn io_err(op: &'static str, e: std::io::Error) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: e.raw_os_error().map(i64::from), detail: e.to_string() }
}

fn label(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    }
}

impl FileTrace {
    /// Opens (creating if needed) `<dir>/<file_name>` for append, with the default cap. `dir` must exist.
    /// Records more verbose than `max_level` are dropped (`Level::Info` keeps Error, Warn and Info).
    pub fn open(dir: &Path, file_name: &str, max_level: Level) -> Result<Self> {
        Self::open_with_cap(dir, file_name, max_level, DEFAULT_CAP_BYTES)
    }

    /// As [`open`](Self::open) with another size cap (tests use a small one).
    pub fn open_with_cap(dir: &Path, file_name: &str, max_level: Level, cap_bytes: u64) -> Result<Self> {
        let plain =
            !file_name.is_empty() && !file_name.contains(['/', '\\', ':']) && file_name != "." && file_name != "..";
        if !plain {
            return Err(PlatformError {
                kind: ErrorKind::InvalidInput,
                op: "FileTrace::open",
                os_code: None,
                detail: "file_name must be one plain file name".into(),
            });
        }
        let path = dir.join(file_name);
        let rotated = dir.join(format!("{file_name}.1"));
        let file =
            OpenOptions::new().create(true).append(true).open(&path).map_err(|e| io_err("FileTrace::open", e))?;
        let size = file.metadata().map_err(|e| io_err("FileTrace::open", e))?.len();
        Ok(Self {
            path,
            rotated,
            cap: cap_bytes.max(1),
            max_level,
            inner: Mutex::new(Inner { file: Some(file), size }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_line(&self, line: &[u8]) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.size > 0 && inner.size + line.len() as u64 > self.cap {
            // Close first: Windows cannot rename a file this process still holds open without share-delete.
            inner.file = None;
            // `rename` replaces an existing `.1` on Windows.
            let _ = fs::rename(&self.path, &self.rotated);
            inner.size = 0;
        }
        if inner.file.is_none() {
            inner.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
            inner.size = inner.file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
        }
        if let Some(f) = inner.file.as_mut() {
            if f.write_all(line).is_ok() {
                inner.size += line.len() as u64;
            } else {
                // Reopen on the next record.
                inner.file = None;
            }
        }
    }
}

impl TraceSink for FileTrace {
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
        if level > self.max_level {
            return;
        }
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let text = what.to_string().replace(['\r', '\n'], " ");
        let line = format!("{ms} {} {target} {text}\n", label(level));
        self.write_line(line.as_bytes());
    }
}
