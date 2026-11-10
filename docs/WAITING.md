# Waiting in the interface

Status: design note, proposed. Nothing here is built yet. The texts in section 5 are proposals: the interface
languages and their wording are a product decision.

The request behind it: on the Mac, "Controlar" was pressed; the Windows machine showed the request to accept and
pick permissions, and the Mac showed nothing at all while it waited. The same must hold for every action, on both
systems and in both roles, that waits for something: the other machine, a person, or the system.

What this note holds:

1. The audit of every wait (section 1).
2. The rule R-UI-13 for `docs/SPEC.md` (section 2).
3. The complete design of the connect wait (section 3).
4. The smallest fix for every other gap (section 4).
5. Every new or changed text, pt-BR and en (section 5).
6. The build plan in thin slices, with the tests that prove each (section 6).
7. The options that were rejected (section 7).

Paths are relative to the repository root. "Read" means the claim comes from reading the code at the time of
writing; nothing was run. Anything marked **GUESS** must be checked by the implementer before it is relied on.

---

## 1. Audit

### 1.1 How the connect wait works today (verified by reading)

- `Controlar` and `Assistir` (`crates/swapdesk-ui/src/interact.rs`, `ControlId::ViewOnly | ControlId::Control`) push
  `Command::Connect { unattended: false }`. Nothing else changes on screen: the buttons stay, enabled.
- The hub (`crates/swapdesk-host/src/ui_hub/ops.rs`, `request_other`) starts a thread `ui-hub-connect` that calls
  `Hub::connect`, which calls `ServiceCore::connect_asking` (`crates/swapdesk-host/src/service/mod.rs`), a
  `block_on` of `Connector::connect`. That returns only after the host decided: QUIC handshake (10 s limit,
  `crates/swapdesk-trust/src/endpoint.rs` `HANDSHAKE_LIMIT`), the control stream (10 s,
  `crates/swapdesk-session/src/viewer.rs` `open`), then `grant_wait` 25 s plus a 5 s guard. Only then does the hub
  reply `SessionId` and spawn `pump_viewer`.
- `service/viewer.rs` `open` pushes `State { session_id: 0, Connecting }` and, through `cfg.on_prompt`,
  `State { session_id: 0, Prompted { remaining_s } }` into the session's out channel. That channel is handed to the
  hub only after the grant, and `pump_viewer` drops every state with `session_id == 0`
  ("Before the host grants there is no session id to show"). So nothing reaches the interface while it waits.
- The host sends `PromptPending { timeout_s: 20 }` once (`crates/swapdesk-session/src/host.rs` `decide`, the prompt
  timeout is `HostConfig::prompt_timeout` = 20 s). The viewer's actor calls `on_prompt(20)` once; any countdown is
  the interface's own (the `tick` in `crates/swapdesk-ui/src/state.rs` already counts `Prompted` down).
- The interface's own deadline: `Command::timeout_s` (`crates/swapdesk-ui/src/api.rs`) gives `Connect` 20 s. The
  host's prompt starts after the handshake, so the interface fails first with "Sem resposta." while the card is
  still open on the host; if the host's user accepts in that last second, the hub's late `SessionId` reply finds no
  pending ticket and is ignored, but `pump_viewer`'s `Active` notification still creates the session: the viewer
  window opens after the interface said "no response".
- `UiState::connect_flow`, `connect_texts` and `dismiss_connect` (`state.rs`), `ConnectPhase`, and the keys
  `ConnectingTitle`, `PromptedTitle`, `EndedTitle` (`strings.rs`) have no caller in `crates/swapdesk-paint` or the
  shells.
- On the `SessionId` reply the interface pushes a `SessionView` in `Connecting`, and `viewer_session()` counts
  `Connecting` as a viewer session: the viewer window opens on that reply, with the bar text `ViewerConnecting`
  ("conectando…"), and becomes `Active` a moment later (the `Active` state is already queued by then).
- The refusal codes are folded before they reach the interface: `views.rs` `api_of` maps `DECLINED` to `Refused`
  and `NOBODY_PRESENT` to `NotPermitted`; a failed QUIC handshake becomes `OpenError::Lost`, then `ApiCode::Timeout`.
  The strip then says "Recusado.", "Isso não foi liberado." or "Sem resposta.", never who refused or why.
- `Connect` is not idempotent (`Command::is_idempotent`), and the buttons stay: a second press sends a second
  `Connect`, which the host refuses with `BUSY` (one question per device, `host.rs` `decide`), so "Ocupado, tente de
  novo." appears while the first request is still waiting.

### 1.2 The waking pattern this design reuses (verified by reading)

`Wake` replies at once with an entry id (`WAKE_ID_BIT | fresh_id`, `ui_hub/extras.rs` `wake`); progress comes as
`Notify::Session { Waking { remaining_s } }`; the device page draws `waking_card` (`crates/swapdesk-paint/src/
screens_main.rs`) in place of the device's content, hides `Assistir`, `Controlar`, `Renomear` and the tabs, and
offers `Cancelar` (`ControlId::WakeCancel`, which sends `Disconnect { session_id: <entry> }`, handled by
`Hub::cancel_wake`). Mock scenario `EstadoAcordando` captures it. Two defects of that pattern matter here:

- After the device appears, `run_wake` calls `Hub::connect` on a thread and waits for the host's user (up to 20 s)
  while the card still says "Acordando", its bar already full.
- `cancel_wake` aborts the async task but not the thread that runs `connect`: a cancel during that phase still
  opens a session a few seconds later.

### 1.3 The table

Columns: where the wait starts (command and code path); how long it can last; what the interface shows today;
whether it can be cancelled; what a timeout or a refusal shows; the gap. "Both" means Windows and macOS: both
systems run the same hub (`swapdesk-host`, in a service on Windows, in process on the Mac) and the same painter.

| # | Wait (role, system) | Starts | Lasts | Shown today | Cancel | Timeout or refusal shows | Gap |
|---|---|---|---|---|---|---|---|
| 1 | Connect, prompted (viewer, both) | `Command::Connect` → `Hub::connect` → `connect_asking` (blocking) | handshake ≤ 10 s + host prompt 20 s; worst case about 50 s (10 + 10 + 25 + 5) | nothing; buttons stay enabled; the viewer window opens only on the grant | no | strip "Recusado." / "Isso não foi liberado." / "Sem resposta." / "Ocupado, tente de novo."; the interface's 20 s can fire before the host's 20 s | **all of it**: no wait shown, no peer named, no countdown, no cancel, refusal reasons folded, race at 20 s, a second press makes `BUSY` |
| 2 | Connect, unattended (viewer, both) | same path, `unattended: true` | handshake only, under 1 s on a LAN | nothing | no | as row 1 | the interface never sends `unattended: true` from a button (only `interact.rs` `unattended: false` exists; see 1.4). When it is sent, the same design covers it with no `Prompted` phase |
| 3 | Wake, then connect (viewer, both) | `Command::Wake` → `Hub::wake` → `run_wake` → thread → `Hub::connect` | up to 60 s wake + row 1 | the waking card; during the connect phase it still says "Acordando", bar full | yes during the wake; a cancel during the connect phase is lost (session opens anyway) | `WakeGaveUp` strip after 60 s; a connect refusal ends the entry `Ended { reason }` with the folded code | the connect phase is invisible inside "Acordando"; cancel race |
| 4 | Incoming request card (host, both) | `HostUi::prompt` → `Notify::IncomingRequest` → dialog `incoming` | 20 s | modal card, "Recusa automática em N s.", `Recusar` / `Só deixar ver` / `Aceitar` | the person answers | the card closes at 0 (`IncomingClosed { Timeout }`); a viewer that gave up closes it silently (`Refused`) | none for the host's user, who waits on nobody. The viewer's side of the same 20 s is row 1 |
| 5 | Pairing, mode open (both sides, both) | `PairOpen` → `PairingState::Open` | 120 s | card with the countdown number and bar, `Cancelar` | yes | `Failed { Timeout }` card with `Tentar de novo` | none |
| 6 | Pairing, peer announced | `HubPairUi::peer_announced` → `PairingState::Waiting` | until the code shows (seconds) | "Esperando o outro computador", body, `Cancelar` | yes | `Failed` card | none (no deadline worth showing) |
| 7 | Pairing, this side confirmed, the other side not yet | `PairConfirm` → `pair_confirming = true` until `Done` / `Failed` | up to the 120 s comparison window (PROTOCOL 3, GUESS value there) | the code card with both buttons greyed (a 55 % surface veil); no text | no control offered | `Failed` card | no words say it waits for the other computer; no way out |
| 8 | Swap asked by this side as viewer (viewer, both) | `TbSwap` → `SwapPhase::Requested` → `RequestSwap` | asker gives up at 22 s | the toolbar's `Inverter` disabled, nothing else | no (C2.4: "There is no cancel op") | the "não aceitou inverter" card in the viewer | no words, no countdown |
| 9 | Swap asked by this side as host (host, both) | `BannerSwap` → same | 22 s | the banner's `Inverter` disabled | no | the error strip of the main window with the reason | same as row 8 |
| 10 | Swap asked of this side (either role, both) | `Notify::Swap { Asked }` → dialog `SwapAsk` | 20 s | modal, "Recusa automática em N s", `Recusar` / `Aceitar e controlar` | the person answers | closes | none |
| 11 | Terminal, the host's user is asked (D13, viewer, both) | `TermOpen` → host `request_consent(Terminal)` sends `ConsentPending { timeout_s: 20 }` | 20 s + shell start; interface limit 25 s | "Abrindo o terminal…" | no | the panel's own line (`TermFailDeclined`, `TermFailTimedOut`, `TermFailNobody`) | the viewer side drops `ConsentPending` (no handler in `swapdesk-session` or the hub): it never says that a person is being asked, nor the time left |
| 12 | Files, the host's user is asked (D13, viewer, both) | `FilesList` (first listing) → host `request_consent(Files)` | 20 s; interface limit **10 s** (`timeout_s`) | placeholder rows after 100 ms | no | strip "Sem resposta." at 10 s while the host's card still runs | the 10 s limit is shorter than the 20 s question; nothing says a person is asked |
| 13 | Consent card (host, both) | `HostUi::consent` → dialog `Consent` | 20 s | modal with countdown, `Recusar` / `Permitir` | the person answers | closes | none |
| 14 | Quick action (viewer, both) | `RunAction` → `Hub::run_action` | service 25 s (`ACTION_WAIT`), interface 30 s | nothing; the button stays enabled | no | strip "{0} não executou a ação." | nothing says it waits; a second press sends a second action |
| 15 | Administrator confirmation (either role, both) | `Elevate` → elevated window / system prompt | 120 s | modal, "Esperando a confirmação do sistema…" with the spinner arc, `Cancelar` | yes (closes Swapdesk's step; the system's own prompt is the system's) | `ElevFailed` card with the reason | none: the system's prompt is in front and owns its time |
| 16 | Chat send (either role, both) | `ChatSend` → `chat_send_task` | a local write; under 100 ms on a LAN (GUESS, not measured) | nothing until the reply adds the bubble | no | strip with the code | none while it stays under 100 ms |
| 17 | File transfer starting (viewer, both) | `FilesSend` / `FilesGet` → first `Notify::Transfer` | short; consent as row 12 on an unattended session | the row appears in `Queued` | yes, per transfer | the row's `Failed` / `Refused` line | none beyond row 12 |
| 18 | Add by address (both) | `AddByAddress` → `Hub::add_by_address` | local check, no network | nothing (fast) | n/a | field line `AddressRefusedWhy` | none |
| 19 | Forget, rename (both) | `ForgetDevice`, `RenameDevice` → trust store write | a local file write | nothing (fast); rename keeps the field open | n/a | rename: inline line; forget: strip | none |
| 20 | Settings and grants writes (both) | `SetSettings`, `SetGrants` (R-UI-2 optimistic) | a local write; admin grants wait for the service | the switch moves at once; admin switches are disabled while they wait (`grant_waiting`) | n/a | the switch reverts and the strip says why | none |
| 21 | Reconnecting after a lost link (viewer, both) | `Notify::Session { Reconnecting { attempt } }` | 60 s (`ReconnectPolicy`) | the viewer window's card "Reconectando ao X", "Tentativa N", `Desconectar` | yes | strip `ReconnectGaveUp` | no time left shown |
| 22 | Reconnecting, host side (host, both) | `HostEvent::PeerOffline` → `Session { Reconnecting { attempt: 0 } }` | 60 s | **likely wrong** (read, not run): `host_session()` matches only `Active` and `viewer_session()` matches any `Reconnecting`, so the banner closes and a viewer window opens on the host | — | — | role lost while reconnecting; must be proven by a state test first |
| 23 | Service down or starting (Windows) | `swapdesk-app/src/connect.rs`: one try of the pipe at launch, else `DownBackend` | until the next launch ("There is no reconnection") | placeholders, then strips "O serviço do Swapdesk não está respondendo." | no | the strip | a UI started before the service (sign-in) never recovers; a pipe break mid-run is final |
| 24 | Service starting (macOS) | in process (`swapdesk-app-mac/src/service.rs`) | starts with the app | n/a | n/a | n/a | none |
| 25 | macOS permission requests (host, macOS) | `ask_for_grants` at start (`swapdesk-app-mac/src/host.rs`); the shared notice `MacPerms` | until the person acts in System Settings (unbounded) | the system's own prompts; the shared `MacPerms` notice exists only in the mock (no producer found in `swapdesk-host` or `swapdesk-app-mac`), and `RequestOsPermissions` is answered `Internal` by `ServiceBackend` (`Route::Unsupported`) | n/a | nothing | the wait for System Settings is invisible; the notice is not wired. Larger than a waiting fix: its own stage |
| 26 | macOS login item approval (macOS) | notice `Service` → `OpenSystemSettings { LoginItems }` | unbounded | the notice card | n/a | n/a | whether the notice clears when the person returns is not proven (GUESS: it clears on the next `NetworkChanged`) |

### 1.4 Findings outside the waiting scope (read, not run)

- **Unattended entry is never asked for by the interface.** `interact.rs` sends `Connect { unattended: false }`
  always; the host grants without a prompt only when `Hello.requested` holds `UNATTENDED`
  (`swapdesk-session/src/policy.rs` `unattended_grant`). So "Entrar sem perguntar" stored at the host still prompts,
  and with nobody at the host the connect is refused `NOBODY_PRESENT`. The elevation path for
  `Connect { unattended: true }` exists in `state.rs` but nothing starts it. To be confirmed by a test and decided
  separately; this note does not change it. It does change one text: the "nobody present" end card must not tell the
  user to grant "Entrar sem perguntar" while that cannot work (section 5).
- Row 22 above (role lost while reconnecting on the host side).
- Row 25 above (the macOS permission notice is not wired in the real app).

### 1.5 Gaps ordered by how much they hurt

1. Connect (rows 1 and 3): the most used action shows nothing for up to 20 s, cannot be cancelled, reports a
   refusal without its reason, races its own timeout against the host's, and a second press makes `BUSY`.
2. Wake then connect (row 3): the cancel is lost during the connect phase and opens a session the user cancelled.
3. Files consent (row 12): the interface fails at 10 s while the host's user still has 10 s to answer.
4. Host-side reconnecting (row 22, if the test confirms it): a viewer window opens on the controlled machine.
5. Terminal consent (row 11): 20 s of "Abrindo o terminal…" while a person is being asked.
6. Windows service not up at launch (row 23): no recovery without a relaunch.
7. Swap asking (rows 8, 9): a disabled button is the only sign for 22 s.
8. Pairing after "Os códigos são iguais" (row 7): greyed buttons, no words.
9. Quick actions (row 14): no sign for up to 25 s; a second press sends a second action.
10. Reconnecting (row 21): no time left.
11. macOS permissions (row 25): the wait is invisible; the notice is not wired (own stage).

---

## 2. Rule R-UI-13 for `docs/SPEC.md`

Appended after R-UI-12, in its style:

- **R-UI-13 Visible waits:** every action that waits on the other machine, on a person or on the system shows,
  within 100 ms of the click, what it waits for and on whom ("Esperando o MacBook aceitar"), in the place the
  action was taken (the device's page for a connection, the panel for a terminal, the card for pairing); it shows
  the time left whenever the wait has a deadline, counted by the interface from the deadline the service reported;
  it offers `Cancelar` whenever the wait can be stopped, and stopping it undoes the request at the other end too;
  the action that started it cannot be sent again while it waits; and it ends in the result or in a failure that
  names who refused or what did not answer, never a generic code. The interface's own timeout of a waiting command
  is longer than the service's, so the service's answer is the one shown (R-UI-7). The wait is a polite live region
  for screen readers, which hear its start, its change of phase and its end, never each second.

---

## 3. The connect wait

### 3.1 Where it shows (recommendation first)

1. **Recommended: the device's page, in place of the device's content, as the waking card does.** The click
   happened there; the waking card is already built and captured on both systems (scenario `EstadoAcordando`); it keeps the device's
   name and detail line in view; it blocks nothing else (another device's page, Ajustes, an incoming request); and
   wake then connect becomes one card that changes phase in place.
2. The page header, in place of the buttons: too little room for the peer's name, the countdown, `Cancelar` and a
   refusal's reason; the reason would need the error strip again.
3. A modal dialog: blocks the whole window for up to 20 s, stacks with the incoming-request and consent dialogs,
   and reads like the host's card, which is the other side's.
4. Opening the viewer window at once with a waiting state inside: a window (and on the Mac, a focus change) for a
   request that may be refused, closed again on refusal.

### 3.2 Service side

**Reply at once.** `UiOp::Connect` replies `SessionId(entry_id)` as soon as the request is accepted for work, with
`entry_id = ENTRY_ID_BIT | fresh_id()`. `ENTRY_ID_BIT` is today's `WAKE_ID_BIT` (bit 63) renamed: wake and connect
entries share one id space and one map. Early refusals stay replies, as today: `NotPermitted` (not the console's
UI), `NeedsAdmin` (unattended from a connection that is not an administrator's), `Invalid`, `NotFound` (unknown
device), `ServiceDown`.

**The entry map.** `Hub.wakes` becomes `Hub.entries: HashMap<u64, Entry>` with
`Entry { abort: AbortHandle, conn: ConnId, device_hex: String }`. `Hub::disconnect` checks it first, as it checks
`cancel_wake` today.

**The attempt runs on the runtime, not on a thread.** Add `ServiceCore::connect_asking_async` (the async twin of
`connect_asking`) and a `waiting: Option<WaitHook>` argument to `Connector::connect` and `service::viewer::open`,
where `WaitHook = Arc<dyn Fn(SessionState) + Send + Sync>`. `open` calls it with `Connecting` at its start and, from
the existing `cfg.on_prompt` hook, with `Prompted { remaining_s }`. The `State { session_id: 0, .. }` pushes into
the out channel and the `session_id == 0` skip in `pump_viewer` go away. The hub's hook sends
`Notify::Session { session_id: entry_id, device_id, state }` to the connection.

**Ends.**

- Granted: `pump_viewer` gets `entry: Option<u64>`. When it forwards the session's first `Active`, it then sends
  `Session { entry_id, Ended { reason: None } }`. Order matters: the session exists in the interface before the
  entry goes, so the viewer window opens before the card closes and nothing flashes in between.
- Not opened: `Session { entry_id, NotOpened { why } }`, a new `SessionState` variant with a new wire enum
  `ConnectWhy` (in `crates/swapdesk-ipc/src/types.rs`, appended tags; "UI and service come from one build",
  CONTRACTS C6). Mapping from `ConnectError`:

  | `ConnectError` | `ConnectWhy` |
  |---|---|
  | `Open(Refused(DECLINED))` | `Declined` |
  | `Open(Refused(TIMEOUT))` (the host's 20 s ran out) | `TimedOut` |
  | `Open(Refused(NOBODY_PRESENT))` | `NobodyPresent` |
  | `Open(Refused(BUSY))` | `Busy` |
  | `Open(Refused(UNKNOWN_DEVICE \| FORGOTTEN))` | `Forgotten` |
  | `Open(Refused(NOT_PERMITTED))` and other host codes | `NotPermitted` |
  | `Open(Lost)`, `Open(Timeout)`, `Refused(Refusal::*)` (LAN guard), no address (`address_of` is `None`) | `Unreachable` |
  | `Open(Refused(UNSUPPORTED_VERSION))`, `Open(Protocol)`, `Socket`, `UnknownDevice` here, `ServiceDown` | `Failed` |

  `Ended { reason }` keeps its meaning for real sessions; `NotOpened` is only ever an entry's last state.
- Cancelled here: `Disconnect { session_id: entry_id }` aborts the task and sends
  `Session { entry_id, Ended { reason: None } }` (as `cancel_wake` does). The UI connection closing aborts all its
  entries.

**What cancel does on the wire and at the host.** Aborting the task drops the `ViewerSession::open` future. Today
that leaves the spawned actor running until its own deadline, still holding the connection, so the host's card
would stay. Add a guard in `crates/swapdesk-session/src/viewer.rs` `open_io`: until the grant arrives, dropping
the future calls `ctx.bye(ErrorCode::NoError)`. The host's actor handles `Bye` in any phase
(`actor.rs`, `Message::Bye(b) => Some(Finish::peer_bye(..))`), its end aborts the decision task, `PromptGuard` drops
with `PromptEnd::Cancelled`, and `HubHostUi::prompt_closed` sends `IncomingClosed { reason: Refused }`: the host's
card closes. PROTOCOL 5.2 gains one sentence: "While it waits for `SessionGrant`, the viewer may end the attempt
with `Bye { NO_ERROR }`; the host closes its question and grants nothing." This is no wire change (the message and
its handling exist since 1.0; the implementer confirms that a 1.0 host closes the card, see test S1-T6).

**Wake then connect.** `run_wake`, after `WakeOutcome::Appeared`, runs the same connect task under the wake's
entry id (no thread): the entry goes `Waking` → `Connecting` → `Prompted` → its end. Its abort handle is the one
already in the map, so a cancel in any phase stops it and the guard above sends `Bye`.

**Timeouts.** The service owns the deadline: handshake 10 s, control stream 10 s, the host's 20 s question inside
the viewer's `grant_wait` (25 s plus a 5 s guard). The `Connect` reply itself now comes at once, so
`Command::timeout_s` drops `Connect` and `Wake` to the default 5 s. The interface keeps a safety net only for news
that never comes (a service that died): `CONNECT_GIVE_UP_S = 60` in `tokens.rs` `layout`, counted from the entry's
creation; past it the interface ends the entry itself as `NotOpened { Failed }`. 60 is above the service's worst
case of about 50 s (10 + 10 + 25 + 5; GUESS that no other step adds time, to be confirmed).

### 3.3 Interface state (`crates/swapdesk-ui`)

- **The entry is a `SessionView`**, as the waking entry is, so `connect_flow()` keeps reading `self.sessions`.
  `SessionView` gains `countdown_total_s: Option<u16>` (set from the first `Prompted` or `Waking` value, used for the
  bar) and `age_s: u16` (counted by `tick` for entries in `Connecting` or `Prompted`, for the 60 s net).
- **At the click** (`activate(ControlId::Control | ControlId::ViewOnly)`): if the device already has an entry,
  nothing is sent. Otherwise push `SessionView::new(0, device_id, Connecting)` (id 0 until the reply, like
  `SwapPhase::Requested { request_id: 0 }`), clear that device's `connect_end`, send `Connect`, and return
  `Effect::Focus(ControlId::ConnectCancel)`. The card is therefore on screen in the frame of the click.
- **`Reply::SessionId(id)` for `Connect`**: the entry with id 0 for that device takes `id`.
- **An error reply** for `Connect`: the entry goes; `NeedsAdmin` keeps today's elevation step; any other code sets
  `connect_end` with `ConnectWhy::Failed`, or `Unreachable` for `NotFound`.
- **`Session { entry, Connecting | Prompted }`**: updates the entry (`Prompted` sets `countdown_total_s` once).
- **`Session { entry, NotOpened { why } }`**: the entry goes and `connect_end = Some(ConnectEnd { device_id, why })`.
  No error strip: the card says it (`apply_session` stops setting `self.error` for entries).
- **`Session { entry, Ended { None } }`**: the entry goes, nothing else.
- **`viewer_session()`** no longer counts `Connecting` (the window opens on `Active` only).
- **`ConnectPhase::Ended`** becomes `Ended { why: ConnectWhy }` (the `in_error_line` flag goes). `connect_flow()`
  becomes `connect_flow(device_id: &str)`: the device page asks for its own device.
- **`connect_end`** stays until `Fechar`, `Tentar de novo`, a new connect to that device, or a `select_device` of
  another device.
- **`rename_available`** is false while the device has an entry or an end card (as while waking).
- **`needs_tick`** already counts `Prompted` and `Waking`; it also counts `Connecting` entries (for the 60 s net).

### 3.4 The screen

The card generalizes `waking_card` into `wait_card(f, a, y, d, card: WaitCard)` in `screens_main.rs`; the waking
card becomes one of its uses, so the two cannot drift apart. Metrics are the waking card's:

- Column: `w = (a.cw - 40) / 3`, at `(a.x, y)` where `y` is the line below the header (the header's
  `y += (title_h + 6 + status.h).max(40) + 28`). `bw = p.hairline()`, `inner = w - 44 - 2 * bw`.
- Panel: `f.panel(Rect::new(a.x, y, w, h), pal.surface)`.
- Inner origin: `ix = a.x + bw + 22`, `iy = y + bw + 20`.
- Icon: `f.icon_circle(ix, iy, tint, ink, icon)`, 40 px; then `iy += 50`.
  Waiting: `pal.link_tint`, `pal.link`, a new `icons::CLOCK = "M21 12a9 9 0 1 1-18 0a9 9 0 1 1 18 0zM12 7v5l3 2"`
  (24-unit viewBox, stroke, like the others). Ended: `pal.danger_tint`, `pal.danger`, `icons::SHIELD_ALERT`.
  Waking keeps `icons::POWER`.
- Title: `text::H2_STATE`, `pal.ink`, `label_in(.., inner, Align::Start)`; then `iy += t.h + 10`.
- Body: `text::BODY`, `pal.soft`; then `iy += b.h + 10`.
- Countdown row (waiting phases only; reserved in `Connecting` so the card does not grow when `Prompted` arrives):
  height `row_h = max(6, caption.h)` where `caption = p.label("0:00", text::CAPTION)`. The time label
  `format::countdown(remaining_s)` in `text::CAPTION`, `pal.soft`, right-aligned at `ix + inner`, vertically centred
  on the row. The track `Rect::new(ix, iy + (row_h - 6) / 2, inner - caption.w - 8, 6)`, radius 3, `pal.line`; the
  fill from the left, width `track.w * elapsed / total`, `pal.link`. In `Connecting` only the track is drawn and no
  label. Then `iy += row_h + 10`.
  The waking card gets the same row (today it has the bar without the time, so it does not meet R-UI-13).
- Buttons, on the bottom line `by = y + h - bw - 20 - 40`, through `actions_row(f, ix, by, inner, list)` so the
  order rule holds by construction:
  - waiting: `[(ConnectCancel, "Cancelar", Kind::Default)]`, alone at the left (waking: `WakeCancel`, as today);
  - ended: `[(ConnectClose, "Fechar", Kind::Default), (ConnectRetry, "Tentar de novo", Kind::Primary)]`, `Fechar` at
    the left, `Tentar de novo` at the right.
- Height: `h = 2 * bw + 40 + 40 + 10 + t.h + 10 + b.h + 10 + [row_h + 10] + 40`, the row term present while
  waiting. This is the waking card's formula with `6` replaced by `row_h`.

Content per phase (`{0}` is the device's name as the sidebar shows it):

| Phase | Title | Body | Row | Buttons |
|---|---|---|---|---|
| `Connecting` | `ConnectingTitle` "Conectando ao {0}" | `ConnectingBody` (new) | track only | `Cancelar` |
| `Prompted` | `PromptedTitle` "Esperando o {0} aceitar" | `PromptedBody` (new) | bar and "0:14" | `Cancelar` |
| `Waking` | `WakingTitle` "Acordando o {0}" | `WakingBody` | bar and "0:36" (new) | `Cancelar` |
| `Ended { why }` | `ConnectEndedTitle` "Não deu para conectar ao {0}" (new) | the `why` line (section 5) | none | `Fechar`, `Tentar de novo` |

The rest of the page while a card shows (waiting or ended): the title and detail line stay; `Assistir`,
`Controlar`, `Acordar` and `Renomear` are not drawn; the tabs and the tab content are not drawn (as while waking).
After `Fechar` the page is the normal page again; after `Tentar de novo` the card goes to `Connecting` at once with
the same intent (the entry remembers `intent`).

The sidebar row of the device: its status line (`format::status_line`) reads `SidebarConnecting` "Conectando…"
during `Connecting`, `SidebarWaitingAccept` "Esperando aceitar" during `Prompted`, and `SidebarWaking` "Acordando…"
during `Waking`; the dot and the colours do not change. That keeps the wait visible from Início and Ajustes.

End states, one by one:

| End | What the user sees |
|---|---|
| Accepted | the viewer window opens (`Active`); the card goes when the entry ends, after it; the page is normal again |
| Declined | ended card, `SwapWhyDeclined` "O {0} recusou." |
| No answer (host's 20 s) | ended card, `ConnectWhyTimedOut` "O {0} não respondeu em 20 segundos." |
| Busy | ended card, `ConnectWhyBusy` |
| Nobody present | ended card, `SwapWhyNobody` "Não há ninguém no {0} agora." |
| Forgotten by the other side | ended card, `ConnectWhyForgotten` |
| Not permitted | ended card, `ConnectWhyNotPermitted` |
| Unreachable | ended card, `ConnectWhyUnreachable` |
| Failed (incl. the interface's 60 s net) | ended card, `ErrInternal` "Erro interno." |
| Cancelled here | no card: the page is normal again at once (a normal end, as the wake's cancel) |

### 3.5 Keyboard and screen readers

- **Focus.** The click's control disappears, so focus moves to `Cancelar` (`Effect::Focus(ControlId::ConnectCancel)`,
  a new effect both shells apply with their existing focus calls: `window.rs` `focus_control` on Windows, the
  canvas's focus on the Mac). On a failure, focus moves to `Fechar` (never to `Tentar de novo`, so a stray Enter
  does not resend). On success the viewer window takes focus as it does today. On cancel, focus returns to
  `Controlar` or `Assistir`, whichever started it.
- **Esc** activates `Cancelar` while a card waits and `Fechar` on an ended card (main window only, not in a dialog).
- **Tab order** is the card's buttons in reading order (the painter's widget order), as for every card.
- **Live region.** A new `Region::Status` in `interact.rs`, mapped in `a11y.rs` to `Role::Status` with
  `Live::Polite`. It holds the card's heading and body: a screen reader hears "Conectando ao MacBook", then
  "Esperando o MacBook aceitar. O MacBook mostrou o pedido…", then the end line. The countdown is a separate
  `UiRole::Progress` node outside the live region, whose label is `ConnectExpiresIn` "O pedido expira em {0} s"
  and whose value is the seconds; it is read when focused or browsed, never announced each second.
  Windows: accesskit's `live` property becomes UI Automation's `LiveSetting` (GUESS: `accesskit_windows` raises
  `LiveRegionChanged` when the node's text changes; verify, else raise it from the shell). macOS: the canvas
  (`crates/swapdesk-shell-mac/src/canvas/ax.rs`) maps `Role::Status` to `AXApplicationStatus` but posts no
  announcement; `ax_sync` posts `NSAccessibilityAnnouncementRequestedNotification` with the live region's text when
  it changed since the last sync.

### 3.6 Mock scenarios (both systems capture them)

Added to `Scenario` in `crates/swapdesk-ui/src/mock.rs`, to `Scenario::ALL` (33 → 37 with these four; 40 with the
three of section 4), and to the expected table of `every_scenario_drives_the_state_to_its_screen`
(`Screen::Devices`, windows `[Main]`):

| Scenario | `--mock` name | Script after the common world |
|---|---|---|
| `EstadoConectando` | `estado-conectando` | `Select(MAC)`, `Activate(ControlId::Control)` (`apply_to` calls `activate` with no backend, so the local entry stays in `Connecting` with id 0) |
| `EstadoAguardandoAceite` | `estado-aguardando-aceite` | `Select(MAC)`, `Event(Session { session_id: ENTRY, device_id: MAC, state: Prompted { remaining_s: 20 } })`, `Ticks(6)` (shows "0:14") |
| `EstadoConexaoRecusada` | `estado-conexao-recusada` | `Select(MAC)`, the `Prompted` event above, then `Event(Session { ENTRY, MAC, NotOpened { why: Declined } })` |
| `EstadoConexaoNinguem` | `estado-conexao-ninguem` | as above with `NobodyPresent` (the longer line checks wrapping) |

`ENTRY` is a constant of the mock with bit 63 set, like a real entry id. The mock backend's `Command::Connect`
(used by live `--mock` runs, not by `apply_to`) changes to the new flow: reply `SessionId(ENTRY)` at once,
`Connecting` at 10 ms, `Prompted { 20 }` at 300 ms, then the session's `Active` under its own id and the entry's
`Ended { None }`. A `--mock <name> --shot` capture on the Mac and the Windows renderer's PNGs give the same four
screens.

---

## 4. The other gaps: smallest fix in the same pattern

| Row | Fix | Why this and not more |
|---|---|---|
| 3 | Covered by 3.2: one entry from wake to connect, the abort covers the connect, `Bye` on drop | |
| 7 | Pairing after "Os códigos são iguais": while `pair_confirming`, the `PairWarn` body is replaced by `PairWaitingBody` ("O outro lado precisa confirmar o código…") and the button row by `[(PairCancel, "Cancelar", Default)]`. A test proves `PairCancel` after `PairConfirm` ends the ceremony on both sides as `Failed` | existing keys; no countdown because the 120 s comparison window is a GUESS value in PROTOCOL 3 and the service sends no deadline |
| 8 | Viewer asked to swap: a card in the viewer window in the same place and style as `swap_declined_card`, title `SwapWaitingTitle` "Esperando o {0} aceitar a inversão", body `SwapWaitingBody`, the countdown row counting down from 22, no button. New mock scenario `InverterAguardando` (`inverter-aguardando`) | there is no cancel op (C2.4) and the swap library cannot cancel an outgoing request; a cancel is its own change, listed in section 7 |
| 9 | Host asked to swap: the banner's status text reads `SwapWaitingTitle` while `swap_requested()` | the banner has no room for a card |
| 11 | Terminal consent: the viewer actor turns `ConsentPending` into `ViewerEvent::ConsentPending { feature, timeout_s }`; `service/viewer.rs` forwards it as `ViewerOut::Consent`; `pump_viewer` sends a new `Notify::ConsentPending { session_id, feature, remaining_s }`; the state keeps `consent_wait: Option<(Feature, u16)>` counted by `tick`. The terminal panel shows `TermConsentWaiting` and the countdown row instead of "Abrindo o terminal…". New mock scenario `TerminalAguardandoConsentimento` (`terminal-aguardando-consentimento`) | not cancellable: there is no message that withdraws a pending `TermOpen`; closing the panel leaves the host's 20 s to run out, which the host's user sees as the card closing |
| 12 | Files consent: the same notification; the files panel shows `FilesConsentWaiting` and the countdown row above the placeholder rows. `Command::timeout_s` for `FilesList` goes from 10 to 25 (as `TermOpen`) | the 25 s covers the 20 s question plus the listing |
| 14 | Quick actions: while a `RunAction` for that session is pending, every quick-action button is disabled and a line `ActionWaiting` "Esperando o {0} executar a ação…" shows under them | one action at a time; success needs no line (its effect shows on the remote screen, or the session ends) |
| 21 | Reconnecting: the card gets the countdown row, counted by the interface from 60 (`RECONNECT_WINDOW_S`, the `ReconnectPolicy` window) from the first `Reconnecting { attempt ≥ 1 }` | the service sends attempts, not a deadline; the window is fixed |
| 22 | Host-side reconnecting: `SessionView` keeps the role of its last `Active`; `viewer_session()` and `host_session()` match `Reconnecting` and `Paused` by that role; the banner shows "Reconectando…" (existing `ReconnectingTitle`) while its session reconnects. First a state test that proves the defect (S5-T5) | if the test does not confirm it, nothing changes |
| 23 | Windows service not up: `swapdesk-app` starts with a backend that queues commands and retries the pipe every second for 30 s at launch (GUESS values), then becomes `ServiceBackend` or `DownBackend`; a pipe break mid-run gets the same 30 s of retries before `ServiceDown`. The interface shows the existing placeholders meanwhile and the existing strip after | shell-side only; no new text |
| 25 | macOS permissions: its own stage, not part of this work: wire the `MacPerms` notice from the host's real grant state, re-check on app activation, and have the notice say `MacPermsWaiting` after its button was pressed | the notice is not wired at all today; fixing only its wait would decorate a screen nobody sees |
| 4, 5, 6, 10, 13, 15–20, 24, 26 | no change | already meet R-UI-13, or finish under 100 ms, or the system's own dialog is in front |

---

## 5. Texts (proposals for the owner)

Existing keys reused, unchanged: `ConnectingTitle` "Conectando ao {0}" / "Connecting to {0}", `PromptedTitle`
"Esperando o {0} aceitar" / "Waiting for {0} to accept", `WakingTitle`, `WakingBody`, `Cancel` "Cancelar" /
"Cancel", `Close` "Fechar" / "Close", `PairRetry` "Tentar de novo" / "Try again", `SwapWhyDeclined` "O {0}
recusou." / "{0} declined.", `SwapWhyNobody` "Não há ninguém no {0} agora." / "Nobody is at {0} right now.",
`ErrInternal` "Erro interno." / "Internal error.", `PairWaitingBody`, `ReconnectingTitle`.

`EndedTitle` stays unused. It cannot be removed (a key travels as its index), so it stays as a tombstone with a
comment, as an exception to R-UI-12.

New keys, appended at the end of the `strings!` table after `ElevConfirm`, in this order:

| Key | pt-BR | en |
|---|---|---|
| `ConnectingBody` | Abrindo a conexão pela rede local. | Opening the connection over the local network. |
| `PromptedBody` | O {0} está mostrando o seu pedido. Quem estiver nele precisa aceitar e escolher o que você pode fazer. | {0} is showing your request. Whoever is at it has to accept and choose what you may do. |
| `ConnectExpiresIn` | O pedido expira em {0} s | The request expires in {0} s |
| `ConnectEndedTitle` | Não deu para conectar ao {0} | Could not connect to {0} |
| `ConnectWhyTimedOut` | O {0} não respondeu em 20 segundos. | {0} did not answer within 20 seconds. |
| `ConnectWhyBusy` | O {0} já está em outra sessão. Tente de novo quando ela terminar. | {0} is already in another session. Try again when it ends. |
| `ConnectWhyForgotten` | O {0} não reconhece mais este computador. Pareie os dois de novo. | {0} no longer knows this computer. Pair the two again. |
| `ConnectWhyNotPermitted` | O {0} não permite esta conexão. | {0} does not allow this connection. |
| `ConnectWhyUnreachable` | O {0} não respondeu pela rede. Confira se ele está ligado e nesta mesma rede. | {0} did not answer over the network. Check that it is on and on this same network. |
| `SidebarConnecting` | Conectando… | Connecting… |
| `SidebarWaitingAccept` | Esperando aceitar | Waiting to be accepted |
| `SidebarWaking` | Acordando… | Waking… |
| `SwapWaitingTitle` | Esperando o {0} aceitar a inversão | Waiting for {0} to accept the swap |
| `SwapWaitingBody` | Se ninguém responder em 20 segundos, você continua controlando o {0}. | If nobody answers within 20 seconds, you keep controlling {0}. |
| `TermConsentWaiting` | Esperando quem usa o {0} permitir o terminal. | Waiting for the person using {0} to allow the terminal. |
| `FilesConsentWaiting` | Esperando quem usa o {0} permitir os arquivos. | Waiting for the person using {0} to allow the files. |
| `ActionWaiting` | Esperando o {0} executar a ação… | Waiting for {0} to run the action… |

Decisions the owner makes on these texts:

- Every pt-BR and en line above.
- The "nobody present" line: the plain `SwapWhyNobody` is proposed. A hint such as "Para entrar sem alguém aceitar,
  quem usa o {0} precisa liberar “Entrar sem perguntar” para este computador." is left out while the interface
  never asks for unattended entry (1.4): the hint would promise something the button cannot do.
- One title for every refusal (`ConnectEndedTitle`) and the reason in the body, or a title per reason ("O {0}
  recusou"). One title is proposed: fewer keys, and the reason is the body's first words.
- `SwapWaitingBody` says 20 seconds because that is the asked side's countdown; the asker gives up at 22 s.

---

## 6. Build plan

Thin slices, in this order. Each one is proven before the next starts. `crates/swapdesk-paint` is edited by the main
session only. Every new text key goes at the end of the strings table (a key travels between the interface and the
service as its index). "Red before" names the tests that fail on today's code.

### S1. Service: the connect entry (`swapdesk-ipc`, `swapdesk-session`, `swapdesk-host`)

Files: `crates/swapdesk-ipc/src/types.rs` (`ConnectWhy`, `SessionState::NotOpened`); `crates/swapdesk-session/src/
viewer.rs` (the bye-on-drop guard in `open_io`); `crates/swapdesk-host/src/service/mod.rs` (`connect_asking_async`,
the `waiting` hook through `Connector::connect`); `crates/swapdesk-host/src/service/viewer.rs` (`open` calls the
hook, the `session_id: 0` pushes go); `crates/swapdesk-host/src/ui_hub/ops.rs` (`connect` as a task with an entry,
`pump_viewer(entry)`, the `ConnectWhy` mapping, `disconnect` checks the entries); `crates/swapdesk-host/src/ui_hub/
extras.rs` (`wakes` becomes `entries`, `run_wake` connects as a task under the wake's id);
`crates/swapdesk-host/src/ui_hub/mod.rs` (the map, aborting a closing connection's entries);
`crates/swapdesk-ui/src/service_backend/convert.rs` (the new state both ways).

Tests (all red before, except T7):

- T1 `swapdesk-host` `ui_hub/tests.rs` `connect_replies_an_entry_at_once_then_connecting_and_prompted_while_the_host_asks`
- T2 `ui_hub/tests.rs` `an_accepted_connect_sends_the_sessions_active_before_the_entry_ends`
- T3 `ui_hub/tests.rs` `each_refusal_ends_the_entry_with_its_own_why` (declined, timed out, nobody present, busy,
  forgotten, unreachable)
- T4 `ui_hub/tests.rs` `cancelling_a_connect_entry_closes_the_hosts_card_and_opens_no_session`
- T5 `ui_hub/tests.rs` `cancelling_during_the_connect_phase_of_a_wake_opens_no_session`
- T6 `swapdesk-session` `tests/flows.rs` `a_viewer_dropped_while_prompted_says_bye_and_the_host_prompt_ends_cancelled`
- T7 `swapdesk-ipc` wire round trip of `NotOpened` for every `ConnectWhy` (in its existing wire-sample tests)
- The existing hub tests that read the session id from the `Connect` reply move to the first `Active`
  notification; they stay green.

### S2. Interface state (`swapdesk-ui`)

Files: `crates/swapdesk-ui/src/state.rs` (entry at the click, id from the reply, `NotOpened`, `viewer_session`,
`connect_flow(device_id)`, `ConnectPhase::Ended { why }`, the 60 s net, `countdown_total_s`); `api.rs`
(`ConnectWhy`, `SessionState::NotOpened`, `Effect::Focus`, `timeout_s` for `Connect`, `Wake`, `FilesList`);
`interact.rs` (card controls `ConnectCancel`, `ConnectClose`, `ConnectRetry`, `Region::Status`, the sidebar status,
`rename_available`, Esc); `a11y.rs` (`Region::Status` as a polite live region, the progress node); `strings.rs`
(the keys of section 5); `tokens.rs` (`CONNECT_GIVE_UP_S`); `mock.rs` (the new `Connect` flow, four scenarios).

Tests:

- Red before: `state_tests.rs` `controlar_shows_the_connect_card_at_once_and_sends_one_connect`,
  `a_second_press_while_waiting_sends_nothing`, `the_entry_counts_the_hosts_seconds_down`,
  `no_viewer_window_opens_while_the_entry_waits`, `an_accepted_connect_opens_the_viewer_and_the_card_goes`,
  `each_why_reads_its_own_line` (one line per `ConnectWhy`, all different), `cancel_sends_disconnect_for_the_entry`,
  `the_interface_gives_up_after_60_s_without_news`, `tentar_de_novo_sends_the_same_intent_again`,
  `the_sidebar_row_says_connecting_and_waiting`; `a11y_tests.rs`
  `the_wait_card_is_a_polite_live_region_and_the_countdown_is_not`.
- Changed to the new behaviour: `the_connect_flow_keeps_connecting_prompted_and_the_ended_reason_until_dismissed`,
  the test around `state_tests.rs` line 92 ("The SessionId reply opens a connecting session, and the viewer window
  exists"), `mock.rs` `r_ui_7_a_refused_admin_prompt_or_a_slow_connect_have_their_own_limits` (Connect 20 → 5),
  `every_scenario_drives_the_state_to_its_screen` (four rows added).

### S3. Painter (`swapdesk-paint`, main session)

Files: `crates/swapdesk-paint/src/screens_main.rs` (`wait_card` with the countdown row, the waking card through
it, the device page hiding its buttons, tabs and rename while a card shows, the sidebar status line);
`crates/swapdesk-paint/src/view.rs` (`icons::CLOCK`).

Tests: `crates/swapdesk-paint/src/tests.rs` `every_scenario_paints_through_the_trait_and_places_every_control`
(covers the new scenarios; red until S3 lands because S2's controls have no place), new
`the_connect_card_has_the_waking_cards_column_and_padding`, new
`cancel_sits_at_the_left_alone_and_retry_at_the_right_of_fechar`; `crates/swapdesk-shell-win/src/tests.rs`
`every_scenario_renders_to_a_png_and_every_control_has_a_place` and
`every_screen_has_a_complete_accessibility_tree_with_bounds` (the four PNGs and their trees).

### S4. Shells (`swapdesk-shell-win`, `swapdesk-shell-mac`)

Files: `crates/swapdesk-shell-win/src/window.rs` (apply `Effect::Focus`; raise `LiveRegionChanged` if accesskit
does not); `crates/swapdesk-shell-mac/src/canvas.rs` and `canvas/ax.rs` (apply `Effect::Focus`; post
`NSAccessibilityAnnouncementRequestedNotification` for a changed live region).

Proof: on the Mac, the window checks (`window_checks` example) and the accessibility comparison of every live
window against the Windows tree; the Mac captures of the four scenarios with `--mock <name> --shot`. Then one real
run across the two machines: the Mac presses `Controlar`, the card shows "Esperando o … aceitar" with the countdown,
`Cancelar` closes the card on the Windows side; once more with `Aceitar`, and once more with `Recusar`.

### S5. The other gaps (one slice each, in the order of 1.5)

- Files and terminal consent (rows 11, 12): `swapdesk-session` (viewer event), `service/viewer.rs`, `ui_hub/ops.rs`
  (`Notify::ConsentPending`), `swapdesk-ipc`, `state.rs`, `screens_viewer.rs` and `screens_files.rs`. Red before:
  `ui_hub/tests.rs` `a_consent_question_reaches_the_viewers_ui_with_its_seconds`; `state_tests.rs`
  `the_terminal_panel_says_who_is_asked_and_counts_down`, `a_files_listing_waits_25_s`.
- Host-side reconnecting (row 22): red before (if the defect is real): `state_tests.rs`
  `a_hosted_session_that_reconnects_keeps_the_banner_and_opens_no_viewer`.
- Windows service retry (row 23): `crates/swapdesk-app/src/connect.rs` and `main.rs`; test in that crate
  `a_service_that_comes_up_within_30_s_is_connected` (with a fake pipe opener).
- Swap waiting (rows 8, 9): `state.rs`, `interact.rs`, `screens_viewer.rs`, scenario `InverterAguardando`. Red
  before: `state_tests.rs` `asking_to_swap_shows_who_is_asked_and_counts_22_s`.
- Pairing confirming (row 7): `interact.rs`, `screens_main.rs`, scenario `PareamentoConfirmando`
  (`pareamento-confirmando`). Red before: `state_tests.rs` `after_the_codes_match_the_card_says_it_waits_and_offers_cancel`;
  `ui_hub/tests.rs` `pair_cancel_after_confirm_ends_the_ceremony`.
- Quick actions (row 14): `state.rs`, `interact.rs`, `screens_viewer.rs`. Red before: `state_tests.rs`
  `a_pending_action_disables_the_actions_and_says_so`.
- Reconnecting countdown (row 21): `state.rs`, `screens_viewer.rs`. Red before: `state_tests.rs`
  `reconnecting_counts_the_minute_down`.

### S6. Documents

`docs/SPEC.md` (R-UI-13 after R-UI-12), `docs/CONTRACTS.md` C2.4 (the `Connect` reply, the entry, `NotOpened`,
`ConnectWhy`, `ConsentPending`), `docs/PROTOCOL.md` 5.2 (the `Bye` sentence), `docs/DECISIONS.md` (why the wait
lives on the device's page), `docs/FLUXOS.md` (the connect flow's steps and proofs; the map's check is rerun for
every flow whose cited files changed), and this note's status line.

---

## 7. Options rejected

- A modal dialog for the connect wait: blocks the window for 20 s and stacks with the host-side dialogs.
- The wait in the page header only: no room for the reason of a refusal, which would need the error strip again.
- Opening the viewer window at once with a waiting state: a window for a request that may be refused.
- Keeping the blocking `Connect` and only forwarding `Prompted` as notifications under a temporary id: the reply
  still comes 20 s later, cancel still has nothing to stop, and the 20 s race stays.
- Raising the interface's `Connect` timeout to 50 s instead of answering at once: no cancel, no phase.
- New `ApiCode` variants for the refusal reasons: `ApiCode` is shared by every command, and remapping `DECLINED`
  would change the terminal's and the swap's failure lines; `ConnectWhy` touches the connect only.
- `Ended { reason }` with a detail message for the reasons: every reader of `Ended` would have to tell an entry from
  a session.
- Making the entry id the future session id: the host picks the session id at the grant, after the wait.
- A cancel for an outgoing swap now: the swap library has no cancel; its own change if wanted.
- Announcing every second of the countdown to screen readers: noise; the progress node holds the value.
- A countdown in `COUNTDOWN` type (28 px) as on the pairing card: too heavy for a third-of-the-page card; the
  caption-size time beside the bar keeps the waking card's look.
- Disabling `Controlar` and `Assistir` instead of hiding them: the waking card already hides them, and two
  patterns for one kind of wait would drift.
- A wait line in the sidebar row in the link colour: draws the eye away from the card; the status text alone is
  enough there.
