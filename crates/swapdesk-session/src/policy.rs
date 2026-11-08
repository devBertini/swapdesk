//! What each side may receive, what each request needs (PROTOCOL.md 4, 5.3, 6.1) and how grants are computed
//! (D8, 6.3, 6.4). Pure functions: the state machine applies them, the tests check them against the tables.

use swapdesk_proto::{Message, PermissionSet, StreamKind};

/// Which end of the session this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Shows its screen and executes: the QUIC server, the one that decides.
    Host,
    /// Looks and controls: the QUIC client.
    Viewer,
}

/// True for the messages only the session core handles: the handshake, keepalive, errors and the messages that
/// carry state the core owns. A feature crate cannot register these.
pub fn is_core_type(type_id: u16) -> bool {
    matches!(type_id, 0x0001..=0x000A | 0x0701..=0x0705 | 0x0F01 | 0x0F02 | 0x0FFF)
}

/// Whether `role` may receive `msg` from its peer at all (direction of 5.3). A message that fails this is a
/// `PROTOCOL_VIOLATION`.
pub fn may_receive(role: Role, msg: &Message) -> bool {
    use Message::*;
    match msg {
        // Both ways.
        Capabilities(_) | Bye(_) | Ping(_) | Pong(_) | Error(_) => true,
        SwapRequest(_) | SwapAccept(_) | SwapDecline(_) | SwapCancel(_) | SwapArmed(_) => true,
        ChatMessage(_) | ChatRead(_) | ClipboardSync(_) | ClipboardSkipped(_) => true,
        FileOffer(_) | FileAccept(_) | FileDecline(_) | FileDone(_) | FileVerified(_) | FilePause(_)
        | FileResume(_) | FileCancel(_) => true,
        // Viewer to host.
        Hello(_) | SelectDisplay(_) | VideoSettings(_) | KeyframeRequest(_) | ReceiverReport(_) | AudioControl(_)
        | SendSpecial(_) | DirRequest(_) | FileGet(_) | TermOpen(_) | TermResize(_) | TermClose(_) | QuickAction(_) => {
            role == Role::Host
        }
        // Host to viewer.
        HelloAck(_)
        | PromptPending(_)
        | SessionGrant(_)
        | SessionRefused(_)
        | PermissionsChanged(_)
        | HostState(_)
        | ConsentPending(_)
        | DisplayList(_)
        | StreamStats(_)
        | HostMute(_)
        | DirListing(_)
        | TermOpened(_)
        | TermExited(_)
        | QuickActionResult(_)
        | WakeInfo(_) => role == Role::Viewer,
        // The pairing endpoint only.
        PairHello(_) | PairNonce(_) | PairReveal(_) | PairConfirm(_) | PairResult(_) => false,
    }
}

/// Messages that may flow before `SessionGrant` (5.2 step 3): the handshake, the decision and keepalive.
pub fn allowed_before_grant(role: Role, msg: &Message) -> bool {
    use Message::*;
    match msg {
        Capabilities(_) | Bye(_) | Ping(_) | Pong(_) | Error(_) => true,
        Hello(_) => role == Role::Host,
        HelloAck(_) | PromptPending(_) | SessionGrant(_) | SessionRefused(_) => role == Role::Viewer,
        _ => false,
    }
}

/// The permissions a request from the viewer needs on the host (6.1 "Gates"). Empty: no permission.
pub fn required(msg: &Message) -> PermissionSet {
    use Message::*;
    match msg {
        SelectDisplay(_) | VideoSettings(_) | KeyframeRequest(_) | ReceiverReport(_) => PermissionSet::VIEW,
        // Turning audio off is always allowed: nothing is captured, encoded or sent (D16).
        AudioControl(m) if m.enabled => PermissionSet::AUDIO,
        // `UNATTENDED` sessions only (D11).
        SendSpecial(_) => PermissionSet::UNATTENDED,
        ClipboardSync(_) | ClipboardSkipped(_) => PermissionSet::CLIPBOARD,
        FileOffer(_) | FileAccept(_) | FileDecline(_) | FileDone(_) | FileVerified(_) | FilePause(_)
        | FileResume(_) | FileCancel(_) | DirRequest(_) | FileGet(_) => PermissionSet::FILES,
        TermOpen(m) if m.admin => PermissionSet::ADMIN_TERMINAL,
        TermOpen(_) | TermResize(_) | TermClose(_) => PermissionSet::TERMINAL,
        QuickAction(_) => PermissionSet::CONTROL,
        _ => PermissionSet::EMPTY,
    }
}

/// What the viewer's `Hello` must also have asked for (D10): an administrator terminal needs `ADMIN_TERMINAL` granted
/// and requested. Empty for everything else.
pub fn required_requested(msg: &Message) -> PermissionSet {
    match msg {
        Message::TermOpen(m) if m.admin => PermissionSet::ADMIN_TERMINAL,
        _ => PermissionSet::EMPTY,
    }
}

/// Whether a session holding `granted`, having asked for `requested`, may have `msg` done for it on the host.
pub fn permitted(granted: PermissionSet, requested: PermissionSet, msg: &Message) -> bool {
    granted.contains(required(msg)) && requested.contains(required_requested(msg))
}

/// The id a request carries, for `Error.about_id` (0 when it has none).
pub fn about_id(msg: &Message) -> u32 {
    use Message::*;
    match msg {
        SelectDisplay(m) => u32::from(m.display),
        KeyframeRequest(m) => u32::from(m.display),
        SwapRequest(m) => m.request_id,
        SwapAccept(m) => m.request_id,
        SwapDecline(m) => m.request_id,
        SwapCancel(m) => m.request_id,
        SwapArmed(m) => m.request_id,
        ChatMessage(m) => m.msg_id,
        FileOffer(m) => m.transfer_id,
        FileAccept(m) => m.transfer_id,
        FileDecline(m) => m.transfer_id,
        FileDone(m) => m.transfer_id,
        FileVerified(m) => m.transfer_id,
        FilePause(m) => m.transfer_id,
        FileResume(m) => m.transfer_id,
        FileCancel(m) => m.transfer_id,
        FileGet(m) => m.transfer_id,
        DirRequest(m) => m.req_id,
        TermOpen(m) => m.term_id,
        TermResize(m) => m.term_id,
        TermClose(m) => m.term_id,
        QuickAction(m) => m.action_id,
        _ => 0,
    }
}

/// Whether `role` may receive a stream of this kind (PROTOCOL.md 4, "Opened by"). `bidi` is the stream type.
pub fn stream_allowed(role: Role, kind: StreamKind, bidi: bool) -> bool {
    match (kind, bidi) {
        // Viewer to host.
        (StreamKind::Input, false) | (StreamKind::Terminal, true) => role == Role::Host,
        // Host to viewer.
        (StreamKind::Video, false) | (StreamKind::CursorShape, false) => role == Role::Viewer,
        // Either way.
        (StreamKind::Clipboard, false) | (StreamKind::FileData, false) => true,
        _ => false,
    }
}

/// The permission a stream of this kind needs on the host.
pub fn stream_required(kind: StreamKind) -> PermissionSet {
    match kind {
        StreamKind::Input => PermissionSet::CONTROL,
        StreamKind::Clipboard => PermissionSet::CLIPBOARD,
        StreamKind::FileData => PermissionSet::FILES,
        StreamKind::Terminal => PermissionSet::TERMINAL,
        StreamKind::Video | StreamKind::CursorShape => PermissionSet::EMPTY,
    }
}

/// Quinn send priority of a stream kind (PROTOCOL.md 4: higher goes first). The control stream is
/// [`crate::link::CONTROL_PRIORITY`].
pub fn priority_of(kind: StreamKind) -> i32 {
    match kind {
        StreamKind::Input => 4,
        StreamKind::CursorShape | StreamKind::Terminal => 2,
        StreamKind::Video => 1,
        StreamKind::Clipboard | StreamKind::FileData => 0,
    }
}

fn without(set: PermissionSet, mask: u16) -> PermissionSet {
    PermissionSet::closure(set.bits() & !mask)
}

/// `UNATTENDED` entry (F-SESS-02): only when the viewer asked for it and the device holds it; the grant is the
/// stored grants intersected with what was requested.
pub fn unattended_grant(stored: PermissionSet, requested: PermissionSet) -> Option<PermissionSet> {
    (requested.contains(PermissionSet::UNATTENDED) && stored.contains(PermissionSet::UNATTENDED))
        .then(|| stored & requested)
}

/// A prompted session (D8, F-SESS-01): what the host's user picked, but never `UNATTENDED` (entry without a prompt
/// is its own mode, decided by `unattended_grant`) and `ADMIN_TERMINAL` only when the device holds it and this
/// viewer asked for it (D10: a viewer whose user is not an administrator does not ask).
pub fn prompted_grant(stored: PermissionSet, picked: PermissionSet, requested: PermissionSet) -> PermissionSet {
    let mut mask = PermissionSet::BIT_UNATTENDED;
    if !(stored.contains(PermissionSet::ADMIN_TERMINAL) && requested.contains(PermissionSet::ADMIN_TERMINAL)) {
        mask |= PermissionSet::BIT_ADMIN_TERMINAL;
    }
    without(picked, mask)
}

/// The host's user changes a running session from the banner: the new set, except that `UNATTENDED` and
/// `ADMIN_TERMINAL` are never added to a session that does not hold them (they need an administrator in Permissões).
pub fn session_change(current: PermissionSet, new: PermissionSet) -> PermissionSet {
    let mut mask = 0;
    if !current.contains(PermissionSet::UNATTENDED) {
        mask |= PermissionSet::BIT_UNATTENDED;
    }
    if !current.contains(PermissionSet::ADMIN_TERMINAL) {
        mask |= PermissionSet::BIT_ADMIN_TERMINAL;
    }
    without(new, mask)
}

/// The stored grants of a device went from `seen` to `now`: a running session loses what was revoked (and what
/// depends on it) and keeps the rest, including what the host's user added for this session alone (D8). A change
/// never widens a running session.
pub fn without_revoked(granted: PermissionSet, seen: PermissionSet, now: PermissionSet) -> PermissionSet {
    let revoked = seen.bits() & !now.bits();
    let mut bits = granted.bits() & !revoked;
    if revoked & PermissionSet::BIT_VIEW != 0 {
        bits &= !(PermissionSet::BIT_CONTROL | PermissionSet::BIT_AUDIO | PermissionSet::BIT_UNATTENDED);
    }
    if revoked & PermissionSet::BIT_TERMINAL != 0 {
        bits &= !PermissionSet::BIT_ADMIN_TERMINAL;
    }
    PermissionSet::closure(bits)
}

/// A swap (6.3 step 5): the stored grants for that device, never `UNATTENDED` entry; limited to what was requested.
pub fn swap_grant(stored: PermissionSet, requested: PermissionSet) -> PermissionSet {
    without(stored & requested, PermissionSet::BIT_UNATTENDED)
}

/// A resumed session (6.4): the original grants intersected with the grants still stored, so a per-session addition
/// of D8 survives only if it is stored too.
pub fn resumed_grant(original: PermissionSet, stored: PermissionSet) -> PermissionSet {
    original & stored
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::*;

    fn all_messages() -> Vec<Message> {
        let caps = Capabilities {
            video_decode: vec![],
            audio: vec![],
            max_datagram: 0,
            keyboard: KeyboardInfo { kind: KbKind::Ansi, layout: String::new() },
            features: 0,
        };
        vec![
            Message::Hello(Hello {
                proto_minor: 0,
                app_version: String::new(),
                name: String::new(),
                os: Os::Windows,
                os_version: String::new(),
                intent: Intent::View,
                requested: PermissionSet::VIEW,
                resume: None,
                swap_token: None,
            }),
            Message::HelloAck(HelloAck {
                proto_minor: 0,
                app_version: String::new(),
                name: String::new(),
                os: Os::Windows,
                os_version: String::new(),
            }),
            Message::Capabilities(caps),
            Message::PromptPending(PromptPending { timeout_s: 20 }),
            Message::SessionGrant(SessionGrant {
                session_id: 1,
                granted: PermissionSet::VIEW,
                mode: GrantMode::Prompted,
                resume_token: [0; 16],
            }),
            Message::SessionRefused(SessionRefused { code: ErrorCode::Declined }),
            Message::PermissionsChanged(PermissionsChanged { granted: PermissionSet::VIEW }),
            Message::Bye(Bye { code: ErrorCode::NoError }),
            Message::HostState(HostState { desktop: Desktop::Default, paused: false }),
            Message::ConsentPending(ConsentPending { feature: Feature::Files, timeout_s: 20 }),
            Message::DisplayList(DisplayList { displays: vec![] }),
            Message::SelectDisplay(SelectDisplay { display: 0 }),
            Message::VideoSettings(VideoSettings {
                preset: Preset::Auto,
                max_fps: 60,
                max_kbps: 0,
                view_width: 0,
                view_height: 0,
            }),
            Message::KeyframeRequest(KeyframeRequest { display: 0, last_good_frame: 0 }),
            Message::ReceiverReport(ReceiverReport {
                frames_presented: 0,
                frames_lost: 0,
                decode_us_p50: 0,
                present_lag_us_p50: 0,
                audio_underruns: 0,
            }),
            Message::StreamStats(StreamStats { kbps: 0, fps: 0, encoder: String::new() }),
            Message::AudioControl(AudioControl { enabled: true }),
            Message::HostMute(HostMute { muted: false }),
            Message::SendSpecial(SendSpecial { key: Special::SecureAttention }),
            Message::SwapRequest(SwapRequest { request_id: 1 }),
            Message::SwapAccept(SwapAccept { request_id: 1, token: [0; 16] }),
            Message::SwapDecline(SwapDecline { request_id: 1, code: ErrorCode::Declined }),
            Message::SwapCancel(SwapCancel { request_id: 1 }),
            Message::SwapArmed(SwapArmed { request_id: 1 }),
            Message::ChatMessage(ChatMessage { msg_id: 1, text: String::new(), sent_at_ms: 0 }),
            Message::ChatRead(ChatRead { up_to: 1 }),
            Message::ClipboardSync(ClipboardSync { enabled: true }),
            Message::ClipboardSkipped(ClipboardSkipped { reason: ClipSkip::TooLarge }),
            Message::FileOffer(FileOffer { transfer_id: 1, name: String::new(), size: 0, mtime_ms: 0, dest: None }),
            Message::FileAccept(FileAccept { transfer_id: 1, resume_from: 0 }),
            Message::FileDecline(FileDecline { transfer_id: 1, code: ErrorCode::Declined }),
            Message::FileDone(FileDone { transfer_id: 1, sha256: [0; 32] }),
            Message::FileVerified(FileVerified { transfer_id: 1, ok: true }),
            Message::FilePause(FilePause { transfer_id: 1 }),
            Message::FileResume(FileResume { transfer_id: 1 }),
            Message::FileCancel(FileCancel { transfer_id: 1 }),
            Message::DirRequest(DirRequest { req_id: 1, path: None }),
            Message::DirListing(DirListing { req_id: 1, path: String::new(), entries: vec![], truncated: false }),
            Message::FileGet(FileGet { transfer_id: 1, path: String::new() }),
            Message::TermOpen(TermOpen { term_id: 1, cols: 80, rows: 24, admin: false }),
            Message::TermOpened(TermOpened { term_id: 1, account: String::new() }),
            Message::TermResize(TermResize { term_id: 1, cols: 80, rows: 24 }),
            Message::TermClose(TermClose { term_id: 1 }),
            Message::TermExited(TermExited { term_id: 1, exit_code: None }),
            Message::QuickAction(QuickAction { action_id: 1, kind: Action::Lock }),
            Message::QuickActionResult(QuickActionResult { action_id: 1, code: ErrorCode::NoError }),
            Message::WakeInfo(WakeInfo { macs: vec![] }),
            Message::PairHello(PairHello {
                name: String::new(),
                os: Os::Windows,
                os_version: String::new(),
                majors: vec![1],
                commit: [0; 32],
            }),
            Message::PairNonce(PairNonce {
                name: String::new(),
                os: Os::Windows,
                os_version: String::new(),
                majors: vec![1],
                nonce_r: [0; 32],
            }),
            Message::PairReveal(PairReveal { nonce_i: [0; 32] }),
            Message::PairConfirm(PairConfirm { matches: true }),
            Message::PairResult(PairResult { ok: true, code: ErrorCode::NoError }),
            Message::Ping(Ping { nonce: 1, t_send_us: 0 }),
            Message::Pong(Pong { nonce: 1, t_send_us: 0, t_peer_us: 0 }),
            Message::Error(ErrorMessage { code: ErrorCode::Internal, fatal: false, about_type: 0, about_id: 0 }),
        ]
    }

    #[test]
    fn the_table_covers_every_message_type() {
        let all = all_messages();
        let mut ids: Vec<u16> = all.iter().map(Message::type_id).collect();
        ids.sort_unstable();
        let mut want = Message::ALL_TYPE_IDS.to_vec();
        want.sort_unstable();
        assert_eq!(ids, want, "a message was added to the protocol: give it a direction and a permission here");
    }

    #[test]
    fn directions_match_section_5_3() {
        for m in all_messages() {
            let host = may_receive(Role::Host, &m);
            let viewer = may_receive(Role::Viewer, &m);
            match m.type_id() {
                // Pairing messages never travel on a session.
                0x0701..=0x0705 => assert!(!host && !viewer, "{m:?}"),
                // Viewer to host only.
                0x0001 | 0x0102..=0x0105 | 0x0107 | 0x0109 | 0x0409 | 0x040B | 0x0501 | 0x0503 | 0x0504 | 0x0601 => {
                    assert!(host && !viewer, "{m:?}")
                }
                // Host to viewer only.
                0x0002
                | 0x0004..=0x0007
                | 0x0009
                | 0x0101
                | 0x000A
                | 0x0106
                | 0x0108
                | 0x040A
                | 0x0502
                | 0x0505
                | 0x0602
                | 0x0603 => assert!(!host && viewer, "{m:?}"),
                _ => assert!(host && viewer, "{m:?}"),
            }
        }
    }

    #[test]
    fn requirements_match_the_gates_of_6_1() {
        let need = |m: &Message| required(m);
        for m in all_messages() {
            let n = need(&m);
            let want = match m.type_id() {
                0x0102..=0x0105 => PermissionSet::VIEW,
                0x0107 => PermissionSet::AUDIO,
                0x0109 => PermissionSet::UNATTENDED,
                0x0303 | 0x0304 => PermissionSet::CLIPBOARD,
                0x0401..=0x0408 | 0x0409 | 0x040B => PermissionSet::FILES,
                0x0501 | 0x0503 | 0x0504 => PermissionSet::TERMINAL,
                0x0601 => PermissionSet::CONTROL,
                _ => PermissionSet::EMPTY,
            };
            assert_eq!(n, want, "{m:?}");
        }
        assert_eq!(
            required(&Message::TermOpen(TermOpen { term_id: 1, cols: 1, rows: 1, admin: true })),
            PermissionSet::ADMIN_TERMINAL
        );
        assert_eq!(
            required(&Message::AudioControl(AudioControl { enabled: false })),
            PermissionSet::EMPTY,
            "turning audio off needs nothing"
        );
    }

    #[test]
    fn core_types_are_the_handshake_keepalive_and_state_messages() {
        for id in [
            0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A, 0x0701, 0x0F01, 0x0F02,
            0x0FFF,
        ] {
            assert!(is_core_type(id), "{id:#x}");
        }
        for id in [0x0101, 0x0102, 0x0201, 0x0301, 0x0401, 0x0501, 0x0601, 0x0603] {
            assert!(!is_core_type(id), "{id:#x}");
        }
    }

    #[test]
    fn stream_rules_follow_the_table_of_section_4() {
        use StreamKind::*;
        assert!(stream_allowed(Role::Host, Input, false) && !stream_allowed(Role::Viewer, Input, false));
        assert!(stream_allowed(Role::Viewer, Video, false) && !stream_allowed(Role::Host, Video, false));
        assert!(stream_allowed(Role::Viewer, CursorShape, false) && !stream_allowed(Role::Host, CursorShape, false));
        assert!(stream_allowed(Role::Host, Clipboard, false) && stream_allowed(Role::Viewer, Clipboard, false));
        assert!(stream_allowed(Role::Host, FileData, false) && stream_allowed(Role::Viewer, FileData, false));
        assert!(stream_allowed(Role::Host, Terminal, true) && !stream_allowed(Role::Viewer, Terminal, true));
        // A kind on the wrong stream type is never allowed.
        assert!(!stream_allowed(Role::Host, Terminal, false) && !stream_allowed(Role::Host, Input, true));
        assert_eq!([Input, Video, CursorShape, Clipboard, FileData, Terminal].map(priority_of), [4, 1, 2, 0, 0, 2]);
        assert_eq!(crate::link::CONTROL_PRIORITY, 3);
    }

    #[test]
    fn a_prompted_grant_never_adds_unattended_or_admin_terminal() {
        let stored = PermissionSet::VIEW;
        let everything =
            PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL | PermissionSet::FILES;
        let g = prompted_grant(stored, everything, everything);
        assert!(!g.contains(PermissionSet::UNATTENDED) && !g.contains(PermissionSet::ADMIN_TERMINAL));
        assert!(
            g.contains(PermissionSet::CONTROL) && g.contains(PermissionSet::FILES),
            "widening beyond the stored grants is allowed (D8)"
        );
        assert!(
            g.contains(PermissionSet::TERMINAL) == everything.contains(PermissionSet::TERMINAL),
            "TERMINAL stays: only the admin bit is stripped"
        );
        // Stage 37, finding 1: stored `UNATTENDED` is never kept by a prompted session (D8, F-SESS-01); stored
        // `ADMIN_TERMINAL` is kept only when the viewer asked for it as well.
        let stored = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL;
        let asked_all = everything;
        let got = prompted_grant(stored, everything, asked_all);
        assert!(!got.contains(PermissionSet::UNATTENDED));
        assert!(
            got.contains(PermissionSet::ADMIN_TERMINAL)
                && got.contains(PermissionSet::CONTROL)
                && got.contains(PermissionSet::FILES)
        );
        let got = prompted_grant(stored, everything, PermissionSet::CONTROL | PermissionSet::TERMINAL);
        assert!(!got.contains(PermissionSet::ADMIN_TERMINAL), "the viewer did not ask for it");
        assert!(got.contains(PermissionSet::TERMINAL));
        // Narrowing is allowed.
        assert_eq!(
            prompted_grant(PermissionSet::CONTROL | PermissionSet::FILES, PermissionSet::VIEW, PermissionSet::CONTROL),
            PermissionSet::VIEW
        );
    }

    #[test]
    fn a_banner_change_never_adds_the_administrator_bits() {
        let cur = PermissionSet::CONTROL;
        let all =
            PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL | PermissionSet::FILES;
        // `ADMIN_TERMINAL` implies `TERMINAL`, which the host's user may add for this session (D8).
        assert_eq!(session_change(cur, all), PermissionSet::CONTROL | PermissionSet::FILES | PermissionSet::TERMINAL);
        // A session that holds them may keep them, and may drop them.
        assert_eq!(session_change(all, all), all);
        assert_eq!(session_change(all, PermissionSet::VIEW), PermissionSet::VIEW);
    }

    #[test]
    fn a_stored_change_removes_only_what_was_revoked_and_what_depends_on_it() {
        let session = PermissionSet::CONTROL | PermissionSet::FILES | PermissionSet::ADMIN_TERMINAL;
        // Nothing revoked: the session is as it was, whatever else changed.
        assert_eq!(
            without_revoked(session, PermissionSet::VIEW, PermissionSet::VIEW | PermissionSet::TERMINAL),
            session
        );
        // FILES revoked.
        assert_eq!(
            without_revoked(session, CF_STORED, PermissionSet::CONTROL),
            PermissionSet::CONTROL | PermissionSet::ADMIN_TERMINAL
        );
        // VIEW revoked: what implies it goes too.
        assert_eq!(
            without_revoked(session, PermissionSet::CONTROL | PermissionSet::FILES, PermissionSet::FILES),
            PermissionSet::FILES | PermissionSet::ADMIN_TERMINAL
        );
        // TERMINAL revoked: ADMIN_TERMINAL implies it.
        assert_eq!(
            without_revoked(session, PermissionSet::ADMIN_TERMINAL, PermissionSet::EMPTY),
            PermissionSet::CONTROL | PermissionSet::FILES
        );
    }

    const CF_STORED: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

    #[test]
    fn an_administrator_terminal_needs_the_request_as_well_as_the_grant() {
        let open = Message::TermOpen(TermOpen { term_id: 1, cols: 80, rows: 24, admin: true });
        let plain = Message::TermOpen(TermOpen { term_id: 1, cols: 80, rows: 24, admin: false });
        assert!(permitted(PermissionSet::ADMIN_TERMINAL, PermissionSet::ADMIN_TERMINAL, &open));
        assert!(!permitted(PermissionSet::ADMIN_TERMINAL, PermissionSet::TERMINAL, &open), "granted but not requested");
        assert!(!permitted(PermissionSet::TERMINAL, PermissionSet::ADMIN_TERMINAL, &open), "requested but not granted");
        assert!(permitted(PermissionSet::TERMINAL, PermissionSet::EMPTY, &plain));
    }

    #[test]
    fn unattended_swap_and_resumed_grants() {
        let stored = PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES;
        assert_eq!(
            unattended_grant(stored, PermissionSet::UNATTENDED | PermissionSet::CONTROL),
            Some(PermissionSet::UNATTENDED | PermissionSet::CONTROL)
        );
        assert_eq!(unattended_grant(stored, PermissionSet::CONTROL), None, "the viewer did not ask for UNATTENDED");
        assert_eq!(
            unattended_grant(PermissionSet::CONTROL, PermissionSet::UNATTENDED),
            None,
            "the device does not hold UNATTENDED"
        );
        let swap = swap_grant(stored, PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES);
        assert!(!swap.contains(PermissionSet::UNATTENDED), "a swap never grants UNATTENDED entry");
        assert!(swap.contains(PermissionSet::CONTROL) && swap.contains(PermissionSet::FILES));
        assert_eq!(
            resumed_grant(PermissionSet::CONTROL | PermissionSet::FILES, PermissionSet::CONTROL),
            PermissionSet::CONTROL
        );
        assert_eq!(resumed_grant(PermissionSet::CONTROL, PermissionSet::EMPTY), PermissionSet::EMPTY);
    }
}
