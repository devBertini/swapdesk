# Design note: learning the other side's grants (`grants_there`)

Status: proposal, 2026-11-10, revised the same day for D44 (each account of a computer has its own Swapdesk identity, trust store, settings and activity; design in `docs/ACCOUNTS.md`). Nothing here is built. The owner's choices still open are in section 9; everything else has a recommended default. **GUESS** marks a number or reading not yet proven by a test.

## 1. The problem

The Permissões screen shows, for each device X paired with this account, a read-only column "Este computador pode, no X": what X lets this account do there. Its source is `PairedDevice.grants_there` (C5.1: "what that device last said it lets this one do there: display only, the other side decides"), carried by `DeviceView.grants_there` and drawn by `there_rows` as Sim / Não / "–". No product code ever writes the field, and the protocol has no message that carries a device's **stored** grants: `SessionGrant` and `PermissionsChanged` carry what one session may do, which in a prompted session is the host user's pick and may differ from what is stored (D8). So every row reads "–".

Required (owner, 2026-11-10): the column follows what the other side stores for this one, at the start of a session, during a session, and **outside any session**: marked there, shown here within seconds while both are on the network.

**The model after D44.** The two ends of every exchange in this note are **account identities**, not machines. A "device" is one account of one computer, with its own key, its own trust store and its own port pair (`ACCOUNTS.md` 1 and 5.2: session port `base + 2k`, pairing port `base + 2k + 1` for slot `k`). A peer pairs with "this computer, account X". `grants_here` and `grants_there` live in that account's store; nothing in this note reads or writes another account's data. On the Mac the app is already per account (D31), and a second account of a Mac takes its own slot (`ACCOUNTS.md` 12).

Constraints kept by every option below:

- 1.0.0 is published. `PROTO_MINOR` travels in `Hello` and `HelloAck`; a message type or enum value introduced in minor N goes only to a peer whose minor is at least N; an unknown type or tag is fatal for the receiver (PROTOCOL.md 1). No existing message layout changes (PROTOCOL.md 11: the 1.0 wire fixtures). Data 1.0 wrote on disk still loads; under D44 1.1 never writes it (`ACCOUNTS.md` 10).
- LAN only, enforced in code: physical interfaces, on-link peers, hop limit 1. Peers authenticate with the keys pinned at pairing; the post-handshake gate looks the key up again (PROTOCOL.md 2.4).
- Nothing makes the other side prompt its user. Nothing a peer says about its own grants is relied on: `grants_there` is display only.

## 2. Recommendation in short

1. **In a session:** a new control message `StoredGrants` (0x000B, both directions, minor 1). Each side sends its account's stored grants for the other device right after the grant, and again whenever its user changes them.
2. **Outside a session:** a short **grants sync connection** between two account keys, on the target account's own session port (the SRV port of its record, or the last port kept in `peers.bin`). Same guard, same caps, same pinned-key handshake and gate as a session. The dialer's first message is `GrantsSync` (0x0801, minor 1) in place of `Hello`, carrying the dialer's stored grants for the other device; the other side answers with one `GrantsSync` carrying its own; the dialer closes. One round trip updates both accounts. No prompt, no presence, no session, no audit entry, whether or not the account is at its console.
3. **Who dials when:** the account whose user changed the grants pushes (1 s after the last change); an account that sees a paired device appear on the network syncs with it; opening a device's Permissões tab syncs if the last sync is older than 30 s.
4. **How a dialer knows the other side is minor 1 or newer before any message:** a new DNS-SD TXT key `minor` in every account's record (1.0 browsers ignore unknown keys), overridden by what an authenticated exchange showed. The same knowledge decides, in `ACCOUNTS.md` 9.3, whether a viewer asks for `UNATTENDED`. A 1.0 peer has no `minor` key, so it is never dialled for a sync and never sent `StoredGrants`.
5. **Nothing new on disk from this note.** `grants_there` is already in the format-1 trust record, now the account's own; `peers.bin` (`ACCOUNTS.md` 14.2) is that note's side file, and a sync that connects only refreshes it under its rule. The UI pipe needs one new op, `RefreshGrants` = **39**, and, only if the owner wants the state note of 7.3, one new `DeviceView` field.
6. **One wire change with `ACCOUNTS.md`.** Its S1 and this note's S1 are the same slice: `StoredGrants` 0x000B, `Elevate` 0x000C, `GrantsSync` 0x0801, `ACCOUNT_AWAY` 22, `Desktop::AccountAway` 4, `PROTO_MINOR` = 1.

## 3. Slice 1: `StoredGrants` in a session

### 3.1 Message

| Type | Name | Dir | Fields | Notes |
|---|---|---|---|---|
| 0x000B | StoredGrants | both | `grants: PermissionSet` | minor 1. What the sender's account has stored for the peer's device (`grants_here` of its record), not what this session holds. Display only for the receiver |

- Range: 0x000B is the next free value of the session core group, beside `PermissionsChanged`; `ACCOUNTS.md` takes 0x000C (`Elevate`). `is_core_type` in `swapdesk-session/src/policy.rs` grows to 0x0001..=0x000C in the shared S1, so a feature crate cannot register either.
- Layout, PROTOCOL.md 9.2 rules: `grants` is a `u16`, little-endian. Example, VIEW and CONTROL: frame `0B 02 03 00` (type varint 0x0B, length 2, `0x0003`).
- Limits: the payload is exactly 2 bytes. The `PermissionSet` decoder already rejects reserved bits 8 to 15 and a set that breaks "implies" (PROTOCOL.md 10): that is `PROTOCOL_VIOLATION`, fatal.

### 3.2 When it is sent

- **Host:** in the batch that follows `SessionGrant` (5.2 step 4), after `WakeInfo` (or where `WakeInfo` would be) and before the first video stream, when `Hello.proto_minor >= 1`. So the 1.0 order and bytes are unchanged for a minor-0 viewer.
- **Viewer:** when it receives `SessionGrant`, when `HelloAck.proto_minor >= 1`.
- **Both:** again whenever the sender's stored grants for that device change while the session lives (`SetGrants` from Permissões, on the host through `HostService::stored_grants_changed`, on the viewer through the account's viewer sessions with that device). It goes through the "must arrive" queue of the actor, like `PermissionsChanged`, latest value wins.
- A swap, a resume or a reconnect is a new connection: the rule above runs again on it. An `Elevate` (`ACCOUNTS.md` 9) changes the session's grants, not the stored ones, so it sends no `StoredGrants`.
- A session paused because its account left the console (`AccountAway`, `ACCOUNTS.md` 5.1) still carries `StoredGrants`: the message is about the stores, not the screen.

### 3.3 Violations

- Received before `SessionGrant` on either side (5.2 step 3: `allowed_before_grant` stays false for it): `PROTOCOL_VIOLATION`, fatal.
- Received from a peer whose `Hello` or `HelloAck` announced minor 0: `UNSUPPORTED_VERSION` ("a message newer than the peer's minor"), fatal.
- A bad `PermissionSet`: `PROTOCOL_VIOLATION`, fatal.
- It needs no permission and is never answered with `NOT_PERMITTED`.

### 3.4 What the receiver does

- Keeps the value as `grants_there` of the peer's record **in its own account's store**, but only if it differs from what is stored; writes the trust store at most once per second per device (the latest value wins: a peer that repeats the message cannot turn it into a stream of disk writes).
- Then the account's hub pushes `DevicesChanged` (C2.4 notify 1) to that account's interfaces only; `DeviceView.grants_there` carries it and the open Permissões tab redraws.
- Nothing else reads it: it never widens or narrows a session, never decides a swap (6.3 step 1 reads the future host's **own** `grants_here`), never feeds a prompt, never decides an `Elevate`.

### 3.5 Code path

- `swapdesk-session`: the actor keeps the peer's minor (from `Hello` on the host, `HelloAck` on the viewer) in `SessionCtx`, and reports it to the account's peer-minor state (4.3); the host sends after the grant batch; the viewer sends on `SessionGrant`; a received one becomes `HostEvent::PeerGrants { device, grants }` on the host and `ViewerEvent::PeerGrants(grants)` on the viewer. The viewer's own stored grants for the host come from its account's trust store through the account core's `Connector`.
- `swapdesk-host`: `ui_hub/host_ui.rs` and the service's viewer path (`service/viewer.rs`) hand both events to one hub function, `apply_peer_grants(device, grants, source)`, which compares, writes and notifies. With one core and one hub per account (`ACCOUNTS.md` S3) the function is per account by construction.

## 4. Slice 2: outside a session

### 4.1 Options compared

**(a) A pulls over a short authenticated connection to B's session endpoint** (A and B are account identities, B's endpoint is B's own port). The open question is the first message, because A does not know B's minor before any exchange and a 1.0 B must never receive what it does not know.

- *(a1) A normal `Hello` with a marker in an existing field.* Ruled out by the 1.0 host's own decision code (`HostInner::decide` in `swapdesk-session/src/host.rs`): an unknown, expired or foreign resume token "is not an error: the session goes the normal way", an invalid swap token falls through, a request without a stored `UNATTENDED` falls through, and the end of every fall-through is a prompt to B's user (or `BUSY` / `NOBODY_PRESENT`). An empty `requested` also prompts. There is no value of any existing field that a 1.0 host answers without a prompt. It would also overload the meaning of fields whose layout must stay frozen.
- *(a2) A new first message (`GrantsSync`) sent only to a peer known to be minor 1 or newer.* Clean on the wire. Needs the minor before dialling: from discovery (a TXT key; 1.0 ignores unknown keys, `swapdesk-lan/src/txt.rs`), or from the last session. The last session alone is not enough: the owner's own case (paired, grants marked, no session yet) has none, and after the D44 update every pairing is new. Discovery is the source; a minor learned in an authenticated exchange is kept in memory and wins over the TXT value.
- *(a3) A new ALPN (`swapdesk-sync/1`) on the session endpoint.* A 1.0 B refuses it in the TLS handshake, so no application message reaches it and no minor is needed in advance. But the 1.0 peer still receives a ClientHello it was never designed for on every attempt, the ALPN scheme of PROTOCOL.md 1 ("major = the number in the ALPN") and its "both ALPNs refused" rule change, and each attempt to a 1.0 peer costs a failed handshake and a log line there. Kept as the fallback if the TXT key proves unreliable (section 10).

**(b) B pushes to A when its user edits the grants** (B dials A's endpoint). The only option that meets "within seconds" by itself; misses A when A is off the network at that moment, and fails when A's firewall blocks inbound on that network profile.

**(c) A hint in the discovery record** (a per-device change counter, or one counter for the account) that makes A pull. Latency depends on unsolicited mDNS announcements reaching A (multicast on Wi-Fi is lossy); a per-device counter publishes to the whole LAN which devices are paired and when each was edited, and a single counter still publishes when this account's grants change. It adds nothing once (b) exists.

**Combinations.** (a2) and (b) are the same exchange in opposite directions: if the opening message carries the dialer's own grants and the answer carries the responder's, one connection serves as both push and pull. That is the recommendation: **(a2) + (b) as one symmetric exchange, discovery carrying only the protocol minor**, plus three triggers (4.4).

### 4.2 Messages

A **grants sync connection** is a connection to an account's session endpoint (`swapdesk/1`, that account's `ServerConfig`, the same pre-handshake guard and per-machine caps, the same pinned-key handshake and post-handshake gate) whose first control message is `GrantsSync` instead of `Hello`. The endpoint fixes which account answers before the handshake (`ACCOUNTS.md` 5.2): the server presents that account's key and its verifier accepts only keys pinned in that account's store, so a device paired with another account of the same computer fails in the handshake.

| Type | Name | Dir | Fields | Notes |
|---|---|---|---|---|
| 0x0801 | GrantsSync | both, sync connection only | `proto_minor: u16`, `grants: PermissionSet` | minor 1. `grants` = what the sender's account has stored for the other device (`grants_here` of its record). `proto_minor` = the sender's `PROTO_MINOR` |

- Range: a new group 0x08xx, "between paired accounts outside a session", as 0x07xx is the pairing endpoint's.
- Layout example, minor 1, VIEW and CONTROL: frame `48 01 04 01 00 03 00` (type varint 0x0801 in two bytes, length 4, `proto_minor` 1, `grants` 0x0003).
- Order: the client opens the control stream (the client's first bidirectional stream, as in a session) and sends one `GrantsSync`. The server answers with one `GrantsSync`. The client closes the connection with application code `NO_ERROR` (there is no session, so no `Bye`). The server closes it itself 2 s after answering if the client has not (GUESS).
- Violations (fatal, the connection closes with the code): any second message, any message other than `GrantsSync` after the first, any other stream or any datagram: `PROTOCOL_VIOLATION`. `GrantsSync` after a `Hello`, or inside a session: `PROTOCOL_VIOLATION`. `GrantsSync.proto_minor == 0`: `PROTOCOL_VIOLATION` (no minor-0 build can send it). A bad `PermissionSet`: `PROTOCOL_VIOLATION`.
- Timeouts: the server waits for the first message within the session's existing handshake timeout (as for `Hello`); the client gives up 5 s after it started dialling (GUESS).
- Over budget (4.6): the server closes with `BUSY` right after reading the first message, without answering and without applying it.
- Never `ACCOUNT_AWAY`: an account that is not at its console answers syncs as it would at the console (4.3).

### 4.3 What each side does

**The server (responder)**, in `HostService::serve` (`swapdesk-session/src/host.rs`) of the account's core: it reads the first control frame; `Hello` goes to the session actor as today (with `ACCOUNTS.md`'s console rule and host reservation); `GrantsSync` goes to a separate small handler that:

1. checks the per-device budget (4.6);
2. reads `grants_here` of the authenticated device's record in **this account's** store (the device comes from the gate, never from the message);
3. answers `GrantsSync { proto_minor: PROTO_MINOR, grants }`;
4. reports the dialer's value as `HostEvent::PeerGrants { device, grants }`, which the account's hub applies as in 3.4, and the dialer's `proto_minor` as `minor_seen`.

It never prompts, never checks presence or the console owner, never takes the machine-wide host reservation of `ACCOUNTS.md` 5.6, never counts as a live session (so a session of this or another account does not make it `BUSY`), never emits `SessionStarted` or `Ended`, writes no audit entry, and logs one debug line. An account's endpoint listens whenever the account has a pairing (`ACCOUNTS.md` 6), so it answers with nobody logged in, at the lock screen, or while another account holds the console. Proposed for the Mac, pending `ACCOUNTS.md` S9: an app whose session is not the active one still answers syncs (it hosts no picture or input, as that note requires).

**The client (dialer)**, `Connector::sync_grants(addr, device)` of the account's core (`swapdesk-host/src/service/mod.rs`): the same steps as `Connector::connect` up to the handshake (IPv4 only, `find_on_link`, a socket with TTL 1, the device's pinned key read from this account's trust store, never from the address, this account's key as the client certificate). The port is the SRV port of the device's discovery record, else its last port in `peers.bin`, else the default port (`ACCOUNTS.md` 5.2, the same order as a session dial). It sends its `GrantsSync`, reads the answer, applies it (3.4), records `minor_seen`, closes, and lets `peers.bin` record the port under that note's rule (at most once a minute per device). An account whose "receive connections" switch is off (`ACCOUNTS.md` 7.2) still dials syncs, as it still dials sessions (proposal); its peers cannot reach it, so its column learns only through its own dials.

**State per paired device, per account**, in memory only, in a new hub module `ui_hub/grants_sync.rs` (one per account hub):

| Field | Meaning |
|---|---|
| `minor_hint: u16` | from the device's TXT record (`minor`, 0 when absent or bad) |
| `minor_seen: Option<u16>` | from an authenticated `Hello`, `HelloAck` or `GrantsSync` with that device; wins over `minor_hint`. When it says 0 against a hint of 1 or more, the hint is ignored until the record changes (`ACCOUNTS.md` 9.3, "When the authenticated `HelloAck` disagrees") |
| `refused: bool` | a sync to it ended with `PROTOCOL_VIOLATION` or `UNSUPPORTED_VERSION` (it is really minor 0); cleared when its record leaves discovery and comes back, or `minor_seen` says 1 or more |
| `unknown_key: bool` | its handshake refused this account's key (it forgot this account, or it is the 1.0 identity of a computer that moved to 1.1); no automatic sync until this account's user acts on that device (Connect, Pair) or the service restarts |
| `dirty: bool` | this account's grants for it changed and no sync has delivered them yet |
| `push_at: Option<Instant>` | when the debounced push is due |
| `in_flight: Option<Instant>` | when the running sync started |
| `last_ok: Option<Instant>` | the last completed sync |
| `peer_applied_at: Option<Instant>` | when a value the peer initiated (its push, or `StoredGrants`) was last applied |

The hub exposes `known_minor(device) -> u16` = `minor_seen`, else `minor_hint` (0 when `refused`). `ACCOUNTS.md` S6 reads it to decide whether a viewer puts `UNATTENDED` in `requested` (9.3 there), so the two notes share one source of what a peer's minor is.

A device is **eligible** when it is paired with this account, seen on the network with an address that passed the guard, `known_minor(device) >= 1`, not `unknown_key`, and no live session of this account with it already carries `StoredGrants` (a session with a minor-1 peer makes the sync connection unnecessary).

**Ordering rule.** Values on different connections can arrive out of order. The answer to a sync A started is dropped if a value initiated by B (B's push, or `StoredGrants`) was applied after that sync started: B sent it later than the answer. Two pushes from the same account are never in flight at once (the next waits for the previous to end or time out). A residual stale value is display only and the next sync corrects it.

### 4.4 When a sync happens

| Trigger | On | What |
|---|---|---|
| This account's user changes its grants for X (`SetGrants`, also through the elevation step of D7) | the changer | `dirty` and `push_at = now + 1 s` (each further change moves it); at `push_at`, if X is eligible and no session carries it, sync with X. Not eligible: `dirty` waits for the next trigger |
| X appears in discovery (a new record, a new port, or `minor` came or grew), this account's core starts, the network changed | both | sync with X; tie-break so both sides do not dial at once: the one whose **account device ID** is lower (bytewise) dials at once, the other after 3 s and only if no sync with X completed meanwhile |
| This account's user opens X's Permissões tab | that account | sync with X if `last_ok` is older than 30 s or absent (op 39, section 6) |
| A session with X ends | both | nothing: the session's `StoredGrants` already brought the value |

So within seconds: the changer pushes 1 s after the last click, and the exchange is one handshake and one round trip on the LAN. GUESS: under 2 s end to end, proven by the loopback test and measured between a Windows machine and a Mac.

"On the network" means the account's endpoint listens and its record is announced. On Windows the service keeps every account that has a pairing announced from boot, logged in or not (`ACCOUNTS.md` 6); an account withdrawn by its own "receive connections" switch is away. On the Mac the service runs inside each account's app (D27), so an account whose app is closed is away until it opens again (at login when "Abrir ao ligar" is on), and the appearance trigger then brings the column up to date.

### 4.5 What a 1.0 peer sees

- Under D44 no pairing survives the update (`ACCOUNTS.md` 10): a 1.0 peer pairs again with the account it wants, and from then on the rules below hold for that pairing.
- An account record of a 1.1 computer carries `minor=1`; a 1.0 browser ignores unknown keys (`Txt::decode`: "Unknown keys are ignored (a later minor may add some)", and the 1.0 fixture test `a_txt_record_with_an_unknown_key_still_reads_as_1_0_reads_it`).
- A 1.0 peer's record has no `minor` key, so no 1.1 account dials it for a sync, and its `Hello`/`HelloAck` say minor 0, so no 1.1 account sends it `StoredGrants`. It receives the same bytes as from 1.0, except `proto_minor` in `Hello`/`HelloAck`, which is the sender's own minor (PROTOCOL.md 11 already compares that field that way).
- If a forged mDNS record claims `minor=1` for a 1.0 machine, one `GrantsSync` reaches it: its control stream decoder rejects the unknown type, the connection closes with `PROTOCOL_VIOLATION` before any session id exists, so it emits no `Ended`, no audit entry and no prompt (`actor.rs`: `HostEvent::Ended` only for `session_id != 0`); one debug log line. The dialer marks it `refused` and does not try again until its record leaves and comes back. The same forged record cannot make a 1.1 viewer ask a 1.0 host for `UNATTENDED` past the authenticated `HelloAck` (`ACCOUNTS.md` 9.3).
- On the 1.1 side, a 1.0 device's column stays "–" (never learned), with the note of 7.3 if the owner wants one.

### 4.6 Rate limits (GUESS numbers)

| Where | Limit |
|---|---|
| Every connection, before the handshake | unchanged: `retry()` for an unvalidated address, 4 connections at once per address, 16 handshakes in flight, blocked addresses dropped (PROTOCOL.md 1). These caps are per machine, one limiter shared by every account's endpoints (`ACCOUNTS.md` 5.6), so syncs to several accounts of one computer share them |
| Server, per account and authenticated device | one sync in flight (a second: `BUSY`); a budget of 6 per minute with a burst of 3 (past it: `BUSY`, one log line per minute per device) |
| Client, per account and device | one sync in flight; appearance and tab triggers at most once per 30 s; pushes debounced 1 s |
| Client, per account | at most 4 syncs in flight; after a network change, starts spread over 0 to 2 s |
| Store writes, per account and device | only on a changed value, at most one per second |

## 5. Threats

| Threat | Answer |
|---|---|
| What a paired device learns | Only what this account lets **it** do: the server reads `grants_here` of the record the gate authenticated, in the store of the account whose port was dialled, never a device named in a message. Not other devices' grants, not who granted `UNATTENDED`, not the audit log. It could learn the same by asking for a session and reading `SessionGrant`. |
| Another account of the same computer | A sync reaches only the account whose endpoint was dialled; that endpoint presents only that account's key and accepts only keys that account pinned. A device paired with account A that dials account B fails in the handshake. No sync reads or writes another account's store, and nothing tells a peer which other accounts exist (an account without a pairing is not announced, `ACCOUNTS.md` 6). |
| An account away from its console | It still answers syncs. That tells a paired device the account exists and what it grants that device, which the device already knew or could learn; never who is at the computer. |
| An unpaired or blocked device | Nothing changes: a blocked address is dropped before the handshake, an off-link or virtual-interface peer refused before it, an unknown key fails the handshake (F-PAIR-03), and the gate checks the key again before any stream. No sync message is read before all of that. |
| A local user holding an account's port | The dialer's pinned-key check fails, so nothing is learned or written; the account is away for its peers until `ACCOUNTS.md` 5.5 moves it to a free slot. |
| Flooding | Unauthenticated: the existing `retry()`, per-address and handshake caps. Authenticated: the per-device budget of 4.6; disk writes coalesced. A paired device that floods is a paired device the user can forget or block. |
| A paired device that lies about what it grants | Harmless by construction: `grants_there` is display only. Nothing reads it to decide (sessions, swap, resume, unattended entry, `Elevate`, terminal and files all read the deciding account's own `grants_here`). The column says what the other side claims; that side enforces its own store. |
| A device that forgot this account keeps being dialled | Its handshake refuses this account's key (F-PAIR-03 alert in its Activity, rate-limited). This account then stops automatic syncs to it (`unknown_key`), so at most one attempt per start of the core or action of the user. The same holds after the D44 update for a peer that still pins the 1.0 key (`ACCOUNTS.md` 10.3). |
| Forged discovery record (`minor`) | Only decides whether a sync is tried (and, in `ACCOUNTS.md` 9.3, whether `UNATTENDED` is asked, bounded there by the authenticated `HelloAck`). Against a real 1.0 machine: one rejected message and a log line (4.5). Against an impostor address: the pinned-key handshake fails. The minor learned in an authenticated exchange wins over the TXT value. |
| Privacy of the discovery hint | `minor` tells everyone on the LAN the protocol level of this build, next to name, OS and key fingerprint, which mDNS already shows (THREAT-MODEL "Reading what mDNS announces": accepted). It is the same in every account's record, so it says nothing about the accounts. No per-device data and no change counter is published. |
| Replay | QUIC and TLS 1.3 integrity on every connection, no 0-RTT, no resumption (PROTOCOL.md 2.4). A replayed old value would need the pinned key. |
| A standard user | Unchanged: `SetGrants` keeps its administrator rules (D7, the elevation step); the sync only reports what is stored. |

## 6. Disk and the UI pipe

- **Disk:** this note adds no file and changes no format.
  - `PairedDevice.grants_there: Option<PermissionSet>` is part of the format-1 trust record, which under D44 is each account's own `trust.bin` (Windows: the account's folder; Mac: the account's secrets folder; `ACCOUNTS.md` 14.2). Only its value is written.
  - 1.1 never opens or writes the 1.0 root files (`ACCOUNTS.md` 10.2), so a downgrade finds the 1.0 data as it was, and the 1.0 data still loads in the 1.0 fixtures' decoders.
  - `peers.bin` (`PeerPorts`, `ACCOUNTS.md` 14.2) sits beside each account's trust store and belongs to that note: the last session port of each paired device. A sync dials the port it reads there when discovery is silent, and a sync that connects lets it record the port under its rule (at most once a minute per device, the entry goes with Forget). This note adds no field to it unless the owner picks the age of 7.2.
  - Still rejected: any field in the trust record (format 2, which a 1.0 build refuses, and the record format stays shared by the account stores).
- **After the update:** nobody inherits (`ACCOUNTS.md` 10), so `grants_there` starts empty in every account and is learned again after each re-pairing, through the first session or the first sync.
- **Forget:** removes the record, its `grants_there` and its `peers.bin` entry; a value arriving for a device no longer paired is dropped (the gate refuses it anyway).
- **UI pipe (C2.4, C6):**
  - No change for the values: `DevicesChanged` and `ListDevices` already carry `DeviceView.grants_there`, and each account's interfaces reach only their own hub (`ACCOUNTS.md` 14.3, routing by SID).
  - New UI op **39** `RefreshGrants { device_id: str(64) }` (C6 `Command::RefreshGrants`). Ops 35 to 38 are the elevation ops (built), 40 to 49 the file ops, 50 `ElevateSession`. The UI sends it when it opens a paired device's Permissões tab; the service replies `Done` at once (`NotFound` for a device that is not paired with this account) and the result arrives as `DevicesChanged`. Any interface of the account may send it (it reveals nothing the list does not).
  - Only if the owner wants the note of 7.3: `DeviceView` gains `there: u8` (0 current, 1 last known while the device is off the network, 2 the device does not report it (1.0), 3 not learned yet). UI and service are one build, so the field is added in both at once; C6 and C2.4 tables change in the same stage.

## 7. What the column shows

### 7.1 Values
`grants_there` stored: Sim / Não per row, as today. Never learned: "–" in every row, as today. After the 1.1 update every account starts at "–" for every device, since every pairing is new.

### 7.2 Offline or 1.0
Proposal: the column always shows the **last value the other side reported** (it is on disk), and "–" only when nothing was ever reported. An age ("há 3 dias") is not proposed. Under D44 it no longer needs a trust format change: it would be one more field, `grants_there_ms: u64`, in `PeerPort` of `peers.bin`, which is not built yet, so S2 of `ACCOUNTS.md` could define it without a format bump, plus a `DeviceView` field. It stays out of the recommendation as the smaller choice; it is an option for the owner.

### 7.3 A one-line note under the column (texts are the owner's decision)
Options:

1. **No note** (smallest): the column alone; Sim / Não may be stale while the other side is away.
2. **A note by state** (recommended), drafted in pt-BR and en:
   - last known, device off the network: "O {0} está fora da rede: isto é o que ele informou por último." / "{0} is not on the network: this is what it last reported."
   - device on 1.0: "O {0} usa uma versão do Swapdesk que não informa isto." / "{0} runs a Swapdesk version that does not report this."
   - not learned yet, device away: "Aparece quando o {0} estiver na rede." / "Shows up when {0} is on the network."
   - current: no extra line (the existing `ThereNote` stays).
3. **Strict:** "–" whenever the device is not on the network now, even if a value is stored.
4. With option 2 or 3, the age of 7.2 may be added to the first line ("informou há 3 dias").

## 8. Build plan (thin slices, in order)

Each slice ends on its tests green and a commit; the wire tests and the documents go in the slice that changes the wire. `swapdesk-proto` and the documents are the main session's. "Red before" means the test fails, or does not compile, on the tree before the slice. Slice names of `ACCOUNTS.md` are written "A-S1" and so on.

**Order and dependencies.**

| Slice | Needs before it | Shared with `ACCOUNTS.md` |
|---|---|---|
| S1 wire | none | **the same change as A-S1** (one minor) |
| S2 in session | S1 | touches `actor.rs`, `policy.rs`, `host.rs` like A-S4 and A-S6: not in a parallel lane with them |
| S3 write and show | S2 | per account once A-S3 gives one hub per account; built before A-S3, it is the single hub's and A-S3 multiplies it |
| S4 discovery `minor` | none (S1 for `PROTO_MINOR` = 1 to publish anything) | A-S3 makes discovery announce one record per account: the key goes in each. **A-S6 needs S4** |
| S5 responder | S1 | must stay outside A-S3's host gate and the machine reservation of `ACCOUNTS.md` 5.6: a guard test after A-S3 (S9) |
| S6 dialer, triggers, peer-minor state | S4, S5 | reads `peers.bin` (A-S2, A-S3) when built after them, the default port before; **A-S6 needs its `known_minor`** |
| S7 op 39 | S3 | op number set by `ACCOUNTS.md` 14.3; ops 35 to 38 already built |
| S8 the note | the owner's pick of 7.3; S7 | none |
| S9 per-account checks | S6, A-S3, A-S4 | runs on A-S3's machine layer |
| S10 cross-machine | S6 (S8 if picked); with S9 when accounts are built | the owner batch of `ACCOUNTS.md` 15 |

Recommended order: S1 (= A-S1), S4, S2, S3, S5, S6, S7, then `ACCOUNTS.md` S2 onward, then S9, S10, S8 when the owner has picked. S4 and S6 come before A-S6 because that slice reads them.

**S1. Wire (one change with A-S1).** Files: `crates/swapdesk-proto/src/control.rs` (`StoredGrants` 0x000B, `GrantsSync` 0x0801, with A-S1's `Elevate` 0x000C, `ErrorCode::AccountAway` 22, `Desktop::AccountAway` 4; the `min_minor(type_id)` table), `crates/swapdesk-proto/src/lib.rs` (`PROTO_MINOR = 1`), `crates/swapdesk-proto/tests/protocol.rs`, `tests/common/mod.rs` (proptest strategies), `tests/fuzz.rs` (new decoders fed), `tests/wire_1_0.rs` (**`TYPES_AFTER_MINOR_0` lists `(0x000B, 1)`, `(0x000C, 1)` and `(0x0801, 1)`, and `ENUM_VALUES_AFTER_MINOR_0` lists A-S1's `ErrorCode` 22 and `Desktop` 4, in this same change**, as PROTOCOL.md 11 requires), `swapdesk-session/src/policy.rs` (`is_core_type` to 0x000C), `docs/PROTOCOL.md` (1: the minor table; 5.2 step 4; 5.3 the new rows; a new 5.4 "Grants sync connection"; 10: the `minor` TXT key; 11 unchanged except that the lists above name the new types).
Tests, red before (the types do not exist):
- `stored_grants_and_grants_sync_round_trip_byte_for_byte`
- `stored_grants_and_grants_sync_have_the_documented_bytes` (`0B 02 03 00`, `48 01 04 01 00 03 00`)
- `a_grant_set_that_breaks_implies_or_uses_reserved_bits_is_rejected_in_both`
- `every_type_names_the_minor_that_introduced_it` (0x000B, 0x000C and 0x0801 give 1, every 1.0 type gives 0), together with A-S1's `elevate_and_account_away_round_trip_byte_for_byte`, `they_have_the_documented_bytes` and `every_new_value_names_minor_1`
Guard, green before and after: the 1.0 wire fixtures (`wire_1_0.rs` of the three crates; `every_message_type_of_minor_0_has_a_fixture` stays green only because the new types are listed).
Minor numbering: this one change bumps `PROTO_MINOR` to 1 ahead of HEVC (M4-4). HEVC can share minor 1 only while no build announcing minor 1 is published; if a release or preview with minor 1 ships before HEVC, HEVC takes minor 2.

**S2. In session.** Files: `crates/swapdesk-session/src/policy.rs` (`may_receive` both ways, never before the grant), `actor.rs` (peer minor kept and reported; host sends after the grant batch; viewer sends on `SessionGrant`; receipt checks minor and raises the events), `ctx.rs` (peer minor; send on change), `host.rs` (`stored_grants_changed` also sends `StoredGrants` to live sessions with a minor-1 peer; `HostEvent::PeerGrants`), `viewer.rs` (`ViewerEvent::PeerGrants`; the viewer's own stored grants for the host).
Tests in `crates/swapdesk-session/tests/flows.rs`, red before:
- `the_viewer_hears_the_hosts_stored_grants_not_the_sessions` (a prompted session where the pick differs from what is stored)
- `the_host_hears_the_viewers_stored_grants`
- `a_stored_grants_change_during_a_session_reaches_the_peer`
- `stored_grants_before_the_grant_is_a_protocol_violation`
- `stored_grants_from_a_minor_0_peer_is_unsupported_version`
Guard, green before and after: `a_minor_0_peer_is_sent_exactly_the_1_0_sequence` (a viewer and a host announcing minor 0: every frame type the other side receives is a 1.0 type).

**S3. Write and show (service, hub).** Files: `crates/swapdesk-host/src/ui_hub/grants_sync.rs` (new: `apply_peer_grants`, compare, coalesced write, `devices_changed`), `ui_hub/host_ui.rs` (`HostEvent::PeerGrants`), `service/viewer.rs` (`ViewerEvent::PeerGrants`), `ui_hub/ops.rs` (`set_grants` also tells live viewer sessions with that device).
Tests, red before:
- `ui_hub/tests.rs`: `a_peers_stored_grants_reach_the_device_view_and_devices_changed`; `an_unchanged_peer_value_writes_nothing`; `the_device_view_of_a_never_reported_device_is_none`
- `service/tests.rs` (two cores on loopback): `two_cores_in_a_session_learn_each_others_stored_grants`; `a_grant_change_on_the_host_during_a_session_shows_on_the_viewer_within_a_second`

**S4. Discovery `minor`.** The 1.0 TXT fixtures of `swapdesk-lan` build `Txt { name, os, fingerprint, pairing }` in a sample table that must compile unchanged in the tree of tag `v1.0.0`, and `the_fixtures_cover_every_txt_key_os_and_pairing_state_and_the_wake_packets` pins the keys `Txt::encode` writes to the 1.0 set. So `minor` is **not** a field of `Txt` and not written by `Txt::encode`: `txt.rs` gains `KEY_MINOR`, `minor_entry(minor) -> Option<(String, String)>` (none for 0) and `read_minor(entries) -> u16` (ASCII decimal, no leading zero, 1 to 65535; absent or bad reads 0 and never refuses the record); `discovery.rs` appends the entry to each own record and puts the read value in `Peer`; `crates/swapdesk-host/src/ui_hub/presence.rs` carries it in `PresencePeer.proto_minor` (`FakePresence` scripts it).
Tests: `the_minor_key_is_appended_from_1_and_read_leniently` (red); `a_1_0_record_without_minor_reads_as_minor_0` and the `swapdesk-lan` 1.0 fixture tests (guards, green before and after).

**S5. Sync connection: the responder.** Files: `crates/swapdesk-session/src/host.rs` (`serve` reads the first frame and dispatches), a new `crates/swapdesk-session/src/grants_sync.rs` (answer, per-device budget, close rules), `link.rs` if the first-frame read needs exposing.
Tests in a new `crates/swapdesk-session/tests/grants_sync.rs`, red before:
- `a_sync_is_answered_with_the_stored_grants_for_that_device_only`
- `a_sync_never_prompts_never_counts_as_a_session_and_writes_no_audit`
- `a_sync_while_another_session_is_live_is_answered_not_busy`
- `a_second_message_a_stream_or_a_datagram_on_a_sync_connection_is_a_violation`
- `a_sync_flood_from_one_device_is_closed_busy_past_its_budget`

**S6. Sync connection: the dialer, the triggers, the peer-minor state.** Files: `crates/swapdesk-host/src/service/mod.rs` (`Connector::sync_grants`), `ui_hub/grants_sync.rs` (state of 4.3 with `known_minor`, triggers of 4.4, limits of 4.6, the ordering rule), `ui_hub/mod.rs` (presence listener and start feed the scheduler), `ui_hub/ops.rs` (`SetGrants` marks `dirty`), `ui_hub/views.rs` (the port order of 4.3 once `peers.bin` exists).
Tests in `crates/swapdesk-host/src/service/tests.rs`, two cores on loopback with scripted presence, red before unless marked:
- `a_grant_change_reaches_the_other_core_without_a_session_within_seconds` (under 3 s)
- `a_core_that_comes_online_learns_the_grants_changed_while_it_was_away`
- `two_cores_that_see_each_other_at_once_make_one_sync`
- `a_peer_that_answers_a_sync_with_a_violation_is_not_dialled_again_until_it_reappears` (a core forced to minor 0 behind a forged `minor=1`)
- `a_peer_that_forgot_this_core_is_not_dialled_again_automatically`
- `an_answer_older_than_a_push_does_not_overwrite_it` (the ordering rule)
- `known_minor_prefers_the_authenticated_value_over_the_hint` (ui_hub unit test; the input of A-S6's `a_viewer_asks_unattended_only_toward_a_host_known_to_be_minor_1`)
- `a_peer_of_minor_0_is_never_dialled_for_a_sync` (guard, green before and after: a core forced to minor 0 records every frame type it receives)

**S7. Refresh on opening the tab (op 39).** Files: `crates/swapdesk-ipc/src/ops.rs` (op 39 `RefreshGrants`), `crates/swapdesk-ui/src/api.rs` (`Command::RefreshGrants`), `state.rs` (opening a paired device's Permissões tab emits it), `service_backend/convert.rs`, `mock.rs`, `crates/swapdesk-host/src/ui_hub/ops.rs`; `docs/CONTRACTS.md` C2.4 and C6.
Tests, red before: `refresh_grants_op_39_round_trips` (ipc); `opening_permissions_asks_the_service_to_refresh_that_device` (ui state); `refresh_grants_is_limited_to_once_per_30_s_per_device` (ui_hub).

**S8. The note (only after the owner's pick of 7.3).** Files: `DeviceView.there` through ipc, `swapdesk-ui` (`api.rs`, `convert.rs`, `interact.rs`, `strings.rs`), `swapdesk-paint/src/screens_main.rs`; both systems; with the age of 7.2, the `PeerPort` field in A-S2's record.
Tests: `there_rows_and_note_for_each_state` (red); the Mac window checks and the Windows renderer show the same screen; the design canvas board updated.

**S9. Per-account checks (after A-S3 and A-S4).** Files: tests only, unless one fails; then the fix goes where it points.
Tests, on A-S3's machine layer with two accounts and fakes, red before (they cannot be written before A-S3):
- `a_sync_reaches_only_the_account_whose_port_it_dials` (a device paired with A dials B's port: the handshake fails and B's store is untouched)
- `a_grant_change_in_one_account_is_pushed_only_to_that_accounts_devices`
- `a_sync_to_an_account_not_at_the_console_is_answered_without_a_prompt` (A-S4's console rule does not apply)
- `a_sync_takes_no_host_reservation` (an unattended entry of another account at the same moment is admitted)
- `a_device_out_of_discovery_is_synced_at_its_port_in_peers_bin`
- `each_accounts_record_carries_minor`

**S10. Cross-machine check (a Windows machine and a Mac, both 1.1, test installs).** With no session between them: the Mac's account marks permissions for the Windows account; the Windows device view carries them within 5 s; the same in the other direction; then a session, a change during it, and its end. Once accounts are built: a second Windows account paired with the Mac on its own slot sees only its own column change. Printed markers for each step; the owner's eye check of the Permissões column on both screens goes in the milestone's owner batch, with `ACCOUNTS.md`'s.

**Documents in the same stages:** `docs/PROTOCOL.md` (S1, S4, S5), `docs/CONTRACTS.md` (C5.1 row of `grants_there`: "written from `StoredGrants` and `GrantsSync`", per account; C2.4 and C6 op 39 in S7), `docs/SPEC.md` F-PERM-01 (the other direction is shown as the other side last reported it, outside sessions too), `docs/THREAT-MODEL.md` (the rows of section 5), the flow map, `docs/DECISIONS.md` (the owner's pick).

## 9. The owner's decisions

1. The design: recommended, the symmetric sync connection between account keys with push, appearance and tab triggers and the `minor` TXT key (sections 2 to 4). Alternatives in section 10.
2. What the column shows when the other side is off the network or on 1.0 (7.2, 7.3): recommended, the last reported value, no age, and the note by state of 7.3 option 2. Open: the texts of the note, the strict "–", and the age (now possible through `peers.bin`, 7.2).
3. Scope: S1 to S7, S9 and S10 are the order; S8 only with a note.

## 10. Options rejected

- A `Hello` with a marker in an existing field: every such `Hello` makes a 1.0 host prompt its user (4.1 a1).
- Learning the minor only from the last session: devices paired with no session yet (every pairing right after the 1.1 update) never sync.
- A new ALPN for the sync: a 1.0 peer receives a ClientHello it was not designed for on every attempt, and the ALPN rules of PROTOCOL.md 1 change; the fallback if the TXT key proves unreliable.
- `minor` as a field of `Txt` or a key written by `Txt::encode`: the 1.0 TXT fixtures of `swapdesk-lan` would no longer compile in the tag's tree or match the 1.0 key set; discovery appends it instead (S4).
- A change counter in the discovery record: publishes to the LAN when grants change (per device, also which devices are paired) and adds nothing once the push exists.
- Polling every paired device on a timer: traffic and wake-ups with no change behind them; the triggers cover the cases.
- Push only: misses changes made while the other side was away.
- Pull only: the "within seconds" order would need polling.
- Reusing `PermissionsChanged` or `SessionGrant`: they carry the session's grants, which differ from the stored ones in prompted sessions (D8) and after an `Elevate`, and their meaning in 1.0 is fixed.
- Storing the time or the peer's minor in the trust record: trust format 2, which a 1.0 build refuses (the age, if wanted, goes in `peers.bin`, 7.2).
- A sync that answers for every account of the computer on one port: it would need the account named after the handshake, the model `ACCOUNTS.md` 5.2 (O) rejects, and would tell a peer which accounts exist.
- Asking the other side's user: the order is "without a prompt".
