//! One JSON line per check, on stdout and appended to a log file (macos.md 12a). No serde: JSON is built by hand.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A JSON value for the `detail` object.
pub enum V {
    S(String),
    I(i64),
    F(f64),
    B(bool),
}

impl From<&str> for V {
    fn from(s: &str) -> Self {
        V::S(s.to_string())
    }
}
impl From<String> for V {
    fn from(s: String) -> Self {
        V::S(s)
    }
}
impl From<bool> for V {
    fn from(b: bool) -> Self {
        V::B(b)
    }
}
impl From<i64> for V {
    fn from(i: i64) -> Self {
        V::I(i)
    }
}
impl From<u64> for V {
    fn from(i: u64) -> Self {
        V::I(i as i64)
    }
}
impl From<u32> for V {
    fn from(i: u32) -> Self {
        V::I(i64::from(i))
    }
}
impl From<usize> for V {
    fn from(i: usize) -> Self {
        V::I(i as i64)
    }
}
impl From<f64> for V {
    fn from(f: f64) -> Self {
        V::F(f)
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn object(fields: Vec<(&str, V)>) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in fields.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&escape(k));
        out.push(':');
        match v {
            V::S(s) => out.push_str(&escape(&s)),
            V::I(n) => {
                let _ = write!(out, "{n}");
            }
            V::F(f) if f.is_finite() => {
                let _ = write!(out, "{f}");
            }
            V::F(_) => out.push_str("null"),
            V::B(b) => out.push_str(if b { "true" } else { "false" }),
        }
    }
    out.push('}');
    out
}

/// `d!("key" => value, ...)` builds a detail object string.
#[macro_export]
macro_rules! d {
    () => { String::from("{}") };
    ($($k:literal => $v:expr),+ $(,)?) => {
        $crate::log::object(vec![$(($k, $crate::log::V::from($v))),+])
    };
}

pub struct Ctx {
    pub log_path: PathBuf,
    pub name: String,
    pub signer: String,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static SESSION: OnceLock<String> = OnceLock::new();

pub const NONCE: &str = match option_env!("PROBE_NONCE") {
    Some(n) => n,
    None => "0",
};

/// The log file for a run: `--log`, else `<logdir>/<name>.log` (root: `/Library/Logs/SwapdeskProbe`).
pub fn log_dir() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc_geteuid() };
        if euid == 0 {
            return PathBuf::from("/Library/Logs/SwapdeskProbe");
        }
    }
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join("Library/Logs/SwapdeskProbe"),
        None => PathBuf::from("."),
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

pub fn uid() -> (i64, i64) {
    #[cfg(unix)]
    {
        // SAFETY: getuid and geteuid have no preconditions.
        unsafe { (i64::from(libc_getuid()), i64::from(libc_geteuid())) }
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

pub fn init(log: Option<String>, name: String) {
    let path = match log {
        Some(p) => PathBuf::from(p),
        None => log_dir().join(format!("{name}.log")),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = CTX.set(Ctx { log_path: path, name, signer: read_signer() });
}

pub fn ctx() -> Option<&'static Ctx> {
    CTX.get()
}

/// `make-app.sh` writes the signer next to each executable's bundle: `Contents/Resources/signer`.
fn read_signer() -> String {
    let Ok(exe) = std::env::current_exe() else { return "unknown".into() };
    let Some(macos_dir) = exe.parent() else { return "unknown".into() };
    let file = macos_dir.join("../Resources/signer");
    std::fs::read_to_string(file).map(|s| s.trim().to_string()).unwrap_or_else(|_| "unknown".into())
}

fn session() -> &'static str {
    SESSION.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            // `launchctl managername` prints Aqua, LoginWindow, System, Background...
            if let Ok(out) = std::process::Command::new("/bin/launchctl").arg("managername").output() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        "n/a".to_string()
    })
}

pub fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Emit one check line. `detail` is a JSON object string (use `d!`).
pub fn emit(check: &str, ok: bool, detail: &str) {
    let (uid, _) = uid();
    let signer = ctx().map_or("unknown", |c| c.signer.as_str());
    let line = format!(
        "{{\"t\":\"{}\",\"check\":{},\"ok\":{},\"detail\":{},\"uid\":{},\"session\":{},\"build\":{},\"signer\":{}}}",
        now_iso(),
        escape(check),
        ok,
        detail,
        uid,
        escape(session()),
        escape(NONCE),
        escape(signer),
    );
    println!("{line}");
    if let Some(c) = ctx() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&c.log_path) {
            let _ = writeln!(f, "{line}");
        }
    }
}
