# Swapdesk contracts C2 to C7, crates and lanes

C1 (the wire) is `docs/PROTOCOL.md`. Flows and budgets: `docs/SPEC.md`. Decisions D1 to D19: `docs/DECISIONS.md`. **UNPROVEN** and **GUESS** as in the SPEC; sources read on 2026-11-07 are cited inline, the rest come from `docs/research/`. A change here after M0 is made by the main session, and every lane rebases on it.

## C2. Processes and local IPC

### C2.1 Windows
| Process | Account, session | Started by | Does | Never does |
|---|---|---|---|---|
| `swapdesk-service.exe` | LocalSystem, session 0 | the SCM at boot (auto start) | every LAN socket (QUIC both roles on two endpoints, mDNS, Wake-on-LAN), LAN guard, identity and trust store (C5), pairing, permissions, presence, swap, audit log, `SendSAS(FALSE)` (research windows.md 5b), launching and watching the helpers, prevent sleep during a session, the **administrator terminal PTY** (SYSTEM, in session 0, so it never sits on the user's desktop), lock, sleep and restart as system calls when no user is logged in | capture (DDA fails in session 0, windows.md 1b), decode, UI, opening any path a client sent, starting a GUI process |
| `swapdesk-helper.exe --role system` | SYSTEM token moved to the console session, `winsta0\default` (windows.md 5d recipe) | the service, whenever a console session exists (also at the login screen); restarted with backoff 1, 2, 4 ... 30 s | DDA capture per output on its own adapter, BGRA to NV12, MF encode, cursor, display list, desktop-switch watcher, `SendInput` on the input desktop **for `UNATTENDED` sessions only** (D11), asking the service for SAS | LAN, files, clipboard, terminals, quick actions |
| `swapdesk-helper.exe --role user` | the logged-on user's token (`WTSQueryUserToken`; for an administrator the service makes sure it is the filtered, medium-integrity token, not the linked elevated one: UNPROVEN which one the call returns, stage 18 checks `TokenElevationType`) in the console session | the service, when a user is logged on in the console session | `SendInput` **for prompted sessions** (medium integrity: UIPI keeps it out of elevated windows and UAC, D11), user terminal PTY, clipboard, the host side's file reads and writes, folder listing, quick actions including Task Manager, audio loopback if the SYSTEM helper cannot hear the user (risk R6) | LAN |
| `Swapdesk.exe` (native Win32 shell, D18) | the user | the user (Start menu; HKCU Run when "Abrir ao ligar" is on; one instance per session through a named mutex); `Swapdesk.exe --elevated <action>` started through UAC (`runas`) for admin-only changes | UI (Win32 + DirectComposition + Direct2D/DirectWrite, AccessKit), prompts, banner, viewer: decode (MF) on the D3D11 device shared with the composition swap chain, render (C3 `Renderer`), keyboard grab (`WH_KEYBOARD_LL` on its own thread), audio playback, the viewer side's file reads and writes through its own file dialogs, writing the exported activity log | LAN (the service relays the viewer's media), SYSTEM rights |

Threads in the system helper: the capture thread and the input thread own **no windows and no hooks** (so `SetThreadDesktop` works, windows.md 1d); the `EVENT_SYSTEM_DESKTOPSWITCH` WinEvent hook lives on a third thread that never switches desktops. The user helper's input thread follows the same rule. Whether a medium-integrity helper may `SetThreadDesktop` to the default input desktop is UNPROVEN (risk R12, stage 16 and 18).

**Install location.** The service starts helpers only from its own per-machine install folder under `%ProgramFiles%`, and only if neither the folder nor the helper file grants write access to anyone but SYSTEM, Administrators and TrustedInstaller; otherwise it logs and starts nothing (F-SYS-01: the installer is per machine, never per user).

**Pipes.** All are byte-mode named pipes under `\\.\pipe\swapdesk\`, framed as PROTOCOL.md 5.1 (varint type, varint length) with the local message set of `swapdesk-ipc`; media messages may be up to 16 MiB, every length checked before allocation.
| Pipe | Created by | DACL | Check by the service | Check by the client |
|---|---|---|---|---|
| `ui-<session id>` | the service, for each interactive session, with `FILE_FLAG_FIRST_PIPE_INSTANCE`; if that create fails, the name is squatted: the service logs an alert and creates no UI pipe for that session | SYSTEM full; the session's logged-on user read/write; nobody else | `GetNamedPipeClientProcessId` → same session; `ImpersonateNamedPipeClient` → user SID is the session's user; the connection is **admin** only if the impersonation token is elevated and the Administrators SID is enabled in it (C2.3) | the UI opens the pipe with `SECURITY_SQOS_PRESENT \| SECURITY_IDENTIFICATION` (the server can identify, never act as, the user) and checks `GetNamedPipeServerProcessId` equals the service PID the SCM reports (`QueryServiceStatusEx`) (https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeserverprocessid, read 2026-11-07) |
| `helper-<role>-<session id>-<128-bit random hex>` | the service, before `CreateProcessAsUserW`; the name goes on the helper's command line | role system: SYSTEM only; role user: SYSTEM and that user | one instance only; the client PID must equal the PID the service launched | the helper checks the server PID against the SCM the same way |
Pipe flags `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE` (a second create fails with `ERROR_ACCESS_DENIED`, proven by the squat test) and `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (without `SQOS_PRESENT` the default is Impersonation) are confirmed on the CreateNamedPipe and CreateFile pages (https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea, https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew, read 2026-11-08); a remote client is not tested (none here).

**Readings fixed by stage 18b.** The DACL is always explicit (the default descriptor grants Everyone read). The user row is `0x12019B` (generic read and write without `FILE_APPEND_DATA`, which on a pipe is `FILE_CREATE_PIPE_INSTANCE`: the user cannot add an instance). The service reads `Hello` before `ImpersonateNamedPipeClient` (on Windows 11 the call also succeeds before any read, so its failure proves nothing). Admin = `TokenIsElevated` and Administrators enabled in `TokenGroups` (a filtered token holds it deny-only). A user token still elevated after the linked-token step (UAC off) is refused. Restart backoff 1, 2, 4, 8, 16, 30 s, reset after 60 s of uptime. Lock/unlock map to `DesktopSwitched(Locked/Default)`; a resume reported twice reaches the consumer once. The helpers live in `%ProgramFiles%\Swapdesk\`; the install check covers the file and its folder, not the ancestors. Overlapped handles: one reader and one writer thread on a shared handle; a timeout cancels with `CancelIoEx`.

**Address changes.** A change of the approved addresses (Windows: the service's NetworkWatcher; macOS: SystemConfiguration dynamic-store notices on `State:/Network/Global/IPv4` and `State:/Network/Interface/*/IPv4`, settled 300 ms, never polled) reaches `UiHub::network_changed`, which first makes the session endpoints follow the approved addresses (`service/listen.rs`: an address no longer approved stops accepting while its live connections run to their end, a new one gets its own endpoint on the session port) and then refreshes discovery, which starts a fresh mDNS daemon so the announcement moves to the new address.

**Switches.**
- Lock, UAC, Ctrl+Alt+Del screen (desktop switch, same session): the capture thread gets `DXGI_ERROR_ACCESS_LOST` and calls `ScreenSource::reacquire` (OpenInputDesktop, SetThreadDesktop, new duplication); the SYSTEM input thread calls `InputSink::follow_input_desktop`; the next frame carries DISCONTINUITY and a keyframe; the service sends `HostState` and pauses sessions without `UNATTENDED`.
- Logon, logoff, fast user switch, console connect or disconnect (`SERVICE_CONTROL_SESSIONCHANGE`): the service stops both helpers, starts them in the new console session, and keeps the QUIC sessions open; the viewer holds its last frame until the DISCONTINUITY keyframe. No console session (`0xFFFFFFFF`): retry every 500 ms.
- Sleep and resume: the service ends nothing on suspend; after resume the viewers reconnect (F-NET-01).

### C2.2 macOS (14.4 or later for unattended access)
| Process | Account, session | Started by | Does | Never does |
|---|---|---|---|---|
| `swapdesk-daemon` (in `Swapdesk.app/Contents/MacOS/`) | root, LaunchDaemon | launchd, registered with `SMAppService.daemon` and approved by an admin (macos.md 5a) | what the Windows service does, except SAS; writes and removes the LoginWindow agent plist (macos.md 5b); administrator terminal PTY (root, no window server); exempt from Local Network privacy as a launchd daemon (TN3179, macos.md 4a); at start, checks that its bundle and executable are root-owned, not writable by others, and validly signed, else it refuses to serve (whether launchd or SMAppService already refuse a modified daemon is UNPROVEN: stage 24 probe) | connect to the window server (Apple DTS, macos.md 2a), open a path a client sent |
| `SwapdeskAgent.app`, Aqua instance (nested, `LSUIElement`, own provisioning profile if D3 is granted) | the user, Aqua session | launchd, registered per user with `SMAppService.agent` by the app at first run | SCK video and audio, VideoToolbox encode, `CGEventPost`, cursor, display list, user terminal PTY, clipboard, the host side's files, quick actions including Activity Monitor | the LAN (agents are not exempt: macos.md 4a) |
| `SwapdeskAgent.app`, LoginWindow instance | root, LoginWindow session | launchd, from `/Library/LaunchAgents/<bundle id>.loginwindow.plist` (root:wheel 0644, `LimitLoadToSessionType` LoginWindow, `AssociatedBundleIdentifiers`), written by the daemon when the first device gets `UNATTENDED` | SCK and `CGEventPost` at the login window within probe P5 and P6, for `UNATTENDED` sessions only | the LAN, terminals, files, launching apps |
| `Swapdesk.app` (native AppKit shell, D18) | the user | the user; login item (SMAppService) when "Abrir ao ligar" is on | UI (AppKit through objc2, NSAccessibility), viewer: VideoToolbox decode, `AVSampleBufferDisplayLayer` with `sampleBufferRenderer` (macos.md 7c) under the toolbar view, CGEventTap grab on its own run-loop thread, audio playback, the viewer side's files through `NSOpenPanel`/`NSSavePanel`, writing the exported activity log | the LAN, root rights |

**Sockets.** Directory `/var/run/swapdesk/` (root:wheel 0755) holds `ui.sock` and `agent.sock` (0666: anyone may connect; every connection is authorized). On accept the daemon takes the peer's **audit token** (`LOCAL_PEERTOKEN`, not `LOCAL_PEERPID`, which a recycled PID could fool) and checks it with `SecCodeCopyGuestWithAttributes` keyed by `kSecGuestAttributeAudit` (the constant exists, macOS 10.0+: https://developer.apple.com/documentation/security/ksecguestattributeaudit, read 2026-11-07, no prose on the page) against a requirement; `ui.sock` accepts only the console user's uid; `agent.sock` accepts the console user's uid (Aqua) or uid 0 from the LoginWindow agent. The client side checks the socket's owner is root and the directory root-owned before connecting.
- **Requirement per build tier:** Developer ID builds: `anchor apple generic and certificate leaf[subject.OU] = "<TeamID>"` plus the bundle identifier. Self-signed builds: the leaf certificate hash of the daemon's own signature plus the identifier. Ad-hoc builds: the exact cdhash of each nested executable, recorded by the daemon when the app registers it; a rebuild requires registering again. (GUESS: requirement strings and the cdhash approach are settled in stage 24.)
- UNPROVEN: `LOCAL_PEERTOKEN` and `getsockopt` use the `libc` crate (D14); stage 24 settles the calls. Loopback/Unix-socket IPC is assumed outside Local Network privacy (GUESS in macos.md 4a; probe P8 step 8).

**Switches.** At login, launchd ends the LoginWindow instance and starts the Aqua one (TN2083, macos.md 2a): the daemon swaps the pipeline's source, keeps the QUIC session and sends DISCONTINUITY. Lock: the Aqua agent keeps running; what it can capture is probe P4 step 5. Fast user switch: the daemon routes to the agent of the console user (UNPROVEN how it learns the console user; stage 24).

### C2.3 Rules for every local client

- **The user's processes are the UI.** Any process running as the session's user can inject code into `Swapdesk.exe`/`Swapdesk.app`, drive its windows (UI Automation, synthetic input, Apple events), or open the UI pipe/socket itself. The service therefore treats every UI connection as "this user" and grants it only what that user could do by hand at the machine: start or answer sessions, edit grants that need no admin, chat. Input in prompted sessions runs with that same user's token (D11), so the UI path never adds privilege.
- **Admin-only operations** (granting `UNATTENDED` or `ADMIN_TERMINAL`, D7; using `UNATTENDED` entry or an administrator terminal on another machine, D10): Windows: accepted only over a UI pipe connection whose impersonation token is elevated with the Administrators SID enabled, made by a `Swapdesk.exe --elevated` instance started through UAC. Group membership alone is never enough: an administrator's standard token has "the administrative Windows privileges and SIDs ... removed" (https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works, read 2026-11-07). macOS: the UI obtains an Authorization Services right (`com.swapdesk.admin`, rule `authenticate-admin`, registered by the daemon; GUESS names) and passes its external form; the daemon checks it with Authorization Services before acting (UNPROVEN API path; stage 24). The UI only hints; the service/daemon enforces.
- **No paths into the service.** No IPC message to the service/daemon carries a file path to open, read or write. The viewer's UI reads the files it sends and writes the files it receives (into the folder its user picked) and streams bytes over its pipe; on the host the user helper or Aqua agent does the same. The service only relays bytes and checks names (PROTOCOL.md 5.3).
- **Self-connection.** The service refuses connections to and from its own addresses and loopback (PROTOCOL.md 1).
- **Logs.** The service's trace log lives in the C5 folder (same DACL). Never logged, at any level: the private key, pairing nonces, the TLS exporter output, `s` and the SAS, resume and swap tokens, terminal bytes, text input records, clipboard content, chat text, file contents (F-SEC-01; `swapdesk-fakes::LogCapture` test, C7). The trace logger is Swapdesk's own (`TraceSink`, C3), no logging crate (D14).

### C2.4 Local message set (`crates/swapdesk-ipc`, stage 18)

Framing: one message = `varint type`, `varint length`, body (PROTOCOL.md 5.1 and 9.2 primitives; `swapdesk-proto::{Reader, Writer, Wire}` are reused, and every proto type named below is embedded with its own `Wire` encoding). A length above the type's limit is refused **before** allocation; an unknown type is a protocol error (the pipe closes). Limits: 16 MiB for `VideoFrame` and `ViewerVideo`, 64 KiB for input batches, 1500 bytes for audio datagrams, 1 MiB + 4 KiB for `FsRequest` and `FsReply` (Files, below), 1 MiB for every other message. Blocking `FrameReader<R: Read>` / `FrameWriter<W: Write>` plus a pure `decode_message(&[u8])`; no tokio, no `unsafe`. A redacting `Debug` (no token, text or byte body printed, only lengths) on every message.

Every connection starts with `Hello` from the client and `HelloAck` from the service; nothing else is accepted first.

| Type | Name | Dir | Body |
|---|---|---|---|
| 0x01 | `Hello` | client→service | `role: u8` (1 ui, 2 helper-system, 3 helper-user), `version: u16` (1), `pid: u32` |
| 0x02 | `HelloAck` | service→client | `version: u16`, `win_session: u32` |
| 0x03 | `Bye` | both | `code: ErrorCode` |
| 0x20 | `StartVideo` | service→helper | `display: u16`, `fps: u16`, `kbps: u32`, `quality: Quality (proto)`, `backend: u8` (0 auto, 1 nvenc, 2 mf) |
| 0x21 | `StopVideo` | service→helper | `display: u16` |
| 0x22 | `SetRate` | service→helper | `display: u16`, `kbps: u32`, `fps: u16` |
| 0x23 | `ForceKeyframe` | service→helper | `display: u16` |
| 0x24 | `Input` | service→helper | `count: varint`, `InputEvent` × count (PROTOCOL 7.3), ≤ 64 KiB |
| 0x25 | `ReleaseAll` | service→helper | — |
| 0x26 | `StartAudio` | service→helper | `exclude_pid: u32` (0 = none) |
| 0x27 | `StopAudio` | service→helper | — |
| 0x28 | `SetAudioMuted` | service→helper | `muted: bool` |
| 0x29 | `Shutdown` | service→helper | — |
| 0x30 | `Displays` | helper→service | `DisplayList` |
| 0x31 | `VideoFrame` | helper→service | `VideoHeader` (32 B, PROTOCOL 7.1), payload bytes |
| 0x32 | `CursorPos` | helper→service | `CursorPosition` |
| 0x33 | `CursorShape` | helper→service | `CursorShape` |
| 0x34 | `AudioPacket` | helper→service | one audio datagram (PROTOCOL 7.2 header + Opus), ≤ 1500 B |
| 0x35 | `AudioMode` | helper→service | `mode: u8` (0 off, 1 loopback, 2 process-exclusion) |
| 0x36 | `DesktopChanged` | helper→service | `Desktop` |
| 0x37 | `SasRequest` | helper→service | — (honoured only for an `UNATTENDED` session) |
| 0x38 | `HelperFault` | helper→service | `code: ErrorCode`, `what: str(256)` (never a never-logged value) |
| 0x39 | `HelperNote` | helper→service | `what: str(256)`: logged by the service at INFO (the encoder serving a display and why, a rebuild); never a never-logged value |
| 0x40 | `UiRequest` | ui→service | `req: u32`, `op: u16`, op body (table below) |
| 0x41 | `UiReply` | service→ui | `req: u32`, `status: u16` (0 ok, else `ApiCode` + 1), body per op |
| 0x42 | `UiNotify` | service→ui | `kind: u16`, body (table below) |
| 0x50 | `ViewerVideo` | service→ui | `session_id: u64`, `VideoHeader`, payload |
| 0x51 | `ViewerAudio` | service→ui | `session_id: u64`, audio datagram |
| 0x52 | `ViewerCursorPos` / 0x53 `ViewerCursorShape` | service→ui | `session_id: u64`, proto cursor record |
| 0x54 | `ViewerInput` | ui→service | `session_id: u64`, `count`, `InputEvent` × count |
| 0x55 | `ViewerKeyframe` | ui→service | `session_id: u64`, `KeyframeRequest` |
| 0x56 | `ViewerReport` | ui→service | `session_id: u64`, `ReceiverReport` |
| 0x80 | `FsRequest` | service→user helper / ui | `req: u32`, `call: u8`, body (Files, below) |
| 0x81 | `FsReply` | user helper / ui→service | `req: u32`, ok + answer or error + `ErrorCode` (Files, below) |

`UiRequest.op` (stage 18 set; later stages add theirs through the main session): 1 `GetThisDevice`, 2 `GetNetwork`, 3 `ListDevices`, 4 `AddByAddress{ipv4: str(15)}`, 5 `PairOpen{device_id: opt str(64)}`, 6 `PairStart{pairing_id: u32, device_id}`, 7 `PairConfirm{pairing_id, matches: bool}`, 8 `PairCancel{pairing_id}`, 9 `SetGrants{device_id, grants: u16}`, 10 `ForgetDevice{device_id}`, 11 `Connect{device_id, intent: u8, unattended: bool}`, 12 `Disconnect{session_id: u64}`, 13 `AnswerIncoming{request_id: u32, decision: u8 (0 accept, 1 view only, 2 decline), grants: u16}`, 14 `SelectDisplay{session_id, display: u16}`, 15 `SetQuality{session_id, quality}`, 16 `SetAudio{session_id, on}`, 17 `SetViewOnly{session_id, on}`, 18 `SendKeys{session_id, keys: u8}`, 19 `GetSettings`, 20 `SetSettings{machine settings: name str(64), service_enabled, lock_on_end, keep_awake}`, 21 `ActivityList{before_ms: opt u64, limit: u32}`; stage 41 adds 22 `RequestSwap{session_id}`, 23 `AnswerSwap{request_id: u32, accept: bool}`, 24 `ChatSend{session_id, text: str(4096)}`, 25 `ChatMarkRead{session_id}`, 26 `Wake{device_id, intent: u8}`; stage 23 (Mac viewer) adds 27 `ListDisplays{session_id}`, answered with the session's display list as the host last sent it (the list `Notify::Displays` carries, empty before the first one; `NotFound` for a session that is not a viewer session of this connection). Reply bodies mirror the C6 `Reply` variant of the same command field by field (strings with the C6/C5 maximum lengths, lists with a `count` checked against the remaining bytes).

`UiNotify.kind`: 1 `DevicesChanged`, 2 `NetworkChanged`, 3 `Pairing{pairing_id, state}`, 4 `IncomingRequest`, 5 `IncomingClosed{request_id, reason}`, 6 `Session{session_id, device_id, state}`, 7 `Displays{session_id, list}`, 8 `Stats{session_id, latency_ms, fps, kbps}`, 9 `ActivityAlert`, 10 `SettingsChanged`; stage 41 adds 11 `Swap{session_id, note}`, 12 `Chat{session_id, item}`, 13 `ChatRead{session_id, up_to: u32}`; bodies mirror the C6 `UiEvent` fields.

**Readings fixed by stage 41 (swap, chat, wake, reconnect).** All of it runs in the service (no helper proxy); the UI only asks and draws.
- `RequestSwap` ("Inverter", from the viewer toolbar or the host banner) replies `RequestId(u32)` at once (reply body of op 22: `u32`); the outcome is `Swap{session_id, note}` with `note` = `Done` (swapped, or the session ended first: the card closes) or `Declined{why}`, `why: u8` 0 declined, 1 timed out (20 s asked side, 22 s asker), 2 nobody present, 3 not permitted (the side about to be controlled has not stored `CONTROL` for the other), 4 busy (one swap per session). Early refusals (3, 2, 4) send nothing to the other side (F-SWAP-02).
- The question to this machine's user is `Swap{session_id, note: Asked{request_id: u32, by: str(64), remaining_s: u16}}` to the UI that owns the viewer session with that device (when this machine would become the host) and to the console's UI; `AnswerSwap{request_id, accept}` resolves it (only a connection the card went to may; replies `NotFound` for an unknown id, `NotPermitted` for another connection), `Swap{.., Done}` closes it when it timed out or was cancelled. The new viewer session of an accepted swap reaches the console's UI as an ordinary `Session` (`Active`, `role: Viewer`), and the host side as `Active`, `role: Host`, with no prompt (`GrantMode::Swap`). There is no cancel op: the swap library has no cancel for an outgoing request.
- `ChatSend` replies `Chat(ChatItem{msg_id: u32, text: str(4096), sent_at_ms: u64})` (op 24 reply body), the message as stored; errors: empty or over-long text `Invalid`, the session's 500-message cap `Busy`, unknown or not this connection's session `NotFound`. The peer's messages are `Chat{session_id, item}`; `ChatMarkRead` tells the peer everything shown is read (`ChatRead{up_to}` at the peer, which marks its own messages read). Each side numbers its own messages from 1. The log lives in the service per session, in memory, and is dropped with the session (also across a resume: F-CHAT-01 clears it at the end of the connection). `ChatItem.text` is as private as input: its `Debug` prints a length.
- `Wake` replies `SessionId(u64)`: the id of the waking entry (bit 63 set), not a session. Progress is `Session{id, device_id, Waking{remaining_s}}` (60 at the start; the UI counts down), the packets go to the broadcast of the approved interface that holds the device's stored last address, TTL 1, to the MACs it reported after pinning, repeated every 5 s, and the hub asks discovery once a second whether the device appeared. On appearing the hub connects as `Connect` does, with `intent` (the UI sends `Control`), and the entry ends `Session{id, .., Ended{reason}}`: `None` when connected or cancelled, `Timeout` after 60 s, `Invalid` with no stored MAC, `Internal` when the socket failed; the connection's own `Session` notifications follow. Errors of the reply: `NotFound` (unknown device or no stored address), `Invalid` (no stored MAC), `Refused` (no approved interface holds the last address), `Internal`. `Disconnect{session_id: <waking id>}` cancels it.
- Reconnect: when a viewer session is lost (`EndKind::Lost`) the service resumes it with the stored token and the original request (same intent, `UNATTENDED` included) for up to 60 s, backoff 0.2 s doubling to 2 s, and tells the UI `Session{session_id, device_id, Reconnecting{attempt}}` (`attempt` from 1; 0 is the "peer silent" warning before the connection is declared lost), then `Active` again with the grants the host gave back; no `IncomingRequest` is raised at the host (F-NET-01). Not back in time, or refused for good: `Ended{reason: Timeout | Refused | ..}`. `Disconnect` while reconnecting stops the attempts and ends the session as a normal end. The `ViewerHandle` of the service keeps the session id and swaps its connection, so ops on the session keep working.

#### Clipboard and quick actions (stage 42)

Numbers used: UI ops 28 and 29 (30 is `RenameDevice`, see "Rename a paired device" below; 31 reserved), notify kind 14 (15 reserved), service-to-peer messages 0x60 to 0x66 (0x67 reserved). The service owns the LAN sockets and never touches the user's clipboard or shell (C2.3), so the C3 `ClipboardProvider` and `ActionRunner` are proxied: the host role's to the **user helper**, the viewer role's to the **UI process that owns the viewer session**. Each proxy is a blocking client in the service (a thread that may block, never the tokio executor) and a one-message server step in the helper or the UI.

| Type | Name | Dir | Body | Limit |
|---|---|---|---|---|
| 0x60 | `ClipChangeCount` | service to helper or UI | `req: u32` | 1 MiB |
| 0x61 | `ClipRead` | service to helper or UI | `req: u32` | 1 MiB |
| 0x62 | `ClipWrite` | service to helper or UI | `req: u32`, content | 16 MiB + 64 |
| 0x63 | `ClipReply` | helper or UI to service | `req: u32`, `tag: u8` (0 `Count{u64}`, 1 `Content`, 2 `Concealed`, 3 `Empty`, 4 `Unsupported`, 5 `Written`, 6 `Failed{u8: 0 unavailable, 1 invalid, 2 other}`) | 16 MiB + 64 |
| 0x64 | `RunAction` | service to user helper | `req: u32`, `action: Action` (PROTOCOL 5.3: 0 Lock, 1 Sleep, 2 Restart, 3 TaskMonitor) | 1 MiB |
| 0x65 | `KeepAwake` | service to user helper | `req: u32`, `on: bool` | 1 MiB |
| 0x66 | `ActionReply` | helper to service | `req: u32`, `code: ErrorCode` (`NO_ERROR` when it ran) | 1 MiB |

- **Content** is `tag: u8` (0 text, 1 PNG) and the bytes to the end of the body; text must be UTF-8. The size is checked against PROTOCOL.md 7.5 (1 MiB text, 16 MiB PNG) before anything is allocated, on both the writing and the reading side; the frame length is checked against the type's limit before the body is read.
- **Matching.** One `req` per call, chosen by the caller; the answer carries the same `req`. Calls to one pipe are answered in order. A caller waits at most 5 s for a clipboard call and 20 s for an action; a late answer is dropped. When the pipe ends every waiter fails at once. A process with no clipboard (or no actions) answers `Failed{other}` (or `UNAVAILABLE`) instead of staying silent. A UI connection never sends 0x60, 0x61, 0x62, 0x64 or 0x65 (they are the service's, a protocol error); a helper never sends 0x60 to 0x62.
- **Redaction.** `Debug` of 0x62 and 0x63 prints the kind and a length (`ClipData::Text(len 12)`), never the content or bytes; `Debug` of every other message of this block prints numbers only.
- **Rename a paired device.** UI op 30 `RenameDevice{device_id: str(64), name: str(64)}` overwrites `PairedDevice.name` of the trust record (a local alias: the peer is not told, the record's format does not change, and a name is never refreshed from the peer after pairing). Console UI only (`NotPermitted` otherwise, like Forget). The service trims the name and answers `Invalid` for an empty one, one over 64 bytes, or one holding a control character, `NotFound` for an unpaired device; it replies `Done` after the store is written and pushes `DevicesChanged`, and every place that shows the peer (banner, requests, activity, chat, lists) reads the record, so the new name shows everywhere.
- **Swap needs the same OS.** Swap between Windows and macOS is not in 1.0: the UIs offer "Inverter" only toward a peer whose OS equals this machine's, `RequestSwap` toward another OS replies `NotPermitted` at once, and a swap asked by a device of another OS is declined without a card.
- **UI ops.** 28 `SetClipboardSync{session_id: u64, on: bool}` flips this side's "Copiar e colar" switch of a session of this connection (its viewer session; on the console's UI also a session this machine hosts) and the peer is told (`ClipboardSync`); reply `Done`, `NotFound` for any other session. 29 `RunAction{session_id: u64, action: Action}` is for a viewer session of this connection only: the service sends `QuickAction` (0x0601) to the host and replies when the host's `QuickActionResult` (0x0602) arrives: `Done` when it ran; else `NotPermitted` (the session lacks `CONTROL`, the UI chose view-only, or the host said `NOT_PERMITTED` / `NOBODY_PRESENT`: Task Manager needs a user), `Timeout` (no result in 25 s), `Refused` / `Internal` for the other host codes, `NotFound` for an unknown session.
- **Notify 14** `ClipboardSkipped{session_id: u64, reason: ApiCode}`: an item was not synced, by this side or the peer. `reason`: `Invalid` too large, `NotPermitted` concealed (marked private by the program that copied it), `Refused` a format this version does not carry. It goes to the UIs that show the session. C6 `UiEvent::ClipboardSkipped` keeps its `reason: ApiCode`; the UI maps the three codes to its three strings.
- **Where each side runs.** Host role: one `ClipboardHandler` in the host registry whose provider is the attached user helper, looked up at each call (no helper: the call fails and nothing syncs). Viewer role: one handler per viewer session whose provider is the UI connection that owns that session (`ServiceBackend::set_clipboard` gives the UI process its `ClipboardProvider`; without one it answers `Failed{other}`). What is on a clipboard when a session starts, or when its provider first answers, stays local. Sync needs the `CLIPBOARD` grant, and a viewer now asks for it with `CONTROL` intent (`requested_for`); the host's user still decides.
- **Quick actions at the host.** The viewer's `QuickAction` needs `CONTROL`; the host's `ActionRelay` (`swapdesk-term`) runs it through a runner that goes to the user helper (`RunAction`) when one is attached and else to the service's own runner (`ServiceConfig::service_actions`; Windows: `WinActions::service()`: lock, sleep and restart as system calls, never a GUI process as SYSTEM). `KeepAwake` is proxied the same way for the `ActionRunner` trait. An action that ran is audited (`AuditEvent::ActionRun(action)`); one that was refused or failed is not.

#### Terminal (stage 43)

Numbers used: `UiRequest.op` 32 `TermOpen{session_id: u64, cols: u16, rows: u16, admin: bool}`, 33 `TermResize{term_id: u32, cols, rows}`, 34 `TermClose{term_id}`; `UiNotify.kind` 16 `TermExited{term_id, exit_code: opt i32}`; messages 0x78 `TermOutput{term_id: u32, bytes}` (service to UI) and 0x79 `TermInput{term_id, bytes}` (UI to service); service to user helper 0x70 `PtyOpen{id: u32, cols: u16, rows: u16, session_user: str(184)}`, 0x72 `PtyInput{id, bytes}`, 0x74 `PtyResize{id, cols, rows}`, 0x75 `PtyClose{id}`; helper to service 0x71 `PtyOpenReply{id, code: ErrorCode, account: str(64)}`, 0x73 `PtyOutput{id, bytes}`, 0x76 `PtyExited{id, code: opt i32}`. Bytes are raw (`bytes` is the rest of the body); a chunk is at most 64 KiB (`MAX_TERM_CHUNK`), so the body limits are 64 KiB + 4 for the four byte-carrying types, 512 for 0x70 and 0x71, 64 for 0x74 to 0x76; every limit is checked before allocation (also on encode). The `Debug` of every one prints lengths only (`Bytes`, `Text`): no terminal byte, no SID, no account.

- **Ids.** `term_id` is connection-wide, picked by the hub, so the UI's input and output never name a session; the hub keeps `term_id -> session` per connection and only the connection that opened a terminal may resize, close or type into it (`NotFound` / dropped otherwise). On the wire to the host the same number is the PROTOCOL `TermOpen.term_id`. `PtyOpen.id` is picked by the service, unique across helpers; a helper's `Pty*` message about an id it was not asked to open, or from the system helper, is ignored.
- **`TermOpen` reply** (op 32 body): `term_id: u32`, `outcome: u8`, `account: str(64)`. Outcomes: 0 opened, 1 not permitted, 2 nobody present, 3 declined (D13), 4 timed out (D13 question or shell start), 5 too many (four per session), 6 unavailable (no user helper, no administrator spawner), 7 failed. The status is ok for every outcome; an `ApiCode` error is for what cannot be placed: unknown session or not this connection's `NotFound`, size outside 1 to 1000 `Invalid`, `admin` from a connection that is not elevated `NeedsAdmin` (C2.3, D10). The reply can take as long as the host's user takes to answer (20 s) plus the shell's start; the UI waits 25 s. Ops 33 and 34 reply `Done`.
- **Who runs the shell.** A user terminal is a shell in the **user helper** (`PtyOpen`); the service never starts one itself. The helper builds its spawner for `session_user` (the SID of the console user, from the service's `TerminalEnv`) and the spawner re-runs the token guard (`check_spawn`) with the helper's own token: a service account, or a user other than `session_user`, is refused (`PtyOpenReply` `NotPermitted`); a system helper refuses every `PtyOpen`; a helper with no terminal support answers `Unavailable`. The administrator terminal is the only shell the service starts, through the `TerminalEnv` admin spawner (its own SYSTEM token in session 0; any other token is refused by the same guard) and needs `ADMIN_TERMINAL` granted and asked for in the viewer's `Hello` (an elevated UI connection asks for it when it connects).
- **Host side of a `TermOpen`** (after the session core's checks): D13 (`consent_need`: in an unattended session, a console user other than the one who granted `UNATTENDED` is asked through `request_consent`, nobody present is refused, the administrator terminal and prompted sessions never ask), then `TermHost::open`; a refusal is a non-fatal `Error{about_type 0x0501, about_id: term_id}` and gives the session's cap slot back, success is `TermOpened`. The viewer then opens the bidirectional stream `0x10` with `term_id` (4 bytes LE) and raw PTY bytes follow both ways; a terminal whose stream does not arrive in 10 s is closed. At the end of a shell the host finishes the stream, waits for the viewer to have the bytes (up to 2 s), then sends `TermExited`; a closed terminal sends none. Revoking `TERMINAL` (or `ADMIN_TERMINAL` for the administrator's) ends it within the second with `TermExited{None}`; the end of the session ends every shell. The audit log gets `TerminalOpened{admin}`.
- **Back-pressure, by not reading.** The helper's relay waits for room in its outbox and so in the pipe; the service reads the helper's pipe only as fast as the viewer takes the output (a 16-chunk channel per terminal, the stream write, QUIC flow control); toward the UI the output goes into a byte-bounded queue of its own that the writer serves after control messages and before video, and the producer waits for room; the shell's output pipe fills and the shell waits. Nothing is dropped. UI input into the service is bounded and never waits (a full queue loses keystrokes); a helper never waits for a shell that does not read its input. A terminal's bytes are never logged (F-SEC-01).
- **Unattended entry** grants `stored & requested`, so the hub also asks for `TERMINAL` when `Connect.unattended` is set (otherwise D13 could never be reached), and for `ADMIN_TERMINAL` from an elevated connection.

#### Files (stage 44, self-contained)

The service never opens a path a peer sent (C2.3): the disk work of F-FILE-01 and F-FILE-02 runs in the process of the user's own files, and the service's `FilesHandler` (`swapdesk-files`, over its `Fs` trait) reaches it through a proxy, `PipeFs`. Numbers used: messages 0x80 and 0x81 (on a helper pipe and on the UI pipe), `UiRequest.op` 40 to 45 (46 to 49 free), `UiNotify.kind` 20 and 21 (22 to 24 free). Every path and name on the wire is a `Text` and every byte body a `Bytes`, so no `Debug` prints a path or a content; every length is checked against its limit before anything is allocated.

- **Who serves the disk.** Host role: the **user helper** (`--role user`) serves the logged-in user's home folder; the service asks it (`HelperLink::fs_caller`). Viewer role: the **UI** that owns the viewer session serves it; the hub binds the session's disk to that connection when the session opens (`ViewerFiles::bind`), and the connection's own `FsReply` messages go to the call that waits. The system helper serves no disk (every call answers `IoFailed`). Calls block: they run in `spawn_blocking` inside `FilesHandler` or on a thread of their own, never on an executor thread. A call that gets no reply in 30 s, or whose pipe closes, fails with `IoFailed`.
- **0x80 `FsRequest`** (service to disk server): `req: u32`, `call: u8`, the call's body. **0x81 `FsReply`** (back): `req: u32`, then `u8` 0 and the answer (`u8` variant, its body) or `u8` 1 and an `ErrorCode` (`NotFound`, `IoFailed`, `InvalidName`, `TooLarge`, `ProtocolViolation`, `Busy` only). Limit of both: 1 MiB + 4 KiB (a full chunk plus its path and name); a declared length above it closes the pipe before a byte is read. `CloseRead` is fire and forget: no reply, and the `req` it carries (0) is not used. Calls (1 to 17, the `Fs` trait one for one): 1 `ListDir{path, max_entries}`, 2 `Stat{path, follow}`, 3 `OpenRead{path}`, 4 `ReadAt{id: u64, offset, len}`, 5 `ReadStat{id}`, 6 `CloseRead{id}`, 7 `StatFile{dir, name, kind}`, 8 `PartCreate{dir, name}`, 9 `PartTruncate{dir, name, len}`, 10 `PartAppend{dir, name, offset, data}` (data is the rest of the body), 11 `PartLen`, 12 `PartRead{dir, name, offset, len}`, 13 `PartSync{dir, name, final_mtime_ms: opt u64}`, 14 `PartRename{dir, name, target_name}`, 15 `RemoveFile{dir, name, kind}`, 16 `MetaRead{dir, name}`, 17 `MetaWrite{dir, name, data}` (data is the rest of the body). Answers: 0 `Done`, 1 `Entries{truncated, entries}`, 2 `Stat(opt)`, 3 `Handle{id, len, mtime_ms}`, 4 `Data` (rest of the body), 5 `StatNow`, 6 `Len`, 7 `Meta(opt)`. Limits: path 1024 bytes, name 255, a chunk (`PartAppend`, `PartRead`, `ReadAt`) 1 MiB (`TooLarge` above), `ListDir` at most 2000 entries whatever `max_entries` says, `MetaRead` at most 1 KiB, `MetaWrite` at most 1 KiB. `PartAppend` needs the part file to be exactly `offset` long (`ProtocolViolation` otherwise).
- **Paths are virtual and checked by the server.** A path is relative to a root the serving process chose, `/` separated (a `\` is refused), each component checked by the file-name rules of `swapdesk-files` (no `.`, `..`, empty component, `:`, control character, reserved device name, trailing dot or space, `.swapdesk-part` or `.swapdesk-meta` name): anything else, an absolute path included, is `InvalidName`. User helper: the root is the user's home folder, the empty path is the root, and `Downloads/Swapdesk` (the folder for a file that arrives with no folder named) is created on first use. UI: no root; only `p<N>/<file name>` (a file the user picked, read-only, the name must be the file's own) and `r<N>[/<folder>...]` (a receive folder the user chose), where `N` is a token the UI made (`Tokens`); nothing is listed, and a token the UI forgot names nothing. The real path of a pick or a receive folder never leaves the UI. `open_read` follows links on purpose (the sender's own reach); everything that writes refuses them. `LocalFs` re-checks every name itself.
- **Read ids** are `u64`s that live only in the serving process: they end with it (an unknown id is `NotFound`, shown as `IoFailed`), and `PipeFs` tags each id with the link that made it, so an id of a dead pipe never meets a file on the next one.
- **UI ops** (reply bodies in brackets): 40 `FilesList{session_id, path: opt str(1024)}` [`Listing{path, entries: [{name, is_dir, size, mtime_ms}] (at most 2000), truncated}`; `None` is the host's home, the path is as the host's helper spells it, relative to its root, empty for the root; answered when the host's `DirListing` arrives, `Timeout` after 8 s, since the protocol gives a refused or failed request no answer of its own], 41 `FilesSend{session_id, picks: [{token: u32, name: str(255)}] (1 to 64), remote_dir: opt str(1024)}` [`TransferIds`, one per pick in order; `remote_dir` `None` is the host's default receive folder `Downloads/Swapdesk`, `Some("")` its root], 42 `FilesGet{session_id, remote_path: str(1024), recv_token: u32}` [`TransferId`], 43 `TransferPause{transfer_id}`, 44 `TransferResume{transfer_id}`, 45 `TransferCancel{transfer_id}` [empty]. They act on a viewer session of the sending connection (`NotFound` otherwise); a name with a separator is `Invalid`; more than 8 unfinished transfers in a session is `Busy`; an unknown transfer is `NotFound`. Transfer numbers come from the hub, one counter for the machine.
- **Notifications:** 20 `Transfer{session_id, transfer_id, name: str(255), outgoing: bool, state}` with `state` `Queued`, `Running{done, total, bytes_per_s}` (at most 4 a second per transfer, the first and last always), `Paused{done, total, why}`, `Done` or `Failed{reason}`; 21 `SavedAs{session_id, transfer_id, name}`, sent just before the `Done` of a received file whose final name is not the offered one (files never overwrite: `name (1).ext`). Both go to the UI that owns the viewer session, and for a host session to the console's UI.
- **Resume.** A transfer's state is kept in the hub (how to start it again: the pick token and destination, or the remote path and receive token). A lost link pauses every transfer of the session (`Paused`; the receiver keeps its verified length on disk); when the reconnect of F-NET-01 brings the connection back, each unfinished transfer the user did not pause is offered again and the receiver continues from its verified length with no click. "Retomar" (44) continues a paused transfer in the live connection, or, when that connection no longer holds it, offers it again. Pause (43) is the user's: a reconnect does not undo it.
- **D13.** On the host the handler is behind a gate: a prompted session needs no question; in an unattended session the files of a user other than the one who granted `UNATTENDED` ask that user once (`request_consent(Feature::Files)`), the peer's file messages wait meanwhile (at most 64). Nobody at the console refuses with `NobodyPresent` at once and asks again with the next request; a "no" (`Declined`) holds for the session and answers every offer with `FileDecline`, and `DirRequest` and `FileGet` with an `Error` message. A resume of the same session keeps the answer.
- **Permissions.** A controlling viewer asks for `FILES` along with `CONTROL`, `AUDIO` and `CLIPBOARD` (`requested_for`; a terminal-using unattended entry adds `TERMINAL`, see stage 43), since an unattended session grants the stored permissions the viewer asked for; the host's user still decides what a prompted session grants.

**Readings fixed by stage 18a.** `UiReply.body` travels as bytes: the requester keeps the op of each `req` and decodes with `ReplyBody::decode(op, bytes)` (the wire has no op tag). `quality` is `swapdesk_proto::Preset` (Auto, Text, Motion; C6 `Quality`). `ViewerAudio` allows 1508 bytes (session id + a 1500-byte datagram). `grants` arrive as raw `u16`: the service validates them (PROTOCOL 6.1) before use.

**Development mode (stage 18).** `swapdesk-service --console` runs the same code as the user, without the SCM: pipes get the same DACL shape with the current user in the SYSTEM row's place, and helpers start with `CreateProcessW` as that user (the `--role system` helper then has no secure-desktop reach). The production path (`WTSQueryUserToken`, the SYSTEM token moved to the console session, `CreateProcessAsUserW`) is exercised only when the service is installed; that needs an elevated install on this PC (the owner's UAC click).

## C3. Platform traits (`crates/swapdesk-platform`)

**Sync, not async.** The traits are blocking calls with timeouts, used from dedicated OS threads; the network side (`quinn`, `tokio`) is async; the two meet in bounded `tokio::sync::mpsc` channels (`blocking_send` / `blocking_recv` on the thread side). Reasons: (1) the OS APIs are thread-affine: `SetThreadDesktop` binds a thread, DDA and MF objects live on one D3D11 device, SCK delivers on a dispatch queue, CGEventTap needs a run loop; tokio's work-stealing would move a task between threads. (2) A capture or encode call must never stall the network executor. (3) Fakes stay plain and deterministic. So the traits are not `Send` unless stated: an object is created on the thread that uses it.

`swapdesk-proto` defines the wire types used here (`InputEvent`, `VideoHeader`, `Preset`, `Action`, `H264Profile`, `PermissionSet`, `Desktop`). This crate depends on `swapdesk-proto` and std only.

```rust
// crates/swapdesk-platform/src/lib.rs (C3). No unsafe. Signatures are the contract; doc comments abbreviated.
use core::time::Duration;
use std::sync::mpsc::SyncSender;
use swapdesk_proto::{Action, Desktop, H264Profile, InputEvent, Preset, VideoHeader};

// ---------- errors ----------
pub type Result<T> = core::result::Result<T, PlatformError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub kind: ErrorKind,
    /// The operation, e.g. "DuplicateOutput", "SCStream.start".
    pub op: &'static str,
    /// HRESULT, OSStatus or errno, when there is one.
    pub os_code: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Desktop switch or mode change: call `reacquire` and go on.
    AccessLost,
    /// An OS permission is missing; the UI names it (F-FIND-04).
    PermissionDenied(OsGrant),
    Unsupported,
    /// Transient: no console session, device busy, the DDA four-process limit.
    Unavailable,
    Timeout,
    InvalidInput,
    /// The object was stopped or its peer went away.
    Closed,
    Os,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsGrant { ScreenRecording, Accessibility, InputMonitoring, ServiceApproval, Administrator }

impl core::fmt::Display for PlatformError { /* "<op>: <kind> (<os_code>) <detail>" */ }
impl std::error::Error for PlatformError {}

// ---------- time ----------
/// Microseconds of a monotonic clock shared by all processes of one machine
/// (QueryPerformanceCounter / mach_continuous_time). Cross-process consistency is UNPROVEN: stage 12 tests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoMicros(pub u64);

pub trait Clock: Send + Sync {
    fn now(&self) -> MonoMicros;
}

// ---------- displays and frames ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct DisplayId(pub u16);
/// Windows: the adapter LUID; macOS: 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct AdapterId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct Rect { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    /// Physical pixels in the virtual desktop (may be negative).
    pub bounds: Rect,
    pub scale_milli: u16,
    pub refresh_mhz: u32,
    pub rotation: u16,
    pub primary: bool,
    pub adapter: AdapterId,
}

/// A GPU surface owned by one platform crate (ID3D11Texture2D, CVPixelBuffer);
/// only that platform's codec or renderer downcasts it.
pub struct GpuSurface(pub Box<dyn core::any::Any>);

pub enum Pixels {
    /// BGRA8, rows of `stride` bytes.
    Cpu { stride: u32, data: Vec<u8> },
    Gpu(GpuSurface),
}

pub struct Frame {
    pub display: DisplayId,
    pub captured_at: MonoMicros,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    /// Empty = unknown, treat as the whole frame.
    pub dirty: Vec<Rect>,
    /// False when only the cursor moved.
    pub content_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape { pub id: u32, pub width: u16, pub height: u16, pub hot_x: u16, pub hot_y: u16, pub rgba: Vec<u8> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    pub display: DisplayId,
    /// Pixels inside the display.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// Present when the shape changed.
    pub shape: Option<CursorShape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig { pub max_fps: u16 }

pub trait ScreenSource {
    fn displays(&mut self) -> Result<Vec<DisplayInfo>>;
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()>;
    /// Blocks up to `timeout`. Ok(None): nothing new. Err(AccessLost): call `reacquire`.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;
    /// Follow the input desktop again and rebuild the capture (Windows); reopen the stream (macOS).
    fn reacquire(&mut self) -> Result<()>;
    /// The cursor change since the last call, if any; the cursor is never drawn into frames.
    fn take_cursor(&mut self) -> Option<CursorUpdate>;
    fn close(&mut self);
}

// ---------- video ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig { pub width: u32, pub height: u32, pub fps: u16, pub kbps: u32, pub preset: Preset, pub adapter: AdapterId }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo { pub name: String, pub hardware: bool, pub profile: H264Profile, pub level: u8, pub low_latency: bool }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub captured_at: MonoMicros,
    pub encode_us: u32,
    pub keyframe: bool,
    /// SPS/PPS included because the configuration or size changed.
    pub config_change: bool,
    pub width: u16,
    pub height: u16,
    pub annex_b: Vec<u8>,
}

pub trait VideoEncoder {
    fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo>;
    /// May return nothing while the encoder fills, or more than one frame.
    /// `force_keyframe` is best effort (VideoToolbox): check `keyframe` on the output and ask again.
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()>;
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;
}

pub struct DecodedFrame {
    pub display: DisplayId,
    pub frame_id: u32,
    pub captured_at: MonoMicros,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
}

pub trait VideoDecoder {
    /// Annex B in, at most one picture out (low-latency mode, no reordering).
    fn decode(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>>;
    /// Drop state after a gap; the next input must be a keyframe.
    fn reset(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode { Fit, OneToOne, Scroll { x: i32, y: i32 } }

/// Where the video goes: a node of the shell's own composition tree (D18, research/ui.md). Raw COM / Objective-C
/// pointers as integers, owned by the shell for the renderer's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSurface {
    /// The DirectComposition device, the parent visual placed under the toolbar visual, and the D3D11 device the
    /// MF decoder also uses (zero-copy: NV12 → video processor → composition swap chain → `SetContent`).
    Windows { dcomp_device: u64, parent_visual: u64, d3d11_device: u64 },
    /// The parent `CALayer`; the renderer adds an `AVSampleBufferDisplayLayer` under the toolbar view's layer and
    /// enqueues through `sampleBufferRenderer` (macOS 14+) with `DisplayImmediately`.
    MacOs { parent_layer: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct PresentStats { pub presented_at: MonoMicros }

pub trait Renderer {
    /// Adds the video node to the shell's tree. One window: the floating toolbar is a sibling node above it, drawn by
    /// the shell (no child HWND or NSView, no airspace problem).
    fn attach(&mut self, surface: NativeSurface, area: Rect) -> Result<()>;
    /// `area` is the video rectangle in window pixels, after the shell's layout.
    fn resize(&mut self, area: Rect) -> Result<()>;
    fn present(&mut self, frame: DecodedFrame, scale: ScaleMode) -> Result<PresentStats>;
    fn set_cursor(&mut self, cursor: &CursorUpdate) -> Result<()>;
    /// Pointer events come from the shell's window; the shell asks the renderer where a window point falls in the
    /// shown display: PROTOCOL.md 7.3 coordinates, or None outside the picture (letterbox, scrolled out).
    fn map_pointer(&self, x: i32, y: i32) -> Option<(u16, u16)>;
    fn detach(&mut self);
}

// ---------- audio ----------
/// 48 kHz, stereo, interleaved f32; 10 ms = 960 values.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBlock { pub captured_at: MonoMicros, pub samples: Vec<f32> }

pub trait AudioSource {
    /// `exclude_pid`: leave this process tree out (Windows process loopback, build 20348+; windows.md 6b).
    fn start(&mut self, exclude_pid: Option<u32>) -> Result<()>;
    fn next_block(&mut self, timeout: Duration) -> Result<Option<AudioBlock>>;
    fn stop(&mut self);
}

pub trait AudioSink {
    fn start(&mut self) -> Result<()>;
    fn write(&mut self, block: &AudioBlock) -> Result<()>;
    /// Audio written but not yet played.
    fn queued(&self) -> Duration;
    fn stop(&mut self);
}

// ---------- input ----------
pub trait InputSink {
    fn displays_changed(&mut self, displays: &[DisplayInfo]);
    /// Effects are checked by tests, never the return value alone (SendInput fails silently under UIPI, windows.md 5a).
    /// Windows: the user helper's sink (medium-integrity user token) serves prompted sessions and the system helper's
    /// sink (SYSTEM) serves only UNATTENDED sessions (D11); the session layer picks the sink, never the sink itself.
    fn apply(&mut self, event: &InputEvent) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
    /// Windows: OpenInputDesktop + SetThreadDesktop on the calling thread. macOS: nothing.
    fn follow_input_desktop(&mut self) -> Result<()>;
}

pub trait InputGrabber {
    /// Keyboard of the viewer while its window has focus: key records (and text records in text mode) go to `events`.
    fn start(&mut self, events: SyncSender<InputEvent>) -> Result<()>;
    /// The shell calls `start` when the viewer window gains focus and `stop` when it loses it (F-INPUT-04).
    /// Swallow system shortcuts (Alt+Tab, Win, Cmd+Tab) and send them instead.
    fn set_grab_system_keys(&mut self, on: bool) -> Result<()>;
    fn stop(&mut self);
}

// ---------- clipboard ----------
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipContent { Text(String), Png(Vec<u8>) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipRead { Content(ClipContent), Concealed, Empty, Unsupported }

pub trait ClipboardProvider {
    /// Changes whenever the clipboard changes (sequence number / changeCount); polled.
    fn change_count(&mut self) -> Result<u64>;
    fn read(&mut self) -> Result<ClipRead>;
    fn write(&mut self, content: &ClipContent) -> Result<()>;
}

// ---------- terminal ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyRequest { pub cols: u16, pub rows: u16, pub admin: bool }

pub trait Pty: Send {
    /// The account the shell runs as ("claudio", "SYSTEM", "root").
    fn account(&self) -> &str;
    fn reader(&mut self) -> Result<Box<dyn std::io::Read + Send>>;
    fn writer(&mut self) -> Result<Box<dyn std::io::Write + Send>>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn try_wait(&mut self) -> Result<Option<i32>>;
    fn kill(&mut self) -> Result<()>;
}

pub trait PtySpawner: Send + Sync {
    /// Own code (D14): ConPTY (`CreatePseudoConsole`) in `swapdesk-sys-win`, `openpty` in `swapdesk-sys-mac`.
    /// Spawns as the account of the calling process; `admin` is refused by a spawner that is not SYSTEM or root.
    /// The admin spawner runs only in the service (session 0) or the daemon, never in a process on the user's desktop.
    fn spawn(&self, request: &PtyRequest) -> Result<Box<dyn Pty>>;
}

// ---------- session, presence, actions ----------
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleState {
    pub session_id: Option<u32>,
    pub user: Option<String>,
    pub user_is_admin: bool,
    pub desktop: Desktop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    ConsoleChanged(ConsoleState),
    DesktopSwitched(Desktop),
    DisplaysChanged,
    Suspending,
    Resumed,
    NetworkChanged,
}

pub trait SessionWatcher {
    fn state(&mut self) -> Result<ConsoleState>;
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SessionEvent>>;
}

pub trait ActionRunner {
    /// Lock, Sleep, Restart, TaskMonitor (Task Manager / Activity Monitor). The user helper / Aqua agent runs every action;
    /// with nobody logged in, the service/daemon runs Lock, Sleep and Restart as system calls and refuses TaskMonitor.
    /// No implementation starts a GUI process as SYSTEM or root.
    fn run(&mut self, action: Action) -> Result<()>;
    /// Keep the machine awake during a session.
    fn keep_awake(&mut self, on: bool) -> Result<()>;
}

// ---------- network facts for the LAN guard ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfKind { Ethernet, WiFi, Virtual, Vpn, Tunnel, Loopback, Other }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetCategory { Public, Private, Domain }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInterface {
    pub index: u32,
    pub name: String,
    pub kind: IfKind,
    pub up: bool,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<(std::net::Ipv4Addr, u8)>,
    /// Windows only (NLM); None on macOS.
    pub category: Option<NetCategory>,
    /// Windows firewall state for this profile; None when unknown.
    pub inbound_blocked: Option<bool>,
}

pub trait NetInterfaces: Send + Sync {
    fn list(&self) -> Result<Vec<NetInterface>>;
}

// ---------- trace log (own logger, D14) ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level { Error, Warn, Info, Debug }

pub trait TraceSink: Send + Sync {
    /// `what` must never hold a never-logged value (C2.3); `swapdesk-fakes::LogCapture` checks it.
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>);
}

// ---------- secrets (C5) ----------
pub trait SecretStore: Send + Sync {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>>;
    fn store(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}
```

Wiring: each platform crate exposes plain constructors (`pub fn open(...) -> Result<impl Trait>`); the binaries pick the implementations. There is no global platform registry.

Note (stage 10): wall-clock time for C5 records and the lockout comes from a `WallClock` trait in `swapdesk-trust` (Unix ms), beside the monotonic C3 `Clock`.

## C4. Key tables (`crates/swapdesk-keys`)

- **Data file:** `crates/swapdesk-keys/data/keys.tsv`, UTF-8, LF, tab-separated, one header line, `#` comment lines; a header comment cites each source with its address and read date.
- **Columns:** `page` (hex, `0x07` keyboard or `0x0C` consumer), `usage` (hex), `name` (HID name, e.g. `KeyboardA`), `win_scan` (set-1 make code, hex; extended keys as `0xE0xx`; `-` = none), `win_vk` (hex, informative, for tests), `mac_kvk` (hex; `-` = none), `flags` (comma list: `iso_swap`, `altgr`, `numpad`, `media`, `jis`, `abnt2`), `note`.
- **Loading:** `include_str!` and one parse into lookup tables behind `std::sync::OnceLock`; no build script. API: `hid_to_win(page, usage) -> Option<u16>`, `win_to_hid(scan) -> Option<(u16, u16)>`, `hid_to_mac(page, usage, kb: KbKind) -> Option<u16>`, `mac_to_hid(kvk, kb: KbKind) -> Option<(u16, u16)>`.
- **Rules:** (1) The wire carries HID only; tables are used on the edges. (2) ISO swap: rows flagged `iso_swap` swap `kVK_ANSI_Grave` (0x32) and `kVK_ISO_Section` (0x0A) when the Mac's `KbKind` is `Iso` (values from macos.md 6b; the swap itself is UNPROVEN until probe P9). (3) AltGr: a Windows viewer drops the synthetic left Ctrl Windows sends just before Right Alt for AltGr (UNPROVEN detection rule; stage 16 tests it on ABNT2). (4) "Trocar Ctrl e Cmd no Mac" is applied by the viewer before sending (LCtrl↔LGui, RCtrl↔RGui), so host sinks stay table-only.
- **Tests (stage 16):** each `usage`, `win_scan` and `mac_kvk` unique per keyboard kind; round trips HID→Windows→HID and HID→macOS→HID; every ABNT2 and US key present, including International1 (0x87) and keypad comma (0x85) (HID values UNPROVEN until stage 16 cites the HID Usage Tables).

## C5. Trust store, settings and audit log

### C5.1 Records (encoded with the PROTOCOL.md 9.2 rules; `format: u16` = 1 first)
| Record | Fields |
|---|---|
| `Identity` | `format`, `pkcs8: bytes` (Ed25519 private key), `spki: [u8;44]`, `created_ms: u64` |
| `TrustStore` | `format`, `devices: Vec<PairedDevice>(64)`, `lockout: Lockout`, `blocked: Vec<[u8;4]>(256)` |
| `PairedDevice` | `device_id: [u8;16]`, `spki: [u8;44]`, `name: str(64)`, `os`, `os_version`, `paired_ms`, `last_seen_ms`, `last_addrs: Vec<[u8;4]>(4)`, `wol_macs: Vec<[u8;6]>(8)` (from `WakeInfo`, so only after pinning), `grants_here: PermissionSet` (what this machine lets that device do here: authoritative; **empty right after pairing**), `unattended_granted_by: Option<UserRef>` (who granted `UNATTENDED`: Windows SID string or macOS uid + name; used by D13), `grants_there: Option<PermissionSet>` (what that device last said it lets this one do there: display only, the other side decides) |
| `Lockout` | `failures: Vec<u64>(8)` (Unix ms of recent failed attempts), `provisional: Option<u64>` (an attempt in progress past step 2, counted until it pins), `locked_until_ms: u64`, `lockouts_24h: u8` |
| `MachineSettings` | `format`, `name: str(64)`, `service_enabled: bool`, `lock_on_end: bool`, `keep_awake: bool`, `port: u16`, `pair_port: u16` |
| `AuditEntry` | `time_ms: u64`, `device: DeviceRef { Known([u8;16]), Unknown { addr: [u8;4], device_id: Option<[u8;16]> } }`, `event: AuditEvent`, `duration_s: Option<u32>` |
`AuditEvent` = `Paired`, `PairFailed(ErrorCode)`, `PairingLocked`, `UnknownDeviceRejected`, `Suppressed { count: u32, kind: u8 }`, `SessionStarted { direction: Direction {Incoming, Outgoing}, mode: GrantMode, granted: PermissionSet, desktop: Desktop }`, `SessionEnded(ErrorCode)`, `Swapped`, `ConsentAsked { feature: Feature, answer: ErrorCode }`, `FilesSent { count: u32, bytes: u64 }`, `FilesReceived { count, bytes }`, `TerminalOpened { admin: bool }`, `ActionRun(Action)`, `GrantsChanged { from: PermissionSet, to: PermissionSet, by: UserRef }`, `Forgotten`, `AddressBlocked([u8;4])`, `PairRefused(ErrorCode)` (tag 17: a pairing attempt that ended before step 2; `PairFailed` is written only from step 2 on).

**Rate limit:** entries caused by unauthenticated peers (`UnknownDeviceRejected`, `PairRefused`, `AddressBlocked`; never `PairFailed` or `PairingLocked`) are written at most once per address per minute and 60 per hour in total; the rest are counted and written as one `Suppressed` entry per hour, written with the first entry of any kind after the hour or when the log closes (GUESS numbers).

Per-user UI settings (language, theme, keyboard mode, quality, overlay, Ctrl/Cmd swap, open at login, release key) are a `UserSettings` record in `%APPDATA%\Swapdesk\ui.bin` (Windows) or `~/Library/Application Support/Swapdesk/ui.bin` (macOS), written by the UI: not secret, not read by the service. Implemented in `swapdesk-ui` (`prefs.rs`: `UserPrefs`, `PrefsStore`, `FilePrefs`) and loaded by the app before the first paint (`UiState::load_prefs`). Layout: `format: u16` LE (1), then one byte each, in this order: language (0 PtBr, 1 En), theme (0 System, 1 Light, 2 Dark), quality (0 Auto, 1 Text, 2 Motion), show_stats, keyboard_mode (0 Auto, 1 Physical, 2 Text), swap_ctrl_cmd, open_at_login, release_key (0 CtrlAlt, 1 RightCtrl, 2 CtrlAltShift); exactly 10 bytes. A new field means `format` 2 (tags never renumber). A missing file is a first start (defaults); a file of another format, of the wrong length, with a tag out of range or unreadable reads as the defaults and leaves one trace line (reason only); it never stops the start. Written on each change to a temporary file beside it (`ui.bin.tmp`), flushed, then renamed over `ui.bin`. The UI's copy wins over the service's answer for these fields (the service answers defaults); the viewer pipeline takes the keyboard mode, the Ctrl/Cmd swap and the release key from it at start. Development instances (`--instance`, `--service-pid`) and scratch runs on the Mac do not read or write it.

### C5.2 On disk
| Item | Windows | macOS |
|---|---|---|
| Folder | `%ProgramData%\Swapdesk\`, owner SYSTEM, protected DACL (inheritance off): SYSTEM full, Administrators full, nobody else | `/Library/Application Support/Swapdesk/`, root:wheel 0700 |
| Identity | `identity.bin` = `CryptProtectData(Identity, entropy = "swapdesk identity v1", CRYPTPROTECT_LOCAL_MACHINE)` | System keychain, a generic-password item (service "Swapdesk", account "identity") holding the `Identity` record, through the SecItem API targeting the file-based keychain |
| Trust store, settings | `trust.bin`, `settings.bin`, DPAPI machine-scope blobs; written to a temp file in the same folder, flushed, renamed | `trust.bin`, `settings.bin`, root 0600, same write rule |
| Audit log | `audit-sessions.log` (sessions, grants, swaps, terminals, transfers, actions, pairings) and `audit-network.log` (unauthenticated and rate-limited entries): length-prefixed `AuditEntry` records appended; the network log rotates at 2 MiB, the sessions log at 10 MiB, entries older than 90 days dropped (GUESS defaults) | same, root 0600 |
| Trace log | `trace.log`, rotated at 10 MiB, never holding the never-logged items (C2.3) | same |

**Creating the folder (Windows).** `%ProgramData%` lets standard users create subfolders, which they would then own (UNPROVEN: the default ProgramData ACL, CREATOR OWNER full control, is not re-read today; stage 10 reads it with `icacls` on this PC). So at every start the service opens `Swapdesk` without following reparse points; if it exists with an owner other than SYSTEM or Administrators, or is a reparse point, it renames it aside (`Swapdesk.untrusted-<time>`), logs an alert and creates a fresh folder with owner SYSTEM and the protected DACL. Every file under it is opened without following reparse points. Uninstall deletes only the names in this table, then the folder if it is empty. macOS: the daemon creates its folder as root 0700 and refuses a pre-existing one not owned by root or that is a symlink, the same way.

**What DPAPI does and does not do.** With `CRYPTPROTECT_LOCAL_MACHINE`, "any user on the computer on which CryptProtectData is called can use CryptUnprotectData to decrypt the data" (https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata, read 2026-11-07), and any local user can likely create such a blob too (GUESS). Machine scope only ties the blobs to this PC; **the DACL is the only protection** against reading and tampering by other local users.

**Why the file-based keychain on macOS:** "Programs that run outside of a user context, like a launchd daemon, must target the file-based keychain" and "In the system context the search list includes just the System keychain" (TN3137, https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains, read 2026-11-07).

Uninstall deletes the known files and the keychain item (a "keep my devices" option is an owner decision for M8). The UI reads none of this: it asks the service (C6).

## C6. In-process UI API between `swapdesk-ui` and the native shells

D18: a native interface on each OS. Each OS has its shell (`swapdesk-shell-win`: Win32 + DirectComposition + Direct2D/DirectWrite + AccessKit; `swapdesk-shell-mac`: AppKit through objc2). Both sit on `swapdesk-ui`, which holds everything the two must agree on: the commands, the events, the view state, the strings (pt-BR, en) and the design tokens. This is a typed Rust API inside one process; there is no serialization and no JSON.

**Threads.** The shell's UI thread never blocks. It calls `UiBackend::send(Command)`, which queues the command for the **backend thread** and returns a `Ticket` at once. The backend thread runs the command: it talks to the service/daemon over the UI pipe/socket (C2), or does the work in the UI process itself for the commands marked **[UI]** (files, export, elevation), always with the user's rights and never sending a path to the service (C2.3). Results and service notifications come back as `UiEvent`s in a queue; the backend then wakes the UI thread through `UiWaker` (`PostMessageW(WM_APP)` on Windows, `dispatch_async` on the main queue on macOS). The UI thread drains the queue, applies each event to `UiState` (`UiState::apply`), and redraws only what changed (idle = no wake-ups, budget B8).

**Video and terminal.** Video frames never cross this API: the viewer pipeline (decoder → `Renderer`, C3) runs on its own thread and presents into the shell's composition tree directly. Terminal output reaches the terminal model through a bounded channel (256 chunks, GUESS) owned by the `TermView` of `swapdesk-ui`, which parses it with the terminal-emulation crate chosen in stage 20 (D19); the shells draw its grid.

**Texts and tokens.** Every string the UI shows is a `Msg` key with arguments, translated by `swapdesk-ui::strings::text(Language, &Msg)`; the shells hold no literal text. Tokens (colours for light and dark, Instrument Sans sizes and weights, spacing, radii, the pair-strip geometry) are constants in `swapdesk-ui::tokens`, read by both shells.

**Accessibility.** `swapdesk-ui::a11y` describes each screen as an `accesskit` tree (roles, names from `Msg`, states, actions); the Windows shell hands it to `accesskit_windows` (UI Automation), the macOS shell maps the same roles and names onto its AppKit controls (NSAccessibility). Text inputs are native controls on both OSs (`EDIT` on Windows, `NSTextField` on macOS) for IME.

```rust
// crates/swapdesk-ui (no unsafe). Signatures are the contract.

// ---------- plumbing ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct Ticket(pub u64);

pub trait UiBackend: Send + Sync {
    /// Never blocks: queues `command` for the backend thread. Its result arrives as `UiEvent::Reply { ticket, .. }`.
    fn send(&self, command: Command) -> Ticket;
    /// Drains the events queued since the last call (called on the UI thread after a wake-up).
    fn drain(&self) -> Vec<UiEvent>;
}

/// Implemented by each shell: wakes its UI thread from any thread.
pub trait UiWaker: Send + Sync { fn wake(&self); }

/// Implementations: `ServiceBackend` (this crate: backend thread over `swapdesk-ipc` on a stream the shell opened and
/// checked, C2), `MockBackend` (this crate, module `mock`, C7; stage 7 cannot hold it, since this crate arrives in stage 11).

// ---------- DTOs ----------
#[derive(Debug, Clone, PartialEq, Eq)] pub struct ApiError { pub code: ApiCode, pub detail: Option<Msg> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiCode { ServiceDown, NotFound, NotPermitted, NeedsAdmin, Busy, Timeout, Refused, Locked, Invalid, Internal }

pub enum Os { Windows, MacOs }
pub enum Permission { View, Control, Audio, Terminal, AdminTerminal, Files, Clipboard, Unattended }
pub struct Permissions(pub u16);                                    // the PROTOCOL.md 6.1 bit set

pub struct ThisDevice { pub name: String, pub os: Os, pub ready: Ready }
pub enum Ready { ReadyToReceive, ServiceOff, ServiceNeedsApproval, NoNetwork }
pub struct NetworkStatus { pub label: String, pub address: Option<String>, pub category: Option<NetCategoryView>, pub notices: Vec<Notice> }
pub enum NetCategoryView { Public, Private, Domain }
pub enum Notice { PublicNetwork, FirewallBlocks, ServiceNeedsApproval, MacPermissionsMissing(Vec<OsPermission>) }
pub enum OsPermission { ScreenRecording, Accessibility, InputMonitoring }

pub enum DeviceStatus { Online { address: String, rtt_ms: u32 }, Asleep, Offline, OnNetworkUnpaired { pairing_open: bool } }
pub struct DeviceView {
    pub id: String, pub name: String, pub os: Os, pub os_version: String, pub status: DeviceStatus,
    pub paired: bool, pub grants_here: Permissions, pub grants_there: Option<Permissions>,
    pub last_session: Option<SessionSummary>, pub wakeable: bool,
}
pub struct SessionSummary { pub at_ms: u64, pub duration_s: u32, pub direction: Direction, pub desktop: DesktopKind }
pub enum Direction { Incoming, Outgoing }
pub enum DesktopKind { Default, Secure, Locked, LoginWindow }

pub enum PairingState { Open { remaining_s: u16 }, Waiting, ShowCode { code: String, peer_name: String }, Done { device_id: String }, Failed { reason: ApiCode } }
pub enum Feature { Terminal, Files }
pub enum RequestKind { Session { intent: Intent }, Consent { feature: Feature } }
pub struct IncomingRequest { pub request_id: u32, pub device: DeviceView, pub kind: RequestKind, pub preset: Permissions, pub timeout_s: u16 }
pub enum Intent { View, Control }
pub enum Decision { Accept { grants: Permissions }, ViewOnly, Decline }   // D8: never Unattended or AdminTerminal here
pub enum ElevatedAction { SetGrants { device_id: String, grants: Permissions }, ConnectUnattended { device_id: String, intent: Intent }, OpenAdminTerminal { session_id: u64 } }

pub enum SessionState { Connecting, Prompted { remaining_s: u16 }, Active { since_ms: u64, role: Role, granted: Permissions },
                        Paused { desktop: DesktopKind }, Reconnecting { attempt: u32 }, Waking { remaining_s: u16 }, Ended { reason: Option<ApiCode> } }   // Ended None: a normal end
pub enum Role { Viewer, Host }
pub struct DisplayView { pub id: u16, pub name: String, pub width: u32, pub height: u32, pub scale_milli: u16, pub primary: bool }
pub enum Scale { Fit, OneToOne, Scroll }
pub enum Quality { Auto, Text, Motion }
pub enum KeyboardMode { Auto, Physical, Text }
pub enum SpecialKeys { CtrlAltDel, AltTab, WinOrCmd, AltF4OrCmdQ, PrintScreen }
pub struct Stats { pub latency_ms: u32, pub fps: u16, pub kbps: u32 }
pub enum SwapEvent { Asked { request_id: u32, by: String, remaining_s: u16 }, Declined { why: SwapWhy }, Done } // stage 41: why = Declined, TimedOut, NobodyPresent, NotPermitted, Busy
pub enum TermEvent { Opened { account: String }, Exited { code: Option<i32> } }   // bytes go through the TermView channel
pub struct DirEntryView { pub name: String, pub is_dir: bool, pub size: u64, pub mtime_ms: u64 }
pub struct Listing { pub path: String, pub entries: Vec<DirEntryView>, pub truncated: bool }
pub enum TransferState { Queued, Running { done: u64, total: u64, bytes_per_s: u64 }, Paused { done: u64, total: u64, why: ApiCode }, Done, Failed { reason: ApiCode } }
pub struct ChatView { pub msg_id: u32, pub mine: bool, pub text: String, pub sent_at_ms: u64, pub read: bool }
pub enum ActionKind { Lock, Sleep, Restart, TaskMonitor }
pub struct AuditView { pub at_ms: u64, pub device_name: String, pub what: Msg, pub duration_s: Option<u32>, pub alert: bool }
// device_name is empty for a device no longer paired (the UI writes "Dispositivo removido"); a session's line sits at its
// start and carries the duration its end recorded.
pub struct Settings {
    pub name: String, pub language: Language, pub open_at_login: bool, pub service_enabled: bool,
    pub lock_on_end: bool, pub keep_awake: bool, pub quality: Quality, pub show_stats: bool,
    pub keyboard_mode: KeyboardMode, pub swap_ctrl_cmd: bool,
    pub theme: Theme,   // appended last: the wire struct is positional, UI and service come from one build
}
pub enum Language { PtBr, En }
pub enum Theme { System, Light, Dark }   // wire tags 0, 1, 2 (never renumbered); System follows the OS, live
pub enum SettingsTarget { NetworkCategory, Accessibility, InputMonitoring, ScreenRecording, LoginItems }
pub struct Msg { pub key: MsgKey, pub args: Vec<String> }   // MsgKey: one variant per string of strings.rs

// ---------- commands (screen in brackets) ----------
pub enum Command {
    GetThisDevice,                                                       // [Dispositivos sidebar]
    GetNetwork,                                                          // [sidebar footer, Estados, Ajustes > Rede]
    ListDevices,                                                         // [Dispositivos]
    AddByAddress { ipv4: String },                                       // [F-FIND-03]
    PairOpen { device_id: Option<String> },                              // [Pareamento; "Parear este computador", 2 min, D12]
    PairStart { pairing_id: u32, device_id: String },                    // initiator, inside pairing mode only
    PairConfirm { pairing_id: u32, matches: bool },                      // [Os códigos são iguais / São diferentes]
    PairCancel { pairing_id: u32 },
    SetGrants { device_id: String, grants: Permissions },                // [Permissões] NeedsAdmin for Unattended/AdminTerminal (D7)
    ForgetDevice { device_id: String },                                  // [Permissões: Esquecer]
    RenameDevice { device_id: String, name: String },                    // [Dispositivo: Renomear] local alias, 1 to 64 bytes
    Elevate { action: ElevatedAction },                                  // [UI] UAC (runas) / Authorization Services; [elevation step]
    Connect { device_id: String, intent: Intent, unattended: bool },     // [Dispositivos: Só ver / Controlar]; unattended needs admin (D10)
    Disconnect { session_id: u64 },                                      // [Visualizador, Banner: Desconectar]
    AnswerIncoming { request_id: u32, decision: Decision },              // [Pedido de conexão; consent card, D13]
    ListDisplays { session_id: u64 },                                    // [Visualizador: Escolher tela]
    SelectDisplay { session_id: u64, display_id: u16 },
    SetScale { session_id: u64, scale: Scale },                          // [Ajustar à janela]
    SetQuality { session_id: u64, quality: Quality },
    SetAudio { session_id: u64, on: bool },                              // [Áudio]; off = nothing captured (D16)
    SetKeyboardMode { session_id: u64, mode: KeyboardMode },
    SetGrab { session_id: u64, on: bool },                               // [Capturar atalhos do sistema]
    SetViewOnly { session_id: u64, on: bool },
    SendKeys { session_id: u64, keys: SpecialKeys },                     // [Enviar Ctrl+Alt+Del e outras teclas]
    RequestSwap { session_id: u64 },                                     // [Inverter: toolbar and banner]
    AnswerSwap { request_id: u32, accept: bool },                        // [Inverter step 2]
    CancelSwap { request_id: u32 },
    TermOpen { session_id: u64, admin: bool, cols: u16, rows: u16 },     // [Terminal]; admin needs admin here (D10)
    TermWrite { term_id: u32, bytes: Vec<u8> },
    TermResize { term_id: u32, cols: u16, rows: u16 },
    TermClose { term_id: u32 },
    RunAction { session_id: u64, action: ActionKind },                   // [Ações rápidas]
    FilesList { session_id: u64, path: Option<String> },                 // [Arquivos: Pasta no MacBook, Trocar pasta, Abrir]
    FilesSend { session_id: u64, local_paths: Vec<std::path::PathBuf>, remote_dir: Option<String> }, // [UI] reads the files itself
    FilesGet { session_id: u64, remote_path: String, local_dir: std::path::PathBuf },                // [UI] writes into local_dir itself; [Baixar]
    TransferPause { transfer_id: u32 },
    TransferResume { transfer_id: u32 },                                 // [Retomar]
    TransferCancel { transfer_id: u32 },
    SetClipboardSync { session_id: u64, on: bool },                      // [Copiar e colar]
    ChatSend { session_id: u64, text: String },                          // [Chat]
    ChatMarkRead { session_id: u64, up_to: u32 },
    Wake { device_id: String },                                          // [Estados: Acordando]
    ActivityList { before_ms: Option<u64>, limit: u32 },                 // [Atividade]
    ActivityExport { path: std::path::PathBuf },                         // [UI] writes ActivityList's result itself; [Exportar registro]
    BlockAddress { ipv4: String },                                       // [Atividade alert: Bloquear]
    GetSettings,                                                         // [Ajustes]
    SetSettings { settings: Settings },                                  // machine fields go to the service; per-user fields stay in the UI (C5.1)
    OpenSystemSettings { target: SettingsTarget },                       // [UI] [Estados buttons]
    RequestOsPermissions,                                                // [UI] [Conceder permissões]
}

pub enum Reply {
    Done,
    ThisDevice(ThisDevice), Network(NetworkStatus), Devices(Vec<DeviceView>), Device(DeviceView),
    PairingId(u32), SessionId(u64), RequestId(u32), TermId(u32), Displays(Vec<DisplayView>),
    Listing(Listing), TransferIds(Vec<u32>), TransferId(u32), Chat(ChatView), Activity(Vec<AuditView>),
    Settings(Settings), MissingPermissions(Vec<OsPermission>),
}

// ---------- events ----------
pub enum UiEvent {
    Reply { ticket: Ticket, result: Result<Reply, ApiError> },
    DevicesChanged(Vec<DeviceView>),
    NetworkChanged(NetworkStatus),
    Pairing { pairing_id: u32, state: PairingState },                    // -> Pareamento
    IncomingRequest(IncomingRequest),                                    // -> Pedido de conexão, or the D13 consent card
    IncomingClosed { request_id: u32, reason: ApiCode },
    Session { session_id: u64, device_id: String, state: SessionState }, // -> Visualizador, Estados (Reconectando, Acordando), Banner (Host)
    Displays { session_id: u64, displays: Vec<DisplayView> },
    Stats { session_id: u64, stats: Stats },                             // once per second while "Mostrar latência e quadros" is on
    Swap { session_id: u64, event: SwapEvent },                          // -> Inverter
    Chat { session_id: u64, message: ChatView },
    ChatRead { session_id: u64, up_to: u32 },
    Term { term_id: u32, event: TermEvent },
    Transfer { transfer_id: u32, session_id: u64, name: String, outgoing: bool, state: TransferState }, // at most 4 per second each
    ClipboardSkipped { session_id: u64, reason: ApiCode },
    ActivityAlert(AuditView),                                            // -> Atividade alert (unknown device)
    SettingsChanged(Settings),
    ElevationResult { action: ElevatedAction, result: Result<(), ApiError> }, // -> the elevation step
    ThemeChanged { dark: bool },                                         // posted by the shell itself
    LanguageChanged(Language),
}

// ---------- view state ----------
/// The whole view model, owned by the UI thread. Pure: `apply` changes it, the shells only read it.
pub struct UiState { /* screen, selection, devices, sessions, prompts, transfers, chat, settings, theme, language */ }
impl UiState {
    pub fn apply(&mut self, event: UiEvent) -> Changed;      // Changed: which screens/regions to redraw
}
```

**Screens covered** (each `docs/design` board, plus the three states the design lacks): Dispositivos and Dispositivos escuro (`ListDevices`, `Connect`, `DevicesChanged`), Pareamento (`PairOpen`, `PairStart`, `PairConfirm`, `Pairing`), Permissões (`SetGrants`, `ForgetDevice`, `Elevate`), Pedido de conexão (`IncomingRequest` Session, `AnswerIncoming`), Visualizador and Visualizador escuro (`Session`, display, scale, quality, audio, keyboard, grab, view-only, `SendKeys`, `Stats`), Sendo controlado (`Session` with role Host, `Disconnect`, `RequestSwap`), Inverter controle (`RequestSwap`, `AnswerSwap`, `Swap`), Terminal e ações rápidas (`TermOpen` and the rest, `RunAction`), Arquivos (`FilesList`, `FilesSend`, `FilesGet`, transfers), Chat e copiar e colar (`ChatSend`, `ChatMarkRead`, `SetClipboardSync`, `ClipboardSkipped`), Erros e estados (`NetworkChanged` notices, `OpenSystemSettings`, `RequestOsPermissions`, Reconectando, `Wake`/Acordando, `SwapEvent::Declined`), Ajustes (`GetSettings`, `SetSettings`), Atividade (`ActivityList`, `ActivityExport`, `BlockAddress`, `ActivityAlert`); new: the D13 consent card (`IncomingRequest` Consent), the elevation step (`Elevate`, `ElevationResult`), "Parear este computador" (`PairOpen { device_id: None }`, `PairingState::Open`).

## C7. Fakes

| Fake | Crate | Implements | Behaviour | Its own test (stage 7) |
|---|---|---|---|---|
| `SyntheticScreen` | `swapdesk-fakes` | `ScreenSource` | CPU BGRA frames at a set size and fps; moving gradient and text so the encoder works; a 16×16-pixel-cell barcode (macroblock-aligned, luma 16 or 235) in the top-left carrying a 32-bit frame counter and the 64-bit `captured_at`; scripted `AccessLost` and display changes | the barcode reads back exactly from its own frames and from frames passed through a lossy JPEG-like quantizer (GUESS stand-in for H.264) |
| `BarcodeReader` | `swapdesk-fakes` | (helper) | reads counter and timestamp from a decoded frame for B1 and B2 | shares the test above |
| `RecordingInputSink` | `swapdesk-fakes` | `InputSink` | stores every event with its time; assertions on order and content | replays a script and compares |
| `SineSource` | `swapdesk-fakes` | `AudioSource` | 1 kHz sine at −12 dBFS, 48 kHz stereo, with a 5 ms marker burst every 1 s whose `captured_at` is logged | the marker positions match the log |
| `RecordingAudioSink` | `swapdesk-fakes` | `AudioSink` | stores blocks; finds marker onsets; counts underruns (a gap with nothing queued) | onset found within 1 ms on clean input |
| `FakeClock` | `swapdesk-fakes` | `Clock` | manual time; tests that need tokio time use `tokio::time::pause` (tokio feature `test-util`) | advance and read |
| `LogCapture` | `swapdesk-fakes` | `TraceSink` | captures every log line of a scripted pairing, session, terminal, transfer and chat run and fails if any never-logged value (C2.3) appears in it | the F-SEC-01 test itself |
| `FakeSessionWatcher`, `FakeActions`, `FakeClipboard`, `FakePtySpawner`, `MemorySecretStore`, `FakeNetInterfaces` | `swapdesk-fakes` | the trait of the same name | scripted events; recorded actions; in-memory clipboard with a concealed flag; an echo PTY; a map; interface lists including vEthernet, WSL, VPN, Wi-Fi, Ethernet | one test each |
| `ImpairedSocket` | `swapdesk-netsim` | `quinn::AsyncUdpSocket` around a real UDP socket, used through `Endpoint::new_with_abstract_socket` | loss %, delay + jitter, reorder %, bandwidth cap, link down/up at run time, seeded RNG | 1% loss measured within tolerance over 100 000 packets; link down drops everything; a quinn connection survives 1% loss |
| `MockBackend` | `swapdesk-ui` (module `mock`, built in stage 11) | `UiBackend` | one named scenario per approved screen state and per new state (consent card, elevation step, "Parear este computador"), chosen by `--mock <scenario>` in development builds of `Swapdesk.exe`; replies and events scripted with delays | each scenario drives `UiState` to its screen without an error; the shell captures each screen for the stage 11 comparison |

## Crates

All crates sit in `crates/` (workspace `members = ["crates/*"]`). Only `*-win` and `*-mac` crates may use `unsafe` (workspace lint `unsafe_code = "forbid"`, opted out per crate); the binaries hold no logic that tests need (flows cite library tests: a binary target's id is not scanned).

| Crate | Kind | Holds / implements | `unsafe` | Created in stage |
|---|---|---|---|---|
| `swapdesk-proto` | lib | C1 types, codec, limits, headers | no | 1 (exists), filled in 6 |
| `swapdesk-platform` | lib | C3 traits and types | no | 6 |
| `swapdesk-fakes` | lib (dev) | C7 fakes but the socket and `MockBackend` | no | 7 |
| `swapdesk-netsim` | lib (dev) | `ImpairedSocket` | no | 7 |
| `swapdesk-lan` | lib | LAN guard (on `NetInterfaces`), DNS-SD with `mdns-sd`, socket setup (TTL 1), Wake-on-LAN packet | no | 9 |
| `swapdesk-net-win` / `swapdesk-net-mac` | lib | `NetInterfaces` (GetAdaptersAddresses, NLM, firewall / getifaddrs + SystemConfiguration interface type, plain `en<N>` Wi-Fi or Ethernet only) | yes | 9 / 23 (D27) |
| `swapdesk-trust` | lib | identity, pairing (PROTOCOL.md 3), TLS configs and verifiers, quinn endpoint and connection setup, trust store, lockout, audit log format | no | 10 |
| `swapdesk-secret-win` / `swapdesk-secret-mac` | lib | `SecretStore` (DPAPI + folder DACL / keychain generic-password items, one per C5 name; login keychain and `~/Library/Application Support/Swapdesk` 0700 until stage 24 moves them to the System keychain and the root folder through `SecretLocation`) | yes | 10 / 23 (D27) |
| `swapdesk-ui` | lib | C6: `Command`, `Reply`, `UiEvent`, `UiBackend`, `ServiceBackend`, `MockBackend` (module `mock`), `UiState`, strings pt-BR/en (`strings.rs`), design tokens (`tokens.rs`), the AccessKit tree per screen (`a11y.rs`, `accesskit` crate), the viewer-session wiring over C3 traits, the [UI] file work | no | 11 |
| `swapdesk-termview` | lib | terminal model: the D19 terminal-emulation crate, grid, scrollback, selection, the bounded output channel | no | 20 |
| `swapdesk-paint` | lib | the screens drawn once for both systems (D37): the `Painter` trait (rectangles, rounded boxes, lines, circles, arcs, SVG icon paths, shadows, clips, laid-out text with an opaque `Label` handle), the `Frame` with its widgets, and the main, viewer (drawer, files, chat, clipboard, terminal slot) and banner screens; reports widgets, edit slots, the video and terminal rectangles; no OS API | no | 26 |
| `swapdesk-shell-win` | lib | the Win32 shell: windows, DirectComposition tree, Direct2D/DirectWrite implementation of `swapdesk-paint`'s `Painter`, `accesskit_windows` adapter, native `EDIT` inputs, per-monitor DPI v2, dark mode (DWM attribute and theme listener), own file dialogs (`IFileOpenDialog`/`IFileSaveDialog`), UAC relaunch, autostart, UI pipe open with the server check (C2.1); in stage 20 also the terminal panel (`terminal.rs`) | yes | 11 |
| `swapdesk-shell-mac` | lib | the AppKit shell: the CoreGraphics/CoreText implementation of `swapdesk-paint`'s `Painter` (`painter.rs`, over hand-declared CoreFoundation, CoreGraphics, CoreText and ImageIO functions in `cg.rs`); every screen with AppKit controls and layer-backed views, NSAccessibility, `NSOpenPanel`/`NSSavePanel`, Authorization Services step, login item, socket open with the owner check, the terminal panel | yes | 23 |
| `swapdesk-app` | bin | `Swapdesk.exe` / `Swapdesk.app` main: picks the shell, the backend (`ServiceBackend` or, in development, `MockBackend`) and the viewer pipeline | no | 11 |
| `swapdesk-capture-win` | lib | `ScreenSource` (DDA, cursor, displays) | yes | 12 |
| `swapdesk-codec-win` | lib | `VideoEncoder`, `VideoDecoder` (Media Foundation) | yes | 13 |
| `swapdesk-video` | lib | stream-per-frame sender and receiver, staleness, rate control, keyframe requests | no | 14 |
| `swapdesk-session` | lib | control stream state machine (PROTOCOL.md 5, 6), permission enforcement, presence, resume, the `ChannelHandler` registry the feature crates plug into | no | 14 |
| `swapdesk-render-win` | lib | `Renderer` (D3D11 video processor into a composition swap chain set as the content of the shell's video visual) | yes | 15 |
| `swapdesk-keys` | lib | C4 tables | no | 16 |
| `swapdesk-input-win` | lib | `InputSink` (SendInput; one instance in each helper, D11), `InputGrabber` (`WH_KEYBOARD_LL`); in stage 22 also `ClipboardProvider` (`clipboard.rs`) | yes | 16 |
| `swapdesk-audio` | lib | Opus encode/decode (`opus` crate with libopus; `opus-rs` if libopus does not build, D16), jitter buffer, datagram packets | no | 17 |
| `swapdesk-audio-win` | lib | `AudioSource` (WASAPI loopback, process loopback), `AudioSink` | yes | 17 |
| `swapdesk-ipc` | lib | local message set and framing over `Read`/`Write` (C2) | no | 18 |
| `swapdesk-sys-win` | lib | service control (`windows-sys` Services API), helper launch (tokens), pipes and their ACLs, `SendSAS`, `SessionWatcher`, `keep_awake`, `TraceSink` to the C5 folder; in stage 20 also `ActionRunner` (`actions.rs`) and the ConPTY `PtySpawner` (`pty.rs`) | yes | 18 |
| `swapdesk-host` | lib | OS-free process logic of stage 18, generic over C3 traits and `Read`/`Write`: the helper's loop (C2.4 service→helper messages drive capture → encode → `VideoFrame`, cursor, displays, input → `InputSink`, audio → `AudioPacket`; one capture thread per display, input on its own thread), the service's relays (helper messages ↔ `swapdesk-video`/`swapdesk-audio`/session on the host side; viewer streams ↔ `ViewerVideo`/`ViewerAudio`/`ViewerInput` on the UI pipe), helper supervision with the restart backoff | no | 18 |
| `swapdesk-service` / `swapdesk-helper` | bin | Windows processes of C2.1: only the wiring of `swapdesk-host` to the `*-win` crates | no | 18 |
| `swapdesk-swap` | lib | swap state machine (PROTOCOL.md 6.3) as a `ChannelHandler` | no | 19 |
| `swapdesk-term` | lib | terminal relay (PTY bytes ↔ 0x10 streams; the spawner is C3 `PtySpawner`, used in the user helper / Aqua agent and in the service / daemon for the admin terminal), quick-action relay, the D13 consent step | no | 20 |
| `swapdesk-files` | lib | offers, resume, SHA-256 check, folder listing | no | 21 |
| `swapdesk-extras` | lib | clipboard sync, chat, Wake-on-LAN sender, reconnect policy | no | 22 |
| `swapdesk-core-mac` | lib | `mac_service_config`: assembles `ServiceConfig` on macOS (identity, trust, settings, net-mac, secret-mac), as `swapdesk-service/src/app.rs` does on Windows; the app embeds `ServiceCore` with it until the stage 24 daemon (D27) | no | 23 |
| `swapdesk-capture-mac` | lib | `ScreenSource` and `AudioSource` (ScreenCaptureKit) | yes | 24 |
| `swapdesk-codec-mac` | lib | `VideoEncoder`, `VideoDecoder` (VideoToolbox) | yes | 23 (decoder), 24 (encoder) |
| `swapdesk-render-mac` | lib | `Renderer` (`AVSampleBufferDisplayLayer` / `sampleBufferRenderer` under the shell's toolbar layer) | yes | 23 |
| `swapdesk-input-mac` | lib | `InputSink` (CGEventPost), `InputGrabber` (CGEventTap), `ClipboardProvider` | yes | 23 (grabber), 24 (sink, clipboard) |
| `swapdesk-audio-mac` | lib | `AudioSink` | yes | 23 |
| `swapdesk-sys-mac` | lib | SMAppService, the LoginWindow plist writer, Unix sockets and peer checks (`libc`), `SessionWatcher`, `ActionRunner`, the `openpty` `PtySpawner`, `TraceSink` | yes | 24 |
| `swapdesk-daemon` / `swapdesk-agent` | bin | macOS processes of C2.2 | no | 24 |

## Lane map (M1 to M4)

A lane owns its crates (every file in them, its `Cargo.toml` included) for its stage. The main session alone edits the workspace `Cargo.toml`, `Cargo.lock`, `docs/` contracts, `docs/FLUXOS.md` merges, and the wiring lines that register a feature crate in `swapdesk-session`, `swapdesk-app` and the binaries. A lane that needs another lane's crate changed stops and asks the main session.

| Milestone | Lane (stage) | Owns | Reads only |
|---|---|---|---|
| M1 | 9 Discovery + LAN guard | `swapdesk-lan`, `swapdesk-net-win` | proto, platform, fakes |
| M1 | 10 Pairing + trust + TLS | `swapdesk-trust`, `swapdesk-secret-win` | proto, platform, fakes, netsim |
| M1 | 11 UI state + Windows shell | `swapdesk-ui`, `swapdesk-shell-win`, `swapdesk-app` | proto, platform, fakes |
| M2 | 12 Capture-win | `swapdesk-capture-win` | platform, fakes |
| M2 | 13 Codec-win | `swapdesk-codec-win` | platform, fakes |
| M2 | 14 Session + video transport | `swapdesk-session`, `swapdesk-video` | proto, platform, trust, fakes, netsim |
| M2 | 15 DComp video visual + renderer (after 12 to 14 merge) | `swapdesk-render-win` | the rest; the shell exposes its `NativeSurface` |
| M3 | 16 Input-win + keys | `swapdesk-input-win`, `swapdesk-keys` | platform, fakes |
| M3 | 17 Audio | `swapdesk-audio`, `swapdesk-audio-win` | platform, fakes, netsim |
| M3 | 18 Service + helpers + IPC | `swapdesk-ipc`, `swapdesk-sys-win`, `swapdesk-host`, `swapdesk-service`, `swapdesk-helper` | everything merged before |
| M4 | 19 Swap | `swapdesk-swap` | session, trust |
| M4 | 20 Terminal + actions | `swapdesk-term`, `swapdesk-termview`, `swapdesk-sys-win/src/actions.rs`, `swapdesk-sys-win/src/pty.rs` and `swapdesk-sys-win/Cargo.toml`, `swapdesk-shell-win/src/terminal.rs` | session, ipc, ui |
| M4 | 21 Files | `swapdesk-files` | session, ipc |
| M4 | 22 Clipboard, chat, wake, reconnect | `swapdesk-extras`, `swapdesk-input-win/src/clipboard.rs` and `swapdesk-input-win/Cargo.toml` | session, lan, trust |

No two lanes of one milestone own the same file. Before M4 starts, the main session adds the empty `actions.rs`, `pty.rs`, `clipboard.rs` and `swapdesk-shell-win/src/terminal.rs` modules and their `mod` lines, so lanes 20 and 22 never touch a `lib.rs` they do not own; a lane of M4 that needs a new string or token asks the main session, which owns `swapdesk-ui` after stage 11. In M5, stage 23 owns `swapdesk-shell-mac`, `swapdesk-render-mac`, the viewer parts of `swapdesk-codec-mac`, `swapdesk-input-mac` and `swapdesk-audio-mac`; stage 24 owns the rest of the mac crates. Lanes never run more than four at once (project rule); M2 runs 12, 13 and 14 together, then 15.

## Readings fixed by stages 12 and 13

- Capture (12): a static screen gives `Ok(None)` after the timeout; a cursor-only change returns `Ok(None)` at once and the caller reads `take_cursor`; `CursorUpdate.x,y` is the hot-spot position in display pixels; GPU frames are `Pixels::Gpu(GpuSurface(Box<GpuFrame>))` holding a pooled BGRA `ID3D11Texture2D` (binds SHADER_RESOURCE + RENDER_TARGET: the video processor's input view refuses SHADER_RESOURCE alone, stage 18) filled by `CopyResource` on the capturing adapter's device (multithread-protected); `captured_at` is the DDA present time (QPC µs) clamped non-decreasing; empty dirty rects mean the whole frame; display ids come from `\.\DISPLAYn`; `DuplicateOutput` failing with NOT_CURRENTLY_AVAILABLE, SESSION_DISCONNECTED, UNSUPPORTED or E_ACCESSDENIED is `Unavailable` (retry after a desktop switch); a lost duplication fails with `AccessLost` until `reacquire` succeeds; `DxgiScreen` is `!Send` and one per display. DDA on this PC stalls up to about 490 ms at times: the video sender repeats the last frame instead of assuming steady 60 fps. Advanced Color displays (HDR or automatic colour management, 09/11): the duplication is made with `DuplicateOutput1` asking for FP16 and BGRA; an FP16 scRGB frame is divided by the display's SDR white level (`DISPLAYCONFIG_SDR_WHITE_LEVEL`, re-read every 2 s), clamped and sRGB-encoded into the BGRA frame by a pixel shader (`CaptureStats.converted` counts them). The legacy `DuplicateOutput` clamped scRGB without the white level: at 320 nits every colour came out 4x too bright in linear light and everything above sRGB ~138 white.
- Codec (13): `EncoderConfig.adapter` is the LUID (`high << 32 | low`), 0 = first hardware adapter; GPU input is the capture's BGRA texture on the encoder's device (else `Unsupported`); decoder GPU output is a single-slice NV12 texture taken from a ring of `GPU_RING` (6) reused textures, so a picture is valid only until 6 more pictures are decoded and consumers present or copy it before that (stage 38; a texture per picture leaked 2 KB a frame), CPU output BGRA with `stride = width*4`; SPS/PPS on every keyframe, `config_change` on the first keyframe after a (re)configure; Texto nítido caps at 30 fps and doubles the bitrate (≤ 200 Mbps); `set_rate` scales to the configured fps and rebuilds when the bitrate is not live-modifiable; a frame-id gap or the discontinuity flag resets the decoder until a keyframe; nothing is `Send`. Default encoder: the capture adapter's hardware MFT (Quick Sync on this PC), then the Windows software MFT. The level in the output type covers the mean bitrate too (Table A-1 MaxBR, High): the NVIDIA H.264 MFT refuses a bitrate over its level. The NVIDIA H.264 MFT (activatable since the NVIDIA GPU drives a display, 09/11) runs CBR in every preset and takes a new mean and peak live (`preset::LiveRate`); the output type carries BT.709 limited range, which the NVIDIA MFT writes into the VUI (Quick Sync and the software MFT write no colour description; VideoToolbox takes that as BT.709 at HD sizes).
- Session (14): resume while the old connection is live waits up to 8 s for the host to see the loss, then falls back to the normal path; bad/used/foreign/expired tokens fall back silently (a wrong-device attempt does not burn the token); lock-on-end makes a lost connection the end (no resume); a grant change kills the token; stored-grant changes only narrow live sessions. NOT_PERMITTED/BUSY/UNAVAILABLE are non-fatal `Error` replies; a permitted request with no handler is UNAVAILABLE. Turning audio off needs no permission; SendSpecial needs UNATTENDED. Stream stop codes: 0x1001 host stale reset, 0x1002 viewer stale stop, 0x1003 budget stop. One live session per host. Liveness: Ping every 1 s, offline after 5 s, give up after 8 s. Video: a keyframe resets older unfinished streams of that display; P-frame backlog over 100 ms resets and cuts the bitrate; keyframe requests at most every 200 ms; 32 MiB receive budget.
- Audio (17): Opus 10 ms frames (LowDelay, CELT only, 128 kbit/s constrained VBR); CELT has no in-band FEC, so loss is concealed by PLC; jitter target 1.1 × peak lateness, 20 to 40 ms; mute sends header-only datagrams with the MUTED flag; audio off never touches the source. `AudioSource::start(Some(pid))` is a request: process-exclusion loopback when available, else plain loopback, reported by `mode()`. `AudioSink::write` drops what exceeds the 40 ms endpoint queue. The receiver schedules by sequence and arrival, never by `capture_us`. Building libopus inside a deep worktree path needs a short CARGO_TARGET_DIR (MAX_PATH in CMake try-compile).
- Input (16): C4 `KbKind` {Ansi, Iso, Jis} in swapdesk-keys; keyboard-page volume usages alias to consumer 0x0C/0xE2, 0xE9, 0xEA on the wire; ISO swap on HID 0x35/0x64 for Iso Macs only; Pause and consumer keys are injected by virtual key, keyboard-page keys by scan code (E0 = extended); the sink sends AltGr as one extended right Alt; the grabber drops Windows' fake keys (scan code bit 9, e.g. 0x21D before AltGr on ABNT2: C4 rule 3 proven on Windows 11). Pointer: pixel = round(v·(size−1)/65535) + display origin, then to the virtual desktop. `Key.locks` is the viewer's toggle state; the sink matches Caps/Num/Scroll before a non-lock key and syncs modifiers to the record. The grabber ignores injected events by default (a local host never feeds itself); text mode uses ToUnicodeEx with dead keys composing into the next key; `\n`, `\r`, `\t` go as keys.
- Render (15): the renderer shares the shell's D3D11 device (minimum-power adapter, Intel here) with the decoder; zero copy. A shell on the NVIDIA adapter halves the frame rate on this PC (Present blocks ~33 ms; cause unproven), so `SWAPDESK_ADAPTER=high` is not offered for viewing. `attach` adds a video visual and a cursor visual above `parent_visual`'s children; `present` waits ≤ 50 ms on the latency object and a timeout drops that picture (non-fatal); `presented_at` is QPC µs after `Present`; GPU frames must be on `WinRenderer::device()` (NV12 needs SHADER_RESOURCE + RENDER_TARGET binds); `map_pointer` takes client pixels, None before the first picture or in the letterbox; Fit scales both ways with one factor; 1:1 = `Scroll{0,0}`; a lost device is `Unavailable` (detach and attach). Stage 18 should set multithread protection in `Gfx::new` before D2D and marshal `WM_SIZE`/`WM_DPICHANGED` to `renderer.resize(video_area())`.
- NVENC (35, D21): `H264Encoder` facade with `EncoderBackend {Auto, Nvenc, MediaFoundation}`; Auto takes NVENC when the capture adapter is NVIDIA (0x10DE) and it opens, else Media Foundation (reason kept). NVENC API 13.1 from nv-codec-headers n13.1.15.0, runtime-loaded; struct layouts checked against MSVC sizeof/offsetof. The capture BGRA texture registers as ARGB without a copy. `set_rate` is always live; `flush` ends the session. The helper's capture loop reconfigures the encoder when `set_rate` fails instead of ending the video, and sends the encoder in use (name, fallback reason, size, fps, kbps, preset) as a `HelperNote` the service logs at INFO. The glue keeps the `GpuFrame` alive until `encode` returns and passes `gpu_payload(f).device()` as the encoder device. Measured: NVENC 2.18 ms median at 1080p60, 3.56 ms at 1440p60 (Quick Sync 7.97 / 11.97 ms).
- Session fixes (37): prompted grants never keep UNATTENDED; ADMIN_TERMINAL only if stored and requested (D8/D10 over PROTOCOL 5.3's "unless stored"); bye, grant and desktop changes travel on a control state polled before every event; `disconnect()` kills the resume token at once; replies to the peer never block the state machine; streams are dispatched on the state machine after `on_start`; one pending prompt per device; Deciding has a deadline; the host admits no stream before the grant; a frame stream reset before its kind is dropped silently (PROTOCOL 4 stale reset). New public API: `HostUi::consent_closed`, `Forwarded::still_permitted` (consumers must call it), `VideoReceiver::set_displays` (call on every DisplayList), `ReceiverStats::ticks`, `SequencerConfig::max_parked_bytes`; the video receiver keeps decode order under one lock and bounds parked and queued bytes. Open: derived Debug in swapdesk-proto prints tokens and chat/clipboard bodies (stage 18 adds redacting Debug there).
- Windows App Control can block a freshly built unsigned test binary (os error 4551); deleting that exe and rebuilding clears it.
- Service, helpers and UI (18): `swapdesk-host` holds the OS-free logic (helper loop, `ServiceCore`, `ui_hub`); the binaries only wire it. The helper builds its screen, encoder and input sink inside the thread that uses them (DDA source and `WinInputSink` are `!Send`); `CaptureEncoder` hands the encoder the capture texture and keeps the `GpuFrame` alive during `encode`; capture textures bind SHADER_RESOURCE + RENDER_TARGET (the video processor refuses SHADER_RESOURCE alone). Input goes to the system helper only for sessions holding UNATTENDED, else to the user helper, never falling back (D11); video, cursor and audio come from the system helper, else the user helper (dev console). `Msg` travels as the index in `MsgKey::ALL` (UI and service from one build); UI commands with no stage-18 op reply `Internal`; a broken UI pipe fails every pending ticket with `ServiceDown` (no C6 event). `SessionState::Ended { reason: None }` is a normal end. A device paired through a typed address is dialled at that address (session port = pairing port − 1). `--console --instance <n> --dev-loopback` runs two services on one PC (ports 47820+10n / 47821+10n, pipes `ui-<session>-<n>`); `Swapdesk.exe --instance <n> --service-pid <pid>` connects to one. Production (`SwapdeskService` under the SCM, SYSTEM token, `session_user_sid`) is compiled and unit-tested only: it runs after an elevated install. M3 gate: `crates/swapdesk-service/tests/gate_m3.rs` (interactive), green 08/11. DDA delivers frames only on change, so the gate checks function, not rate.
