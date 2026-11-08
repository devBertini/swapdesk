use std::sync::{Arc, Mutex};

use swapdesk_platform::{Clock, DisplayInfo, InputSink, MonoMicros, Result};
use swapdesk_proto::InputEvent;

use crate::clock::MonotonicClock;
use crate::lock;

/// What the sink was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRecord {
    Event(InputEvent),
    ReleaseAll,
    DisplaysChanged(Vec<DisplayInfo>),
    FollowInputDesktop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub at: MonoMicros,
    pub what: InputRecord,
}

/// Stores every call with its time. Clones share the log, so a test keeps a handle after moving the sink.
#[derive(Clone)]
pub struct RecordingInputSink {
    clock: Arc<dyn Clock>,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl RecordingInputSink {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(MonotonicClock::new()))
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self { clock, log: Arc::default() }
    }

    fn push(&self, what: InputRecord) {
        let at = self.clock.now();
        lock(&self.log).push(Recorded { at, what });
    }

    /// Every call, in order.
    pub fn records(&self) -> Vec<Recorded> {
        lock(&self.log).clone()
    }

    /// Only the applied events, in order.
    pub fn events(&self) -> Vec<InputEvent> {
        lock(&self.log)
            .iter()
            .filter_map(|r| match &r.what {
                InputRecord::Event(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn clear(&self) {
        lock(&self.log).clear();
    }
}

impl Default for RecordingInputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink for RecordingInputSink {
    fn displays_changed(&mut self, displays: &[DisplayInfo]) {
        self.push(InputRecord::DisplaysChanged(displays.to_vec()));
    }

    fn apply(&mut self, event: &InputEvent) -> Result<()> {
        self.push(InputRecord::Event(event.clone()));
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        self.push(InputRecord::ReleaseAll);
        Ok(())
    }

    fn follow_input_desktop(&mut self) -> Result<()> {
        self.push(InputRecord::FollowInputDesktop);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeClock;
    use std::time::Duration;
    use swapdesk_proto::MouseButton;

    #[test]
    fn replays_a_script_and_compares() {
        let clock = FakeClock::new(MonoMicros(100));
        let mut sink = RecordingInputSink::with_clock(Arc::new(clock.clone()));
        let handle = sink.clone();
        let script = vec![
            InputEvent::Pointer { display: 0, x: 100, y: 200 },
            InputEvent::Button { button: MouseButton::Left, down: true },
            InputEvent::Key { usage_page: 0x07, usage: 0x04, down: true, modifiers: 0x02, locks: 0 },
            InputEvent::Text("olá".to_string()),
            InputEvent::Wheel { dx: 0, dy: -120 },
            InputEvent::Button { button: MouseButton::Left, down: false },
        ];
        for e in &script {
            clock.advance(Duration::from_millis(2));
            sink.apply(e).unwrap();
        }
        clock.advance(Duration::from_millis(2));
        sink.release_all().unwrap();
        sink.follow_input_desktop().unwrap();
        assert_eq!(handle.events(), script);
        let records = handle.records();
        assert_eq!(records.len(), 8);
        assert_eq!(records[0].at, MonoMicros(2_100));
        assert!(records.windows(2).all(|w| w[0].at <= w[1].at));
        assert_eq!(records[6].what, InputRecord::ReleaseAll);
        assert_eq!(records[7].what, InputRecord::FollowInputDesktop);
        handle.clear();
        assert!(handle.records().is_empty());
    }
}
