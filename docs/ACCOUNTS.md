# Design note: each account has its own Swapdesk (D44)

Status: design, 2026-11-10. Nothing here is built. It designs D44 (`docs/DECISIONS.md`): each account of a computer has its own Swapdesk data (pairings, the permissions in both directions, settings, activity), on every system, and nothing is shared between accounts. The points still open come back to the owner in section 19. **UNPROVEN** and **GUESS** as in `docs/SPEC.md`. Facts about Windows, macOS and the libraries were read on the pages listed in section 16, on 2026-11-10.

**Decided by the owner, 2026-11-10.**
1. **Nobody inherits the 1.0 data** (section 10). Every account starts empty; each paired computer forgets this computer and pairs again with the account it wants. The 1.0 files stay on disk untouched and are never served by 1.1. There is no inheritance, no claim card and no write-through.
2. **`UNATTENDED` for a standard account** is allowed when an administrator approves it at the UAC prompt in that account's session, and the approver is recorded (8.2).
3. **A confirmation on the viewer's machine** guards the SYSTEM-level reach on a peer. It is the system's own: the ticket and pass of `ELEVATION.md`, which is UAC on Windows and the administrator password dialog on the Mac. It comes before `Elevate` and before the administrator terminal outside an elevated session. Plain unattended entry asks nothing (8.3).
4. **The name a peer sees** is "<computer name> · <Windows account name>", editable in Ajustes, with the line "Outros computadores da rede veem este nome." (6). The default of a computer's first account is proposed in 6, for the owner to confirm.

**Changes after review (2026-11-10).**
- 5.2 and 14.2: `ip:port` by address in production, and a per-account `peers.bin` with the last seen port of each paired device.
- 9.3: a 1.1 viewer asks for `UNATTENDED` only toward a host known to be minor 1 or newer, with the rule for a disagreeing `HelloAck`.
- 5.1: a fence at every desktop or owner change, so no frame or input crosses accounts.
- 8.3: the gate against code running as the user, now decided.
- 5.5: port squatting between local accounts.
- 5.6: smaller gaps settled.
- 10: rewritten for decision 1. The inheritance rules, `ClaimLegacy`, op 51, `legacy-sync.bin` and the write-through are gone.
- 15: slices and dependencies updated. 17: the final set of elevation actions. 19: only the points still open.

Sources in the repository: `docs/SPEC.md` (F-PERM-*, F-SESS-*, trust boundaries, threats), `docs/PROTOCOL.md` (1, 2, 3, 5, 6), `docs/CONTRACTS.md` (C2.1 to C2.4, C5), `docs/THREAT-MODEL.md`, `docs/DECISIONS.md` (D7, D10, D11, D13, D24, D31, D32, D38), and the two unbuilt notes `docs/ELEVATION.md` (with `docs/ELEVATION-MAC.md`) and `docs/GRANTS-SYNC.md`, whose required changes are in section 17.

## 1. The design in brief

1. **One identity per account.** Each account that uses Swapdesk has its own device key, trust store, settings and activity log. A peer pairs with "this computer, account X" and sees each account of a computer as a device of its own. A machine key shared by the accounts is rejected: the peer would hold one record for all of them and could not keep them apart (section 3).
2. **The service keeps every account's data** in a folder of its own under `%ProgramData%\Swapdesk\accounts\<SID>\`, protected by the folder rules of 1.0 and DPAPI machine scope. So it can serve an account whose user is logged in, locked, or not logged in (section 4). The interface of each account reaches only its own data: the service picks the account by the SID of the pipe's client.
3. **Each account listens on its own pair of ports** (a "slot": session port `47820 + 2k`, pairing port `47821 + 2k`) and announces its own DNS-SD record with that port.
   - The server knows which key to present before the handshake.
   - A 1.0 peer reaches every account through discovery, with no change on its side, and a 1.0 browser lists the accounts as separate devices.
   - Without discovery a 1.0 peer reaches only slot 0; a 1.1 peer types `address:port` and remembers each device's last port (5.2).
   - Picking the key by TLS server name does not work for 1.0 peers (5.2).
4. **An account hosts only its own console session.** A connection to account A is admitted when A owns the console (unlocked, locked, or at a UAC prompt). For a device holding `UNATTENDED` it is also admitted when nobody is logged in at the console (the sign-in screen).
   - Otherwise it is refused: `ACCOUNT_AWAY` for a minor-1 peer, `NOBODY_PRESENT` for 1.0.
   - A live session pauses when another account takes the console, and a fence keeps every frame and input from crossing.
   - D13 becomes a rule with no question: terminal and files only ever run as the account itself (section 5).
5. **D7 stays, D11 stays, D13 is replaced, and D10's account check falls** (section 8).
   - A standard account can hold `UNATTENDED` when an administrator approves it at the UAC prompt in that account's session ("over the shoulder"), with the approver recorded.
   - D10's other effect stays, narrower: the system's own confirmation on the viewer's machine before the SYSTEM-level reach on a peer (owner decisions 2 and 3).
6. **Unattended entry enters normally, then offers "elevate"** (the owner's idea, section 9). The session starts with the user's own input and pauses at secure desktops. After the viewer-side confirmation, one click sends a new `Elevate` message (protocol minor 1), and the host adds `UNATTENDED`, and a stored `ADMIN_TERMINAL`, to the live session with the existing `PermissionsChanged`. A 1.1 viewer asks for unattended entry only toward a host it knows to be minor 1 or newer, so a 1.0 host never sees a request the 1.0 interface did not send.
7. **The update from 1.0: nobody inherits** (owner decision 1, section 10). Every account starts empty with a new key. The 1.0 key and pairings stay on disk, never served or written by 1.1, so going back to 1.0 still works. Every peer forgets this computer and pairs again with the account it wants.
8. **Install:** 1.1 keeps the per-machine install only; "for one user" is a machine install that only one account opens. A per-user install without the service is a later option with listed limits (section 11).
9. **macOS** is already per account on disk (D31). It needs a free port slot when two accounts run the app, and must stop hosting while its session is not the active one (section 12).

## 2. What exists today (read in the code, 2026-11-10)

**Windows.**
- **Storage.** One identity and one trust store for the machine, kept by the SYSTEM service in `%ProgramData%\Swapdesk` (C5.2): `identity.bin` (DPAPI machine scope, entropy "swapdesk identity v1"), `trust.bin`, `settings.bin`, `audit-sessions.log`, `audit-network.log`, `trace.log`. The service creates the folder: owner SYSTEM, protected DACL (SYSTEM and Administrators full), moved aside when someone else pre-created it (`swapdesk-secret-win/src/folder.rs`).
- **One core, one hub.** One `ServiceCore` (`swapdesk-host/src/service/mod.rs`) built from one `ServiceConfig` (one `Identity`, one `TrustHandle`, one `session_port`, `config.rs`). One `UiHub` built from one `HubParams` (the same identity, trust, `MachineSettings`, presence and audit log; `ui_hub/mod.rs`).
- **Interfaces.** Each interactive session's interface talks to the service over `ui-<session id>`; the hub knows each connection's `ConnInfo { session_id, user: UserRef (SID and name), admin, can_elevate }`. Ops that change who may reach the machine, or make it reach out, are reserved to the console's interface (`ops.rs` `require_console`). Every account therefore sees and uses the same pairings, and a peer sees "the PC".
- **TLS.** Raw public keys (RFC 7250): the server presents the one device key from its resolver (`swapdesk-trust/src/tls.rs` `Resolver`), and the client always sends the fixed server name `SERVER_NAME = "swapdesk"` ("it carries no meaning").
- **Discovery.** One record: instance name = the key fingerprint, SRV port = the session port, TXT `name`, `os`, `fp`, `pair=1` (`swapdesk-lan`).
  - A browser assumes the pairing port is the session port + 1 (`ui_hub/presence.rs` `pair_port: p.port.wrapping_add(1)`).
  - It replaces any other record seen at one of the same addresses **and the same port** (`presence.rs` `found`).
  - A paired device not in discovery is dialled at its last stored address and the default port (`views.rs` `address_of`).
  - "Adicionar por endereço" accepts `ip:port` only in development mode (`ops.rs` `add_by_address`).
- **Host decisions.**
  - Unattended entry grants `stored ∩ requested` and includes `UNATTENDED` (`swapdesk-session/src/policy.rs` `unattended_grant`).
  - Input goes to the SYSTEM helper exactly when the session's grants hold `UNATTENDED` (`service/routing.rs`), and a change of that bit moves the injector (`host_relay.rs`).
  - The host's banner can change a live session's grants through `HostService::set_session_grants`, which sends `PermissionsChanged` (`swapdesk-session/src/host.rs`).
  - One live hosted session per machine (`max_live_sessions = 1`).
  - The interface always sends `Connect { unattended: false }` (`swapdesk-ui` `interact.rs`).
- **Firewall.** The rules of the install script allow inbound UDP 47820 and 47821 (and 5353) for the service program only, Private and Domain profiles, remote LocalSubnet (`packaging/windows/install.ps1`).

**macOS.** The app runs the core and the hub in its own process (D27, D32, `swapdesk-app-mac/src/service.rs` `InProcessService`).
- Identity, trust store and settings are files in `~/Library/Application Support/Swapdesk/secrets` (mode 0600 in a 0700 folder, D31, `swapdesk-secret-mac/src/location.rs`); the audit and trace logs are in `~/Library/Application Support/Swapdesk` (`store.dir()`).
- **The Mac is already per account on disk**: two accounts of one Mac each have their own key, pairings and log.
- Two gaps: both accounts' apps try to bind 47820 and 47821, and the in-process hub treats its one interface as always at the console (`set_console_session(Some(UI_SESSION))`), with the user recorded as `UserRef { id: "local-user", .. }`.

## 3. The model

### 3.1 Options

**(A) One identity per account (chosen).** Account X of computer M has its own key `K_MX`, its own trust store `T_MX`, its own settings and log. A peer P pairs with `K_MX`. If account Y of M also pairs with P, P holds a second record, `K_MY`, with its own name and its own grants.

**(B) One machine identity, a trust store split per account.** M keeps one key `K_M`; each account has its own list of pinned peers and grants. P pins `K_M` once.

### 3.2 What a peer sees, and why a shared key mixes accounts at the peer

| | (A) per account | (B) one machine key |
|---|---|---|
| P's device list | one entry per account of M that paired with P ("Office PC · ana", "Office PC · rui") | one entry, "Office PC" |
| P's grants to M | per account: what P lets M-ana do is not what P lets M-rui do | one set for the key: whatever P granted "Office PC" applies to whichever account of M dials with `K_M` |
| A second account of M pairs with P | a new record on P, a new code comparison | refused: P already pins `K_M` (`ALREADY_PAIRED`, PROTOCOL 3 step 0), or a re-pair silently reuses the first account's record and grants |
| P dials M | P names the account by the key it expects; M presents that account's key | M presents `K_M`; P cannot say which account it means, and M must guess (several accounts may pin P) |
| M-ana forgets P | P's record of `K_Mana` stops working; `K_Mrui` is unaffected | P still pins `K_M`, which M-rui still uses; P cannot tell "ana forgot you" apart from "rui did" |
| A 1.0 peer | works unchanged per account (5.3) | cannot name the account at all |

The peer makes its own decisions about a key: its grants, its `UNATTENDED`, its "forget". With (B) the same key stands for several people. A grant the peer's user gave while thinking of one account is then usable by every account of M that the service lets dial with that key.
- M can refuse to dial for an account that did not pair, but the peer cannot check that.
- As host, M must pick an account for a key that several accounts pin.

That is exactly the mixing D44 forbids, moved to the other machine. (A) keeps the rule on both sides with the checks that already exist: the pinned key is the account. It costs one key and one endpoint per account (5.2); nothing in pairing or TLS changes.

## 4. Windows storage

### 4.1 Where each account's data lives

| Item | Path under `%ProgramData%\Swapdesk\` | Format |
|---|---|---|
| Machine index (new) | `accounts.bin` | `AccountsIndex` (14.2), DPAPI machine scope like every other file |
| Machine settings | `settings.bin` (1.0 file, kept) | `MachineSettings` format 1, unchanged; 1.1 only reads it (10.2) |
| Trace log | `trace.log` (kept) | unchanged; machine-wide, administrators only (C2.3) |
| 1.0 data | `identity.bin`, `trust.bin`, `audit-*.log` (kept) | never opened, served or written by 1.1 (10.2) |
| Account folder (new) | `accounts\<SID>\` | the SID string, checked to be `S-1-` followed by digits and dashes only, at most 184 bytes (the `str(184)` of C2.4), so it can never name another path |
| Account identity | `accounts\<SID>\identity.bin` | `Identity` format 1, unchanged; DPAPI machine scope, the same entropy |
| Account trust store | `accounts\<SID>\trust.bin` | `TrustStore` format 1, unchanged (pairings, grants both ways, `unattended_granted_by`, lockout, blocked addresses) |
| Account settings (new) | `accounts\<SID>\account.bin` | `AccountSettings` format 1 (14.2) |
| Account activity | `accounts\<SID>\audit-sessions.log`, `audit-network.log` (and their rotated `.1` copies) | `AuditEntry` as 1.0, one new event tag (14.2) |
| Last seen ports (new) | `accounts\<SID>\peers.bin` | `PeerPorts` format 1 (14.2): the session port each paired device was last seen on (5.2) |

The service makes `accounts\` and every `<SID>` folder with the 1.0 folder rules: owner SYSTEM, protected DACL, opened without following reparse points, moved aside when they exist with another owner or as a reparse point (`ensure_folder`). A standard user cannot create anything inside `%ProgramData%\Swapdesk` (its DACL grants only SYSTEM and Administrators), so nobody can plant a folder for another account.

An account's folder is made when that account first needs an identity, that is when it opens Swapdesk and pairs, never for accounts that never opened it. When an account is removed from Windows, the service stops serving a folder whose SID no longer resolves; its files stay until uninstall. GUESS: `LookupAccountSidW` fails with "none mapped"; UNPROVEN, proven by a test that deletes a scratch account in the owner's batch.

### 4.2 Why DPAPI machine scope and `%ProgramData%`, not the user's profile

The service must use an account's key when that account is not logged in: the sign-in screen after a restart is the classic unattended case, and a peer must also be told "away" by the right key.

- **User-scope DPAPI** ties decryption to the user's logon credentials: "Typically, only a user with logon credentials that match those of the user who encrypted the data can decrypt the data" (CryptProtectData, Remarks). With nobody logged in, the service holds no such credentials. This is a reading of the page, not tested; nothing here relies on user scope.
- **The user's profile.** Loading another user's profile needs that user's token, "returned by the LogonUser, ... OpenProcessToken, or OpenThreadToken function" (LoadUserProfileW). With nobody logged in there is no process or thread of that user to take it from, and `LogonUser` needs the password. And "If a service or an application impersonates a user, the system does not load the user's profile".
- **So the data lives where the service always reaches it**, under the 1.0 folder. Machine scope means "any user on the computer where the encryption was done can decrypt the data" (CryptProtectData). That is why C5.2 already says the **DACL is the only protection**, and it stays so.

### 4.3 Who can read or change an account's data

| Who | Through Swapdesk | On disk |
|---|---|---|
| The account itself | everything of its own, through the service (its interface reads no file, as in 1.0) | nothing directly (the DACL does not list the user) |
| Another standard account | nothing: its interface reaches only its own account (7.1) | nothing (DACL) |
| An administrator account | nothing of another account: it cannot list, read, change, forget or use another account's pairings, grants, settings or activity. At the UAC prompt in that account's session it can approve an action the account itself asked for (8.2), and it is recorded as the approver. Uninstalling removes every account's data | readable: the DACL lists Administrators, as in 1.0 |
| SYSTEM (the service) | everything | everything |

Administrators keep their 1.0 entry in the DACL. A SYSTEM-only DACL would not stop them: administrators "are considered part of the Trusted Computing Base" (Microsoft servicing criteria, quoted in `ELEVATION.md` section 6) and can take ownership. It would also complicate uninstall and support. The note says so plainly instead: the product gives an administrator no way into another account's Swapdesk data, and the disk does not stop one who uses Windows' own tools. This stays out of scope, as THREAT-MODEL "Out of scope" already says for an attacker with administrator rights.

## 5. Hosting

### 5.1 Which account answers

The peer chooses the account: it dials the account's endpoint and expects that account's key (5.2). The only question is whether that account may host **now**. The **console owner** is the user of the session attached to the physical console, or nobody (the sign-in screen). The service already follows it (`SERVICE_CONTROL_SESSIONCHANGE`, C2.1) and already takes the console user's token for the user helper.

Rule: a host session of account A is admitted, and keeps its picture and input, only while the console owner is A, or while the owner is nobody and the session entered through `UNATTENDED`.

| Console state | A peer of the console account A | A peer of another account B |
|---|---|---|
| A logged in, unlocked | as 1.0: prompt to A's interface, or unattended entry | refused: `ACCOUNT_AWAY` (minor 1) or `NOBODY_PRESENT` (1.0) |
| A locked | prompted: `NOBODY_PRESENT` (6.2: presence needs an unlocked session, as 1.0); unattended: enters, and sees A's lock screen after the elevation step (section 9) | refused as above; B's peer never sees A's lock screen (it shows A's name, picture and notifications) |
| Sign-in screen, nobody logged in (after a restart, or after "switch user" leaves the console to the sign-in screen) | prompted: `NOBODY_PRESENT`; unattended: enters (sign-in screen after the elevation step) | the same: the sign-in screen belongs to no account |
| Fast user switching: A at the console, B's session disconnected | as the first row | refused as above. Swapdesk never connects the console to B's session: that would push A off the screen |
| A remote (non-console) Windows session of B, console owner A or nobody | as the rows above | B is not at the console: refused, or the sign-in screen if the owner is nobody and the entry is unattended. Swapdesk captures only the console session (C2.1) |

**During a session**, the console owner may change to someone other than the session's account: a person signs in as another account at the keyboard, or the peer itself types another account's password at the sign-in screen. Then:
- The service stops routing that console's picture and input to the session. The helpers restart in the new console session (C2.1) and are attached only to the core that may host (15, S4).
- The host sends `HostState { desktop: AccountAway, paused: true }` to a minor-1 viewer, or `{ desktop: Locked, paused: true }` to a 1.0 viewer, whose interface already shows a paused state for it. `AccountAway` says only that this account is not at the console, never whether another account is.
- If the console comes back within 60 s (GUESS) to the session's account, or to nobody for an unattended session, the session continues with a keyframe. Otherwise it ends with `Bye { ACCOUNT_AWAY }` (minor 1) or `Bye { NOBODY_PRESENT }` (1.0).

**The order at a change, so nothing crosses (the fence).**
- **Where it can cross.** A helper captures and injects only in its own Windows session, so a switch to another session (fast user switching, "switch user") cannot show the new account's desktop to the old helpers.
  - The window is a sign-in **inside the same session**. At the sign-in screen after a restart the console session has no user and the SYSTEM helper runs there for an unattended session; a sign-in turns that same session's desktop into the new account's.
  - Until the service hears of it (`SERVICE_CONTROL_SESSIONCHANGE`, or the helper's `DesktopChanged`), nothing orders "stop" before "next frame": the capture thread re-acquires the new desktop by itself (C2.1 "Switches"), and the switch hook runs on another thread.
- **The rule.** After **every** desktop switch the system helper holds its capture and its input until the service lets it go on:
  1. The capture thread, on `DXGI_ERROR_ACCESS_LOST`, sends `CaptureHeld { generation }` (0x3A) itself, on the same pipe and so ahead of any frame of the new desktop, and captures nothing more. (Built: a dedicated message rather than `DesktopChanged`, which other paths also send.)
  2. The input thread drops every input event from the moment it follows a new input desktop.
  3. The service closes the session's gate, so frames and input of the old generation are dropped. It then reads the user of the console session at that moment (the token or the session information it already takes for the user helper).
  4. It sends a new `ResumeCapture { generation }` (service to helper, 0x2A, local IPC) only when that user is the session's account, or nobody for an unattended session at the sign-in screen.
  5. If no user is readable yet on a `Default` desktop, it stays closed and asks again every 200 ms for 10 s (GUESS), then pauses the session as above. It fails closed.
  - As built (after the review of S3 and S4, 11/11): the service lets a helper go on only when both the console owner it keeps (changed when it hears of a logon) and a fresh read of the console session's user, made at that moment, allow it, since the logon notice may come after the switch. A logged-in user whose SID cannot be read counts as another account, in the fresh read and in the kept owner (read again every second until it can be). The service also drops video, cursor and audio from a held helper. The sign-in screen, with nobody at the console, is `Locked` to the account's sessions, so a session that entered with a prompt pauses there.
- **Cost.** One round trip to the service at each desktop switch (lock, UAC, the Ctrl+Alt+Del screen). Within one account it adds a few milliseconds (GUESS) before the keyframe that 1.0 already sends.
- **Proof.**
  - `a_desktop_switch_holds_frames_and_input_until_the_service_resumes`: the helper, with a scripted `AccessLost` on the C7 synthetic source.
  - `a_sign_in_at_the_sign_in_screen_never_routes_a_frame_or_an_input_of_the_new_account`: the service with a scripted helper and a scripted console owner. The helper reports `CaptureHeld`; while another account owns the console no `ResumeCapture` is sent (and the service drops video and cursor of a held helper); once the account owns it again the helper is let go on. The helper side (no frame and no injected input while held) is the test above; the pause after 10 s reuses `AccountAway`, proven by `when_another_account_takes_the_console_the_session_pauses_then_ends`.
  - By hand at the sign-in screen (owner batch): the viewer's frame counter and the host's trace line "fenced" across a sign-in at the keyboard.

So a device paired with account A can type another account's password at the sign-in screen, as a person at the keyboard can, but never sees or controls that account's desktop.

### 5.2 How the service presents the right identity

TLS 1.3 fixes the order: the server sends its `{Certificate}` in its first flight, and the client sends its own only after the server's `Finished` (RFC 8446, Figure 1). So the server must choose which account's key to present before it knows who the client is. There are three ways.

**(P) A port pair per account (recommended).** Account slot `k` (0 to 31, GUESS) listens on session port `base + 2k` and pairing port `base + 2k + 1`, where `base` is `settings.bin` `port` (47820). Each endpoint has its own `ServerConfig`, with that account's key in the resolver and that account's trust store in the verifier: the 1.0 configuration per account, unchanged.
- **1.0 peers.** A 1.0 peer dials the SRV port of the record it chose and reaches that account. Its pairing assumption (session port + 1) holds, and its browser keeps every record because they differ by port (`found`). Every account is reachable from 1.0 in both roles.
- **The verifier** checks the account's own store, so a key pinned only by another account fails in the handshake, before the post-handshake gate. The gate (PROTOCOL 2.4) still re-checks it.
- **Cost.** One more socket per approved address per active account, two while its pairing mode is open. The firewall rule moves from "47820, 47821" to the range "47820-47883". `New-NetFirewallRule -LocalPort` takes ranges, as in its own example `-LocalPort 12345,5000-5020`. That is an install script change and the owner's UAC click.
- **Reaching an account on a slot other than 0 without discovery.**
  - *Typed address.* 1.1 accepts `ip:port` in production as well as in development. The LAN guard judges the IP as now, the port may be any of 1 to 65535, and pairing by address dials that port + 1 (`Manual::pair_port`). The Início network line of an account whose slot is not 0 shows its address as `address:port`.
  - *A 1.0 peer* cannot type a port and pairs by address on 47821, which is slot 0. It reaches another slot only through discovery, which every 1.0 build has. This limit goes in the README.
  - *The fallback dial of a paired device missing from discovery* uses the last stored address and the default port today, and the trust record has no port. Adding one would make trust format 2, which a 1.0 build refuses. Instead each account keeps a side file, `peers.bin` (14.2), with the device ID, the last session port and when it was seen.
    - It is written when discovery shows a paired device on a port other than the stored one, or when a session or sync connects on it, at most once a minute per device.
    - `address_of` uses it before the default port. 1.0 never reads it.
    - A dial at a stale port fails at the key check with no harm.
- **A port held by another program or another local user** leaves that account unable to receive connections: 5.5.

**(S) One port, the key chosen by the TLS server name.** A 1.1 client would send the target account's fingerprint as the server name, and the resolver would read `ClientHello::server_name()` ("Returns `None` if the client did not supply a SNI", rustls 0.23.46). Rejected:
- Every 1.0 client sends the fixed name `swapdesk` (`tls.rs` `SERVER_NAME`; rustls sends SNI by default, `enable_sni` "The default is true"). A 1.0 peer could therefore reach only one account of the machine.
- A 1.0 browser would see several records at one address and port and keep only the last one it heard (`found`).
- The session verifier cannot see the server name.

**(O) Choose after the handshake** (one key for the machine, then the account named in `Hello`). This is model (B), rejected in section 3.

### 5.3 What a 1.0 peer sends and gets

- **To a 1.1 machine:** the same bytes as from a 1.0 machine.
  - It dials the record's port and gets that account's key. Its `Hello` and `HelloAck` carry minor 0, and the host never sends it a minor-1 type or value.
  - Refusals use `NOBODY_PRESENT`, and the pause uses `Desktop::Locked`.
  - The 1.0 interface never asks for unattended entry. A minor-0 client that did ask would get the 1.0 grant with `UNATTENDED` at once, since it has no elevation button.
- **From a 1.1 machine:** the 1.1 account presents its own key as the client certificate, and the 1.0 host accepts it only if it pinned that account's key.
  - A 1.0 host that paired with this computer before the update pinned the 1.0 key, which 1.1 never presents. It refuses the 1.1 account until its user pairs again (10.3).

### 5.4 D13 after the change

D13 asked the logged-in user before an unattended session opened the terminal or files of a user other than the one who granted `UNATTENDED`. With the rule of 5.1, a session of account A only ever runs while the console owner is A or nobody. The "logged-in user" is therefore A, the grantor acted within A's own store, and the question can no longer arise.

**D13 is replaced** by this rule: a terminal or a file operation of a host session runs only as the session's account. With nobody logged in it is refused (`UNAVAILABLE`), except the administrator terminal (F-TERM-02).
- The host-side consent code stays only as a guard that refuses and never asks: `consent_need` returns "refuse" if the console user ever differs from the account.
- `ConsentPending` (0x000A) stays in the protocol, and the viewer still handles it from a 1.0 host.
- `unattended_granted_by` keeps its place in the record (format 1), holding the account's own reference.

### 5.5 Port squatting between local accounts

- **The gap.** The service binds each approved address with a plain `UdpSocket::bind` (`swapdesk-lan` `socket.rs`). Microsoft's socket security page (16, fact 8) says, for binds made under two different user accounts:
  - A socket bound first on the wildcard address makes a later bind of the same port on a specific address fail (`WSAEACCES`).
  - A wildcard bind by another user **after** the service's specific bind succeeds, and takes the port on every other address.

  So any local user can hold a slot's port before the service binds it: when a slot is first allocated, or after an address change, since the service binds the new address then. That account then cannot receive connections.
  - The address-change case already exists in 1.0 for 47820.
  - It is a denial of service only: a squatter receives nothing a peer would accept, because the peer checks the key.
- **Recommended (GUESS until probed):** at start, before any user signs in, the service binds the two ports of slot 0 (always, even before any account exists) and of every allocated slot on the **wildcard** address with `SO_EXCLUSIVEADDRUSE`. Per the same page, after an exclusive wildcard bind no other account can bind that port on any address.
  - The guard keeps refusing any `Incoming` whose `local_ip()` is not an approved address (PROTOCOL 1), before the handshake. Outgoing packets keep TTL 1.
  - This replaces "an endpoint per approved address" with "one endpoint per slot port, filtered by the arrival address". That needs `Incoming::local_ip()` on a wildcard socket on Windows. Probed 2026-11-11 (`cargo run -p swapdesk-host --example probe_local_ip`): a connection from 127.0.0.1 to a QUIC endpoint on `0.0.0.0` reads `local_ip() = Some(127.0.0.1)`, so the recommended path is open.
  - A pairing port bound but not served answers nothing, as now.
- **Fallback if the probe fails:** keep the per-address binds, with `SO_EXCLUSIVEADDRUSE`.
  - When a bind fails, the service names the program holding the port (the owning process of the UDP table entry) in its trace log and in the account's Início, and moves the account to the next free slot.
  - Its record then announces the new port, and `peers.bin` on its peers follows.
  - Residual: a local user who holds every free port can keep new accounts from receiving connections. That is a local denial of service, like filling the disk; accepted, with the program named.
- **Mac:** the same next-free-slot rule (section 12). The app binds its pair at start.

### 5.6 Smaller gaps, settled

- **Resume after Elevate.** `Elevate` changes the session's grants, and a grant change invalidates the resume token (PROTOCOL 6.4). After a lost link the reconnect enters again through `UNATTENDED`, unelevated, and the button shows again. Test: `a_reconnect_after_elevate_enters_again_unelevated`.
- **One hosted session per machine across cores.** Each core's `max_live_sessions = 1` commits under its own lock. The machine layer therefore holds one reservation, which `decide` takes atomically before any grant and which is released at the session's end; the core's own cap stays as a second check. Test: `two_unattended_entries_at_the_sign_in_screen_at_once_admit_exactly_one`.
- **Caps.**
  - The handshake caps (16 in flight, 4 per address, PROTOCOL 1) stay **per machine**: one limiter is shared by every account's endpoints.
  - "One pairing at a time" stays per machine, through a machine gate.
  - The pairing **lockout** becomes per account, since it lives in each trust store; pairing needs that account's user to open "Parear". PROTOCOL 3 says so.
  - The caps per session are unchanged.
- **The administrator terminal before Elevate.** An unattended entry to a minor-1 viewer also leaves out `ADMIN_TERMINAL`. `Elevate` adds both bits, each only if stored and requested. A prompted session keeps D8: `ADMIN_TERMINAL` only if stored, once the host's user has accepted.
- **`ACCOUNT_AWAY` toward 1.0, in every message.** The session's send path maps the code once, at encoding, for a minor-0 peer: 22 becomes `NOBODY_PRESENT` (5) in `SessionRefused`, `SwapDecline`, `Bye`, `Error`, `QuickActionResult`, `FileDecline` and `PairResult`. Test: `no_minor_1_error_code_reaches_a_minor_0_peer_in_any_message`.
- **Console-only ops.** `require_console` stays for everything that widens or reaches out. Ops that only narrow the account's own reach (`ForgetDevice`, a `SetGrants` that only removes bits, "receive connections" off) are allowed from any interactive session of the account's own user.
- **Undefined states.**
  - A deleted account keeps its slot while its folder exists, until uninstall.
  - A 33rd account gets no slot: it can view and start pairings as initiator, and Início says it cannot receive connections.
  - Role `Elevator` stays accepted on `ui-<session>` (same-user elevation, as built) and is the only role on the new `elev-<session>` (over the shoulder, 17.1).
  - A 1.0 uninstall after a downgrade leaves `accounts\` behind (10.5).

## 6. Discovery

- **One record per account**, never one per machine. The instance name is the account key's fingerprint and the SRV port is the account's session port. TXT is as 1.0 (`name`, `os`, `fp`, `pair=1` while that account's pairing mode is open), plus `minor` from `GRANTS-SYNC.md`. One mDNS daemon in the service registers them all: `swapdesk-lan` `Discovery` grows from one own record to a list.
- **When:** an account listens and is announced while it has at least one paired device or its pairing mode is open, whether its user is logged in or not. Its peers must reach it for unattended entry and for an "away" answer, and the grants sync of `GRANTS-SYNC.md` must find it.
  - An account that never paired neither listens nor appears, so the LAN does not learn how many accounts the computer has.
  - The account's own "receive connections" switch (7.2) also withdraws it.
- **What 1.0 browsers show:** one entry per announced account, each with its own name, as separate devices. Pairing from a 1.0 machine reaches the account whose entry the user chose, on that record's port + 1.
- **This machine's own accounts** are filtered out of its own "Na rede" list, and a peer cannot dial its own machine (the LAN guard refuses own addresses). Two accounts of one computer can therefore never pair with each other.
- **The name a peer sees (owner decision 4):** each account's "name on the network" in Ajustes, in place of 1.0's machine name. Its default is "<computer name> · <Windows account name>", editable, with the line "Outros computadores da rede veem este nome." / "Other computers on the network see this name." under the field.
  - **The first (or only) account of a computer: proposal, for the owner to confirm.** The first account to get an identity on a computer is named without the account part: the name in the 1.0 `settings.bin` when there is one (10.2), else the Windows computer name. Every later account gets "<computer name> · <Windows account name>". The first account keeps its name when others arrive, so nothing is renamed on its peers' lists.
    - After the update, a one-account computer therefore reappears under the very name its peers knew. Only its key is new, and they pair again with an entry they recognize.
    - The alternative is the suffix for every account, the first included: the same rule everywhere, at the cost of a rename the peers notice once.
  - The `·` and the Windows account name reveal that account name on the LAN, next to what mDNS already shows (THREAT-MODEL "Reading what mDNS announces": accepted). The line under the field says so.

## 7. The viewer side and settings

### 7.1 One hub per account

The service runs one hub and one core per account (15, S3). The pipe server reads the client's SID (it already does, through `ImpersonateNamedPipeClient`, C2.1) and hands the connection to that account's hub.
- A hub holds only its account's identity, store, settings, log and core. No op of account A can return or change B's data, even through a bug in an op handler: the reference does not exist in A's hub.
- Each account's interface lists only its own pairings, dials only with its own key, and sees only its own activity.
- The console rule of 1.0 stays for everything that widens or reaches out. Ops that only narrow the account's own reach are also accepted from its user's other sessions (5.6).

### 7.2 Per account or per machine

| Setting (1.0 place) | 1.1 | Why |
|---|---|---|
| Computer's name (`MachineSettings.name`) | per account, the "name on the network" (6) | it is what a peer sees of the account |
| Service on or off (`service_enabled`) | per account: "receive connections" (the account's endpoint closes and its record is withdrawn); starts on. 1.0's value is not read, since in 1.0 it only wrote a trace line (`swapdesk-service/src/app.rs`) | one account must not cut another's access. Stopping the whole service stays a Windows act of an administrator |
| Lock on end (`lock_on_end`) | per account, starts off (the 1.0 default) | it acts on that account's own console session at the end of its own sessions |
| Keep awake (`keep_awake`) | per account, starts on (the 1.0 default, `MachineSettings::new`) | it holds the machine awake only during that account's sessions |
| Ports (`port`, `pair_port`) | per machine, an administrator's; read from the 1.0 `settings.bin` | the base of every slot, and the firewall range |
| UI preferences (`ui.bin`) | per account already (`%APPDATA%`, C5.1) | unchanged |
| Pairing lockout, blocked addresses (`TrustStore`) | per account | each account's pairing endpoint has its own lockout; a block by one account does not silence another's peers |

The interface's settings op (C2.4 op 20) keeps its body; its four fields now mean the account's own values.

## 8. D7, D10, D11 and D13 after the change

### 8.1 Decision by decision

| Decision | Threat it covered | After D44 | What covers the threat now |
|---|---|---|---|
| **D7** granting `UNATTENDED` or `ADMIN_TERMINAL` needs a local administrator | a standard user gives a remote device machine-level reach: SYSTEM shell (F-TERM-02), the sign-in screen, input into elevated windows and UAC | **stays, both bits**. `ADMIN_TERMINAL` is SYSTEM. `UNATTENDED` still reaches the sign-in screen (the list of accounts, logon attempts, restart) and SYSTEM input | the administrator's approval at UAC, also over the shoulder for a standard account, with the approver recorded (8.2; owner decision 2) |
| **D10** any account may connect with the grants other machines gave this machine; using `UNATTENDED` or `ADMIN_TERMINAL` on another machine needs a local administrator here | (1) another account of the viewer machine uses grants another machine gave "this machine"; (2) as a side effect, code running as the user could not reach unattended entry or the administrator terminal on a peer without a UAC approval | **(1) falls**: the other machine's grants are given to an account's key, and only that account dials with it. **(2) stays, narrower** (8.3, owner decision 3): the system's confirmation on this machine before `Elevate` and before the administrator terminal outside an elevated session. Plain unattended entry asks nothing | (1) the per-account key (section 3); (2) the ticket and pass of `ELEVATION.md` on the viewer's machine |
| **D11** prompted sessions inject with the user's token; elevated windows and UAC only in `UNATTENDED` sessions | input into elevated windows and UAC by a device that was only accepted, not trusted for unattended access | **stays**, refined: the SYSTEM helper injects only for a session holding `UNATTENDED` **after the elevation step** (minor-1 viewers), or at entry (minor 0), section 9 | the session's `UNATTENDED` bit, as now (`routing.rs`) |
| **D13** the terminal and files of a user other than the grantor ask that user | an unattended session reaches another user's terminal or files | **replaced** (5.4): no other user's session is ever reachable | the console rule of 5.1 |

### 8.2 `UNATTENDED` from a standard account (owner decision 2)

The reach of `UNATTENDED` after D44, for a device of account A:
- **A's own session:** entry without a prompt, A's lock screen, and UAC prompts in A's session. For a standard A, a UAC prompt asks for an administrator's credentials, so controlling it gives nothing without a password the peer must already know.
- **The sign-in screen with nobody logged in:** machine-level. The peer sees the account list, can try passwords (Windows' own account lockout policy applies), can restart, and can sign in to any account whose password it knows. With 5.1, that sign-in does not give it that account's desktop.
- **SYSTEM input in A's session:** for a standard A, elevated windows exist in A's session only after an administrator typed credentials there.

D7 therefore stays. The 1.0 workaround ("an administrator signs in to their own account and grants it there") no longer reaches A's store, so the UAC prompt in A's session may be answered by an administrator typing their credentials:
- The elevated instance runs as the administrator Z in A's Windows session. It confirms A's ticket over a separate elevator pipe that admits administrators (`elev-<session id>`, 17.1).
- The pass lands on A's connection, and the grant is written to A's store with `unattended_granted_by` = A.
- The audit entry records the approver: `GrantsChanged.by` = Z. The `UserRef` is already in the format.

### 8.3 The gate against code running as the user (owner decision 3)

Without any check on the viewer's machine, any process of the account could open the UI pipe as that account, malware at medium integrity included. It could send `Connect` with unattended entry, op 50 `ElevateSession` and `TermOpen { admin: true }`, and get a SYSTEM shell on every peer that granted this account `ADMIN_TERMINAL`, with no prompt on either machine. The asset is SYSTEM on **another** machine, which that machine's administrator lent to a person, not to whatever runs in the person's session.

**Decided: the system's own confirmation (option V1).** The ticket and pass of `ELEVATION.md` run on the viewer's machine before:
- `ElevateSession` (op 50), and
- `TermOpen { admin: true }` outside a session that this machine elevated (`OpenAdminTerminal`). An elevated session opens the administrator terminal without a second prompt.

| | Windows | macOS (1.x, in-process service) |
|---|---|---|
| What asks | UAC: an administrator account answers with one click on the secure desktop; a standard account needs an administrator's credentials | the administrator password dialog (`system.privilege.admin`, `ELEVATION-MAC.md`) |
| Cost | one prompt per `Elevate` and per administrator terminal outside an elevated session; nothing for plain unattended entry | the same |
| What THREAT-MODEL "same user" then says | code running as the user can control a peer's desktop as the peer's user through a stored unattended grant, but cannot reach SYSTEM on a peer (the sign-in screen, UAC, the administrator terminal) without a person's approval at UAC | the dialog guards against a person at the keyboard, not against code in the app's own process (the `ELEVATION-MAC.md` text), until the root daemon |

Standard accounts need an administrator of their own machine at each such step, as D10 asked in 1.0. A later improvement for them is a user-presence check (Windows Hello consent); whether code at medium integrity can satisfy it is UNPROVEN and out of 1.1.

## 9. Unattended entry with an elevation step

The owner's idea: "enter-without-asking enters normally and then offers a button to request the elevation; if it is not used, the session is connected and does everything without the elevation". It replaces the connect-time administrator check of D10, and the SYSTEM-level step keeps the system's confirmation on the viewer's machine (8.3).

### 9.1 What a session has

| | Before the elevation | After the elevation |
|---|---|---|
| Entry | without a prompt, `GrantMode::Unattended`, `granted` = stored ∩ requested **without** `UNATTENDED` and `ADMIN_TERMINAL` | the same session, `PermissionsChanged { granted ∪ UNATTENDED ∪ (ADMIN_TERMINAL if stored and requested) }` |
| Input on A's normal desktop | the user helper, A's medium-integrity token (D11): no elevated window, no UAC | the SYSTEM helper (`routing.rs` follows the bit) |
| Secure desktops (UAC, A's lock screen, the sign-in screen) | paused: video stops and input is dropped, the 1.0 rule for a session without `UNATTENDED` (`HostState { paused: true }`). The viewer's paused view offers the button | seen and controlled |
| Ctrl+Alt+Del (`SendSpecial`) | `NOT_PERMITTED` (it needs `UNATTENDED`) | allowed |
| Screen, audio, files, terminal as A, clipboard, chat, swap, quick actions | as stored and requested | unchanged |
| Administrator terminal | not available (5.6) | available without a second confirmation (8.3) |
| Resume after a lost link | `Resumed`, the original grants ∩ stored (PROTOCOL 6.4) | the elevation invalidated the token: the reconnect enters again, unelevated (5.6) |
| Swap | never elevated (6.3) | never elevated |

An entry that finds the sign-in screen (the restart case) starts paused, and one click (after the confirmation) shows it. Whether such an entry should elevate by itself is open (19).

### 9.2 What the button asks, and from whom

| Option | What happens | Cost and worth |
|---|---|---|
| **(E1) The system's confirmation on the viewer's machine, then a mode switch on the host (chosen with decision 3)** | the viewer's user confirms (8.3), then the viewer sends `Elevate`. The host checks that the device still holds `UNATTENDED` in A's store and that the session entered as `Unattended`, adds the bits, writes `Elevated` to A's activity and shows "Controle elevado" on its banner | the reach was approved by an administrator of the host when `UNATTENDED` was granted (D7). The confirmation keeps code running as the user from using it, and the host shows and logs it. One prompt on the viewer, one message |
| (E2) Only a confirmation on the viewer's machine | nothing shown or logged on the host | the host's person and log would not see the elevation |
| (E3) An approval on the host | the host's present user accepts "Allow elevated control" | it blocks the main case, since nobody can answer at the sign-in screen or at A's lock screen. As an addition for a present user it would be a new prompt type; listed for later |

### 9.3 The wire

- **`Elevate`, type 0x000C, viewer to host, no fields, minor 1.** It is sent only to a host whose `HelloAck.proto_minor` ≥ 1, only after `SessionGrant`, and only in a session the host granted as `Unattended` without `UNATTENDED`. Frame: `0C 00`. 0x000B is `StoredGrants` (`GRANTS-SYNC.md`), and `is_core_type` grows to 0x000C.
- **The host's answer.**
  - Success is `PermissionsChanged { granted: current ∪ UNATTENDED ∪ (ADMIN_TERMINAL if stored and requested) }`, through `ctx.modify_granted`, the path the banner uses. It invalidates the resume token (5.6).
  - Refusal is `Error { code: NOT_PERMITTED, fatal: false, about_type: 0x000C, about_id: 0 }`: the device no longer holds `UNATTENDED`, or the session was not an unattended entry.
  - Already elevated: the same `PermissionsChanged` again (idempotent).
  - `Elevate` before the grant is `PROTOCOL_VIOLATION`. From a peer that announced minor 0 it is `UNSUPPORTED_VERSION`, which cannot happen with a real 1.0 build.
- **Entry rule by minor.** To a viewer whose `Hello.proto_minor` ≥ 1, an unattended grant is `stored ∩ requested` without `UNATTENDED` and `ADMIN_TERMINAL`. To a minor-0 client that asked, it is the 1.0 grant (5.3). No bytes change for it.
- **Viewer side, local.** UI op 50 `ElevateSession { session_id }` is accepted only with the pass of the confirmation of 8.3 (`ElevAction::ElevateSession`); without it the answer is `NeedsAdmin` and the elevation step opens. Its replies:
  - `Done` when the host's `PermissionsChanged` with `UNATTENDED` arrives;
  - `NotPermitted` on the host's `Error`;
  - `Invalid` toward a minor-0 host;
  - `Timeout` after 10 s.

  `SessionState::Active` gains `elevation: u8` (0 not available, 1 available, 2 elevated) so the session bar and the paused view show the button.
- **When a 1.1 viewer asks for `UNATTENDED`** (this settles `ELEVATION.md` decision 3):
  - The 1.0 interface never asked. A 1.0 host that stored `UNATTENDED` for a key has therefore never let that key in without a prompt, and asking toward it would admit the session at once with the SYSTEM injector, even for "Assistir" with its user present.
  - So the viewer puts `UNATTENDED` (and `ADMIN_TERMINAL`) in `requested` only toward a host it **knows** to be minor 1 or newer. It knows from an earlier authenticated exchange with that device (`HelloAck`, `GrantsSync`), or else from the `minor` key of its discovery record (`GRANTS-SYNC.md` 4.3: `minor_seen` wins over `minor_hint`). Unknown, or 0, keeps the 1.0 behaviour: `unattended: false` and a prompt.
  - "Controlar" and "Assistir" both ask under that rule, with no separate control.
  - The host decides: without a stored `UNATTENDED` it prompts, and a prompted grant never holds either bit (D8).
- **When the authenticated `HelloAck` disagrees** (the hint said 1 or more and the host says 0: a forged record, or a host that went back to 1.0):
  - The viewer keeps the session as the 1.0 host grants it. If that grant holds `UNATTENDED`, the session shows as elevated from the start, without the button.
  - It records minor 0 for the device, never sends a minor-1 message on that connection, and writes one trace line.
  - A forged record gains an attacker nothing: the key check still holds.

## 10. The update from 1.0: nobody inherits (owner decision 1)

### 10.1 What 1.1 does at its first start

- The service finds the 1.0 files at the root of the folder and no `accounts.bin`. It writes `accounts.bin` with no account and binds slot 0's ports (5.5).
- **No account receives any 1.0 data.** The 1.0 key, its pairings, its grants in both directions, its activity log and its settings stay where they are, untouched.
- Every account starts empty. The first account to need an identity (it opens Swapdesk and pairs, or opens "Parear este computador") gets a **new** key and slot 0, which is port 47820, the 1.0 port.

### 10.2 What 1.1 still reads from the 1.0 files

- **Only `settings.bin`, read-only:**
  - `port` and `pair_port`, the base of the slots, so a computer configured on other ports keeps them;
  - (D55: the first account takes the suffix like every other, so `name` is not read);
  - nothing else. `service_enabled`, `lock_on_end` and `keep_awake` are not carried: each account starts with its own defaults.
- **Never:** `identity.bin` (the 1.0 key is never loaded, presented or used to dial), `trust.bin`, the 1.0 audit logs.
- **The notice (D55, kept):** whether `identity.bin` exists, to show a one-time note in each account's Pareados list.

### 10.3 What a peer experiences

- **A peer on 1.0, or on 1.1, that paired with this computer before the update** holds the 1.0 key.
  - In its discovery, this computer's first account appears on the same address and port with a new fingerprint. A 1.0 or 1.1 browser replaces the old record at that address and port (`found`), so the old paired entry shows as offline from then on, and a new, unpaired device appears under the first account's name.
  - Dialling the old entry fails in the handshake, because the endpoint presents the new key and the peer accepts only the pinned one. Waking it sends the packet but the device never "appears" with the old key.
  - When this computer's account dials that peer, the peer refuses the unknown key in its handshake. On a 1.0 peer that also raises the "aparelho desconhecido" alert, rate-limited.
  - The grants that peer once gave this computer stay in its own store, bound to the old key, which nothing uses any more.
- **To pair again**, both users open "Parear" within 2 minutes, as for any first pairing (D12). The peer's user has two routes:
  - **"Parear" on the old entry.** 1.0 already supports this as a re-pairing that replaces the key: it removes the old record and pins the new key with no grants (`pin` in `swapdesk-trust/src/pairing.rs`; `KeyReplaced` in the log).
  - **"Esquecer" on the old entry, then "Parear" on the new one.** Either way the new pairing starts with no grants on either side; unattended access is granted again where it is wanted (D7).
- **A computer with several accounts** shows one new entry per account that pairs. Each peer pairs only with the accounts it wants.

### 10.4 What the release notes and README must say

- Each account of a Windows computer now has its own pairings. Updating starts every account empty: pairings made before 1.1 are not carried over.
- On each other computer, forget this computer's old entry (or choose "Parear" on it) and pair again with the account you use. Grant unattended access again where you want it.
- Peers still on 1.0 can pair with any account of a 1.1 computer through the device list. By address, only the computer's first account is reachable (5.2).
- Going back to 1.0 restores the pairings exactly as they were before the update (10.5).
- The Mac app was already per account; nothing changes there except the port of a second account.
- The wording is the owner's decision (19).

### 10.5 Going back to 1.0, and updating again

- **Going back to 1.0:** 1.0 finds its root files exactly as the update left them. The machine-wide key, pairings and grants of before the update work again, for every account, as 1.0 always did.
  - Peers that already paired again with an account replaced or forgot the old key. For them the restored pairing fails, and they pair again with 1.0's key if they want to.
  - Peers that never paired again work as before. That includes any unattended grant the computer gave them in 1.0.
  - Everything made under 1.1 (accounts, pairings, settings) is invisible to 1.0.
- **Updating to 1.1 again:** 1.1 finds `accounts.bin` and the account folders as it left them. Each account resumes its 1.1 pairings. The root data is again never served, so anything paired under 1.0 in between stays on the 1.0 key and must be paired again with an account.
  - Nothing is shared between 1.0 and 1.1, so nothing is reconciled and nothing can come back.
- **The 1.0 uninstall after a downgrade** deletes only the names it knows and leaves `accounts.bin` and `accounts\`, which it reports. Its `-Force` removes the whole folder.

### 10.6 What the 1.1 uninstall removes

Unless `-KeepData`:
- in each `accounts\<SID>\`: `identity.bin`, `trust.bin`, `account.bin`, `peers.bin`, the audit logs, and their `.1`, `.tmp` and `.damaged-*` copies;
- `accounts.bin`;
- the 1.0 root names it already knows (`identity.bin`, `trust.bin`, `settings.bin`, the audit logs, `trace.log`, with the same copies);
- then each folder that is empty.

`-Force` removes the whole folder, as today. A folder that is a link is never entered.

## 11. Install modes

- **What exists:** a per-machine install under `%ProgramFiles%`, the LocalSystem service, and firewall rules for the service program (`packaging/windows`; F-SYS-01: "never per user").
- **"For all users"** is that install with this note: every account that opens Swapdesk gets its own data; an account that never opens it has none and is not announced.
- **"For one user"** can mean the same install used by one account only, which then costs nothing to the others. Or it can mean a **per-user install without the service**, the way the Mac app works (D32): `Swapdesk.exe` in the user's own program folder, the core inside the app, no SYSTEM component.
  - It could: view and control other machines; host the user's own unlocked desktop while the app runs, with input at medium integrity (no elevated windows, no UAC); and run terminal, files and clipboard as the user.
  - It could not: host the sign-in screen, the lock screen or UAC; host while the app is closed or nobody is logged in; offer `UNATTENDED` or the administrator terminal; or receive connections at all until an inbound firewall rule exists. Creating that rule needs an administrator. Whether Windows' first-run firewall prompt can add it for a standard user is UNPROVEN; it is probed in that stage.
  - It would also need its own rules: the per-machine helper-path check (C2.1) does not apply, and something must decide what happens when a per-machine install exists too.
- **Recommendation for 1.1** (open, 19): the per-machine install only, with per-account data. The per-user mode is a later stage, designed with the Windows installer of 1.2 (D38).

## 12. macOS (and Linux)

**Already per account:** the key, trust store, settings and activity live in each user's own `~/Library/Application Support/Swapdesk` (D31). A Mac account never saw another account's pairings, so the Mac has no migration.

**Changes:**
1. **Port slot.** When another account's app already holds the stored ports (fast user switching with both apps running), the app takes the next free pair (47822/47823, then 47824/47825...). It stores the pair in its own `settings.bin`, where `port` and `pair_port` are per user on the Mac, so the slot stays the same across restarts. Its record announces the port it really has.
2. **Hosting only while its session is active.** The app listens to `NSWorkspace.sessionDidResignActiveNotification` and `sessionDidBecomeActiveNotification`.
   - Apple: "This notification allows an app to disable some processing, such as when its user session switches out, and re-enable when that session switches back in". And "If an app launches in an inactive session, the workspace sends `sessionDidResignActiveNotification` after `willFinishLaunchingNotification`".
   - While inactive, the hub's console session is `None`: prompts and entries are refused with `ACCOUNT_AWAY` or `NOBODY_PRESENT`, and a live session pauses as in 5.1.
   - UNPROVEN what capture and input do in an inactive session; the design stops before it needs to know.
3. **The user reference** becomes the uid, as C5.1 already says ("macOS uid + name"), in place of `"local-user"`. This is not a format change.
4. **The Mac elevation note:** the `ConnectUnattended` path drops, and `SetGrants` stays (D7 on the Mac host). `OpenAdminTerminal` stays and `ElevateSession` joins it, as the viewer-side confirmation of 8.3: the administrator password dialog, which on 1.x guards against a person, not against code in the app's process.
5. **Last seen ports:** the app keeps `peers.bin` in its secrets folder, as on Windows.
6. **Two mDNS responders from two users' apps on one Mac:** whether both can bind UDP 5353 next to the system's own responder is UNPROVEN. It is proven with two logged-in accounts on the Mac (owner batch); the fallback is that only the active session's app announces.

**The later root daemon** (C2.2) must keep this rule: per-uid folders under its root folder, never one machine store.

**Linux** (no port exists): any future port keeps the same rule, either per-user data with the app or per-uid folders under a root daemon.

## 13. Threats at the new boundary (account against account on one machine)

| Attacker or case | What it tries | What stops it, or what remains |
|---|---|---|
| Account A's interface (or any process of A) | read B's pairings, grants, settings or activity | the pipe server routes by the client's SID to A's hub, which holds no reference to B's data (7.1). Test: two accounts' connections, each list and each op sees only its own |
| The same | change or forget B's pairings, change B's settings | the same: every op acts on the hub's own store |
| The same | use B's pairings (dial B's peers) | A's core presents A's key. B's peers pinned B's key, so the handshake fails at the peer |
| A standard account | grant `UNATTENDED` or `ADMIN_TERMINAL` in its own store | D7: an administrator's approval at UAC, over the shoulder, recorded as the approver (8.2), enforced by the service |
| An administrator account Z | see or change B's Swapdesk data | no path in the product (4.3). On disk the DACL lists Administrators: out of scope (TCB), as in 1.0 |
| The same | approve something B did not ask for | a ticket exists only for an action that B's own interface asked for, on B's connection, in B's Windows session (`ELEVATION.md` ticket rules) |
| A peer paired with A | reach B's session (B at the console, locked or disconnected), or get a frame or an input across a sign-in | 5.1: refused at entry; a live session pauses and ends; the helpers are attached only to the core that may host; the fence holds capture and input at every desktop switch until the service has read the console's user |
| The same, with `UNATTENDED`, at the sign-in screen | sign in to B, see the account list, restart, guess passwords | signing in to B makes B the owner, and A's session pauses (5.1). The rest is the reach an administrator approved with `UNATTENDED` (D7); Windows' account lockout policy applies; accepted |
| A peer paired with A | learn whether A is at the computer | `ACCOUNT_AWAY` tells it; accepted, since it is A's own paired device |
| Any LAN host | count the computer's accounts, learn their names | only accounts with a pairing listen and are announced (6); each announced name holds the Windows account name unless edited (owner decision 4); accepted as mDNS already shows names |
| The UI pipe of another session | act as the console's account | 1.0 pipe rules unchanged (DACL, first instance, session and SID checks), and the hub of the other session's user is that user's own anyway |
| A squatter on the new elevator pipe | receive a ticket, or pose as the service | first-instance creation and the server-PID check against the SCM, as for `ui-<session>` (C2.1) |
| A standard user | pre-create `accounts\<SID>` for an account that has none yet | the parent folder admits only SYSTEM and Administrators; the 1.0 move-aside rule also runs per subfolder |
| A forged discovery record | send a peer to another account's port | the client checks the key it expects (PROTOCOL 2.2), as in 1.0 |
| Code running as the user on the viewer's machine | use the account's pairings to reach SYSTEM on a peer (`Elevate`, the administrator terminal) | 8.3 (decided): the system's own confirmation, UAC on Windows. On macOS 1.x the dialog guards against a person, not against code in the app's process. Remains: plain unattended entry (the peer's desktop as its user), as THREAT-MODEL already accepts for everything the user can do |
| Another local user | hold an account's ports so it cannot receive connections | 5.5: slot ports bound exclusively at start (once the probe passes), else the program named and the account moved to a free slot. Remains: a local denial of service by holding every free port; accepted |
| The 1.0 data left on disk | be served again, or reach an account | 1.1 never opens `identity.bin` or `trust.bin` (10.2). Only a downgrade to 1.0 serves them again, as 1.0 always did; release notes (10.4) |
| Machine-wide effects of one account's session | lock, sleep or restart the machine, keep it awake | lock and keep-awake act within that account's sessions. Sleep and restart need `CONTROL` in a session that the console owner (or the sign-in screen) admitted, the same as a person at the keyboard; accepted |
| The trace log | read every account's device IDs and session times | administrators only (C2.3); never the never-logged items (F-SEC-01) |

## 14. Exact additions

### 14.1 Wire (protocol minor 1, shared with `GRANTS-SYNC.md`)

| Addition | Kind | Bytes | Sent to |
|---|---|---|---|
| `Elevate` | message 0x000C, V→H, no fields | `0C 00` | a host with minor ≥ 1, after the grant, in an unattended session without `UNATTENDED` |
| `ACCOUNT_AWAY` = 22 | `ErrorCode` value | in `SessionRefused`: `06 02 16 00` | a peer with minor ≥ 1, in any message that carries an `ErrorCode`; toward a minor-0 peer the encoder maps it to `NOBODY_PRESENT` (5) in every message (5.6) |
| `Desktop::AccountAway` = 4 | enum value | `HostState`: `09 02 04 01` | a viewer with minor ≥ 1; a minor-0 viewer gets `Desktop::Locked` with `paused: true` |
| Unattended grant without `UNATTENDED` and `ADMIN_TERMINAL` | semantics, no new bytes | | a viewer whose `Hello.proto_minor` ≥ 1 |
| `requested` | semantics: the viewer asks for `UNATTENDED` and `ADMIN_TERMINAL` from any account, but only toward a host known to be minor ≥ 1 (9.3). The `Hello` note "only for a local administrator (D10)" goes | | hosts of minor ≥ 1 only |
| Endpoints per account | ports `base + 2k` and `base + 2k + 1`, `k` 0 to 31 (GUESS) | | every peer, through the SRV port of each record |
| DNS-SD | one record per announced account; TXT keys unchanged (plus `minor` of `GRANTS-SYNC.md`) | | every browser |

`min_minor`: 0x000C, `ErrorCode` 22 and `Desktop` 4 give 1; every 1.0 type and value gives 0. The server name stays `swapdesk`.

### 14.2 Disk (Windows; PROTOCOL 9.2 rules, `format: u16` first)

| Record | File | Fields |
|---|---|---|
| `AccountsIndex` (new) | `%ProgramData%\Swapdesk\accounts.bin` | `format` = 1, `accounts: Vec<AccountEntry>(32)` |
| `AccountEntry` (new) | in `AccountsIndex` | `sid: str(184)`, `slot: u8` (0 to 31, unique), `created_ms: u64` |
| `AccountSettings` (new) | `accounts\<SID>\account.bin` | `format` = 1, `name: str(64)`, `receive: bool`, `lock_on_end: bool`, `keep_awake: bool` |
| `Identity`, `TrustStore` | `accounts\<SID>\identity.bin`, `trust.bin` | format 1, unchanged |
| `AuditEvent` | `accounts\<SID>\audit-*.log` | new tag 18 `Elevated` (no fields; the session entry it belongs to precedes it). Written only to per-account logs, which 1.0 never reads |
| `PeerPorts` (new) | `accounts\<SID>\peers.bin` (Windows); `peers.bin` in the secrets folder (macOS) | `format` = 1, `entries: Vec<PeerPort>(64)`, `PeerPort { device_id: [u8;16], session_port: u16, seen_ms: u64 }`; an entry goes with its device on Forget |
| `MachineSettings` | `settings.bin` | format 1, unchanged; read-only for 1.1 (`port`, `pair_port`, and `name` as the first account's default) |
| 1.0 `Identity`, `TrustStore`, audit logs | root `identity.bin`, `trust.bin`, `audit-*.log` | untouched; never opened by 1.1 |

The write rule is C5.2's (a temporary file in the same folder, flushed, renamed).
- A missing or unreadable `account.bin` reads as the defaults, with one trace line.
- A damaged `accounts.bin` is renamed aside and rebuilt from the account folders found, with the slots in folder order (GUESS; settled in S2).

### 14.3 Local IPC and C6 (one build, so no compatibility rule)

- **UI op 50 `ElevateSession { session_id: u64 }`** (9.3).
  - It and `TermOpen { admin: true }` outside an elevated session take a pass of `ElevAction` `ElevateSession { session_id }` (new tag 3) or `OpenAdminTerminal { session_id }` (tag 2, kept) (8.3).
  - Numbering: ops 35 to 38 are `ELEVATION.md`'s, 39 is proposed for `GRANTS-SYNC.md`'s `RefreshGrants`, and 40 to 49 belong to the file ops.
  - `ElevAction` tag 1 (`ConnectUnattended`) and op 51 are not assigned.
- **`SessionState::Active` gains `elevation: u8`.** `ThisDevice` gains `had_1_0_data: bool` only if the owner keeps the notice of 19.
- **Settings.** Op 20 `SetSettings` and op 19 `GetSettings` keep their bodies; the fields are the account's.
- **Op 4 `AddByAddress`** accepts `ip:port` in production.
- **Routing.** `ConnInfo` keeps `user` as the routing key, and the pipe server hands each connection to its account's hub.
- **Helper IPC.** `ResumeCapture { generation: u32 }`, service to system helper, 0x2A (the next free number after 0x29 `Shutdown`). `DesktopChanged` (0x36) is now also sent by the capture thread, ahead of any frame of a new desktop (5.1).
- **A new pipe `elev-<session id>`.** DACL: SYSTEM full, Administrators read/write. First instance, `PIPE_REJECT_REMOTE_CLIENTS`. It accepts only role `Elevator` (17.1).

## 15. Build plan (thin slices, in order)

"Red before" means the test fails, or does not compile, on the tree before the slice. Unit and loopback tests run on the Windows development machine with fakes. The machine layer is OS-free behind traits (a fake console-owner source, a fake folder store), so isolation and routing are proven without a second Windows account. Hosting across machines needs the Mac or the second Windows machine of D41, since a computer cannot dial itself (LAN guard).

**Order and dependencies.**

| Slice | Needs before it | Can run beside |
|---|---|---|
| S1 wire | `GRANTS-SYNC.md` S1 is the same change (one minor) | S2 |
| S2 account storage | none | S1 |
| S3 machine layer, one core per account, ports, discovery | S2 | S9 (Mac) |
| S4 console binding and the fence | S3, S1 | S8 |
| S5 first start, the 1.0 files left alone | S3 | S4, S8 |
| S6 unattended entry, `Elevate`, the viewer-side confirmation | S1, S4; `ELEVATION.md` slices 1 to 5; `GRANTS-SYNC.md` S4 (the `minor` TXT key) | S8 |
| S7 approval over the shoulder | `ELEVATION.md` slices 3 to 5 | S6 |
| S8 per-account settings and names | S3 | S4, S5, S6 |
| S9 macOS | S1 (for `AccountAway`); its own crates | S3 to S8 |
| S10 install script | S3 (the range and the file names) | any |
| S11 documents | each slice's own lines go in with it; the rest last | |

S10 must be installed before any cross-machine proof of a slot other than 0. The real update from 1.0 is proven in the VM of D41 (its own stage, not yet proven) or by hand on the owner's own update. The unit proofs of S5 use byte fixtures of 1.0 files (the M2-6 fixtures).

**S1. Wire.** `crates/swapdesk-proto`:
- `control.rs`: `Elevate` 0x000C, `ErrorCode::AccountAway` 22, `Desktop::AccountAway` 4, the `min_minor` entries;
- `lib.rs`: `PROTO_MINOR` = 1;
- `tests/protocol.rs`, `tests/common/mod.rs`, `tests/fuzz.rs`.

Tests:
- Red before: `elevate_and_account_away_round_trip_byte_for_byte`, `they_have_the_documented_bytes` (`0C 00`, `06 02 16 00`, `09 02 04 01`), `every_new_value_names_minor_1`.
- Guard, green before and after: the 1.0 wire fixtures of M2-6.

**S2. Account storage.**
- `crates/swapdesk-secret-win`: `folder.rs` (a subfolder spec under the protected root), `lib.rs` (`WinSecretStore::open_account(sid)`).
- `crates/swapdesk-trust/src/records.rs`, or `swapdesk-host/src/ui_hub/store.rs` beside `MachineSettings`: `AccountsIndex`, `AccountSettings`, `PeerPorts`.
- `swapdesk-trust/src/audit.rs`: tag 18.

Tests, red before:
- `an_account_folder_is_created_owned_by_system_with_the_protected_dacl`, `a_pre_created_account_folder_with_another_owner_is_moved_aside`, `a_sid_with_anything_but_digits_and_dashes_names_no_folder`;
- `accounts_index_account_settings_and_peer_ports_round_trip`, `audit_tag_18_round_trips_and_a_1_0_log_still_reads`.

The ACL tests run in development shape (the current user in SYSTEM's row), as stage 10's did.

**S3. Machine layer and one core per account.**
- `crates/swapdesk-host`:
  - a new module `accounts`: the registry, the slots, the machine-wide host reservation (5.6), the shared handshake limiter;
  - `service/mod.rs` and `swapdesk-session/src/host.rs`: a `HostGate` taken atomically in `decide` before a grant, carrying the console rule of 5.1;
  - `ui_hub/presence.rs` (the machine's own fingerprints filtered);
  - `ui_hub/views.rs` (`address_of` reads `peers.bin`);
  - `ui_hub/ops.rs` (`ip:port` in production; narrowing ops from any session of the account's user).
- `swapdesk-lan`: `discovery.rs` (several own records); `socket.rs` (the exclusive wildcard binding of 5.5 after its probe, else exclusive per-address binds).
- `crates/swapdesk-service`: `app.rs` builds the registry, a core and a hub per account; `ui_pipe.rs` routes by SID.

Tests, red before unless marked:
- `each_account_listens_on_its_own_slot_and_presents_its_own_key`, `a_key_pinned_by_one_account_fails_the_handshake_on_the_other`;
- `a_ui_connection_reaches_only_its_own_accounts_data` (two `ConnInfo` users: `ListDevices`, `ActivityList`, `SetGrants`, `ForgetDevice`);
- `two_unattended_entries_at_the_sign_in_screen_at_once_admit_exactly_one`;
- `an_account_without_pairings_neither_listens_nor_announces`;
- `two_records_of_one_address_with_different_ports_both_stay_listed` (a guard on `found`, green before);
- `this_machines_other_accounts_are_not_listed`;
- `a_typed_address_with_a_port_is_accepted_in_production_and_pairs_on_the_next_port`;
- `a_paired_device_out_of_discovery_is_dialled_at_its_last_seen_port`;
- `a_narrowing_from_a_switched_out_session_of_the_same_user_is_accepted`;
- `a_held_port_moves_the_account_to_the_next_free_slot_and_names_the_holder`.

Measure and probe:
- B6 with two accounts (one runtime per core; if B6 fails, the cores share one runtime: GUESS);
- the probe of 5.5, `Incoming::local_ip()` on a wildcard socket.

**S4. Console binding and the fence.**
- `crates/swapdesk-sys-win`: the console session's user.
- `crates/swapdesk-helper` and `swapdesk-host/src/helper.rs`: the capture thread sends `DesktopChanged` and waits for `ResumeCapture`; the input thread drops events after a desktop change.
- `crates/swapdesk-ipc`: `ResumeCapture` 0x2A.
- `crates/swapdesk-service/src/helpers.rs`: helpers attached to the core that may host.
- `swapdesk-session`: `HostState` by minor, the 60 s pause then `Bye`, the `ErrorCode` mapping for minor 0.
- The D13 guard: `consent_need` refuses.

Tests, red before:
- `a_request_to_an_account_not_at_the_console_is_refused_account_away_or_nobody_present_by_minor`;
- `at_the_sign_in_screen_an_unattended_device_of_any_account_enters`, `a_prompted_request_at_the_sign_in_screen_is_nobody_present`;
- `when_another_account_takes_the_console_the_session_pauses_then_ends`;
- `the_helpers_follow_the_console_owners_core`;
- `a_desktop_switch_holds_frames_and_input_until_the_service_resumes`;
- `a_sign_in_at_the_sign_in_screen_never_routes_a_frame_or_an_input_of_the_new_account`;
- `no_minor_1_error_code_reaches_a_minor_0_peer_in_any_message`;
- `terminal_and_files_never_run_as_another_account` (it replaces the D13 consent tests);
- after the review: `each_console_owner_gives_the_sessions_their_desktop_and_an_unreadable_user_is_away`, `an_account_asked_for_twice_at_once_is_built_once`, `a_session_that_starts_while_its_account_is_away_ends_after_the_minute`. Left as defects of their own: a running helper that does not move when the hosting account changes at the sign-in screen (fails closed), and the pairing port and a not-receiving slot (5.5, 6).

**S5. First start: the 1.0 files left alone.** `crates/swapdesk-host/src/accounts` (the first start, the read of `settings.bin`, the first account's name), `crates/swapdesk-ui` (the notice, only if kept).

Tests, red before:
- `a_first_1_1_start_on_1_0_files_gives_no_account_any_1_0_data`;
- `the_1_0_root_files_stay_byte_for_byte_and_still_load_with_the_1_0_decoder`;
- `the_first_account_gets_a_new_key_slot_0_and_the_1_0_name`;
- `a_peer_that_pinned_the_1_0_key_fails_the_handshake_and_can_pair_again_with_replaces` (two cores on loopback, the peer re-pairs through its old entry);
- `a_second_start_after_a_downgrade_finds_its_accounts_and_still_ignores_the_root_data`.

**S6. Unattended entry, the elevation step and the viewer-side confirmation.**
- `swapdesk-session`: `policy.rs` (the unattended grant by minor, without the two bits); `actor.rs` and `host.rs` (`Elevate` through `modify_granted`, the audit event).
- `swapdesk-host`: `ui_hub/ops.rs` (`Connect` asks only toward a known minor-1 host; the D10 `NeedsAdmin` on `Connect` goes); op 50 and `TermOpen { admin }` behind the 8.3 pass; `ElevAction` tag 3.
- `swapdesk-ipc`.
- `swapdesk-ui`: `state.rs`, `interact.rs`, the button and the paused view, after the owner's texts.
- `swapdesk-paint`: the session bar and paused screens.

Tests, red before:
- `a_minor_1_viewer_enters_unattended_without_the_unattended_and_admin_terminal_bits`;
- `elevate_adds_both_bits_and_moves_input_to_the_system_helper`, `elevate_without_a_stored_unattended_is_not_permitted`, `elevate_before_the_grant_is_a_violation`;
- `an_unelevated_unattended_session_pauses_at_a_secure_desktop`;
- `a_reconnect_after_elevate_enters_again_unelevated`;
- `a_viewer_asks_unattended_only_toward_a_host_known_to_be_minor_1`;
- `a_minor_0_helloack_after_a_minor_1_hint_keeps_the_session_and_records_minor_0`;
- `elevate_and_the_administrator_terminal_need_the_viewer_side_pass`;
- `an_elevated_session_opens_the_administrator_terminal_without_a_second_pass`.

Guards, green before and after:
- `a_minor_0_viewer_gets_unattended_at_entry_as_in_1_0`;
- `toward_a_1_0_host_the_viewer_never_asks_unattended`.

The built tests that assert `NeedsAdmin` on `Connect { unattended }` change meaning in this slice; they are rewritten and named in the commit.

**S7. Approval over the shoulder** (17.1).
- `crates/swapdesk-service/src/ui_pipe.rs`: the `elev-<session>` pipe.
- `swapdesk-host/src/ui_hub/elevation.rs`: the approver rule, and the pass carrying the approver.
- `ops.rs`: `GrantsChanged.by` = the approver.

Two built tests of `ELEVATION.md` are reversed, named in the commit:
- `an_account_that_is_not_an_administrator_gets_no_ticket`: a standard account now gets one;
- `an_approver_of_another_user_or_session_is_refused`: another user in the same session is now accepted, and another session stays refused.

New tests, red before:
- `an_administrator_of_another_account_in_the_same_session_confirms_a_standard_accounts_ticket`;
- `the_grant_lands_in_the_asking_accounts_store_with_the_approver_in_the_log`;
- `an_unelevated_or_other_session_approver_is_refused`;
- `the_elevator_pipe_serves_only_the_elevator_role`.

**S8. Per-account settings and names.** `ui_hub/store.rs`, `ui_hub/ops.rs` (ops 19 and 20 per account), the presence TXT name, and the `swapdesk-ui` strings (the line under the name field is decided).

Tests, red before:
- `settings_of_one_account_do_not_change_anothers`;
- `receive_off_closes_the_accounts_endpoint_and_withdraws_its_record`;
- `a_later_account_is_named_computer_dot_account_and_the_first_keeps_its_name`.

**S9. macOS.**
- `swapdesk-app-mac/src/service.rs`: the slot fallback, the uid `UserRef`, `peers.bin`.
- `src/app.rs`: the session notices.
- `swapdesk-core-mac/src/lib.rs`.

Tests, red before:
- `a_busy_port_pair_moves_to_the_next_slot_and_stays_there` (OS-free, runs in the Windows suite too);
- `an_inactive_session_refuses_and_pauses` (scripted notices).

On the Mac with two logged-in accounts (owner batch): both apps announce, a PC dials each, and the inactive one answers away.

**S10. Install script.**
- `packaging/windows/install.ps1`: the firewall range.
- `uninstall.ps1`: the names of 10.6.
- `packaging/windows/README.md`.

Proof: the `-DryRun` output lists the range and the names; the real run is the owner's UAC click.

**S11. Documents.**
- `docs/PROTOCOL.md`:
  - 1: account endpoints and slots; the handshake caps per machine;
  - 3: the lockout per account, one pairing at a time per machine;
  - 5.3: the `Hello` note without D10, the `SessionGrant` row (the unattended grant by minor), `Elevate`, `HostState`;
  - 6.1: the note on `UNATTENDED` entry;
  - 6.4: an `Elevate` invalidates the resume token like any grant change;
  - 8: code 22 and its mapping for minor 0;
  - 10: one DNS-SD record per account.
- `docs/CONTRACTS.md`:
  - C2.1: the pipes, `elev-<session>`, the helper fence and `ResumeCapture`;
  - C2.3: admin-only operations and the viewer-side confirmation;
  - C2.4: op 50, `ElevAction` tag 3, op 4 with `ip:port`;
  - C5.1 and C5.2: the per-account rows, `peers.bin`, and the 1.0 files left as they are.
- `docs/SPEC.md`:
  - F-PERM-01;
  - F-PERM-03 without D10's account check;
  - F-SESS-01;
  - F-SESS-02 (the sign-in screen only with nobody logged in, unelevated until the step; no D13);
  - F-TERM-02 (an elevated session or the viewer-side confirmation);
  - F-FIND-03 (`ip:port`);
  - F-XUPG-01 (rewritten: the update keeps nothing of 1.0 for any account; 1.0 data stays for a downgrade);
  - data, the trust boundaries and the threats.
- `docs/THREAT-MODEL.md`: the rows of section 13; the "same user" section from 8.3; the D10 and D13 rows go.
- `docs/DECISIONS.md`: the owner's decisions of 2026-11-10; D10 and D13 marked replaced.
- The README and the release notes (10.4).
- The flow map.

`check_flows.py --check` exits 0.

**What needs the owner at the keyboard** (one batch):
- a second local Windows account on the Windows development machine, made for the proof and removed afterwards (the owner's yes);
- signing in to it, and switching users, while the Mac is connected (5.1 and the fence);
- a restart to the sign-in screen with the Mac entering unattended and elevating (9.1);
- a UAC prompt in a standard account answered with an administrator's credentials (8.2);
- the confirmation for `Elevate` and the administrator terminal on the viewer's machine (8.3);
- the reinstall of the service with the new firewall range (UAC);
- two accounts on the Mac (12);
- the update from 1.0 with the Mac re-pairing through its old entry (10.3).

## 16. Facts relied on (read 2026-11-10)

1. **DPAPI scopes.** CryptProtectData, Remarks: "Typically, only a user with logon credentials that match those of the user who encrypted the data can decrypt the data"; and for `CRYPTPROTECT_LOCAL_MACHINE`, "any user on the computer where the encryption was done can decrypt the data". https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata. CryptUnprotectData: "Usually, the only user who can decrypt the data is a user with the same logon credentials as the user who encrypted the data." https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptunprotectdata
2. **Another user's profile from a service.** LoadUserProfileW: the token is the one "returned by the LogonUser, CreateRestrictedToken, DuplicateToken, OpenProcessToken, or OpenThreadToken function"; "If a service or an application impersonates a user, the system does not load the user's profile"; "the caller must be an administrator or the LocalSystem account". https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-loaduserprofilew
3. **TLS 1.3 order.** RFC 8446 section 2, Figure 1: the server's `{Certificate*}` follows `{EncryptedExtensions}` and `{CertificateRequest*}` in its first flight; the client's `{Certificate*}` comes after the server's `{Finished}`. https://www.rfc-editor.org/rfc/rfc8446.html
4. **rustls server name.** `ClientHello::server_name()`: "Get the server name indicator. Returns `None` if the client did not supply a SNI." (rustls 0.23.46) https://docs.rs/rustls/latest/rustls/server/struct.ClientHello.html. `ClientConfig::enable_sni`: "Whether to send the Server Name Indication (SNI) extension during the client handshake. The default is true." https://docs.rs/rustls/latest/rustls/client/struct.ClientConfig.html
5. **Firewall port ranges.** The New-NetFirewallRule example: `-Protocol TCP -LocalPort 12345,5000-5020`. https://learn.microsoft.com/en-us/powershell/module/netsecurity/new-netfirewallrule
6. **macOS session switching.** `NSWorkspace.sessionDidResignActiveNotification`: "A notification that the workspace posts before a user session switches out." "This notification allows an app to disable some processing, such as when its user session switches out, and re-enable when that session switches back in." "If an app launches in an inactive session, the workspace sends sessionDidResignActiveNotification after willFinishLaunchingNotification and before sending didFinishLaunchingNotification." https://developer.apple.com/documentation/appkit/nsworkspace/sessiondidresignactivenotification (read through Apple's documentation data, the same text as the page)
7. **UAC, over-the-shoulder credentials, filtered tokens, the TCB statement:** the pages cited in `ELEVATION.md` section 6, not re-read for this note.
8. **Port binding between user accounts.** Microsoft, "Using SO_REUSEADDR and SO_EXCLUSIVEADDRUSE":
   - The table for binds "made under different user accounts": a first default bind on the wildcard address makes a second default bind of the same port on a specific address fail with `WSAEACCES`. A first default bind on a specific address lets a second wildcard bind succeed.
   - After a first bind with `SO_EXCLUSIVEADDRUSE` on the wildcard address, every second bind of that port fails (`INUSE` or `ACCESS`).
   - "All server applications must set **SO_EXCLUSIVEADDRUSE** for a strong level of socket security."

   https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse

**UNPROVEN, and how each is proven:**
- `Incoming::local_ip()` on a wildcard socket on Windows: 5.5, the probe in S3.
- Whether the session's token or session information names the new user as soon as its desktop appears: 5.1, and the fence fails closed meanwhile; the S4 test and the owner batch.
- That the service cannot use a user-scope DPAPI blob of a user who is not logged in: a reading of fact 1; not relied on.
- `LookupAccountSidW` for a deleted account: an S2 test with a scratch account, owner batch.
- The cost of one core per account: B6 with two accounts, S3.
- The helper restart time when the sign-in screen is lent to an unattended session: measured in S4; a multiplexer if it is too slow.
- Two users' mDNS responders on one Mac: S9, owner batch.
- Capture and input in an inactive Mac session: not needed, because hosting stops.
- Whether a per-user Windows install can get its inbound firewall rule without an administrator: the later per-user stage.

## 17. Changes to the other notes

### 17.1 `ELEVATION.md`

**Final set of actions** (`ElevAction`, tag `u8`):
- 0 `SetGrants { device_id, grants }`: D7, on the host.
- 2 `OpenAdminTerminal { session_id }`: the viewer-side confirmation before the administrator terminal outside an elevated session (8.3).
- 3 `ElevateSession { session_id: u64 }` (new): the viewer-side confirmation before `Elevate` (8.3), with its followup op 50 in `UiState` like the others.
- Tag 1 (`ConnectUnattended`) is removed and never assigned again: plain unattended entry asks nothing. Section 2.3 and the `ops.rs` `request_other` check go.
- Tag 4 and op 51 are not used: there is no claim of the 1.0 data.
- `Viewer.admin_entry` becomes "this session was elevated through a confirmed `ElevateSession`". Such a session opens the administrator terminal without a second pass.

**The over-the-shoulder approver path** (open point 1 reversed, owner decision 2):
- Begin is allowed to any console interface: a standard account gets a ticket and a UAC credentials prompt. `ConnInfo.can_elevate` only chooses the text (consent or credentials); it no longer refuses a ticket.
- The elevated instance opens the new pipe `elev-<session id>` (DACL: SYSTEM, Administrators; role `Elevator` only). `ui-<session>` keeps its DACL and still accepts role `Elevator` from the same user, as built.
- Describe and Confirm require an elevated connection with Administrators enabled **in the same Windows session as the ticket**, no longer the same user SID.
- The pass carries the approver's `UserRef`, which the hub writes as `GrantsChanged.by`.
- Section 8's objection ("two people recorded ... changes data written on disk") no longer holds: D13's grantor is moot, and `by` is an existing `UserRef`.
- Two built tests are reversed (15, S7). The threat rows "Standard user at the keyboard" and "Over the shoulder" change with it. Exit code 4 remains for an instance the service refuses.

**Other points:**
- **The `ADMIN_TERMINAL` request rule** (3.2): every connection asks for it toward a host known to be minor 1 or newer (9.3). The host grants it only when stored, and in an unattended entry only after `Elevate` (5.6).
- **Decision 3** (the unattended entry control) is settled by 9.3.
- The `SetGrants` path being built now is unaffected until S7.

### 17.2 `ELEVATION-MAC.md`

- `AdminAction` keeps `Grants` and `AdminTerminal`, gains `ElevateSession`, and loses `ConnectUnattended`.
- The confirmation before `Elevate` and before the administrator terminal is the system administrator dialog. Its limit is the one that note states: it guards against a person, not against code in the app's process.
- The Mac `ConnInfo.user` becomes the uid (12).
- The over-the-shoulder path has no Mac counterpart: the dialog already accepts any administrator's name and password, and the audit can name the approver only if `AuthorizationCopyInfo` returns it (UNPROVEN, as that note says).

### 17.3 `GRANTS-SYNC.md`

- Everything "per machine" is per account identity. A sync connection runs between two account keys, on the account's own session port (the record's SRV port). The responder reads `grants_here` from the store of the endpoint's account, which the endpoint fixes before the handshake. The tie-break of 4.4 compares the two account device IDs.
- An account not at its console still answers syncs: no prompt and no presence, as 4.3 already says. Its endpoint listens whenever it has a pairing (6), so the appearance trigger works with nobody logged in.
- `minor` is published in every account's record, and S6 here reads it to decide whether to ask for `UNATTENDED`.
- `StoredGrants` keeps 0x000B and this note takes 0x000C. Both are minor 1, and the S1 slices of both notes are one change.
- Section 6 guessed op 35 for `RefreshGrants`, but 35 to 38 are `ELEVATION.md`'s: it takes **39**.
- After the update no pairing survives, so `grants_there` starts empty for every account and is learned again after re-pairing.

## 18. Options rejected

- One machine key with a trust store per account (3): the peer holds one record for every account, and its grants and "forget" mix them.
- Picking the key by TLS server name (5.2 S): 1.0 peers reach only one account, and 1.0 browsers flicker between records of one address and port.
- Naming the account in `Hello` after the handshake (5.2 O): the same as the machine key.
- User-scope DPAPI or the user's own profile folder for the account data (4.2): unusable by the service with nobody logged in, which is the sign-in screen case.
- A SYSTEM-only DACL on the account folders: administrators are TCB and can take ownership; it adds uninstall work and no protection (4.3).
- Connecting a peer of B to B's disconnected session by switching the console (5.1): it throws the person at the keyboard off the screen.
- Letting a peer of B see A's lock screen: it shows A's name and notifications.
- Keeping D10's account check: it tests the account against itself (8.1).
- A plain in-app confirmation, or none, for the SYSTEM-level reach on a peer: code running as the user would pass it (8.3).
- Keeping D13's question: no other user's session is reachable (5.4).
- A viewer-side confirmation alone, with nothing shown on the host (9.2 E2); host approval as the only elevation (9.2 E3).
- Announcing every account always, or a presence key in the record: the LAN would learn how many accounts exist, or who is at the computer.
- Any inheritance of the 1.0 data, automatic or claimed (owner decision 1): a peer's grants to the machine would land in one account that its user never chose.
- Deleting or moving the 1.0 files at the update: going back to 1.0 would make a new key and break every peer that still uses it.
- A port field in the trust record (5.2): trust format 2, which a 1.0 build refuses.
- Asking for `UNATTENDED` toward every host (9.3): a 1.0 host that stored it would stop prompting even for "Assistir" with its user present.
- One core holding every account's identity, with keyed lookups everywhere: it isolates by discipline in every handler, where a core per account isolates by construction (S3).
- A per-user Windows install in 1.1 (11): a second product mode with its own limits and firewall question, better designed with the 1.2 installer.

## 19. Still open for the owner

Recommendation first. The four points the owner decided on 2026-11-10 are at the top of this note. Points 1, 2, 3 and 5 were decided on 2026-11-11 (D55): 1 the alternative (the suffix for every account), 2, 3 and 5 as recommended.

1. **The first account's name** (6). Recommended: the first account of a computer is named like the computer was in 1.0 (the 1.0 Swapdesk name, else the Windows computer name), with no account part. Later accounts get "<computer name> · <Windows account name>". Alternative: the account part for every account, the first included.
2. **Texts of the elevation step** (9). Recommended:
   - button: "Elevar controle" / "Elevate control", with the D42 hint "Controlar também os avisos de administrador e a tela de entrada" / "Also control administrator prompts and the sign-in screen";
   - paused view: "O {0} mostra uma tela protegida. Eleve o controle para vê-la e usá-la." / "{0} shows a protected screen. Elevate control to see and use it.";
   - host banner: "Controle elevado" / "Elevated control".

   Also inside the step:
   - (a) an entry that finds the sign-in screen starts paused with the button (recommended), or elevates by itself after the confirmation;
   - (b) the paused view shows nothing (recommended, the 1.0 pause), or shows the picture without control.
3. **Texts of the account rule** (5.1, 5.5). Recommended:
   - refusal (`ACCOUNT_AWAY`): "Ninguém está usando esta conta no {0} agora." / "Nobody is using this account on {0} right now.";
   - pause (`AccountAway`): "Esta conta não está mais na tela do {0}. A sessão volta quando ela voltar." / "This account is no longer on {0}'s screen. The session resumes when it is back.";
   - a held port: "Outro programa está usando a porta desta conta: {0}." / "Another program is using this account's port: {0}.".
4. **Which accounts are visible on the network** (6). Recommended: only accounts that have a pairing or have pairing open. Alternative: every account that ever opened Swapdesk.
5. **A one-time note after the update** (new, 10.2). Recommended: yes. In each account's empty Pareados list on a computer that had 1.0 data: "Os pareamentos de antes da versão 1.1 não passam para as contas. Pareie de novo com os computadores que você usa." / "Pairings made before version 1.1 do not carry over to accounts. Pair again with the computers you use." It costs one read of whether the 1.0 `identity.bin` exists. Alternative: release notes only.
6. **The release notes and README wording** for the update (10.4). Recommended: the points of 10.4, in both languages.
7. **Install modes** (11). Recommended: 1.1 keeps the per-machine install with per-account data; a per-user install without the service is designed with the 1.2 installer. Alternative: add it to 1.1.
8. **The model and the reach** (3, 5.2): one identity per account, each on its own port pair, firewall range 47820-47883 (32 accounts, GUESS). Recommended: approve as designed; no alternative keeps 1.0 peers working with every account.
9. **Scope in 1.1** (15). Recommended: S1 to S11 in 1.1, with the wire shared with `GRANTS-SYNC.md` in one minor. The hand proofs at the sign-in screen, the lock screen and UAC run with the unattended stage of 1.2 (D38). Alternative: move S6 and S7 to 1.2 together with those proofs. The wire would then carry minor 2, or minor 1 if nothing with minor 1 shipped before.
