# Threat model

Status: version 1.0.0. This describes the mitigations as designed and says which are tested. It is not a statement that Swapdesk is secure. Discovery, the LAN guard, pairing, the trust store, TLS and the local IPC are implemented and tested in code, and a focused security review of pairing, the LAN guard, grants, the IPC ACLs and the wire decoder ran before 1.0. It found and 1.0 fixes: a UI of another Windows session (fast user switching) could pair, grant, connect or answer a swap that made this machine the host, now reserved to the console session's interface; a standard user could widen what an unattended device may do, now an administrator's change; and blocked addresses are now dropped before the handshake. The wire and IPC decoders are property-tested with random input (`proptest`); coverage-guided fuzzing and a long soak have not happened. There has been no independent audit.

**Version 1.1: each account has its own Swapdesk (D44, D45; design in [ACCOUNTS.md](ACCOUNTS.md)).** On Windows every account has its own device key, trust store, settings, activity and port pair, kept by the service in a folder of its own, and a computer is paired with one account, never with the whole machine. The sections below describe that model; where a statement is about 1.0 it says so. Version 1.1 also learns what the other side allows outside sessions ([GRANTS-SYNC.md](GRANTS-SYNC.md)) and can raise an entry without a prompt to the sign-in screen, the lock screen and administrator prompts after a confirmation (the elevation step).

**macOS in version 1** runs as one app of the logged-in user (README, Architecture): there is no root daemon, so the statements below about a daemon, root-owned sockets and the System keychain describe the later design. On the Mac the device key and trust store are files readable only by that user (mode 0600 in a 0700 folder), the same reach as DPAPI user scope: any process of that user can read them.

Sources: [SPEC.md](SPEC.md) ("Threats", "Data and architecture", flows F-*), [PROTOCOL.md](PROTOCOL.md), [CONTRACTS.md](CONTRACTS.md) (C2 processes and IPC, C5 storage), [DECISIONS.md](DECISIONS.md). Marks as in those files: **UNPROVEN** = no primary source settles it yet and a stage must prove it; **GUESS** = a design number or inference still to confirm.

To report a vulnerability, see [../SECURITY.md](../SECURITY.md).

## Assets

- The screen and the keyboard and mouse of each machine.
- Code execution as SYSTEM (Windows) or root (macOS) on a host.
- The device private key (one Ed25519 key per account on Windows and per user on the Mac) and the trust store of paired devices and their grants, and what separates one account's data from another's.
- Clipboard contents and files reachable through a session.
- The audit log (its integrity matters as evidence of what happened).
- The release signing key (version 1.1, D53): an Ed25519 key that signs the update manifest. Whoever holds it, together with control of what github.com serves for the repository, could install code as SYSTEM on every Windows install and with the Mac app's permissions on every Mac. It lives on the maintainer's computer under DPAPI with one offline copy, never on GitHub; a second, offline-only key is compiled in for rotation.

## Trust boundaries

1. **The LAN to the service.** Only pinned keys reach a session, and the key is checked again after every handshake. Pairing is the only unauthenticated path, on its own endpoint, open only in pairing mode. (SPEC "Trust boundaries"; PROTOCOL 1, 2.4.)
2. **User processes to the service or daemon.** Any process of the session's user can act as the interface. The service therefore grants a UI connection only what the user could do by hand, and admin-only changes need an elevated or authorized connection, and what reaches SYSTEM on another machine needs the system's own confirmation on the viewer's machine. (CONTRACTS C2.3.)
3. **The service to the helpers.** The service starts them from the per-machine install folder and checks their PID on the pipe. (CONTRACTS C2.1.)
4. **The remote peer to the viewer.** Video is decoded only in the user-level viewer, never in a SYSTEM or root process. (SPEC.)
5. **One account to another on the same machine.** Each account has its own key, store and port pair, the service routes each interface connection to its account's own hub, a host session is admitted only while its account owns the console, and the helpers hold capture and input at every desktop switch until the service has read who is at the console. (ACCOUNTS.md 5, 7.)
6. **The internet to the updater (version 1.1, D53).** The only traffic beyond the LAN: the service (Windows, SYSTEM) or the app (Mac) fetches the signed release manifest from github.com over the system's HTTPS client, and the package after a person's click. Nothing it receives is read before the manifest's Ed25519 signature is checked against the compiled release keys, and the package is opened only after its size and SHA-256 match the signed manifest. It is not a peer connection, has no listening socket and can neither open a session nor change a grant; the LAN guard does not apply to it and is not changed by it. ([UPDATES.md](UPDATES.md) 4, 11.)

## Attackers considered

### Another host on the LAN

Capabilities assumed: it can send any packet to the machine's ports, spoof addresses on the segment (including ARP spoofing), sniff and alter traffic on a shared Wi-Fi, flood.

| Threat | Mitigation as designed | Where |
|---|---|---|
| Man in the middle during first pairing | Pairing only when both users opened "Parear" within 2 minutes. Commitment before reveal, then a 6-digit code over both keys and the TLS exporter, compared by both users. A man in the middle gets one guess, 1 in 10^6 (RFC 6189 argument). Three failures in 15 minutes lock pairing for 15 minutes, and the failed attempt is written to disk before the nonce is sent. | SPEC F-PAIR-01, F-PAIR-02, STRIDE row S; PROTOCOL 3 |
| Unsolicited pairing | The pairing endpoint is closed outside pairing mode, so nothing answers. | PROTOCOL 1, 3; SPEC D12 |
| Impersonating a paired device, or skipping the key check by resuming TLS | TLS 1.3 with mutual authentication and only pinned keys. No tickets, no session storage, no 0-RTT. After every handshake the key is looked up in the trust store again before any stream is accepted. | PROTOCOL 2.2, 2.4; SPEC F-PAIR-03 |
| An unknown key tries a session | Refused in the handshake, logged (rate-limited), alert shown in Activity with a block action. | SPEC F-PAIR-03 |
| Altered or replayed packets | QUIC and TLS 1.3 integrity. Swap and resume tokens are single-use, bound to the device, with 10 s and 60 s lifetimes. | SPEC STRIDE row T; PROTOCOL 6.3, 6.4 |
| Handshake, pairing, stream, transfer or audit-log floods | Caps on handshakes and connections per address, QUIC retry and ignore under load, one stream on the pairing endpoint, explicit receive windows and a buffered-bytes budget per connection, no allocation from a peer-declared length, caps on terminals, transfers and chat, rate-limited unauthenticated audit entries. Most numbers are marked GUESS. | SPEC STRIDE row D; PROTOCOL 1, 9 |
| Malformed messages reaching a privileged parser | `swapdesk-proto` has no `unsafe`, strict length limits, rejects trailing bytes and unknown types. Property tests feed random bytes to every network and IPC decoder (`crates/swapdesk-proto/tests/fuzz.rs`, `crates/swapdesk-ipc/tests/fuzz.rs`); coverage-guided fuzzing is not done. | SPEC STRIDE row E; PROTOCOL 5.1 |
| Tricking the machine into connecting to itself | Own and loopback addresses are refused. | SPEC F-FIND-02 |
| A peer from outside the subnet or on a virtual interface | Physical interfaces only, peers must be on-link in the arrival interface's subnet, checked before the TLS handshake. TTL 1 on QUIC. | SPEC F-FIND-02; PROTOCOL 1 |
| Reading what mDNS announces | Accepted: name, OS and key fingerprint are visible on the LAN, and no secret is in the announcement. | SPEC STRIDE row I |
| A malicious host writing files to the viewer | The viewer accepts a file only as the answer to its own request, into the folder its user chose, with checked names and a size cap. | SPEC F-FILE-01 |
| A paired device learns more than it should from the grants sync | It learns only what this account lets it do: the server reads the grants of the record the gate authenticated, in the store of the account whose port was dialled, never a device named in a message; not other devices' grants, not who granted `UNATTENDED`, not the log. It could learn the same by asking for a session. | GRANTS-SYNC 5 |
| An unpaired or blocked device, or a flood, on the grants sync | Nothing changes: a blocked address is dropped before the handshake, an unknown key fails the handshake, the gate checks the key again before any stream, and no sync message is read before that. Authenticated floods meet a budget per device (closed `BUSY`) and disk writes are coalesced. | GRANTS-SYNC 5; PROTOCOL 5.4 |
| A paired device that lies about what it grants | Harmless by construction: what it reports is display only. Nothing reads it to decide a session, swap, resume, unattended entry, `Elevate`, terminal or files; each side enforces its own store. | GRANTS-SYNC 5 |
| A forged discovery record (`minor`, or another account's port) | The `minor` key only decides whether a sync is tried and whether `UNATTENDED` is asked, bounded by the authenticated `HelloAck`, which wins; against an impostor address the pinned-key handshake fails, and the client checks the key it expects, as in 1.0. | GRANTS-SYNC 5; PROTOCOL 10 |
| A device that forgot this account keeps being dialled, or pins the 1.0 key after the update | Its handshake refuses this account's key (the unknown-device alert, rate-limited) and this account stops automatic syncs to it. A peer that still pins the 1.0 key refuses the new key until its user pairs again (ACCOUNTS.md 10.3). | GRANTS-SYNC 5 |
| Counting the computer's accounts or learning their names from the LAN | Only an account with a pairing or an open pairing mode listens and is announced, and "receive connections" off withdraws it. Each announced name holds the Windows account name unless the user edited it, which mDNS shows already; accepted, and the line under the name field says so. The `minor` key tells everyone the protocol level next to name, OS and fingerprint, the same in every record. | ACCOUNTS 6 |

### Another local user (or a standard user) on the same machine

| Threat | Mitigation as designed | Where |
|---|---|---|
| Editing or pre-creating the trust store, adding a key or grant | The service creates the data folder with owner SYSTEM and a protected DACL, moves aside a pre-existing folder with another owner or a reparse point, and never follows reparse points. On Windows the files are DPAPI machine-scope blobs, but that scope does not stop other local users from decrypting, so the DACL is the real protection. | CONTRACTS C5.2; SPEC STRIDE row T |
| Squatting or talking to the pipes or sockets | Explicit DACLs, first-instance pipes (a squatted name is detected), PID and session and SID checks by the service, server checks by the client, audit-token and code-signature checks on macOS. | CONTRACTS C2.1, C2.2 |
| A standard user granting a remote device unattended access or an administrator terminal | Granting `UNATTENDED` or `ADMIN_TERMINAL` needs a local administrator, enforced by the service from an elevated connection, never by the interface. For a standard account an administrator approves at the UAC prompt in that account's session ("over the shoulder"): the approval is a single-use pass for the exact action on the asking account's connection, and the approver is recorded in the log (`GrantsChanged.by`). | SPEC F-PERM-01, D7, D45; ACCOUNTS 8.2 |
| Account A's interface (or any process of A) reads, changes, forgets or uses account B's pairings, grants, settings or activity | The pipe server routes by the client's SID to A's own hub, which holds no reference to B's data; A's core presents A's key, so B's peers refuse it in the handshake. | CONTRACTS C2.1, C2.3; ACCOUNTS 7.1 |
| An administrator account Z sees or changes B's Swapdesk data, or approves something B did not ask for | No path in the product: it cannot list, read, change, forget or use another account's data. A ticket exists only for an action B's own interface asked for, on B's connection, in B's Windows session. On disk the folder's DACL lists Administrators, so Windows' own tools can read it: out of scope (administrators are the trusted computing base), as in 1.0. | ACCOUNTS 4.3 |
| A peer paired with account A reaches B's session (B at the console, locked or disconnected), or gets a frame or an input across a sign-in | Refused at entry (`ACCOUNT_AWAY`, `NOBODY_PRESENT` to a 1.0 peer); a live session pauses and ends after 60 s; helpers attach only to the core that may host; at every desktop switch the helper holds capture and input until the service has read the console's user afresh (an unreadable user counts as another account). With `UNATTENDED` at the sign-in screen the peer sees the account list and can try passwords (Windows' lockout policy applies); signing in to B makes B the owner and A's session pauses. | ACCOUNTS 5.1 |
| A peer paired with A learns whether A is at the computer | `ACCOUNT_AWAY` tells it; accepted, since it is A's own paired device. | ACCOUNTS 5.1 |
| A standard user pre-creates `accounts\<SID>` for an account that has none yet | The parent folder admits only SYSTEM and Administrators, and the move-aside rule also runs per subfolder. | CONTRACTS C5.2 |
| Another local user holds an account's ports so it cannot receive connections | The slot ports are bound exclusively at start where the probe allows; else the service names the program holding the port in its trace log and moves the account to the next free slot. Remains: a local denial of service by holding every free port; accepted. | ACCOUNTS 5.5 |
| One account's session locks, sleeps or restarts the machine, or keeps it awake | Lock and keep-awake act within that account's sessions. Sleep and restart need `CONTROL` in a session the console owner (or the sign-in screen) admitted, as for a person at the keyboard; accepted. | ACCOUNTS 13 |
| The 1.0 data left on disk is served again or reaches an account | 1.1 never opens `identity.bin` or `trust.bin` at the root; only a downgrade to 1.0 serves them again, as 1.0 always did (README, "Updating from 1.0"). | ACCOUNTS 10 |
| The trace log read for every account's device IDs and session times | Administrators only; never the never-logged items (F-SEC-01). | CONTRACTS C2.3 |
| Replacing helper or daemon binaries | Per-machine install only. The service starts helpers only from a folder users cannot write. The macOS daemon checks its bundle owner and signature at start. | SPEC F-SYS-01, F-SYS-02 |

### A malicious process running as the same user

This attacker is largely **accepted**. Any process of the logged-in user can drive the interface or open its pipe, because the user could do the same by hand. The design limits the damage:

- The service grants a UI connection only what the user could do by hand. Prompted sessions inject input with the user's own medium-integrity token, so the interface path never adds privilege (D11).
- Admin-only changes need an elevated connection that the user must approve through UAC (or the macOS admin password).
- What reaches SYSTEM on **another** machine needs the same kind of approval on this one: `Elevate` ("Elevar controle") and an administrator terminal outside a session this machine elevated each take the system's own confirmation (D45), so code running as the user cannot use a stored grant to get a SYSTEM shell or the sign-in screen on a peer with no prompt on either machine. On the Mac in 1.x the dialog guards against a person at the keyboard, not against code in the app's own process, until a root daemon exists (D54).
- The private key lives only in the service or daemon, not in any user process. A user-level process cannot read it, according to the design (DACL on the data folder, the System keychain on macOS).
- Secrets are not written to logs: private key, pairing nonces, the TLS exporter output, the code, tokens, terminal bytes, text input, clipboard, chat and file contents. A log-capture test is planned for this (F-SEC-01).

What it can still do: anything the user can do, such as start sessions to already-paired devices with the grants already stored (including plain unattended entry, which controls the peer's desktop as the peer's user), read what the user can read, and act as the user in prompted sessions.

### A stolen, lost or forgotten device

- The device holds the keys of the machines paired with it. If someone else uses it logged in as the owner, they have what the owner's grants gave that device on the other machines.
- "Forget" on the other machine deletes the key and grants, ends any session and invalidates resume tokens (F-PAIR-04). This is the revocation mechanism. It must be done on every machine that paired with the lost one.
- A device with `UNATTENDED` is the most sensitive case, since it connects without a prompt. Grant it sparingly.
- Disk theft: the device key and trust store are protected by DPAPI (machine scope) and the folder DACL on Windows, and by the System keychain and root-only files on macOS. Machine-scope DPAPI ties the data to the PC but does not stop code running on that PC, and offline attacks on an unencrypted disk are not covered by any statement in the sources. **Not claimed.**

## Mitigations beyond the table

- A visible "being controlled" banner with Disconnect during every session, and an append-only audit log kept by the service, with unauthenticated entries rate-limited and kept apart from session and grant entries (F-SESS-03, F-SESS-04).
- No media, clipboard, file or terminal stream before the session is granted, and only for granted permissions. A new device has no grants. A revoked grant stops the feature within 1 s (F-PERM-02).
- Panic isolation: a panic in the service ends one session only, and the interface and helpers abort and are restarted (D15, F-SYS-03).
- Dependencies are kept to a minimal audited list (D14).

## The update channel

Version 1.1 checks GitHub for a new release and installs it on a person's click (D53, design in [UPDATES.md](UPDATES.md)). The request carries no identifier of the user or the machine, and the switch in Settings > Updates turns it off; with it off the updater sends nothing.

| Threat | What stops it, or what remains |
|---|---|
| Taken-over GitHub account (assets replaced, a malicious tag built and attested) | The manifest key is not on GitHub: no signature, no install. Remains: the attacker can withhold releases (users stay on their version) and can make the manual-download path serve anything |
| Leaked release key | Useful only together with control of what github.com serves for this repository, or a broken TLS. Rotation to the offline key ([SECURITY.md](../SECURITY.md)). Remains: installs that have not updated keep trusting the leaked key until they do |
| Man in the middle | TLS validated by the system; even a broken TLS cannot forge the signature. A replayed old manifest only delays |
| Rollback to an old vulnerable version | The version must be greater than the running one; a rolled-back version is not offered again by itself |
| Disk filled by a huge download | The size comes from the signed manifest and bounds the download, under a 256 MiB cap; free space is checked first; partial files are deleted on cancel or failure |
| A standard user racing the downloaded file (Windows) | The file, the staging folder and the applier's copy live in a folder only SYSTEM and Administrators can open; the moved files are checked by the install policy |
| A same-user process on the Mac | Can touch the update folder between the check and the swap: accepted, it can already replace the app; the staged bundle's code signature is checked again against the running app's just before the swap |
| A standard user or a remote viewer starting an install | Allowed by design: only a newer signed release, and live sessions end only after the confirmation |
| A peer on the LAN | No wire message about updates exists; the LAN guard is unchanged |
| A new version that does not start | Rolled back within 60 seconds on both systems |
| Parser bugs in a privileged process | Manifest and signature are bounded and parsed only after the signature check; the package is opened only after its hash matches; both readers are property-tested and fuzzed |

## Out of scope

- Anything beyond the LAN, except the update check, covered above. 1.x has no relay, cloud or NAT traversal. A later mode beyond the LAN (D36) gets its own threat model before it is built, and this one stays true for the LAN-only default.
- An attacker with administrator, SYSTEM or root on a machine, or with kernel code execution there. That includes an administrator reading another account's Swapdesk folder with Windows' own tools: the product gives an administrator no way into another account's data, and the disk does not stop one who uses the operating system's.
- Physical access to an unlocked machine.
- Hiding that Swapdesk is present from the LAN: names, OS and key fingerprints are visible through discovery.
- FileVault pre-boot on macOS (unreachable by design).
- Traffic analysis. Encrypted traffic size and timing are not hidden.
- Side channels in the cryptographic libraries, which the project relies on (`rustls`, `ring`, `quinn`).
- Denial of service by someone who can simply jam the radio or cut the cable.

## Not yet verified

These are marked UNPROVEN or GUESS in the design documents. They are risks, not claims.

| Item | Status | Reference |
|---|---|---|
| `quinn` negotiating RFC 7250 raw public keys end to end (fallback is self-signed certificates with key pinning) | Settled: raw public keys are used end to end, Ed25519 only, tested in `swapdesk-trust` | SPEC R1; PROTOCOL 2.2, 2.3 |
| That a `rustls` or `quinn` default does not re-enable resumption or early data | Settled: resumption and early data are off, asserted by tests in `swapdesk-trust` | SPEC R11; PROTOCOL 2.4 |
| The `ClientHello` ALPN accessor used to refuse a hello offering both ALPNs | UNPROVEN | PROTOCOL 1 |
| Default ACL of `%ProgramData%` and whether a standard user could pre-create the folder or a DPAPI blob | UNPROVEN, GUESS | CONTRACTS C5.2 |
| Whether `WTSQueryUserToken` returns the filtered or the elevated token for an administrator | The user helper was measured not elevated (`TokenElevationType` limited) on an administrator account | CONTRACTS C2.1 |
| Whether a medium-integrity helper can use the default input desktop (injection for prompted sessions) | UNPROVEN, risk R12 | CONTRACTS C2.1; SPEC R12 |
| The elevated or authorized connection for admin-only changes (UAC-started instance, Authorization Services) | Built (`ELEVATION.md`, `ELEVATION-MAC.md`; the threat table is `ELEVATION.md` 5). Windows: UAC, the elevated instance's own window and a single-use pass bound to the asking connection and the exact action. macOS 1.x: the check runs in the app's own process (D54): it stops a person at the keyboard who is not an administrator, not code running as the same user. Still a hand check: the real UAC prompt and the real macOS password dialog (owner batch) | SPEC R14; CONTRACTS C2.3; D54 |
| Whether the service names the new user as soon as a sign-in changes the desktop of the console session (the fence fails closed meanwhile), and what a deleted account's SID does in the lookup | UNPROVEN: unit proofs with scripted helpers and owner, by hand at the sign-in screen and with a scratch account | ACCOUNTS 5.1, 4.1 |
| Exclusive binding of the slot ports against another local user's squatting, and two users' mDNS responders on one Mac | GUESS and UNPROVEN: probes with two accounts, owner batch | ACCOUNTS 5.5, 12 |
| `SoftwareSASGeneration` on Windows 11 Home | Risk R5, not tested yet (unattended access is not in 1.0) | SPEC R5 |
| Whether launchd or `SMAppService` refuse a modified daemon binary on macOS | UNPROVEN, risk R13 | CONTRACTS C2.2 |
| macOS peer-token calls and code-signature requirement strings | Not used in 1.0 (no daemon on macOS) | CONTRACTS C2.2 |
| What macOS allows at the login window and lock screen | Probes P4 to P6 not yet run; not in 1.0 | SPEC R4, F-UNAT-02 |
| Whether macOS daemon-owned networking is exempt from Local Network privacy | Assumed from an Apple technical note, risk R10 (probe P8) | SPEC R10 |
| Rate limits, lockout durations, handshake caps and timeouts | GUESS numbers | PROTOCOL 1, 3; CONTRACTS C5.1 |
| The whole implementation against these threats | A focused review ran before 1.0; coverage-guided fuzzing and an independent audit have not | |
