//! The UI's end of the clipboard calls of C2.4 (messages 0x60 to 0x63): the service asks, over the UI pipe, for the
//! clipboard of the user this UI runs as (the viewer role of a session), and this answers with the real provider.
//!
//! [`answer_clipboard`] is one message's step, also used by the user helper's loop (host role). The provider runs on
//! its own thread so a slow clipboard never holds the pipe's reader.

use swapdesk_ipc::{ClipData, ClipFail, ClipReplyBody, Message};
use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider, ErrorKind, PlatformError};

fn clip_fail(e: &PlatformError) -> ClipReplyBody {
    ClipReplyBody::Failed(match e.kind {
        ErrorKind::Unavailable | ErrorKind::Timeout | ErrorKind::AccessLost => ClipFail::Unavailable,
        ErrorKind::InvalidInput => ClipFail::Invalid,
        _ => ClipFail::Other,
    })
}

/// The server step of the clipboard trio: `Some(reply)` for 0x60, 0x61 and 0x62, `None` for any other message.
pub fn answer_clipboard<P: ClipboardProvider + ?Sized>(provider: &mut P, msg: &Message) -> Option<Message> {
    let (req, body) = match msg {
        Message::ClipChangeCount { req } => {
            (*req, provider.change_count().map_or_else(|e| clip_fail(&e), ClipReplyBody::Count))
        }
        Message::ClipRead { req } => (
            *req,
            match provider.read() {
                Ok(ClipRead::Content(ClipContent::Text(t))) => ClipReplyBody::Content(ClipData::Text(t)),
                Ok(ClipRead::Content(ClipContent::Png(p))) => ClipReplyBody::Content(ClipData::Png(p)),
                Ok(ClipRead::Concealed) => ClipReplyBody::Concealed,
                Ok(ClipRead::Empty) => ClipReplyBody::Empty,
                Ok(ClipRead::Unsupported) => ClipReplyBody::Unsupported,
                Err(e) => clip_fail(&e),
            },
        ),
        Message::ClipWrite { req, content } => {
            let content = match content {
                ClipData::Text(t) => ClipContent::Text(t.clone()),
                ClipData::Png(p) => ClipContent::Png(p.clone()),
            };
            (*req, provider.write(&content).map_or_else(|e| clip_fail(&e), |()| ClipReplyBody::Written))
        }
        _ => return None,
    };
    Some(Message::ClipReply { req, body })
}

/// The answer of a process that has no clipboard to give.
pub fn no_clipboard(msg: &Message) -> Option<Message> {
    let req = match msg {
        Message::ClipChangeCount { req } | Message::ClipRead { req } | Message::ClipWrite { req, .. } => *req,
        _ => return None,
    };
    Some(Message::ClipReply { req, body: ClipReplyBody::Failed(ClipFail::Other) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_platform::Result;

    struct Mem {
        count: u64,
        content: Option<ClipContent>,
    }

    impl ClipboardProvider for Mem {
        fn change_count(&mut self) -> Result<u64> {
            Ok(self.count)
        }
        fn read(&mut self) -> Result<ClipRead> {
            Ok(self.content.clone().map_or(ClipRead::Empty, ClipRead::Content))
        }
        fn write(&mut self, content: &ClipContent) -> Result<()> {
            self.count += 1;
            self.content = Some(content.clone());
            Ok(())
        }
    }

    #[test]
    fn the_trio_is_answered_and_other_messages_are_not() {
        let mut m = Mem { count: 4, content: None };
        let r = |req| answer_clipboard(&mut Mem { count: 4, content: None }, &Message::ClipRead { req });
        assert_eq!(r(1), Some(Message::ClipReply { req: 1, body: ClipReplyBody::Empty }));
        let w = Message::ClipWrite { req: 2, content: ClipData::Text("a".into()) };
        assert_eq!(answer_clipboard(&mut m, &w), Some(Message::ClipReply { req: 2, body: ClipReplyBody::Written }));
        assert_eq!(
            answer_clipboard(&mut m, &Message::ClipChangeCount { req: 3 }),
            Some(Message::ClipReply { req: 3, body: ClipReplyBody::Count(5) })
        );
        assert_eq!(
            answer_clipboard(&mut m, &Message::ClipRead { req: 4 }),
            Some(Message::ClipReply { req: 4, body: ClipReplyBody::Content(ClipData::Text("a".into())) })
        );
        assert_eq!(answer_clipboard(&mut m, &Message::Shutdown), None);
        assert_eq!(no_clipboard(&w), Some(Message::ClipReply { req: 2, body: ClipReplyBody::Failed(ClipFail::Other) }));
    }
}
