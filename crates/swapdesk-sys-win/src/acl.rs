//! Reading ACLs back: the entries of a DACL as plain data, from an open kernel handle or from a file path.

use std::ffi::OsStr;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, GetSecurityInfo, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};

use crate::sys::{LocalMem, Result, invalid, last_error, os_error, sid_to_string, wide};

pub const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
pub const ACCESS_DENIED_ACE_TYPE: u8 = 1;
/// `ACE_HEADER.AceFlags` bit: the entry only applies to children of the object.
pub const INHERIT_ONLY_ACE: u8 = 0x08;

/// One entry of a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceInfo {
    pub ace_type: u8,
    pub flags: u8,
    pub mask: u32,
    /// `S-1-5-...`; `None` for an entry type this crate does not parse (object and callback ACEs).
    pub sid: Option<String>,
}

/// Entries of a DACL, in order.
///
/// # Safety
/// `acl` must point to a valid ACL that stays alive for the call.
unsafe fn aces_of(acl: *const ACL) -> Result<Vec<AceInfo>> {
    let mut size = ACL_SIZE_INFORMATION { AceCount: 0, AclBytesInUse: 0, AclBytesFree: 0 };
    // SAFETY: the caller guarantees a valid ACL; `size` is a live out-buffer of the stated size.
    if unsafe {
        GetAclInformation(acl, (&raw mut size).cast(), size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation)
    } == 0
    {
        return Err(last_error("GetAclInformation"));
    }
    let mut out = Vec::with_capacity(size.AceCount as usize);
    for i in 0..size.AceCount {
        let mut ace: *mut core::ffi::c_void = null_mut();
        // SAFETY: `i` is below the ACE count just read; `ace` is a live out-parameter.
        if unsafe { GetAce(acl, i, &mut ace) } == 0 {
            return Err(last_error("GetAce"));
        }
        // SAFETY: GetAce returned a pointer to an ACE inside the ACL, which starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        let (mask, sid) = if header.AceType == ACCESS_ALLOWED_ACE_TYPE || header.AceType == ACCESS_DENIED_ACE_TYPE {
            // SAFETY: allowed and denied ACEs share the layout of ACCESS_ALLOWED_ACE: header, mask, then the SID.
            let a = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid_ptr: PSID =
                (ace as *mut u8).wrapping_add(core::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart)).cast();
            (a.Mask, Some(sid_to_string(sid_ptr)?))
        } else {
            (0, None)
        };
        out.push(AceInfo { ace_type: header.AceType, flags: header.AceFlags, mask, sid });
    }
    Ok(out)
}

/// The DACL of an open kernel object handle (needs READ_CONTROL), such as a named pipe.
pub fn dacl_of_kernel_handle(handle: HANDLE) -> Result<Vec<AceInfo>> {
    read_via_handle(handle, SE_KERNEL_OBJECT)
}

fn read_via_handle(handle: HANDLE, kind: SE_OBJECT_TYPE) -> Result<Vec<AceInfo>> {
    let (mut dacl, mut sd): (*mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
    // SAFETY: the caller's handle has READ_CONTROL; the out-parameters are live; `sd` is freed by LocalMem.
    let err = unsafe {
        GetSecurityInfo(handle, kind, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut sd)
    };
    if err != 0 {
        return Err(os_error("GetSecurityInfo", err));
    }
    let _free = LocalMem(sd);
    if dacl.is_null() {
        return Err(invalid("GetSecurityInfo", "the object has no DACL (everyone has full access)"));
    }
    // SAFETY: `dacl` points into `sd`, which is alive until the end of this function.
    unsafe { aces_of(dacl) }
}

/// Owner (as `S-1-5-...`) and DACL of a file or folder.
pub fn security_of_path(path: &std::path::Path) -> Result<(String, Vec<AceInfo>)> {
    let w = wide(OsStr::new(path))?;
    let (mut owner, mut dacl, mut sd): (PSID, *mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut(), null_mut());
    // SAFETY: `w` is a null-terminated wide string; the out-parameters are live; `sd` is freed by LocalMem.
    let err = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(os_error("GetNamedSecurityInfoW", err));
    }
    let _free = LocalMem(sd);
    if dacl.is_null() {
        return Err(invalid("GetNamedSecurityInfoW", "the object has no DACL (everyone has full access)"));
    }
    let owner = sid_to_string(owner)?;
    // SAFETY: `dacl` points into `sd`, which is alive until the end of this function.
    Ok((owner, unsafe { aces_of(dacl) }?))
}
