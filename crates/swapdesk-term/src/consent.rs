//! D13: in an unattended session, the terminal (and files) of a user other than the one who granted `UNATTENDED`
//! ask that user first; nobody present is a refusal. Screen and control never ask.

use swapdesk_proto::{ErrorCode, Feature};

/// What the decision needs. Accounts are compared as the same kind of identifier (SIDs on Windows, uids on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsentFacts<'a> {
    /// The session entered without a prompt (`GrantMode::Unattended`, or a resume of one).
    pub session_unattended: bool,
    /// The account of the local administrator who granted `UNATTENDED` to this device (the trust store, C5).
    pub granted_by: Option<&'a str>,
    /// The user logged in at the host's console now; `None` at the login screen.
    pub console_user: Option<&'a str>,
    /// An administrator terminal (a SYSTEM or root shell outside any user's session) is being opened.
    pub admin_terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentNeed {
    /// Go ahead.
    NotNeeded,
    /// Ask the logged-in user (`ConsentPending`, the card of F-UI-01) and wait for the answer.
    Ask,
    /// Refuse with this code.
    Refuse(ErrorCode),
}

/// Whether opening the terminal needs the logged-in user's yes.
pub fn consent_need(f: &ConsentFacts<'_>) -> ConsentNeed {
    if !f.session_unattended || f.admin_terminal {
        // A prompted session had its user's say already; the administrator terminal holds no user's data.
        return ConsentNeed::NotNeeded;
    }
    match (f.console_user, f.granted_by) {
        (None, _) => ConsentNeed::Refuse(ErrorCode::NobodyPresent),
        (Some(user), Some(granter)) if user == granter => ConsentNeed::NotNeeded,
        // Another user is logged in, or the granter is unknown: ask, never assume.
        (Some(_), _) => ConsentNeed::Ask,
    }
}

/// Puts the question to the logged-in user. The service's implementation sends `ConsentPending`, shows the card on the
/// host's UI and waits up to the consent timeout; it answers `Ok` for yes, or `NobodyPresent`, `Declined`, `Timeout`.
pub trait ConsentAsker {
    fn ask(&mut self, feature: Feature) -> Result<(), ErrorCode>;
}

/// The D13 step for `feature`: decides, asks when needed, and returns the refusal code if the terminal must not open.
pub fn run_consent(facts: &ConsentFacts<'_>, feature: Feature, asker: &mut dyn ConsentAsker) -> Result<(), ErrorCode> {
    match consent_need(facts) {
        ConsentNeed::NotNeeded => Ok(()),
        ConsentNeed::Refuse(code) => Err(code),
        ConsentNeed::Ask => asker.ask(feature),
    }
}
