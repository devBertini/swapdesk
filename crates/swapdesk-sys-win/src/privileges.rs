//! Privileges of the `--role system` helper's token (evaluation item B5).
//!
//! The service is LocalSystem with a long list of privileges (`SeTcbPrivilege`, `SeAssignPrimaryTokenPrivilege`,
//! `SeDebugPrivilege`, `SeBackupPrivilege`, `SeRestorePrivilege`, `SeImpersonatePrivilege`, ...). The helper decodes
//! nothing from the LAN, but it does run a capture, an encoder and an input injector, so it gets the service's token
//! with every privilege *deleted* (not just disabled: a deleted privilege cannot be switched back on) except the
//! ones in [`SYSTEM_HELPER_KEPT_PRIVILEGES`]. What stays is the user (SYSTEM), the groups and the integrity level:
//! those are what open the secure desktops (`winlogon`, UAC) and let `SendInput` reach elevated windows, and none of
//! them is a privilege.
//!
//! `SendSAS` is the service's (docs/ARCHITECTURE.md), and so are setting the helper's session id
//! (`SeTcbPrivilege`, checked on the *caller*, which is the service) and `CreateProcessAsUser`
//! (`SeAssignPrimaryTokenPrivilege`, also the caller's): the helper's token needs none of them.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, LUID};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, LUID_AND_ATTRIBUTES, LookupPrivilegeNameW, TOKEN_PRIVILEGES, TokenPrivileges, TokenSessionId,
};

use crate::sys::{OwnedHandle, Result, invalid, last_error, token_info};

/// The privileges the `--role system` helper keeps; everything else in the service's token is deleted. A privilege
/// is here only when code the helper runs needs it. Not listed on purpose (each is unused by the helper's code, found
/// by search): `SeTcb`, `SeAssignPrimaryToken`, `SeIncreaseQuota` (the service starts processes, the helper starts none),
/// `SeShutdown` (quick actions run in the user helper, not here), `SeDebug`, `SeBackup`, `SeRestore`, `SeTakeOwnership`,
/// `SeLoadDriver`, `SeImpersonate` (the service's pipe server impersonates clients, the helper is the client),
/// `SeCreateGlobal`, `SeCreateSymbolicLink`, `SeSecurity`, `SeSystemEnvironment`.
pub const SYSTEM_HELPER_KEPT_PRIVILEGES: &[&str] = &[
    // Bypass traverse checking: every file open by path (the helper's DLLs under Program Files, the GPU driver's
    // shader cache, `%TEMP%`) is checked only at the last component. Without it a restricted parent folder breaks
    // the open. Every token has it and `DISABLE_MAX_PRIVILEGE` itself leaves it, so it is not an extra power.
    "SeChangeNotifyPrivilege",
];

/// One privilege of a token.
pub(crate) struct Privilege {
    pub name: String,
    pub luid: LUID,
}

/// Splits `present` (names) into those `kept` lets stay and those removed. Pure; names compare case-insensitively.
pub(crate) fn partition_privileges(present: &[String], kept: &[&str]) -> (Vec<String>, Vec<String>) {
    present.iter().cloned().partition(|p| kept.iter().any(|k| k.eq_ignore_ascii_case(p)))
}

fn privilege_name(luid: &LUID) -> Result<String> {
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; null system name = this machine; `len` is a live out-parameter.
    unsafe { LookupPrivilegeNameW(null(), luid, null_mut(), &mut len) };
    if len == 0 {
        return Err(last_error("LookupPrivilegeNameW"));
    }
    // On the second call `len` is the count without the terminator.
    let mut buf = vec![0u16; len as usize + 1];
    let mut cap = buf.len() as u32;
    // SAFETY: `buf` holds `cap` UTF-16 units; `luid` is a live LUID.
    if unsafe { LookupPrivilegeNameW(null(), luid, buf.as_mut_ptr(), &mut cap) } == 0 {
        return Err(last_error("LookupPrivilegeNameW"));
    }
    Ok(String::from_utf16_lossy(&buf[..cap as usize]))
}

/// Every privilege the token holds (enabled or not). `token` needs TOKEN_QUERY.
pub(crate) fn token_privileges(token: HANDLE) -> Result<Vec<Privilege>> {
    let info = token_info(token, TokenPrivileges)?;
    // SAFETY: the buffer holds a TOKEN_PRIVILEGES written by the call above, 8-byte aligned.
    let header = unsafe { &*info.as_ptr().cast::<TOKEN_PRIVILEGES>() };
    let first: *const LUID_AND_ATTRIBUTES = header.Privileges.as_ptr();
    let mut out = Vec::with_capacity(header.PrivilegeCount as usize);
    for i in 0..header.PrivilegeCount as usize {
        // SAFETY: the kernel wrote `PrivilegeCount` entries starting at `Privileges`, all inside the buffer.
        let entry = unsafe { &*first.add(i) };
        out.push(Privilege { name: privilege_name(&entry.Luid)?, luid: entry.Luid });
    }
    Ok(out)
}

/// A copy of `source` (same type, user, groups, integrity level and session) with every privilege deleted except
/// those named in `kept`; a kept privilege the token does not hold is simply absent. `source` needs TOKEN_DUPLICATE
/// and TOKEN_QUERY. The result is a primary token when `source` is one, and is not a "restricted" token in the
/// SID sense (no restricting SIDs), so its access checks are the user's own. The returned handle has only the access
/// of the `source` handle (measured: both 0xa for a source opened with DUPLICATE and QUERY), so a source meant for
/// `CreateProcessAsUser` must carry TOKEN_ASSIGN_PRIMARY too.
pub(crate) fn restrict_privileges(source: HANDLE, kept: &[&str]) -> Result<OwnedHandle> {
    let present = token_privileges(source)?;
    let names: Vec<String> = present.iter().map(|p| p.name.clone()).collect();
    let (_, removed) = partition_privileges(&names, kept);
    let delete: Vec<LUID_AND_ATTRIBUTES> = present
        .iter()
        .filter(|p| removed.contains(&p.name))
        .map(|p| LUID_AND_ATTRIBUTES { Luid: p.luid, Attributes: 0 })
        .collect();
    let mut out: HANDLE = null_mut();
    // SAFETY: `source` is a token handle with TOKEN_DUPLICATE and TOKEN_QUERY; `delete` holds `delete.len()` entries (a null
    // pointer with count 0 when empty); no SIDs are disabled or restricted; `out` is a live out-parameter.
    let ok = unsafe {
        CreateRestrictedToken(
            source,
            0,
            0,
            null(),
            delete.len() as u32,
            if delete.is_empty() { null() } else { delete.as_ptr() },
            0,
            null(),
            &mut out,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateRestrictedToken"));
    }
    Ok(OwnedHandle(out))
}

/// The helper's token: `source` (the service's token already moved to `session_id`) minus every privilege not in
/// [`SYSTEM_HELPER_KEPT_PRIVILEGES`]. Checks the result before returning it: nothing outside the kept list may remain, and the session
/// id must have survived the copy (a token that fell back to session 0 would start the helper on the wrong desktop).
pub(crate) fn restrict_system_helper_token(source: HANDLE, session_id: u32) -> Result<OwnedHandle> {
    let restricted = restrict_privileges(source, SYSTEM_HELPER_KEPT_PRIVILEGES)?;
    let names: Vec<String> = token_privileges(restricted.0)?.into_iter().map(|p| p.name).collect();
    let (_, extra) = partition_privileges(&names, SYSTEM_HELPER_KEPT_PRIVILEGES);
    if !extra.is_empty() {
        return Err(invalid(
            "restrict_system_helper_token",
            format!("privileges remained after the restriction: {}", extra.join(", ")),
        ));
    }
    let seen = token_session(restricted.0)?;
    if seen != session_id {
        return Err(invalid(
            "restrict_system_helper_token",
            format!("the restricted token is in session {seen}, not {session_id}"),
        ));
    }
    Ok(restricted)
}

/// `TokenSessionId` of a token.
pub(crate) fn token_session(token: HANDLE) -> Result<u32> {
    let info = token_info(token, TokenSessionId)?;
    // SAFETY: the buffer holds a DWORD written by GetTokenInformation(TokenSessionId), 8-byte aligned.
    Ok(unsafe { *info.as_ptr().cast::<u32>() })
}

#[cfg(test)]
mod tests {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY, TokenElevationType, TokenType,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessAsUserW, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
    };

    use super::*;
    use crate::sys::{open_process_token, token_user_sid, wide};

    fn names(token: HANDLE) -> Vec<String> {
        token_privileges(token).unwrap().into_iter().map(|p| p.name).collect()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_kept_list_is_small_and_has_only_what_is_justified() {
        assert_eq!(SYSTEM_HELPER_KEPT_PRIVILEGES, ["SeChangeNotifyPrivilege"]);
    }

    #[test]
    fn partition_splits_present_privileges_by_the_kept_list() {
        let present = strs(&["SeTcbPrivilege", "SeChangeNotifyPrivilege", "SeDebugPrivilege", "SeShutdownPrivilege"]);
        let (kept, removed) = partition_privileges(&present, SYSTEM_HELPER_KEPT_PRIVILEGES);
        assert_eq!(kept, strs(&["SeChangeNotifyPrivilege"]));
        assert_eq!(removed, strs(&["SeTcbPrivilege", "SeDebugPrivilege", "SeShutdownPrivilege"]));
        // A kept name the token lacks adds nothing; case does not matter.
        let (kept, removed) =
            partition_privileges(&strs(&["sechangenotifyprivilege"]), &["SeChangeNotifyPrivilege", "SeTcbPrivilege"]);
        assert_eq!((kept, removed), (strs(&["sechangenotifyprivilege"]), vec![]));
        assert_eq!(partition_privileges(&[], SYSTEM_HELPER_KEPT_PRIVILEGES), (vec![], vec![]));
    }

    fn fetch<T: Copy>(token: HANDLE, class: i32) -> T {
        let info = token_info(token, class).unwrap();
        // SAFETY: the buffer holds a value of the class's plain type written by GetTokenInformation, 8-byte aligned.
        unsafe { *info.as_ptr().cast::<T>() }
    }

    #[test]
    fn restricting_this_process_token_keeps_only_the_kept_list_and_nothing_else_changes() {
        let me = open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY).unwrap();
        let before = names(me.0);
        assert!(before.iter().any(|p| p == "SeChangeNotifyPrivilege"), "every token holds it: {before:?}");
        let session = token_session(me.0).unwrap();

        let restricted = restrict_system_helper_token(me.0, session).unwrap();
        let after = names(restricted.0);
        assert_eq!(after, ["SeChangeNotifyPrivilege"], "was {before:?}");
        // The rest of the token is the same: user, session, primary type, elevation.
        assert_eq!(token_user_sid(restricted.0).unwrap(), token_user_sid(me.0).unwrap());
        assert_eq!(token_session(restricted.0).unwrap(), session, "the session id survives the restriction");
        assert_eq!(fetch::<i32>(restricted.0, TokenType), fetch::<i32>(me.0, TokenType), "still a primary token");
        assert_eq!(fetch::<i32>(restricted.0, TokenElevationType), fetch::<i32>(me.0, TokenElevationType));
        // The source token was not touched.
        assert_eq!(names(me.0), before);
    }

    #[test]
    fn a_wider_kept_list_keeps_exactly_those_present() {
        let me = open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY).unwrap();
        let before = names(me.0);
        let kept = ["SeChangeNotifyPrivilege", "SeIncreaseWorkingSetPrivilege", "SeTcbPrivilege"];
        let restricted = restrict_privileges(me.0, &kept).unwrap();
        let mut after = names(restricted.0);
        let mut expected: Vec<String> = before.iter().filter(|p| kept.contains(&p.as_str())).cloned().collect();
        after.sort();
        expected.sort();
        assert_eq!(after, expected, "was {before:?}");
        assert!(!after.iter().any(|p| p == "SeTcbPrivilege"), "a kept name this token never held is not conjured");
    }

    #[test]
    fn a_process_started_with_the_restricted_token_runs_with_only_the_kept_privileges() {
        // The restricted handle carries the access of its source handle (probed: both 0xa without ASSIGN_PRIMARY), as the
        // service's MAXIMUM_ALLOWED duplicate does; CreateProcessAsUser needs TOKEN_ASSIGN_PRIMARY on it.
        let me = open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY).unwrap();
        let session = token_session(me.0).unwrap();
        let restricted = restrict_system_helper_token(me.0, session).unwrap();
        let sysroot = std::env::var("SystemRoot").unwrap();
        let exe = wide(format!(r"{sysroot}\System32\cmd.exe").as_ref()).unwrap();
        // SAFETY: plain data; zeroed is valid, `cb` is set below.
        let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
        si.cb = size_of::<STARTUPINFOW>() as u32;
        // SAFETY: plain data.
        let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: the token is a restricted copy of this process's own primary token (no privilege is needed to assign it);
        // `exe` is null-terminated; the process starts suspended, so it runs nothing before it is read and ended below.
        let ok = unsafe {
            CreateProcessAsUserW(
                restricted.0,
                exe.as_ptr(),
                null_mut(),
                null(),
                null(),
                0,
                CREATE_SUSPENDED,
                null(),
                null(),
                &si,
                &mut pi,
            )
        };
        assert_ne!(ok, 0, "{:?}", last_error("CreateProcessAsUserW"));
        let (process, thread) = (OwnedHandle(pi.hProcess), OwnedHandle(pi.hThread));
        let mut raw: HANDLE = null_mut();
        // SAFETY: the process handle has full access; `raw` is a live out-parameter.
        let opened = unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut raw) };
        let child_token = OwnedHandle(raw);
        let child_privileges = if opened != 0 { Some(names(child_token.0)) } else { None };
        // SAFETY: the process handle is open and has PROCESS_TERMINATE.
        unsafe { TerminateProcess(process.0, 1) };
        drop(thread);
        assert_ne!(opened, 0, "{:?}", last_error("OpenProcessToken"));
        assert_eq!(child_privileges.unwrap(), ["SeChangeNotifyPrivilege"]);
    }
}
