# Threat model

Status: pre-alpha. This describes the mitigations as designed. It is not a statement that Swapdesk is secure. Some mitigations are implemented and tested in code (the plan's stages 9, 10 and 34 cover discovery, the LAN guard, pairing, the trust store and the fixes from a review of them). Others are written down and not built yet. The security review (stage 28), the protocol fuzzing (stage 29) and the soak test (stage 30) have not happened. There has been no independent audit.

Sources: [SPEC.md](SPEC.md) ("Threats", "Data and architecture", flows F-*), [PROTOCOL.md](PROTOCOL.md), [CONTRACTS.md](CONTRACTS.md) (C2 processes and IPC, C5 storage), [DECISIONS.md](DECISIONS.md). Marks as in those files: **UNPROVEN** = no primary source settles it yet and a stage must prove it; **GUESS** = a design number or inference still to confirm.

To report a vulnerability, see [../SECURITY.md](../SECURITY.md).

## Assets

- The screen and the keyboard and mouse of each machine.
- Code execution as SYSTEM (Windows) or root (macOS) on a host.
- The device private key (one Ed25519 key per machine) and the trust store of paired devices and their grants.
- Clipboard contents and files reachable through a session.
- The audit log (its integrity matters as evidence of what happened).

## Trust boundaries

1. **The LAN to the service.** Only pinned keys reach a session, and the key is checked again after every handshake. Pairing is the only unauthenticated path, on its own endpoint, open only in pairing mode. (SPEC "Trust boundaries"; PROTOCOL 1, 2.4.)
2. **User processes to the service or daemon.** Any process of the session's user can act as the interface. The service therefore grants a UI connection only what the user could do by hand, and admin-only changes need an elevated or authorized connection. (CONTRACTS C2.3.)
3. **The service to the helpers.** The service starts them from the per-machine install folder and checks their PID on the pipe. (CONTRACTS C2.1.)
4. **The remote peer to the viewer.** Video is decoded only in the user-level viewer, never in a SYSTEM or root process. (SPEC.)

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
| Malformed messages reaching a privileged parser | `swapdesk-proto` has no `unsafe`, strict length limits, rejects trailing bytes and unknown types. Fuzzing is planned (stage 29, not done). | SPEC STRIDE row E; PROTOCOL 5.1 |
| Tricking the machine into connecting to itself | Own and loopback addresses are refused. | SPEC F-FIND-02 |
| A peer from outside the subnet or on a virtual interface | Physical interfaces only, peers must be on-link in the arrival interface's subnet, checked before the TLS handshake. TTL 1 on QUIC. | SPEC F-FIND-02; PROTOCOL 1 |
| Reading what mDNS announces | Accepted: name, OS and key fingerprint are visible on the LAN, and no secret is in the announcement. | SPEC STRIDE row I |
| A malicious host writing files to the viewer | The viewer accepts a file only as the answer to its own request, into the folder its user chose, with checked names and a size cap. | SPEC F-FILE-01 |

### Another local user (or a standard user) on the same machine

| Threat | Mitigation as designed | Where |
|---|---|---|
| Editing or pre-creating the trust store, adding a key or grant | The service creates the data folder with owner SYSTEM and a protected DACL, moves aside a pre-existing folder with another owner or a reparse point, and never follows reparse points. On Windows the files are DPAPI machine-scope blobs, but that scope does not stop other local users from decrypting, so the DACL is the real protection. | CONTRACTS C5.2; SPEC STRIDE row T |
| Squatting or talking to the pipes or sockets | Explicit DACLs, first-instance pipes (a squatted name is detected), PID and session and SID checks by the service, server checks by the client, audit-token and code-signature checks on macOS. | CONTRACTS C2.1, C2.2 |
| A standard user granting a remote device unattended access or an administrator terminal | Granting `UNATTENDED` or `ADMIN_TERMINAL` needs a local administrator, enforced by the service from an elevated connection, never by the interface. | SPEC F-PERM-01, D7 |
| A standard user using another machine's unattended entry or administrator terminal through this one | Refused by the viewer's service, which offers a prompted session instead. | SPEC F-PERM-03, D10 |
| Reading another user's terminal or files in an unattended session | The logged-in user is asked first. Nobody present means refused. | SPEC F-SESS-02, D13 |
| Replacing helper or daemon binaries | Per-machine install only. The service starts helpers only from a folder users cannot write. The macOS daemon checks its bundle owner and signature at start. | SPEC F-SYS-01, F-SYS-02 |

### A malicious process running as the same user

This attacker is largely **accepted**. Any process of the logged-in user can drive the interface or open its pipe, because the user could do the same by hand. The design limits the damage:

- The service grants a UI connection only what the user could do by hand. Prompted sessions inject input with the user's own medium-integrity token, so the interface path never adds privilege (D11).
- Admin-only changes need an elevated connection that the user must approve through UAC (or the macOS admin password).
- The private key lives only in the service or daemon, not in any user process. A user-level process cannot read it, according to the design (DACL on the data folder, the System keychain on macOS).
- Secrets are not written to logs: private key, pairing nonces, the TLS exporter output, the code, tokens, terminal bytes, text input, clipboard, chat and file contents. A log-capture test is planned for this (F-SEC-01).

What it can still do: anything the user can do, such as start sessions to already-paired devices with the grants already stored, read what the user can read, and act as the user in prompted sessions.

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

## Out of scope

- Anything beyond the LAN. There is no relay, cloud or NAT traversal, by design and permanently.
- An attacker with administrator, SYSTEM or root on a machine, or with kernel code execution there.
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
| `quinn` negotiating RFC 7250 raw public keys end to end (fallback is self-signed certificates with key pinning) | UNPROVEN, risk R1, spiked at the start of stage 10 (see the plan for its outcome) | SPEC R1; PROTOCOL 2.2, 2.3 |
| That a `rustls` or `quinn` default does not re-enable resumption or early data | Risk R11, to be proven by a stage 10 test | SPEC R11; PROTOCOL 2.4 |
| The `ClientHello` ALPN accessor used to refuse a hello offering both ALPNs | UNPROVEN | PROTOCOL 1 |
| Default ACL of `%ProgramData%` and whether a standard user could pre-create the folder or a DPAPI blob | UNPROVEN, GUESS | CONTRACTS C5.2 |
| Whether `WTSQueryUserToken` returns the filtered or the elevated token for an administrator | UNPROVEN, checked in stage 18 | CONTRACTS C2.1 |
| Whether a medium-integrity helper can use the default input desktop (injection for prompted sessions) | UNPROVEN, risk R12 | CONTRACTS C2.1; SPEC R12 |
| The elevated or authorized connection for admin-only changes (UAC-started instance, Authorization Services) | Risk R14 (UNPROVEN for macOS) | SPEC R14; CONTRACTS C2.3 |
| `SoftwareSASGeneration` on Windows 11 Home | Risk R5, tested in stage 26 | SPEC R5 |
| Whether launchd or `SMAppService` refuse a modified daemon binary on macOS | UNPROVEN, risk R13 | CONTRACTS C2.2 |
| macOS peer-token calls and code-signature requirement strings | GUESS and UNPROVEN, settled in stage 24 | CONTRACTS C2.2 |
| What macOS allows at the login window and lock screen | Probes P4 to P6 not yet run (stage 8) | SPEC R4, F-UNAT-02 |
| Whether macOS daemon-owned networking is exempt from Local Network privacy | Assumed from an Apple technical note, risk R10 (probe P8) | SPEC R10 |
| Rate limits, lockout durations, handshake caps and timeouts | GUESS numbers | PROTOCOL 1, 3; CONTRACTS C5.1 |
| The whole implementation against these threats | Security review (stage 28) and fuzzing (stage 29) not done | plan stages 28, 29 |
