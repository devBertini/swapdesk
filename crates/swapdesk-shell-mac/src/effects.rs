//! What the shell does for each [`Effect`] the state asks for. Pure Rust: the table is checked on every platform, and
//! the shell (`shell.rs`) runs each [`Handling`] with AppKit. Neither match has a catch-all, so an effect without a
//! handler does not build.

use swapdesk_ui::{ControlId, EditId, Effect, SelectId};

/// The shell's answer to an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handling {
    /// The option menu of a select, under its control; the choice goes back through `UiState::choose`.
    Menu(SelectId),
    /// The keyboard goes to the native text field of the input.
    Focus(EditId),
    /// The focus goes to a painted control of the window (the one that was activated has gone).
    FocusControl(ControlId),
    /// The text of the native field goes through `UiState::submit_edit`.
    Submit(EditId),
    /// A save panel for the activity log, then `ActivityExport`.
    SaveLogPanel,
    /// The folder of the app's logs opens in the Finder.
    OpenLogsFolder,
    /// An open panel for the files to send, then `UiState::send_files`.
    ChooseFiles,
    /// An open panel for the folder to receive into, then `UiState::set_receive_dir`.
    ChooseFolder,
    /// The page of `UiState::take_web_page` opens in the browser.
    OpenWebPage,
    /// The explorer's local file, with the app macOS has for it (`/usr/bin/open`).
    OpenLocalFile,
    /// The explorer's context menu, at the pointer, from `UiState::context_items`; the choice goes back through
    /// `UiState::context_chosen`.
    ContextMenu,
    /// The explorer's local item, shown in the Finder (`/usr/bin/open -R`).
    RevealInFinder,
    /// The system drag of `UiState::take_system_drag` starts on the canvas (slice 5; a promise per file).
    SystemDrag,
}

impl Handling {
    /// It waits for a person (a menu or a panel runs its own event loop): it runs after the activation that asked
    /// for it was dispatched, with no borrow of the state held.
    pub fn waits_for_a_person(self) -> bool {
        match self {
            Handling::Menu(_)
            | Handling::ContextMenu
            | Handling::SaveLogPanel
            | Handling::ChooseFiles
            | Handling::ChooseFolder => true,
            Handling::Focus(_)
            | Handling::FocusControl(_)
            | Handling::Submit(_)
            | Handling::OpenLogsFolder
            | Handling::OpenWebPage
            | Handling::OpenLocalFile
            | Handling::RevealInFinder
            | Handling::SystemDrag => false,
        }
    }
}

/// The handler of `effect`.
pub fn handling(effect: Effect) -> Handling {
    match effect {
        Effect::Menu(select) => Handling::Menu(select),
        Effect::FocusEdit(edit) => Handling::Focus(edit),
        Effect::Focus(control) => Handling::FocusControl(control),
        Effect::SubmitEdit(edit) => Handling::Submit(edit),
        Effect::SaveLog => Handling::SaveLogPanel,
        Effect::OpenLogs => Handling::OpenLogsFolder,
        Effect::PickFiles => Handling::ChooseFiles,
        Effect::PickReceiveFolder => Handling::ChooseFolder,
        Effect::OpenWebPage => Handling::OpenWebPage,
        Effect::OpenLocal => Handling::OpenLocalFile,
        Effect::ContextMenu => Handling::ContextMenu,
        Effect::RevealLocal => Handling::RevealInFinder,
        Effect::SystemDrag => Handling::SystemDrag,
    }
}
