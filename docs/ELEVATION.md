# Elevation: admin-only actions from an unelevated interface

Status: design note for stage 56, first written 2026-11-10, updated the same day with the owner's decisions and with `ACCOUNTS.md` (D44, sections 8, 9 and 17.1). Part of it is built (section 7 marks each slice). It covers the service side, which both systems share, and the Windows client. The macOS client is designed in `ELEVATION-MAC.md`; section 4 lists what the service offers it.

**Decided by the owner, 2026-11-10.**
1. The elevated instance shows its own confirmation window with the action and "Confirmar" (slice 6).
2. A failed step shows a reason line. `ElevNotAdmin`, `ElevTimedOut`, `ElevConfirmBody` and `ElevConfirm` were approved as first written. Decision 4 makes `ElevNotAdmin`'s wording wrong, so a replacement is proposed in 3.7.
3. On the Mac, the "Terminal de administrador" switch stays on the Permissões page, off and inert, with the line "Indisponível no macOS nesta versão". That belongs to `ELEVATION-MAC.md` and is only mentioned here.
4. **Over the shoulder is accepted.** A standard account may be granted `UNATTENDED` when an administrator approves at the UAC prompt in that account's session. The approver is an elevated administrator in the same Windows session, not necessarily the same user. Approval goes over a separate elevator pipe that admits administrators, and the audit entry records the approver.
5. **Unattended entry enters with the user's own reach**, and a button in the session elevates it (`ACCOUNTS.md` 9). Before `Elevate`, and before the administrator terminal outside an elevated session, the viewer's machine asks for the system's confirmation with this note's ticket and pass. Plain unattended entry asks nothing. The connect-time action (`ConnectUnattended`) is gone.

## 0. The defect, as read in the code

- The interface opens its elevation step ("Confirmação de administrador") when a reply is `ApiCode::NeedsAdmin` (`crates/swapdesk-ui/src/state.rs`). "Continuar" pushes `Command::Elevate { action }` (`interact.rs`).
- Before this work, `ServiceBackend` answered `Command::Elevate` as unsupported, so the step hung on "Waiting". No code started an elevated process.
- The service's checks were right but unreachable. `ConnInfo.admin` is true only for an elevated token with Administrators enabled (`swapdesk-sys-win/src/pipe.rs` `identify_client`), so no unelevated interface could pass:
  - `ui_hub/ops.rs` (`SetGrants`, and `Connect { unattended }`, which `ACCOUNTS.md` S6 removes);
  - `ui_hub/terminal.rs` (`TermOpen { admin }`).
- The incoming card's "Deixar entrar sem perguntar" is a `SetGrants` that adds `UNATTENDED` (`interact.rs`), so it failed the same way.

**Where it stands now:**
- Ops 35 to 38, the ticket, the pass and the interface's worker are built for `SetGrants` (section 7).
- Nothing on Windows can confirm a ticket yet. There is no `--elevated` instance, and the pipe server does not accept role `Elevator`.
- The pipe server sets `ConnInfo.can_elevate` to false, so the built `ElevationBegin` refuses every ticket from the installed service until slice 3 or `ACCOUNTS.md` S7.

## 1. Decision in brief

**The mechanism: a one-use ticket and a one-use pass.**
1. The unelevated interface asks the service for a ticket bound to its connection and to the exact action. The ticket lives 120 s.
2. It starts `Swapdesk.exe --elevated <ticket>` with the `runas` verb.
3. The elevated instance opens the session's elevator pipe and asks the service what the ticket is for.
4. It shows that action in its own high-integrity window, and on "Confirmar" it confirms the ticket.
5. The service puts a pass on the asking connection: single use, 30 s, bound to the exact action, and carrying the approver.
6. The interface gets `ElevationResult { Ok }` and sends its original command again. The command finds the pass and runs through its existing code path.

**The actions** (`ElevAction`, `ACCOUNTS.md` 17.1):

| Tag | Action | Machine | Guards | Its command after the pass |
|---|---|---|---|---|
| 0 | `SetGrants { device_id, grants }` | host side (D7) | adding `UNATTENDED` or `ADMIN_TERMINAL`, or widening a device that holds `UNATTENDED` | `SetGrants` (op 9) |
| 1 | retired, never assigned again | | `ConnectUnattended` is gone: plain unattended entry asks nothing | |
| 2 | `OpenAdminTerminal { session_id }` | viewer side (`ACCOUNTS.md` 8.3) | the administrator terminal in a session this machine did not elevate | `TermOpen { admin: true }` (op 32) |
| 3 | `ElevateSession { session_id }` | viewer side (`ACCOUNTS.md` 8.3) | `Elevate` toward the host (protocol minor 1) | `ElevateSession` (op 50, `ACCOUNTS.md` 9.3) |

**Who may approve.** Any elevated connection, meaning its token is elevated with Administrators enabled, **in the same Windows session as the ticket**. It may be the asking user (consent prompt) or another administrator account typed in over the shoulder (credentials prompt). Same-user approval may arrive on `ui-<session>`, as `ACCOUNTS.md` 5.6 keeps. The instance itself always uses the new `elev-<session>` pipe, which admits administrators and serves role `Elevator` only. The approver of a `SetGrants` is written as `GrantsChanged.by`.

**Since D61 (11/11) there is no confirmation window**: the elevated instance approves the action the service holds, and the person confirms it in the step before UAC. **Since D62** a confirmed `SetGrants` leaves a 10-minute grace on the asking connection for that device: a later ticket for the same device's permissions from that connection is answered already confirmed (`ttl_s` 0) and no instance is started. What follows describes the window as it was built (owner decision 1, reversed by D61). The UAC prompt names only Swapdesk.exe. A same-user process can swap the action at the very moment the user expects a prompt, by injecting into the interface or by starting its own ticket. The window shows the action as the service holds it, and code at medium integrity cannot click it (UIPI, section 6).

**An elevated session opens the administrator terminal without a second pass.** A viewer session that this machine elevated through a confirmed `ElevateSession` is marked `elevated`. The confirmation already approved SYSTEM-level reach on that peer for the session, and the session controls the host's UAC and sign-in screen (D11).

## 2. The mechanism end to end

Actors:
- **UI**: account A's `Swapdesk.exe`, medium integrity, connection A on `ui-<session>`.
- **Hub**: the service's hub of account A (`ACCOUNTS.md` 7.1: one per account).
- **Instance**: `Swapdesk.exe --elevated`, high integrity, connection E with role `Elevator` on `elev-<session>`. It runs as A (consent) or as administrator Z (credentials).

### 2.1 `SetGrants` from Permissões, A an administrator (consent prompt)

```
UI (conn A, medium)                     Hub of account A                          Instance (conn E, high)
1  SetGrants{dev, g} ----------------->  g widens unattended reach, A not admin, no pass
                     <-----------------  NeedsAdmin
   step opens (Ask); "Continuar"
2  worker: ElevationBegin{SetGrants{dev, g}} ->
                                         A is the console's UI; dev paired; g valid (PROTOCOL 6.1)
                                         ticket t (16 random bytes), bound to (A, session, action), 120 s;
                                         replaces any open ticket of A
                     <-----------------  ElevationTicket{t, ttl_s: 120}
3  ShellExecuteExW("runas", <verified own exe>, "--elevated <t as 32 hex>")
   [UAC consent prompt on the secure desktop]
                                                                                   4  open elev-<session>; server PID = SCM's;
                                                                                      Hello{role: Elevator}
                                         pipe checks: client in this session; token elevated with
                                         Administrators enabled -> serve_elevator(E)
                                         <---------------------------------------  ElevationDescribe{t}
                                         t open; E admin; E.session = t.session
                                         --------------------------------------->  ElevationAction{action, peer_name, remaining_s}
                                                                                   5  window: action line + ElevConfirmBody
                                                                                      [Cancelar] [Confirmar]; "Confirmar"
                                         <---------------------------------------  ElevationConfirm{t, approve: true}
                                         t -> Confirmed; pass{A, action, approver: E.user, 30 s}
                                         --------------------------------------->  Done; exit code 0
6  worker: process ended; ElevationEnd{t} -> Confirmed; record removed
   UiEvent::ElevationResult{action, Ok(())}; reply Done to the Elevate ticket
7  state: step closes; followup SetGrants{dev, g}
   SetGrants{dev, g} ----------------->  conn.admin OR take_pass(A, SetGrants{dev, g}) -> approver Z' (= A here)
                                         A's store written; audit GrantsChanged{by: approver};
                                         unattended_granted_by = A; DevicesChanged
                     <-----------------  Done
```

### 2.2 The same, A a standard account (over the shoulder)

Steps 1 and 2 are identical. A standard account gets a ticket; nothing checks the account type at Begin. Then:
- At step 3, UAC shows the credentials prompt. An administrator Z types Z's name and password.
- The instance runs as Z in A's Windows session. It opens `elev-<session>`, which admits Administrators; `ui-<session>` would refuse Z, since its DACL lists A only.
- The service builds E's `ConnInfo` from Z's token: user = Z's SID and Z's account name, read from that SID and not from the session's user.
- Describe and Confirm check only "elevated" and "same Windows session". The pass carries Z.
- At step 7 the grant lands in **A's** store, with `GrantsChanged.by` = Z and `unattended_granted_by` = A (`ACCOUNTS.md` 8.2).
- If nobody types valid credentials, UAC ends as declined (`ERROR_CANCELLED`, UNPROVEN, section 6) and nothing changes.

### 2.3 The incoming card's "Deixar entrar sem perguntar"

- The card answers `AnswerIncoming` first: a prompted session, never with `UNATTENDED` (D8).
- It then sends `SetGrants { device, grants_here + UNATTENDED }`. The reply is `NeedsAdmin`, and the step opens with `ElevActGrantUnattended`.
- From there it is 2.1, or 2.2 for a standard account. The pass binds the exact bit set the card computed.

### 2.4 `ElevateSession`: the button in an unattended session (viewer side)

```
UI A   in a session entered unattended toward a minor-1 host, unelevated (SessionState::Active.elevation = 1),
       the person presses the elevate button (texts: ACCOUNTS.md 19.2)
       ElevateSession{s} (op 50) -> Hub: conn.admin OR take_pass(A, ElevateSession{s}); none -> NeedsAdmin
       step (ElevActElevateSession) -> 2.1 steps 2 to 6 with action ElevateSession{s}
       (Begin checks: s is a viewer session of A, entered unattended, host minor >= 1, not yet elevated)
       followup ElevateSession{s}
Hub    pass consumed -> sends Elevate (0x000C) to the host
Host   PermissionsChanged{granted ∪ UNATTENDED ∪ (ADMIN_TERMINAL if stored and requested)}, audit Elevated
Hub    on that PermissionsChanged: Viewer[s].elevated = true; reply Done to op 50
       (host Error NOT_PERMITTED -> NotPermitted; minor-0 host -> Invalid; 10 s -> Timeout: ACCOUNTS.md 9.3)
```

The session stays A's throughout: its media, input, files and terminals.

### 2.5 `OpenAdminTerminal`

```
UI A   TermOpen{s, admin: true}
Hub    allowed when: A.admin, OR Viewer[s].elevated, OR take_pass(A, OpenAdminTerminal{s});
       otherwise NeedsAdmin -> step (ElevActAdminTerminalOpen) -> 2.1 steps 2 to 6
       (Begin checks: s is a viewer session of A whose grants hold ADMIN_TERMINAL)
       -> followup TermOpen{s, admin: true, 80, 24} -> pass consumed -> term_open as today
```

After `ACCOUNTS.md` S6, a session holds `ADMIN_TERMINAL` normally only after `Elevate`, and is then `elevated`, so no pass is needed. The pass serves the remaining cases:
- a session that holds the bit without this machine's elevation, for example a minor-0 host that granted it at entry (`ACCOUNTS.md` 9.3, a disagreeing `HelloAck`);
- a prompted grant that holds it.

### 2.6 Failure sequences

- **UAC "No"**, or the credentials prompt closed. `ShellExecuteExW` returns FALSE with `ERROR_CANCELLED`. `ElevationEnd` answers `Open` and closes t. The result is `Refused`.
- **Approved by an elevation without Administrators enabled.** This happens under a policy, or an unusual token. `elev-<session>` refuses at open: exit code 4, `NotPermitted`. If the open succeeded, the service answers `NeedsAdmin`: exit code 6, `NotPermitted`.
- **An approver in another Windows session** cannot see the ticket: the pipe server and the hub both compare sessions. Exit code 4, `NotPermitted`.
- **"Cancelar" in the instance's window.** `approve: false`, exit code 1, outcome `Declined`, result `Refused`.
- **Too slow.** The ticket expires 120 s after Begin, and a late Confirm gets `NotFound`: exit code 5. The worker stops waiting 110 s after Begin, sends End, and the result is `Timeout`. If the UI's own 120 s run out first, the failed `Elevate` reply becomes `Failed(Timeout)` in the step. That part is built.
- **The service went away.** The result is `ServiceDown` (exit code 3 from the instance).
- **The asking connection closed.** Its ticket and passes go at once, and a later Describe or Confirm gets `NotFound` (exit code 5).
- **"Cancelar" on the step while Waiting.** The step closes and nothing more is sent. A pass confirmed afterwards sits unused for 30 s and expires. It is usable only by A, and only for the action that was just approved.

## 3. Exact additions

Each item is marked **built** or **to build**.

### 3.1 `swapdesk-ipc`

- **Built:**
  - `ElevationTicket([u8; 16])`: 16 raw bytes on the wire, and a `Debug` that prints no byte.
  - `ElevationOutcome`: 0 `Confirmed`, 1 `Declined`, 2 `Open`, 3 `Unknown`.
  - `IpcRole::Elevator = 4`.
  - `ElevAction` tag 0 `SetGrants { device_id: str(64), grants: u16 }`.
  - Ops 35 to 38, and the `ReplyBody` variants `ElevationTicket { ticket, ttl_s }`, `ElevationAction { action, peer_name: str(64), remaining_s }` and `ElevationOutcome`. Op 37 replies `Done`.
- **To build** (inside `ACCOUNTS.md` S6, slice 7 here): `ElevAction` tag 2 `OpenAdminTerminal { session_id: u64 }` and tag 3 `ElevateSession { session_id: u64 }`. Tag 1, and every tag from 4 up, stays a codec error. Op 50 `ElevateSession` and `SessionState::Active.elevation` belong to `ACCOUNTS.md` 14.3.

| Op | Name and body | Sent by | Reply body (ok) | Errors |
|---|---|---|---|---|
| 35 | `ElevationBegin { action }` | role UI, the console's connection | `ElevationTicket { ticket, ttl_s }` (120) | `NotPermitted`: not the console's UI. `Invalid`: malformed device id or grants; a session that cannot be elevated, or does not hold `ADMIN_TERMINAL`. `NotFound`: unknown device, or not a viewer session of this connection. `Busy`: 16 tickets open. |
| 36 | `ElevationDescribe { ticket }` | role Elevator | `ElevationAction { action, peer_name, remaining_s }` | `NeedsAdmin`: E not elevated with Administrators enabled. `NotPermitted`: E in another Windows session. `NotFound`: not open, or the asker is gone. |
| 37 | `ElevationConfirm { ticket, approve }` | role Elevator | `Done` | as op 36 |
| 38 | `ElevationEnd { ticket }` | role UI, the asking connection | `ElevationOutcome` | none: another connection's ticket answers `Unknown` |

The built `ElevationBegin` also refuses `NotPermitted` when `can_elevate` is false. `ACCOUNTS.md` S7 removes that refusal (3.2).

### 3.2 Service: `swapdesk-host` `ui_hub` (`elevation.rs`, `ops.rs`, `terminal.rs`)

**Built, for tag 0:**
- The ticket table: one ticket per connection (a new Begin replaces it), at most 16 in all, 120 s, swept on every elevation op and pass lookup, and never logged. The trace names the action and the device.
- The pass list on each `Conn`: at most four; a fifth pushes the oldest out. Each pass lives 30 s, is single use, and matches the exact action.
- `take_pass`; the cleanup when a connection ends; `set_elevation_ttls` for tests.
- `serve_elevator` and `serve_elevator_cancellable`. They take a reader and writer already past the handshake. The loop is separate: no notifications, not a console connection, one Describe and one Confirm, and an end after Confirm, on `Bye`, at the end of the pipe or after 125 s.
- The public `UiHub::describe_elevation` and `UiHub::confirm_elevation`.
- `set_grants` consumes a matching pass when the change needs one.
- A UI connection asking ops 36 or 37 gets `NotPermitted`.

**To change in `ACCOUNTS.md` S7 (over the shoulder):**
- **Approver rule.** `with_ticket` requires `approver.admin` and `approver.session_id == ticket.session`. The comparison of the approver's user with the asker's goes.
- **Begin.** The `can_elevate` refusal goes, so a standard account gets a ticket. After this, nothing in the hub reads `ConnInfo.can_elevate`. Recommended: remove the field at its six construction sites. The interface learns "consent or credentials" from its own token (3.4).
- **The pass carries the approver.** `Pass { action, approver: UserRef, expires }`. `take_pass` returns `Option<UserRef>`. `set_grants` writes `GrantsChanged.by` = the approver, else the connection's user, and keeps `unattended_granted_by` = the asking connection's user.

**To build in `ACCOUNTS.md` S6 (slice 7 here):**
- `checked_action` for tag 2: a viewer session of this connection whose grants hold `ADMIN_TERMINAL`.
- `checked_action` for tag 3: a viewer session of this connection, entered unattended, toward a host of minor 1 or newer, not yet elevated.
- `peer_name` is the session's device name.
- `Viewer.elevated: bool` is set when the host's `PermissionsChanged` with `UNATTENDED` answers op 50.
- `term_open`: `admin && !(conn.admin || viewer.elevated || take_pass(OpenAdminTerminal{s}))` is `NeedsAdmin`.
- Op 50: `conn.admin || take_pass(ElevateSession{s})`, else `NeedsAdmin`.
- The `Connect { unattended }` check in `request_other` goes. The `ADMIN_TERMINAL` request follows `ACCOUNTS.md` 9.3: every connection asks for it toward a host known to be minor 1 or newer. The rule of the first version of this note, based on `can_elevate`, is superseded.

### 3.3 Pipes: `swapdesk-service` `ui_pipe.rs`, `swapdesk-sys-win` `pipe.rs`

**To build in slice 3 (same user):**
- `ui-<session>` accepts `Hello.role` `Elevator` with the same session, user and identity checks, and hands it to `serve_elevator_cancellable`.
- `ConnInfo.can_elevate` is filled from a new `ClientIdentity.administrators_present`: the Administrators SID in the token, enabled or deny-only. Until S7 removes the Begin refusal, this is what lets the installed service give tickets to administrators.

**To build in `ACCOUNTS.md` S7 (over the shoulder):**
- **The pipe.** New pipe `elev-<session id>` (`elev-<session>-<n>` under `--instance`), created by the service beside `ui-<session>`:
  - DACL: SYSTEM full, Administrators read/write (the `0x12019B` row, without `FILE_CREATE_PIPE_INSTANCE`);
  - `FILE_FLAG_FIRST_PIPE_INSTANCE`, with the C2.1 squat rule (if the create fails: alert, no pipe);
  - `PIPE_REJECT_REMOTE_CLIENTS`;
  - role `Elevator` only, anything else gets `Bye(NotPermitted)`.
- **Checks on `elev-<session>`:**
  - the client process runs in that session;
  - `identify_client` says elevated with Administrators enabled, else `Bye(NotPermitted)`, exit code 4;
  - `ConnInfo.user` = the token's SID and that SID's account name (not `session_user_name`);
  - then `serve_elevator_cancellable` on the hub of the account whose console session it is. A ticket lives in the asker's hub, and the session's interface is the asker's.
- A filtered token holds Administrators deny-only, which an allow ACE ignores (section 6). So an unelevated process of A cannot open `elev-<session>`.
- **Development mode.** The development service creates `elev-<session>-<n>` with the current user in SYSTEM's row plus Administrators. An elevated instance of the developer opens it, so the UAC hand check can run without an install. GUESS: a medium-integrity development service can read an elevated client's identification token (`TOKEN_QUERY`, no-write-up default).

### 3.4 Interface, shared by both systems (`swapdesk-ui`)

**Built:**
- The `Elevator` trait and `ElevatorRun`; `ServiceBackend::set_elevator`.
- The worker of `Command::Elevate`: Begin, `run` until 110 s, always End, then `ElevationResult` and `Done`.
- The outcome mapping of 3.6, where the service's outcome decides and the exit code only says why.
- The `SetGrants` followup.
- A failed `Elevate` reply goes into the step, not the strip.
- The reason line by code (`elevation_failure_line`).
- `elevated_client::run` (the instance's side: Hello as `Elevator`, Describe, ask, Confirm) and its `ElevatedExit` codes.
- The four approved texts, appended at the end of the strings table.

**To change in slice 7 (inside `ACCOUNTS.md` S6):**
- C6 `ElevatedAction` loses `ConnectUnattended` and gains `ElevateSession { session_id }`.
- `state.rs`:
  - `NeedsAdmin` on `Command::ElevateSession { session_id }` opens the step with `ElevateSession`;
  - `Connect { unattended: true }` no longer opens it;
  - a confirmed `ElevateSession` follows up with `Command::ElevateSession`;
  - a confirmed `OpenAdminTerminal` follows up with `begin_term(admin: true)` on that session, if it is still the viewer's.
- `service_backend/elevation.rs` `wire_action` maps all three.
- The `MsgKey` slot of `ElevActConnectUnattended` is kept in place, because keys travel as their index. It becomes `ElevActElevateSession` with the owner's text (3.7).

**To change in `ACCOUNTS.md` S7:**
- The step's body before "Continuar" says what Windows will ask: consent for an administrator account, an administrator's password for a standard one (3.7).
- The Windows shell reads its own token once at start, with the `administrators_present` test of 3.3, and sets it on `UiState`.

### 3.5 Windows client (to build: slices 5 and 6)

**Command line:**

```
Swapdesk.exe --elevated <ticket: exactly 32 lowercase hex digits>
             [--service-pid <pid>] [--instance <n>]      (development builds only, passed through from the asking UI)
```

- `cli.rs` accepts `--elevated` in every build. Combined with `--mock`, or given twice, it is exit code 2, as for any wrong argument.
- `main.rs`: with `--elevated` the program runs only the instance. It starts no viewer pipeline, reads no prefs file, does no autostart and sends no `startup_commands`. If the C2.1 single-instance mutex is built, the instance skips it.

**The instance:**
1. It opens `elev-<session>` with the server-PID check against the SCM (`--service-pid` in development). Until S7 builds that pipe it opens `ui-<session>`, which serves the same-user case only.
2. It runs `elevated_client::run` with `confirm` = the window (slice 6), then exits with the code below.
3. The window speaks the instance's own Windows display language. After over-the-shoulder approval that is Z's language, and Z is the one reading.

**Exit codes**, built in `ElevatedExit`; 2 is the command line's:

| Code | Meaning |
|---|---|
| 0 | Confirmed. |
| 1 | "Cancelar" pressed or the window closed; `approve: false` was sent. |
| 2 | Wrong command line. |
| 3 | The service cannot be reached. |
| 4 | The pipe or the service refused this process: access denied at open, `Bye(NotPermitted)`, or `NotPermitted`. |
| 5 | The ticket is unknown, used or expired, or the window ran out of `remaining_s`. |
| 6 | The service says this process is not an administrator (`NeedsAdmin`). |
| 7 | Internal. |

**The launcher.** `swapdesk-sys-win`, new `elevate.rs`; the `unsafe` stays in the platform crate.
- `run_as_admin(exe, params, owner) -> Result<ElevatedProcess, RunAsError>`.
- On its own thread: `CoInitializeEx(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)`, then `ShellExecuteExW` with:
  - `lpVerb = "runas"`;
  - parameters quoted with `launch.rs` `quote_arg`;
  - `fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI`;
  - `hwnd` = the main window, else null;
  - `nShow = SW_SHOWNORMAL`.
- `ERROR_CANCELLED` is `RunAsError::Declined`.
- `ElevatedProcess::wait(timeout)` uses `WaitForSingleObject`, then `GetExitCodeProcess`. A NULL `hProcess` gives `Ended { exit_code: None }`.

**`WinElevator` (`swapdesk-app`, new `elevate.rs`).**
- The exe is `std::env::current_exe()`, checked with `InstallPolicy::production().verify`: under `%ProgramFiles%`, with the file and its folder writable only by SYSTEM, Administrators and TrustedInstaller.
- A failed check is `Failed(Internal)` with a trace line. The check is skipped only in a development build started with `--service-pid`.
- The launcher is injected, so tests replace it.

**The window (slice 6).**
- A small top-level window that paints the elevation dialog layout of `swapdesk-paint`:
  - `ElevTitle`;
  - the action line, as the service described it;
  - `ElevConfirmBody`;
  - "Cancelar" on the left and "Confirmar" on the right;
  - no default button, so a stray Enter confirms nothing;
  - accessible names.
- It acts only on real clicks and keys.
- When `remaining_s` runs out it closes as declined (exit code 5).
- On the Mac the system's administrator dialog carries the text instead (`ELEVATION-MAC.md`).

### 3.6 Each failure, its `ApiCode` in `ElevationResult`, and its line (built mapping)

`ElevationEnd` decides first: `Confirmed` gives `Ok`, `Declined` gives `Refused`. Only `Open` or `Unknown` fall through to the run:

| Where it ended | `ElevatorRun` | Result | Reason line |
|---|---|---|---|
| Begin: not the console's UI (and, until S7, an account without Administrators) | none | `NotPermitted` | `ElevNotAdmin` (new wording proposed, 3.7) |
| Begin: unknown device or session | none | `NotFound` | `ErrNotFound` |
| Begin: 16 tickets open | none | `Busy` | `ErrBusy` |
| Begin or End: service gone | none | `ServiceDown` | `ErrServiceDown` |
| UAC declined, or credentials not given | `Declined` | `Refused` | none today; `ElevDeclined` proposed (3.7) |
| Window "Cancelar" | `Ended{1}`, End = `Declined` | `Refused` | as above |
| Approval without an administrator, or from another session | `Ended{4}` or `Ended{6}` | `NotPermitted` | `ElevNotAdmin` |
| Service unreachable from the instance | `Ended{3}` | `ServiceDown` | `ErrServiceDown` |
| Ticket expired or used; worker deadline | `Ended{5}`, `TimedOut`, or End = `Unknown` | `Timeout` | `ElevTimedOut` |
| Exe not in a protected folder, launch error, exit 2 or 7, unreadable exit code | `Failed(Internal)` or `Ended{_}` | `Internal` | `ErrInternal` |

"Tentar de novo" sends `Elevate` again, which opens a new ticket and replaces the old one.

### 3.7 Texts

**Approved by the owner (2026-11-10), built:**

| Key | pt-BR | en |
|---|---|---|
| `ElevTimedOut` | "O tempo para confirmar acabou." | "The time to confirm ran out." |
| `ElevConfirmBody` | "Confirme como administrador para aplicar esta mudança." | "Confirm as an administrator to apply this change." |
| `ElevConfirm` | "Confirmar" | "Confirm" |

**Approved by the owner (2026-11-11, D56) and built with S7:** `ElevNotAdmin` (new wording), `ElevDeclined`, `ElevBodyWindowsCredentials` (shown when the interface's own token is not an administrator account's). The table keeps the proposals as they were approved:

| Key | pt-BR | en | Why |
|---|---|---|---|
| `ElevNotAdmin`, new wording, same key | "A confirmação não veio de um administrador deste computador." | "The confirmation did not come from an administrator of this computer." | The built text sends the person to sign in with an administrator account. After decision 4 that is wrong: an administrator approves in this account's session. The case left is a prompt answered without an administrator, or from another session. |
| `ElevDeclined` (new, appended) | "A confirmação foi recusada." | "The confirmation was declined." | The reason line for `Refused` (UAC "No", credentials not given, window "Cancelar"). Today the step shows only "Nada foi alterado." |
| `ElevBodyWindowsCredentials` (new, appended) | "Esta mudança precisa de um administrador deste computador. O Windows vai pedir o nome e a senha de um administrador." | "This change needs an administrator of this computer. Windows will ask for an administrator's name and password." | The step's body for a standard account, which gets the credentials prompt. `ElevBodyWindows` stays for administrator accounts. |
| `ElevActElevateSession` (the slot of `ElevActConnectUnattended`) | "Elevar o controle em {0}" | "Elevate control on {0}" | The step's and the window's action line for tag 3. It follows the button wording the owner approved (D55). Built with this text (11/11); shown to the owner in the milestone's batch. |

`ElevActAdminTerminalOpen` ("Abrir o terminal de administrador") stays for tag 2.

### 3.8 Contract and document text that changes with the build

- **CONTRACTS C2.1**: the `Swapdesk.exe` row names `--elevated <ticket>`, started through UAC (`runas`) to confirm one action. A new pipe row `elev-<session>` (DACL SYSTEM plus Administrators, role `Elevator` only, approver in the same session).
- **C2.3** "Admin-only operations": accepted from an elevated connection, or from the asking connection through a single-use pass. The pass is made by an elevated administrator connection in the same Windows session, which confirmed that connection's ticket in a window that shows the action. The approver is recorded for grant changes. Elevation tickets join the never-logged list.
- **C2.4**: ops 35 to 38, `ElevAction` tags 0, 2 and 3, role `Elevator`, and op 50 (`ACCOUNTS.md`). The terminal section and the "Unattended entry" line follow `ACCOUNTS.md` 9.3: the old connect-time administrator check goes.
- **C6**: `ElevatedAction` is `SetGrants`, `OpenAdminTerminal` and `ElevateSession`. `Command::Elevate` runs in the backend with the platform's `Elevator`.
- **THREAT-MODEL**: the rows of section 5. The "same user" section takes `ACCOUNTS.md` 8.3.
- **SPEC R14** names this note. **FLUXOS**: F-PERM-01 and F-PERM-03 cite the elevation tests.

## 4. What the service offers a macOS client

Shared code in `swapdesk-host` and `swapdesk-ui`, callable as it is:

1. **Ops 35 and 38** on its own UI connection: Begin and End, with the rules of 3.1.
2. **In-process confirmation**, for the 1.x app, whose hub runs in the app:
   - `UiHub::describe_elevation(&ticket, &approver)` and `UiHub::confirm_elevation(&ticket, &approver, approve)`;
   - `approver` is a `ConnInfo` with `admin: true` and the ticket's session id;
   - after S7 the approver's user is recorded and need not equal the asker's. Where `AuthorizationCopyInfo` can name the administrator, that name goes in, else the console user (UNPROVEN, `ELEVATION-MAC.md`);
   - in process, the check guards against a person at the keyboard, not against code in the app's process. That limit is the Mac note's to state.
3. **`UiHub::serve_elevator` and `serve_elevator_cancellable`**, for a later root daemon whose elevated helper connects over a socket.
4. **The interface side:** `ServiceBackend::set_elevator`, the worker, the followups and the failure lines. The Mac supplies only `Elevator::run`.
5. **The actions:** tag 0 for Permissões, and tag 3 before `Elevate`. Tag 2 is not offered on the Mac in this version: the "Terminal de administrador" switch stays off and inert on Permissões with "Indisponível no macOS nesta versão" (owner decision 3, `ELEVATION-MAC.md`).

## 5. Threat table

| Attacker | What it can do | What stops it, or what remains |
|---|---|---|
| **Same-user process** (malware at medium integrity, on either machine's side) | Open `ui-<session>` as the user; Begin a ticket for any action (a grant here, `ElevateSession` or the administrator terminal toward a peer); start `runas` on Swapdesk.exe with its own ticket; inject into the unelevated interface and swap the action just before Begin. | **UAC:** each ticket needs a person's yes on the secure desktop. Microsoft classes UAC as defense in depth, not a boundary (section 6). **The window:** it shows the action as the service holds it, and medium-integrity code cannot inject input into it or drive it (UIPI; UIA without uiAccess). **The pass:** single use, 30 s, the exact action, on the asking connection only. **Remains (accepted, THREAT-MODEL "same user"):** after a real approval, it can use what was approved through the interface it controls, for example type into an elevated session or an open administrator terminal. It can use plain unattended entry, the peer's desktop as its user, which asks nothing by decision 5 (`ACCOUNTS.md` 8.3). It might cover the window with its own topmost window; whether Windows prevents that is UNPROVEN. On an administrator account it can reach admin by other means, outside any design here. |
| **Standard user at the keyboard** | Press "Continuar"; type an administrator's credentials, or not. | Without valid administrator credentials, UAC gives no elevated process and nothing changes. With them, an administrator approved, which is the intended path (decision 4). The approver is written in the audit entry of a grant. |
| **Over the shoulder, the administrator Z** | Approve A's ticket. | Z is an administrator, part of the trusted computing base (section 6). Z approves only the action A's interface asked for, as the window shows it, and the grant lands in A's store with Z recorded. Z cannot approve anything A did not ask for: a ticket exists only on A's connection (`ACCOUNTS.md` 13). |
| **Credential harvesting** | Malware imitates the credentials prompt to collect an administrator's password. | Not made by this design. Microsoft notes the risk of the "Prompt for credentials" setting (section 6, fact 4). The design adds no prompt of its own that asks for a password. Accepted. |
| **Another Windows session** (fast user switching) | Its interface and an elevated instance of its own run in another session. | Begin requires the console session's connection. Both pipes refuse a client of another session, and the hub requires the approver's session to equal the ticket's. |
| **Ticket theft or replay** | Read the ticket from the instance's command line (UNPROVEN whether a lower-integrity process can; assumed yes); present it again later. | A ticket is useful only to an elevated administrator in the same session, which is already trusted. Confirming it only creates the pass the asker requested, on the asker's connection. It is single use (Confirm or End closes it; a replay gets `NotFound`), expires after 120 s, dies with the asking connection, and is never logged. |
| **Race with a legitimate elevation** | While the user approves action A, Begin its own ticket for B and start its own `runas`, or swap A for B inside the interface. | Its own ticket gives a second UAC prompt, then a window naming B, and a pass from it lands only on its own connection. A swap inside the interface shows in the window. Each `runas` carries one ticket, and medium-integrity code cannot change the elevated process's command line. |
| **Forged elevated instance path** | Redirect the program the interface starts; start its own program with `runas` under a Swapdesk-like name. | The interface starts only its own image, after `InstallPolicy::verify`. If the user approves a foreign program at UAC, that program is an administrator, out of this threat model, so the approver's image path is not checked. |
| **Squatter on `elev-<session>` or `ui-<session>`** | Receive a ticket, or answer "Done" as the service. | The service creates both pipes as first instance and raises an alert when it cannot. The instance checks the server PID against the SCM before sending anything (C2.1). A fake "Done" or exit code 0 grants nothing: the pass exists only in the real service, and a resend without one gets `NeedsAdmin` again. |
| **An elevated process of another account in a session not its own** | Open `elev-<session>` of A's session. | The DACL admits administrators, but the pipe server refuses a client process of another session, and administrators are TCB anyway. |
| **The elevated instance as a target** | Feed it input while it runs at high integrity. | It parses only its command line (32 hex digits) and the service's replies (length-checked decoder, property-tested). It opens no file dialog, no network and no viewer. It loads from the protected install folder and exits after Confirm. |
| **Floods** | Begin in a loop; open many elevator connections. | One ticket per connection (replaced), 16 per machine (`Busy`), four passes per connection. An elevator connection serves two ops and ends within 125 s. |

## 6. Windows facts this relies on (read 2026-11-10)

1. **`runas` verb.** "Launches an application as Administrator. User Account Control (UAC) will prompt the user for consent to run the application elevated or enter the credentials of an administrator account used to run the application." `SEE_MASK_NOCLOSEPROCESS`: "the **hProcess** member receives the process handle ... In some cases ... no handle will be returned. The calling application is responsible for closing the handle." `hProcess` "will be **NULL** if no process was launched". `SEE_MASK_NOASYNC` "must be specified if the thread calling ShellExecuteEx does not have a message loop". `SEE_MASK_FLAG_NO_UI`: "Do not display user interface (UI) error dialogs ... Security prompts are exempted and will still be shown." `hwnd`: "the owner window, used to display and position any UI". Source: https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-shellexecuteinfow
2. **ShellExecuteExW.** "Returns **TRUE** if successful; otherwise, **FALSE**. Call GetLastError". Among its values: "ERROR_CANCELLED: The function prompted the user for additional information, but the user canceled the request." "COM should be initialized before **ShellExecuteEx** is called ... `CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)`". Source: https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw
   - The page does not name UAC as the prompt behind `ERROR_CANCELLED`. That a UAC "No", or a closed credentials prompt, yields exactly this code is UNPROVEN until the owner's hand check.
   - Also UNPROVEN: that the call returns only after the prompt is answered.
3. **Waiting and the exit code.** `SYNCHRONIZE` is "Required to wait for the process to terminate using the wait functions". `PROCESS_QUERY_LIMITED_INFORMATION` is "Required to retrieve certain information about a process (see GetExitCodeProcess ...)". Source: https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights
   - UNPROVEN: which rights the handle returned for an elevated process carries, in particular one that runs as another account. The design does not depend on it: `ElevationEnd` gives the outcome.
4. **The token of an elevation.** For an administrator, two tokens: the standard one has "the administrative Windows privileges and SIDs ... removed". For standard users, "the *credential prompt*": they "perform an administrative task by entering valid credentials for a local administrator account". "If the policy setting is set to **Prompt for credentials**, malware imitating the credential prompt might be able to gather the credentials from the user." Source: https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works
   - The Application Information service starts the program "by creating a new process for the application with an administrative user's full access token". Source: https://learn.microsoft.com/en-us/windows-server/security/user-account-control/how-user-account-control-works
   - Reading of both pages, which over-the-shoulder approval (2.2) relies on: with credentials typed over the shoulder, the process runs with that administrator account's token, in the session where the prompt was answered. Its user SID is therefore Z's, not the session user's. The pages do not state the SID or the session in so many words; the owner's hand check with a standard account confirms both.
5. **Filtered token, deny-only Administrators.** `SE_GROUP_USE_FOR_DENY_ONLY`: "When the system performs an access check, it checks for access-denied ACEs that apply to the SID; it ignores access-allowed ACEs for the SID. If this attribute is set, SE_GROUP_ENABLED is not set". Source: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-token_groups
   - `TokenElevationTypeDefault`: "The token does not have a linked token"; `Full`: "an elevated token"; `Limited`: "a limited token". Source: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_elevation_type
   - Basis of `administrators_present`, and of "an unelevated process cannot open `elev-<session>`" (its Administrators is deny-only).
6. **Integrity levels.** "Standard users receive medium, elevated users receive high." "By default, the system creates every object with an access mask of SYSTEM_MANDATORY_LABEL_NO_WRITE_UP." Source: https://learn.microsoft.com/en-us/windows/win32/secauthz/mandatory-integrity-control
7. **UIPI.** For SendInput: "This function is subject to UIPI. Applications are permitted to inject input only into applications that are at an equal or lesser integrity level." Source: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput
   - "UIPI is a security feature that prevents messages from being received from a lower-integrity-level sender", while "Certain messages whose value is smaller than WM_USER are required to be passed through the filter". Source: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-changewindowmessagefilterex
   - "An application that doesn't have UIAccess in the manifest starts with medium IL and cannot access elevated ("medium+" IL) process UI"; UIAccess requires an Authenticode signature and "a secure location", for example Program Files. Source: https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview
   - Which messages below `WM_USER` pass the filter is not listed on the page. The window therefore confirms only on a real click or key on "Confirmar": UNPROVEN set, handled by design.
8. **Command line of a higher-integrity process.** No Microsoft page read today says whether a medium-integrity process can read it. UNPROVEN, and the design assumes it can.
9. **UAC is not a security boundary.** Microsoft's servicing criteria list UAC among defense-in-depth features ("Intent to service?" No). "Administrative processes and users are considered part of the Trusted Computing Base (TCB) for Windows." Source: https://www.microsoft.com/en-us/msrc/windows-security-servicing-criteria

## 7. Build plan

"Red before" means the test fails, or does not compile against the new API, on the tree before the slice. Every proof is a loopback or in-process test. The UAC click is the owner's hand check.

**Built (tag 0, `SetGrants`):**
- **Slice 1, ipc.** Ops 35 to 38, the ticket, `ElevationOutcome`, role `Elevator`, tag 0; `tests/roundtrip.rs`.
- **Slice 2, the hub.** `ui_hub/elevation.rs` and `ops.rs`. Tests in `ui_hub/tests/elevation.rs`:
  - `a_confirmed_set_grants_ticket_lets_the_asking_connection_grant_unattended_once`
  - `the_pass_is_bound_to_the_exact_action_and_to_the_asking_connection`
  - `a_ticket_is_single_use_and_ends_after_its_ttl`
  - `the_pass_expires_after_its_ttl`
  - `a_connection_that_is_not_elevated_can_neither_describe_nor_confirm`
  - `an_approver_of_another_user_or_session_is_refused`
  - `an_account_that_is_not_an_administrator_gets_no_ticket`
  - `a_ui_of_another_session_gets_no_ticket`
  - `a_ticket_is_refused_for_a_change_the_command_would_refuse`
  - `declining_leaves_no_pass_and_end_reports_each_outcome`
  - `closing_the_asking_connection_drops_its_ticket_and_passes`
  - `a_new_begin_replaces_the_connections_open_ticket`
  - `the_sixteenth_open_ticket_is_the_last`
  - `the_trace_names_the_action_and_the_device_never_the_ticket`
  - `a_ui_connection_cannot_answer_for_a_ticket`
  - `an_elevator_connection_gets_no_notification_and_no_other_op`
  - `an_elevator_connection_serves_nothing_but_one_describe_and_one_confirm`
  - `a_silent_elevator_connection_ends_after_its_lifetime_and_a_bye_ends_it_at_once`
- **Slice 4, the shared interface.** `service_backend/elevation.rs`, `elevated_client.rs`, `state.rs`, `interact.rs`, `strings.rs`.
  - `service_backend/tests.rs`:
    - `elevate_asks_a_ticket_runs_the_elevator_ends_it_and_reports_ok`
    - `elevate_does_not_hold_the_backend_while_the_elevator_runs`
    - `a_declined_uac_ends_the_ticket_and_reports_refused`
    - `elevate_without_an_elevator_reports_internal_and_sends_no_ticket`
    - `an_action_on_hold_sends_no_ticket_and_reports_internal`
    - `the_elevated_client_confirms_declines_and_maps_each_refusal_to_its_exit_code`
    - `the_elevated_client_exits_unreachable_without_a_service`
  - `state_tests.rs`:
    - `a_confirmed_elevation_resends_the_command_that_needed_it`
    - `a_failed_elevate_reply_shows_in_the_step_not_the_strip`

**Left, in order:**

| Order | Slice | Needs before it | `ACCOUNTS.md` link |
|---|---|---|---|
| 1 | **3**, same-user pipe path | built 1, 2 | none; independent of every `ACCOUNTS.md` slice |
| 2 | **5**, Windows client: launcher, `--elevated`, `WinElevator` | 3 | `ACCOUNTS.md` S6 needs slices 1 to 5; S7 needs 3 to 5 |
| 3 | **6**, the confirmation window (decided in) | 5 | none; the hand check of S7 shows it |
| 4 | **`ACCOUNTS.md` S7**, over the shoulder: `elev-<session>`, the approver rule, the pass with its approver, the Begin rule, the credentials text | 3, 5 (6 for its hand check) | it is S7 itself, beside S6 |
| 5 | **7**, viewer-side actions: tags 2 and 3, op 50 behind the pass, `Viewer.elevated`, C6 actions | 5, and `ACCOUNTS.md` S1 (`Elevate` 0x000C) and S4 | built **inside `ACCOUNTS.md` S6**, which also needs `GRANTS-SYNC.md` S4 |
| 6 | **8**, documents | each slice's own lines go with it; the rest last | with `ACCOUNTS.md` S11 |

**Slice 3: same-user pipe path.** Files: `crates/swapdesk-sys-win/src/pipe.rs` (`administrators_present`); `crates/swapdesk-service/src/ui_pipe.rs` (role `Elevator` goes to `serve_elevator_cancellable`; `can_elevate` from the token).
- `swapdesk-sys-win` `identify_client_reports_administrators_present_for_a_filtered_administrator`. It needs a test user who is an administrator with UAC on, and passes vacuously, with a message, on a standard account. Red before.
- `swapdesk-service` `ui_pipe` `an_elevator_hello_is_served_by_the_restricted_loop_and_another_role_is_refused` (development-shape pipe). Red before.
- `an_administrator_accounts_ui_connection_can_open_a_ticket` (development-shape pipe; the built Begin then answers a ticket). Red before.

**Slice 5: Windows client.** Files: `crates/swapdesk-sys-win/src/elevate.rs`; `crates/swapdesk-app/src/{cli,main,elevate}.rs`; the full loop in new `crates/swapdesk-crossdrive/tests/elevation.rs`, which already depends on the hub and the interface, so no Cargo change.
- `cli` `elevated_takes_exactly_32_hex_digits_and_no_mock`. Red before.
- `swapdesk-app` `elevate` tests, each red before:
  - `the_command_line_is_the_verified_exe_with_the_ticket_and_the_dev_flags` (fake launcher);
  - `uac_cancel_maps_to_declined_and_a_launch_error_to_internal`;
  - `an_unprotected_exe_is_never_launched`.
- `crossdrive` tests, each red before:
  - `continuar_runs_the_elevated_confirmation_and_the_step_closes_with_the_grant_applied`. It drives a real `UiHub` as the asking UI over an in-memory duplex, with `ServiceBackend` and `UiState`. A fake launcher runs `elevated_client::run` against `serve_elevator` with an elevated `ConnInfo`. This is stage 56's done-when test, minus the UAC click.
  - `a_declined_uac_shows_the_failure_and_changes_nothing`;
  - `an_unelevated_elevated_instance_gets_needs_admin_and_no_pass` (exit code 6).

**Slice 6: the confirmation window.** Files: `crates/swapdesk-shell-win` (new window module over `swapdesk-paint`'s dialog layout); the `confirm` closure in `crates/swapdesk-app/src/elevate.rs`.
- `swapdesk-shell-win` tests, each red before:
  - `the_elevated_window_shows_the_action_from_the_service_and_two_buttons_in_order` (offscreen paint and control tree);
  - `enter_alone_confirms_nothing`;
  - `it_closes_as_declined_when_the_time_runs_out`.
- One capture for the canvas board in the owner batch.

**`ACCOUNTS.md` S7: over the shoulder.** This note's part:
- `ui_pipe.rs`: `elev-<session>`.
- `elevation.rs`: the approver rule and `Pass.approver`.
- `ops.rs`: `GrantsChanged.by`.
- Begin without `can_elevate`, and `ConnInfo.can_elevate` removed.
- The instance opens `elev-<session>`.
- The shell's account type, and `ElevBodyWindowsCredentials` once the owner approves it.

Reversed tests, named in the commit:
- `an_account_that_is_not_an_administrator_gets_no_ticket`: a standard account now gets one.
- `an_approver_of_another_user_or_session_is_refused`: another user in the same session is accepted; another session stays refused.

New tests, each red before (the first four are `ACCOUNTS.md`'s):
- `an_administrator_of_another_account_in_the_same_session_confirms_a_standard_accounts_ticket`
- `the_grant_lands_in_the_asking_accounts_store_with_the_approver_in_the_log`
- `an_unelevated_or_other_session_approver_is_refused`
- `the_elevator_pipe_serves_only_the_elevator_role`
- `the_approver_connection_is_named_by_its_own_token_not_the_sessions_user` (`ui_pipe`)
- `a_standard_accounts_step_says_windows_will_ask_for_an_administrators_password` (`state_tests`)

**Slice 7: viewer-side actions** (inside `ACCOUNTS.md` S6). Files: `swapdesk-ipc` (tags 2 and 3); `swapdesk-host` `ui_hub/{elevation,ops,terminal}.rs`; `swapdesk-ui` `api.rs` (C6 actions), `state.rs`, `interact.rs`, `service_backend/elevation.rs`, `strings.rs` (the `ElevActElevateSession` slot).
- ipc `elev_action_tags_2_and_3_round_trip_and_tag_1_is_a_codec_error`. Red before.
- hub, each red before:
  - `a_session_ticket_is_refused_unless_it_is_this_connections_and_can_be_elevated`
  - `an_admin_terminal_ticket_needs_a_session_holding_admin_terminal`
  - `elevate_and_the_administrator_terminal_need_the_viewer_side_pass` (`ACCOUNTS.md`'s)
  - `an_elevated_session_opens_the_administrator_terminal_without_a_second_pass` (`ACCOUNTS.md`'s)
  - `an_admin_terminal_pass_opens_one_terminal`
- `state_tests`, each red before:
  - `a_confirmed_elevate_session_sends_elevate_session_again`
  - `a_confirmed_admin_terminal_opens_the_administrator_terminal`
- Changed meaning, named in the commit:
  - `other_admin_only_commands_also_lead_to_the_elevation_step` (`ConnectUnattended` goes, `ElevateSession` comes);
  - `an_action_on_hold_sends_no_ticket_and_reports_internal` (no action is on hold any more; it becomes `every_action_gets_a_ticket`).

**Slice 8: documents.** The text changes of 3.8, the FLUXOS proofs for F-PERM-01 and F-PERM-03, and `check_flows.py --check` exits 0.

**The owner's hand check (owner batch).** On the installed service, or a development service of the same user:
- (a) Permissões, "Entrar sem perguntar": Continuar, UAC Sim, the window, Confirmar. The switch turns on and Atividade shows the change.
- (b) The same with UAC Não: the reason line, and nothing changes.
- (c) The incoming card's "Deixar entrar sem perguntar".
- (d) `ADMIN_TERMINAL` granted the same way.
- (e) In a standard account: the credentials prompt answered by an administrator. The grant lands and Atividade names the administrator.
- (f) After `ACCOUNTS.md` S6, with the Mac or the second Windows machine: elevate a session and open the administrator terminal.

Facts 2 to 4 of section 6 (UNPROVEN) are read off these runs.

## 8. Options rejected

- **The elevated instance applies `SetGrants` itself.** That makes two code paths for grant changes and loses the asker as the D13 grantor. The pass reuses the existing path.
- **The action on the command line (`--elevated set-grants <device> <bits>`).** Any same-user process could start it with any action. UAC shows no action. Nothing binds it to the asking interface.
- **The service runs the action on Confirm and answers by notification.** `TermOpen` and op 50 replies carry results that the interface's pending command expects.
- **Widening `ui-<session>`'s DACL to Administrators for over-the-shoulder approval.** That would put a second identity on the pipe that carries everything of A's interface. **Replaced, not rejected outright:** over the shoulder is accepted (decision 4) through the separate `elev-<session>` pipe, which serves only the elevator role.
- **Refusing over-the-shoulder approval.** The first version recommended it. Reversed by the owner (decision 4) because, after D44, an administrator signed in to their own account can no longer reach A's store (`ACCOUNTS.md` 8.2).
- **Requiring the approver to be the asking user.** That falls with decision 4. The Windows session stays the binding.
- **`ConnectUnattended`: a confirmation at connect time.** Replaced by decision 5. Plain unattended entry asks nothing, and the confirmation moves to `Elevate` and the administrator terminal (`ACCOUNTS.md` 8.3 and 9).
- **Asking for `ADMIN_TERMINAL` only from administrator accounts (`can_elevate`), the first version's rule.** Superseded by `ACCOUNTS.md` 9.3: every connection asks toward a host known to be minor 1 or newer, and use is guarded by the viewer-side confirmation.
- **No mid-session wire message.** The first version rejected one for a single button. Now accepted as `Elevate` (0x000C, minor 1, `ACCOUNTS.md` 9.3), which carries both bits.
- **UAC approval alone, no window.** That leaves the "swap the action at the expected moment" attack open (decision 1 keeps the window).
- **A Windows `TaskDialog` as the window.** It breaks the rule "one interface on both systems" more than a painted copy of the existing step does.
- **A whole elevated interface.** It would put the viewer, its decoder and its file dialogs at high integrity, against SPEC's trust boundary 4.
- **The administrator terminal's panel in the elevated instance.** That means a second viewer window at high integrity, with the same objection; the residual risk is in section 5.
- **Checking the approver's image path.** Any process that passed UAC is an administrator, part of the TCB.
- **A new `UiNotify` kind for the outcome.** Op 38 returns it from the same source, the service.

## 9. For the owner

**Texts that need approval (3.7):**
1. `ElevNotAdmin`, new wording: "A confirmação não veio de um administrador deste computador." / "The confirmation did not come from an administrator of this computer."
2. `ElevDeclined` (new): "A confirmação foi recusada." / "The confirmation was declined." It is the reason line for a declined UAC prompt or window; without it the step keeps "Nada foi alterado." alone.
3. `ElevBodyWindowsCredentials` (new), the step's body for a standard account: "... O Windows vai pedir o nome e a senha de um administrador." / "... Windows will ask for an administrator's name and password."
4. `ElevActElevateSession`: "Elevar o controle em {0}" / "Elevate control on {0}". It goes with the button texts still open in `ACCOUNTS.md` 19.2.

**Hand checks:** the owner batch of section 7.

Nothing else in this note is open. The scope of slice 7 and S7 within 1.1 is `ACCOUNTS.md` 19.9.
