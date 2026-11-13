//! The helper loop's clipboard and quick-action calls (stage 42): a service-side driver talks to `run_helper` over the
//! in-memory duplex pipe, with `FakeClipboard` and `FakeActions` behind the factories.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_fakes::{FakeActions, FakeClipboard, FakeClock, Pacing, RecordingInputSink, SineSource, SyntheticScreen};
use swapdesk_ipc::{ClipData, ClipFail, ClipReplyBody, FrameReader, FrameWriter, Handshake, IpcRole, Message};
use swapdesk_platform::{ClipContent, ErrorKind, MonoMicros, PlatformError};
use swapdesk_proto::{Action, ErrorCode};

use crate::helper::{HelperDeps, HelperError, HelperStats, run_helper};
use crate::helper_tests::Passthrough;
use crate::testpipe::{PipeWriter, duplex};

const WAIT: Duration = Duration::from_secs(10);

type Deps = HelperDeps<SyntheticScreen, Passthrough, RecordingInputSink, SineSource>;

fn base_deps() -> Deps {
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let audio_clock = Arc::clone(&clock);
    HelperDeps::new(
        |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
        |_frame, _config, _backend| Ok(Passthrough { n: 0, size: None }),
        || Ok(RecordingInputSink::new()),
        move || Ok(SineSource::new(audio_clock.clone()).realtime()),
        clock,
    )
}

struct Rig {
    writer: Option<FrameWriter<PipeWriter>>,
    inbox: Receiver<Message>,
    helper: Option<JoinHandle<Result<HelperStats, HelperError>>>,
}

fn rig(deps: Deps) -> Rig {
    let (helper_end, service_end) = duplex();
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, deps, IpcRole::HelperUser, 4242));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    Handshake::server(&mut reader, &mut writer, 3).expect("server handshake");
    let (tx, inbox) = channel();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = reader.read_message() {
            if tx.send(m).is_err() {
                break;
            }
        }
    });
    Rig { writer: Some(writer), inbox, helper: Some(helper) }
}

impl Rig {
    /// Sends `msg` and returns the answer carrying its `req` (faults and the display list are skipped).
    fn ask(&mut self, msg: Message) -> Message {
        self.writer.as_mut().expect("open").write_message(&msg).expect("written");
        loop {
            if let m @ (Message::ClipReply { .. } | Message::ActionReply { .. }) =
                self.inbox.recv_timeout(WAIT).expect("an answer")
            {
                return m;
            }
        }
    }

    fn finish(&mut self) {
        self.writer = None;
        self.helper.take().expect("running").join().expect("helper thread").expect("a clean end");
    }
}

fn reply(req: u32, body: ClipReplyBody) -> Message {
    Message::ClipReply { req, body }
}

#[test]
fn the_clipboard_trio_is_answered_from_the_user_clipboard_and_concealed_content_is_flagged() {
    let clip = FakeClipboard::new();
    let mut r = rig(base_deps().with_clipboard({
        let clip = clip.clone();
        move || Ok(clip.clone())
    }));
    assert_eq!(r.ask(Message::ClipChangeCount { req: 1 }), reply(1, ClipReplyBody::Count(0)));
    assert_eq!(r.ask(Message::ClipRead { req: 2 }), reply(2, ClipReplyBody::Empty));

    clip.copy_externally(ClipContent::Text("olá".into()), false);
    assert_eq!(r.ask(Message::ClipChangeCount { req: 3 }), reply(3, ClipReplyBody::Count(1)));
    assert_eq!(r.ask(Message::ClipRead { req: 4 }), reply(4, ClipReplyBody::Content(ClipData::Text("olá".into()))));

    clip.copy_externally(ClipContent::Text("hunter2".into()), true);
    assert_eq!(
        r.ask(Message::ClipRead { req: 5 }),
        reply(5, ClipReplyBody::Concealed),
        "the secret never leaves the helper"
    );
    clip.copy_unsupported();
    assert_eq!(r.ask(Message::ClipRead { req: 6 }), reply(6, ClipReplyBody::Unsupported));

    assert_eq!(
        r.ask(Message::ClipWrite { req: 7, content: ClipData::Png(vec![1, 2, 3]) }),
        reply(7, ClipReplyBody::Written)
    );
    assert_eq!(clip.writes(), vec![ClipContent::Png(vec![1, 2, 3])]);
    // The answers come in the order of the calls, one at a time.
    assert_eq!(r.ask(Message::ClipRead { req: 8 }), reply(8, ClipReplyBody::Content(ClipData::Png(vec![1, 2, 3]))));
    r.finish();
}

#[test]
fn without_a_clipboard_or_when_it_cannot_be_built_the_calls_fail_and_the_loop_goes_on() {
    let mut none = rig(base_deps());
    assert_eq!(none.ask(Message::ClipRead { req: 1 }), reply(1, ClipReplyBody::Failed(ClipFail::Other)));
    assert_eq!(
        none.ask(Message::RunAction { req: 2, action: Action::Lock }),
        Message::ActionReply { req: 2, code: ErrorCode::Unavailable }
    );
    none.finish();

    let mut broken = rig(base_deps().with_clipboard(|| -> Result<FakeClipboard, PlatformError> {
        Err(PlatformError {
            kind: ErrorKind::Unavailable,
            op: "clipboard",
            os_code: None,
            detail: "no window station".into(),
        })
    }));
    assert_eq!(broken.ask(Message::ClipChangeCount { req: 1 }), reply(1, ClipReplyBody::Failed(ClipFail::Other)));
    assert_eq!(
        broken.ask(Message::ClipChangeCount { req: 2 }),
        reply(2, ClipReplyBody::Failed(ClipFail::Other)),
        "it is built again at the next call"
    );
    broken.finish();
}

#[test]
fn actions_run_as_the_user_with_the_refusal_code_and_keep_awake_is_recorded() {
    let actions = FakeActions::new();
    actions.refuse(Action::Restart, ErrorKind::Unsupported);
    actions.refuse(Action::Sleep, ErrorKind::Unavailable);
    let mut r = rig(base_deps().with_actions({
        let actions = actions.clone();
        move || Ok(actions.clone())
    }));
    assert_eq!(
        r.ask(Message::RunAction { req: 1, action: Action::Lock }),
        Message::ActionReply { req: 1, code: ErrorCode::NoError }
    );
    assert_eq!(
        r.ask(Message::RunAction { req: 2, action: Action::TaskMonitor }),
        Message::ActionReply { req: 2, code: ErrorCode::NoError }
    );
    assert_eq!(
        r.ask(Message::RunAction { req: 3, action: Action::Restart }),
        Message::ActionReply { req: 3, code: ErrorCode::NotPermitted }
    );
    assert_eq!(
        r.ask(Message::RunAction { req: 4, action: Action::Sleep }),
        Message::ActionReply { req: 4, code: ErrorCode::Unavailable }
    );
    assert_eq!(
        r.ask(Message::KeepAwake { req: 5, on: true }),
        Message::ActionReply { req: 5, code: ErrorCode::NoError }
    );
    assert_eq!(
        r.ask(Message::KeepAwake { req: 6, on: false }),
        Message::ActionReply { req: 6, code: ErrorCode::NoError }
    );
    assert_eq!(actions.ran(), vec![Action::Lock, Action::TaskMonitor]);
    assert_eq!(actions.awake_calls(), vec![true, false]);
    r.finish();
}
