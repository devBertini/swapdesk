//! Generic-password items through the SecItem API: class `kSecClassGenericPassword`, attributes service and account,
//! value data.
//!
//! * [`Target::Login`] sets no keychain key: macOS uses the user's search list and adds to the default (login) keychain,
//!   which is the file-based keychain because no data-protection keychain key is set (TN3137).
//! * [`Target::File`] names one keychain file (stage 24: `/Library/Keychains/System.keychain`; tests: a throwaway
//!   keychain). The file is opened with `SecKeychainOpen` on every call (a keychain reference is not shared between
//!   threads here), items are added with `kSecUseKeychain` and found, updated and deleted with `kSecMatchSearchList`.
//!   `SecKeychain*` is deprecated by Apple, but `kSecUseKeychain` and `kSecMatchSearchList` take that type and there is
//!   no replacement for addressing a keychain file with SecItem on macOS.

use std::ffi::CString;
use std::ptr::NonNull;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType, Type, kCFBooleanTrue,
};
use objc2_security::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate, SecKeychain, errSecDuplicateItem,
    errSecInteractionNotAllowed, errSecItemNotFound, errSecUserCanceled, kSecAttrAccount, kSecAttrService, kSecClass,
    kSecClassGenericPassword, kSecMatchLimit, kSecMatchLimitAll, kSecMatchLimitOne, kSecMatchSearchList,
    kSecReturnData, kSecUseKeychain, kSecValueData,
};
use swapdesk_platform::{ErrorKind, PlatformError, Result};

type Dict = CFDictionary<CFString, CFType>;

/// Which keychain a call addresses.
#[derive(Debug, Clone)]
pub enum Target {
    /// The user's search list and default keychain (the login keychain).
    Login,
    /// One keychain file, by its POSIX path.
    File(CString),
}

fn status_error(op: &'static str, status: i32) -> PlatformError {
    // errSecInteractionNotAllowed: the keychain is locked or has no user session to ask (an SSH session, a daemon).
    // errSecUserCanceled: the user dismissed a keychain prompt. Neither is retried here: the caller reports it.
    let (kind, detail) = match status {
        s if s == errSecInteractionNotAllowed => {
            (ErrorKind::Unavailable, "the keychain needs user interaction (locked, or no GUI session)".to_string())
        }
        s if s == errSecUserCanceled => (ErrorKind::Unavailable, "the user dismissed the keychain prompt".to_string()),
        s => (ErrorKind::Os, format!("keychain status {s}")),
    };
    PlatformError { kind, op, os_code: Some(i64::from(status)), detail }
}

/// A keychain reference opened for the length of one call; `None` for the login keychain.
fn open(target: &Target) -> Result<Option<CFRetained<SecKeychain>>> {
    let Target::File(path) = target else { return Ok(None) };
    let mut keychain: *mut SecKeychain = std::ptr::null_mut();
    // SAFETY: `path` is a NUL-terminated string that outlives the call and `keychain` is a valid out-pointer.
    #[allow(deprecated)]
    let status = unsafe { SecKeychain::open(NonNull::from(path.as_c_str()).cast(), NonNull::from(&mut keychain)) };
    if status != 0 {
        return Err(status_error("SecKeychainOpen", status));
    }
    let ptr = NonNull::new(keychain).ok_or_else(|| status_error("SecKeychainOpen", -2))?;
    // SAFETY: on success `keychain` is a +1 retained SecKeychainRef that this call owns (Create rule).
    Ok(Some(unsafe { CFRetained::from_raw(ptr) }))
}

/// The attributes that name one item (or, without an account, every item of the service).
fn base(service: &str, account: Option<&str>) -> Vec<(&'static CFString, CFRetained<CFString>)> {
    // SAFETY: reading the exported constants of the Security framework, which live for the whole process.
    let (class_key, class_value, service_key, account_key) =
        unsafe { (kSecClass, kSecClassGenericPassword, kSecAttrService, kSecAttrAccount) };
    let mut v: Vec<(&'static CFString, CFRetained<CFString>)> =
        vec![(class_key, class_value.retain()), (service_key, CFString::from_str(service))];
    if let Some(a) = account {
        v.push((account_key, CFString::from_str(a)));
    }
    v
}

fn dict(entries: &[(&CFString, &CFType)]) -> CFRetained<Dict> {
    let keys: Vec<&CFString> = entries.iter().map(|(k, _)| *k).collect();
    let values: Vec<&CFType> = entries.iter().map(|(_, v)| *v).collect();
    Dict::from_slices(&keys, &values)
}

/// Builds a query: the item's attributes, the keychain restriction (`kSecMatchSearchList`) and `extra` entries.
fn query(
    attrs: &[(&'static CFString, CFRetained<CFString>)],
    keychain: Option<&SecKeychain>,
    extra: &[(&CFString, &CFType)],
) -> CFRetained<Dict> {
    let list = keychain.map(|k| CFArray::<SecKeychain>::from_objects(&[k]));
    let mut entries: Vec<(&CFString, &CFType)> = attrs.iter().map(|(k, v)| (*k, &***v)).collect();
    if let Some(list) = &list {
        // SAFETY: reading an exported constant of the Security framework.
        entries.push((unsafe { kSecMatchSearchList }, &***list));
    }
    entries.extend_from_slice(extra);
    // The dictionary retains the keys and values it was made from, so `list` may drop after this call.
    dict(&entries)
}

/// The item's bytes, or `None` when there is no such item.
pub fn get(target: &Target, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
    let keychain = open(target)?;
    let attrs = base(service, Some(account));
    // SAFETY: reading exported constants of the Security framework.
    let (return_data, limit_key, limit_one) = unsafe { (kSecReturnData, kSecMatchLimit, kSecMatchLimitOne) };
    // SAFETY: `kCFBooleanTrue` is a CoreFoundation constant, valid for the whole process.
    let yes: &CFBoolean = unsafe { kCFBooleanTrue }.ok_or_else(|| status_error("SecItemCopyMatching", -1))?;
    let q = query(&attrs, keychain.as_deref(), &[(return_data, yes), (limit_key, limit_one)]);

    let mut result: *const CFType = std::ptr::null();
    // SAFETY: `q` is a valid dictionary of CFString keys and CFType values; `result` is a valid out-pointer.
    let status = unsafe { SecItemCopyMatching(q.as_opaque(), &mut result) };
    if status == errSecItemNotFound {
        return Ok(None);
    }
    if status != 0 {
        return Err(status_error("SecItemCopyMatching", status));
    }
    let Some(ptr) = NonNull::new(result.cast_mut()) else {
        return Err(status_error("SecItemCopyMatching", -2));
    };
    // SAFETY: on success `result` is a +1 retained CFType that this call owns (Copy rule); `CFRetained` releases it.
    let value = unsafe { CFRetained::from_raw(ptr) };
    let data = value.downcast::<CFData>().map_err(|_| PlatformError {
        kind: ErrorKind::Os,
        op: "SecItemCopyMatching",
        os_code: None,
        detail: "the item's value is not data".to_string(),
    })?;
    Ok(Some(data.to_vec()))
}

/// Adds the item, or replaces its bytes when it exists.
pub fn set(target: &Target, service: &str, account: &str, bytes: &[u8]) -> Result<()> {
    let keychain = open(target)?;
    let attrs = base(service, Some(account));
    let data = CFData::from_bytes(bytes);
    // SAFETY: reading exported constants of the Security framework.
    let (value_key, use_keychain) = unsafe { (kSecValueData, kSecUseKeychain) };
    let mut add: Vec<(&CFString, &CFType)> = attrs.iter().map(|(k, v)| (*k, &***v)).collect();
    add.push((value_key, &data));
    if let Some(k) = &keychain {
        add.push((use_keychain, k));
    }
    let add = dict(&add);
    // SAFETY: `add` is a valid attribute dictionary; a null result pointer is allowed (no result requested).
    let status = unsafe { SecItemAdd(add.as_opaque(), std::ptr::null_mut()) };
    if status == 0 {
        return Ok(());
    }
    if status != errSecDuplicateItem {
        return Err(status_error("SecItemAdd", status));
    }
    let q = query(&attrs, keychain.as_deref(), &[]);
    let change = dict(&[(value_key, &*data)]);
    // SAFETY: both dictionaries are valid: the query names the item, the change holds the new value.
    let status = unsafe { SecItemUpdate(q.as_opaque(), change.as_opaque()) };
    if status != 0 {
        return Err(status_error("SecItemUpdate", status));
    }
    Ok(())
}

/// Deletes the item; one that is not there is a success. Without an account, deletes every item of the service.
pub fn remove(target: &Target, service: &str, account: Option<&str>) -> Result<()> {
    let keychain = open(target)?;
    let attrs = base(service, account);
    // SAFETY: reading exported constants of the Security framework.
    let (limit_key, limit_all) = unsafe { (kSecMatchLimit, kSecMatchLimitAll) };
    let q = if account.is_none() {
        query(&attrs, keychain.as_deref(), &[(limit_key, limit_all)])
    } else {
        query(&attrs, keychain.as_deref(), &[])
    };
    // SAFETY: `q` is a valid dictionary of CFString keys and CFType values.
    let status = unsafe { SecItemDelete(q.as_opaque()) };
    if status == 0 || status == errSecItemNotFound { Ok(()) } else { Err(status_error("SecItemDelete", status)) }
}

/// Creates a new, unlocked keychain file protected by `password` (development and tests: a throwaway keychain that
/// needs no user session). `SecKeychainCreate` does not add the keychain to the user's search list.
pub fn create_file(path: &CString, password: &str) -> Result<()> {
    let mut keychain: *mut SecKeychain = std::ptr::null_mut();
    let length = u32::try_from(password.len()).map_err(|_| status_error("SecKeychainCreate", -50))?;
    // SAFETY: `path` is a NUL-terminated string and `password` a buffer of `length` bytes, both alive for the call;
    // no prompt, no initial access object; `keychain` is a valid out-pointer.
    #[allow(deprecated)]
    let status = unsafe {
        SecKeychain::create(
            NonNull::from(path.as_c_str()).cast(),
            length,
            password.as_ptr().cast(),
            false,
            None,
            NonNull::from(&mut keychain),
        )
    };
    if status != 0 {
        return Err(status_error("SecKeychainCreate", status));
    }
    if let Some(ptr) = NonNull::new(keychain) {
        // SAFETY: on success `keychain` is a +1 retained SecKeychainRef that this call owns; dropping releases it.
        drop(unsafe { CFRetained::from_raw(ptr) });
    }
    Ok(())
}

/// Deletes a keychain file made by [`create_file`] (and removes it from any search list).
pub fn delete_file(path: &CString) -> Result<()> {
    let Some(keychain) = open(&Target::File(path.clone()))? else { return Ok(()) };
    // SAFETY: `keychain` is a valid reference; the call deletes the file and does not consume the reference.
    #[allow(deprecated)]
    let status = unsafe { SecKeychain::delete(Some(&keychain)) };
    if status != 0 {
        return Err(status_error("SecKeychainDelete", status));
    }
    Ok(())
}
