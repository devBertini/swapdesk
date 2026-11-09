use std::sync::{Arc, Mutex};

use swapdesk_platform::{Level, TraceSink};

use crate::lock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedLine {
    pub level: Level,
    pub target: &'static str,
    pub text: String,
}

/// A never-logged value found in a line. Names the forbidden value and the form it appeared in, never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub line_index: usize,
    pub name: String,
    pub form: &'static str,
}

struct Forbidden {
    name: String,
    /// (form, bytes to look for)
    patterns: Vec<(&'static str, Vec<u8>)>,
}

#[derive(Default)]
struct Inner {
    lines: Vec<CapturedLine>,
    forbidden: Vec<Forbidden>,
}

/// Captures every log line and checks them against the never-logged values of C2.3 (F-SEC-01). A forbidden value is
/// looked for as raw bytes, lowercase and uppercase hex, and base64 (standard and URL alphabet, unpadded).
/// Clones share the capture.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Inner>>);

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn hex(bytes: &[u8], table: &[u8; 16]) -> Vec<u8> {
    bytes.iter().flat_map(|b| [table[usize::from(b >> 4)], table[usize::from(b & 15)]]).collect()
}

fn base64_unpadded(bytes: &[u8], alphabet: &[u8; 64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 4 / 3 + 2);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize]);
        }
    }
    out
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a never-logged value under a name (the name is what a failure prints).
    /// Panics when the value is shorter than 4 bytes: it would match ordinary text.
    pub fn forbid(&self, name: &str, value: impl AsRef<[u8]>) {
        let v = value.as_ref();
        assert!(v.len() >= 4, "forbidden value {name} is too short to check");
        let patterns = vec![
            ("raw", v.to_vec()),
            ("hex", hex(v, HEX_LOWER)),
            ("HEX", hex(v, HEX_UPPER)),
            ("base64", base64_unpadded(v, B64_STD)),
            ("base64url", base64_unpadded(v, B64_URL)),
        ];
        lock(&self.0).forbidden.push(Forbidden { name: name.to_string(), patterns });
    }

    pub fn lines(&self) -> Vec<CapturedLine> {
        lock(&self.0).lines.clone()
    }

    pub fn violations(&self) -> Vec<Violation> {
        let inner = lock(&self.0);
        let mut found = Vec::new();
        for (line_index, line) in inner.lines.iter().enumerate() {
            let text = line.text.as_bytes();
            for f in &inner.forbidden {
                if let Some((form, _)) = f.patterns.iter().find(|(_, p)| contains(text, p)) {
                    found.push(Violation { line_index, name: f.name.clone(), form });
                }
            }
        }
        found
    }

    /// Fails when any captured line holds a forbidden value.
    pub fn assert_clean(&self) {
        let v = self.violations();
        assert!(v.is_empty(), "never-logged values in the log: {v:?}");
    }
}

impl TraceSink for LogCapture {
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
        lock(&self.0).lines.push(CapturedLine { level, target, text: what.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0xA7; 32];
    const NONCE: [u8; 16] =
        [0x3C, 0x91, 0xD2, 0x05, 0x7E, 0xB8, 0x44, 0x19, 0xF0, 0x62, 0x8A, 0xCD, 0x21, 0x56, 0x0B, 0xE3];
    const EXPORTER: [u8; 32] = [0x5D; 32];
    const TOKEN: [u8; 16] =
        [0xFB, 0xEF, 0xBE, 0x42, 0x9A, 0x31, 0xC8, 0x6F, 0x04, 0xB5, 0x2D, 0x93, 0x18, 0xAA, 0x6C, 0xF7];

    fn forbid_all(cap: &LogCapture) {
        cap.forbid("private key", KEY);
        cap.forbid("pairing nonce", NONCE);
        cap.forbid("exporter output", EXPORTER);
        cap.forbid("sas", "483920");
        cap.forbid("resume token", TOKEN);
        cap.forbid("terminal bytes", "ls -la /root/secret-dir");
        cap.forbid("text input", "hunter2-typed");
        cap.forbid("clipboard", "clip-content-xyz");
        cap.forbid("chat text", "oi, segredo do chat");
        cap.forbid("file contents", "linha privada do arquivo");
    }

    /// The scripted pairing, session, terminal, transfer and chat run: what the code is allowed to say.
    fn scripted_run(sink: &dyn TraceSink) {
        sink.log(
            Level::Info,
            "pair",
            format_args!("pairing started with device {}", "0123456789abcdef0123456789abcdef"),
        );
        sink.log(Level::Info, "pair", format_args!("sas confirmed by both sides, {} digits", 6));
        sink.log(
            Level::Info,
            "session",
            format_args!("session opened, intent={:?}, grant={:?}", "Control", "Prompted"),
        );
        sink.log(Level::Debug, "term", format_args!("terminal spawned {}x{} as {}", 120, 30, "alex"));
        sink.log(Level::Debug, "term", format_args!("terminal output {} bytes", 4096));
        sink.log(Level::Info, "files", format_args!("transfer {} of {} bytes done, sha256 ok", 1, 2_048));
        sink.log(Level::Info, "chat", format_args!("chat message received, {} bytes", 19));
        sink.log(Level::Warn, "clip", format_args!("clipboard item skipped: Concealed"));
    }

    #[test]
    fn f_sec_01_a_clean_scripted_run_passes_and_every_leak_form_fails() {
        let cap = LogCapture::new();
        forbid_all(&cap);
        scripted_run(&cap);
        assert_eq!(cap.lines().len(), 8);
        cap.assert_clean();

        let leaks: Vec<(&str, String, &str)> = vec![
            ("private key", format!("loaded key {}", String::from_utf8(hex(&KEY, HEX_LOWER)).unwrap()), "hex"),
            ("pairing nonce", format!("nonce {}", String::from_utf8(hex(&NONCE, HEX_UPPER)).unwrap()), "HEX"),
            (
                "exporter output",
                format!("exporter {}", String::from_utf8(base64_unpadded(&EXPORTER, B64_STD)).unwrap()),
                "base64",
            ),
            (
                "resume token",
                format!("token {}", String::from_utf8(base64_unpadded(&TOKEN, B64_URL)).unwrap()),
                "base64url",
            ),
            ("sas", "sas shown 483920 to user".to_string(), "raw"),
            ("terminal bytes", "pty wrote: ls -la /root/secret-dir".to_string(), "raw"),
            ("text input", "typed hunter2-typed".to_string(), "raw"),
            ("clipboard", "clip: clip-content-xyz".to_string(), "raw"),
            ("chat text", "chat: oi, segredo do chat".to_string(), "raw"),
            ("file contents", "chunk linha privada do arquivo".to_string(), "raw"),
        ];
        for (name, line, form) in leaks {
            let cap = LogCapture::new();
            forbid_all(&cap);
            scripted_run(&cap);
            cap.log(Level::Error, "leak", format_args!("{line}"));
            let v = cap.violations();
            assert_eq!(v, vec![Violation { line_index: 8, name: name.to_string(), form }], "{name}");
            assert!(!format!("{v:?}").contains("483920"));
        }
    }

    #[test]
    fn captures_level_and_target() {
        let cap = LogCapture::new();
        cap.log(Level::Warn, "x", format_args!("a {}", 1));
        assert_eq!(cap.lines(), vec![CapturedLine { level: Level::Warn, target: "x", text: "a 1".to_string() }]);
    }

    #[test]
    #[should_panic(expected = "too short")]
    fn short_values_are_refused() {
        LogCapture::new().forbid("x", "ab");
    }
}
