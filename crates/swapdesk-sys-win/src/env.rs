//! The environment block of a token's user, shared by the helper launch and the terminal spawner.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};

use crate::sys::{Result, last_error};

/// A Unicode environment block from `CreateEnvironmentBlock`, destroyed on drop.
pub(crate) struct EnvBlock(*mut c_void);

impl EnvBlock {
    /// The block of the user `token` belongs to; the service's own environment is not inherited.
    pub(crate) fn for_token(token: HANDLE) -> Result<EnvBlock> {
        let mut env: *mut c_void = null_mut();
        // SAFETY: the caller passes a primary token with TOKEN_QUERY and TOKEN_DUPLICATE; `env` is a live out-parameter; not inheriting.
        if unsafe { CreateEnvironmentBlock(&mut env, token, 0) } == 0 {
            return Err(last_error("CreateEnvironmentBlock"));
        }
        Ok(EnvBlock(env))
    }

    /// For `lpEnvironment` together with `CREATE_UNICODE_ENVIRONMENT`; valid while `self` lives.
    pub(crate) fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Drop for EnvBlock {
    fn drop(&mut self) {
        // SAFETY: the block came from CreateEnvironmentBlock and is destroyed once.
        unsafe { DestroyEnvironmentBlock(self.0) };
    }
}
