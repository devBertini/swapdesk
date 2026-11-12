//! "Abrir ao ligar" on Windows: the switch of the settings screen and the per-user Run key say the same thing.
//!
//! At a normal start the Run key is made to match the per-user record's switch (the installed copy only: see
//! `swapdesk_sys_win::login`); while the app runs, a settings change that flips the switch is handed to `apply`.
//! Pure Rust, no OS calls: the caller supplies `apply`.

use std::sync::{Arc, Mutex, PoisonError};

use swapdesk_ui::{Command, Ticket, UiBackend, UiEvent};

/// The backend the window talks to: every command goes on unchanged, and a settings change that flips
/// "Abrir ao ligar" is handed to `apply` (which writes or removes the Run value off the UI thread).
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
