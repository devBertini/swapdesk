//! Which helper does what (C2.1, D11): a pure rule, so it is tested without a process.
//!
//! * Input of a prompted session goes to the user helper (medium integrity: UIPI keeps it out of elevated windows and
//!   UAC); input of an `UNATTENDED` session goes to the system helper. A missing helper drops the input: it never
//!   falls back to the other one, since that would hand a prompted session the SYSTEM injector.
//! * Video, cursor, displays and audio come from the system helper (DDA capture, the desktop watcher); when only the
//!   user helper is attached (a development console run), it serves them.

use swapdesk_ipc::IpcRole;
use swapdesk_proto::PermissionSet;

/// The two helper processes of C2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperKind {
    System,
    User,
}

impl HelperKind {
    /// The kind of a helper `Hello.role`; `None` for the roles of the interface (the UI and its elevated instance).
    pub fn of_role(role: IpcRole) -> Option<Self> {
        match role {
            IpcRole::HelperSystem => Some(Self::System),
            IpcRole::HelperUser => Some(Self::User),
            IpcRole::Ui | IpcRole::Elevator => None,
        }
    }

    pub fn role(self) -> IpcRole {
        match self {
            Self::System => IpcRole::HelperSystem,
            Self::User => IpcRole::HelperUser,
        }
    }
}

/// The helper that injects the input of a session holding `granted` (D11): `UNATTENDED` means the system helper,
/// everything else the user helper. A prompted grant never holds `UNATTENDED`; a resumed unattended session keeps
/// it (`policy::resumed_grant`), so the grant is the right thing to ask, not the mode at the start.
pub fn input_helper(granted: PermissionSet) -> HelperKind {
    if granted.contains(PermissionSet::UNATTENDED) { HelperKind::System } else { HelperKind::User }
}

/// The helper that serves video, cursor, displays and audio: the system one, else the user one.
pub fn media_helper<T>(system: Option<T>, user: Option<T>) -> Option<(HelperKind, T)> {
    match (system, user) {
        (Some(s), _) => Some((HelperKind::System, s)),
        (None, Some(u)) => Some((HelperKind::User, u)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompted_session_injects_through_the_user_helper() {
        for granted in [
            PermissionSet::VIEW,
            PermissionSet::CONTROL,
            PermissionSet::CONTROL | PermissionSet::AUDIO,
            PermissionSet::CONTROL | PermissionSet::FILES | PermissionSet::TERMINAL,
        ] {
            assert_eq!(input_helper(granted), HelperKind::User, "{granted:?}");
        }
    }

    #[test]
    fn an_unattended_session_injects_through_the_system_helper() {
        for granted in [
            PermissionSet::UNATTENDED,
            PermissionSet::UNATTENDED | PermissionSet::CONTROL,
            PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::AUDIO,
        ] {
            assert_eq!(input_helper(granted), HelperKind::System, "{granted:?}");
        }
    }

    #[test]
    fn losing_unattended_moves_the_injection_back_to_the_user_helper() {
        let before = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
        let after = PermissionSet::CONTROL;
        assert_eq!((input_helper(before), input_helper(after)), (HelperKind::System, HelperKind::User));
    }

    #[test]
    fn media_comes_from_the_system_helper_and_falls_back_to_the_user_one() {
        assert_eq!(media_helper(Some(1), Some(2)), Some((HelperKind::System, 1)));
        assert_eq!(media_helper(Some(1), None), Some((HelperKind::System, 1)));
        assert_eq!(media_helper(None, Some(2)), Some((HelperKind::User, 2)));
        assert_eq!(media_helper::<u8>(None, None), None);
    }

    #[test]
    fn helper_roles_map_both_ways_and_the_ui_is_no_helper() {
        for kind in [HelperKind::System, HelperKind::User] {
            assert_eq!(HelperKind::of_role(kind.role()), Some(kind));
        }
        assert_eq!(HelperKind::of_role(IpcRole::Ui), None);
    }
}
