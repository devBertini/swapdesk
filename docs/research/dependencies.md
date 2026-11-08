# Dependency audit (decision D9)

The owner's order (D9, 2026-11-07): audit every third-party dependency, keep only what is really needed; the software must be extremely fast and optimized. This file is the evidence and the proposal. Nothing here is decided until the owner picks; D4 and D9 in `docs/DECISIONS.md` stay as they are until then.

Marks as in the SPEC: **UNPROVEN** = no primary source settles it; **GUESS** = a number or inference to confirm in its stage. Every version and date below was read on **2026-11-07** from the crates.io API (`https://crates.io/api/v1/crates/<name>` and `.../<name>/<version>/dependencies`), the npm registry (`https://registry.npmjs.org/<name>`) or the GitHub API (`https://api.github.com/repos/<owner>/<repo>`), unless another address is given.

## How the numbers were made

- A throwaway Cargo workspace under the session's scratch folder (`.../scratchpad/depcheck`), stable toolchain, this PC (Core Ultra 9 275HX, 24 cores). Crates were fetched by cargo; no tool was installed.
- **Transitive count** = unique `name version` pairs in `cargo tree -p <crate> --target <triple> -e normal,build --prefix none --no-dedupe`, the crate itself included, with the features proposed below. Windows = `x86_64-pc-windows-msvc`, macOS = `aarch64-apple-darwin` (resolution only; nothing was built for macOS). Crates shared with others are counted in each row; the union is at the end.
- **Build time** = wall time of a clean `cargo build --release` of a small sample using all 24 cores. **Size** = the `.exe` in bytes. A sample is not Swapdesk; the numbers compare options, they do not predict the final binaries.
- Hot paths (SPEC B1 to B6): capture → encode (helper) → IPC → service → QUIC → service → IPC → decode → present (viewer); audio datagrams; the input stream; and idle CPU of service + helpers (B6, ≤ 1% of all cores).

## 1. Network and crypto

### 1.1 quinn (QUIC)
- **For:** every LAN session and pairing: per-frame uni streams, datagrams for audio and cursor, TLS 1.3, exporter (PROTOCOL.md 1 to 4).
- **std or OS instead?** No. std has no QUIC; Windows MsQuic and Apple Network.framework QUIC are separate per-OS stacks with different APIs (the cross-platform core would split in two). `quinn-proto` alone is sans-I/O and could run on plain threads with a `std::net::UdpSocket`, which removes tokio and `quinn-udp`; cost: we would write timers, the socket loop and stream wakeups ourselves (GUESS: 1 000+ lines) and lose quinn-udp's GSO/GRO and ECN. Not worth it in v1.
- **Maintenance:** 0.11.12, 2026-09-14; repo pushed 2026-11-06; 209 open issues. `quinn-proto` resolved to 0.11.19. Active.
- **License:** MIT OR Apache-2.0.
- **Default features:** `log`, `platform-verifier`, `runtime-tokio`, `rustls-ring`, `bloom` (crates.io API). PROTOCOL.md 1 relies on `rustls-ring` being in the default set, which is true, but the default also brings `rustls-platform-verifier` (OS certificate stores, useless with pinned keys) and `bloom` (`fastbloom`, a token log for NEW_TOKEN address validation: without it quinn's `ValidationTokenConfig` defaults to `NoneTokenLog` and sends 0 tokens, read in `quinn-proto-0.11.19/src/config/mod.rs`). Swapdesk needs neither: no 0-RTT, no Web PKI.
- **Transitive:** 45 crates on Windows, 42 on macOS with `default-features = false, features = ["runtime-tokio", "rustls-ring"]` (includes rustls, ring, tokio, `rand` 0.10, `tracing`, two `getrandom` versions). `tracing` is a hard dependency of quinn (not optional): with no subscriber installed each event is a cheap disabled check (GUESS: negligible on the hot path).
- **Size and build:** the sample "client endpoint + rustls ring config + tokio current-thread" builds clean in 28 s and is 1 390 592 bytes with the proposed release profile (section 4).
- **Hot path:** yes, every video, audio and input byte. quinn is the only QUIC crate here with datagrams, stream priorities and `Endpoint::new_with_abstract_socket` (needed by C7's impairment layer).
- **Verdict: keep with minimal features** `default-features = false, features = ["runtime-tokio", "rustls-ring"]`. Drop `bloom`, `log`, `platform-verifier`.

### 1.2 rustls (TLS 1.3)
- **For:** the TLS 1.3 handshake inside QUIC, RFC 7250 raw public keys, the custom pinned-key verifiers, the exporter.
- **std or OS?** No (quinn speaks rustls only).
- **Maintenance:** 0.23.46, 2026-10-14. Very active. **License:** Apache-2.0 OR ISC OR MIT.
- **Default features:** `aws_lc_rs`, `logging`, `prefer-post-quantum`, `std`, `tls12`. Swapdesk is TLS 1.3 only (PROTOCOL.md 2.2), so `tls12` is dead code; `logging` pulls `log`; `prefer-post-quantum` needs aws-lc-rs.
- **Transitive:** 15 (Windows) / 16 (macOS) with `default-features = false, features = ["ring", "std"]`, ring included. Confirmed with `cargo tree -e features` that only `ring` and `std` are on and no aws-lc-rs is in the graph.
- **Hot path:** yes (packet protection runs through the provider's AES-GCM or ChaCha20). At Swapdesk's rates (tens of Mbit/s, GUESS) either provider is far from a limit.
- **Trade to know:** with ring there is no post-quantum hybrid key exchange (X25519MLKEM768 needs aws-lc-rs per the feature list above). Harvest-now-decrypt-later against a home LAN is a low risk, but it is a security trade the owner should know.
- **Verdict: keep with minimal features** `default-features = false, features = ["ring", "std"]`.

### 1.3 ring vs aws-lc-rs
- **For:** rustls's crypto provider, plus Swapdesk's own crypto: Ed25519 device keys, SHA-256 (pairing commitments, device ID, file check F-FILE-01), and the random bytes for nonces, tokens and pipe names (`ring::rand::SystemRandom`). So no `sha2`, `rand` or `getrandom` direct dependency is needed.
- **Maintenance:** ring 0.17.14, released 2025-03-11 (no release since), but commits keep landing (latest on `main` 2026-11-07, GitHub API). RustSec: RUSTSEC-2025-0010 "ring is unmaintained" applies to versions before 0.17 only ("Versions of ring prior to 0.17 are unmaintained", https://rustsec.org/advisories/RUSTSEC-2025-0010.html, read 2026-11-07); RUSTSEC-2025-0009 (a vulnerability fixed in 0.17.12) is below the current 0.17.14 (https://rustsec.org/packages/ring.html). aws-lc-rs 1.19.0, 2026-10-28, very active.
- **License:** ring "Apache-2.0 AND ISC"; aws-lc-rs "ISC AND (Apache-2.0 OR ISC)". Both fit MIT OR Apache-2.0 distribution (GUESS: no lawyer read).
- **Transitive:** ring 9 (Windows) / 10 (macOS); aws-lc-rs 13 / 13.
- **Build here (measured):** a ring-only sample builds clean in **7 s**, 130 560 bytes. The same sample with aws-lc-rs **fails** on this PC: `aws-lc-sys v0.47.0` reports "NASM command not found or failed to execute". So aws-lc-rs needs NASM here (a new tool), contrary to the older note in research windows.md 10d; ring needs only the MSVC Build Tools, which are present.
- **Hot path:** packet crypto, as 1.2; no measured difference at our rates (GUESS).
- **Verdict: keep ring**; drop aws-lc-rs (it never enters the graph with the features above).

### 1.4 mdns-sd (DNS-SD discovery)
- **For:** announce and browse `_swapdesk._udp` on the guard-approved interfaces (F-FIND-01).
- **std or OS?** Partly. Windows has a native DNS-SD API: `DNS_SERVICE_REGISTER_REQUEST` and `DNS_SERVICE_BROWSE_REQUEST` each take an `InterfaceIndex` ("the interface index over which the service is to be advertised"; minimum client Windows 10; https://learn.microsoft.com/en-us/windows/win32/api/windns/ns-windns-dns_service_register_request and .../ns-windns-dns_service_browse_request, read 2026-11-07). macOS has `dns_sd` (DNSServiceRegister/Browse) in libSystem. That path needs zero crates and coexists with Dnscache and mDNSResponder by design, but it is two code paths, and whether `DnsServiceBrowse` from a session-0 LocalSystem service keeps reporting presence and absence within F-FIND-01's 10 s is **UNPROVEN**. research windows.md 8 already proved mdns-sd's coexistence with Dnscache.
- **Maintenance:** 0.21.7, 2026-11-02; repo pushed 2026-11-03. Active. **License:** Apache-2.0 OR MIT.
- **Default features:** `async`, `logging`. With `default-features = false`: 13 crates (Windows) / 12 (macOS), of which `mio`, `socket2`, `windows-sys` and `fastrand` are shared with tokio and quinn; unique ones: `flume`, `spin`, `lock_api`, `scopeguard`, `if-addrs`, `socket-pktinfo`.
- **Idle CPU (B6):** mdns-sd runs one thread that blocks in `poll` until its earliest timer (`service_daemon.rs`, `run`). One timer is the interface re-check: `IP_CHECK_INTERVAL_IN_SECS_DEFAULT = 5`, which enumerates interfaces every 5 s; `set_ip_check_interval(0)` turns it off (read in `mdns-sd-0.21.7/src/service_daemon.rs`). Swapdesk's own `NetInterfaces` watcher (C3) already knows when interfaces change and can call `enable_interface`/`disable_interface`, so set the interval to 0.
- **Hot path:** no (discovery only).
- **Verdict: keep with minimal features** `default-features = false`; call `set_ip_check_interval(0)`. Alternative for later: the OS DNS-SD APIs, if stage 9 finds mdns-sd costly at idle.

### 1.5 tokio (async runtime)
- **For:** quinn's runtime in the service/daemon (both roles), the service's local IPC servers (`tokio::net::windows::named_pipe`, Unix sockets) and timers. In the UI, Tauri brings its own tokio anyway.
- **Could a smaller runtime or plain threads do?** C3 already keeps every platform trait synchronous on dedicated OS threads, so the helpers and agents (capture, encode, input, audio, PTY, clipboard, files) need **no async runtime at all**: they talk to the service over blocking IPC. Only the service/daemon needs a runtime, because quinn needs one. quinn's other runtime is `smol`: 31 crates (Windows) / 32 (macOS) against tokio's **7 / 6** with `rt, net, time, sync`, so smol is larger here. Plain threads would mean `quinn-proto` without quinn (1.1). So tokio is the smallest workable choice.
- **Maintenance:** 1.54.1, 2026-10-21. Very active. **License:** MIT.
- **Features:** tokio's default set is empty. Minimal for the service: `rt` (current-thread scheduler), `net`, `time`, `sync`. Add `macros` only if `tokio::select!` is used (adds the `tokio-macros` proc-macro; `syn` is already in the graph). Dev only: `test-util` (C7 `FakeClock` uses `tokio::time::pause`) and `macros` for `#[tokio::test]`. Not needed: `fs` (std::fs on a thread), `process` (helpers are launched with `CreateProcessAsUserW`), `signal`, `io-util` (quinn's streams have their own `read`/`write_all`), `rt-multi-thread`.
- **Thread side of the channels:** C3's bounded `tokio::sync::mpsc` with `blocking_send`/`blocking_recv` lives inside the service only (where both worlds meet). Crates used by helpers should use `std::sync::mpsc::SyncSender` as C3's trait signatures already do, so they do not depend on tokio.
- **Scheduler (GUESS, measure in stage 14):** start the service on a current-thread runtime; QUIC at 1080p60 is a few thousand packets per second, well within one core. A multi-thread runtime defaults to one worker per core (24 here); if one thread is not enough, use `worker_threads(2)`. Parked workers cost no CPU, so B6 is not at stake either way.
- **Feature unification:** in a workspace build, tokio's features are the union of all members' (Tauri turns on `rt-multi-thread`, `fs`, `io-util`, measured with `cargo tree -e features -i tokio`). Unused code is removed by the linker, so this costs build time, not runtime (GUESS). Building each binary with `-p` keeps its own set.
- **Verdict: keep with minimal features** `default-features = false, features = ["rt", "net", "time", "sync"]` in the service/daemon and `swapdesk-trust`, `swapdesk-session` and friends; dev: `test-util`, `macros`. No tokio in helpers, agents or platform crates.

## 2. Media, terminal and OS bindings

### 2.1 opus (audio codec)
- **For:** 48 kHz stereo system audio (F-AUDIO-01, PROTOCOL.md 7.2, B3).
- **Which binding:** `opus` 0.4.0 (2026-08-23, MIT/Apache-2.0, https://github.com/SpaceManiac/opus-rs) depends only on `opusic-sys ^0.7.3` (resolved 0.7.7, 2026-11-04, BSD-3-Clause, https://github.com/DoumanAsh/opusic-sys). The README of `opus` says: "By default, you need `cmake` and a C compiler"; "These requirements come from opusic-sys" (https://github.com/SpaceManiac/opus-rs, read 2026-11-07).
- **Does it build libopus from C?** Yes. opusic-sys: "By default, `libopus` is bundled"; CMake "is needed when using the `bundled` feature"; bundled libopus is **1.6.1**; without `bundled` it looks for a system library (`OPUS_LIB_DIR`, `OPUS_LIB_STATIC`) (https://github.com/DoumanAsh/opusic-sys, read 2026-11-07). Transitive: 9 crates both targets (`cmake`, `cc`, `opusic-sys` and their helpers).
- **Measured here:** a clean build **fails** without CMake on PATH ("is `cmake` not installed?"). CMake ships inside the VS 2022 Build Tools (`.../Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe`), and pointing the `CMAKE` variable at it gets further, but the build then failed in the long scratch path with MSBuild `FTK1011: could not create the new file tracking log file` (GUESS: the path length; the repo's own `target/` path is shorter, worktrees are longer). On the MacBook, CMake is not part of Xcode (GUESS); whether it is installed there is **UNPROVEN**. So the encode/decode speed of this binding could not be measured today.
- **Alternatives:**
  - `audiopus` 0.2.0 (2019-10-11) / `audiopus_sys` 0.2.2 (2021): stale. Drop.
  - `opus-rs` 0.1.41 by restsend (2026-11-05, BSD-3-Clause, https://github.com/restsend/opus-rs): pure Rust ("A pure-Rust implementation of the Opus audio codec (RFC 6716), ported from the reference C implementation (libopus 1.6)"), no C toolchain. Its own README lists "Known gap: at wideband, opus-rs codes fewer excitation bits than libopus at the same gains", tonality analysis not ported, and a changelog of recent fuzz-found panics and an undefined-behaviour fix. Too young for v1 (read 2026-11-07).
  - **No codec:** this is LAN-only. Raw 16-bit PCM 48 kHz stereo is 1.536 Mbit/s (arithmetic), small next to video, with zero codec delay. Costs: no packet-loss concealment (a lost datagram is an audible click), 5 ms frames to fit a ~1 200-byte datagram (200 packets/s, arithmetic), and a change to PROTOCOL.md 7.2. Worth keeping as the fallback if the CMake build is a problem.
  - OS codecs (Media Foundation, AudioToolbox Opus): availability on every Windows edition is **UNPROVEN**; not pursued.
- **Hot path:** yes (audio, B3). libopus is the reference and is SIMD-optimized (opusic-sys passes `OPUS_X86_MAY_HAVE_AVX2=ON`, seen in the build command).
- **Verdict: keep `opus` 0.4 (bundled libopus 1.6.1)**, and add to `.cargo/config.toml` an `[env] CMAKE` pointing at the Build Tools CMake (a repo file, not an install) once stage 17 proves the build in the repo path. If it still fails, the fallback is raw PCM (owner decision, since PROTOCOL.md 7.2 changes).

### 2.2 portable-pty (terminal)
- **For:** user and admin shells in a PTY (F-TERM-01/02).
- **OS instead?** Yes, on both. Windows: ConPTY (`CreatePseudoConsole`, `ResizePseudoConsole`, a `STARTUPINFOEX` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`), all in `windows-sys`. macOS: `openpty` + `posix_spawn` (or `fork`/`login_tty`), all in `libc` (GUESS: about 150 to 250 lines per OS, in the `*-win`/`*-mac` crates that already allow `unsafe`). The account question is already solved by the process model: the user helper runs with the user's token and the system helper as SYSTEM, so a plain spawn in the current process gives the right account.
- **Maintenance:** 0.9.0, released **2025-02-11**, no release in 21 months. The code lives in the wezterm monorepo; the `pty` folder had commits in 2026 ("pty: Replace winapi with windows-sys", 2026-08-25), unreleased (GitHub API, commits under `pty/`). **License:** MIT.
- **Transitive:** **25 (Windows) / 18 (macOS)**, the heaviest per feature: `winapi` 0.3, `winreg` 0.10, `shared_library`, `lazy_static`, `anyhow`, `nix` 0.28, `serial2` (a serial-port crate Swapdesk never uses), `downcast-rs`, `filedescriptor`, `shell-words`, and duplicate `bitflags` 1 and 2, `thiserror` 1 and 2 (dependency list on crates.io).
- **Hot path:** no; the terminal is a byte relay.
- **Verdict: replace with own ConPTY / openpty code** in `swapdesk-sys-win` and `swapdesk-sys-mac` (or a `*-pty-*` crate), behind C3's `PtySpawner`. This removes about 20 unique crates and the old `winapi`. D4 named portable-pty, so this is the owner's decision; keeping it is safe but heavy.

### 2.3 windows-rs: `windows` vs `windows-sys`
- **For:** every Windows API. COM-based ones need the `windows` crate (interfaces, `Release`, `QueryInterface`): DXGI Desktop Duplication, Direct3D 11, Media Foundation, WASAPI (`IMMDevice`, `IAudioClient`), `INetworkListManager`, the firewall's `INetFwPolicy2`. Flat C APIs fit `windows-sys` (raw, no COM wrappers): sockets, IP Helper, services (SCM), named pipes and ACLs, tokens and `CreateProcessAsUserW`, WTS, `SendInput`, hooks, DPAPI, `SendSAS` (found in `windows-sys 0.61.2`, `Win32/Security/Authentication/Identity`), power requests.
- **Maintenance:** `windows` 0.62.2 and `windows-sys` 0.61.2, both 2025-10-06. The repo moved to a common **0.100.0** on 2026-09-03 (release 74; `windows-core`, `windows-link`, `windows-bindgen`, `windows-result`, `windows-strings` are published at 0.100.0), but issue #4867 says "We may initially publish the release without `windows` and `windows-sys`", that `windows-sys` "replaces its `Win32_*` and `Wdk_*` features and modules with lowercase, header-based ones", and that "The umbrella crates are also a poor fit for most reusable libraries", suggesting to "generate any other required APIs locally with `windows-bindgen`" (https://github.com/microsoft/windows-rs/issues/4867, read 2026-11-07). So the next upgrade renames every feature and module path. **License:** MIT OR Apache-2.0.
- **Transitive:** `windows` with 5 COM features: 15 crates (it carries `windows-core`, `windows-implement`/`windows-interface` proc-macros, `windows-future`, `windows-collections`, `windows-numerics`, `windows-threading`, `windows-strings`, `windows-result`, `windows-link`). `windows-sys`: 2 (`windows-sys`, `windows-link`), and 12 crates of the graph already depend on `windows-sys 0.61.2`.
- **Build (measured):** `windows` (DXGI, D3D11, Media Foundation, Audio, COM) + `windows-sys` (9 feature groups): **14 s** clean dev build, **1 s** incremental. Fine for the 30 s loop.
- **Option: `windows-bindgen`** (0.100.0): generate only the functions and interfaces Swapdesk calls into a checked-in `bindings.rs`; runtime needs `windows-link` (and `windows-core` for COM, GUESS: the docs page names only `windows-link`), https://docs.rs/windows-bindgen/latest/windows_bindgen/, read 2026-11-07. Smallest compile and immune to the umbrella renames, at the cost of a generation step (run it from a test or an xtask, not from every `build.rs`, so normal builds do not parse metadata).
- **Hot path:** yes (capture, encode, decode, present, input), but the crates are zero-cost bindings; the speed is the OS's.
- **Verdict: keep with minimal features.** `windows` 0.62.2 only in the COM crates (`capture-win`, `codec-win`, `render-win`, `audio-win`, `net-win`), each listing only its features: `Win32_Graphics_Dxgi`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Direct3D` (capture, codec, render); `Win32_Media_MediaFoundation`, `Win32_System_Com` (codec); `Win32_Media_Audio`, `Win32_System_Com_StructuredStorage` (audio, GUESS for property stores); `Win32_Networking_NetworkListManager`, `Win32_NetworkManagement_WindowsFirewall` (net-win); `Win32_UI_WindowsAndMessaging` (render). `windows-sys` 0.61.2 in the flat crates (`sys-win`, `input-win`, `secret-win`, and `net-win` for IP Helper and sockets). The exact feature list is each stage's to confirm (GUESS above). When 0.100 umbrella crates appear, prefer `windows-bindgen` over a rename-everything upgrade (owner's call then).

### 2.4 objc2 and its framework crates
- **For:** every macOS API (ScreenCaptureKit, VideoToolbox, CoreMedia, CoreVideo, CoreGraphics events, AVFoundation renderer, SMAppService, AppKit for the renderer view, dispatch queues, blocks).
- **Alternative:** the `screencapturekit` crate needs Swift (RESEARCH.md); hand-written `extern` declarations for Objective-C would be unsafe guesswork. objc2 is the standard.
- **Maintenance:** `objc2` 0.6.5 (2026-10-05, MIT); framework crates 0.3.2 (2025-10-04, "Zlib OR Apache-2.0 OR MIT"; `objc2-foundation` MIT); `block2` 0.6.2 (MIT); `dispatch2` 0.3.1 (2026-02-26). Repo pushed 2026-10-17.
- **Default features:** framework crates turn almost everything on. objc2's docs: "Most features are enabled by default", library developers should "disable them by adding `default-features = false`", which "can vastly help reduce the compile-time of the framework crates" (https://docs.rs/objc2/latest/objc2/topics/about_generated/cargo_features/index.html, read 2026-11-07). With defaults, `objc2-screen-capture-kit` alone pulls 22 crates (AVFoundation, CoreImage, Metal, QuartzCore, MediaToolbox, ImageIO, UniformTypeIdentifiers, CoreAudio...), `objc2-app-kit` 17, `objc2-video-toolbox` 15.
- **Crates the contracts need that D4 does not name one by one** (all of the objc2 family, same license and release line): `objc2-foundation`, `objc2-core-foundation`, `objc2-core-graphics`, `objc2-screen-capture-kit`, `objc2-core-media`, `objc2-core-video`, `objc2-video-toolbox`, `objc2-av-foundation` (`sampleBufferRenderer`), `objc2-service-management`, `objc2-application-services` (AX trust check), `objc2-app-kit` (renderer view, NSPasteboard for the clipboard), `block2`, `dispatch2`; and three more that C2.2/C3/C5 imply: `objc2-security` (System keychain SecItem, C5.2; code-signature check of a socket peer, C2.2), `objc2-system-configuration` (`NetInterfaces`, C3), `objc2-io-kit` (`keep_awake` power assertion) or `objc2-audio-toolbox`/`objc2-core-audio` (`AudioSink`), all 0.3.2 on crates.io (read 2026-11-07). Which exact ones each stage needs is **UNPROVEN** until stages 23 and 24.
- **Hot path:** yes on macOS (capture, encode, decode, present), as thin bindings.
- **Verdict: keep with minimal features**: every framework crate with `default-features = false, features = ["std", <only the classes used>]`, e.g. `objc2-screen-capture-kit` with `SCStream`, `SCShareableContent`, `SCError` and the bridges it needs. Confirm with the owner that "objc2 and its framework crates" in D4 covers the family members listed above.

### 2.5 libc vs rustix vs std (macOS peer credentials and PTY)
- **For:** C2.2 reads the peer's uid and pid on `ui.sock` and `agent.sock` (`LOCAL_PEERCRED`, `LOCAL_PEERPID`, and `LOCAL_PEERTOKEN` for an audit token for the code-signature check), and the PTY replacement (2.2) needs `openpty`/`posix_spawn`.
- **std:** `UnixStream::peer_cred` is "a nightly-only experimental API" behind `peer_credentials_unix_socket`, tracking issue #42839 (https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html, read 2026-11-07). Not usable on stable.
- **rustix** 1.1.6: `socket_peercred` exists only under `#[cfg(linux_kernel)]` (read in `rustix-1.1.6/src/net/sockopt.rs`), so it does not cover macOS. Drop.
- **libc** 0.2.191 (2026-10-16, MIT OR Apache-2.0): has `LOCAL_PEERCRED`, `LOCAL_PEERPID`, `LOCAL_PEERTOKEN`, `struct xucred`, `getpeereid`, `getsockopt`, `openpty` (read in `libc-0.2.191/src/unix/bsd/apple/mod.rs` and `bsd/mod.rs`). 1 crate, no dependencies, and **already in the graph** through tokio, mio, quinn-udp, mdns-sd and the objc2 family, so a direct dependency adds zero crates and zero build time.
- **Hand-written `extern "C"`:** possible (zero crates), but struct layouts like `xucred` and `audit_token_t` copied by hand are an error source for no gain, since libc is compiled anyway.
- **Hot path:** no (connection setup only).
- **Verdict: keep libc** (macOS crates only).

## 3. UI shell, frontend and serialization

### 3.1 Tauri 2 vs a native shell
- **For:** `Swapdesk.exe`/`Swapdesk.app`: the approved HTML design (docs/design), prompts, banner, settings; the video itself is drawn by a native child view (C6, R7), so Tauri is not on the video path unless the stage 15 spike picks WebCodecs.
- **Maintenance:** `tauri` 2.12.5 (2026-10-21), `wry` 0.57.1 (2026-10-23), `tauri-build` 2.7.4, `tauri-cli` 2.12.4 (2026-11-06); repo pushed 2026-11-06. **License:** Apache-2.0 OR MIT.
- **Default features:** `wry`, `compression`, `common-controls-v6`, `dynamic-acl`, `x11`, `dbus` (crates.io API); `x11`/`dbus` are Linux-only and `dynamic-acl` "does not enable additional features" (https://docs.rs/crate/tauri/latest/features, read 2026-11-07).
- **Measured (minimal app: one command, one HTML page, `default-features = false, features = ["wry", "common-controls-v6"]`, `custom-protocol`):**
  - Transitive: **203 crates (normal), 219 with build dependencies**, both targets (includes its own tokio with `rt-multi-thread`, `serde_json`, `muda` menus, `png`, `regex`, `toml`, `url`, `http`, `cookie`, `glob`, `webview2-com` or `objc2-web-kit`).
  - Clean release build: **2 min 09 s**; exe **3 770 880 bytes** (fat LTO, one codegen unit, panic abort).
  - Memory after 6 s, three runs: the Swapdesk-side process **5.1 to 5.2 MB private** (23.6 to 26.3 MB working set); WebView2 adds **6 processes, 136 to 147 MB private** (318 to 341 MB working set, which double-counts shared pages).
  - Startup: `WaitForInputIdle` returned in 58 to 67 ms warm and 1 822 ms on the very first run (WebView2 profile creation). Time to first paint was not measured (**UNPROVEN**).
  - Idle CPU over 10 s: host process 0 ms, WebView2 16 to 63 ms, i.e. at most 0.03% of the 24 cores (1% = 2 400 ms per 10 s). And the UI is not part of B6 (service + helpers only).
  - macOS WKWebView costs: not measurable here (**UNPROVEN**; stage 23).
- **Alternatives:**
  - **Native per OS** (Win32 or WinUI through windows-rs, AppKit through objc2): lowest memory (GUESS: tens of MB), no WebView, but two UIs for about 14 screens × light/dark × pt-BR/en, and the approved HTML design would be redrawn twice. Weeks of extra work (GUESS).
  - **Rust-native GUI** (egui, iced, slint): one codebase, no WebView; their own trees are also large (GUESS: 100 to 300 crates, not measured), the approved design would be rebuilt in another styling model, and accessibility, IME and text quality vary by toolkit. Every one of them is a new dependency for the owner's yes.
- **Verdict: keep Tauri 2 with minimal features** `default-features = false, features = ["wry", "common-controls-v6"]` (drop `compression`, `dynamic-acl`, `x11`, `dbus`), plus `custom-protocol` for release builds; `build.removeUnusedCommands` and a capability file listing only Swapdesk's commands ("only include commands that you use in the ACL", https://v2.tauri.app/concept/size/, read 2026-11-07). `common-controls-v6` only matters if native message boxes or dialogs are used (GUESS); drop it if not. The real cost is ~140 MB of WebView2 memory while the window is open; the owner should know it (section 6).

### 3.2 xterm.js
- **For:** the Terminal panel (F-TERM-01).
- **Maintenance:** `@xterm/xterm` 6.0.0 (2025-12-22), `@xterm/addon-fit` 0.11.0, `@xterm/addon-webgl` 0.19.0 (same day); repo pushed 2026-10-29. **License:** MIT. 0 npm dependencies each.
- **Size (measured from jsDelivr):** `lib/xterm.mjs` 344 970 bytes (88 019 gzip), `css/xterm.css` 7 112, `addon-fit.mjs` 1 967, `addon-webgl.mjs` 126 558 (35 749 gzip).
- **Alternatives:** writing a VT parser and renderer is out of proportion; hterm is less maintained (GUESS, not read).
- **Hot path:** no, and it loads only when the panel opens (dynamic `import()`).
- **Verdict: keep** `@xterm/xterm` + `@xterm/addon-fit`; add `addon-webgl` only if the DOM renderer is measured too slow for fast output (GUESS: not needed for a remote shell).

### 3.3 Frontend toolchain
- **Options:**
  - **A. TypeScript only, no bundler, no framework (recommended).** `tsc` emits ES modules into `ui/dist`; the WebView loads them from Tauri's embedded assets (local, no network, so many small modules are cheap; GUESS: no measurable startup difference for a UI of this size). xterm.js is copied as one vendored `.mjs`. The C7 mock backend runs in any static server (for example `python -m http.server`), with `?mock=<scenario>`. One npm dev dependency: `typescript` 7.0.2 (2026-07-08, Apache-2.0; 7.x ships native per-platform binaries as optional packages, seen in its npm `dependencies`). Costs: no hot reload (a manual refresh), no minification (irrelevant offline). `@tauri-apps/api` (2.12.4, 0 dependencies) can be replaced by `app.withGlobalTauri` (GUESS that `Channel` is reachable there; check in stage 11).
  - **B. Vite + TypeScript, no framework.** `vite` 8.3.6 (2026-11-05, MIT) with 5 direct dependencies (`rolldown`, `lightningcss`, `postcss`, `picomatch`, `tinyglobby`), native binaries included. Gains hot reload and minified output; nothing of it ships in the app. A larger supply chain at build time only.
  - **C. A framework (React, Svelte, Solid).** About 14 screens of mostly static layout with small state: a framework adds a runtime and build plugins for no measured need.
- **Hot path:** none (the UI never carries video with the native renderer).
- **Verdict: A** (TypeScript only); B is the fallback if the manual refresh slows stage 11. Owner decision (D9 lists Vite + TypeScript as waiting on this audit). `@tauri-apps/cli` 2.12.4 (or `cargo install tauri-cli`) is still needed for `tauri build` and the NSIS installer; that is a tool install for the main session in M8.

### 3.4 serde (and serde_json)
- **For:** only the C6 DTOs (`#[derive(Serialize, Deserialize)]` for Tauri commands and events), the per-user UI settings JSON (C5.1) and the C6 JSON fixtures test. Not in `swapdesk-proto`, the service or the helpers.
- **Maintenance:** serde 1.0.229 (2026-07-18), serde_json 1.0.154 (2026-10-11), both MIT OR Apache-2.0.
- **Transitive:** serde with `derive` 7; serde_json 5. Both are already required by Tauri itself, so in `swapdesk-app` they add zero crates.
- **Verdict: keep, in `swapdesk-app` only.** `serde_json` becomes a direct dependency there (settings file, fixtures test): zero new crates, but a new name in `[workspace.dependencies]`, so the owner's yes (project rule).

### 3.5 Hand-written wire codec vs postcard
- **postcard** 1.1.3 (2025-07-24, MIT OR Apache-2.0) with serde derive: 11 crates (`postcard`, `cobs`, `serde`, `serde_derive`, `syn`, `thiserror`...). Its varint is not the QUIC varint of PROTOCOL.md 9.2, so adopting it changes the wire format; limits, canonical checks and the fixed media headers (7.x) stay hand-written anyway (PROTOCOL.md 9.1).
- **Hand-written:** zero crates; keeps `swapdesk-proto` dependency-free, so its unit tests and proptest round trips compile in seconds (the 30 s loop) and its fuzz targets have no third-party code inside the SYSTEM/root parser (SPEC threat E).
- **Hot path:** the per-frame headers and input records are fixed layouts read in place; no serde machinery on the hot path either way.
- **Verdict: keep the hand-written codec; drop postcard.**

### 3.6 proptest (dev only)
- **For:** the round-trip and rejection properties of PROTOCOL.md 9.3, and later fuzz-like coverage.
- **Maintenance:** 1.12.0 (2026-10-10), MIT OR Apache-2.0. **Default features:** `std`, `fork`, `timeout`, `bit-set` (`fork`/`timeout` bring `rusty-fork`, `wait-timeout`, `tempfile`).
- **Transitive:** with `default-features = false, features = ["std"]`: proptest plus `rand`, `rand_core`, `chacha20`, `rand_xorshift`, `regex-syntax`, `num-traits`, `bitflags`, `unarray`, `core_detect` (most shared with quinn's `rand`).
- **Verdict: keep (dev only), minimal features** `default-features = false, features = ["std"]`. Add `bit-set` only if a strategy needs it.

## 4. Crates the contracts imply that D4 does not list

| Need (where) | Implied crate | Proposal | New crate? |
|---|---|---|---|
| Random nonces, tokens, pipe names (PROTOCOL 3, C2.1) | `rand`, `getrandom` | `ring::rand::SystemRandom` | no |
| SHA-256 (pairing, device ID, F-FILE-01) | `sha2` | `ring::digest` | no |
| Self-signed fallback certificate (PROTOCOL 2.3) | `rcgen` | fixed DER template, already the plan | no |
| Seeded RNG in `ImpairedSocket` (C7) | `rand`/`rand_pcg` (already transitive through quinn) | a 10-line SplitMix64 or PCG in `swapdesk-netsim` | no |
| macOS peer credentials (C2.2) | `libc` | keep (2.5) | yes, owner's yes (D9 lists it) |
| macOS keychain, code signature, SystemConfiguration, power, audio out (C2.2, C3, C5) | `objc2-security`, `objc2-system-configuration`, `objc2-io-kit`, `objc2-audio-toolbox` or `objc2-core-audio` | objc2 family (2.4) | confirm D4 covers them |
| `block2`, `dispatch2`, `objc2-foundation`, `objc2-core-foundation` (research macos.md probe) | same | objc2 family (2.4) | confirm D4 covers them |
| UI settings JSON and C6 fixtures (C5.1, C6) | `serde_json` | keep in `swapdesk-app` (3.4) | name only, zero crates |
| File pickers: "Escolher arquivos", "Exportar registro" (F-FILE-01, F-SESS-04) | `tauri-plugin-dialog` 2.8.4 (brings `rfd` 0.18.0) | own `IFileOpenDialog`/`IFileSaveDialog` (`windows`) and `NSOpenPanel`/`NSSavePanel` (`objc2-app-kit`) behind a small trait | plugin would be new; own code is not |
| "Abrir ao ligar", single instance | `tauri-plugin-autostart`, `tauri-plugin-single-instance` | own code: HKCU Run key / SMAppService login item; a named mutex or the UI pipe | no |
| Windows service plumbing (C2.1) | `windows-service` | `windows-sys` Services API | no |
| Diagnostics logging | `log`/`tracing-subscriber` | none in v1: quinn's `tracing` events stay off; Swapdesk's audit log is its own format; a debug subscriber would be a new crate | no |
| Fuzzing (stage 29) | `libfuzzer-sys` (cargo-fuzz) | proptest first, as the plan allows; libfuzzer-sys only on the owner's yes in stage 29 | later |
| CMake for libopus (2.1) | a build tool, not a crate | the CMake inside VS Build Tools through `[env] CMAKE` | no install |

## 5. Build profiles

### 5.1 Measured on the network sample (quinn + rustls/ring + tokio), clean release builds, 24 cores

| Variant | opt | lto | cgu | panic | debug / strip | Build | `.exe` bytes |
|---|---|---|---|---|---|---|---|
| Cargo's default release | 3 | false (thin local) | 16 | unwind | none / none | 17 s | 1 950 720 |
| Thin LTO | 3 | thin | 16 | abort | none / symbols | 18 s | 1 540 096 |
| Fat, unwind | 3 | fat | 1 | unwind | none / symbols | 26 s | 1 718 784 |
| Fat, abort | 3 | fat | 1 | abort | none / symbols | 28 s | 1 390 592 |
| **Fat, abort, line tables** | 3 | fat | 1 | abort | line-tables-only / none | 27 s | **1 390 080** (+ 4.9 MB PDB beside it) |
| Size-first | s | fat | 1 | abort | none / symbols | 22 s | 1 134 592 |

On Windows (MSVC) debug info lives in the separate `.pdb`, so line tables cost nothing in the `.exe` (row 5 vs row 4). The minimal Tauri app with fat LTO took 2 min 09 s clean. Cargo's defaults and the meaning of each key: https://doc.rust-lang.org/cargo/reference/profiles.html (read 2026-11-07): release defaults are `opt-level = 3`, `lto = false` ("thin local LTO"), `codegen-units = 16`, `panic = 'unwind'`, `strip = "none"`, `debug = false`; "Tests, benchmarks, build scripts, and proc macros ignore the `panic` setting"; package overrides "cannot specify the `panic`, `lto`, or `rpath` settings"; macOS `split-debuginfo` defaults to `unpacked` when debug info is on.

### 5.2 Proposal

```toml
# Shipping profile: what the budgets B1 to B6 are measured on.
[profile.release]
opt-level = 3            # speed: the hot path has Rust code (QUIC, framing, any CPU colour conversion)
lto = "fat"              # -29% size against Cargo's default on the sample, cross-crate inlining
codegen-units = 1
panic = "abort"          # -19% size against unwind on the sample; see the trade below
debug = "line-tables-only"  # file:line in crash backtraces; on Windows it goes only into the .pdb
split-debuginfo = "packed"  # macOS: a .dSYM beside the binary instead of objects left in target/
strip = "none"           # keep symbol names for in-process backtraces; ship without the .pdb/.dSYM
incremental = false

# Local performance work: release code with full symbols, never shipped.
[profile.profiling]
inherits = "release"
debug = true

# The unit loop (reached cases under 30 s).
[profile.dev]
debug = "line-tables-only"   # GUESS: faster link than full debug info; set `debug = true` locally to step with variables

[profile.dev.package."*"]
opt-level = 1                # GUESS: third-party crates (ring's C, quinn) run faster in tests; built once, cached
debug = false

[profile.dev.build-override]
opt-level = 0                # build scripts and proc macros compile fast
```

- **What hurts debugging:** `panic = "abort"` ends the whole process on any panic, so a panic in one tokio task of the service ends every session (with `unwind`, tokio would turn it into a `JoinError` and the rest would live). For a SYSTEM/root process that parses untrusted input, ending at once and being restarted by the SCM recovery actions or launchd `KeepAlive` is defensible, but the service must then hold no state that only memory keeps (the trust store and audit log are already on disk). `lto = "fat"` + `codegen-units = 1` make stepping in a debugger confusing (inlining) and slow the release build (28 s vs 17 s on the sample; minutes for Tauri). `line-tables-only` gives backtraces but no variables; use the `profiling` profile for that. `opt-level = 3` vs `"s"`: "s" was 18% smaller on the sample, but nothing here is size-bound; keep 3.
- **Tests are not affected** by `panic = "abort"` (Cargo ignores it for tests), so proptest and `#[should_panic]` keep working.
- **Option, not proposed by default:** `[profile.release.package.swapdesk-proto] overflow-checks = true` makes arithmetic overflow in the network parser a panic (then an abort) instead of a wrap; GUESS: negligible cost, since the codec does little arithmetic. The codec should use checked arithmetic anyway.
- **Not proposed:** `target-cpu=native` (the binary runs on other machines). `x86-64-v2` would be safe if Windows 11 is the minimum (**UNPROVEN** both the minimum and the gain).
- **Dev profile check:** the `dev.package."*"` and `dev` lines are GUESSES; stage 6/7 measures one clean and one incremental `cargo nextest run -p swapdesk-proto` and `-p swapdesk-netsim` with and without them, and keeps whichever meets the 30 s budget.

## 6. Summary

| Candidate | What for | Latest (date) | License | Transitive (Win / Mac) | Hot path | Verdict |
|---|---|---|---|---|---|---|
| quinn | QUIC | 0.11.12 (2026-09-14) | MIT OR Apache-2.0 | 45 / 42 (incl. rustls, ring, tokio) | yes | keep, `default-features = false`, `runtime-tokio`, `rustls-ring` |
| rustls | TLS 1.3, RPK | 0.23.46 (2026-10-14) | Apache-2.0 OR ISC OR MIT | 15 / 16 | yes | keep, `default-features = false`, `ring`, `std` |
| ring | crypto provider, Ed25519, SHA-256, RNG | 0.17.14 (2025-03-11; commits to 2026-11-07) | Apache-2.0 AND ISC | 9 / 10 | yes | keep |
| aws-lc-rs | alternative provider | 1.19.0 (2026-10-28) | ISC AND (Apache-2.0 OR ISC) | 13 / 13; fails here without NASM | yes | drop |
| mdns-sd | DNS-SD | 0.21.7 (2026-11-02) | Apache-2.0 OR MIT | 13 / 12 | no | keep, `default-features = false`, IP check off |
| tokio | quinn runtime, IPC servers | 1.54.1 (2026-10-21) | MIT | 7 / 6 | yes (service) | keep, `rt`, `net`, `time`, `sync`; dev `test-util`, `macros`; not in helpers |
| smol | alternative runtime | 2.0.2 (2024-09-07) | Apache-2.0 OR MIT | 31 / 32 | | drop |
| opus (+ opusic-sys, libopus 1.6.1) | audio codec | 0.4.0 (2026-08-23) / 0.7.7 (2026-11-04) | MIT/Apache-2.0; BSD-3-Clause | 9 / 9; needs CMake | yes | keep; build via Build Tools CMake; PCM fallback |
| opus-rs (pure Rust) | alternative | 0.1.41 (2026-11-05) | BSD-3-Clause | 1 | | not now (too young) |
| portable-pty | PTY | 0.9.0 (2025-02-11) | MIT | 25 / 18 | no | replace with own ConPTY / openpty |
| windows | COM APIs | 0.62.2 (2025-10-06); 0.100 announced | MIT OR Apache-2.0 | 15 | yes | keep, per-crate features, COM crates only |
| windows-sys | flat Win32 | 0.61.2 (2025-10-06) | MIT OR Apache-2.0 | 2 | yes | keep, per-crate features |
| objc2 + framework crates | macOS APIs | 0.6.5 (2026-10-05) / 0.3.2 (2025-10-04) | MIT; Zlib OR Apache-2.0 OR MIT | 2; SCK with defaults 22 | yes | keep, `default-features = false` everywhere |
| libc | macOS peer credentials, PTY | 0.2.191 (2026-10-16) | MIT OR Apache-2.0 | 1 (already in graph) | no | keep (macOS) |
| rustix | alternative | 1.1.6 (2026-10-27) | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 / 4; no macOS peercred | | drop |
| Tauri 2 | UI shell | 2.12.5 (2026-10-21) | Apache-2.0 OR MIT | 203 / 203 (219 with build) | no (native video view) | keep, `default-features = false`, `wry`, `common-controls-v6` |
| xterm.js | terminal view | 6.0.0 (2025-12-22) | MIT | 0 npm deps | no | keep (+ addon-fit) |
| Vite | dev server, bundler | 8.3.6 (2026-11-05) | MIT | 5 direct npm deps | no | drop for now (fallback) |
| TypeScript | type checking, emit | 7.0.2 (2026-07-08) | Apache-2.0 | native binaries | no | keep (dev) |
| serde (+ serde_json) | C6 DTOs, UI settings | 1.0.229 / 1.0.154 | MIT OR Apache-2.0 | 7 / 5 (both already in Tauri) | no | keep, `swapdesk-app` only |
| postcard | alternative codec | 1.1.3 (2025-07-24) | MIT OR Apache-2.0 | 11 | | drop (hand-written codec) |
| proptest | property tests | 1.12.0 (2026-10-10) | MIT OR Apache-2.0 | ~11, mostly shared | dev | keep (dev), `default-features = false`, `std` |

**Union with the proposed features** (`cargo tree`, unique name + version): service side (network, crypto, mDNS, opus, portable-pty, libc, `windows`) = **85 crates on Windows, 97 on macOS** (objc2 crates counted with their default features, so the real macOS number is lower); with Tauri = **264 / 281**. Replacing portable-pty removes about 20 of them. The service, helpers and agents never link Tauri.

### Final minimal dependency list

Runtime, shipped:
1. `quinn` 0.11: `default-features = false, features = ["runtime-tokio", "rustls-ring"]` (service/daemon side crates).
2. `rustls` 0.23: `default-features = false, features = ["ring", "std"]`.
3. `ring` 0.17 (also the source of Ed25519, SHA-256 and random bytes).
4. `tokio` 1: `default-features = false, features = ["rt", "net", "time", "sync"]` (service/daemon and network crates only).
5. `mdns-sd` 0.21: `default-features = false`.
6. `opus` 0.4 (bundled libopus 1.6.1, CMake at build time).
7. `windows` 0.62 (COM crates, per-crate features) and `windows-sys` 0.61 (flat crates, per-crate features).
8. `objc2` 0.6, `block2`, `dispatch2` and the `objc2-*` framework crates, each `default-features = false` with only the classes used.
9. `libc` 0.2 (macOS crates).
10. `tauri` 2: `default-features = false, features = ["wry", "common-controls-v6"]`, with `serde` (`derive`) and `serde_json` in `swapdesk-app` only.
11. Frontend: `@xterm/xterm`, `@xterm/addon-fit`.

Build and dev only: `typescript`; `proptest` (`default-features = false, features = ["std"]`); `tokio` `test-util` and `macros` in dev-dependencies; `tauri-build`; `@tauri-apps/cli` or `tauri-cli` for the installer (M8).

Dropped: `portable-pty` (own ConPTY/openpty), `aws-lc-rs`, `smol`, `rustix`, `postcard`, Vite (fallback only), a UI framework, `rand`/`sha2`/`rcgen`, Tauri plugins (own pickers and autostart).

### For the owner's decision
1. **portable-pty:** replace it with Swapdesk's own ConPTY (Windows) and openpty (macOS) code, about 20 fewer crates and no `winapi` (recommended); or keep it as D4 says.
2. **Frontend toolchain:** TypeScript only, no bundler and no framework (recommended); or Vite + TypeScript.
3. **libc** for the macOS peer checks and the PTY (recommended: it is already compiled through tokio and objc2); or hand-written declarations.
4. **The objc2 family:** confirm that D4's "objc2 and its framework crates" covers `objc2-security`, `objc2-system-configuration`, `objc2-io-kit` and the audio crates, as well as `block2` and `dispatch2`.
5. **serde_json** as a direct dependency of `swapdesk-app` (settings and C6 fixtures; zero new crates).
6. **File pickers:** Swapdesk's own native dialogs (recommended); or `tauri-plugin-dialog` (with `rfd`).
7. **Audio, only if the libopus build fails in stage 17:** raw PCM on the LAN instead of Opus (changes PROTOCOL.md 7.2); or the pure-Rust `opus-rs`.
8. **Known costs to accept:** about 140 MB of private WebView2 memory while the Swapdesk window is open; no post-quantum key exchange with ring; and `panic = "abort"` in release, so a panic restarts the service and ends every session.
9. **The windows-rs 0.100 move:** when the renamed umbrella crates come out, move to locally generated bindings (`windows-bindgen`) instead of renaming every feature and path; that decision comes when they are published.
