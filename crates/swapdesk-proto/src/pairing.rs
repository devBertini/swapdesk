//! Pairing messages and the inputs of the commitment and the SAS (PROTOCOL.md section 3).
//!
//! This crate holds no cryptography: [`CommitmentInput`] and [`SasInput`] build the exact byte strings that the
//! caller hashes with SHA-256, and [`Sas::from_digest`] turns the digest of the second into the six digits.

use core::fmt;

use crate::codec::{Redacted, RedactedLen};
use crate::types::{ErrorCode, Os};

/// ALPN of the pairing endpoint (section 1).
pub const PAIR_ALPN: &[u8] = b"swapdesk-pair/1";
/// Prefix of the commitment preimage (step 1).
pub const COMMIT_LABEL: &[u8] = b"swapdesk commit v1";
/// Prefix of the SAS preimage (step 4).
pub const SAS_LABEL: &[u8] = b"swapdesk sas v1";
/// TLS exporter label of step 4 (RFC 5705 private use); the context is empty and the length is 32 bytes.
pub const EXPORTER_LABEL: &[u8] = b"EXPERIMENTAL swapdesk pair v1";
pub const EXPORTER_LEN: usize = 32;
/// Length of `nonce_I` and `nonce_R`.
pub const NONCE_LEN: usize = 32;
/// Longest `majors` list of `PairHello` and `PairNonce`.
pub const MAX_PAIR_MAJORS: usize = 8;

wire_struct! {
    /// Step 1, initiator to responder (0x0701). `commit = H(COMMIT_LABEL || spki_I || nonce_I)`.
    PairHello {
        name: String [64],
        os: Os,
        os_version: String [32],
        majors: Vec<u16> [MAX_PAIR_MAJORS],
        commit: [u8; 32],
    }
}

wire_struct! {
    /// Step 2, responder to initiator (0x0702).
    PairNonce {
        name: String [64],
        os: Os,
        os_version: String [32],
        majors: Vec<u16> [MAX_PAIR_MAJORS],
        nonce_r: [u8; 32] => redact,
    }
}

wire_struct! {
    /// Step 3, initiator to responder (0x0703): opens the commitment.
    PairReveal { nonce_i: [u8; 32] => redact }
}

wire_struct! {
    /// Step 5, each to the other (0x0704): the local user's verdict on the six digits.
    PairConfirm { matches: bool }
}

wire_struct! {
    /// Step 6, each to the other (0x0705).
    PairResult { ok: bool, code: ErrorCode }
}

/// Length of a device key's `SubjectPublicKeyInfo` (RFC 8410 Ed25519 form). The preimages below concatenate
/// `spki_I || spki_R` with no length prefix, which is unambiguous only while both have this exact length, so the
/// inputs take fixed-size arrays and the compiler enforces it.
pub const SPKI_LEN: usize = 44;

/// What `commit` hashes (step 1): `COMMIT_LABEL || spki_I || nonce_I`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommitmentInput<'a> {
    pub spki_i: &'a [u8; SPKI_LEN],
    pub nonce_i: &'a [u8; NONCE_LEN],
}

/// `nonce_I` stays secret until step 3: it never shows in `Debug`.
impl fmt::Debug for CommitmentInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitmentInput")
            .field("spki_i", &RedactedLen(self.spki_i.len()))
            .field("nonce_i", &Redacted)
            .finish()
    }
}

impl CommitmentInput<'_> {
    /// The bytes to hash with SHA-256.
    pub fn preimage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(COMMIT_LABEL.len() + SPKI_LEN + NONCE_LEN);
        out.extend_from_slice(COMMIT_LABEL);
        out.extend_from_slice(self.spki_i);
        out.extend_from_slice(self.nonce_i);
        out
    }
}

/// What `s` hashes (step 4): `SAS_LABEL || ek || spki_I || spki_R || nonce_I || nonce_R`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SasInput<'a> {
    /// The TLS exporter output ([`EXPORTER_LABEL`], empty context, [`EXPORTER_LEN`] bytes).
    pub ek: &'a [u8; EXPORTER_LEN],
    pub spki_i: &'a [u8; SPKI_LEN],
    pub spki_r: &'a [u8; SPKI_LEN],
    pub nonce_i: &'a [u8; NONCE_LEN],
    pub nonce_r: &'a [u8; NONCE_LEN],
}

/// The exporter output and both nonces decide the code: none of them shows in `Debug`.
impl fmt::Debug for SasInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SasInput")
            .field("ek", &Redacted)
            .field("spki_i", &RedactedLen(self.spki_i.len()))
            .field("spki_r", &RedactedLen(self.spki_r.len()))
            .field("nonce_i", &Redacted)
            .field("nonce_r", &Redacted)
            .finish()
    }
}

impl SasInput<'_> {
    /// The bytes to hash with SHA-256.
    pub fn preimage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SAS_LABEL.len() + EXPORTER_LEN + 2 * SPKI_LEN + 2 * NONCE_LEN);
        out.extend_from_slice(SAS_LABEL);
        out.extend_from_slice(self.ek);
        out.extend_from_slice(self.spki_i);
        out.extend_from_slice(self.spki_r);
        out.extend_from_slice(self.nonce_i);
        out.extend_from_slice(self.nonce_r);
        out
    }
}

/// The six digits both users compare: `u64::from_be_bytes(s[0..8]) mod 1_000_000`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sas(u32);

/// The code is never logged: it is what two users compare to be sure no one sits between them.
impl fmt::Debug for Sas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sas(<redacted>)")
    }
}

impl Sas {
    pub const MODULUS: u64 = 1_000_000;

    /// From the SHA-256 digest `s` of a [`SasInput`] preimage.
    pub fn from_digest(s: &[u8; 32]) -> Self {
        let mut head = [0u8; 8];
        head.copy_from_slice(&s[..8]);
        Self((u64::from_be_bytes(head) % Self::MODULUS) as u32)
    }

    /// 0 to 999 999.
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Sas {
    /// Zero-padded in two groups of three, "482 913".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03} {:03}", self.0 / 1000, self.0 % 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_preimage_is_label_spki_nonce() {
        let spki = [7u8; SPKI_LEN];
        let nonce = [9u8; NONCE_LEN];
        let p = CommitmentInput { spki_i: &spki, nonce_i: &nonce }.preimage();
        assert!(p.starts_with(b"swapdesk commit v1"));
        assert_eq!(p.len(), COMMIT_LABEL.len() + SPKI_LEN + NONCE_LEN);
        assert_eq!(&p[18..18 + SPKI_LEN], &spki);
        assert_eq!(&p[18 + SPKI_LEN..], &nonce);
    }

    #[test]
    fn sas_preimage_orders_its_parts_as_step_4() {
        let (ek, ni, nr) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let (si, sr) = ([4u8; SPKI_LEN], [5u8; SPKI_LEN]);
        let p = SasInput { ek: &ek, spki_i: &si, spki_r: &sr, nonce_i: &ni, nonce_r: &nr }.preimage();
        let mut want = b"swapdesk sas v1".to_vec();
        want.extend_from_slice(&ek);
        want.extend_from_slice(&si);
        want.extend_from_slice(&sr);
        want.extend_from_slice(&ni);
        want.extend_from_slice(&nr);
        assert_eq!(p, want);
    }

    #[test]
    fn debug_of_the_pairing_inputs_and_the_code_shows_no_secret() {
        // Distinct marker bytes: each prints as its decimal value under a derived Debug, so a leak shows as "171".
        let (ek, ni, nr) = ([171u8; 32], [172u8; 32], [173u8; 32]);
        let (si, sr) = ([174u8; SPKI_LEN], [175u8; SPKI_LEN]);
        let commit = format!("{:?}", CommitmentInput { spki_i: &si, nonce_i: &ni });
        let sas_in = format!("{:?}", SasInput { ek: &ek, spki_i: &si, spki_r: &sr, nonce_i: &ni, nonce_r: &nr });
        let mut digest = [0u8; 32];
        digest[..8].copy_from_slice(&482_913u64.to_be_bytes());
        let sas = format!("{:?}", Sas::from_digest(&digest));
        for (shown, secrets) in [
            (&commit, &["172", "174"][..]),
            (&sas_in, &["171", "172", "173", "174", "175"][..]),
            (&sas, &["482", "913"][..]),
        ] {
            for secret in secrets {
                assert!(!shown.contains(secret), "{secret} leaked in {shown}");
            }
            assert!(shown.contains("redacted"), "{shown}");
        }
    }

    #[test]
    fn sas_is_the_first_eight_bytes_mod_a_million() {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&482_913u64.to_be_bytes());
        assert_eq!(Sas::from_digest(&s).to_string(), "482 913");
        s[..8].copy_from_slice(&(1_000_000u64 * 7 + 42).to_be_bytes());
        assert_eq!(Sas::from_digest(&s).to_string(), "000 042");
        s[..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(Sas::from_digest(&s).value(), (u64::MAX % 1_000_000) as u32);
        s[8..].fill(0xFF);
        assert_eq!(Sas::from_digest(&s).value(), (u64::MAX % 1_000_000) as u32);
    }
}
