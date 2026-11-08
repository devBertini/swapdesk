# Research

Primary-source notes behind the design, each fact with its address, read date and quote. UNPROVEN marks what no primary page settles; those items get a probe in their stage.

- Windows: `docs/research/windows.md` (capture on the secure desktop, hybrid GPU, Media Foundation, input and SAS, audio loopback, firewall, mDNS, installer, QUIC/TLS, pairing crypto).
- macOS: `docs/research/macos.md` (ScreenCaptureKit, TCC, Local Network, SMAppService, input, VideoToolbox, signing, the M0-mac probe).

## Verdicts that changed the plan

- The service, not the helper, calls `SendSAS`; SoftwareSASGeneration on Windows 11 Home is tested in M6.
- Capture runs on the adapter that enumerates each output; encoding goes through one Media Foundation encoder chosen per adapter LUID (NVIDIA, Intel and the Windows software MFT are present on this PC). No NVENC SDK in v1.
- 4:4:4 is out of v1 (the Media Foundation decoder handles 4:2:0 only); the "text" preset is 4:2:0 at high bitrate.
- Hop limit 1 applies to the QUIC sockets only; mDNS keeps TTL 255.
- No Rust PAKE crate is audited (owner decision on the pairing UX, D5).
- Cisco's openh264 binary must be downloaded separately to the user's device with a user toggle (openh264.org/BINARY_LICENSE.txt, read 2026-11-07), so it does not fit a bundled installer (owner decision, D4).
- macOS: the LoginWindow agent cannot be registered through SMAppService; the root daemon installs a legacy plist in `/Library/LaunchAgents` with `AssociatedBundleIdentifiers`. Daemon and Aqua agent stay on SMAppService.
- macOS: ScreenCaptureKit works in a pre-login agent from macOS 14.4; minimum for unattended access is macOS 14.4. CGDisplayStream is deprecated (probe fallback only).
- macOS: agents get the Local Network prompt and daemons do not, so agents never touch the LAN and talk to the daemon over local IPC only; the viewer retries after the first prompt.
- macOS: Persistent Content Capture needs a provisioning profile, so the capture agent is a nested app-like bundle; the request form is the owner's action.
- macOS: ad-hoc builds lose TCC grants on every rebuild; the README explains the build tiers.
- macOS: `AVSampleBufferDisplayLayer.enqueue` is deprecated in 15; use `sampleBufferRenderer` (14+). Forced keyframes are best effort.
- macOS: Wake-on-LAN to a MacBook has no Apple source: promised for Windows targets, best effort on the Mac until probe P10.
- macOS: objc2 framework crates, not the `screencapturekit` crate (needs Swift to build).

## M0-mac probe results (stage 8, 2026-11-08)

One run of `probes/mac/CHECKLIST.md` on the owner's MacBook: macOS 27.2 (26B123), Apple silicon, FileVault on, ANSI keyboard with the Brazilian - Pro layout. Ad-hoc signature only (no Developer ID on this Mac). Raw lines: `probes/mac/results.txt` and `probes/mac/results/`. The owner decides each "no".

| Question (stage 8) | Answer | Evidence |
|---|---|---|
| SMAppService daemon | yes | `register` gives `requiresApproval`; one switch in Login Items makes it `enabled`; runs as uid 0 |
| SMAppService Aqua agent | yes | `enabled` at once after the daemon was approved, no second approval |
| SMAppService LoginWindow agent | no (as researched) | `Service cannot load in requested session` (SMAppServiceErrorDomain 134) |
| SCK video and audio in the Aqua agent | yes | 71 to 148 frames per 3 to 5 s at 3420x2214; 249 audio buffers, peak 0.30 |
| SCK while the screen is locked | yes | frames keep coming; mean luma 33 before, 94 locked (the lock screen is captured) |
| SCK in the LoginWindow agent (legacy plist, root) | **no** (refutes the 14.4 verdict above on this Mac) | `sck.shareable` and `sck.start` succeed, then 9 black frames (luma 0.003) and the stream stops: "failed to find displays or windows to capture" |
| CGDisplayStream fallback at the login window | unknown | never ran (scheduled after the injection, which hangs) |
| CGEventPost while locked (Aqua agent) | **no** | 12 events posted per round, two keycodes, preflight true; no pointer move, no dots in the password field |
| CGEventPost unlocked (control) | yes | the pointer drew the expected square around the screen centre |
| CGEventPost at the login window, plain build | **no** | in two logouts the root agent logs `inject.begin` and nothing after it (no `inject.posted`, no further cycles) |
| CGEventPost with `__CGPreLoginApp` | unknown | not run |
| TCC across an ad-hoc rebuild | **no** (as researched) | new cdhash drops Screen Recording, Accessibility, Input Monitoring for the app and the agent; the registered agent also fails to spawn (`EX_CONFIG` 78) until unregistered and registered again |
| TCC across a Developer ID or self-signed rebuild | unknown | not run (no Developer ID; no self-signed certificate made) |
| How TCC lists the nested agent | as the main app | Screen Recording shows one entry, "SwapdeskProbe"; the agent's grants follow it |
| Preflight vs SCK | preflight lags | SCK delivered frames while `CGPreflightScreenCaptureAccess` still said false |
| Local Network from a process started by a terminal | yes, no alert | first UDP send to the LAN gateway succeeds at t=0 |
| Local Network from the app | yes, no alert this run | first send ok at t=0 to the LAN gateway; the app was already allowed in Privacy & Security > Local Network (answered earlier, likely during the Bonjour check), and the grant survived two ad-hoc rebuilds and `tccutil reset All`; errors before "Allow" not measured |
| Bonjour register and browse | yes | `dns-sd -B _swapdesk._udp` sees `swapdesk-probe` within 3 s |
| Agent to daemon over a Unix socket | yes, no alert | `pong ping` in 0 ms |
| Key left of `1` / right of left Shift | kVK 0x32 / no such key | ANSI keyboard: right of left Shift is Z (kVK 6); the ISO swap could not be checked |
| `ç` on Brazilian - Pro | no key | it comes from the press-and-hold accent menu (`c` repeats, then the menu choice arrives as kVK 18 "1") |
| Active tap swallowing Cmd+Tab | yes | `swallowed:true`, switcher hidden; Input Monitoring already granted, no extra prompt |
| VideoToolbox low latency (1080p H.264) | yes | session ok, forced keyframes at 100 and 300 honoured, mean 8.0 ms, p95 8.7 ms |
| `CGWindowListCreateImage` in the macOS 27.2 SDK | still declared | `SCREEN_CAPTURE_OBSOLETE(10.5,14.0,15.0)` |
| Wake-on-LAN, FileVault unlock over SSH | unknown | not run (`womp 0` today) |

Consequence to decide: on this Mac neither capture nor input works before login or while locked, so unattended access to the Mac (login window, unlocking) has no working path in v1; prompted and logged-in sessions work.

## Swapdesk.app bundle and signing (stage 23e, 2026-11-08)

Info.plist keys of `scripts/mac/build-app.sh`, each read on 2026-11-08 at `https://developer.apple.com/documentation/bundleresources/information-property-list/<key>` (lower-case key name; the DocC JSON behind each page was read):

| Key | Value here | What the page says that decides it |
|---|---|---|
| `CFBundleIdentifier` | `io.github.devbertini.swapdesk.dev` | "A unique identifier for a bundle", reverse-DNS, only letters, digits, hyphens and periods; "The bundle ID also validates an app's signature". The `.dev` suffix keeps the development identity apart from the release one (the Local Network and TCC lists key on it). |
| `CFBundleExecutable` | `Swapdesk` | "The name of the bundle's executable file". The cargo binary is `swapdesk-app-mac` (a bin named `Swapdesk` collides with `Swapdesk.exe` of `swapdesk-app` in a shared target folder on Windows); the script installs it under this name. |
| `CFBundlePackageType` | `APPL` | "For apps, the code is `APPL`". |
| `CFBundleShortVersionString`, `CFBundleVersion` | `0.1.0` (the workspace version without `-preview`), build number | Short version: only digits and periods, "three period-separated integers"; build version: one to three integers, "For macOS apps, increment the build version before you distribute a build". |
| `CFBundleDevelopmentRegion`, `CFBundleLocalizations` | `en`; `en`, `pt-BR` | The development region is the fallback language when no resource matches the user's; `CFBundleLocalizations` names the localizations the bundle ships. |
| `LSMinimumSystemVersion` | `14.0` | "The minimum version of the operating system required for the app to run in macOS"; the lowest value is 10 for apps linking the macOS SDK, so 14.0 is a choice of this project (set by the stage 23e order, not derived from a page). |
| `NSHighResolutionCapable` | `true` | "Whether the Cocoa app supports high-resolution displays": the layers are drawn at the screen's backing scale. |
| `NSLocalNetworkUsageDescription` | pt-BR and en strings in `Resources/<lang>.lproj/InfoPlist.strings`, English in `Info.plist` | "A message that tells people why the app is requesting access to the local network. Any app that uses the local network, directly or indirectly, should include this description. This includes apps that use Bonjour [...] as well as direct unicast or multicast connections to local hosts." (macOS 11.0+). |
| `NSBonjourServices` | `_swapdesk._udp` | "Bonjour service types browsed by the app", an array like `_myservice._udp`; ours is `swapdesk_lan::discovery::SERVICE_TYPE` without the `.local.` suffix. |

TN3179, "Understanding local network privacy" (`https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy`, read 2026-11-08): on macOS (15+) the first local network operation of an app raises an alert and the answer is stored per user and per app (System Settings > Privacy & Security > Local Network). Registering, browsing and resolving Bonjour services, UDP unicast sends, multicast and broadcast, and outgoing TCP connections to a local address need the grant; listening for incoming TCP and receiving a UDP unicast do not. macOS allows by itself: daemons started by `launchd`, programs running as root, and "Command-line tools run from Terminal or over SSH, including any child processes they spawn". Consequence: a test run from SSH never shows the alert and proves nothing about it; only the app opened by `open` or by the owner (the responsible code is then the bundle, "Swapdesk") does. The multicast entitlement "isn't required on macOS".

### D25 result: does a self-signed certificate give a stable identity?

Run on the MacBook (macOS 27.2, arm64) with `scripts/mac/remote.sh run 'bash scripts/mac/build-app.sh --compare'`:

- The certificate path works over SSH with no dialog: a self-signed code-signing certificate "Swapdesk Development" (RSA 2048, key usage digitalSignature, extended key usage codeSigning) made with the system LibreSSL, imported into its own keychain file `~/.swapdesk-dev-signing/swapdesk-dev.keychain-db` (never the login keychain, not on the search list), `set-key-partition-list` so `codesign` needs no prompt. `codesign --force --keychain <file> --sign <sha1>` signs with the untrusted certificate; `codesign --verify --strict` passes ("valid on disk", "satisfies its Designated Requirement"). Gatekeeper is not asked (the bundle is not quarantined).
- Designated requirement, two builds with different code directories (cdhash `236fc906...` and `a36a6836...`, and separately a real rebuild of the binary, cdhash `efb9bb1a...`): all three `identifier "io.github.devbertini.swapdesk.dev" and certificate leaf = H"776423a9a18872ca0cbe5a7a211b0f41f20fbf6b"`. The same bundle signed ad hoc has `cdhash H"..."` as its designated requirement, which changes on every build (control in the same run).
- So the requirement that identifies the app is stable across rebuilds with the certificate and not without it. UNPROVEN: that TCC (Screen Recording, Accessibility, Input Monitoring, Local Network) keeps an existing grant when the requirement stays the same. That needs the owner's clicks: grant once to the build, rebuild with `build-app.sh`, open again and see whether the panes still list it as allowed. Until then D25's fallback (ad hoc and a README warning) stays the safe wording.
- Gotcha: `/opt/homebrew/bin/openssl` (OpenSSL 4.1.0, first on the remote PATH) writes PKCS#12 files that `security import` rejects ("MAC verification failed during PKCS12 import (wrong password?)"); the script uses `/usr/bin/openssl` (LibreSSL 3.3.6). On failure of any step the script falls back to an ad hoc signature and prints why.
- `open -n -W -a <bundle> --args --selftest <file>` from an SSH session starts the app in the logged-in user's GUI session (the main window is on screen: `windows=1`, `window.0=Swapdesk|visible=true`), so the launch check runs without a click as long as someone is logged in at the Mac.
