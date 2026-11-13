//! File-name rules shared by both sides (PROTOCOL.md 5.3, "Names on both sides"), plus the guards this crate adds
//! because Windows reads those names as another path.

use swapdesk_proto::ErrorCode;

/// Suffixes of the files a receiver keeps while a transfer runs. A peer may not offer a name that ends in one.
pub const PART_SUFFIX: &str = ".swapdesk-part";
pub const META_SUFFIX: &str = ".swapdesk-meta";

const RESERVED: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// True for a Windows reserved device name, with or without an extension ("nul.txt" is the device).
pub fn is_reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// The invisible bidirectional controls that reorder what a person reads ("photo" + U+202E + "gpj.exe" shows as
/// "photoexe.jpg"): embeddings and overrides U+202A-202E, isolates U+2066-2069, the marks U+200E and U+200F and the
/// Arabic letter mark U+061C. They are not in the `Cc` class, so `char::is_control` does not see them.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{61c}')
}

/// One path component the receiver may create: not empty, not `.` or `..`, no `/`, `\`, `:` (which also closes the
/// alternate data stream `name:stream`), control or bidi control character, not a reserved device name, no leading space or
/// trailing dot or space (Windows drops them, so "con." would be the device), and not one of this crate's own
/// partial-file names.
pub fn validate_name(name: &str) -> Result<(), ErrorCode> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(ErrorCode::InvalidName);
    }
    if name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control() || is_bidi_control(c)) {
        return Err(ErrorCode::InvalidName);
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(ErrorCode::InvalidName);
    }
    if is_reserved_device(name) {
        return Err(ErrorCode::InvalidName);
    }
    let lower = name.to_lowercase();
    if lower.ends_with(PART_SUFFIX) || lower.ends_with(META_SUFFIX) {
        return Err(ErrorCode::InvalidName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_names_pass() {
        for n in ["a.txt", "relatório final.pdf", "x", ".gitignore", "a b", "file.tar.gz", "COM10", "console.log"] {
            assert_eq!(validate_name(n), Ok(()), "{n}");
        }
    }

    #[test]
    fn bad_names_are_refused() {
        let bad = [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../x",
            "..\\x",
            "C:\\x",
            "C:x",
            "/etc/passwd",
            "name:stream",
            "a:$DATA",
            "nul",
            "NUL",
            "nul.txt",
            "Con.tar.gz",
            "com1",
            "LPT9.x",
            "aux ",
            "con.",
            "name.",
            "name ",
            " name",
            "a\0b",
            "a\nb",
            "a\u{7f}b",
            "x.swapdesk-part",
            "X.SWAPDESK-META",
            "COM\u{b9}",
        ];
        for n in bad {
            assert_eq!(validate_name(n), Err(ErrorCode::InvalidName), "{n:?}");
        }
    }

    #[test]
    fn bidi_and_format_controls_are_refused() {
        // The usual spoofing set: embeddings and overrides (U+202A-202E), isolates (U+2066-2069), the marks U+200E and
        // U+200F and the Arabic letter mark U+061C. They are not `Cc`, so `is_control` alone lets them through.
        for c in [
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
            '\u{200e}', '\u{200f}', '\u{61c}',
        ] {
            for n in [format!("photo{c}gpj.exe"), format!("{c}x"), format!("x{c}")] {
                assert_eq!(validate_name(&n), Err(ErrorCode::InvalidName), "{n:?}");
            }
        }
        // Ordinary right-to-left names stay valid: only the invisible controls are refused.
        assert_eq!(validate_name("\u{5e9}\u{5dc}\u{5d5}\u{5dd}.txt"), Ok(()));
        assert_eq!(validate_name("\u{645}\u{631}\u{62d}\u{628}\u{627}.txt"), Ok(()));
    }
}
