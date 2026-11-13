# Administrator confirmation on the Mac

Status: design note for stage 57. Not built on the Mac; the shared side it relies on is partly built (section 5.1). Companion of `docs/ELEVATION.md` (the shared service side and the Windows client, section 4: what the service offers a Mac client) and `docs/ACCOUNTS.md` (D44: the final set of actions, sections 8, 9, 12 and 17.2). Updated 2026-11-10 with the owner's decisions of that day.

The defect: the step "Confirmação de administrador" ("Administrator confirmation") opens when the service answers `NeedsAdmin`, but on the Mac nothing behind "Continuar" asks macOS for an administrator. So nothing that needs a local administrator can pass on the Mac (D7, D10 as narrowed by D44, F-PERM-01, F-PERM-03).

What exists now (read 2026-11-10):

- **Interface (`swapdesk-ui`), shared and built.**
  - `state.rs` opens the step on `ApiCode::NeedsAdmin`; `interact.rs` `activate_dialog` pushes `Command::Elevate { action }` and sets `ElevPhase::Waiting`.
  - `ServiceBackend::set_elevator(Arc<dyn Elevator>)` and `service_backend/elevation.rs`: on `Elevate`, a worker thread sends op 35 `ElevationBegin` (a ticket bound to this connection and the exact action, 120 s) and calls `Elevator::run(&ticket, deadline)` with a deadline 110 s later (`RUN_FOR`). It always sends op 38 `ElevationEnd`, then emits `UiEvent::ElevationResult` and `Done`. The service's outcome decides the result (`outcome_result`); the run only says why a failure failed.
  - On `ElevationResult Ok` the state sends the original command again; today only `SetGrants` has its followup. A failed `Elevate` reply becomes `ElevPhase::Failed(code)` with no error strip. The failure lines `ElevNotAdmin` (`NotPermitted`; wording under revision) and `ElevTimedOut` (`Timeout`) exist.
  - `elevation_action_text(lang, action, peer_name)` is a free function shared by the step and the elevated window.
- **Hub (`swapdesk-host/src/ui_hub/elevation.rs`), shared and built.**
  - Tickets: 16 random bytes, 120 s, one per connection, at most 16.
  - `UiHub::describe_elevation(&ticket, &approver)` and `UiHub::confirm_elevation(&ticket, &approver, approve)`. Both require `approver.admin` and the ticket's session and user id, and an asking connection that is still connected.
  - On approval, a pass on the asking connection: 30 s, one use, exact action, at most four. `take_pass` in `set_grants`. `ConnInfo.can_elevate` gates Begin (`NotPermitted` when false).
  - The wire `ElevAction` holds only tag 0 `SetGrants` so far.
- **The final set of actions** (`ACCOUNTS.md` 17.1, owner decision 3 of D44):
  - tag 0 `SetGrants`;
  - tag 2 `OpenAdminTerminal { session_id }`: the viewer's machine confirms before the administrator terminal outside an elevated session;
  - tag 3 `ElevateSession { session_id }`: the viewer's machine confirms before `Elevate` (UI op 50);
  - tag 1 (`ConnectUnattended`) is removed for good: plain unattended entry asks nothing.
- **The Mac app** has no daemon in 1.x (D27, D32). `swapdesk-app-mac/src/service.rs` `InProcessService` runs the core and the hub in the app and serves the window's backend over an in-memory pipe. Its one `ConnInfo` is `{ session_id: UI_SESSION, user, admin: false, can_elevate: false }`, with `user.id` = `"local-user"` (D44 makes it the uid, `ACCOUNTS.md` 12). No elevator is set. So on the Mac `Elevate` ends in `Err(Internal)`, and Begin would answer `NotPermitted` anyway.
- **The Mac's terminal spawner** (`swapdesk-sys-mac/src/pty.rs`) refuses `request.admin` with `PermissionDenied(Administrator)` ("the administrator terminal is not available on this Mac"); D32 keeps the root terminal out of v1.

## 1. What Apple's pages say (read 2026-11-10)

Pages read through Apple's documentation data (the same text as the HTML pages). Each fact below carries its page.

### 1.1 Asking for an administrator

- **Authorization Services** (https://developer.apple.com/documentation/security/authorization-services): the Security Server is a daemon that computes authorization and "relies on the Security Agent to interface with users when authentication is needed. Thus an app can verify credentials (usernames and passwords) without ever accessing them directly." The means of authentication may change (Touch ID) without changing the app. "The Authorization Services API is not supported within an App Sandbox because the API allows privilege escalation."
- **AuthorizationCreate** (https://developer.apple.com/documentation/security/authorizationcreate(_:_:_:_:)): creates an authorization reference and can authorize at once. Passing no reference out gives a one-time check: "the Security Server attempts to authorize the requested rights and returns the appropriate result code without actually granting the rights." When done, `AuthorizationFree`.
- **AuthorizationCopyRights** (https://developer.apple.com/documentation/security/authorizationcopyrights(_:_:_:_:_:)): "Authorizes and preauthorizes rights synchronously." To authorize before a privileged operation, pass `interactionAllowed` and `extendRights`. Without `partialRights`, one denied right gives `errAuthorizationDenied`; without `interactionAllowed`, a right that needs the user gives `errAuthorizationInteractionNotAllowed`; with it, a user who cancels gives `errAuthorizationCanceled`. The `environment` can carry "icon or prompt data to be used in the authentication dialog box", and since 10.4 a user name and password ("authorize a user without displaying the authentication dialog box"); it is not stored in the reference. `destroyRights` "prevent[s] the Security Server from preserving the rights obtained during this call."
- **AuthorizationCopyRightsAsync** (https://developer.apple.com/documentation/security/authorizationcopyrightsasync(_:_:_:_:_:)): macOS 10.7+, the same call with a completion callback.
- **AuthorizationFlags** (https://developer.apple.com/documentation/security/authorizationflags and its members): `interactionAllowed` "permits user interaction as needed"; `extendRights` "permits the Security Server to attempt to grant the rights requested"; `destroyRights` on `AuthorizationFree` "revokes authorization from the process as well as from any other process that is sharing the authorization". Bit values from the generated bindings of `objc2-security` 0.3.2 (https://docs.rs/objc2-security/0.3.2/src/objc2_security/generated/Authorization.rs.html): `InteractionAllowed = 1<<0`, `ExtendRights = 1<<1`, `PartialRights = 1<<2`, `DestroyRights = 1<<3`, `PreAuthorize = 1<<4`.
- **Result codes** (https://developer.apple.com/documentation/security/authorization-services-result-codes; values from the same generated file): `errAuthorizationSuccess` 0, `errAuthorizationDenied` -60005 ("denied authorization for one or more requested rights ... also returned if there was no definition found in the policy database"), `errAuthorizationCanceled` -60006 ("The user canceled the operation"), `errAuthorizationInteractionNotAllowed` -60007, `errAuthorizationInternal` -60008.
- **AuthorizationFree** (https://developer.apple.com/documentation/security/authorizationfree(_:_:)): "To remove all shared and non-shared authorizations, pass the constant destroyRights."

### 1.2 Which right

- **An existing right.** `kAuthorizationRightExecute` (https://developer.apple.com/documentation/security/kauthorizationrightexecute) is the string `system.privilege.admin` (generated `AuthorizationTags.rs`); macOS names its own rights `system.*`. Asking for it with `interactionAllowed | extendRights` makes the Security Agent ask for an administrator. The archived Authorization Services Programming Guide, "Authorization Services Tasks" (https://developer.apple.com/library/archive/documentation/Security/Conceptual/authorization_concepts/03authtasks/authtasks.html): "The user enters an administrator user name and password and clicks OK. The Security Server then uses the user name and password to authenticate and authorize the user."
- **A custom right.** "Authorization Concepts" (https://developer.apple.com/library/archive/documentation/Security/Conceptual/authorization_concepts/02authconcepts/authconcepts.html): "You must create the rights your application uses", in reverse-DNS form; a right with no rule of its own falls to the generic rule. **AuthorizationRightSet** (https://developer.apple.com/documentation/security/authorizationrightset(_:_:_:_:_:_:)) "Creates or updates a right entry in the policy database" from a rule dictionary or the name of a rule to copy, with a `descriptionKey` looked up in the given bundle's localizations (that description is what the dialog explains). Delegate rules (https://developer.apple.com/documentation/security/policy-database-constants): `kAuthorizationRuleAuthenticateAsAdmin` = `authenticate-admin` ("the user must authenticate as an administrator"), `kAuthorizationRuleIsAdmin` = `is-admin` ("the user must be an administrator", membership), `kAuthorizationRuleAuthenticateAsSessionUser` = `authenticate-session-owner`. **AuthorizationRightGet** needs no reference: "the policy database is world readable". **AuthorizationRightRemove** removes a right.
- **What writing a right requires.** The pages say `AuthorizationRightSet` takes "a valid authorization reference used to authorize modifications"; they do not say which right that reference must hold. UNPROVEN: adding a right needs an administrator (the database's own `config.add.*` rights). Probe on the Mac: `security authorizationdb read config.add.` (read only).

### 1.3 The prompt, how long a right lasts, sharing

- **Text.** `kAuthorizationEnvironmentPrompt` (https://developer.apple.com/documentation/security/kauthorizationenvironmentprompt) is "invocation-specific additional text", "a localized UTF-8 string"; `kAuthorizationEnvironmentIcon` is "a full path to an image compatible with the NSImage class". The main sentence of the dialog is the system's. UNPROVEN: whether macOS 14 to 27 still shows the additional text, and which app name it shows for an ad-hoc or self-signed `Swapdesk.app`. Hand check: section 5, slice M7.
- **Credentials cache** ("Authorization Concepts", section "The Credentials Cache and the Authentication Dialog"): the Security Server keeps a global cache per login session and a cache per authorization reference. Whether a dialog appears "depends on whether the Security Server has already cached the required credential". A rule's `timeout` is how long a credential serves (0: once; missing: the whole login session); `shared` puts it in the global cache. "The only way to guarantee that a credential acquired when you request a right is not shared with other authorization instances is to destroy the credential", with `AuthorizationFree(..., kAuthorizationFlagDestroyRights)`. The guide's example of an administrator who logs in and is never asked again describes old releases. UNPROVEN what `system.privilege.admin` delegates to on current macOS (from memory, a GUESS: `authenticate-admin-nonshared`, shared false, a short timeout). Probe: `security authorizationdb read system.privilege.admin` and `security authorizationdb read authenticate-admin-nonshared` (read only).
- **Standard user.** The dialog asks for an administrator's name and password, which then authorize the right for the calling process ("Authorization Concepts": a non-admin user who tries to change a system preference is shown the authentication dialog). UNPROVEN on macOS 27: the name field is empty for a standard user and filled in for an administrator; an administrator with Touch ID is offered Touch ID; a standard user's own password is refused in place (the dialog shakes) and how many tries end the call with `errAuthorizationDenied`. Hand check: slice M7.
- **Threads.** The dialog belongs to the Security Agent, another process, so the calling thread only waits. Apple names no thread requirement for the C calls. UNPROVEN that a call from a background thread of an AppKit app shows the dialog in front and with keyboard focus. The page of `AuthorizationMakeExternalForm` (https://developer.apple.com/documentation/security/authorizationmakeexternalform(_:_:)) says the system uses the asking process "to determine whether the authorization dialog should have keyboard focus". The person has just clicked "Continuar", so the app is in front. Hand check: slice M7. Nothing in the pages withdraws a dialog that is open: no cancel call exists for a running `AuthorizationCopyRights`.
- **Sandbox and hardened runtime.** Not in an App Sandbox (quote above; `SFAuthorization`, https://developer.apple.com/documentation/securityfoundation/sfauthorization, says the same). The Hardened Runtime page (https://developer.apple.com/documentation/security/hardened-runtime) lists no entitlement for Authorization Services: its exceptions are JIT, unsigned memory, DYLD variables, library validation, executable memory, debugging, and the resource entitlements (audio input, camera, location, contacts, calendars, photos, Apple Events). Swapdesk is neither sandboxed nor signed with the hardened runtime today (`scripts/mac/build-app.sh` signs without `--options runtime`). UNPROVEN: that the calls behave the same under the hardened runtime. Proven when a notarized build exists (D41).
- **Deprecation.** `AuthorizationCreate`, `AuthorizationCopyRights`, `AuthorizationFree`, `AuthorizationRightSet`/`Get`/`Remove`, `AuthorizationMakeExternalForm`/`CreateFromExternalForm` and the flags carry no deprecation on their pages (macOS 10.0+; the async call 10.7+). `AuthorizationExecuteWithPrivileges` (https://developer.apple.com/documentation/security/authorizationexecutewithprivileges) is deprecated since 10.7: "Use a launchd-launched helper tool and/or the Service Management framework".
- **External form** (`AuthorizationMakeExternalForm`, `AuthorizationCreateFromExternalForm`, https://developer.apple.com/documentation/security/authorizationcreatefromexternalform(_:_:)): passes a reference to another process ("Authorizations are bound by session, process, and time limits"). This is the daemon path of C2.3. 1.x has no daemon to receive it.
- **Who typed it.** `AuthorizationCopyInfo` (https://developer.apple.com/documentation/security/authorizationcopyinfo(_:_:_:)) "Retrieves supporting data such as the user name ... gathered during evaluation". UNPROVEN that it returns the administrator's name after `authenticate-admin`. So naming the approver is not free, and by the owner's decision the Mac's audit line in 1.1 does not name the approving administrator (3.5). Probe, if it is ever wanted: in the M7 hand run, a debug build traces whether `AuthorizationCopyInfo(ref, "username")` returned an item (its presence, never its value).

### 1.4 What LocalAuthentication can and cannot prove

`LAPolicy.deviceOwnerAuthentication` (https://developer.apple.com/documentation/localauthentication/lapolicy/deviceownerauthentication) authenticates "in macOS with Touch ID, Apple Watch, or the user's password"; `evaluatePolicy` (https://developer.apple.com/documentation/localauthentication/lacontext/evaluatepolicy(_:localizedreason:reply:)) takes an app reason string and replies on a private queue; `LARight` (https://developer.apple.com/documentation/localauthentication/laright, macOS 13+) gates a resource with the same requirements. All of them prove that **the person is the logged-in user** (the device owner), which a standard user also is. None asks for another account or proves an administrator. It is not an administrator check here and is not used.

## 2. Recommended path and one alternative

### Recommended: an in-process check with `system.privilege.admin`, one exact action, once

The Mac supplies only `Elevator::run`. The shared worker has already taken a ticket for the exact action (op 35).
1. `run` asks macOS on a thread of its own. It creates a fresh authorization reference and calls `AuthorizationCopyRights` for `system.privilege.admin` with `interactionAllowed | extendRights | destroyRights`. The step's action line goes as `kAuthorizationEnvironmentPrompt` (owner's decision, 2026-11-10). The reference is then freed with `destroyRights`, so no credential outlives the request.
2. Only after an approval does `run` build the approver `ConnInfo` (`admin: true`, the app connection's session and user). It then calls `UiHub::confirm_elevation(&ticket, &approver, true)` in process.
3. The hub puts its shared pass on the app's connection (30 s, one use, that exact action). The shared worker reads the outcome (op 38) and reports `ElevationResult`. The interface sends the original command again, and the hub lets it through once.

Reasons:
- macOS asks: the Security Agent draws the dialog and checks the password. Swapdesk never sees it.
- An existing right needs no write to the system's policy database, no extra prompt the first time, and nothing to remove at uninstall.
- An administrator's name and password are required (`authenticate-admin`), not group membership (`is-admin` would pass an administrator's session without a password, which SPEC forbids).
- No new crate: `objc2-security` is already a workspace dependency (`swapdesk-secret-mac` uses it). Its `Authorization` and `AuthorizationTags` features hold every call and constant. Enabling them in `swapdesk-sys-mac` adds one dependency edge to `Cargo.lock`, a main-session change.
- Nothing Mac-specific in the hub or the interface: the ticket, the pass, the resend, the failure lines and the 120 s, 110 s and 30 s budgets are the shared ones (`ELEVATION.md` 4), so both systems behave alike.

### Alternative: a custom right `com.swapdesk.admin`

The right is registered once with `AuthorizationRightSet`: rule `authenticate-admin`, `shared` false, `timeout` 0, a localized description from the app bundle. It is then asked for in the same way. It is better where the prompt should name Swapdesk's own reason, and it is what a future daemon would check with the external form. It is not chosen for 1.x for these reasons:
- It writes the system's policy database from a user app. Writing it needs an administrator (UNPROVEN, 1.2), so the first use shows two prompts or one combined, more behaviour to prove.
- It leaves an entry behind after the app is deleted.
- Its gain (`timeout` 0, not shared) is already had with a fresh reference and `destroyRights` per request.

It stays the path for the root daemon (below).

### What the check is worth, honestly

In 1.x the service runs **in the same process** as the window (D27, D32), and the hub's connection is an in-memory pipe.
- **It guards against a person at the keyboard who is not an administrator.** A standard user, or anyone using an unlocked session, cannot grant `UNATTENDED` on the Mac, elevate a session or open an administrator terminal from it without an administrator's name and password typed into macOS's own dialog. That is the D7 promise, and the D10 promise as narrowed by D44, for the person at the machine.
- **It does not guard against code running as that user.** Such code can start `Swapdesk.app` with `DYLD_INSERT_LIBRARIES` (no hardened runtime today), replace a user-writable app bundle, or drive the process some other way. With the app's own code in its hands, it can call `confirm_elevation` itself or skip the check: the decision and the check live in one address space that the user owns. The hardened runtime and library validation (a notarized build) would remove the easy injection, not the principle.
- **With a root daemon (out of scope, C2.2):** the app would send the right's external form over `ui.sock`; the daemon would internalize it and call `AuthorizationCopyRights` without interaction for the right (the custom right of the alternative). The decision would then sit in a root process that user code cannot write. Code running as the user would still need an administrator's password, or a live shared credential, to get the right. Only then does the Mac match Windows' elevated-token check.

**Text for `docs/THREAT-MODEL.md`** (to be added when stage 57 is built; the row of R14 changes to "macOS 1.x: in-process, see ELEVATION-MAC"):

> macOS 1.x (no daemon, D32): the administrator confirmation is made by the app that also runs the service. macOS's own dialog (Authorization Services, right `system.privilege.admin`) proves that an administrator typed their name and password for one exact action. This protects against a non-administrator at the keyboard. It does not protect against code running as the logged-in user, which can alter the app's process and pass the check. Accepted for 1.x because such code already controls everything that user can do; the remedy is the root daemon of C2.2, which checks the right's external form itself (out of scope).

## 3. Hook-up

### 3.1 Where each piece lives

| Piece | Crate, file | `unsafe` |
|---|---|---|
| Authorization Services binding and status mapping | `swapdesk-sys-mac`, new `src/admin.rs` (macOS system glue, C3, beside `pty.rs` and `actions.rs`) | yes, here only |
| `MacElevator`, the Mac's `Elevator`, and its seam `AdminConfirm` | `swapdesk-app-mac`, new `src/elevate.rs` | none |
| The app connection's `can_elevate`, and the elevator handed to the backend | `swapdesk-app-mac/src/service.rs` (`InProcessService::start`) | none |
| The system prompt chosen for a normal run, a scripted one for tests | `swapdesk-app-mac/src/app.rs`, `service.rs` tests | none |
| The inert administrator-terminal row on a Mac host | `swapdesk-ui` (`interact.rs`, `strings.rs`), `swapdesk-paint` (`screens_main.rs`, `view.rs`) | none |

`swapdesk-shell-mac` does not change. The prompt is the Security Agent's window, not an AppKit panel of the app, so `effects.rs` gains no effect and nothing runs on the main thread. My first draft had a backend wrapper (`Elevating`) and its own pass slot in `ConnInfo`; both are gone. The shared worker and the shared pass replace them (section 6).

### 3.2 Signatures

`crates/swapdesk-sys-mac/src/admin.rs` (unchanged from the first draft):

```rust
/// How macOS answered the administrator check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// An administrator's name and password were accepted for `system.privilege.admin`.
    Approved,
    /// The person pressed Cancel in the system's dialog (errAuthorizationCanceled).
    Cancelled,
    /// The credentials were not an administrator's, or tries ran out (errAuthorizationDenied).
    Denied,
    /// No dialog could be shown (errAuthorizationInteractionNotAllowed): no GUI session.
    NoInteraction,
    /// Called on the main thread: refused without asking (the call blocks until a person answers).
    MainThread,
    /// Any other status, kept for the trace log.
    Failed(i32),
}

/// Pure: the outcome of an Authorization Services status. Built and tested on every host.
pub fn outcome_of(status: i32) -> AdminOutcome;
/// Pure: `Some(MainThread)` when called on the main thread.
pub fn refuse_on_main(is_main: bool) -> Option<AdminOutcome>;

/// Asks macOS for an administrator (`system.privilege.admin`, interaction allowed, rights extended and destroyed after
/// the call, a fresh reference freed with `destroyRights`). Blocks until the dialog ends. `prompt` is the dialog's
/// additional text. Never call it on the main thread (`libc::pthread_main_np`): it returns `MainThread`.
#[cfg(target_os = "macos")]
pub fn confirm_administrator(prompt: Option<&str>) -> AdminOutcome;

/// The same without interaction: never shows a dialog (tests and probes).
#[cfg(target_os = "macos")]
pub fn check_administrator_silently() -> AdminOutcome;
```

`crates/swapdesk-app-mac/src/elevate.rs`, built and tested on every host:

```rust
/// The seam tests replace: asks for an administrator, showing `line`, and blocks until answered.
pub trait AdminConfirm: Send + Sync {
    fn confirm(&self, line: &str) -> Confirmation;
}

/// The app's own copy of the outcome, so this module builds without `swapdesk-sys-mac` off macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation { Approved, Cancelled, Denied, NoInteraction, Failed }

/// macOS's dialog: `swapdesk_sys_mac::admin::confirm_administrator(Some(line))`, its outcome mapped.
#[cfg(target_os = "macos")]
pub struct SystemAdminPrompt;

/// The Mac's `Elevator`: asks through `AdminConfirm`, and only on an approval confirms the ticket in process.
pub struct MacElevator { /* hub: UiHub, asker: ConnInfo, confirm: Arc<dyn AdminConfirm>, open: Arc<AtomicBool> */ }

impl MacElevator {
    /// `asker` is the app connection's own `ConnInfo` (admin false), the one `serve` was given.
    pub fn new(hub: UiHub, asker: ConnInfo, confirm: Arc<dyn AdminConfirm>) -> Self;
}

impl swapdesk_ui::service_backend::Elevator for MacElevator {
    fn run(&self, ticket: &ElevationTicket, line: &str, deadline: Instant) -> ElevatorRun; // `line`: need N1 (3.6)
}

/// Pure: the approver an approval stands for: `asker` with `admin: true`, same session and user.
pub fn approver_of(asker: &ConnInfo) -> ConnInfo;
/// Pure: what `run` returns for a confirmation and, after an approval, the hub's answer to `confirm_elevation`.
pub fn run_of(c: Confirmation, confirmed: Option<Result<(), ApiCode>>) -> ElevatorRun;
```

### 3.3 Threads and the deadline

- **`run` is called on the shared worker thread** (`swapdesk-ui-elevate`). The UI (main) thread keeps drawing "Esperando a confirmação do sistema…" and the service's tokio runtime is not touched, because `confirm_elevation` only takes the hub's mutexes.
- **The dialog cannot be withdrawn** (1.3). So `run` does not call `confirm` on the worker itself. It starts one thread, `swapdesk-admin-check`, which calls `confirm(line)` and sends the answer into a channel. `run` waits on that channel until `deadline` (110 s after the ticket):
  - An answer in time goes through 3.4.
  - No answer by the deadline: `run` returns `TimedOut` and drops its end of the channel. When the person later answers, the answer goes nowhere and nothing is confirmed. Only `run`, before its deadline, ever calls `confirm_elevation`.
- **One dialog at a time.** `open` is set while a `swapdesk-admin-check` thread lives. A new `run` while a timed-out dialog is still on screen returns `Failed(Busy)` (the existing `ErrBusy` line) instead of stacking a second dialog.

### 3.4 What `run` does

1. If a dialog is still open, return `Failed(Busy)`.
2. Ask (`AdminConfirm::confirm(line)` on its own thread) and wait until `deadline`.
3. Map the confirmation with `run_of`:
   - **`Approved`:** build `approver_of(&self.asker)` now, never earlier, and call `hub.confirm_elevation(ticket, &approver, true)`.
     - `Ok` gives `Ended { exit_code: Some(0) }`. The shared worker's op 38 then answers `Confirmed`, and the result is `Ok`.
     - `Err(NotFound)` (the ticket expired or the asking connection is gone) gives `Failed(Timeout)`.
     - Any other `Err` is a bug in the approver and gives `Failed(Internal)`, with a trace line.
   - **`Cancelled`:** `Declined`. No call to the hub: the app has no administrator to decline with. Op 38 closes the open ticket, and the result is `Refused`.
   - **`Denied`:** `Failed(NotPermitted)`.
   - **`NoInteraction` or `Failed`:** `Failed(Internal)`, with the status in the trace log.
   - **Deadline passed:** `TimedOut`.

The approver is the asker with `admin: true`: the same `session_id` (`UI_SESSION`), the same `UserRef` (the uid after D44, whatever the app connection carries), and `can_elevate: true`. It therefore passes `with_ticket` both as built today (same user id and session) and after `ACCOUNTS.md` S7 (same session only). It lives for one call and is never stored.

### 3.5 Failure mapping

The service's outcome decides first (`outcome_result`). The Mac's `ElevatorRun` only says why a failure failed:

| What happened | Authorization status | `ElevatorRun` | `ElevationResult` | The step |
|---|---|---|---|---|
| An administrator's name and password accepted, ticket confirmed | 0 | `Ended { Some(0) }`, op 38 `Confirmed` | `Ok(())` | closes; the original command goes again and passes once |
| Cancel in macOS's dialog | -60006 | `Declined` | `Err(Refused)` | the failure page, "A confirmação não foi concluída / Nada foi alterado." with "Tentar de novo" (owner's decision: as Windows' UAC "No") |
| Wrong password, or not an administrator's credentials, after the dialog's own retries | -60005 | `Failed(NotPermitted)` | `Err(NotPermitted)` | failure page plus the not-an-administrator line (wording under revision, see "Still open") |
| No dialog possible (no GUI session) | -60007 | `Failed(Internal)` | `Err(Internal)` | failure page plus `ErrInternal` |
| No answer within 110 s, or approved after the ticket expired | none | `TimedOut` / `Failed(Timeout)` | `Err(Timeout)` | failure page plus `ElevTimedOut` |
| A dialog of an earlier request still open | none | `Failed(Busy)` | `Err(Busy)` | failure page plus `ErrBusy` |
| Called on the main thread (a bug), or any other status | other | `Failed(Internal)` | `Err(Internal)` | failure page plus `ErrInternal` |
| The person pressed "Cancelar" on the step while Waiting | (dialog still up) | runs on | ignored by the state | if an administrator then approves, the pass sits unused for 30 s on the app's own connection, for the exact action just approved, and expires (`ELEVATION.md` 2.5) |

**Audit.** The pass is consumed by the existing code path. Today `GrantsChanged.by` and D13's grantor are the app connection's user. After `ACCOUNTS.md` 17.1 they are the approver's `UserRef`, which on the Mac is the same account. So the Mac's audit line names the account that asked, never the administrator who typed the password. That is the owner's decision for 1.1: naming the administrator is not free (`AuthorizationCopyInfo` is UNPROVEN, 1.3). Swapdesk never sees a name or password typed into the dialog, and the trace log carries the outcome and status number only.

### 3.6 What the Mac still needs from the shared side

- **N1: the step's line reaches `run`.** The owner decided that the action line goes into macOS's dialog, but `Elevator::run(&ticket, deadline)` gets no text, and the backend worker knows neither the language nor the device's name. The smallest change:
  - `Command::Elevate { action, line: String }`, where `interact.rs` `activate_dialog` fills `line` with the line the step shows (`elevation_action_text`);
  - `Route::Elevate(action, line)`;
  - `Elevator::run(&self, ticket, line: &str, deadline)`. Windows ignores `line`, because its elevated window reads the action from the service (`describe_elevation`).

  On the Mac the text is information only: the ticket binds the exact action, whatever the line says. Rejected alternatives: building an admin approver before the prompt only to call `describe_elevation` (an admin `ConnInfo` before any administrator), and giving `run` the `ElevatedAction` and making the Mac find the language and device name itself. If N1 is refused, the dialog carries no extra text, and the step behind it still shows the line.
- **N2: tags 2 and 3 with their followups.** These are `ElevAction::OpenAdminTerminal`, `ElevAction::ElevateSession`, UI op 50, `ElevatedAction::ElevateSession`, and `apply_elevation_result` pushing `TermOpen { admin: true }` and op 50 again. Built by the shared side (`ELEVATION.md`, `ACCOUNTS.md` S-slices). Until then only `SetGrants` reaches `run`. The other actions end in `Err(Internal)` before any dialog (`wire_action` gives `None`), which is honest.
- **N3: tag 1 removed** with `ElevatedAction::ConnectUnattended` and its `Connect` check (`ACCOUNTS.md` 17.1). Nothing on the Mac refers to it.
- Nothing else: `confirm_elevation`, `set_elevator`, the pass, the resend of `SetGrants`, the `Failed` reply handling and the two failure lines are built.

### 3.7 The administrator-terminal row on a Mac host (owner's decision)

The "Terminal de administrador" switch stays on the Mac's Permissões page, off and inert, with the line "Indisponível no macOS nesta versão". Proposed English: **"Not available on macOS in this version"**. Proposed key: `PermAdminTerminalMacOff`.

Where the row's state and text come from, so both systems paint one screen:
- **One shared predicate in `swapdesk-ui`:** `UiState::grant_available(p) -> bool` is false only for `Permission::AdminTerminal` when `self.this.os == Os::MacOs`. `this` is the machine that stores the grant, and it is the same source that already picks `ElevBodyMac` (`elevation_body_key`).
- **`permission_rows`** gives that row the line `PermAdminTerminalMacOff` as its hint, in place of `PermAdminTerminalHint`.
- **The control specs (`interact.rs`, the `DeviceTab::Permissions` arm)** mark that switch `disabled` and `toggled: Some(false)`. Tab skips it, as for any disabled control, and the accessibility tree reads it as a dimmed switch with the line as its description.
- **`activate(ControlId::Grant(AdminTerminal))`** does nothing while the row is unavailable, so the step can never open for it on the Mac.
- **`swapdesk-paint` `screens_main.rs`** draws the switch off and in its disabled look: `View::switch` needs a `disabled` flag besides `waiting`. Paint is edited by the main session.
- **Because the row depends only on `UiState`,** the Windows renderer paints the Mac's case from a mock scenario whose `this.os` is macOS, and the Mac app paints the same scenario. One capture serves the canvas board for both.

The hub needs no change. Nothing in the UI can ask for the bit, and the Mac's spawner refuses an administrator terminal anyway (section 4).

## 4. The actions on the Mac

| Tag, `ElevAction` | Who asks on the Mac | Command sent again after `Ok` | Hub check that takes the pass | What then happens |
|---|---|---|---|---|
| 0 `SetGrants { device_id, grants }` | the Mac **as host** (D7): Permissões "Entrar sem perguntar", any widening while `UNATTENDED` is involved, and the incoming card's "Deixar entrar sem perguntar" | `SetGrants` (followup built) | `set_grants` | the account's trust store gets the grants, `DevicesChanged`, audit as in 3.5. `ADMIN_TERMINAL` never comes here on a Mac host, because its row is inert (3.7) |
| 2 `OpenAdminTerminal { session_id }` | the Mac **as viewer**: `TermOpen { admin: true }` toward a host that granted `ADMIN_TERMINAL`, outside a session this Mac elevated (`ACCOUNTS.md` 8.3) | `TermOpen { admin: true }` (followup: N2) | `term_open` | the host starts its administrator terminal: a SYSTEM shell on a Windows host (F-TERM-02). In a session elevated through tag 3 it opens without a second dialog |
| 3 `ElevateSession { session_id }` | the Mac **as viewer**, in an unattended session not yet elevated: the session bar's or paused view's elevation button (`ACCOUNTS.md` 9) | UI op 50 (followup: N2) | op 50 | the viewer sends the wire `Elevate` (0x000C) to a minor-1 host. The host adds `UNATTENDED` (and `ADMIN_TERMINAL` if stored) |

- **Plain unattended entry asks nothing.** "Controlar" and "Assistir" toward a host that stored `UNATTENDED` enter without a prompt and without any dialog on the Mac. The connect-time action is gone (tag 1).
- **The Mac as host of tag 3.** A Mac host receiving `Elevate` is the business of `ACCOUNTS.md` and the 1.2 stage of D24, not of this note. No dialog appears on the host for it.
- **What the administrator terminal runs as on the Mac today:** nothing. When the Mac hosts, `MacPtySpawner` refuses every `admin` request with `PermissionDenied(Administrator)`, and its shell always runs as the app's own user, never root (D32). This note does not change that, and 3.7 makes the grant inert on the Permissões page to match.

## 5. Build plan

### 5.1 Already built on the shared side (read 2026-11-10)

| Piece | Where | Its tests (by name) |
|---|---|---|
| Ops 35 and 38, `ElevationTicket`, `ElevationOutcome`, role `Elevator`, `ElevAction` tag 0 | `swapdesk-ipc` | the ipc round-trip and redaction tests of `ELEVATION.md` slice 1 |
| Tickets, passes, `describe_elevation`, `confirm_elevation`, `set_elevation_ttls`, `serve_elevator`, `ConnInfo.can_elevate`, the pass in `set_grants` | `swapdesk-host/src/ui_hub/elevation.rs`, `ops.rs` | `tests/elevation.rs`: `a_confirmed_set_grants_ticket_lets_the_asking_connection_grant_unattended_once`, `a_ticket_is_single_use_and_ends_after_its_ttl`, `a_connection_that_is_not_elevated_can_neither_describe_nor_confirm`, `an_account_that_is_not_an_administrator_gets_no_ticket`, `a_ui_of_another_session_gets_no_ticket`, `a_ticket_is_refused_for_a_change_the_command_would_refuse`, `closing_the_asking_connection_drops_its_ticket_and_passes`, `a_new_begin_replaces_the_connections_open_ticket`, `the_sixteenth_open_ticket_is_the_last`, `the_trace_names_the_action_and_the_device_never_the_ticket`, `a_ui_connection_cannot_answer_for_a_ticket`, and the three `serve_elevator` tests |
| `Elevator`, `ElevatorRun`, `set_elevator`, the worker (Begin, `run` with `RUN_FOR` 110 s, End, `ElevationResult` then `Done`), `outcome_result` | `swapdesk-ui/src/service_backend/elevation.rs` | the backend tests of `ELEVATION.md` slice 4 |
| The resend of `SetGrants` after `Ok`; a failed `Elevate` reply becomes `Failed(code)`; the lines `ElevNotAdmin` and `ElevTimedOut`; `elevation_action_text` as a free function | `swapdesk-ui` `state.rs`, `interact.rs`, `strings.rs` | `state_tests.rs` (the failure-line cases) |
| `elevated_client::run` (Windows' instance; not used on the Mac) | `swapdesk-ui/src/elevated_client.rs` | the crossdrive loop of `ELEVATION.md` slice 5 |

### 5.2 What is left for the Mac, in order

Each slice ends on its tests run on a Mac, with clippy green there. The pure tests also run in the Windows suite.

| Slice | Files | Proven by (test names) | Needs a person |
|---|---|---|---|
| M1 binding | `swapdesk-sys-mac/Cargo.toml` (`objc2-security`, features `std`, `Authorization`, `AuthorizationTags`, macOS target; `Cargo.lock` by the main session), `src/admin.rs`, `src/lib.rs` | `admin::tests::every_authorization_status_maps_to_one_outcome` (pure: 0, -60005, -60006, -60007, -60008, other); `admin::tests::the_main_thread_is_refused_without_asking` (pure `refuse_on_main`); `admin::tests::the_right_asked_for_is_system_privilege_admin` (the item's name bytes); macOS: `admin::tests::a_check_without_interaction_is_never_approved` (`check_administrator_silently()` over SSH: `NoInteraction` or `Denied`, never `Approved`, within 5 s) | no |
| M2 the Mac's elevator | `swapdesk-app-mac/src/elevate.rs`, `src/lib.rs` | pure: `elevate::tests::every_confirmation_maps_to_its_run` (`run_of`, each row of 3.5); `elevate::tests::the_approver_is_the_asker_as_administrator_and_nothing_else`. Against a real `UiHub` (the `InProcessService` of `service.rs` tests, short TTLs through `set_elevation_ttls`), a scripted `AdminConfirm` and a ticket from op 35: `elevate::tests::an_approval_confirms_the_ticket_and_the_grant_passes_once`; `elevate::tests::a_cancel_confirms_nothing_and_ends_refused`; `elevate::tests::a_denial_confirms_nothing_and_ends_not_permitted`; `elevate::tests::an_answer_after_the_deadline_confirms_nothing` (the scripted confirm blocks past a short deadline and then approves: no pass on the connection); `elevate::tests::a_second_run_while_a_dialog_is_open_is_busy`; `elevate::tests::the_hub_sees_no_approver_before_the_approval` (the scripted confirm checks that a resend still gets `NeedsAdmin` while it is asking) | no |
| M3 wiring | `swapdesk-app-mac/src/service.rs` (`can_elevate: true` on the app connection; `ServiceParams.admin_confirm: Option<Arc<dyn AdminConfirm>>`; `backend.set_elevator(MacElevator::new(hub.clone(), info.clone(), confirm))`), `src/app.rs` (`SystemAdminPrompt` on a normal run; none on `--selftest`, `--drive-host`, `--mock`) | `service::tests::the_app_connection_can_elevate_and_is_not_admin`; `service::tests::continuar_asks_the_system_and_the_unattended_grant_passes_once`: `UiState` + `ServiceBackend` + `InProcessService` + scripted approval. Switch, `NeedsAdmin`, step, "Continuar", `ElevationResult Ok`, resend, grant stored, `DevicesChanged`; then a second widening is `NeedsAdmin` again. `service::tests::a_cancelled_system_dialog_shows_the_failure_and_changes_nothing` | no |
| M4 the prompt text (after N1) | `elevate.rs` | `elevate::tests::the_steps_line_is_what_the_system_prompt_gets` (the scripted confirm records `line`) | no |
| M5 the inert row (3.7) | `swapdesk-ui` `interact.rs`, `strings.rs`, `mock.rs` (a Mac-host scenario); `swapdesk-paint` `view.rs`, `screens_main.rs` (main session) | `state_tests::the_admin_terminal_switch_is_off_and_inert_on_a_mac_host` (disabled, off, the line as description, activation sends nothing); `state_tests::the_admin_terminal_switch_is_unchanged_on_a_windows_host`; paint: `a_disabled_switch_is_drawn_off_and_leaves_the_tab_order`; the Mac's `window_checks` and the accessibility comparison of the scenario | the canvas board (one capture, both systems) |
| M6 tags 2 and 3 on the Mac (after N2) | none beyond tests in `service.rs` | `service::tests::an_admin_terminal_from_the_mac_viewer_asks_the_system_once`; `service::tests::elevating_a_session_from_the_mac_viewer_asks_the_system` (two in-process services on loopback; the Mac side is the viewer) | no |
| M7 hand check | none | on the Mac, the real app, development bundle: (a) as an administrator: the dialog appears in front with focus, shows the line (or not: record it) and the app's name, the window keeps painting "Esperando…", and the grant passes; (b) as a standard user with an administrator's name and password: passes; with the user's own password: refused with the not-an-administrator line, counting the tries; (c) Cancel in the dialog: the failure page; (d) the dialog left alone for two minutes: `ElevTimedOut`, and a late approval changes nothing; (e) the probes `security authorizationdb read system.privilege.admin` and `... authenticate-admin-nonshared`, recorded here | **yes**: the password dialog |
| M8 documents | `THREAT-MODEL.md` (the text in section 2; R14 row), `CONTRACTS.md` C2.3 (macOS: "1.x in process: `system.privilege.admin`, then `confirm_elevation` in process; daemon: external form of `com.swapdesk.admin`"), `SPEC.md` R14, a decision in `DECISIONS.md` (the Mac's 1.x check is in process and what it is worth), FLUXOS F-PERM-01 and F-PERM-03 proofs | `check_flows.py --check` exits 0 | no |

M1 to M3 make `SetGrants` work on the Mac and close the defect for D7. M4 waits on N1 and M6 on N2. M5 is independent and can go first.

Optional, only if the cross-machine harness should cover it: in dev-harness builds, an `AdminConfirm` scripted by the `--ui-script` file (approve or cancel), so the scripted app run drives switch, step and "Continuar" through the real window. Never in a release build.

## 6. Rejected options

- **`LocalAuthentication` (`deviceOwnerAuthentication`, `LARight`):** proves the logged-in user, not an administrator; a standard user passes it.
- **Group membership (`getgrouplist`/`dsmemberutil`, rule `is-admin`):** membership alone is not consent and SPEC forbids it; a person at an administrator's unlocked session would pass without a password.
- **Our own password field checked with PAM, `dscl -authonly` or `kAuthorizationEnvironmentPassword`:** Swapdesk would handle an administrator's password; Apple's design keeps it in the Security Agent.
- **`AuthorizationExecuteWithPrivileges`:** deprecated since 10.7 and runs a tool as root, which nothing here needs.
- **`osascript` "do shell script ... with administrator privileges":** runs a command as root to prove a password, needs Apple Events, and shows a script prompt.
- **`sudo` in a terminal or a setuid helper:** not a dialog the person recognizes, and a setuid file is a root surface for a yes/no check.
- **A root daemon now (`SMAppService.daemon`, external form, `serve_elevator` over a socket):** the real fix, but D32 keeps it out of v1. It needs an administrator's approval in Login Items and the signing work of R13.
- **The custom right `com.swapdesk.admin` in 1.x:** the alternative of section 2. It writes the system policy database for no gain over a fresh reference with `destroyRights`.
- **Keeping the credential for the session (no `destroyRights`, one long-lived reference):** a second admin-only action within the right's timeout would pass without a dialog.
- **Setting `ConnInfo.admin = true` after a confirmation:** it would open every admin-only action for the rest of the app's life instead of one action once.
- **The first draft's own path: a backend wrapper (`Elevating`) and an `AdminPass` slot in `ConnInfo`:** replaced by the shared ticket, pass and worker, which already exist and behave alike on both systems.
- **Building the admin approver before the dialog to call `describe_elevation`:** an administrator's `ConnInfo` would exist before any administrator answered (N1 carries the text instead).
- **Calling `confirm_elevation` from the dialog's own thread:** a late answer after the deadline could then confirm. Only `run`, before its deadline, confirms.
- **`AuthorizationCopyRightsAsync`:** it works too, but its callback thread is unspecified; one named thread with the synchronous call is simpler to reason about and test.
- **Hiding the administrator-terminal row on the Mac:** the owner chose an inert row with a line, which keeps one layout on both systems.

## Still open

For the product owner:
1. **The not-an-administrator line** (`ElevNotAdmin`, wording under revision). Today it reads "Esta conta não é administradora deste computador. Entre com uma conta de administrador para fazer esta mudança." On the Mac it shows when the credentials typed were not an administrator's, and a standard account can approve with an administrator's name and password in the same dialog, so "sign in with an administrator account" is not the Mac's remedy. Recommendation: a wording true on both systems, for example "Quem confirmou não é administrador deste computador. Peça a um administrador para confirmar." / "Whoever confirmed is not an administrator of this computer. Ask an administrator to confirm."
2. **The English of the inert row:** "Not available on macOS in this version" (proposed, 3.7).
3. **Scope:** whether stage 57 also adds the harness-scripted confirmation for the scripted app run (optional).

For the shared side (not the owner's): N1 (the step's line into `Elevator::run`) and N2 (tags 2 and 3 with their followups), section 3.6.
