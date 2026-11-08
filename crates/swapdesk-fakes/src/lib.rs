//! Fakes of contract C7: deterministic stand-ins for the platform traits of `swapdesk-platform` and the synthetic
//! sources that the budgets B1 to B3 are measured with. No OS, no `unsafe`.
//!
//! Fakes that tests share between threads are cheap handles (`Clone` shares the state): the test keeps one handle
//! and gives the other to the code under test.

mod audio;
mod barcode;
mod clipboard;
mod clock;
mod input;
mod logs;
mod net;
mod pty;
mod screen;
mod secrets;
mod session;

pub use audio::{MarkerLog, Onset, RecordingAudioSink, SineSource};
pub use barcode::{BARCODE_CELL, BarcodeReader, BarcodeReading, draw_barcode};
pub use clipboard::FakeClipboard;
pub use clock::{FakeClock, MonotonicClock};
pub use input::{InputRecord, Recorded, RecordingInputSink};
pub use logs::{CapturedLine, LogCapture, Violation};
pub use net::FakeNetInterfaces;
pub use pty::{FAKE_USER, FakePtySpawner};
pub use screen::{Pacing, SyntheticScreen};
pub use secrets::MemorySecretStore;
pub use session::{FakeActions, FakeSessionWatcher};

use std::sync::{Mutex, MutexGuard};

/// Lock that ignores poisoning: a fake must stay usable after a test thread panicked.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
