//! Keep the machine awake during a session (C3 `ActionRunner::keep_awake`).
//!
//! `SetThreadExecutionState(ES_CONTINUOUS | ...)` belongs to the **thread** that calls it: the state lasts until that thread
//! calls it again or ends. So one dedicated thread owns the state and serves on/off requests, whichever thread the caller
//! is on. While on, the system and the display stay awake (`ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use swapdesk_platform::{ErrorKind, PlatformError, Result};
use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};

use crate::sys::last_error;

enum Cmd {
    Set(bool, Sender<Result<()>>),
    Quit,
}

/// The thread that owns the execution state. Dropping it clears the state and ends the thread.
pub struct KeepAwake {
    tx: Sender<Cmd>,
    on: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn gone() -> PlatformError {
    PlatformError {
        kind: ErrorKind::Closed,
        op: "KeepAwake",
        os_code: None,
        detail: "the keep-awake thread ended".into(),
    }
}

impl KeepAwake {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let on = Arc::new(AtomicBool::new(false));
        let on_thread = Arc::clone(&on);
        let thread = std::thread::Builder::new()
            .name("swapdesk-keep-awake".into())
            .spawn(move || {
                while let Ok(Cmd::Set(want, reply)) = rx.recv() {
                    let flags =
                        if want { ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED } else { ES_CONTINUOUS };
                    // SAFETY: no preconditions; the state applies to this thread, which lives until the loop ends.
                    let previous = unsafe { SetThreadExecutionState(flags) };
                    let result = if previous == 0 { Err(last_error("SetThreadExecutionState")) } else { Ok(()) };
                    if result.is_ok() {
                        on_thread.store(want, Ordering::SeqCst);
                    }
                    let _ = reply.send(result);
                }
                // Quit, or every sender is gone: release the request before the thread ends.
                // SAFETY: no preconditions.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                on_thread.store(false, Ordering::SeqCst);
            })
            .map_err(|e| PlatformError {
                kind: ErrorKind::Os,
                op: "KeepAwake::new",
                os_code: e.raw_os_error().map(i64::from),
                detail: e.to_string(),
            })?;
        Ok(Self { tx, on, thread: Some(thread) })
    }

    /// Turns the request on or off; `Ok` once the OS accepted it.
    pub fn set(&self, on: bool) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx.send(Cmd::Set(on, reply_tx)).map_err(|_| gone())?;
        reply_rx.recv().map_err(|_| gone())?
    }

    /// Whether the last accepted request was "on".
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
