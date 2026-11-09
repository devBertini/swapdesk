//! The app as its own login item (D32): `SMAppService.mainApp.register()`.
//!
//! Apple (ServiceManagement, `SMAppService`, read 09/11): `mainAppService` is the service for the main application;
//! `register()` on it means "the application launches on subsequent logins" and needs no administrator's approval
//! (a LaunchDaemon would). `status` is `SMAppServiceStatus`: `notRegistered` 0, `enabled` 1, `requiresApproval` 2,
//! `notFound` 3.
//!
//! No crate for it: the class is reached at run time (`AnyClass::get` + `msg_send!`) and the framework is linked here.
//! [`register`] is called only by a normal run (see `Options::registers_login_item`), never by a test or a scratch run.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSError;
use swapdesk_platform::Level;

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

/// `SMAppServiceStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
    /// A value this build does not know (a newer macOS).
    Other(isize),
}

impl Status {
    pub fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Self::NotRegistered,
            1 => Self::Enabled,
            2 => Self::RequiresApproval,
            3 => Self::NotFound,
            other => Self::Other(other),
        }
    }

    /// The word the trace log shows.
    pub fn name(self) -> String {
        match self {
            Self::NotRegistered => "notRegistered".to_owned(),
            Self::Enabled => "enabled".to_owned(),
            Self::RequiresApproval => "requiresApproval".to_owned(),
            Self::NotFound => "notFound".to_owned(),
            Self::Other(raw) => format!("unknown({raw})"),
        }
    }
}

/// The trace line and its level for a registration attempt: `error` is the registration's error text, if it failed.
pub fn describe(status: Option<Status>, error: Option<&str>) -> (Level, String) {
    let status_text = status.map_or_else(|| "unavailable".to_owned(), Status::name);
    match (error, status) {
        (Some(e), _) => (Level::Warn, format!("login item: register failed ({e}); status {status_text}")),
        (None, Some(Status::Enabled)) => (Level::Info, format!("login item: status {status_text}")),
        (None, _) => (Level::Warn, format!("login item: status {status_text}")),
    }
}

/// Registers the running app as a login item and returns the trace line (level and text) for the result.
pub fn register() -> (Level, String) {
    let Some(class) = AnyClass::get(c"SMAppService") else {
        return describe(None, Some("class SMAppService not found"));
    };
    // SAFETY: `mainAppService` is a class method of `SMAppService` that takes nothing and returns the service (or nil).
    let service: Option<Retained<AnyObject>> = unsafe { msg_send![class, mainAppService] };
    let Some(service) = service else {
        return describe(None, Some("mainAppService returned nil"));
    };
    // SAFETY: `registerAndReturnError:` takes an `NSError **` (what `msg_send!` passes for the trailing `_`) and returns
    // a BOOL; the error, if any, is autoreleased and retained by the `Result`.
    let registered: Result<(), Retained<NSError>> = unsafe { msg_send![&*service, registerAndReturnError: _] };
    let error = registered.err().map(|e| e.localizedDescription().to_string());
    describe(status_of(&service), error.as_deref())
}

/// `-[SMAppService status]` of `service`.
fn status_of(service: &AnyObject) -> Option<Status> {
    // SAFETY: `status` takes nothing and returns an `SMAppServiceStatus` (`NSInteger`).
    let raw: isize = unsafe { msg_send![service, status] };
    Some(Status::from_raw(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raw_values_are_apples() {
        assert_eq!(Status::from_raw(0), Status::NotRegistered);
        assert_eq!(Status::from_raw(1), Status::Enabled);
        assert_eq!(Status::from_raw(2), Status::RequiresApproval);
        assert_eq!(Status::from_raw(3), Status::NotFound);
        assert_eq!(Status::from_raw(9), Status::Other(9));
        assert_eq!(Status::from_raw(1).name(), "enabled");
        assert_eq!(Status::from_raw(2).name(), "requiresApproval");
        assert_eq!(Status::from_raw(9).name(), "unknown(9)");
    }

    #[test]
    fn only_an_enabled_service_without_error_is_info() {
        assert_eq!(describe(Some(Status::Enabled), None), (Level::Info, "login item: status enabled".to_owned()));
        assert_eq!(
            describe(Some(Status::RequiresApproval), None),
            (Level::Warn, "login item: status requiresApproval".to_owned())
        );
        assert_eq!(describe(Some(Status::NotFound), None).0, Level::Warn);
        let (level, text) = describe(Some(Status::NotRegistered), Some("Operation not permitted"));
        assert_eq!(level, Level::Warn);
        assert_eq!(text, "login item: register failed (Operation not permitted); status notRegistered");
        assert_eq!(describe(None, Some("x")).1, "login item: register failed (x); status unavailable");
    }

    /// Reaching the class must work without registering anything: this only looks the class up.
    #[test]
    fn the_service_class_is_reachable_at_run_time() {
        assert!(AnyClass::get(c"SMAppService").is_some(), "SMAppService is not loaded: the framework link failed");
    }
}
