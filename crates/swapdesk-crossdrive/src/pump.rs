//! Waiting for the events and replies of a [`UiBackend`], and for a file the orchestrator writes.

use std::path::Path;
use std::time::{Duration, Instant};

use swapdesk_ui::{ApiError, Command, Reply, UiBackend, UiEvent};

const POLL: Duration = Duration::from_millis(20);

/// Everything the backend delivered, kept so a later wait can still see an earlier event.
pub struct Pump<'a> {
    backend: &'a dyn UiBackend,
    seen: Vec<UiEvent>,
}

impl<'a> Pump<'a> {
    pub fn new(backend: &'a dyn UiBackend) -> Self {
        Self { backend, seen: Vec::new() }
    }

    fn poll(&mut self) {
        self.seen.extend(self.backend.drain());
    }

    /// The events delivered since `cursor` (which starts at 0 and is advanced), for a loop that reacts to each one.
    pub fn fresh(&mut self, cursor: &mut usize) -> Vec<UiEvent> {
        self.poll();
        let out = self.seen[(*cursor).min(self.seen.len())..].to_vec();
        *cursor = self.seen.len();
        out
    }

    /// The first event `f` accepts, from what was already delivered or what arrives within `timeout`.
    pub fn wait<T>(
        &mut self,
        what: &str,
        timeout: Duration,
        mut f: impl FnMut(&UiEvent) -> Option<T>,
    ) -> Result<T, String> {
        let end = Instant::now() + timeout;
        loop {
            self.poll();
            if let Some(v) = self.seen.iter().find_map(&mut f) {
                return Ok(v);
            }
            if Instant::now() >= end {
                return Err(format!("timed out after {}s waiting for {what}", timeout.as_secs()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Sends `command` and waits for its reply. The outer error is a timeout; the inner one is the service's refusal.
    pub fn request(&mut self, command: Command, timeout: Duration) -> Result<Result<Reply, ApiError>, String> {
        let what = format!("the reply to {command:?}");
        let ticket = self.backend.send(command);
        self.wait(&what, timeout, |e| match e {
            UiEvent::Reply { ticket: t, result } if *t == ticket => Some(result.clone()),
            _ => None,
        })
    }

    /// As [`request`](Self::request), with a refusal turned into an error text.
    pub fn ask(&mut self, command: Command, timeout: Duration) -> Result<Reply, String> {
        let what = format!("{command:?}");
        self.request(command, timeout)?.map_err(|e| format!("{what} refused: {:?}", e.code))
    }
}

/// The trimmed text of `path`, once it exists and is not empty.
pub fn wait_file(path: &Path, what: &str, timeout: Duration) -> Result<String, String> {
    let end = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                return Ok(text);
            }
        }
        if Instant::now() >= end {
            return Err(format!("timed out after {}s waiting for {what} ({})", timeout.as_secs(), path.display()));
        }
        std::thread::sleep(POLL);
    }
}
