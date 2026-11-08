//! What the viewer sends to the host's PTY for a key or a paste (xterm conventions).

/// A key as the shell reports it after layout and IME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    BackTab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { shift: false, ctrl: false, alt: false };

    /// The xterm modifier parameter (1 = none).
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self.shift || self.ctrl || self.alt
    }
}

/// The bytes for `key`; `app_cursor` is [`Modes::app_cursor`](crate::Modes). Empty when the key sends nothing.
pub fn encode_key(key: Key, mods: Mods, app_cursor: bool) -> Vec<u8> {
    let csi_letter = |letter: char| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[1;{}{letter}", mods.param()).into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods.any() { format!("\x1b[{n};{}~", mods.param()).into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    match key {
        Key::Char(c) => {
            let mut out = Vec::new();
            if mods.alt {
                out.push(0x1b);
            }
            if mods.ctrl {
                match ctrl_byte(c) {
                    Some(b) => out.push(b),
                    None => return Vec::new(),
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        Key::Enter => alt_prefix(mods, b"\r"),
        Key::Backspace => alt_prefix(mods, if mods.ctrl { b"\x08" } else { b"\x7f" }),
        Key::Tab => alt_prefix(mods, b"\t"),
        Key::BackTab => b"\x1b[Z".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => csi_letter('A'),
        Key::Down => csi_letter('B'),
        Key::Right => csi_letter('C'),
        Key::Left => csi_letter('D'),
        Key::Home => csi_letter('H'),
        Key::End => csi_letter('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F(n @ 1..=4) => {
            let letter = (b'O' + n) as char;
            if mods.any() {
                format!("\x1b[1;{}{letter}", mods.param()).into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            }
        }
        Key::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]),
        Key::F(_) => Vec::new(),
    }
}

fn alt_prefix(mods: Mods, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// A paste: the text with escape characters removed (so it cannot end the bracket early or inject a sequence),
/// newlines as carriage returns, wrapped in `ESC [ 200 ~` ... `ESC [ 201 ~` when the program asked for it.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(b'\r');
            }
            '\n' => out.push(b'\r'),
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}
