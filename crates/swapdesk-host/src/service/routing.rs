//! Which helper does what (C2.1, D11): a pure rule, so it is tested without a process.
//!
//! * Input of a prompted session goes to the user helper (medium integrity: UIPI keeps it out of elevated windows and
//!   UAC); input of an `UNATTENDED` session goes to the system helper. A missing helper drops the input: it never
//!   falls back to the other one, since that would hand a prompted session the SYSTEM injector.
//! * Video, cursor, displays and audio come from the system helper (DDA capture, the desktop watcher); when only the
//!   user helper is attached (a development console run), it serves them.

use swapdesk_ipc::IpcRole;
use swapdesk_proto::{Desktop, ErrorCode, PermissionSet, Special};
use swapdesk_session::ConsoleOwner;

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

/// Where the input of a session goes now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRoute {
    /// To this helper.
    Helper(HelperKind),
    /// Nowhere: the records are dropped.
    Drop,
}

/// Where the input of a session holding `granted` goes while the host is on `desktop` (D11, F-UNAT-01, PROTOCOL.md
/// `HostState`): nothing without `CONTROL`; nothing while another account is at the console; on the user's own desktop
/// the helper [`input_helper`] picks; on any other desktop (the secure desktop, the lock screen, the sign-in screen)
/// only the system helper and only for a session that holds `UNATTENDED`. The user helper never gets a record for a
/// desktop that is not `Default`: it runs at medium integrity and has no business there, and a session without
/// `UNATTENDED` is paused on such a desktop.
pub fn input_route(granted: PermissionSet, desktop: Desktop) -> InputRoute {
    if !granted.contains(PermissionSet::CONTROL) {
        return InputRoute::Drop;
    }
    match desktop {
        Desktop::AccountAway => InputRoute::Drop,
        Desktop::Default => InputRoute::Helper(input_helper(granted)),
        Desktop::Secure | Desktop::Locked | Desktop::LoginWindow => {
            if granted.contains(PermissionSet::UNATTENDED) {
                InputRoute::Helper(HelperKind::System)
            } else {
                InputRoute::Drop
            }
        }
    }
}

/// The desktop a host session is told it is on, from what the sources know (ACCOUNTS.md 5.1, F-UNAT-01):
/// `session` is what the session's lock events say (`Default` or `Locked`), `helper` what the media helper's probe of
/// the input desktop says (`Default` or `Secure`), `owner` who is at the console.
///
/// The more specific non-`Default` report wins, so a lock the session knows stays `Locked` while the helper only knows
/// "not the user's desktop", and a UAC prompt, which no session event announces, is `Secure`. Another account at the
/// console is `AccountAway` whatever the desktop; nobody at the console (the sign-in screen) is `Locked` unless a
/// protected desktop already says more.
pub fn effective_desktop(session: Desktop, helper: Desktop, owner: ConsoleOwner) -> Desktop {
    let desktop = if session != Desktop::Default { session } else { helper };
    match owner {
        ConsoleOwner::Other => Desktop::AccountAway,
        ConsoleOwner::Nobody if desktop == Desktop::Default => Desktop::Locked,
        ConsoleOwner::This | ConsoleOwner::Nobody => desktop,
    }
}

/// What the host does with a viewer's `SendSpecial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialVerdict {
    /// The service may send the key now.
    Send,
    /// Refused; the viewer is told with this code.
    Refuse(ErrorCode),
}

/// Whether a session holding `granted` may send `key` while the host is on `desktop` (D11): only `UNATTENDED` sessions
/// (`NOT_PERMITTED` otherwise), and never while another account is at the console, because the secure attention
/// sequence would land on that account's session (`ACCOUNT_AWAY`). The desktop does not matter otherwise: Ctrl+Alt+Del
/// is what opens the security screen from the user's desktop and what leads from the lock screen to the sign-in.
pub fn special_verdict(key: Special, granted: PermissionSet, desktop: Desktop) -> SpecialVerdict {
    match key {
        Special::SecureAttention => {
            if !granted.contains(PermissionSet::UNATTENDED) {
                SpecialVerdict::Refuse(ErrorCode::NotPermitted)
            } else if desktop == Desktop::AccountAway {
                SpecialVerdict::Refuse(ErrorCode::AccountAway)
            } else {
                SpecialVerdict::Send
            }
        }
    }
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

    const CONTROL: PermissionSet = PermissionSet::CONTROL;

    #[test]
    fn a_report_of_a_protected_desktop_reaches_the_sessions_of_the_console_owner() {
        let at = |session, helper, owner| effective_desktop(session, helper, owner);
        assert_eq!(at(Desktop::Default, Desktop::Default, ConsoleOwner::This), Desktop::Default);
        assert_eq!(at(Desktop::Default, Desktop::Secure, ConsoleOwner::This), Desktop::Secure, "a UAC prompt");
        assert_eq!(
            at(Desktop::Locked, Desktop::Secure, ConsoleOwner::This),
            Desktop::Locked,
            "the lock screen keeps its name"
        );
        assert_eq!(
            at(Desktop::Locked, Desktop::Default, ConsoleOwner::This),
            Desktop::Locked,
            "the lock event alone holds"
        );
        assert_eq!(at(Desktop::Default, Desktop::Secure, ConsoleOwner::Nobody), Desktop::Secure);
    }

    #[test]
    fn another_account_at_the_console_is_away_whatever_the_helper_sees() {
        for session in [Desktop::Default, Desktop::Locked, Desktop::Secure] {
            for helper in [Desktop::Default, Desktop::Secure] {
                assert_eq!(effective_desktop(session, helper, ConsoleOwner::Other), Desktop::AccountAway);
            }
        }
    }

    #[test]
    fn the_sign_in_screen_is_locked_unless_a_protected_desktop_says_more() {
        assert_eq!(effective_desktop(Desktop::Default, Desktop::Default, ConsoleOwner::Nobody), Desktop::Locked);
    }

    #[test]
    fn no_control_means_no_input_on_any_desktop() {
        for desktop in Desktop::ALL.iter().copied() {
            assert_eq!(input_route(PermissionSet::VIEW, desktop), InputRoute::Drop, "{desktop:?}");
            assert_eq!(input_route(PermissionSet::UNATTENDED, desktop), InputRoute::Drop, "{desktop:?}");
        }
    }

    #[test]
    fn on_the_users_desktop_the_grant_picks_the_helper() {
        assert_eq!(input_route(CONTROL, Desktop::Default), InputRoute::Helper(HelperKind::User));
        assert_eq!(
            input_route(CONTROL | PermissionSet::UNATTENDED, Desktop::Default),
            InputRoute::Helper(HelperKind::System)
        );
    }

    #[test]
    fn a_protected_desktop_gets_input_from_the_system_helper_and_only_for_unattended() {
        for desktop in [Desktop::Secure, Desktop::Locked, Desktop::LoginWindow] {
            assert_eq!(input_route(CONTROL, desktop), InputRoute::Drop, "{desktop:?}: a prompted session is paused");
            assert_eq!(
                input_route(CONTROL | PermissionSet::UNATTENDED, desktop),
                InputRoute::Helper(HelperKind::System),
                "{desktop:?}"
            );
        }
    }

    #[test]
    fn nothing_is_injected_while_another_account_is_at_the_console() {
        assert_eq!(input_route(CONTROL | PermissionSet::UNATTENDED, Desktop::AccountAway), InputRoute::Drop);
        assert_eq!(input_route(CONTROL, Desktop::AccountAway), InputRoute::Drop);
    }

    #[test]
    fn the_user_helper_never_gets_input_for_a_desktop_that_is_not_default() {
        for granted in [
            CONTROL,
            CONTROL | PermissionSet::UNATTENDED,
            CONTROL | PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::FILES,
        ] {
            for desktop in Desktop::ALL.iter().copied().filter(|d| *d != Desktop::Default) {
                assert_ne!(input_route(granted, desktop), InputRoute::Helper(HelperKind::User), "{desktop:?}");
            }
        }
    }

    #[test]
    fn ctrl_alt_del_needs_unattended() {
        for desktop in Desktop::ALL.iter().copied() {
            for granted in [PermissionSet::VIEW, CONTROL, CONTROL | PermissionSet::FILES | PermissionSet::TERMINAL] {
                assert_eq!(
                    special_verdict(Special::SecureAttention, granted, desktop),
                    SpecialVerdict::Refuse(ErrorCode::NotPermitted),
                    "{granted:?} on {desktop:?}"
                );
            }
        }
    }

    #[test]
    fn ctrl_alt_del_goes_on_every_desktop_but_one_that_belongs_to_another_account() {
        let granted = PermissionSet::UNATTENDED | CONTROL;
        for desktop in [Desktop::Default, Desktop::Secure, Desktop::Locked, Desktop::LoginWindow] {
            assert_eq!(
                special_verdict(Special::SecureAttention, granted, desktop),
                SpecialVerdict::Send,
                "{desktop:?}"
            );
        }
        assert_eq!(
            special_verdict(Special::SecureAttention, granted, Desktop::AccountAway),
            SpecialVerdict::Refuse(ErrorCode::AccountAway)
        );
    }

    #[test]
    fn an_account_away_session_without_unattended_is_told_not_permitted_first() {
        assert_eq!(
            special_verdict(Special::SecureAttention, CONTROL, Desktop::AccountAway),
            SpecialVerdict::Refuse(ErrorCode::NotPermitted)
        );
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
