//! Bodies of `UiRequest` (by op), `UiReply` (by the op it answers) and `UiNotify` (by kind), C2.4.

use swapdesk_proto::{Action, CodecError, Feature, Intent, Preset, Reader, Wire, Writer};

use crate::error::{IpcError, Result};
use crate::files::{FileListing, FilesNote, FilesOp};
use crate::redact::{ElevationTicket, Text};
use crate::term::{MAX_TERM_ACCOUNT, TermOutcome};
use crate::types::*;

type CResult<T> = core::result::Result<T, CodecError>;

/// `UiRequest.op` and its body. `grants` are the raw `u16` bit set of PROTOCOL.md 6.1: the service validates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiOp {
    GetThisDevice,
    GetNetwork,
    ListDevices,
    AddByAddress {
        ipv4: String,
    },
    PairOpen {
        device_id: Option<String>,
    },
    PairStart {
        pairing_id: u32,
        device_id: String,
    },
    PairConfirm {
        pairing_id: u32,
        matches: bool,
    },
    PairCancel {
        pairing_id: u32,
    },
    SetGrants {
        device_id: String,
        grants: u16,
    },
    ForgetDevice {
        device_id: String,
    },
    Connect {
        device_id: String,
        intent: Intent,
        unattended: bool,
    },
    Disconnect {
        session_id: u64,
    },
    AnswerIncoming {
        request_id: u32,
        decision: Decision,
        grants: u16,
    },
    SelectDisplay {
        session_id: u64,
        display: u16,
    },
    SetQuality {
        session_id: u64,
        quality: Preset,
    },
    SetAudio {
        session_id: u64,
        on: bool,
    },
    SetViewOnly {
        session_id: u64,
        on: bool,
    },
    SendKeys {
        session_id: u64,
        keys: SpecialKeys,
    },
    GetSettings,
    SetSettings(SettingsUpdate),
    ActivityList {
        before_ms: Option<u64>,
        limit: u32,
    },
    /// "Inverter": ask the other side of the session to swap (PROTOCOL.md 6.3). The reply carries this request's
    /// number at once; the outcome arrives as `Notify::Swap`.
    RequestSwap {
        session_id: u64,
    },
    /// The user answered the swap card (`SwapNote::Asked`).
    AnswerSwap {
        request_id: u32,
        accept: bool,
    },
    /// Send a chat message; the reply carries it as stored (id, time).
    ChatSend {
        session_id: u64,
        /// Chat text is never logged: `Text` prints its length only.
        text: Text,
    },
    /// The chat panel shows everything the peer sent: the peer is told (`ChatRead`).
    ChatMarkRead {
        session_id: u64,
    },
    /// Wake a paired device and connect when it appears (F-WAKE-01). The reply carries the id of the waking entry:
    /// progress arrives as `Notify::Session` with `SessionState::Waking`, and the entry ends when the connection
    /// opens (its own session then goes through the usual `Session` notifications) or the 60 s run out.
    Wake {
        device_id: String,
        intent: Intent,
    },
    /// The display list of a viewer session of this connection, as the host last sent it (the list `Notify::Displays`
    /// carries; empty until the host's first list arrived).
    ListDisplays {
        session_id: u64,
    },
    /// The "Copiar e colar" switch of a session: this side's flag; the peer is told.
    SetClipboardSync {
        session_id: u64,
        on: bool,
    },
    /// A quick action on the host of a viewer session. The reply comes when the host answered (`QuickActionResult`):
    /// `Done` when it ran, else the code of why it did not.
    RunAction {
        session_id: u64,
        action: Action,
    },
    /// Give a paired device a local name (the alias this machine shows for it; the peer is not told). Reply `Done`.
    RenameDevice {
        device_id: String,
        name: String,
    },
    /// Open a terminal on the host of viewer session `session_id` (F-TERM-01, F-TERM-02). The reply (op 32) carries
    /// the connection-wide `term_id` and how the open ended; it comes when the shell runs or was refused, which for
    /// the D13 question of an unattended session can take up to 20 s. `admin` needs an administrator connection.
    TermOpen {
        session_id: u64,
        cols: u16,
        rows: u16,
        admin: bool,
    },
    /// The grid of terminal `term_id` changed size.
    TermResize {
        term_id: u32,
        cols: u16,
        rows: u16,
    },
    /// Close terminal `term_id` (no `TermExited` follows: the UI asked for it).
    TermClose {
        term_id: u32,
    },
    /// Open an administrator confirmation for `action` (op 35). Sent by role `Ui`, the console's connection only; the
    /// reply carries the ticket the elevated instance presents.
    ElevationBegin {
        action: ElevAction,
    },
    /// The elevated instance asks what ticket `ticket` is for (op 36). Sent by role `Elevator` only.
    ElevationDescribe {
        ticket: ElevationTicket,
    },
    /// The elevated instance's answer to the confirmation window (op 37): `approve` false closes the ticket. Sent
    /// by role `Elevator` only. Reply `Done`.
    ElevationConfirm {
        ticket: ElevationTicket,
        approve: bool,
    },
    /// The asking connection is done with `ticket` (op 38); the reply says how it ended. Sent by role `Ui`, the
    /// asking connection only.
    ElevationEnd {
        ticket: ElevationTicket,
    },
    /// Op 39: the interface opened a paired device's Permissões: the service syncs that device's grants if its last
    /// sync is older than 30 s (GRANTS-SYNC.md 6). `Done` at once; the result arrives as `DevicesChanged`.
    RefreshGrants {
        device_id: String,
    },
    /// Ops 40 to 49: the files feature (stage 44), see [`FilesOp`].
    Files(FilesOp),
}

impl UiOp {
    /// The `op` number on the wire.
    pub const fn code(&self) -> u16 {
        match self {
            Self::GetThisDevice => 1,
            Self::GetNetwork => 2,
            Self::ListDevices => 3,
            Self::AddByAddress { .. } => 4,
            Self::PairOpen { .. } => 5,
            Self::PairStart { .. } => 6,
            Self::PairConfirm { .. } => 7,
            Self::PairCancel { .. } => 8,
            Self::SetGrants { .. } => 9,
            Self::ForgetDevice { .. } => 10,
            Self::Connect { .. } => 11,
            Self::Disconnect { .. } => 12,
            Self::AnswerIncoming { .. } => 13,
            Self::SelectDisplay { .. } => 14,
            Self::SetQuality { .. } => 15,
            Self::SetAudio { .. } => 16,
            Self::SetViewOnly { .. } => 17,
            Self::SendKeys { .. } => 18,
            Self::GetSettings => 19,
            Self::SetSettings(_) => 20,
            Self::ActivityList { .. } => 21,
            Self::RequestSwap { .. } => 22,
            Self::AnswerSwap { .. } => 23,
            Self::ChatSend { .. } => 24,
            Self::ChatMarkRead { .. } => 25,
            Self::Wake { .. } => 26,
            Self::ListDisplays { .. } => 27,
            Self::SetClipboardSync { .. } => 28,
            Self::RunAction { .. } => 29,
            Self::RenameDevice { .. } => 30,
            Self::TermOpen { .. } => 32,
            Self::TermResize { .. } => 33,
            Self::TermClose { .. } => 34,
            Self::ElevationBegin { .. } => 35,
            Self::ElevationDescribe { .. } => 36,
            Self::ElevationConfirm { .. } => 37,
            Self::ElevationEnd { .. } => 38,
            Self::RefreshGrants { .. } => 39,
            Self::Files(f) => f.code(),
        }
    }

    pub(crate) fn put_body(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::GetThisDevice | Self::GetNetwork | Self::ListDevices | Self::GetSettings => {}
            Self::AddByAddress { ipv4 } => w.put_str(ipv4, MAX_IPV4)?,
            Self::PairOpen { device_id } => device_id.put(w, MAX_NAME)?,
            Self::PairStart { pairing_id, device_id } => {
                w.put_u32(*pairing_id);
                w.put_str(device_id, MAX_NAME)?;
            }
            Self::PairConfirm { pairing_id, matches } => {
                w.put_u32(*pairing_id);
                w.put_bool(*matches);
            }
            Self::PairCancel { pairing_id } => w.put_u32(*pairing_id),
            Self::SetGrants { device_id, grants } => {
                w.put_str(device_id, MAX_NAME)?;
                w.put_u16(*grants);
            }
            Self::ForgetDevice { device_id } => w.put_str(device_id, MAX_NAME)?,
            Self::Connect { device_id, intent, unattended } => {
                w.put_str(device_id, MAX_NAME)?;
                intent.put(w, 0)?;
                w.put_bool(*unattended);
            }
            Self::Disconnect { session_id } => w.put_u64(*session_id),
            Self::AnswerIncoming { request_id, decision, grants } => {
                w.put_u32(*request_id);
                decision.put(w, 0)?;
                w.put_u16(*grants);
            }
            Self::SelectDisplay { session_id, display } => {
                w.put_u64(*session_id);
                w.put_u16(*display);
            }
            Self::SetQuality { session_id, quality } => {
                w.put_u64(*session_id);
                quality.put(w, 0)?;
            }
            Self::SetAudio { session_id, on } | Self::SetViewOnly { session_id, on } => {
                w.put_u64(*session_id);
                w.put_bool(*on);
            }
            Self::SendKeys { session_id, keys } => {
                w.put_u64(*session_id);
                keys.put(w, 0)?;
            }
            Self::SetSettings(s) => s.put(w, 0)?,
            Self::ActivityList { before_ms, limit } => {
                before_ms.put(w, 0)?;
                w.put_u32(*limit);
            }
            Self::RequestSwap { session_id }
            | Self::ChatMarkRead { session_id }
            | Self::ListDisplays { session_id } => w.put_u64(*session_id),
            Self::AnswerSwap { request_id, accept } => {
                w.put_u32(*request_id);
                w.put_bool(*accept);
            }
            Self::ChatSend { session_id, text } => {
                w.put_u64(*session_id);
                w.put_str(&text.0, MAX_CHAT_TEXT)?;
            }
            Self::Wake { device_id, intent } => {
                w.put_str(device_id, MAX_NAME)?;
                intent.put(w, 0)?;
            }
            Self::SetClipboardSync { session_id, on } => {
                w.put_u64(*session_id);
                w.put_bool(*on);
            }
            Self::RunAction { session_id, action } => {
                w.put_u64(*session_id);
                action.put(w, 0)?;
            }
            Self::RenameDevice { device_id, name } => {
                w.put_str(device_id, MAX_NAME)?;
                w.put_str(name, MAX_NAME)?;
            }
            Self::TermOpen { session_id, cols, rows, admin } => {
                w.put_u64(*session_id);
                w.put_u16(*cols);
                w.put_u16(*rows);
                w.put_bool(*admin);
            }
            Self::TermResize { term_id, cols, rows } => {
                w.put_u32(*term_id);
                w.put_u16(*cols);
                w.put_u16(*rows);
            }
            Self::TermClose { term_id } => w.put_u32(*term_id),
            Self::ElevationBegin { action } => action.put(w, 0)?,
            Self::ElevationDescribe { ticket } | Self::ElevationEnd { ticket } => ticket.put(w, 0)?,
            Self::RefreshGrants { device_id } => w.put_str(device_id, MAX_NAME)?,
            Self::ElevationConfirm { ticket, approve } => {
                ticket.put(w, 0)?;
                w.put_bool(*approve);
            }
            Self::Files(f) => f.put_body(w)?,
        }
        Ok(())
    }

    pub(crate) fn get_body(op: u16, r: &mut Reader<'_>) -> Result<Self> {
        Ok(match op {
            1 => Self::GetThisDevice,
            2 => Self::GetNetwork,
            3 => Self::ListDevices,
            4 => Self::AddByAddress { ipv4: r.str(MAX_IPV4)? },
            5 => Self::PairOpen { device_id: <Option<String>>::get(r, MAX_NAME)? },
            6 => Self::PairStart { pairing_id: r.u32()?, device_id: r.str(MAX_NAME)? },
            7 => Self::PairConfirm { pairing_id: r.u32()?, matches: r.bool()? },
            8 => Self::PairCancel { pairing_id: r.u32()? },
            9 => Self::SetGrants { device_id: r.str(MAX_NAME)?, grants: r.u16()? },
            10 => Self::ForgetDevice { device_id: r.str(MAX_NAME)? },
            11 => Self::Connect { device_id: r.str(MAX_NAME)?, intent: Intent::get(r, 0)?, unattended: r.bool()? },
            12 => Self::Disconnect { session_id: r.u64()? },
            13 => Self::AnswerIncoming { request_id: r.u32()?, decision: Decision::get(r, 0)?, grants: r.u16()? },
            14 => Self::SelectDisplay { session_id: r.u64()?, display: r.u16()? },
            15 => Self::SetQuality { session_id: r.u64()?, quality: Preset::get(r, 0)? },
            16 => Self::SetAudio { session_id: r.u64()?, on: r.bool()? },
            17 => Self::SetViewOnly { session_id: r.u64()?, on: r.bool()? },
            18 => Self::SendKeys { session_id: r.u64()?, keys: SpecialKeys::get(r, 0)? },
            19 => Self::GetSettings,
            20 => Self::SetSettings(SettingsUpdate::get(r, 0)?),
            21 => Self::ActivityList { before_ms: <Option<u64>>::get(r, 0)?, limit: r.u32()? },
            22 => Self::RequestSwap { session_id: r.u64()? },
            23 => Self::AnswerSwap { request_id: r.u32()?, accept: r.bool()? },
            24 => Self::ChatSend { session_id: r.u64()?, text: Text(r.str(MAX_CHAT_TEXT)?) },
            25 => Self::ChatMarkRead { session_id: r.u64()? },
            26 => Self::Wake { device_id: r.str(MAX_NAME)?, intent: Intent::get(r, 0)? },
            27 => Self::ListDisplays { session_id: r.u64()? },
            28 => Self::SetClipboardSync { session_id: r.u64()?, on: r.bool()? },
            29 => Self::RunAction { session_id: r.u64()?, action: Action::get(r, 0)? },
            30 => Self::RenameDevice { device_id: r.str(MAX_NAME)?, name: r.str(MAX_NAME)? },
            32 => Self::TermOpen { session_id: r.u64()?, cols: r.u16()?, rows: r.u16()?, admin: r.bool()? },
            33 => Self::TermResize { term_id: r.u32()?, cols: r.u16()?, rows: r.u16()? },
            34 => Self::TermClose { term_id: r.u32()? },
            35 => Self::ElevationBegin { action: ElevAction::get(r, 0)? },
            36 => Self::ElevationDescribe { ticket: ElevationTicket::get(r, 0)? },
            37 => Self::ElevationConfirm { ticket: ElevationTicket::get(r, 0)?, approve: r.bool()? },
            38 => Self::ElevationEnd { ticket: ElevationTicket::get(r, 0)? },
            39 => Self::RefreshGrants { device_id: r.str(MAX_NAME)? },
            40..=49 => Self::Files(FilesOp::get_body(op, r)?),
            other => return Err(IpcError::Codec(unknown_op(other))),
        })
    }
}

fn unknown_op(op: u16) -> CodecError {
    CodecError::UnknownTag { what: "UiRequest op", tag: u64::from(op) }
}

/// The body of a successful `UiReply`, by the op it answers (the C6 `Reply` of the same command). The wire body
/// carries no tag: [`ReplyBody::decode`] needs the op of the request with the same `req`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyBody {
    /// Empty body (ops 6 to 10, 12 to 18, 20, 23, 25, 28 to 30, 33, 34, 37, 43 to 45).
    Done,
    ThisDevice(ThisDevice),
    Network(NetworkStatus),
    Devices(Vec<DeviceView>),
    Device(DeviceView),
    PairingId(u32),
    SessionId(u64),
    /// The number of a swap request (op 22).
    RequestId(u32),
    /// The chat message as stored (op 24).
    Chat(ChatItem),
    Settings(Settings),
    Activity(Vec<AuditView>),
    /// The display list of a viewer session (op 27).
    Displays(Vec<DisplayView>),
    /// How a terminal open ended (op 32): the connection-wide id, the outcome and, when it opened, the account the
    /// shell runs as (empty otherwise).
    TermOpened {
        term_id: u32,
        outcome: TermOutcome,
        account: Text,
    },
    /// The ticket of an administrator confirmation and how long it lives (op 35).
    ElevationTicket {
        ticket: ElevationTicket,
        ttl_s: u16,
    },
    /// What a ticket is for, the name of the peer it concerns and the seconds left (op 36).
    ElevationAction {
        action: ElevAction,
        peer_name: String,
        remaining_s: u16,
    },
    /// How a ticket ended (op 38).
    ElevationOutcome(ElevationOutcome),
    /// A folder of the host (op 40).
    Listing(FileListing),
    /// The transfer numbers of `FilesSend`, one per pick (op 41).
    TransferIds(Vec<u32>),
    /// The transfer number of `FilesGet` (op 42).
    TransferId(u32),
}

impl ReplyBody {
    /// The body bytes to put in `Message::UiReply`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        match self {
            Self::Done => {}
            Self::ThisDevice(v) => v.put(&mut w, 0)?,
            Self::Network(v) => v.put(&mut w, 0)?,
            Self::Devices(v) => v.put(&mut w, MAX_DEVICES)?,
            Self::Device(v) => v.put(&mut w, 0)?,
            Self::PairingId(v) => w.put_u32(*v),
            Self::SessionId(v) => w.put_u64(*v),
            Self::RequestId(v) => w.put_u32(*v),
            Self::Chat(v) => v.put(&mut w, 0)?,
            Self::Settings(v) => v.put(&mut w, 0)?,
            Self::Activity(v) => v.put(&mut w, MAX_ACTIVITY)?,
            Self::Displays(v) => v.put(&mut w, MAX_DISPLAYS)?,
            Self::TermOpened { term_id, outcome, account } => {
                w.put_u32(*term_id);
                outcome.put(&mut w, 0)?;
                account.put(&mut w, MAX_TERM_ACCOUNT)?;
            }
            Self::ElevationTicket { ticket, ttl_s } => {
                ticket.put(&mut w, 0)?;
                w.put_u16(*ttl_s);
            }
            Self::ElevationAction { action, peer_name, remaining_s } => {
                action.put(&mut w, 0)?;
                w.put_str(peer_name, MAX_NAME)?;
                w.put_u16(*remaining_s);
            }
            Self::ElevationOutcome(v) => v.put(&mut w, 0)?,
            Self::Listing(v) => v.put(&mut w, 0)?,
            Self::TransferIds(v) => v.put(&mut w, crate::files::MAX_PICKS)?,
            Self::TransferId(v) => w.put_u32(*v),
        }
        Ok(w.into_bytes())
    }

    /// Decodes the body of the reply to the request with op number `op`; trailing bytes are an error.
    pub fn decode(op: u16, body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let v = match op {
            1 => Self::ThisDevice(ThisDevice::get(&mut r, 0)?),
            2 => Self::Network(NetworkStatus::get(&mut r, 0)?),
            3 => Self::Devices(<Vec<DeviceView>>::get(&mut r, MAX_DEVICES)?),
            4 => Self::Device(DeviceView::get(&mut r, 0)?),
            5 => Self::PairingId(r.u32()?),
            11 | 26 => Self::SessionId(r.u64()?),
            22 => Self::RequestId(r.u32()?),
            27 => Self::Displays(<Vec<DisplayView>>::get(&mut r, MAX_DISPLAYS)?),
            24 => Self::Chat(ChatItem::get(&mut r, 0)?),
            19 => Self::Settings(Settings::get(&mut r, 0)?),
            21 => Self::Activity(<Vec<AuditView>>::get(&mut r, MAX_ACTIVITY)?),
            32 => Self::TermOpened {
                term_id: r.u32()?,
                outcome: TermOutcome::get(&mut r, 0)?,
                account: Text::get(&mut r, MAX_TERM_ACCOUNT)?,
            },
            35 => Self::ElevationTicket { ticket: ElevationTicket::get(&mut r, 0)?, ttl_s: r.u16()? },
            36 => Self::ElevationAction {
                action: ElevAction::get(&mut r, 0)?,
                peer_name: r.str(MAX_NAME)?,
                remaining_s: r.u16()?,
            },
            38 => Self::ElevationOutcome(ElevationOutcome::get(&mut r, 0)?),
            40 => Self::Listing(FileListing::get(&mut r, 0)?),
            41 => Self::TransferIds(<Vec<u32>>::get(&mut r, crate::files::MAX_PICKS)?),
            42 => Self::TransferId(r.u32()?),
            6..=10 | 12..=18 | 20 | 23 | 25 | 28..=30 | 33 | 34 | 37 | 39 | 43..=45 => Self::Done,
            other => return Err(IpcError::Codec(unknown_op(other))),
        };
        r.finish()?;
        Ok(v)
    }
}

/// `UiReply.status` for a success.
pub const STATUS_OK: u16 = 0;

/// The `ApiCode` a `UiReply.status` carries (`None` for ok).
pub fn status_code(status: u16) -> Result<Option<ApiCode>> {
    if status == STATUS_OK {
        return Ok(None);
    }
    u8::try_from(status - 1).ok().and_then(ApiCode::from_tag).map(Some).ok_or(IpcError::BadStatus(status))
}

/// `UiNotify.kind` and its body (the C6 `UiEvent` of the same name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notify {
    DevicesChanged(Vec<DeviceView>),
    NetworkChanged(NetworkStatus),
    Pairing {
        pairing_id: u32,
        state: PairingState,
    },
    IncomingRequest(IncomingRequest),
    IncomingClosed {
        request_id: u32,
        reason: ApiCode,
    },
    Session {
        session_id: u64,
        device_id: String,
        state: SessionState,
    },
    Displays {
        session_id: u64,
        list: Vec<DisplayView>,
    },
    Stats {
        session_id: u64,
        latency_ms: u32,
        fps: u16,
        kbps: u32,
    },
    ActivityAlert(AuditView),
    SettingsChanged(Settings),
    /// The swap card of a session (this machine asked, or is asked).
    Swap {
        session_id: u64,
        note: SwapNote,
    },
    /// A chat message from the peer of the session.
    Chat {
        session_id: u64,
        item: ChatItem,
    },
    /// The peer read this machine's messages up to `up_to`.
    ChatRead {
        session_id: u64,
        up_to: u32,
    },
    /// An item of the clipboard was not sent, by this side or by the peer (kind 14). `reason` is `Invalid` for too
    /// large, `NotPermitted` for concealed content and `Refused` for a format this version does not carry.
    ClipboardSkipped {
        session_id: u64,
        reason: ApiCode,
    },
    /// Terminal `term_id` ended by itself, after its last `TermOutput` (the shell exited, the host revoked the
    /// grant, or the session ended): `exit_code` is the shell's, when it left one.
    TermExited {
        term_id: u32,
        exit_code: Option<i32>,
    },
    /// The host is asking its user whether this viewer session may use `feature` (D13): the question runs for
    /// `remaining_s` more seconds. The answer arrives as the outcome of the request that caused it.
    ConsentPending {
        session_id: u64,
        feature: Feature,
        remaining_s: u16,
    },
    /// Kinds 20 to 24: the files feature (stage 44), see [`FilesNote`].
    Files(FilesNote),
}

impl Notify {
    /// The `kind` number on the wire.
    pub const fn kind(&self) -> u16 {
        match self {
            Self::DevicesChanged(_) => 1,
            Self::NetworkChanged(_) => 2,
            Self::Pairing { .. } => 3,
            Self::IncomingRequest(_) => 4,
            Self::IncomingClosed { .. } => 5,
            Self::Session { .. } => 6,
            Self::Displays { .. } => 7,
            Self::Stats { .. } => 8,
            Self::ActivityAlert(_) => 9,
            Self::SettingsChanged(_) => 10,
            Self::Swap { .. } => 11,
            Self::Chat { .. } => 12,
            Self::ChatRead { .. } => 13,
            Self::ClipboardSkipped { .. } => 14,
            Self::TermExited { .. } => 16,
            Self::ConsentPending { .. } => 17,
            Self::Files(f) => f.kind(),
        }
    }

    pub(crate) fn put_body(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::DevicesChanged(v) => v.put(w, MAX_DEVICES)?,
            Self::NetworkChanged(v) => v.put(w, 0)?,
            Self::Pairing { pairing_id, state } => {
                w.put_u32(*pairing_id);
                state.put(w, 0)?;
            }
            Self::IncomingRequest(v) => v.put(w, 0)?,
            Self::IncomingClosed { request_id, reason } => {
                w.put_u32(*request_id);
                reason.put(w, 0)?;
            }
            Self::Session { session_id, device_id, state } => {
                w.put_u64(*session_id);
                w.put_str(device_id, MAX_NAME)?;
                state.put(w, 0)?;
            }
            Self::Displays { session_id, list } => {
                w.put_u64(*session_id);
                list.put(w, MAX_DISPLAYS)?;
            }
            Self::Stats { session_id, latency_ms, fps, kbps } => {
                w.put_u64(*session_id);
                w.put_u32(*latency_ms);
                w.put_u16(*fps);
                w.put_u32(*kbps);
            }
            Self::ActivityAlert(v) => v.put(w, 0)?,
            Self::SettingsChanged(v) => v.put(w, 0)?,
            Self::Swap { session_id, note } => {
                w.put_u64(*session_id);
                note.put(w, 0)?;
            }
            Self::Chat { session_id, item } => {
                w.put_u64(*session_id);
                item.put(w, 0)?;
            }
            Self::ChatRead { session_id, up_to } => {
                w.put_u64(*session_id);
                w.put_u32(*up_to);
            }
            Self::ClipboardSkipped { session_id, reason } => {
                w.put_u64(*session_id);
                reason.put(w, 0)?;
            }
            Self::TermExited { term_id, exit_code } => {
                w.put_u32(*term_id);
                exit_code.put(w, 0)?;
            }
            Self::ConsentPending { session_id, feature, remaining_s } => {
                w.put_u64(*session_id);
                feature.put(w, 0)?;
                w.put_u16(*remaining_s);
            }
            Self::Files(f) => f.put_body(w)?,
        }
        Ok(())
    }

    pub(crate) fn get_body(kind: u16, r: &mut Reader<'_>) -> Result<Self> {
        Ok(match kind {
            1 => Self::DevicesChanged(<Vec<DeviceView>>::get(r, MAX_DEVICES)?),
            2 => Self::NetworkChanged(NetworkStatus::get(r, 0)?),
            3 => Self::Pairing { pairing_id: r.u32()?, state: PairingState::get(r, 0)? },
            4 => Self::IncomingRequest(IncomingRequest::get(r, 0)?),
            5 => Self::IncomingClosed { request_id: r.u32()?, reason: ApiCode::get(r, 0)? },
            6 => Self::Session { session_id: r.u64()?, device_id: r.str(MAX_NAME)?, state: SessionState::get(r, 0)? },
            7 => Self::Displays { session_id: r.u64()?, list: <Vec<DisplayView>>::get(r, MAX_DISPLAYS)? },
            8 => Self::Stats { session_id: r.u64()?, latency_ms: r.u32()?, fps: r.u16()?, kbps: r.u32()? },
            9 => Self::ActivityAlert(AuditView::get(r, 0)?),
            10 => Self::SettingsChanged(Settings::get(r, 0)?),
            11 => Self::Swap { session_id: r.u64()?, note: SwapNote::get(r, 0)? },
            12 => Self::Chat { session_id: r.u64()?, item: ChatItem::get(r, 0)? },
            13 => Self::ChatRead { session_id: r.u64()?, up_to: r.u32()? },
            14 => Self::ClipboardSkipped { session_id: r.u64()?, reason: ApiCode::get(r, 0)? },
            16 => Self::TermExited { term_id: r.u32()?, exit_code: <Option<i32>>::get(r, 0)? },
            17 => Self::ConsentPending { session_id: r.u64()?, feature: Feature::get(r, 0)?, remaining_s: r.u16()? },
            20..=24 => Self::Files(FilesNote::get_body(kind, r)?),
            other => {
                return Err(IpcError::Codec(CodecError::UnknownTag { what: "UiNotify kind", tag: u64::from(other) }));
            }
        })
    }
}
