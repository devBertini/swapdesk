//! The app's trace log (C2.3): `trace.log` in the C5 folder (`~/Library/Application Support/Swapdesk`), one line per
//! record ("<unix ms> <level> <target> <text>"), rotated to `trace.log.1` at a size cap. Never content: callers follow
//! the never-logged list of C2.3. The Windows counterpart is `swapdesk-sys-win`'s `FileTrace`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use swapdesk_platform::{Level, TraceSink};

/// Rotate when the file would pass this size.
pub const DEFAULT_CAP: u64 = 2 * 1024 * 1024;

struct Inner {
    file: Option<File>,
    size: u64,
}

pub struct FileTrace {
    path: PathBuf,
    rotated: PathBuf,
    cap: u64,
    max_level: Level,
    inner: Mutex<Inner>,
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
    /// Opens (appending) `<dir>/<file_name>`; records above `max_level` are dropped.
    pub fn open(dir: &Path, file_name: &str, max_level: Level, cap_bytes: u64) -> std::io::Result<Self> {
        let path = dir.join(file_name);
        let rotated = dir.join(format!("{file_name}.1"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
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
            inner.file = None;
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
        self.write_line(format!("{ms} {} {target} {text}\n", label(level)).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swapdesk-app-mac-trace-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lines_are_written_filtered_by_level_and_rotated_at_the_cap() {
        let dir = temp("a");
        let t = FileTrace::open(&dir, "trace.log", Level::Info, 200).unwrap();
        t.log(Level::Info, "core", format_args!("started\nsecond line"));
        t.log(Level::Debug, "core", format_args!("dropped"));
        let text = fs::read_to_string(t.path()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(" INFO core started second line"), "{text}");
        for i in 0..10 {
            t.log(Level::Warn, "core", format_args!("filler record number {i} to pass the cap"));
        }
        assert!(dir.join("trace.log.1").exists(), "rotated");
        assert!(fs::metadata(t.path()).unwrap().len() <= 200 + 100);
        let _ = fs::remove_dir_all(&dir);
    }
}
