//! What the phone's shell does for each [`Effect`] the state asks for. The match has no catch-all, so an effect the
//! shared state gains does not build here until it has an answer (the Mac's `effects.rs` is the same table for
//! AppKit).

use swapdesk_ui::{ControlId, EditId, Effect, SelectId};

/// The phone's answer to an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handling {
    /// The choices of a select as a list over the screen; the answer goes back through `UiState::choose`.
    Menu(SelectId),
    /// The keyboard goes to the native text field of the input.
    Focus(EditId),
    /// The focus goes to a painted control (the one that was activated has gone).
    FocusControl(ControlId),
    /// The text of the native field goes through `UiState::submit_edit`.
    Submit(EditId),
    /// The page of `UiState::take_web_page` opens in the phone's browser.
    OpenWebPage,
    /// A later slice of the Android plan builds it (`docs/ANDROID.md` 3); until then the app says so in its log
    /// instead of doing nothing in silence.
    Later(&'static str),
    /// A notice only a computer shows asks for it; a phone never gets that notice.
    NotOnAPhone(&'static str),
}

/// The handler of `effect`.
pub fn handling(effect: Effect) -> Handling {
    match effect {
        Effect::Menu(select) => Handling::Menu(select),
        Effect::FocusEdit(edit) => Handling::Focus(edit),
        Effect::Focus(control) => Handling::FocusControl(control),
        Effect::SubmitEdit(edit) => Handling::Submit(edit),
        Effect::OpenWebPage => Handling::OpenWebPage,
        Effect::SaveLog => Handling::Later("A7: the activity log is exported through the system's document picker"),
        Effect::OpenLogs => Handling::Later("A7: the logs are exported, a phone shows no folder of an app"),
        Effect::ShowViewer => Handling::Later("A4: the session's screen comes back to the front"),
        Effect::ShowMain => Handling::Later("A14: the conversation of a phone that is controlled"),
        Effect::OpenLocal => Handling::Later("A12: a file of the phone opens with the app Android has for it"),
        Effect::RevealLocal => Handling::Later("A12: a file of the phone is shown in the system's file app"),
        Effect::ContextMenu => Handling::Later("A12: the explorer's menu opens on a long press"),
        Effect::SystemDrag => Handling::NotOnAPhone("a drag out of the window"),
        Effect::OpenLoginItems => Handling::NotOnAPhone("the Mac's Login Items"),
        Effect::RequestOsPermissions => Handling::NotOnAPhone("the Mac's privacy permissions"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_page_of_the_phone_can_ask_for_has_an_answer_now() {
        assert_eq!(handling(Effect::Menu(SelectId::Theme)), Handling::Menu(SelectId::Theme));
        assert_eq!(handling(Effect::FocusEdit(EditId::Address)), Handling::Focus(EditId::Address));
        assert_eq!(handling(Effect::SubmitEdit(EditId::Rename)), Handling::Submit(EditId::Rename));
        assert_eq!(handling(Effect::Focus(ControlId::Control)), Handling::FocusControl(ControlId::Control));
        assert_eq!(handling(Effect::OpenWebPage), Handling::OpenWebPage);
        // What waits for its slice says which one.
        for later in [Effect::SaveLog, Effect::OpenLogs, Effect::ShowViewer, Effect::ShowMain, Effect::ContextMenu] {
            assert!(matches!(handling(later.clone()), Handling::Later(why) if why.starts_with('A')), "{later:?}");
        }
    }
}
