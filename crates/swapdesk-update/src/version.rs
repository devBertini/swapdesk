//! `MAJOR.MINOR.PATCH`, optionally `-PRE` (dot-separated ASCII letters, digits and `-`), compared as numbers:
//! `1.10.0 > 1.9.0`, and a pre-release sorts before its release (`1.1.0-rc.1 < 1.1.0`).

use core::cmp::Ordering;

use crate::UpdateError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// The pre-release part without its `-`; `None` for a release.
    pub pre: Option<String>,
}

fn number(s: &str) -> Option<u32> {
    // Decimal, no sign, no leading zero except "0" itself.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok()
}

impl Version {
    pub fn parse(s: &str) -> Result<Version, UpdateError> {
        let bad = UpdateError::Malformed("version");
        if s.len() > 64 {
            return Err(bad);
        }
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let (Some(a), Some(b), Some(c), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(bad);
        };
        let (Some(major), Some(minor), Some(patch)) = (number(a), number(b), number(c)) else { return Err(bad) };
        if let Some(p) = pre
            && (p.is_empty()
                || p.split('.').any(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')))
        {
            return Err(bad);
        }
        Ok(Version { major, minor, patch, pre: pre.map(str::to_owned) })
    }

    /// A pre-release (`-preview.1`, `-rc.2`): never offered on the stable channel.
    pub fn is_pre(&self) -> bool {
        self.pre.is_some()
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

/// Pre-release identifiers: numbers compare as numbers and sort before words; a shorter list that is a prefix of a
/// longer one sorts first.
fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut x = a.split('.');
    let mut y = b.split('.');
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let o = match (number(p), number(q)) {
                    (Some(m), Some(n)) => m.cmp(&n),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_pre(a, b),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
