# Swapdesk wire protocol, major version 1 (contract C1)

Implemented by `crates/swapdesk-proto` (types, codec, limits; no I/O, no OS, no `unsafe`) and used by `swapdesk-trust` (pairing, TLS), `swapdesk-session` (control stream), `swapdesk-video` and `swapdesk-audio` (media). Flow IDs refer to `docs/SPEC.md`, decisions D1 to D19 to `docs/DECISIONS.md`. **UNPROVEN** and **GUESS** as in the SPEC. Sources read on 2026-11-07 are cited inline; the rest come from `docs/research/`.

A change to this file after M0 is a contract change: the main session makes it, bumps `PROTO_MINOR` (additive) or the major (anything else), and every lane rebases.

## 1. Transport, endpoints, ALPN and versions

- QUIC v1 through `quinn` 0.11 with `rustls` 0.23 and the ring provider (quinn feature `rustls-ring`, in quinn's default set: https://docs.rs/crate/quinn/latest/features, read 2026-11-07). IPv4 only in v1.
- **Two endpoints per approved interface address, each with its own `ServerConfig`** (a `ClientCertVerifier` sees no ALPN, and quinn's `Incoming::accept_with(server_config)` fixes the configuration before the ClientHello is read, so per-ALPN rules cannot live in one endpoint):

| Endpoint | UDP port | ALPN | Open | Server verifier | Transport limits |
|---|---|---|---|---|---|
| Session | 47820 (GUESS: not checked against the IANA registry; configurable) | `swapdesk/1` only (already `swapdesk_proto::ALPN`) | while the service runs | pinned keys only (2.2) | section 1, "Session limits" |
| Pairing | 47821 (GUESS) | `swapdesk-pair/1` only | only while this machine is in pairing mode (D12, section 3); otherwise the socket is closed and nothing answers | any well-formed Ed25519 key (2.2) | 1 bidirectional stream, 0 unidirectional, no datagrams, receive windows 64 KiB, idle timeout 10 s with keep-alive 2 s (the 120 s comparison keeps it open), one connection at a time |

- A ClientHello that offers both ALPNs, or the other endpoint's ALPN, is refused (the certificate resolver sees the ClientHello and returns nothing; UNPROVEN: the rustls `ClientHello::alpn()` accessor is not re-read today, stage 10 checks it).
- Sockets are `std::net::UdpSocket` with `set_ttl(1)`, handed to `quinn::Endpoint::new(config, server_config, socket, runtime)`; the impairment layer (C7) uses `Endpoint::new_with_abstract_socket` (https://docs.rs/quinn/latest/quinn/struct.Endpoint.html, read 2026-11-07).
- **Before any handshake** each `quinn::Incoming` is checked: the remote address must be on-link in the subnet of the approved interface that owns `Incoming::local_ip()`, and must not be loopback or one of this machine's addresses; blocked addresses are dropped. Otherwise `refuse()` (https://docs.rs/quinn/latest/quinn/struct.Incoming.html, read 2026-11-07: `local_ip` "The local IP address which was used when the peer established the connection").
- **Load:** at most 16 handshakes in flight per machine and 4 connections per remote address. Every `Incoming` whose `remote_address_validated()` is false gets `retry()` first and takes no place (a forged source address cannot fill a peer's places or the handshake cap); a validated one takes a place of its address, and the 5th at once gets `ignore()` ("not sending any packet in response", same page). Honest peers pay one extra round trip.
- **Major version** = the number in the ALPN. A client offers every major it speaks on the session endpoint, newest first; the server picks the highest; no common ALPN fails the handshake (the UI says "versão incompatível").
- **Minor version** = `PROTO_MINOR: u16` in `Hello`/`HelloAck` (starts at 0). A peer sends a message type or an enum value introduced in minor N only when the other side's minor is ≥ N. So an unknown type or tag is always a protocol violation (fatal), never "ignore". Layouts of existing messages never change within a major: the bytes of every 1.0 message are kept as test fixtures (section 11).
- **What each minor added.** The code names the minor of every type and value (`Message::type_min_minor`, `ErrorCode::min_minor`, `Desktop::min_minor`), and `Message::min_minor` gives the minor a peer must have announced to be sent a message: the newest of its type and of the enum values it carries.

| Minor | First release | Message types | Enum values | Toward an older peer |
|---|---|---|---|---|
| 0 | 1.0 | every type of 5.3 not listed below | every value not listed below | |
| 1 | 1.1 | `StoredGrants` 0x000B, `Elevate` 0x000C, `GrantsSync` 0x0801 | `ErrorCode` `ACCOUNT_AWAY` = 22, `Desktop` `AccountAway` = 4 | the three types are never sent; `ACCOUNT_AWAY` is sent as `NOBODY_PRESENT` in every message that carries a code, and `HostState { AccountAway }` as `HostState { Locked, paused: true }` (`ErrorCode::for_minor`, `Desktop::for_minor`) |

- **What of minor 1 this tree does so far** (this bullet goes when the last of it is built): every type and value above is encoded, decoded and kept from a peer of minor 0 by the session's send path, which every control message leaves through. In a session, `StoredGrants` is accepted after the grant and not yet used or sent, and `Elevate` is answered with the non-fatal `NOT_PERMITTED` of a session that may not elevate. The grants sync connection (5.4) and the `AccountAway` states are not produced or served yet.
- **Session limits** (GUESS values, tuned in stage 14): max idle timeout 6 s; keep-alive 1 s; datagrams on, receive buffer 64 KiB; `max_concurrent_uni_streams` 512 (one stream per video frame at 60 fps, research windows.md 10b); `max_concurrent_bidi_streams` 16; explicit `receive_window` 16 MiB and `stream_receive_window` 8 MiB; a budget of 32 MiB of bytes buffered per connection across all streams, past which the receiver stops (`RecvStream::stop`) the oldest video streams first and then refuses new streams; congestion control quinn's default.
- **No allocation from a peer-declared length:** every length is checked against its limit (section 9) before anything is reserved; large payloads are read in bounded chunks.
- QUIC application close codes = the error codes of section 8.

## 2. TLS 1.3 with pinned keys

### 2.1 Identity
- Each machine has one long-term **device key**: Ed25519, generated with ring at first service start, stored per C5, used in both roles and for pairing. It never leaves the service/daemon process.
- **SPKI** = the DER `SubjectPublicKeyInfo` of that key (RFC 8410 form, 44 bytes; UNPROVEN length: check in stage 10 against ring's output).
- **Device ID** = the first 16 bytes of SHA-256(SPKI). Shown as 32 lowercase hex characters in mDNS TXT (`fp`) and logs.

### 2.2 Primary: RFC 7250 raw public keys
- TLS 1.3 only, signature scheme Ed25519 only, mutual authentication mandatory on both endpoints.
- Server: a `ResolvesServerCert` whose `only_raw_public_keys()` returns true, serving the device SPKI. Client: a `ResolvesClientCert` whose `only_raw_public_keys()` returns true (https://docs.rs/rustls/latest/rustls/client/trait.ResolvesClientCert.html, rustls 0.23.46, read 2026-11-07).
- Verifiers: `requires_raw_public_keys()` = true and `client_auth_mandatory()` = true (https://docs.rs/rustls/latest/rustls/server/danger/trait.ClientCertVerifier.html, read 2026-11-07). Each endpoint's `ServerConfig` has its own verifier:
  - Session endpoint: the server accepts only an SPKI in the trust store. The client accepts only the SPKI of the device it meant to reach (looked up by device ID, never by address).
  - Pairing endpoint: both accept any well-formed Ed25519 SPKI; possession is still proved by the TLS 1.3 `CertificateVerify` signature (`verify_tls13_signature`).
- quinn passing RPK end to end is **UNPROVEN** (risk R1): stage 10 starts with a one-hour spike.

### 2.3 Fallback: self-signed certificate with SPKI pinning
- If the spike fails: each side presents a minimal self-signed X.509 v3 certificate that wraps the same Ed25519 key, built from a fixed DER template in `swapdesk-trust` and signed with ring (no new crate). Validity fields are ignored.
- The verifiers accept only certificates that match the template exactly, extract the SPKI at its fixed position, and apply the rules of 2.2.

### 2.4 No resumption, no early data, and the post-handshake gate
- rustls servers send tickets unless told not to: `send_tls13_tickets` "The default is 2", and "If this is 0, no tickets are sent and clients will not be able to do any resumption"; on resumption and client authentication the same page says "rustls does not enforce any policy here" (https://docs.rs/rustls/latest/rustls/server/struct.ServerConfig.html, read 2026-11-07).
- So, on both endpoints, every `ServerConfig` sets `send_tls13_tickets = 0`, a session storage that stores nothing, a ticketer that issues nothing, and `max_early_data_size = 0` (the documented default, set explicitly); every `ClientConfig` sets `resumption = Resumption::disabled()` ("Disable all use of session resumption", https://docs.rs/rustls/latest/rustls/client/struct.Resumption.html, read 2026-11-07) and `enable_early_data = false`. quinn's 0-RTT is never used.
- **Gate (normative):** after every handshake, before accepting or opening any stream or datagram, the service reads `Connection::peer_identity()`, extracts the peer's SPKI and looks it up in the trust store again (session endpoint: it must be pinned, and on the client side equal to the expected device; pairing endpoint: it is recorded for section 3). A failure closes with `UNKNOWN_DEVICE`. The gate does not trust that a verifier ran.
- **Test (stage 10):** a client that keeps a ticket from an earlier connection and offers resumption and 0-RTT gets a full handshake, and a key removed from the trust store between two connections is refused by the gate.

### 2.5 Unknown keys
- A session handshake with an unpinned key fails with a TLS alert. The service logs `UnknownDeviceRejected { addr, device_id }` (F-PAIR-03), rate-limited (C5.1).

## 3. Pairing (pairing endpoint, `swapdesk-pair/1`), ZRTP-style SAS

Decision D5: both screens show a 6-digit code from a commitment exchange; both users confirm (RFC 6189 section 4.4.1.1: a hash commitment "constrains the attacker to only one guess"). No PAKE. Decision D12: pairing happens only when both users opened "Parear" within the last 2 minutes.

Roles: the **initiator** I is the machine whose user chose the device in "Parear" (the QUIC client); the **responder** R is the other (the QUIC server).

**Pairing mode.** A user enters it by opening "Parear" (on the other device's entry, or "Parear este computador"). It lasts 2 minutes, or until a pairing ends, or until the user closes the screen. On R, pairing mode opens the pairing endpoint (section 1) and adds `pair=1` to the mDNS TXT (GUESS: lets I's list show that R is ready); if R's user chose a device, R accepts only that device's announced fingerprint. On I, pairing mode is what allows the connection: the service never starts a pairing its user did not ask for. Outside pairing mode the pairing endpoint is closed, so an unsolicited pairing gets no answer, no prompt and no log entry beyond the rate-limited unknown-address counter.

Messages go on the single bidirectional stream, framed as in section 5. Notation: `||` concatenation, `H` = SHA-256 (ring), strings are ASCII without a terminator.

| Step | Direction | Message (type) | Content | Checks |
|---|---|---|---|---|
| 0 | | TLS 1.3 handshake on the pairing endpoint | both SPKIs exchanged (2.2 pairing rules), gate of 2.4 | R answers `PairResult { ok: false }` with `PAIRING_LOCKED` or `BUSY` (a pairing is already running); a peer key that is already pinned gets `PairResult { ok: false, code: ALREADY_PAIRED }` |
| 1 | I to R | `PairHello` (0x0701) | `name`, `os`, `os_version`, `majors`, `commit = H("swapdesk commit v1" \|\| spki_I \|\| nonce_I)` | `nonce_I` = 32 random bytes, kept secret until step 3 |
| 2 | R to I | `PairNonce` (0x0702) | `name`, `os`, `os_version`, `majors`, `nonce_R` (32 random bytes) | **before sending it**, R writes a provisional failed attempt to the trust store (C5 `Lockout`); R's UI shows "Parear com <I>" |
| 3 | I to R | `PairReveal` (0x0703) | `nonce_I` | R checks `H("swapdesk commit v1" \|\| spki_I \|\| nonce_I) == commit`; a mismatch is `COMMIT_MISMATCH` and an alert |
| 4 | both | (local) | `ek = TLS-Exporter("EXPERIMENTAL swapdesk pair v1", empty context, 32)` through quinn's `Connection::export_keying_material`; `s = H("swapdesk sas v1" \|\| ek \|\| spki_I \|\| spki_R \|\| nonce_I \|\| nonce_R)`; `SAS = u64::from_be_bytes(s[0..8]) mod 1_000_000`, shown zero-padded as "482 913" | the label uses RFC 5705's private-use prefix: "Label values beginning with "EXPERIMENTAL" MAY be used for private use without registration" (https://www.rfc-editor.org/rfc/rfc5705.html, section 4, read 2026-11-07). Both SPKIs go into `s` because the TLS 1.3 exporter secret covers only "ClientHello...server Finished" (https://www.rfc-editor.org/rfc/rfc8446.html, section 7.1, read 2026-11-07), not the client's Certificate |
| 5 | each to the other | `PairConfirm` (0x0704) | `matches: bool`, sent when the local user presses "Os códigos são iguais" or "São diferentes" | the receiving UI shows nothing about it (D12 removes "Falta você"): it waits for its own user |
| 6 | each to the other | `PairResult` (0x0705) | `ok: bool`, `code: ErrorCode` | a side pins the peer only when it sent and received `matches = true`; then it writes the trust store (C5: no grants, `grants_here` empty), clears R's provisional failure, sends `PairResult { ok: true }` and closes with `NO_ERROR` |
| 7 | | the first session | a new connection on the session endpoint | `WakeInfo` is sent and stored only in sessions, that is after pinning |

- **Why one guess:** in each TLS connection the committed nonce is hidden until the other nonce is fixed, so a man in the middle cannot steer either side's SAS; with two connections it must hope the two random SAS values match: 1 in 10^6 per attempt. The modulo bias of `2^64 mod 10^6` is below 2^-44 (arithmetic).
- **Timeouts (GUESS):** 10 s from the handshake to step 3; 120 s for the users' comparison (step 5). A timeout is a failed attempt.
- **Failed attempt** = any end after R sent `PairNonce` without both confirmations and pinning: `matches = false`, a timeout, `COMMIT_MISMATCH`, a lost connection, a closed screen. R's provisional entry already counts it; only a successful pinning clears it. I counts its own failures the same way.
- **Lockout:** 3 failed attempts within 15 minutes, from any address, lock pairing on that machine for 15 minutes; each further lockout within 24 h doubles it, up to 24 h (GUESS numbers). Locked: `PairResult { ok: false, code: PAIRING_LOCKED }` right after the handshake. Paired devices are never affected.
- **Rate:** one pairing at a time per machine; one pairing connection per source address per 30 s, counted only once the attempt is validated (after `retry()`) and admitted past the handshake cap, so a forged packet never spends the real initiator's turn; later attempts inside the 30 s are ignored and write no audit entry; a blocked address is dropped before the handshake.

## 4. What travels where (session endpoint)

Every stream starts with its **stream kind** (one QUIC varint). Datagrams start with a one-byte kind.

| Carrier | Kind | Opened by | Content | Priority (quinn `set_priority`, higher first) |
|---|---|---|---|---|
| bidi stream, the client's first | (none: always control) | client | control messages (section 5), both ways | 3 |
| uni stream | 0x01 input | viewer | input records (section 7.3), one long-lived stream per session | 4 |
| uni stream | 0x02 video frame | host | one encoded frame: header (7.1) + Annex B; one stream per frame | 1 |
| uni stream | 0x03 cursor shape | host | one cursor image per stream (7.4) | 2 |
| uni stream | 0x04 clipboard item | either | one clipboard item (7.5) | 0 |
| uni stream | 0x05 file data | sender of a file | `transfer_id: u32`, `offset: u64`, then raw bytes to the end of the stream; the receiver stops it at `size − offset` bytes and fails the transfer if more arrive | 0 |
| bidi stream | 0x10 terminal | viewer | `term_id: u32`, then raw PTY bytes both ways | 2 |
| datagram | 0x01 audio | host | one Opus packet (7.2) | n/a |
| datagram | 0x02 cursor position | host | latest pointer position (7.4) | n/a |

- In a session the QUIC client is the viewer. After a swap the roles and the client change together (new connection, 6.3).
- Audio goes with `send_datagram` (drops the oldest under congestion, research windows.md 10a); a datagram larger than `max_datagram_size()` is never sent.
- **Stale video:** the host resets (`SendStream::reset`) every unfinished frame stream of a display when a keyframe for that display is queued, and resets all of them and forces a keyframe at a lower bitrate when the unsent backlog exceeds 100 ms at the current send rate (GUESS). The viewer `stop`s streams of frames older than the last presented one. A gap in `frame_id` makes the viewer drop P-frames until the next keyframe and send `KeyframeRequest` (at most one per 200 ms). Intra-refresh is UNPROVEN on the MF vendor encoders; v1 relies on keyframes.

## 5. Control stream

### 5.1 Framing
`frame = type: varint, len: varint, payload: len bytes`. `len` ≤ 65 535; a larger value is `TOO_LARGE` (fatal). A payload decodes completely or the frame is a `PROTOCOL_VIOLATION` (fatal): trailing bytes, unknown type or tag, out-of-range length.

### 5.2 Order of a session
1. Viewer: `Hello`. Host: `HelloAck`. Both: `Capabilities`.
2. Host decides (F-SESS-01/02): grants at once when `Hello.requested` holds `UNATTENDED` and the device holds it, when `Hello.resume` or `Hello.swap_token` is valid (6.3, 6.4), or else sends `PromptPending` and waits for its user; then `SessionGrant` or `SessionRefused`.
3. Nothing but the session and keepalive messages flows before `SessionGrant`; no other stream may be opened (a violation is fatal). While it waits for `SessionGrant`, the viewer may end the attempt with `Bye { NO_ERROR }`; the host closes its question and grants nothing (the message and its handling exist since 1.0).
4. Host: `HostState`, `DisplayList`, `WakeInfo`, then `StoredGrants` when the viewer's `Hello.proto_minor` is 1 or more, then video. A viewer whose host announced minor 1 or more sends its own `StoredGrants` when it receives `SessionGrant`. For a viewer of minor 0 the order and the bytes are those of 1.0. Either side ends with `Bye`.

### 5.3 Messages
Types are `u16` values sent as varints. Field types in section 9. "V" = viewer, "H" = host, "P" = pairing endpoint only.

| Type | Name | Dir | Fields | Notes |
|---|---|---|---|---|
| 0x0001 | Hello | V→H | `proto_minor: u16`, `app_version: str(32)`, `name: str(64)`, `os: Os`, `os_version: str(32)`, `intent: Intent {View, Control}`, `requested: PermissionSet`, `resume: Option<Resume { session_id: u64, token: [u8;16] }>`, `swap_token: Option<[u8;16]>` | first message, exactly once. The viewer's service sets `UNATTENDED` in `requested` only toward a host it knows to be of minor 1 or newer: from the `HelloAck` of an earlier session with that device while its discovery record stays what it was then, else from the `minor` key of the record (section 10; a record that changed since is believed over what was said before, in both directions); toward any other host it does not ask, as the 1.0 interface never did. Until each account of a machine has its own key (D44) it asks only for an administrator account of the viewer's machine. `ADMIN_TERMINAL` is set only for a local administrator (D10, F-PERM-03) |
| 0x0002 | HelloAck | H→V | `proto_minor: u16`, `app_version`, `name`, `os`, `os_version` | |
| 0x0003 | Capabilities | both | `video_decode: Vec<VideoCap>(8)` with `VideoCap { codec: Codec, profile: H264Profile {Main, High}, max_level: u8, max_width: u16, max_height: u16, max_fps: u16 }`, `audio: Vec<AudioCodec>(4)`, `max_datagram: u16`, `keyboard: KeyboardInfo { kind: KbKind {Ansi, Iso, Jis, Abnt2, Unknown}, layout: str(64) }`, `features: u32` (bit 0 clipboard images, 1 file resume, 2 terminal, 3 admin terminal, 4 quick actions, 5 SAS) | what this build and OS can do, not what is granted |
| 0x0004 | PromptPending | H→V | `timeout_s: u16` | viewer shows "Aguardando <host>" with the countdown |
| 0x0005 | SessionGrant | H→V | `session_id: u64`, `granted: PermissionSet`, `mode: GrantMode {Prompted, Unattended, Resumed, Swap}`, `resume_token: [u8;16]` | Prompted: `granted` = what the host's user picked for this session, which may differ from the stored grants (D8) but never holds `UNATTENDED` or `ADMIN_TERMINAL` unless stored; Unattended: stored grants ∩ `requested`, and for a viewer whose `Hello.proto_minor` is 1 or more without `UNATTENDED` and `ADMIN_TERMINAL` (it is in without a prompt and asks for the two with `Elevate`; a viewer of minor 0 gets them at once, as in 1.0); Swap: stored grants ∩ `requested`; Resumed: 6.4 |
| 0x0006 | SessionRefused | H→V | `code: ErrorCode` | `DECLINED`, `TIMEOUT`, `NOBODY_PRESENT`, `NOT_PERMITTED`, `BUSY`; to a viewer of minor 1 or more also `ACCOUNT_AWAY` |
| 0x0007 | PermissionsChanged | H→V | `granted: PermissionSet` | effective at once; the host stops what was revoked within 1 s (F-PERM-02) and invalidates the resume token |
| 0x0008 | Bye | both | `code: ErrorCode` | `NO_ERROR` (user, "Desconectar"), `SWAPPED`, `HOST_SHUTDOWN`, `FORGOTTEN`; invalidates the resume token |
| 0x0009 | HostState | H→V | `desktop: Desktop {Default, Secure, Locked, LoginWindow, AccountAway}`, `paused: bool` (`AccountAway`: minor 1, the account that hosts is not the one at the console) | on every desktop switch; without `UNATTENDED` the host pauses video and drops input while the desktop is not `Default` |
| 0x000A | ConsentPending | H→V | `feature: Feature {Terminal, Files}`, `timeout_s: u16` | D13: an unattended session asked for the terminal or files of a logged-in user other than the one who granted `UNATTENDED`; the host asks that user; then the request proceeds or fails with `DECLINED`, `TIMEOUT` or `NOBODY_PRESENT` |
| 0x000B | StoredGrants | both | `grants: PermissionSet` | minor 1. What the sender's account has stored for the peer's device, not what this session holds; for display only (the receiver keeps it as what the other side allows, and nothing else reads it). Sent after the grant (5.2 step 4) and again whenever the stored grants change during the session. Before `SessionGrant` it is `PROTOCOL_VIOLATION`; from a peer that announced minor 0 it is `UNSUPPORTED_VERSION`. Frame for VIEW and CONTROL: `0B 02 03 00` |
| 0x000C | Elevate | V→H | none | minor 1. A session that entered without a prompt asks for `UNATTENDED` (and `ADMIN_TERMINAL` if stored and requested) after the system's confirmation on the viewer's machine. The host answers `PermissionsChanged` with the wider set, or `Error { NOT_PERMITTED, fatal: false, about_type: 0x000C }` when the device no longer holds `UNATTENDED` or the session was not an unattended entry. Frame: `0C 00` |
| 0x0101 | DisplayList | H→V | `displays: Vec<Display>(16)` with `Display { id: u16, name: str(64), x: i32, y: i32, width: u32, height: u32, scale_milli: u16, refresh_mhz: u32, rotation: u16, primary: bool }` | physical pixels in the host's virtual desktop; sent again on every change |
| 0x0102 | SelectDisplay | V→H | `display: u16` | |
| 0x0103 | VideoSettings | V→H | `preset: Preset {Auto, Text, Motion}`, `max_fps: u16`, `max_kbps: u32`, `view_width: u16`, `view_height: u16` | Text = 4:2:0, high bitrate, fps capped at 30 (GUESS) |
| 0x0104 | KeyframeRequest | V→H | `display: u16`, `last_good_frame: u32` | |
| 0x0105 | ReceiverReport | V→H | `frames_presented: u32`, `frames_lost: u32`, `decode_us_p50: u32`, `present_lag_us_p50: u32`, `audio_underruns: u32` | once per second; feeds rate control |
| 0x0106 | StreamStats | H→V | `kbps: u32`, `fps: u16`, `encoder: str(64)` | once per second; the overlay |
| 0x0107 | AudioControl | V→H | `enabled: bool` | the host opens audio capture only while enabled and `AUDIO` is granted; disabled = nothing captured, encoded or sent (D16). Default off until the viewer enables it |
| 0x0108 | HostMute | H→V | `muted: bool` | host user's mute |
| 0x0109 | SendSpecial | V→H | `key: Special {SecureAttention}` | `UNATTENDED` sessions only (D11), else `NOT_PERMITTED`; Windows: the system helper asks the service to call `SendSAS(FALSE)` |
| 0x0201 | SwapRequest | either | `request_id: u32` | 6.3 |
| 0x0202 | SwapAccept | either | `request_id: u32`, `token: [u8;16]` | sent by the side that was asked |
| 0x0203 | SwapDecline | either | `request_id: u32`, `code: ErrorCode` | `DECLINED`, `TIMEOUT`, `NOBODY_PRESENT`, `NOT_PERMITTED`, `BUSY` |
| 0x0204 | SwapCancel | either | `request_id: u32` | |
| 0x0205 | SwapArmed | either | `request_id: u32` | the future host is ready for the new connection |
| 0x0301 | ChatMessage | both | `msg_id: u32`, `text: str(4096)`, `sent_at_ms: u64` (Unix ms) | kept in memory only, at most 500 per session (GUESS) |
| 0x0302 | ChatRead | both | `up_to: u32` | |
| 0x0303 | ClipboardSync | both | `enabled: bool` | each side's own switch |
| 0x0304 | ClipboardSkipped | both | `reason: ClipSkip {Concealed, TooLarge, Unsupported}` | the item itself was not sent |
| 0x0401 | FileOffer | sender | `transfer_id: u32`, `name: str(255)`, `size: u64`, `mtime_ms: u64`, `dest: Option<str(1024)>` | `dest` is honoured only by a host receiving from its viewer (the folder chosen in Arquivos); a viewer ignores it and writes only into the folder its own user chose. A viewer accepts a `FileOffer` only as the answer to its own `FileGet` with the same `transfer_id` |
| 0x0402 | FileAccept | receiver | `transfer_id: u32`, `resume_from: u64` | `resume_from` = bytes already verified on disk |
| 0x0403 | FileDecline | receiver | `transfer_id: u32`, `code: ErrorCode` | `INVALID_NAME` for a bad name |
| 0x0404 | FileDone | sender | `transfer_id: u32`, `sha256: [u8;32]` | after the last data stream finished |
| 0x0405 | FileVerified | receiver | `transfer_id: u32`, `ok: bool` | a mismatch deletes the partial file |
| 0x0406 | FilePause | either | `transfer_id: u32` | |
| 0x0407 | FileResume | either | `transfer_id: u32` | the receiver answers with `FileAccept` |
| 0x0408 | FileCancel | either | `transfer_id: u32` | |
| 0x0409 | DirRequest | V→H | `req_id: u32`, `path: Option<str(1024)>` | `None` = the user's home |
| 0x040A | DirListing | H→V | `req_id: u32`, `path: str(1024)`, `entries: Vec<DirEntry>(2000)` with `DirEntry { name: str(255), is_dir: bool, size: u64, mtime_ms: u64 }`, `truncated: bool` | as the logged-in user; the host stops adding entries before the encoded frame would pass 65 535 bytes and sets `truncated` |
| 0x040B | FileGet | V→H | `transfer_id: u32`, `path: str(1024)` | the host answers with `FileOffer` (same id) |
| 0x0501 | TermOpen | V→H | `term_id: u32`, `cols: u16`, `rows: u16`, `admin: bool` | then V opens the 0x10 stream; at most 4 per session (GUESS); `admin` needs `ADMIN_TERMINAL` granted and requested (D10) |
| 0x0502 | TermOpened | H→V | `term_id: u32`, `account: str(64)` | e.g. "claudio" or "SYSTEM"; shown in the panel |
| 0x0503 | TermResize | V→H | `term_id: u32`, `cols: u16`, `rows: u16` | |
| 0x0504 | TermClose | V→H | `term_id: u32` | |
| 0x0505 | TermExited | H→V | `term_id: u32`, `exit_code: Option<i32>` | |
| 0x0601 | QuickAction | V→H | `action_id: u32`, `kind: Action {Lock, Sleep, Restart, TaskMonitor}` | requires `CONTROL` (F-ACT-01) |
| 0x0602 | QuickActionResult | H→V | `action_id: u32`, `code: ErrorCode` | |
| 0x0603 | WakeInfo | H→V | `macs: Vec<[u8;6]>(8)` | stored by the viewer's service for F-WAKE-01; the broadcast address is never taken from the peer |
| 0x0701 to 0x0705 | Pair* | P | section 3 | pairing endpoint only |
| 0x0801 | GrantsSync | both, sync connection only | `proto_minor: u16`, `grants: PermissionSet` | minor 1. The only message of a grants sync connection (5.4): `grants` is what the sender's account has stored for the other device, `proto_minor` the sender's own minor. Frame for minor 1, VIEW and CONTROL: `48 01 04 01 00 03 00` |
| 0x0F01 | Ping | both | `nonce: u32`, `t_send_us: u64` | once per second each way |
| 0x0F02 | Pong | both | `nonce: u32`, `t_send_us: u64`, `t_peer_us: u64` | RTT and clock offset (NTP-style) for the overlay; B1 is measured on one PC with the shared clock instead |
| 0x0FFF | Error | both | `code: ErrorCode`, `fatal: bool`, `about_type: u16`, `about_id: u32` | `about_id` = the request, transfer or terminal id it answers, 0 if none |

**Names on both sides** (`FileOffer.name`, `DirEntry.name` used for a download): one path component, not empty, not `.` or `..`, no `/`, `\`, `:` or control characters, and not a Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`, with or without an extension). A bad name is `INVALID_NAME`.

**Wake-on-LAN** is not a session message: the waking machine's service sends the magic packet (6 × 0xFF then the MAC 16 times; UDP port 9; GUESS, stage 22 cites the format) with TTL 1 to the broadcast address of the approved local interface whose subnet holds the device's last known address, for each MAC stored from `WakeInfo`.

### 5.4 Grants sync connection (minor 1)

Two paired devices tell each other what each has stored for the other without opening a session, so the read-only column of Permissões follows the other side within seconds.

- It is a connection to the session endpoint (`swapdesk/1`, the same pre-handshake guard, caps, pinned-key handshake and post-handshake gate as a session) whose first control message is `GrantsSync` instead of `Hello`.
- The client opens the control stream and sends one `GrantsSync`. The server answers with one `GrantsSync`. The client closes the connection with the application code `NO_ERROR`; there is no session, so no `Bye`. The server closes it itself 2 s after answering if the client has not (GUESS).
- Nothing else may travel: a second message, any message other than `GrantsSync`, any other stream or any datagram is `PROTOCOL_VIOLATION` and closes the connection. `GrantsSync` after a `Hello` or inside a session is `PROTOCOL_VIOLATION`. `GrantsSync.proto_minor == 0` is `PROTOCOL_VIOLATION`: no build of minor 0 can send it.
- It never prompts, never takes the host reservation of a session, is not written to the activity log, and is never answered `ACCOUNT_AWAY`.
- A device over its budget of syncs is closed with `BUSY` right after its first message, without an answer and without its value being kept.
- A client dials only a device it knows to be of minor 1 or more: from an earlier authenticated exchange (`HelloAck`, `GrantsSync`), or else from the `minor` key of its DNS-SD record (section 10). A 1.0 device is never dialled for this, so it never sees the message.

## 6. Permissions, presence, swap, resume

### 6.1 PermissionSet (`u16` bit set)
| Bit | Name | Implies | Gates |
|---|---|---|---|
| 0 | VIEW | | video, cursor, `DisplayList` |
| 1 | CONTROL | VIEW | input stream, `QuickAction`, being the target of a swap |
| 2 | AUDIO | VIEW | audio datagrams |
| 3 | TERMINAL | | `TermOpen { admin: false }` |
| 4 | ADMIN_TERMINAL | TERMINAL | `TermOpen { admin: true }` |
| 5 | FILES | | file messages, 0x05 streams, `Dir*` |
| 6 | CLIPBOARD | | 0x04 streams |
| 7 | UNATTENDED | VIEW | entry without a prompt, also at the login, lock and UAC screens; the SYSTEM injector and `SendSpecial` (D11) |
Bits 8 to 15 are reserved and must be 0. A set that breaks "implies" is a `PROTOCOL_VIOLATION`. Chat needs no permission inside a session. A newly pinned device has an empty set (every session prompts).

### 6.2 Presence
"Someone at the host" = a Swapdesk UI is connected to the service/daemon from the console session, and that session is unlocked (not the login, lock or UAC desktop). Without presence, prompts are refused at once with `NOBODY_PRESENT`.

### 6.3 Swap (decision D1: a new connection with the pinned keys)
Names: A = the side that asks, B = the side asked. Either side can ask: the viewer from the toolbar, the host from the banner.
1. The future host is the current viewer. It must already have granted `CONTROL` to the other device in its own trust store. If the future host is A, A checks this before sending and the UI says why it cannot ask; if it is B, B checks on receipt and answers `SwapDecline { NOT_PERMITTED }` at once, without a prompt. No presence at B is `SwapDecline { NOBODY_PRESENT }` at once.
2. A sends `SwapRequest`. B shows "pediu para inverter" with a 20 s countdown (auto-decline sends `SwapDecline { TIMEOUT }`); A gives up after 22 s.
3. B accepts: B sends `SwapAccept { token }` with 16 random bytes.
4. The future host arms the token for the future viewer's device ID for 10 s, single-use, and sends `SwapArmed`.
5. Both close the old connection with `Bye { SWAPPED }`. The future viewer's service opens a new session connection to the future host with `Hello { swap_token }`. The future host grants its stored grants for that device (`GrantMode::Swap`) without a prompt, never `UNATTENDED` entry, and consumes the token.
B4 (≤ 1 s) is measured from step 3 to the first presented frame.

### 6.4 Resume
- `SessionGrant.resume_token` (16 random bytes) lets the same viewer device resume the same session with `Hello { resume }` without a prompt (`GrantMode::Resumed`), but only after the connection was **lost** (idle timeout or transport error), within 60 s of the loss, once.
- The host invalidates it on `Bye` from either side, "Desconectar", a grant change, Forget, lock-on-end, and once used.
- A resumed session gets the original session's grants ∩ the grants still stored; per-session additions of D8 survive only if still stored, otherwise they are dropped.
- A host reboot loses every token: an unattended device reconnects through `UNATTENDED`; any other gets a fresh prompt.

## 7. Media and input formats

All fixed-size integers are **little-endian**. Offsets in bytes after the stream kind (streams) or including the kind (datagrams).

### 7.1 Video frame (stream kind 0x02), 32-byte header
| Off | Size | Field |
|---|---|---|
| 0 | 1 | `version` = 1 |
| 1 | 1 | `flags`: bit 0 KEYFRAME, bit 1 CONFIG (SPS/PPS included; configuration or size changed), bit 2 DISCONTINUITY (encoder or capture restarted, e.g. a desktop switch); other bits 0 |
| 2 | 2 | `display_id` |
| 4 | 4 | `frame_id`, per display, +1 per encoded frame, wraps |
| 8 | 8 | `capture_us`: the host's monotonic clock (C3 `MonoMicros`) at capture |
| 16 | 4 | `encode_us`: encode duration |
| 20 | 2 | `width` (coded picture, pixels) |
| 22 | 2 | `height` |
| 24 | 1 | `codec`: 1 = H.264 Annex B with start codes (research windows.md 3b) |
| 25 | 3 | reserved, 0 |
| 28 | 4 | `payload_len` (≤ 8 MiB, checked before reading) |
Then `payload_len` bytes of Annex B. The stream ends right after them.

### 7.2 Audio datagram (kind 0x01), 16-byte header
| Off | Size | Field |
|---|---|---|
| 0 | 1 | kind = 0x01 |
| 1 | 1 | `flags`: bit 0 DISCONTINUITY (device change), bit 1 MUTED |
| 2 | 2 | `frame_ms` (10) |
| 4 | 4 | `seq` |
| 8 | 8 | `capture_us` of the first sample |
Then one Opus packet, 48 kHz stereo (libopus through the `opus` crate, or `opus-rs` if libopus does not build: the same packets, D16).

### 7.3 Input records (stream kind 0x01)
Each record is `tag: u8` and fixed fields. The host applies them in order: in a prompted session through the user helper with the logged-in user's token (medium integrity, so elevated windows and UAC do not receive them, D11), in an `UNATTENDED` session through the SYSTEM helper.
| Tag | Event | Fields after the tag |
|---|---|---|
| 0x01 | Key (physical) | `usage_page: u16` (0x07 keyboard, 0x0C consumer), `usage: u16`, `down: u8` (0/1), `modifiers: u8` (HID modifier byte: bit 0 LCtrl, 1 LShift, 2 LAlt, 3 LGui, 4 RCtrl, 5 RShift, 6 RAlt, 7 RGui), `locks: u8` (bit 0 Caps, 1 Num, 2 Scroll) |
| 0x02 | Text | `len: u16` (≤ 256), UTF-8 bytes |
| 0x03 | Pointer (absolute) | `display: u16`, `x: u16`, `y: u16`: 0 to 65535 across the display; pixel = round(x × (width − 1) / 65535) |
| 0x04 | Button | `button: u8` (1 left, 2 right, 3 middle, 4 back, 5 forward), `down: u8` |
| 0x05 | Wheel | `dx: i16`, `dy: i16` in 1/120 of a notch (dy > 0 = away from the user, dx > 0 = right) |
| 0x06 | ReleaseAll | none: every key and button up (focus lost, grab off) |
`modifiers` is the viewer's state after the event; the host presses or releases modifiers to match it before applying a Key record. Key translation: C4.

### 7.4 Cursor
- Position datagram (kind 0x02), 16 bytes: `kind: u8`, `flags: u8` (bit 0 VISIBLE), `display: u16`, `seq: u32`, `x: u16`, `y: u16` (as 7.3), `shape_id: u32`.
- Shape stream (kind 0x03): `shape_id: u32`, `width: u16`, `height: u16` (each ≤ 256), `hot_x: u16`, `hot_y: u16`, then `width × height × 4` bytes of straight RGBA. The viewer caches up to 64 shapes per session (GUESS).

### 7.5 Clipboard item (stream kind 0x04)
`seq: u32`, `kind: u8` (1 UTF-8 text, 2 PNG), `len: u32` (text ≤ 1 MiB, PNG ≤ 16 MiB; GUESS; checked before reading), then `len` bytes. The newest `seq` wins.

## 8. Error codes (`ErrorCode`, `u16`; also QUIC application close codes)
| Code | Name | Meaning |
|---|---|---|
| 0 | NO_ERROR | normal end |
| 1 | PROTOCOL_VIOLATION | malformed or out-of-order message, unknown type or tag |
| 2 | UNSUPPORTED_VERSION | no common major, or a message newer than the peer's minor |
| 3 | UNKNOWN_DEVICE | key not pinned (or gone at the gate) |
| 4 | NOT_PERMITTED | not granted in this session |
| 5 | NOBODY_PRESENT | no one at the host to answer |
| 6 | DECLINED | the user said no |
| 7 | TIMEOUT | no answer in time |
| 8 | BUSY | another pairing, session or swap is running, or a per-session cap is reached |
| 9 | PAIRING_LOCKED | lockout (section 3) |
| 10 | COMMIT_MISMATCH | the pairing commitment did not open |
| 11 | SAS_REJECTED | a user pressed "São diferentes" |
| 12 | TOO_LARGE | over a size limit |
| 13 | NOT_FOUND | path, display, transfer or terminal unknown |
| 14 | IO_FAILED | disk or OS error |
| 15 | UNAVAILABLE | the feature cannot run now (no user logged in, helper restarting, OS permission missing) |
| 16 | SWAPPED | closed by a swap |
| 17 | HOST_SHUTDOWN | the host is stopping or rebooting |
| 18 | FORGOTTEN | the peer forgot this device |
| 19 | INTERNAL | a bug; logged with context |
| 20 | INVALID_NAME | a file name broke section 5.3's rules |
| 21 | ALREADY_PAIRED | the pairing peer's key is already pinned |
| 22 | ACCOUNT_AWAY | minor 1: the account that would answer is not at the console of its computer. A peer of minor 0 is sent `NOBODY_PRESENT` in its place |

## 9. Serialization

### 9.1 Choice (decided: D14)
A **hand-written codec in `swapdesk-proto`**, no serialization crate: the wire format is exactly this file; strict limits and canonical checks live in one place; the hot-path headers (7.x) need hand-written fixed layouts anyway; each decoder is a fuzz target. Kept honest by round-trip property tests (`proptest`, dev only). Rejected in the audit (`docs/research/dependencies.md` 3.5): `postcard` (its varint is not the QUIC varint of 9.2, so it would change the wire format, and the limits stay hand-written anyway) and `bincode` (unmaintained: "the bincode team has taken the decision to cease development permanently", RUSTSEC-2025-0141, https://rustsec.org/advisories/RUSTSEC-2025-0141.html, read 2026-11-07). The same codec rules encode the on-disk records of C5.

### 9.2 Primitive encoding
- `varint`: QUIC variable-length integer, RFC 9000 section 16 (two high bits give the length 1, 2, 4 or 8 bytes; big-endian; https://www.rfc-editor.org/rfc/rfc9000.html, read 2026-11-07). RFC 9000 allows non-minimal encodings; **Swapdesk rejects them**, so every accepted byte string re-encodes to itself.
- `u8`, `u16`, `u32`, `u64`, `i16`, `i32`: fixed little-endian. `bool`: one byte, 0 or 1 only. `[u8; N]`: N raw bytes.
- `str(max)`: varint byte length ≤ max, valid UTF-8, no NUL.
- `Option<T>`: one byte 0 (None) or 1 then `T`.
- `Vec<T>(max)`: varint count ≤ max, then the items; nothing is reserved before the count is checked, and never more than the bytes left in the frame allow.
- Enums: one `u8` tag, values in declaration order from 0 as listed here; data-carrying variants put their fields after the tag.
- Structs: fields in the order listed, no padding, no field names.
- `PermissionSet`: `u16` (6.1). `ErrorCode`: `u16`. `Os`: u8 {0 Windows, 1 MacOS}. `Codec`: u8 {1 H264}. `AudioCodec`: u8 {1 Opus48Stereo}.

### 9.3 Proof (stage 6)
proptest round trips for every message and header: `decode(encode(x)) == x`; `encode(decode(b)) == b` for every accepted `b`; truncation, trailing bytes, over-limit lengths, huge declared counts, bad UTF-8, bad tags and non-minimal varints are rejected without panics or large allocations. The same decoders are the fuzz targets of stage 29.

## 10. Readings fixed by the stage-6 code

- `PermissionSet` decode rejects reserved bits 8 to 15 and any set that breaks the implications of 6.1; `CONTROL`, `AUDIO` and `UNATTENDED` include `VIEW`.
- `TOO_LARGE` only for a frame length above 65 535, a video `payload_len` above 8 MiB and an over-limit clipboard `len`; every other malformed input is `PROTOCOL_VIOLATION`. `FrameHeader` rejects an unknown type or an over-limit length before the payload is read.
- `PairHello.majors`: `Vec<u16>`, at most 8. `PairHello`/`PairNonce` names `str(64)`, versions `str(32)`; `HelloAck` limits as `Hello`.
- `VideoHeader`: version always 1 (not stored); unknown flag bits, a nonzero reserved field and a non-H.264 codec are rejected. Audio and cursor headers check their kind byte and flag bits.
- Input: `Key.usage_page` is 0x07 or 0x0C; `locks` uses bits 0 to 2; `Button` is 1 to 5; `Text` at most 256 bytes of UTF-8 (NUL allowed there and in clipboard text). Cursor at most 256x256, RGBA exactly `width*height*4` bytes; hot spot unchecked.
- `Capabilities.features` accepts any `u32` (unknown bits ignored). `PairResult.ok` and `code` are not forced to agree. The 0x0FFF message type is `ErrorMessage`.
- The file-name rule for `INVALID_NAME` is session logic, set in stage 21.
- DNS-SD (stage 9): service `_swapdesk._udp.local.`, instance name = the 32-hex key fingerprint, host `swapdesk-<12 hex>.local.`, SRV port = the session port, TXT keys `name` (≤ 64 bytes), `os` (`windows`|`macos`), `fp`, `pair=1` only in pairing mode, `minor` (the record's protocol minor in ASCII decimal, 1 to 65535, written only from minor 1 on; absent or malformed reads 0 and never refuses the record; a hint only, an authenticated `HelloAck` or `GrantsSync` wins); unknown keys ignored, at most 16 entries of ≤ 255 bytes. IPv4 prefixes 1 to 30 only; 169.254/16 allowed for a direct cable. An approved interface is a hardware Ethernet/Wi-Fi adapter with no virtual or VPN marker (fails closed). mDNS alone detects a clean shutdown in about 1 s; asleep or crashed devices need the session-layer liveness probe (stage 14/22) for the 10 s criterion of F-FIND-01.
- TLS (stage 10): R1 resolved, RFC 7250 raw public keys work end to end through quinn 0.11 and rustls 0.23 (ring); 2.3 stays unused. SPKI is 44 bytes. The session endpoint requires an offered `swapdesk/*` ALPN and refuses `swapdesk-pair/1`; the pairing endpoint the reverse; a hello offering both is refused on either. Client resumption is disabled explicitly in production.
- Pairing (stage 10): the responder's refusals (PAIRING_LOCKED, BUSY, ALREADY_PAIRED, NOT_PERMITTED, UNSUPPORTED_VERSION) are its reply to `PairHello` in place of `PairNonce`. Pinning happens on both confirmations, with the store write and the provisional clear in one atomic write; the final `PairResult { ok: true }` cannot undo it. A side that rejects sends `PairConfirm { false }` then `PairResult { false, SAS_REJECTED }`. The pairing endpoint stays up past the 2-minute mode while an attempt runs and closes when the mode has ended and none runs; the mode closes when an attempt ends. The initiator counts a provisional failure from `PairNonce`. Replacing a reinstalled device's key needs the user to open "Parear" on that device's entry (`replaces`); it logs `KeyReplaced` (audit tag 16). The lockout window counts from the end of the failing attempt.

## 11. Wire fixtures of 1.0

The rule of section 1, that layouts of existing messages never change within a major, is checked against bytes, not against a reading of this file. The reference bytes of every wire item of version 1.0.0 are kept as test fixtures, and a test of each tree decodes them and encodes the same values back.

- **What they hold.** One text file of hex per sample (16 bytes per line, so a changed byte changes one line) and an `INDEX.txt` that names every sample with its family and what it is. The samples are explicit values written in one source file, with no randomness and no clock: every control and pairing message with its optional parts, every value of every wire enum, every permission and feature bit, the framing header, the stream starts (input, video, cursor shape, clipboard, file data, terminal), the audio and cursor datagram headers, the two pairing preimages and the six digits, the constants both peers must agree on (ALPN, labels, ports, limits), and the 1.0 values computed outside the proto crate: the DNS-SD TXT record, the Wake-on-LAN magic packet and broadcast address (`swapdesk-lan`), and the SPKI, device ID and pairing hashes (`swapdesk-trust`). Each of the three crates has its own `tests/wire_samples/mod.rs` and its own `tests/fixtures/v1.0.0/`.
- **How they were made.** Never by hand and never by the current code: the sample table compiles unchanged in the tree of tag `v1.0.0`, `generate.rs` ran there as an example of the crate, and its output was copied into `tests/fixtures/v1.0.0/`. A fixture is therefore what 1.0 put on the wire.
- **What the tests prove** (`tests/wire_1_0.rs` of each crate). Every fixture decodes into exactly its sample. Encoding the sample gives the fixture byte for byte, which is what the current code sends to a peer of minor 0; the one exception is `proto_minor` of `Hello` and `HelloAck`, which is the sender's own minor: the test encodes it with the current `PROTO_MINOR` and compares every other byte. The set is complete: a message type, an enum value, a stream kind, a datagram kind, an input record or a permission bit without a fixture fails the test, so a change cannot skip one silently. A message type or enum value introduced after minor 0 is listed, with its minor, in the constants `TYPES_AFTER_MINOR_0` and `ENUM_VALUES_AFTER_MINOR_0` of the proto test, in the change that adds it. Last, one flipped byte of any fixture fails the comparison.
- **A red fixture is a wire change.** It is never fixed by regenerating: either the code stopped being compatible with 1.0, or the change is a new major. A new sample for something added in a later minor is welcome, but its bytes come from the code that introduced it, and the 1.0 fixtures stay as they are.
- **How to regenerate** (only to add a sample of 1.0 that was missed, never to change one). Make a worktree of the tag outside the project folder, copy the folder `crates/<crate>/tests/wire_samples/` of the current tree into the same place of that worktree and its `generate.rs` to `crates/<crate>/examples/generate.rs`, run `cargo run -p <crate> --example generate -- <output folder>` for `swapdesk-proto`, `swapdesk-lan` and `swapdesk-trust`, copy each output over `crates/<crate>/tests/fixtures/v1.0.0/` of the current tree, and remove the worktree. The generator also decodes what it wrote with the 1.0 code and stops if a sample does not round-trip.
