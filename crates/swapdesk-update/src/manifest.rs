//! The release manifest and its signature (`docs/UPDATES.md` 4.2 and 4.3).
//!
//! ```text
//! swapdesk-update 1
//! version 1.1.1
//! channel stable
//! apply auto
//! file windows-x64 Swapdesk-windows-x64-1.1.1.sdpkg 12401664 <64 hex>
//! file macos-arm64 Swapdesk-macos-arm64-1.1.1.sdpkg 13107200 <64 hex>
//! ```
//!
//! ASCII, LF, at most [`MAX_MANIFEST`] bytes, one `key value` per line, every key exactly once except `file` (one per
//! system), unknown keys refused. The signature file is `swapdesk-update-sig 1`, `key <16 hex>`, `sig <128 hex>`, at
//! most [`MAX_SIGNATURE`] bytes; the signed message is [`DOMAIN`] followed by the manifest's exact bytes. The
//! signature is checked before a byte of the manifest is parsed.

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use crate::keys::{PublicKey, RELEASE_KEYS, key_id};
use crate::{UpdateError, Version, hex, unhex};

pub const MAX_MANIFEST: usize = 4096;
pub const MAX_SIGNATURE: usize = 256;
/// The largest package a manifest may name (GUESS: 20 times the 1.1 package).
pub const MAX_PACKAGE: u64 = 256 * 1024 * 1024;
/// What precedes the manifest in the signed message, so no signature made for another purpose verifies as one.
pub const DOMAIN: &[u8] = b"swapdesk-update-manifest\n";

const FIRST: &str = "swapdesk-update 1";
const SIG_FIRST: &str = "swapdesk-update-sig 1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Preview,
}

/// `manual`: a release that needs the installer or a step by hand; the app links to the release page instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    Auto,
    Manual,
}

/// The package of one system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// `windows-x64` or `macos-arm64`.
    pub target: String,
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    pub channel: Channel,
    pub apply: Apply,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    /// The package of `target`.
    pub fn entry(&self, target: &str) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.target == target)
    }

    /// The manifest's text, as [`parse`] reads it.
    pub fn to_text(&self) -> String {
        let mut s = format!("{FIRST}\nversion {}\n", self.version);
        s += match self.channel {
            Channel::Stable => "channel stable\n",
            Channel::Preview => "channel preview\n",
        };
        s += match self.apply {
            Apply::Auto => "apply auto\n",
            Apply::Manual => "apply manual\n",
        };
        for f in &self.files {
            s += &format!("file {} {} {} {}\n", f.target, f.name, f.size, hex(&f.sha256));
        }
        s
    }
}

fn token_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn size_of(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok().filter(|n| *n > 0 && *n <= MAX_PACKAGE)
}

/// Splits ASCII text into its lines: LF only, a last LF required, no empty line.
fn lines(bytes: &[u8]) -> Result<Vec<&str>, UpdateError> {
    if !bytes.is_ascii() || bytes.contains(&b'\r') || bytes.last() != Some(&b'\n') {
        return Err(UpdateError::Malformed("not LF-terminated ASCII"));
    }
    let text = core::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| UpdateError::Malformed("text"))?;
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.iter().any(|l| l.is_empty()) {
        return Err(UpdateError::Malformed("an empty line"));
    }
    Ok(lines)
}

/// Reads a manifest's text. Call only on bytes whose signature verified ([`verify_with`] does both).
pub fn parse(bytes: &[u8]) -> Result<Manifest, UpdateError> {
    if bytes.len() > MAX_MANIFEST {
        return Err(UpdateError::TooLarge);
    }
    let lines = lines(bytes)?;
    if lines.first() != Some(&FIRST) {
        return Err(UpdateError::Malformed("first line"));
    }
    let (mut version, mut channel, mut apply) = (None, None, None);
    let mut files: Vec<FileEntry> = Vec::new();
    for line in &lines[1..] {
        let (key, value) = line.split_once(' ').ok_or(UpdateError::Malformed("a line without a value"))?;
        match key {
            "version" => {
                if version.replace(Version::parse(value)?).is_some() {
                    return Err(UpdateError::BadKey);
                }
            }
            "channel" => {
                let c = match value {
                    "stable" => Channel::Stable,
                    "preview" => Channel::Preview,
                    _ => return Err(UpdateError::Malformed("channel")),
                };
                if channel.replace(c).is_some() {
                    return Err(UpdateError::BadKey);
                }
            }
            "apply" => {
                let a = match value {
                    "auto" => Apply::Auto,
                    "manual" => Apply::Manual,
                    _ => return Err(UpdateError::Malformed("apply")),
                };
                if apply.replace(a).is_some() {
                    return Err(UpdateError::BadKey);
                }
            }
            "file" => {
                let f: Vec<&str> = value.split(' ').collect();
                let [target, name, size, hash] = f[..] else { return Err(UpdateError::Malformed("file")) };
                let (Some(size), Some(sha256)) = (size_of(size), unhex::<32>(hash)) else {
                    return Err(UpdateError::Malformed("file"));
                };
                if !token_ok(target) || !token_ok(name) {
                    return Err(UpdateError::Malformed("file"));
                }
                if files.iter().any(|e| e.target == target) {
                    return Err(UpdateError::BadKey);
                }
                files.push(FileEntry { target: target.into(), name: name.into(), size, sha256 });
            }
            _ => return Err(UpdateError::BadKey),
        }
    }
    let (Some(version), Some(channel), Some(apply)) = (version, channel, apply) else {
        return Err(UpdateError::Malformed("a missing key"));
    };
    if files.is_empty() {
        return Err(UpdateError::Malformed("no file"));
    }
    Ok(Manifest { version, channel, apply, files })
}

/// The signature file: the key id and the signature.
fn parse_signature(bytes: &[u8]) -> Result<([u8; 8], [u8; 64]), UpdateError> {
    if bytes.len() > MAX_SIGNATURE {
        return Err(UpdateError::TooLarge);
    }
    let lines = lines(bytes)?;
    let [first, key, sig] = lines[..] else { return Err(UpdateError::Malformed("signature")) };
    if first != SIG_FIRST {
        return Err(UpdateError::Malformed("signature"));
    }
    let id = key.strip_prefix("key ").and_then(unhex::<8>).ok_or(UpdateError::Malformed("signature key"))?;
    let sig = sig.strip_prefix("sig ").and_then(unhex::<64>).ok_or(UpdateError::Malformed("signature"))?;
    Ok((id, sig))
}

/// Checks `signature` over `manifest` against `keys`, then reads the manifest.
pub fn verify_with(manifest: &[u8], signature: &[u8], keys: &[PublicKey]) -> Result<Manifest, UpdateError> {
    if manifest.len() > MAX_MANIFEST {
        return Err(UpdateError::TooLarge);
    }
    let (id, sig) = parse_signature(signature)?;
    let key = keys.iter().find(|k| key_id(k) == id).ok_or(UpdateError::UnknownKey)?;
    let mut message = Vec::with_capacity(DOMAIN.len() + manifest.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(manifest);
    UnparsedPublicKey::new(&ED25519, key).verify(&message, &sig).map_err(|_| UpdateError::BadSignature)?;
    parse(manifest)
}

/// [`verify_with`] the keys of the releases.
pub fn verify(manifest: &[u8], signature: &[u8]) -> Result<Manifest, UpdateError> {
    verify_with(manifest, signature, RELEASE_KEYS)
}

/// Signs `manifest` with the PKCS#8 Ed25519 key `pkcs8`: the signature file's text (the release tool, slice U2, and
/// tests).
pub fn sign(manifest: &[u8], pkcs8: &[u8]) -> Result<String, UpdateError> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| UpdateError::Malformed("signing key"))?;
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key().as_ref());
    let mut message = Vec::with_capacity(DOMAIN.len() + manifest.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(manifest);
    let sig = pair.sign(&message);
    Ok(format!("{SIG_FIRST}\nkey {}\nsig {}\n", hex(&key_id(&public)), hex(sig.as_ref())))
}

/// The public half of the PKCS#8 Ed25519 key `pkcs8`.
pub fn public_key(pkcs8: &[u8]) -> Result<PublicKey, UpdateError> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| UpdateError::Malformed("signing key"))?;
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key().as_ref());
    Ok(public)
}
