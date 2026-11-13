//! DPAPI with the machine scope (`CRYPTPROTECT_LOCAL_MACHINE`), C5.2.
//!
//! Machine scope only ties a blob to this PC: any local user can decrypt one, so the folder's DACL is the real
//! protection (see `folder.rs`). The user scope ([`protect_user`]) ties a blob to the Windows account that made it:
//! only the release tool uses it, for the release signing key (`UPDATES.md` 4.4).

use std::ptr::{null, null_mut};

use swapdesk_platform::PlatformError;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

use crate::sys::{LocalMem, last_error, wipe};

const FLAGS: u32 = CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN;
const USER_FLAGS: u32 = CRYPTPROTECT_UI_FORBIDDEN;

fn blob(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, PlatformError> {
    let cb = u32::try_from(bytes.len()).map_err(|_| crate::sys::invalid("dpapi", "input over 4 GiB"))?;
    Ok(CRYPT_INTEGER_BLOB { cbData: cb, pbData: bytes.as_ptr().cast_mut() })
}

/// Copies the output buffer and zeroes it in place: it holds plaintext key material after `CryptUnprotectData`,
/// and `LocalFree` does not clear what it frees.
pub(crate) fn copy_and_wipe(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() || out.cbData == 0 {
        return Vec::new();
    }
    // SAFETY: on success DPAPI returns `cbData` bytes at `pbData`, owned by the caller until the free; nothing else
    // reads or writes them meanwhile.
    let buffer = unsafe { std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize) };
    let copy = buffer.to_vec();
    wipe(buffer);
    copy
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let _free = LocalMem(out.pbData.cast());
    copy_and_wipe(&out)
}

/// `CryptProtectData(data, entropy, CRYPTPROTECT_LOCAL_MACHINE)`.
pub fn protect(data: &[u8], entropy: &[u8]) -> Result<Vec<u8>, PlatformError> {
    protect_with(data, entropy, FLAGS)
}

/// `CryptProtectData` in the user scope: only the Windows account that made the blob can open it.
pub fn protect_user(data: &[u8], entropy: &[u8]) -> Result<Vec<u8>, PlatformError> {
    protect_with(data, entropy, USER_FLAGS)
}

/// [`unprotect`] for a blob of [`protect_user`].
pub fn unprotect_user(blob_bytes: &[u8], entropy: &[u8]) -> Result<Vec<u8>, PlatformError> {
    unprotect_with(blob_bytes, entropy, USER_FLAGS)
}

fn protect_with(data: &[u8], entropy: &[u8], flags: u32) -> Result<Vec<u8>, PlatformError> {
    let (input, ent) = (blob(data)?, blob(entropy)?);
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    // SAFETY: `input` and `ent` point at live slices for the call; DPAPI only reads them (the cast from const to
    // mut is the API's signature); `out` is a live out-parameter whose buffer `take` frees.
    let ok = unsafe { CryptProtectData(&input, null(), &ent, null(), null(), flags, &mut out) };
    if ok == 0 {
        return Err(last_error("CryptProtectData"));
    }
    Ok(take(out))
}

/// `CryptUnprotectData(blob, entropy)`; fails when the blob was altered or the entropy differs.
pub fn unprotect(blob_bytes: &[u8], entropy: &[u8]) -> Result<Vec<u8>, PlatformError> {
    unprotect_with(blob_bytes, entropy, FLAGS)
}

fn unprotect_with(blob_bytes: &[u8], entropy: &[u8], flags: u32) -> Result<Vec<u8>, PlatformError> {
    let (input, ent) = (blob(blob_bytes)?, blob(entropy)?);
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    // SAFETY: as in `protect`; the description out-pointer is null, so nothing else is allocated.
    let ok = unsafe { CryptUnprotectData(&input, null_mut(), &ent, null(), null(), flags, &mut out) };
    if ok == 0 {
        return Err(last_error("CryptUnprotectData"));
    }
    Ok(take(out))
}
