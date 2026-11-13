//! The administrator confirmation (`docs/ELEVATION-MAC.md` 2, 3.2): macOS's own dialog, through Authorization Services.
//!
//! * Which right: `system.privilege.admin` (`kAuthorizationRightExecute`), an existing right, so nothing is written to
//!   the system's policy database. It asks for an administrator's name and password; the Security Agent draws the
//!   dialog and checks the password, and this process never sees either.
//! * How: a fresh authorization reference per call, `AuthorizationCopyRights` with `interactionAllowed | extendRights |
//!   destroyRights`, then `AuthorizationFree` with `destroyRights`, so no credential outlives the request. The reference
//!   is never kept and the rights it was given are never used for anything else: only the status matters.
//! * The call blocks until a person answers and nothing can withdraw an open dialog, so it never runs on the main
//!   thread ([`refuse_on_main`]); the caller gives it a thread of its own.
//! * The outcome type and the status mapping are pure and built on every host; the calls are macOS only.

/// How macOS answered the administrator check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// An administrator's name and password were accepted for `system.privilege.admin`.
    Approved,
    /// The person pressed Cancel in the system's dialog (`errAuthorizationCanceled`).
    Cancelled,
    /// The credentials were not an administrator's, or the tries ran out (`errAuthorizationDenied`).
    Denied,
    /// No dialog could be shown (`errAuthorizationInteractionNotAllowed`): no GUI session.
    NoInteraction,
    /// Called on the main thread: refused without asking (the call blocks until a person answers).
    MainThread,
    /// Any other status, kept for the trace log.
    Failed(i32),
}

/// `errAuthorizationSuccess`.
const SUCCESS: i32 = 0;
/// `errAuthorizationDenied`.
const DENIED: i32 = -60005;
/// `errAuthorizationCanceled`.
const CANCELED: i32 = -60006;
/// `errAuthorizationInteractionNotAllowed`.
const INTERACTION_NOT_ALLOWED: i32 = -60007;

/// The outcome of an Authorization Services status (`OSStatus`). `errAuthorizationInternal` (-60008) and every other
/// value are `Failed`, with the status kept.
pub fn outcome_of(status: i32) -> AdminOutcome {
    match status {
        SUCCESS => AdminOutcome::Approved,
        CANCELED => AdminOutcome::Cancelled,
        DENIED => AdminOutcome::Denied,
        INTERACTION_NOT_ALLOWED => AdminOutcome::NoInteraction,
        other => AdminOutcome::Failed(other),
    }
}

/// `Some(MainThread)` when called on the main thread: the check blocks until a person answers.
pub fn refuse_on_main(is_main: bool) -> Option<AdminOutcome> {
    is_main.then_some(AdminOutcome::MainThread)
}

#[cfg(target_os = "macos")]
pub use mac::{check_administrator_silently, confirm_administrator};

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};

    use objc2_security::{
        AuthorizationCopyRights, AuthorizationCreate, AuthorizationEnvironment, AuthorizationFlags, AuthorizationFree,
        AuthorizationItem, AuthorizationItemSet, AuthorizationRef, AuthorizationRights,
        kAuthorizationEnvironmentPrompt, kAuthorizationRightExecute,
    };

    use super::{AdminOutcome, SUCCESS, outcome_of, refuse_on_main};

    /// `errAuthorizationInvalidRef`: the call succeeded and gave no reference.
    const NO_REFERENCE: i32 = -60002;

    /// The right asked for, as the item of the rights set.
    pub(super) fn right_item() -> AuthorizationItem {
        AuthorizationItem {
            name: kAuthorizationRightExecute.as_ptr(),
            valueLength: 0,
            value: ptr::null_mut(),
            flags: 0,
        }
    }

    /// The dialog's additional text as the item of the environment: `prompt` as UTF-8 bytes (`valueLength` counts them,
    /// no terminator). The item only points into `text`, which must outlive the call that takes it.
    pub(super) fn prompt_item(text: &str) -> AuthorizationItem {
        AuthorizationItem {
            name: kAuthorizationEnvironmentPrompt.as_ptr(),
            valueLength: text.len(),
            value: text.as_ptr().cast_mut().cast::<c_void>(),
            flags: 0,
        }
    }

    /// The right's name as the bytes the system reads, without the terminator.
    #[cfg(test)]
    pub(super) fn item_name(item: &AuthorizationItem) -> &[u8] {
        // SAFETY: both item builders above point `name` at a `'static` NUL-terminated constant of objc2-security.
        unsafe { std::ffi::CStr::from_ptr(item.name) }.to_bytes()
    }

    /// Asks macOS for an administrator (`system.privilege.admin`, interaction allowed, rights extended and destroyed
    /// after the call, a fresh reference freed with `destroyRights`). Blocks until the dialog ends. `prompt` is the
    /// dialog's additional text. Never call it on the main thread: it returns `MainThread` without asking.
    pub fn confirm_administrator(prompt: Option<&str>) -> AdminOutcome {
        // SAFETY: `pthread_main_np` takes no argument and only reads the calling thread's identity.
        if let Some(refused) = refuse_on_main(unsafe { libc::pthread_main_np() } != 0) {
            return refused;
        }
        let flags = AuthorizationFlags::InteractionAllowed | AuthorizationFlags::ExtendRights;
        copy_rights(prompt.filter(|p| !p.is_empty()), flags)
    }

    /// The same check without interaction: it never shows a dialog, so it is never `Approved` unless the system already
    /// holds a credential for the right (tests and probes).
    pub fn check_administrator_silently() -> AdminOutcome {
        copy_rights(None, AuthorizationFlags::ExtendRights)
    }

    fn copy_rights(prompt: Option<&str>, flags: AuthorizationFlags) -> AdminOutcome {
        let mut reference: AuthorizationRef = ptr::null();
        // SAFETY: no rights and no environment are passed (null is allowed for both) and `reference` is a valid place for
        // the created reference. Defaults ask nothing of the user.
        let created =
            unsafe { AuthorizationCreate(ptr::null(), ptr::null(), AuthorizationFlags::Defaults, &raw mut reference) };
        if created != SUCCESS || reference.is_null() {
            return outcome_of(if created == SUCCESS { NO_REFERENCE } else { created });
        }

        let mut right = right_item();
        let mut rights: AuthorizationRights = AuthorizationItemSet { count: 1, items: &raw mut right };
        let mut prompt_items = prompt.map(prompt_item);
        let environment: Option<AuthorizationEnvironment> =
            prompt_items.as_mut().map(|item| AuthorizationItemSet { count: 1, items: ptr::from_mut(item) });
        let environment_ptr: *const AuthorizationEnvironment = environment.as_ref().map_or(ptr::null(), ptr::from_ref);

        // SAFETY: `reference` was created above and is freed below; `rights` and `environment` point at items that live
        // until this call returns (`right`, `prompt_items`, and the prompt's bytes borrowed from `prompt`); the
        // authorized-rights out parameter is null, which the call allows when the granted set is not wanted.
        let status = unsafe {
            AuthorizationCopyRights(
                reference,
                NonNull::from(&mut rights),
                environment_ptr,
                flags | AuthorizationFlags::DestroyRights,
                ptr::null_mut(),
            )
        };
        // SAFETY: `reference` is valid and not used again. `destroyRights` revokes whatever it was given, here and in
        // any process that shared it.
        let freed = unsafe { AuthorizationFree(reference, AuthorizationFlags::DestroyRights) };
        let outcome = outcome_of(status);
        // A freed reference that failed to free is no reason to turn an answer into a failure, except an approval: an
        // approval whose credentials may live on is not one.
        if freed != SUCCESS && outcome == AdminOutcome::Approved {
            return outcome_of(freed);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_authorization_status_maps_to_one_outcome() {
        assert_eq!(outcome_of(0), AdminOutcome::Approved);
        assert_eq!(outcome_of(-60005), AdminOutcome::Denied);
        assert_eq!(outcome_of(-60006), AdminOutcome::Cancelled);
        assert_eq!(outcome_of(-60007), AdminOutcome::NoInteraction);
        assert_eq!(outcome_of(-60008), AdminOutcome::Failed(-60008));
        assert_eq!(outcome_of(-60001), AdminOutcome::Failed(-60001));
        assert_eq!(outcome_of(1), AdminOutcome::Failed(1));
        assert_eq!(outcome_of(i32::MIN), AdminOutcome::Failed(i32::MIN));
    }

    #[test]
    fn the_main_thread_is_refused_without_asking() {
        assert_eq!(refuse_on_main(true), Some(AdminOutcome::MainThread));
        assert_eq!(refuse_on_main(false), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_right_asked_for_is_system_privilege_admin() {
        assert_eq!(mac::item_name(&mac::right_item()), b"system.privilege.admin");
        assert_eq!(mac::right_item().valueLength, 0);
        let text = "Entrar sem perguntar";
        let p = mac::prompt_item(text);
        assert_eq!(mac::item_name(&p), b"prompt");
        assert_eq!(p.valueLength, text.len());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_check_without_interaction_is_never_approved() {
        let started = std::time::Instant::now();
        let outcome = mac::check_administrator_silently();
        // A prompt waits for a person for minutes; the silent path answers in seconds, and how many depends on what
        // else is asking the security daemon (4.8 s alone and 16 s beside the keychain tests, 10/11/2026). The bound
        // sits between the two.
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "it must not wait for anybody");
        assert!(
            matches!(outcome, AdminOutcome::NoInteraction | AdminOutcome::Denied),
            "a silent check was answered {outcome:?}"
        );
    }
}
