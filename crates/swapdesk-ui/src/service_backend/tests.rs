//! `ServiceBackend` against a fake service on an in-memory duplex.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_ipc::{Bytes, FrameReader, FrameWriter, IPC_VERSION, IpcRole, Message, Notify, ReplyBody, UiOp};
use swapdesk_proto::{Codec, CursorPosition, InputEvent, KeyframeRequest, ReceiverReport, VideoHeader};

use super::convert as cv;
use super::*;
use crate::api::*;
use crate::state::{Screen, UiState};

const WAIT: Duration = Duration::from_secs(5);

// ---------- in-memory duplex ----------

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv_timeout(Duration::from_secs(10)) {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

impl Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.tx.send(data.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn pipe() -> (PipeReader, PipeWriter) {
    let (tx, rx) = mpsc::channel();
    (PipeReader { rx, buf: Vec::new(), pos: 0 }, PipeWriter { tx })
}

#[derive(Default)]
struct CountWaker(AtomicUsize);

impl UiWaker for CountWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct RecordingSink {
    video: Mutex<Vec<(u64, VideoHeader, Vec<u8>)>>,
    audio: Mutex<Vec<(u64, Vec<u8>)>>,
    cursor: Mutex<Vec<(u64, CursorPosition)>>,
}

impl ViewerSink for RecordingSink {
    fn video(&self, session_id: u64, header: VideoHeader, payload: Vec<u8>) {
        self.video.lock().unwrap().push((session_id, header, payload));
    }
    fn audio(&self, session_id: u64, datagram: Vec<u8>) {
        self.audio.lock().unwrap().push((session_id, datagram));
    }
    fn cursor_pos(&self, session_id: u64, pos: CursorPosition) {
        self.cursor.lock().unwrap().push((session_id, pos));
    }
    fn cursor_shape(&self, _session_id: u64, _shape: swapdesk_proto::CursorShape) {}
}

/// The service end: reads what the UI writes, writes what the UI reads. Dropping it closes the pipe.
struct Fake {
    from_ui: FrameReader<PipeReader>,
    to_ui: FrameWriter<PipeWriter>,
}

impl Fake {
    fn connect_with(sink: Option<Arc<dyn ViewerSink>>) -> (ServiceBackend, Fake, Arc<CountWaker>) {
        let (ui_in, svc_out) = pipe();
        let (svc_in, ui_out) = pipe();
        let mut to_ui = FrameWriter::new(svc_out);
        // The pipe is buffered, so the service can answer `Hello` before it is read: `start` finds the ack waiting.
        to_ui.write_message(&Message::HelloAck { version: IPC_VERSION, win_session: 3 }).unwrap();
        let waker = Arc::new(CountWaker::default());
        let backend = ServiceBackend::start_with_sink(ui_in, ui_out, waker.clone(), 4242, sink).unwrap();
        let mut from_ui = FrameReader::new(svc_in);
        assert_eq!(
            from_ui.read_message().unwrap(),
            Some(Message::Hello { role: IpcRole::Ui, version: IPC_VERSION, pid: 4242 })
        );
        assert_eq!(backend.win_session(), 3);
        (backend, Fake { from_ui, to_ui }, waker)
    }

    fn connect() -> (ServiceBackend, Fake, Arc<CountWaker>) {
        Fake::connect_with(None)
    }

    fn read(&mut self) -> Message {
        self.from_ui.read_message().unwrap().expect("the UI closed the pipe")
    }

    fn request(&mut self) -> (u32, UiOp) {
        match self.read() {
            Message::UiRequest { req, op } => (req, op),
            other => panic!("expected UiRequest, got {other:?}"),
        }
    }

    fn write(&mut self, m: Message) {
        self.to_ui.write_message(&m).unwrap();
    }

    fn reply(&mut self, req: u32, body: ReplyBody) {
        self.write(Message::UiReply { req, status: 0, body: Bytes(body.encode().unwrap()) });
    }

    fn fail(&mut self, req: u32, code: swapdesk_ipc::ApiCode) {
        self.write(Message::UiReply { req, status: code.status(), body: Bytes::default() });
    }
}

fn collect(b: &ServiceBackend, n: usize) -> Vec<UiEvent> {
    let end = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < n && Instant::now() < end {
        got.extend(b.drain());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(got.len(), n, "events: {got:?}");
    got
}

fn device(id: &str) -> DeviceView {
    DeviceView {
        id: id.into(),
        name: format!("Device {id}"),
        os: Os::MacOs,
        os_version: "14.5".into(),
        status: DeviceStatus::Online { address: "192.168.0.7".into(), rtt_ms: 3 },
        paired: true,
        grants_here: Permissions(1),
        grants_there: None,
        last_session: None,
        wakeable: false,
    }
}

fn devices_body(ids: &[&str]) -> ReplyBody {
    ReplyBody::Devices(ids.iter().map(|i| cv::device_to_wire(&device(i))).collect())
}

fn settings() -> Settings {
    Settings {
        name: "PC".into(),
        language: Language::PtBr,
        open_at_login: false,
        service_enabled: true,
        lock_on_end: true,
        keep_awake: false,
        quality: Quality::Auto,
        show_stats: false,
        keyboard_mode: KeyboardMode::Auto,
        swap_ctrl_cmd: false,
        theme: Theme::System,
    }
}

// ---------- tests ----------

#[test]
fn list_devices_round_trips_with_the_right_ticket() {
    let (b, mut svc, _) = Fake::connect();
    let t = b.send(Command::ListDevices);
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::ListDevices);
    svc.reply(req, devices_body(&["a", "b"]));
    assert_eq!(
        collect(&b, 1),
        vec![UiEvent::Reply { ticket: t, result: Ok(Reply::Devices(vec![device("a"), device("b")])) }]
    );
}

#[test]
fn replies_out_of_order_reach_their_own_tickets() {
    let (b, mut svc, _) = Fake::connect();
    let t1 = b.send(Command::ListDevices);
    let t2 = b.send(Command::GetSettings);
    let (r1, o1) = svc.request();
    let (r2, o2) = svc.request();
    assert_eq!((o1, o2), (UiOp::ListDevices, UiOp::GetSettings));
    assert_ne!(r1, r2);
    svc.reply(r2, ReplyBody::Settings(cv::settings_to_wire(&settings())));
    svc.reply(r1, devices_body(&["a"]));
    let got = collect(&b, 2);
    assert_eq!(got[0], UiEvent::Reply { ticket: t2, result: Ok(Reply::Settings(settings())) });
    assert_eq!(got[1], UiEvent::Reply { ticket: t1, result: Ok(Reply::Devices(vec![device("a")])) });
}

#[test]
fn error_statuses_map_to_api_codes() {
    let (b, mut svc, _) = Fake::connect();
    let t1 = b.send(Command::SetGrants { device_id: "a".into(), grants: Permissions(0x80) });
    let t2 = b.send(Command::Disconnect { session_id: 9 });
    let t3 = b.send(Command::GetNetwork);
    let (r1, o1) = svc.request();
    assert_eq!(o1, UiOp::SetGrants { device_id: "a".into(), grants: 0x80 });
    let (r2, _) = svc.request();
    let (r3, _) = svc.request();
    svc.fail(r1, swapdesk_ipc::ApiCode::NeedsAdmin);
    svc.reply(r2, ReplyBody::Done);
    // An ok status whose body does not decode for the op is an internal error, not a hang.
    svc.write(Message::UiReply { req: r3, status: 0, body: Bytes(vec![1, 2, 3]) });
    let got = collect(&b, 3);
    assert_eq!(got[0], UiEvent::Reply { ticket: t1, result: Err(ApiError::new(ApiCode::NeedsAdmin)) });
    assert_eq!(got[1], UiEvent::Reply { ticket: t2, result: Ok(Reply::Done) });
    assert_eq!(got[2], UiEvent::Reply { ticket: t3, result: Err(ApiError::new(ApiCode::Internal)) });
}

#[test]
fn notifications_become_events_and_wake_the_ui() {
    let (b, mut svc, waker) = Fake::connect();
    let incoming = IncomingRequest {
        request_id: 5,
        device: device("mac"),
        kind: RequestKind::Session { intent: Intent::Control },
        preset: Permissions(3),
        timeout_s: 30,
    };
    let session = UiEvent::Session {
        session_id: 11,
        device_id: "mac".into(),
        state: SessionState::Active { since_ms: 1, role: Role::Viewer, granted: Permissions(3) },
    };
    svc.write(Message::UiNotify(Notify::DevicesChanged(vec![cv::device_to_wire(&device("mac"))])));
    svc.write(Message::UiNotify(cv::event_to_notify(&session).unwrap()));
    svc.write(Message::UiNotify(Notify::IncomingRequest(cv::incoming_to_wire(&incoming))));
    let got = collect(&b, 3);
    assert_eq!(got[0], UiEvent::DevicesChanged(vec![device("mac")]));
    assert_eq!(got[1], session);
    assert_eq!(got[2], UiEvent::IncomingRequest(incoming));
    let wakes = waker.0.load(Ordering::SeqCst);
    assert!((1..=3).contains(&wakes), "wakes: {wakes}");
}

#[test]
fn viewer_video_reaches_the_sink_and_viewer_input_is_written() {
    let sink = Arc::new(RecordingSink::default());
    let (b, mut svc, _) = Fake::connect_with(Some(sink.clone()));
    let header = VideoHeader {
        keyframe: true,
        config: false,
        discontinuity: false,
        display_id: 1,
        frame_id: 7,
        capture_us: 10,
        encode_us: 2,
        width: 64,
        height: 48,
        codec: Codec::H264,
        payload_len: 3,
    };
    let pos = CursorPosition { visible: true, display: 1, seq: 2, x: 100, y: 200, shape_id: 0 };
    svc.write(Message::ViewerVideo { session_id: 8, header, payload: vec![1, 2, 3].into() });
    svc.write(Message::ViewerAudio { session_id: 8, datagram: Bytes(vec![9; 20]) });
    svc.write(Message::ViewerCursorPos { session_id: 8, pos });
    let end = Instant::now() + WAIT;
    while sink.cursor.lock().unwrap().is_empty() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(*sink.video.lock().unwrap(), vec![(8, header, vec![1, 2, 3])]);
    assert_eq!(*sink.audio.lock().unwrap(), vec![(8, vec![9; 20])]);
    assert_eq!(*sink.cursor.lock().unwrap(), vec![(8, pos)]);
    assert!(b.drain().is_empty(), "media is not a UiEvent");

    let events = vec![InputEvent::Pointer { display: 1, x: 5, y: 6 }, InputEvent::ReleaseAll];
    b.viewer_input(8, events.clone());
    match svc.read() {
        Message::ViewerInput { session_id, events: got } => assert_eq!((session_id, got.0), (8, events)),
        other => panic!("expected ViewerInput, got {other:?}"),
    }
    let request = KeyframeRequest { display: 1, last_good_frame: 6 };
    let report = ReceiverReport {
        frames_presented: 1,
        frames_lost: 0,
        decode_us_p50: 2,
        present_lag_us_p50: 3,
        audio_underruns: 0,
    };
    b.viewer_keyframe(8, request.clone());
    b.viewer_report(8, report.clone());
    assert_eq!(svc.read(), Message::ViewerKeyframe { session_id: 8, request });
    assert_eq!(svc.read(), Message::ViewerReport { session_id: 8, report });
}

#[test]
fn closing_the_service_fails_pending_tickets_with_service_down() {
    let (b, mut svc, waker) = Fake::connect();
    let t1 = b.send(Command::ListDevices);
    let t2 = b.send(Command::GetThisDevice);
    svc.request();
    svc.request();
    drop(svc);
    let got = collect(&b, 2);
    let down = |t| UiEvent::Reply { ticket: t, result: Err(ApiError::new(ApiCode::ServiceDown)) };
    assert_eq!(got, vec![down(t1), down(t2)]);
    assert!(waker.0.load(Ordering::SeqCst) >= 1);
    assert!(b.is_down());
    // Later commands fail at once instead of waiting for a reply that cannot come.
    let t3 = b.send(Command::GetNetwork);
    assert_eq!(b.drain(), vec![down(t3)]);
}

#[test]
fn ui_local_and_unsupported_commands_reply_at_once_without_the_pipe() {
    let (b, mut svc, _) = Fake::connect();
    let local = [
        b.send(Command::SetScale { session_id: 1, scale: Scale::Fit }),
        b.send(Command::SetKeyboardMode { session_id: 1, mode: KeyboardMode::Text }),
        b.send(Command::SetGrab { session_id: 1, on: true }),
    ];
    // No library cancels a swap and the UI has no control for it: unsupported, like the commands of stages 42 to 44.
    let unsupported = b.send(Command::CancelSwap { request_id: 1 });
    let got = collect(&b, 4);
    for (i, t) in local.iter().enumerate() {
        assert_eq!(got[i], UiEvent::Reply { ticket: *t, result: Ok(Reply::Done) });
    }
    assert_eq!(got[3], UiEvent::Reply { ticket: unsupported, result: Err(ApiError::new(ApiCode::Internal)) });
    // Nothing of these reached the service: the next thing it reads is the first real request, with req 1.
    let t = b.send(Command::ListDevices);
    let (req, op) = svc.request();
    assert_eq!((req, op), (1, UiOp::ListDevices));
    svc.reply(req, devices_body(&[]));
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t, result: Ok(Reply::Devices(vec![])) }]);
}

#[test]
fn produced_events_drive_the_devices_screen() {
    let (b, mut svc, _) = Fake::connect();
    let mut state = UiState::default();
    let command = Command::ListDevices;
    let t = b.send(command.clone());
    state.track(t, &command);
    let (req, _) = svc.request();
    svc.reply(req, devices_body(&["a", "b"]));
    for e in collect(&b, 1) {
        state.apply(e);
    }
    assert_eq!(state.screen(), Screen::Home, "the window starts on Início");
    assert_eq!(state.devices().len(), 2);
    svc.write(Message::UiNotify(Notify::DevicesChanged(vec![cv::device_to_wire(&device("b"))])));
    for e in collect(&b, 1) {
        state.apply(e);
    }
    assert_eq!(state.devices().len(), 1);
    assert_eq!(state.selected_device().unwrap().id, "b");
}

#[test]
fn list_displays_travels_as_op_27_and_the_reply_is_the_session_list() {
    let (b, mut svc, _) = Fake::connect();
    let wire = swapdesk_ipc::DisplayView {
        id: 2,
        name: "Tela".into(),
        width: 1920,
        height: 1080,
        scale_milli: 1500,
        primary: true,
    };
    let t = b.send(Command::ListDisplays { session_id: 7 });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::ListDisplays { session_id: 7 });
    assert_eq!(op.code(), 27);
    svc.reply(req, ReplyBody::Displays(vec![wire]));
    let want = DisplayView { id: 2, name: "Tela".into(), width: 1920, height: 1080, scale_milli: 1500, primary: true };
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t, result: Ok(Reply::Displays(vec![want])) }]);
}

#[test]
fn swap_chat_and_wake_commands_travel_as_their_ops_and_come_back_as_events() {
    use swapdesk_ipc::{ChatItem, SwapNote, SwapWhy as WireWhy, Text};
    let (b, mut svc, _) = Fake::connect();

    // "Inverter": a request number at once, then the outcome as a notification.
    let t = b.send(Command::RequestSwap { session_id: 7 });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::RequestSwap { session_id: 7 });
    svc.reply(req, ReplyBody::RequestId(41));
    svc.write(Message::UiNotify(Notify::Swap {
        session_id: 7,
        note: SwapNote::Declined { why: WireWhy::NobodyPresent },
    }));
    svc.write(Message::UiNotify(Notify::Swap {
        session_id: 7,
        note: SwapNote::Asked { request_id: 3, by: "MacBook".into(), remaining_s: 20 },
    }));
    assert_eq!(
        collect(&b, 3),
        vec![
            UiEvent::Reply { ticket: t, result: Ok(Reply::RequestId(41)) },
            UiEvent::Swap { session_id: 7, event: SwapEvent::Declined { why: SwapWhy::NobodyPresent } },
            UiEvent::Swap {
                session_id: 7,
                event: SwapEvent::Asked { request_id: 3, by: "MacBook".into(), remaining_s: 20 }
            },
        ]
    );
    // The answer to a card.
    b.send(Command::AnswerSwap { request_id: 3, accept: true });
    assert_eq!(svc.request().1, UiOp::AnswerSwap { request_id: 3, accept: true });

    // Chat: the reply is this side's own message, a notification is the peer's, and the read mark goes out.
    let t = b.send(Command::ChatSend { session_id: 7, text: "oi".into() });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::ChatSend { session_id: 7, text: swapdesk_ipc::Text("oi".into()) });
    svc.reply(req, ReplyBody::Chat(ChatItem { msg_id: 1, text: Text("oi".into()), sent_at_ms: 55 }));
    svc.write(Message::UiNotify(Notify::Chat {
        session_id: 7,
        item: ChatItem { msg_id: 1, text: Text("olá".into()), sent_at_ms: 56 },
    }));
    svc.write(Message::UiNotify(Notify::ChatRead { session_id: 7, up_to: 1 }));
    assert_eq!(
        collect(&b, 3),
        vec![
            UiEvent::Reply {
                ticket: t,
                result: Ok(Reply::Chat(ChatView {
                    msg_id: 1,
                    mine: true,
                    text: "oi".into(),
                    sent_at_ms: 55,
                    read: false
                }))
            },
            UiEvent::Chat {
                session_id: 7,
                message: ChatView { msg_id: 1, mine: false, text: "olá".into(), sent_at_ms: 56, read: false }
            },
            UiEvent::ChatRead { session_id: 7, up_to: 1 },
        ]
    );
    b.send(Command::ChatMarkRead { session_id: 7, up_to: 1 });
    assert_eq!(svc.request().1, UiOp::ChatMarkRead { session_id: 7 });

    // Wake: the reply is the id of the waking entry, whose countdown arrives as a session state.
    let t = b.send(Command::Wake { device_id: "d".into() });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::Wake { device_id: "d".into(), intent: swapdesk_proto::Intent::Control });
    svc.reply(req, ReplyBody::SessionId(1 << 63));
    svc.write(Message::UiNotify(Notify::Session {
        session_id: 1 << 63,
        device_id: "d".into(),
        state: swapdesk_ipc::SessionState::Waking { remaining_s: 60 },
    }));
    assert_eq!(
        collect(&b, 2),
        vec![
            UiEvent::Reply { ticket: t, result: Ok(Reply::SessionId(1 << 63)) },
            UiEvent::Session {
                session_id: 1 << 63,
                device_id: "d".into(),
                state: SessionState::Waking { remaining_s: 60 }
            },
        ]
    );
}

#[test]
fn clipboard_and_action_commands_travel_as_ops_28_and_29_and_a_skipped_item_comes_back() {
    let (b, mut svc, _) = Fake::connect();
    let t = b.send(Command::SetClipboardSync { session_id: 7, on: false });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::SetClipboardSync { session_id: 7, on: false });
    svc.reply(req, ReplyBody::Done);
    let t2 = b.send(Command::RunAction { session_id: 7, action: ActionKind::Sleep });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::RunAction { session_id: 7, action: swapdesk_proto::Action::Sleep });
    svc.fail(req, swapdesk_ipc::ApiCode::NotPermitted);
    svc.write(Message::UiNotify(Notify::ClipboardSkipped { session_id: 7, reason: swapdesk_ipc::ApiCode::Invalid }));
    assert_eq!(
        collect(&b, 3),
        vec![
            UiEvent::Reply { ticket: t, result: Ok(Reply::Done) },
            UiEvent::Reply { ticket: t2, result: Err(ApiError::new(ApiCode::NotPermitted)) },
            UiEvent::ClipboardSkipped { session_id: 7, reason: ApiCode::Invalid },
        ]
    );
}

struct MemClip {
    count: u64,
    content: Option<swapdesk_platform::ClipContent>,
}

impl swapdesk_platform::ClipboardProvider for MemClip {
    fn change_count(&mut self) -> swapdesk_platform::Result<u64> {
        Ok(self.count)
    }
    fn read(&mut self) -> swapdesk_platform::Result<swapdesk_platform::ClipRead> {
        Ok(self.content.clone().map_or(swapdesk_platform::ClipRead::Empty, swapdesk_platform::ClipRead::Content))
    }
    fn write(&mut self, content: &swapdesk_platform::ClipContent) -> swapdesk_platform::Result<()> {
        self.count += 1;
        self.content = Some(content.clone());
        Ok(())
    }
}

// ---------- the terminal (stage 43) ----------

#[derive(Default)]
struct TermSink {
    out: Mutex<Vec<(u32, Vec<u8>)>>,
}

impl ViewerSink for TermSink {
    fn video(&self, _: u64, _: VideoHeader, _: Vec<u8>) {}
    fn audio(&self, _: u64, _: Vec<u8>) {}
    fn cursor_pos(&self, _: u64, _: CursorPosition) {}
    fn cursor_shape(&self, _: u64, _: swapdesk_proto::CursorShape) {}
    fn term_output(&self, term_id: u32, bytes: Vec<u8>) {
        self.out.lock().unwrap().push((term_id, bytes));
    }
}

fn term_opened(term_id: u32, outcome: swapdesk_ipc::TermOutcome, account: &str) -> ReplyBody {
    ReplyBody::TermOpened { term_id, outcome, account: swapdesk_ipc::Text(account.into()) }
}

#[test]
fn an_open_terminal_is_an_id_then_the_account_and_input_is_written_as_messages() {
    let (b, mut svc, _) = Fake::connect();
    let t = b.send(Command::TermOpen { session_id: 4, admin: false, cols: 100, rows: 30 });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::TermOpen { session_id: 4, cols: 100, rows: 30, admin: false });
    svc.reply(req, term_opened(7, swapdesk_ipc::TermOutcome::Opened, "alex"));
    assert_eq!(
        collect(&b, 2),
        vec![
            UiEvent::Reply { ticket: t, result: Ok(Reply::TermId(7)) },
            UiEvent::Term { term_id: 7, event: TermEvent::Opened { account: "alex".into() } }
        ],
        "the id answers the command, the account follows as the terminal's own event"
    );

    // Input is not an op: it is the TermInput message, cut into chunks, answered Done at once.
    let big = vec![b'x'; swapdesk_ipc::MAX_TERM_CHUNK + 10];
    let t2 = b.send(Command::TermWrite { term_id: 7, bytes: big });
    match svc.read() {
        Message::TermInput { term_id: 7, bytes } => assert_eq!(bytes.0.len(), swapdesk_ipc::MAX_TERM_CHUNK),
        other => panic!("expected TermInput, got {other:?}"),
    }
    match svc.read() {
        Message::TermInput { term_id: 7, bytes } => assert_eq!(bytes.0.len(), 10),
        other => panic!("expected TermInput, got {other:?}"),
    }
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t2, result: Ok(Reply::Done) }]);

    let t3 = b.send(Command::TermResize { term_id: 7, cols: 90, rows: 20 });
    let (r3, op3) = svc.request();
    assert_eq!(op3, UiOp::TermResize { term_id: 7, cols: 90, rows: 20 });
    svc.reply(r3, ReplyBody::Done);
    let t4 = b.send(Command::TermClose { term_id: 7 });
    let (r4, op4) = svc.request();
    assert_eq!(op4, UiOp::TermClose { term_id: 7 });
    svc.reply(r4, ReplyBody::Done);
    assert_eq!(
        collect(&b, 2),
        vec![
            UiEvent::Reply { ticket: t3, result: Ok(Reply::Done) },
            UiEvent::Reply { ticket: t4, result: Ok(Reply::Done) }
        ]
    );
}

#[test]
fn a_refused_open_is_an_error_that_says_why() {
    use crate::strings::MsgKey as K;
    use swapdesk_ipc::TermOutcome as O;
    for (outcome, code, key) in [
        (O::NobodyPresent, ApiCode::Refused, K::TermFailNobody),
        (O::Declined, ApiCode::Refused, K::TermFailDeclined),
        (O::TimedOut, ApiCode::Timeout, K::TermFailTimedOut),
        (O::NotPermitted, ApiCode::NotPermitted, K::TermFailNotPermitted),
        (O::TooMany, ApiCode::Busy, K::TermFailTooMany),
        (O::Unavailable, ApiCode::NotFound, K::TermFailUnavailable),
        (O::Failed, ApiCode::Internal, K::TermFailed),
    ] {
        let (b, mut svc, _) = Fake::connect();
        let t = b.send(Command::TermOpen { session_id: 4, admin: false, cols: 80, rows: 24 });
        let (req, _) = svc.request();
        svc.reply(req, term_opened(0, outcome, ""));
        assert_eq!(
            collect(&b, 1),
            vec![UiEvent::Reply { ticket: t, result: Err(ApiError { code, detail: Some(Msg::new(key)) }) }],
            "{outcome:?}"
        );
    }
}

#[test]
fn the_service_reads_and_writes_this_uis_clipboard_over_its_pipe() {
    use swapdesk_ipc::{ClipData, ClipFail, ClipReplyBody};
    let (b, mut svc, _) = Fake::connect();
    // No provider yet: every call is answered with a failure, never left waiting.
    svc.write(Message::ClipRead { req: 1 });
    assert_eq!(svc.read(), Message::ClipReply { req: 1, body: ClipReplyBody::Failed(ClipFail::Other) });

    b.set_clipboard(MemClip { count: 3, content: None });
    svc.write(Message::ClipChangeCount { req: 2 });
    assert_eq!(svc.read(), Message::ClipReply { req: 2, body: ClipReplyBody::Count(3) });
    svc.write(Message::ClipWrite { req: 3, content: ClipData::Text("olá".into()) });
    assert_eq!(svc.read(), Message::ClipReply { req: 3, body: ClipReplyBody::Written });
    svc.write(Message::ClipRead { req: 4 });
    assert_eq!(svc.read(), Message::ClipReply { req: 4, body: ClipReplyBody::Content(ClipData::Text("olá".into())) });
    svc.write(Message::ClipChangeCount { req: 5 });
    assert_eq!(svc.read(), Message::ClipReply { req: 5, body: ClipReplyBody::Count(4) });
}

#[test]
fn terminal_output_goes_to_the_sink_and_the_end_is_an_event() {
    let sink = Arc::new(TermSink::default());
    let (b, mut svc, _) = Fake::connect_with(Some(sink.clone()));
    svc.write(Message::TermOutput { term_id: 7, bytes: Bytes(b"hello".to_vec()) });
    svc.write(Message::TermOutput { term_id: 8, bytes: Bytes(b"other".to_vec()) });
    svc.write(Message::UiNotify(Notify::TermExited { term_id: 7, exit_code: Some(2) }));
    assert_eq!(collect(&b, 1), vec![UiEvent::Term { term_id: 7, event: TermEvent::Exited { code: Some(2) } }]);
    assert_eq!(
        *sink.out.lock().unwrap(),
        vec![(7, b"hello".to_vec()), (8, b"other".to_vec())],
        "output is not a UiEvent"
    );
    assert!(b.drain().is_empty());
    // Without a sink the bytes are dropped, not queued.
    let (b2, mut svc2, _) = Fake::connect();
    svc2.write(Message::TermOutput { term_id: 1, bytes: Bytes(b"x".to_vec()) });
    svc2.write(Message::UiNotify(Notify::TermExited { term_id: 1, exit_code: None }));
    assert_eq!(collect(&b2, 1), vec![UiEvent::Term { term_id: 1, event: TermEvent::Exited { code: None } }]);
}

// ---------- files (stage 44) ----------

fn files_temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-ui-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The service's disk call to the UI and the UI's reply.
fn disk_call(
    svc: &mut Fake,
    req: u32,
    call: swapdesk_ipc::FsCall,
) -> Result<swapdesk_ipc::FsAnswer, swapdesk_proto::ErrorCode> {
    svc.write(Message::FsRequest { req, call });
    loop {
        match svc.read() {
            Message::FsReply { req: r, result } if r == req => return result,
            Message::FsReply { .. } => {}
            other => panic!("expected FsReply, got {other:?}"),
        }
    }
}

#[test]
fn files_commands_travel_as_ops_40_to_45_and_their_answers_become_the_c6_replies_and_events() {
    use swapdesk_ipc::{FileEntry, FileListing, FilesNote, FilesOp, FsAnswer, FsCall, Text, TransferWire};
    use swapdesk_proto::ErrorCode;
    let (b, mut svc, _) = Fake::connect();

    // The listing: the reply is the folder as the host spells it.
    let t = b.send(Command::FilesList { session_id: 7, path: Some("Docs".into()) });
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::Files(FilesOp::List { session_id: 7, path: Some(Text("Docs".into())) }));
    svc.reply(
        req,
        ReplyBody::Listing(FileListing {
            path: Text("Docs".into()),
            entries: vec![FileEntry { name: Text("a.pdf".into()), is_dir: false, size: 9, mtime_ms: 3 }],
            truncated: true,
        }),
    );
    assert_eq!(
        collect(&b, 1),
        vec![UiEvent::Reply {
            ticket: t,
            result: Ok(Reply::Listing(Listing {
                path: "Docs".into(),
                entries: vec![DirEntryView { name: "a.pdf".into(), is_dir: false, size: 9, mtime_ms: 3 }],
                truncated: true
            })),
        }]
    );

    // Send: the picked files become tokens (the service never sees the paths); the reply is one number per file.
    let dir = files_temp("send");
    std::fs::write(dir.join("one.txt"), b"1").unwrap();
    std::fs::write(dir.join("two.txt"), b"22").unwrap();
    let t = b.send(Command::FilesSend {
        session_id: 7,
        local_paths: vec![dir.join("one.txt"), dir.join("two.txt")],
        remote_dir: Some("incoming".into()),
    });
    let (req, op) = svc.request();
    let UiOp::Files(FilesOp::Send { session_id: 7, picks, remote_dir }) = op else { panic!("not a send: {op:?}") };
    assert_eq!(remote_dir, Some(Text("incoming".into())));
    assert_eq!(picks.iter().map(|p| p.name.0.as_str()).collect::<Vec<_>>(), ["one.txt", "two.txt"]);
    assert_ne!(picks[0].token, picks[1].token);
    svc.reply(req, ReplyBody::TransferIds(vec![5, 6]));
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t, result: Ok(Reply::TransferIds(vec![5, 6])) }]);
    // The UI's own disk serves those tokens, and only those: the picked file reads; its neighbour does not.
    let h = disk_call(&mut svc, 100, FsCall::OpenRead { path: Text(format!("p{}/one.txt", picks[0].token)) });
    let Ok(FsAnswer::Handle(h)) = h else { panic!("a handle, got {h:?}") };
    assert_eq!(
        disk_call(&mut svc, 101, FsCall::ReadAt { id: h.id, offset: 0, len: 10 }),
        Ok(FsAnswer::Data(Bytes(b"1".to_vec())))
    );
    for bad in [
        format!("p{}/two.txt", picks[0].token),
        "one.txt".to_string(),
        format!("{}", dir.join("one.txt").display()),
        "../one.txt".to_string(),
    ] {
        assert_eq!(
            disk_call(&mut svc, 102, FsCall::OpenRead { path: Text(bad.clone()) }),
            Err(ErrorCode::InvalidName),
            "{bad}"
        );
    }
    // A file with no name cannot be sent: refused at once, nothing reaches the service.
    let t = b.send(Command::FilesSend {
        session_id: 7,
        local_paths: vec![std::path::PathBuf::from("/")],
        remote_dir: None,
    });
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t, result: Err(ApiError::new(ApiCode::Invalid)) }]);

    // Get: the receive folder is a token too; the service writes into it by that token and nowhere else.
    let recv = files_temp("recv");
    let t = b.send(Command::FilesGet { session_id: 7, remote_path: "Docs/a.pdf".into(), local_dir: recv.clone() });
    let (req, op) = svc.request();
    let UiOp::Files(FilesOp::Get { session_id: 7, remote_path, recv_token }) = op else { panic!("not a get: {op:?}") };
    assert_eq!(remote_path.0, "Docs/a.pdf");
    svc.reply(req, ReplyBody::TransferId(8));
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: t, result: Ok(Reply::TransferId(8)) }]);
    let into = Text(format!("r{recv_token}"));
    assert_eq!(
        disk_call(&mut svc, 110, FsCall::PartCreate { dir: into.clone(), name: Text("a.pdf".into()) }),
        Ok(FsAnswer::Done)
    );
    assert!(recv.join("a.pdf.swapdesk-part").exists());
    for bad in ["r999".to_string(), "..".to_string(), String::new(), format!("r{recv_token}/../x")] {
        assert_eq!(
            disk_call(&mut svc, 111, FsCall::PartCreate { dir: Text(bad.clone()), name: Text("x".into()) }),
            Err(ErrorCode::InvalidName),
            "{bad}"
        );
    }
    assert_eq!(
        disk_call(&mut svc, 112, FsCall::ListDir { path: into, max_entries: 5 }),
        Err(ErrorCode::InvalidName),
        "the UI lists nothing for the service"
    );

    // Pause, resume, cancel.
    for (cmd, op) in [
        (Command::TransferPause { transfer_id: 5 }, FilesOp::Pause { transfer_id: 5 }),
        (Command::TransferResume { transfer_id: 5 }, FilesOp::Resume { transfer_id: 5 }),
        (Command::TransferCancel { transfer_id: 5 }, FilesOp::Cancel { transfer_id: 5 }),
    ] {
        b.send(cmd);
        assert_eq!(svc.request().1, UiOp::Files(op));
    }

    // The notifications: a transfer's state, and the name a received file was saved under.
    let note = |state| {
        Message::UiNotify(Notify::Files(FilesNote::Transfer {
            session_id: 7,
            transfer_id: 8,
            name: Text("a.pdf".into()),
            outgoing: false,
            state,
        }))
    };
    svc.write(note(TransferWire::Running { done: 1, total: 9, bytes_per_s: 4 }));
    svc.write(Message::UiNotify(Notify::Files(FilesNote::SavedAs {
        session_id: 7,
        transfer_id: 8,
        name: Text("a (1).pdf".into()),
    })));
    svc.write(note(TransferWire::Paused { done: 3, total: 9, why: swapdesk_ipc::ApiCode::Timeout }));
    assert_eq!(
        collect(&b, 3),
        vec![
            UiEvent::Transfer {
                transfer_id: 8,
                session_id: 7,
                name: "a.pdf".into(),
                outgoing: false,
                state: TransferState::Running { done: 1, total: 9, bytes_per_s: 4 }
            },
            UiEvent::TransferSavedAs { transfer_id: 8, session_id: 7, name: "a (1).pdf".into() },
            UiEvent::Transfer {
                transfer_id: 8,
                session_id: 7,
                name: "a.pdf".into(),
                outgoing: false,
                state: TransferState::Paused { done: 3, total: 9, why: ApiCode::Timeout }
            },
        ]
    );
    // Dropping the backend ends the disk thread and closes the pipe.
    drop(b);
    let _ = (std::fs::remove_dir_all(&dir), std::fs::remove_dir_all(&recv));
}

#[test]
fn the_files_events_map_both_ways() {
    let events = [
        UiEvent::Transfer {
            transfer_id: 1,
            session_id: 2,
            name: "x".into(),
            outgoing: true,
            state: TransferState::Queued,
        },
        UiEvent::Transfer {
            transfer_id: 1,
            session_id: 2,
            name: "x".into(),
            outgoing: false,
            state: TransferState::Done,
        },
        UiEvent::Transfer {
            transfer_id: 1,
            session_id: 2,
            name: "x".into(),
            outgoing: false,
            state: TransferState::Failed { reason: ApiCode::Refused },
        },
        UiEvent::TransferSavedAs { transfer_id: 1, session_id: 2, name: "x (1)".into() },
    ];
    for e in events {
        let n = cv::event_to_notify(&e).expect("it crosses the pipe");
        assert_eq!(cv::notify_to_event(n).unwrap(), e);
    }
}

// ---------- elevation ----------

use swapdesk_ipc::{ElevAction, ElevationOutcome, ElevationTicket};

const TICKET: ElevationTicket = ElevationTicket([7; 16]);

/// An `Elevator` that answers as scripted and remembers what it was asked.
struct FakeElevator {
    run: ElevatorRun,
    seen: Mutex<Vec<(ElevationTicket, Instant)>>,
    /// While set, `run` waits for it first.
    gate: Mutex<Option<Receiver<()>>>,
}

impl FakeElevator {
    fn new(run: ElevatorRun) -> Arc<Self> {
        Arc::new(FakeElevator { run, seen: Mutex::new(Vec::new()), gate: Mutex::new(None) })
    }
}

impl Elevator for FakeElevator {
    fn run(&self, ticket: &ElevationTicket, _line: &str, deadline: Instant) -> ElevatorRun {
        self.seen.lock().unwrap().push((*ticket, deadline));
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.recv_timeout(WAIT);
        }
        self.run
    }
}

fn grants_action() -> ElevatedAction {
    ElevatedAction::SetGrants { device_id: "mac".into(), grants: Permissions(0b1000_0011) }
}

/// Plays the service for one `Elevate`: answers Begin with `TICKET`, then End with `outcome`.
fn serve_elevation(svc: &mut Fake, outcome: ElevationOutcome) {
    let (req, op) = svc.request();
    assert_eq!(
        op,
        UiOp::ElevationBegin { action: ElevAction::SetGrants { device_id: "mac".into(), grants: 0b1000_0011 } }
    );
    svc.reply(req, ReplyBody::ElevationTicket { ticket: TICKET, ttl_s: 120 });
    let (req, end) = svc.request();
    assert_eq!(end, UiOp::ElevationEnd { ticket: TICKET }, "the ticket is always ended");
    svc.reply(req, ReplyBody::ElevationOutcome(outcome));
}

fn elevation_events(b: &ServiceBackend, t: Ticket) -> Result<(), ApiError> {
    let got = collect(b, 2);
    let UiEvent::ElevationResult { action, run: _, result } = got[0].clone() else { panic!("{got:?}") };
    assert_eq!(action, grants_action());
    // The ticket of the command is answered `Done` after the result, so the strip shows nothing more.
    assert_eq!(got[1], UiEvent::Reply { ticket: t, result: Ok(Reply::Done) });
    result
}

#[test]
fn elevate_asks_a_ticket_runs_the_elevator_ends_it_and_reports_ok() {
    let (b, mut svc, _) = Fake::connect();
    let elevator = FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) });
    b.set_elevator(elevator.clone());
    let before = Instant::now();
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    serve_elevation(&mut svc, ElevationOutcome::Confirmed);
    assert_eq!(elevation_events(&b, t), Ok(()));
    let seen = elevator.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, TICKET);
    let left = seen[0].1.duration_since(before);
    assert!(left > Duration::from_secs(105) && left <= Duration::from_secs(111), "{left:?}");
}

#[test]
fn elevate_does_not_hold_the_backend_while_the_elevator_runs() {
    let (b, mut svc, _) = Fake::connect();
    let elevator = FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) });
    let (open, gate) = mpsc::channel();
    *elevator.gate.lock().unwrap() = Some(gate);
    b.set_elevator(elevator);
    let e = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    let (req, _) = svc.request();
    svc.reply(req, ReplyBody::ElevationTicket { ticket: TICKET, ttl_s: 120 });
    // The elevator is still running: another command goes through the same pipe and is answered.
    let l = b.send(Command::ListDevices);
    let (req, op) = svc.request();
    assert_eq!(op, UiOp::ListDevices);
    svc.reply(req, devices_body(&[]));
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: l, result: Ok(Reply::Devices(vec![])) }]);
    open.send(()).unwrap();
    let (req, _) = svc.request();
    svc.reply(req, ReplyBody::ElevationOutcome(ElevationOutcome::Confirmed));
    assert_eq!(elevation_events(&b, e), Ok(()));
}

#[test]
fn a_declined_uac_ends_the_ticket_and_reports_refused() {
    let (b, mut svc, _) = Fake::connect();
    b.set_elevator(FakeElevator::new(ElevatorRun::Declined));
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    serve_elevation(&mut svc, ElevationOutcome::Open);
    assert_eq!(elevation_events(&b, t), Err(ApiError::new(ApiCode::Refused)));
}

#[test]
fn elevate_without_an_elevator_reports_internal_and_sends_no_ticket() {
    let (b, mut svc, _) = Fake::connect();
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    assert_eq!(elevation_events(&b, t), Err(ApiError::new(ApiCode::Internal)));
    // Nothing reached the service: the next thing it reads is the first real request, with req 1.
    let l = b.send(Command::ListDevices);
    assert_eq!(svc.request(), (1, UiOp::ListDevices));
    svc.reply(1, devices_body(&[]));
    assert_eq!(collect(&b, 1), vec![UiEvent::Reply { ticket: l, result: Ok(Reply::Devices(vec![])) }]);
}

#[test]
fn an_action_on_hold_sends_no_ticket_and_reports_internal() {
    let (b, mut svc, _) = Fake::connect();
    let elevator = FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) });
    b.set_elevator(elevator.clone());
    for action in [
        ElevatedAction::ConnectUnattended { device_id: "mac".into(), intent: Intent::Control },
        ElevatedAction::OpenAdminTerminal { session_id: 1 },
    ] {
        let t = b.send(Command::Elevate { action: action.clone(), line: String::new(), run: 1 });
        let got = collect(&b, 2);
        assert_eq!(got[0], UiEvent::ElevationResult { action, run: 1, result: Err(ApiError::new(ApiCode::Internal)) });
        assert_eq!(got[1], UiEvent::Reply { ticket: t, result: Ok(Reply::Done) });
    }
    assert!(elevator.seen.lock().unwrap().is_empty());
    b.send(Command::ListDevices);
    assert_eq!(svc.request(), (1, UiOp::ListDevices));
}

#[test]
fn the_outcome_comes_from_the_service_not_from_the_exit_code() {
    let (b, mut svc, _) = Fake::connect();
    // The process says 0, the service says nobody confirmed: a failure.
    b.set_elevator(FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) }));
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    serve_elevation(&mut svc, ElevationOutcome::Open);
    assert_eq!(elevation_events(&b, t), Err(ApiError::new(ApiCode::Internal)));

    // The other way: the process says 5, the service says confirmed: Ok.
    let (b, mut svc, _) = Fake::connect();
    b.set_elevator(FakeElevator::new(ElevatorRun::Ended { exit_code: Some(5) }));
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    serve_elevation(&mut svc, ElevationOutcome::Confirmed);
    assert_eq!(elevation_events(&b, t), Ok(()));
}

#[test]
fn a_failed_run_is_refined_by_the_exit_code_and_the_end_outcome() {
    use ElevationOutcome as O;
    let run = |exit_code| ElevatorRun::Ended { exit_code };
    let cases = [
        (run(Some(1)), O::Declined, ApiCode::Refused),
        (run(Some(1)), O::Open, ApiCode::Refused),
        (run(Some(3)), O::Open, ApiCode::ServiceDown),
        (run(Some(4)), O::Open, ApiCode::NotPermitted),
        (run(Some(6)), O::Open, ApiCode::NotPermitted),
        (run(Some(5)), O::Unknown, ApiCode::Timeout),
        (run(Some(0)), O::Unknown, ApiCode::Timeout),
        (run(Some(2)), O::Open, ApiCode::Internal),
        (run(Some(7)), O::Open, ApiCode::Internal),
        (run(None), O::Open, ApiCode::Internal),
        (ElevatorRun::TimedOut, O::Open, ApiCode::Timeout),
        (ElevatorRun::Failed(ApiCode::Internal), O::Open, ApiCode::Internal),
    ];
    for (run, outcome, want) in cases {
        let (b, mut svc, _) = Fake::connect();
        b.set_elevator(FakeElevator::new(run));
        let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
        serve_elevation(&mut svc, outcome);
        assert_eq!(elevation_events(&b, t), Err(ApiError::new(want)), "{run:?} then {outcome:?}");
    }
}

#[test]
fn a_refused_begin_reports_its_code_and_the_service_going_away_reports_service_down() {
    for code in [swapdesk_ipc::ApiCode::NotPermitted, swapdesk_ipc::ApiCode::NotFound, swapdesk_ipc::ApiCode::Busy] {
        let (b, mut svc, _) = Fake::connect();
        let elevator = FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) });
        b.set_elevator(elevator.clone());
        let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
        let (req, _) = svc.request();
        svc.fail(req, code);
        assert_eq!(elevation_events(&b, t), Err(ApiError::new(cv::code_from_wire(code))));
        assert!(elevator.seen.lock().unwrap().is_empty(), "nothing runs without a ticket");
    }
    let (b, mut svc, _) = Fake::connect();
    b.set_elevator(FakeElevator::new(ElevatorRun::Ended { exit_code: Some(0) }));
    let t = b.send(Command::Elevate { action: grants_action(), line: String::new(), run: 1 });
    let _ = svc.request();
    drop(svc);
    assert_eq!(elevation_events(&b, t), Err(ApiError::new(ApiCode::ServiceDown)));
}

// ---------- the elevated instance's side ----------

use crate::elevated_client::{self, ElevatedExit};

/// Runs `elevated_client::run` against a service side whose answers are written before it starts (the pipe buffers
/// them): the ack, the answer to the Describe (req 1) and the answer to the Confirm (req 2). Returns the exit, what
/// the client wrote and the confirmations it was asked for.
fn elevated_run(
    describe: Result<ReplyBody, swapdesk_ipc::ApiCode>,
    confirm_answer: bool,
    confirm_reply: Result<ReplyBody, swapdesk_ipc::ApiCode>,
) -> (ElevatedExit, Vec<Message>, Vec<(ElevAction, String, u16)>) {
    let (client_in, svc_out) = pipe();
    let (svc_in, client_out) = pipe();
    let mut to_client = FrameWriter::new(svc_out);
    to_client.write_message(&Message::HelloAck { version: IPC_VERSION, win_session: 3 }).unwrap();
    for (req, r) in [(1, describe), (2, confirm_reply)] {
        let m = match r {
            Ok(body) => Message::UiReply { req, status: 0, body: Bytes(body.encode().unwrap()) },
            Err(code) => Message::UiReply { req, status: code.status(), body: Bytes::default() },
        };
        to_client.write_message(&m).unwrap();
    }
    let mut asked = Vec::new();
    let exit = elevated_client::run(client_in, client_out, &TICKET, |action, peer, left| {
        asked.push((action.clone(), peer.to_owned(), left));
        confirm_answer
    });
    drop(to_client);
    let mut from_client = FrameReader::new(svc_in);
    let mut wrote = Vec::new();
    while let Ok(Some(m)) = from_client.read_message() {
        wrote.push(m);
    }
    (exit, wrote, asked)
}

fn described() -> Result<ReplyBody, swapdesk_ipc::ApiCode> {
    Ok(ReplyBody::ElevationAction {
        action: ElevAction::SetGrants { device_id: "mac".into(), grants: 0b1000_0011 },
        peer_name: "MacBook".into(),
        remaining_s: 100,
    })
}

#[test]
fn the_elevated_client_confirms_declines_and_maps_each_refusal_to_its_exit_code() {
    let hello = Message::Hello { role: IpcRole::Elevator, version: IPC_VERSION, pid: std::process::id() };
    let describe = Message::UiRequest { req: 1, op: UiOp::ElevationDescribe { ticket: TICKET } };
    let confirm = |approve| Message::UiRequest { req: 2, op: UiOp::ElevationConfirm { ticket: TICKET, approve } };

    let (exit, wrote, asked) = elevated_run(described(), true, Ok(ReplyBody::Done));
    assert_eq!((exit, exit.code()), (ElevatedExit::Confirmed, 0));
    assert_eq!(wrote, vec![hello.clone(), describe.clone(), confirm(true)]);
    assert_eq!(
        asked,
        vec![(ElevAction::SetGrants { device_id: "mac".into(), grants: 0b1000_0011 }, "MacBook".to_owned(), 100)]
    );

    let (exit, wrote, _) = elevated_run(described(), false, Ok(ReplyBody::Done));
    assert_eq!((exit, exit.code()), (ElevatedExit::Declined, 1));
    assert_eq!(wrote, vec![hello.clone(), describe.clone(), confirm(false)], "approve: false is sent");

    let refused = |code| elevated_run(Err(code), true, Ok(ReplyBody::Done));
    let (exit, wrote, asked) = refused(swapdesk_ipc::ApiCode::NotFound);
    assert_eq!((exit, exit.code()), (ElevatedExit::Expired, 5));
    assert!(asked.is_empty(), "nobody is asked about a ticket that is gone");
    assert_eq!(wrote, vec![hello.clone(), describe.clone()]);
    let (exit, _, _) = refused(swapdesk_ipc::ApiCode::NeedsAdmin);
    assert_eq!((exit, exit.code()), (ElevatedExit::NotAdmin, 6));
    let (exit, _, _) = refused(swapdesk_ipc::ApiCode::NotPermitted);
    assert_eq!((exit, exit.code()), (ElevatedExit::Refused, 4));

    // The ticket ran out while the person was deciding.
    let (exit, _, _) = elevated_run(described(), true, Err(swapdesk_ipc::ApiCode::NotFound));
    assert_eq!(exit, ElevatedExit::Expired);
}

#[test]
fn the_elevated_client_exits_unreachable_without_a_service() {
    let (client_in, svc_out) = pipe();
    let (_svc_in, client_out) = pipe();
    drop(svc_out);
    let exit = elevated_client::run(client_in, client_out, &TICKET, |_, _, _| true);
    assert_eq!((exit, exit.code()), (ElevatedExit::Unreachable, 3));
}

/// Review of the elevation, finding 9: the service ends an elevator connection whose lifetime ran out with
/// `Bye { TIMEOUT }`; the ticket is gone with it, which is `Expired`, not an unreachable service.
#[test]
fn an_elevator_connection_ended_for_its_lifetime_exits_expired() {
    let (client_in, svc_out) = pipe();
    let (_svc_in, client_out) = pipe();
    let mut to_client = FrameWriter::new(svc_out);
    to_client.write_message(&Message::HelloAck { version: IPC_VERSION, win_session: 3 }).unwrap();
    to_client.write_message(&Message::Bye { code: swapdesk_proto::ErrorCode::Timeout }).unwrap();
    let exit = elevated_client::run(client_in, client_out, &TICKET, |_, _, _| panic!("nobody is asked"));
    assert_eq!((exit, exit.code()), (ElevatedExit::Expired, 5));
}
