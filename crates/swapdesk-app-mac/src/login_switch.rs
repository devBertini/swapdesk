//! "Abrir ao ligar" on the Mac: the switch of the settings screen and the app's login item (D32) say the same thing.
//!
//! The system's answer is the truth. At a normal start the switch shows whether the login item is registered. The
//! first start registers it, because the app hosts from inside its own process and must come back after a login
//! (D32); after that the person's choice stands: off stays off, on is registered again if the system dropped it.
//! While the app runs, a settings change that flips the switch registers or unregisters the item.
//!
//! Pure Rust, tested on every host; `login_item` holds the calls to `SMAppService`.

use std::sync::{Arc, Mutex, PoisonError};

use swapdesk_ui::{Command, Ticket, UiBackend, UiEvent};

/// What a normal start does with the login item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtStart {
    Register,
    Leave,
}

/// `first_run`: no per-user record existed before this start. `enabled`: the system has the login item registered.
/// `wanted`: the switch as the per-user record holds it.
pub fn at_start(first_run: bool, enabled: bool, wanted: bool) -> AtStart {
    if !enabled && (first_run || wanted) { AtStart::Register } else { AtStart::Leave }
}

/// The backend the window talks to on a normal run: every command goes on unchanged, and a settings change that
/// flips "Abrir ao ligar" is handed to `apply` (which registers or unregisters the login item off the UI thread).
pub struct Following {
    inner: Arc<dyn UiBackend>,
    /// The switch as last applied.
    on: Mutex<bool>,
    apply: Box<dyn Fn(bool) + Send + Sync>,
}

impl Following {
    /// `on` is the switch as the start left it.
    pub fn new(inner: Arc<dyn UiBackend>, on: bool, apply: Box<dyn Fn(bool) + Send + Sync>) -> Self {
        Self { inner, on: Mutex::new(on), apply }
    }
}

impl UiBackend for Following {
    fn send(&self, command: Command) -> Ticket {
        if let Command::SetSettings { settings } = &command {
            let mut on = self.on.lock().unwrap_or_else(PoisonError::into_inner);
            if *on != settings.open_at_login {
                *on = settings.open_at_login;
                (self.apply)(*on);
            }
        }
        self.inner.send(command)
    }

    fn drain(&self) -> Vec<UiEvent> {
        self.inner.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_ui::mock::Scenario;
    use swapdesk_ui::{ControlId, UiState};

    #[test]
    fn the_first_start_registers_and_later_the_persons_choice_stands() {
        // D32: a first start registers the app, whatever the record's default says.
        assert_eq!(at_start(true, false, false), AtStart::Register);
        // Registered already (this start or a 1.0 that always registered): nothing to do.
        assert_eq!(at_start(false, true, false), AtStart::Leave);
        assert_eq!(at_start(true, true, false), AtStart::Leave);
        assert_eq!(at_start(false, true, true), AtStart::Leave);
        // Turned off by the person: it stays off.
        assert_eq!(at_start(false, false, false), AtStart::Leave);
        // Wanted, but the system dropped it (a rebuilt app): registered again.
        assert_eq!(at_start(false, false, true), AtStart::Register);
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Command>>);

    impl UiBackend for Recorder {
        fn send(&self, command: Command) -> Ticket {
            self.0.lock().unwrap().push(command);
            Ticket(1)
        }
        fn drain(&self) -> Vec<UiEvent> {
            Vec::new()
        }
    }

    #[test]
    fn flipping_the_switch_in_the_window_is_applied_once_and_other_settings_are_not() {
        let inner = Arc::new(Recorder::default());
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&applied);
        let mut state: UiState = Scenario::Ajustes.prepared_state();
        let start = state.settings().expect("settings").open_at_login;
        let backend = Following::new(inner.clone(), start, Box::new(move |on| seen.lock().unwrap().push(on)));
        let send = |state: &mut UiState, id: ControlId| {
            for c in state.activate(id).commands {
                backend.send(c);
            }
        };

        // Another switch of the same screen: the command goes on, the login item is left alone.
        send(&mut state, ControlId::ShowStats);
        assert!(applied.lock().unwrap().is_empty());
        assert_eq!(inner.0.lock().unwrap().len(), 1, "the command reached the service");

        send(&mut state, ControlId::OpenAtLogin);
        assert_eq!(*applied.lock().unwrap(), vec![!start]);
        send(&mut state, ControlId::ShowStats);
        assert_eq!(*applied.lock().unwrap(), vec![!start], "an unrelated change does not apply it again");
        send(&mut state, ControlId::OpenAtLogin);
        assert_eq!(*applied.lock().unwrap(), vec![!start, start]);
        assert_eq!(inner.0.lock().unwrap().len(), 4, "every command reached the service");
    }
}
