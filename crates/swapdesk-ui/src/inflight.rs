//! R-UI-3: a backend merges identical commands that are still in flight into one.
//!
//! Only idempotent commands merge (reads and "set to this value"): two chat messages with the same text are two
//! messages. A merged `send` returns the first caller's ticket, so one reply answers both.

use std::sync::Mutex;

use crate::api::{Command, Ticket};

#[derive(Default)]
pub struct InFlight {
    list: Mutex<Vec<(Command, Ticket)>>,
}

/// What `join` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Joined {
    /// An identical command is in flight: its ticket stands for this one too.
    Merged(Ticket),
    /// A new command: the caller runs it and calls `finish` when it is answered.
    Fresh,
}

impl InFlight {
    pub fn new() -> InFlight {
        InFlight::default()
    }

    /// Registers `command` under `ticket`, or reports the identical one already running.
    pub fn join(&self, command: &Command, ticket: Ticket) -> Joined {
        let Ok(mut list) = self.list.lock() else { return Joined::Fresh };
        if command.is_idempotent()
            && let Some((_, t)) = list.iter().find(|(c, _)| c == command)
        {
            return Joined::Merged(*t);
        }
        list.push((command.clone(), ticket));
        Joined::Fresh
    }

    /// The command of `ticket` was answered.
    pub fn finish(&self, ticket: Ticket) {
        if let Ok(mut list) = self.list.lock() {
            list.retain(|(_, t)| *t != ticket);
        }
    }

    pub fn len(&self) -> usize {
        self.list.lock().map_or(0, |l| l.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_reads_merge_until_answered() {
        let f = InFlight::new();
        assert_eq!(f.join(&Command::ListDevices, Ticket(1)), Joined::Fresh);
        assert_eq!(f.join(&Command::ListDevices, Ticket(2)), Joined::Merged(Ticket(1)));
        assert_eq!(f.join(&Command::GetSettings, Ticket(3)), Joined::Fresh);
        assert_eq!(f.len(), 2);
        f.finish(Ticket(1));
        assert_eq!(f.join(&Command::ListDevices, Ticket(4)), Joined::Fresh);
    }

    #[test]
    fn chat_messages_never_merge() {
        let f = InFlight::new();
        let c = Command::ChatSend { session_id: 1, text: "ok".into() };
        assert_eq!(f.join(&c, Ticket(1)), Joined::Fresh);
        assert_eq!(f.join(&c, Ticket(2)), Joined::Fresh);
    }
}
