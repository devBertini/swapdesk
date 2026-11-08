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

/// One path component the receiver may create: not empty, not `.` or `..`, no `/`, `\`, `:` (which also closes the
/// alternate data stream `name:stream`) or control character, not a reserved device name, no leading space or
/// trailing dot or space (Windows drops them, so "con." would be the device), and not one of this crate's own
/// partial-file names.
pub fn validate_name(name: &str) -> Result<(), ErrorCode> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(ErrorCode::InvalidName);
    }
    if name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control()) {
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
}
