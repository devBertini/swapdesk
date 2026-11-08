# Swapdesk research: macOS (plan stage 3)

Status: research note for M0 stage 3, written 2026-11-07. It feeds the macOS part of `docs/RESEARCH.md` and holds the M0-mac probe design and checklist.
Every source below was read on **2026-11-07**. Quotes are short and verbatim. A claim with no primary source is marked **UNPROVEN**. A design inference is marked **GUESS**.

Verdicts: **confirmed** (a primary page says so), **refuted** (a primary page says the opposite of what the plan assumed), **unproven** (no primary page found; the M0-mac probe or a later stage must settle it).

Conventions:
- Apple documentation was read through its JSON form, `https://developer.apple.com/tutorials/data/documentation/<path>.json`; the address given is the human page `https://developer.apple.com/documentation/<path>`.
- Apple Developer Forums block scripted reads. Forum quotes below were read through WebFetch, which returns the page through a summarizing model. They are replies by Apple DTS engineers (labelled on the page), so they count as Apple statements, but **a human should re-read each forum quote at its address before it is cited outside this note.** One fetched quote spells the launchd key `LimitLoadToSession`; Apple's TN2083 and every plist read here spell it `LimitLoadToSessionType`, and this note uses that spelling.
- **Assumption (recorded as asked):** the owner's MacBook has Apple silicon (`aarch64-apple-darwin`). Its macOS version is unknown here; probe step P0 records it. Current macOS at the time of reading is macOS 27 "Golden Gate" (release notes below). Apple silicon is also the only safe long-term target: "All Intel-based software will no longer be compatible with macOS 28.0, excluding legacy games." (macOS 27 release notes, `macos-release-notes/macos-27-release-notes`, section Rosetta).

---

## 1. ScreenCaptureKit (SCK): versions, audio, the re-prompt, Persistent Content Capture

### 1a. Minimum macOS for video 12.3, for system audio 13.0: confirmed
- https://developer.apple.com/documentation/screencapturekit (platforms: macOS 12.3)
  > "Request screen recording permission from the person before capturing content. In the Info pane of the Xcode target editor, add a `NSScreenCaptureUsageDescription` key"
- https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturesaudio (macOS 13.0)
  > "A stream doesn't capture audio by default. Set this value to true if you require audio capture."
- https://developer.apple.com/documentation/screencapturekit/scstreamoutputtype/audio (macOS 13.0)
  > "A captured sample buffer wraps an AudioBufferList structure that contains the audio samples."
- `captureMicrophone` is macOS 15.0 (same JSON tree); Swapdesk does not need it.

### 1b. Excluding the app's own audio: confirmed, with a catch
- https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/excludescurrentprocessaudio (macOS 13.0)
  > "The default value is false. If you include your app process in the stream output, you can set this value to true to exclude its audio."
- Consequence: the flag excludes the **capturing process** only. The host captures in the agent, and the agent plays no audio, so the flag is harmless but does not exclude the Swapdesk viewer when a Mac views itself or another session on the same Mac. To keep a same-Mac viewer out, filter by application in `SCContentFilter` (exclude the viewer's bundle). **GUESS** that application exclusion also removes that app's audio; the probe does not cover it (no same-Mac sessions in v1 measurements: CLAUDE.md "never a capture that films its own viewer").

### 1c. Deprecated capture APIs and the macOS 15 alerts: confirmed
- https://developer.apple.com/documentation/macos-release-notes/macos-15-release-notes (ScreenCaptureKit, Deprecations)
  > "Applications utilizing deprecated APIs for content capture such as `CGDisplayStream` & `CGWindowListCreateImage` can trigger system alerts indicating they might be able to collect detailed information about the user. Developers need to migrate to `ScreenCaptureKit` and `SCContentSharingPicker`."
- https://developer.apple.com/documentation/macos-release-notes/macos-15_1-release-notes
  > "Applications using our deprecated content capture technologies now have enhanced user awareness policies. Users will see fewer dialogs if they regularly use apps in which they have already acknowledged and accepted the risks."
- https://developer.apple.com/documentation/screencapturekit/sccontentsharingpicker (macOS 14.0)
  > "Avoid creating your own sharing picker. Use the picker provided by the shared static property."

### 1d. Periodic Screen Recording re-prompt cadence on macOS 15+: unproven
- No Apple documentation page states the cadence (weekly in betas, monthly later, per developer reports in forum threads 760112 and 765103, not by Apple staff). The Apple user guide (https://support.apple.com/guide/mac-help/mchld6aa7d23/mac, macOS 27) only says "For each app listed, turn the ability to record on or off." **UNPROVEN.**
- An unattended host cannot depend on someone clicking a recurring dialog. That is the reason for D3 (the entitlement request).

### 1e. Persistent Content Capture entitlement: confirmed (exists, restricted, request form); unproven (what exactly it suppresses)
- https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.persistent-content-capture (macOS 14.4)
  > "A Boolean value that indicates whether a Virtual Network Computing (VNC) app needs persistent access to screen capture."
  > "Before your app can use this entitlement, request permission to use it by submitting the Persistent Content Capture Entitlement Request form. After receiving permission from Apple to use this entitlement, add it to your app's profile"
- The form is `https://developer.apple.com/contact/request/persistent-content-capture/`; it redirects to the Apple Account sign-in (curl, HTTP 302 to `idmsa.apple.com`). Submitting it leaves this PC: the owner's yes, done by the owner.
- Who may request: the page names "VNC" apps. An Apple Developer Program team is needed to have an App ID and a profile (see 8a). **UNPROVEN** whether Apple grants it to a free, open-source, LAN-only tool.
- The entitlement is "added to your app's profile", so it must be authorized by a **provisioning profile**. A standalone executable cannot carry a profile:
  - https://developer.apple.com/documentation/xcode/signing-a-daemon-with-a-restricted-entitlement
    > "This is problematic because a daemon is a standalone executable, so you can't embed a provisioning profile in it. To get around this limitation, wrap your daemon in an app-like structure."
  - Consequence: the capture agent becomes an **app-like bundle** (`SwapdeskAgent.app`, `LSUIElement`) nested in the main app, with its own `Contents/embedded.provisionprofile`.
- A newer System Settings pane exists: https://support.apple.com/guide/mac-help/mchle046574e/mac, title "Control access to your screen and audio through Remote Desktop on Mac" (Privacy & Security > Remote Desktop). TeamViewer's own page (https://www.teamviewer.com/ams/global/support/knowledge-base/teamviewer-remote/remote-control/remote-control-a-mac, modified 2025-02-07) says of that toggle: "Ensures that unattended access remains functional for TeamViewer full client and Host." **GUESS:** the Remote Desktop pane lists apps holding this entitlement, and an app there is not re-prompted. The probe cannot test it without the entitlement; M8 tests it if Apple grants it.
- Consequence: v1 must work, with the documented degradation (re-prompts), without the entitlement. The UI explains the re-prompt when it happens.

---

## 2. Capture and input from a LaunchAgent: Aqua and LoginWindow

### 2a. Session types and the pre-login agent: confirmed
- TN2083 Daemons and Agents (archived, "no longer being updated"): https://developer.apple.com/library/archive/technotes/tn2083/_index.html
  > "pre-login launchd agent LoginWindow Runs in the loginwindow context"
  > "If you don't specify the LimitLoadToSessionType property, launchd assumes a value of Aqua."
  > "if you set up your agent to run in LoginWindow and Aqua, the system will first run an instance of your agent in the loginwindow context. When a user logs in, that instance will be terminated and a second instance will launch in the standard GUI context."
  > "If you need to connect to the window server in a pre-login context, create a pre-login launchd agent."
- PreLoginAgents sample, "Read Me About PreLoginAgents.txt" (zip at https://developer.apple.com/library/archive/samplecode/PreLoginAgents/Introduction/Intro.html, version 1.1, 2014):
  > "A pre-login launchd agent is run as root."
  > "launchd will not honour the "UserName" and "GroupName" properties in the launchd property list file for a pre-login launchd agent"
  > "CGEventTaps do not work by default in the pre-login environment <rdar://problem/5636091>."
  > Install steps: copy the plist "to "/Library/LaunchAgents"" and "Log out."
- Apple DTS, forum thread 768146 (Nov 2024): "You should not be doing this from a `launchd` daemon ever. It has no business connecting to the window server. Rather, use a pre-login agent." and "pre-login agents run as root and you _really_ don't want to run an entire GUI app as root." (https://developer.apple.com/forums/thread/768146)

### 2b. SCK in the LoginWindow agent: confirmed by Apple DTS for macOS 14.4+; the probe must still prove it
- Apple DTS, forum thread 814152 (Jan 2026): https://developer.apple.com/forums/thread/814152
  > "The daemon does all the global stuff, like manage the network connection." / "The agent does all the GUI stuff, like record the screen and post keyboard and mouse events."
  > "I've recently prototyped this approach using ScreenCaptureKit and it works as expected on on macOS 14.4 and later."
  > "In macOS 14.4 we fixed a bug that prevents you from using ScreenCaptureKit in the pre-login context (r. 121253782)."
- Older DTS reply, thread 742164 (Nov 2023): "ScreenCaptureKit is not currently available to pre-login agents (`FB12776922`). If you need to record the screen in that environment, use a legacy API like `CGDisplayStream`." (https://developer.apple.com/forums/thread/742164). That predates the 14.4 fix.
- Counter-report (developer, not Apple, Nov 2024, thread 768146): on macOS 15, CGDisplayCreateImage, CGDisplayStream, AVCaptureScreenInput and SCK all failed pre-login for that developer.
- Consequence: the plan's process model (root daemon for the network, agents for capture and input) is the one Apple recommends. SCK is the only capture path to build; CGDisplayStream is a **probe-only fallback** (deprecated and alert-raising, 1c). Whether SCK at the login window needs a Screen Recording grant, and from whom, is **UNPROVEN** (no user is logged in; the agent is root). Probe P5 answers it.

### 2c. CGDisplayStream and CGWindowListCreateImage: confirmed deprecated (1c); availability in the current SDK unproven
- The Core Graphics JSON pages for `CGDisplayStreamCreate` and `CGWindowListCreateImage` list no platforms and no deprecation summary (read 2026-11-07), so the doc pages do not settle whether the macOS 27 SDK still exports them. **UNPROVEN**; probe P0 greps the SDK headers on the MacBook.

### 2d. CGEventPost at the login window: unproven
- No Apple page says CGEventPost works or fails from a pre-login agent. The PreLoginAgents note above covers event **taps** only. An old forum thread (62687, developer, 2016) reports CGEventPost failing at the login window while capture worked. A community claim (Stack Overflow, 2017, not primary) says the binary needs an empty `__CGPreLoginApp` section. **UNPROVEN.** Probe P6 tests both a plain build and one linked with `-sectcreate __CGPreLoginApp __cgpreloginapp /dev/null`.

### 2e. What other remote desktop tools document: only weak primary evidence
- RustDesk (open source, vendor source code), `src/platform/privileges_scripts/agent.plist` on `master` (https://github.com/rustdesk/rustdesk/blob/master/src/platform/privileges_scripts/agent.plist): `LimitLoadToSessionType` = `LoginWindow`, `Aqua`; `AssociatedBundleIdentifiers` = `com.carriez.rustdesk`; `ProcessType` = `Interactive`. Its `install.scpt` writes the plist with `echo ... > /Library/LaunchAgents/com.carriez.RustDesk_server.plist && chown root:wheel` under `with administrator privileges`, i.e. **not SMAppService**. Its capturer (`libs/scrap/src/quartz/capturer.rs`) calls `CGDisplayStreamCreateWithDispatchQueue`, i.e. the deprecated API.
- TeamViewer and AnyDesk knowledge-base pages list Screen Recording, Accessibility and Full Disk Access (AnyDesk: https://support.anydesk.com/docs/security-permissions-on-macos, updated 2025-07-30: "Without these permissions, AnyDesk will not be able to receive incoming sessions or provide full functionality.") and say nothing about the login window. TeamViewer's statements on FileVault and the login screen exist only in its community forum (staff replies, not a KB page), so they are not used here.

---

## 3. TCC: Screen Recording, Accessibility, Input Monitoring, code identity

### 3a. Which permission gates which call: confirmed (preflight/request APIs exist), partly unproven (exact mapping)
- `CGPreflightScreenCaptureAccess()` / `CGRequestScreenCaptureAccess()`, `CGPreflightPostEventAccess()` / `CGRequestPostEventAccess()`, `CGPreflightListenEventAccess()` / `CGRequestListenEventAccess()`: all macOS 10.15 (https://developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess() and siblings; the pages carry declarations only, no prose).
- `AXIsProcessTrusted()`: "Returns whether the current process is a trusted accessibility client." (https://developer.apple.com/documentation/applicationservices/1460720-axisprocesstrusted)
- MDM PPPC service keys (https://developer.apple.com/documentation/devicemanagement/privacypreferencespolicycontrol/services-data.dictionary):
  > ListenEvent: "Allows the application to use CoreGraphics and HID APIs to listen to (receive) CGEvents and HID events from all processes. A profile can't grant access to these events; it can only deny it."
  > PostEvent: "Specifies the policies for the application to use CoreGraphics APIs to send CGEvents to the system event stream."
  > ScreenCapture: "Allows the application to capture (read) the contents of the system display. A profile can't grant access to the contents; it can only deny it."
  > Accessibility: "Deprecated: use the `Privacy` key in the declarative management `com.apple.configuration.app.settings` configuration. Deprecated: macOS 27+"
- Event taps (https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)):
  > "Event taps receive key up and key down events if one of the following conditions is true: The current process is running as the root user. Access for assistive devices is enabled."
- Consequence: host agent needs Screen Recording plus PostEvent/Accessibility; the viewer needs ListenEvent ("Input Monitoring") for a listen-only tap. **UNPROVEN:** whether the viewer's *active* tap (one that swallows Cmd+Tab, needed for "grab system keys") needs Accessibility as well. Probe P9 checks both tap kinds.
- macOS 27: "Apps can no longer access the local TCC database directly." (macOS 27 release notes, TCC). The probe and the product read grants only through the preflight APIs.

### 3b. Grants attach to the designated requirement (DR): confirmed
- TN3127 Inside Code Signing: Requirements: https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements
  > "macOS solves this problem by recording your app's DR in its database of apps authorized to access the microphone. Each time your app tries to access the microphone, macOS checks that this version of the app satisfies the original DR."
  > "Ad hoc signed code, called Sign to Run Locally by Xcode, has a DR but it's tied to that specific version of the code. In both cases macOS can't reliably track the identity of the code."
  > "If you tweak the code and run it again, macOS repeats that prompt."
  > Developer ID DR: "anchor apple generic and identifier "..." and (... certificate leaf[subject.OU] = <TeamID>)"
- Consequence: with Developer ID, every rebuild keeps its grants as long as the bundle id (code-signing identifier) and the Team ID stay the same. With ad-hoc signing, every rebuild loses Screen Recording, Accessibility and Input Monitoring. Each nested code item (app, agent, daemon) has its own identifier and its own DR, so grants are per item. **UNPROVEN:** whether TCC shows the agent as itself or as the containing app ("responsible code"); probe P4 records what System Settings lists.
- A self-signed code-signing certificate (Keychain Access) gives a DR pinned to that certificate. **UNPROVEN** whether TCC keeps grants across rebuilds for such a DR; probe P7 variant C tests it, because it is the only stable identity an open-source builder without a paid account can have.

### 3c. MDM PPPC cannot grant Screen Recording or Input Monitoring: confirmed
- Quotes in 3a ("A profile can't grant access ... it can only deny it"). Irrelevant for a home user, but it means no managed shortcut exists for the re-prompt.

---

## 4. Local Network privacy (TN3179)

Source for all of 4: TN3179 Understanding local network privacy, https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy

### 4a. Who is prompted and who is exempt: confirmed
- Platforms table: "macOS | yes | macOS 15".
  > "macOS automatically allows local network access by: Any daemon started by `launchd`; Any program running as root; Command-line tools run from Terminal or over SSH, including any child processes they spawn"
  > "The exception for `launchd` daemons doesn't apply to `launchd` agents."
  > "If you ship a `launchd` agent that's not installed using `SMAppService`, make macOS aware of the responsible code by setting the `AssociatedBundleIdentifiers` property in your `launchd` property list."
  > "Device managers aren't able to configure local network privacy using MDM."
  > "macOS maintains separate local network privacy state for each user account."
- Operations table: outgoing TCP "yes"; "Listening for and accepting incoming TCP connections | no"; "Sending a UDP unicast | yes"; "Receiving an incoming UDP unicast | no"; multicast send/receive "yes"; "Registering a service with Bonjour | yes"; "Browsing for Bonjour services | yes".
- Consequence for Swapdesk (QUIC is UDP, mDNS is multicast):
  - The **daemon** (root, launchd daemon) is exempt: it owns discovery, the host side of QUIC and the trust store, as planned.
  - The **LoginWindow agent** runs as root, so it is exempt, but it must not use the LAN anyway.
  - The **Aqua agent** is *not* exempt. It must talk only to the daemon over local IPC (XPC or a Unix socket; loopback is not a local network address per the definition "an IP network associated with a broadcast-capable network interface"). **GUESS:** loopback is outside the check; probe P8 confirms with the agent's IPC.
  - The **viewer** (the Tauri app, user level by rule) sends QUIC unicast and Wake-on-LAN broadcasts, so it is prompted. If the viewer also browses DNS-SD itself, that needs the grant too; letting the daemon browse and hand the list to the UI over IPC keeps discovery working before the grant.
  - Proofs must run from the signed `.app` (Terminal is exempt), as the plan already says.

### 4b. Info.plist keys: confirmed
- TN3179: "If your app accesses the local network, add the NSLocalNetworkUsageDescription property to its `Info.plist`" and "add a list of service types to the NSBonjourServices property". "The multicast entitlement isn't required on macOS."
- https://developer.apple.com/documentation/bundleresources/information-property-list/nsbonjourservices: "Bonjour service type strings look like `_ipp._tcp`, and `_myservice._udp`". So `_swapdesk._udp` is the entry.
- For app extensions and helpers the keys go into the app's `Info.plist` (TN3179: "add the NSLocalNetworkUsageDescription and NSBonjourServices properties to the app's `Info.plist`, not to the app extension's").

### 4c. The first attempt may fail; short-lived processes get no prompt: confirmed
  > "If the system presents a local network alert in response to one of your local network operations, it may deny the operation immediately, before the user has responded to the alert. ... If you can't use one of these preferred APIs, add appropriate retry logic."
  > "macOS fails to display the local network alert when a process with a very short lifespan performs a local network operation (FB16131937). ... update your code to not exit immediately after a local network operation fails."
  > "There's no API to explicitly bring up the local network alert (FB8711182) ... connect a UDP socket to a local network address. This triggers the local network alert without generating any network traffic."
- Consequence: `quinn` has no "waits for connectivity". The viewer's connect path retries (backoff) for the first ~30 s and the UI shows "Allow Local Network" guidance. The app triggers the alert at first launch, in context, with a connected UDP socket.

### 4d. Identity, reset and test hygiene: confirmed
  > "To ensure that local network privacy reliably tracks the identity of your macOS program, sign it with an Apple-issued code-signing identity."
  > "Local network privacy uses your main executable UUID as part of its implementation. If your main executable has no UUID, or shares a UUID with other programs, local network privacy may behave weirdly."
  > Reset: "select it in the list and click the remove button." / "These buttons were introduced in macOS 27.2. On earlier versions of macOS there's no way to reset your program's Local Network privilege (FB14944392)." Alternatives: a VM snapshot or a new user account.
  > "People might see unexpected behavior in System Settings > Privacy & Security if they have multiple versions of the same app installed (FB15568200)."
- Consequence: the probe keeps one copy in `/Applications`; on macOS before 27.2 each Local Network retest needs a fresh user account. `rustc`/`ld` emit an `LC_UUID`; **GUESS** that each rebuild gets a new UUID; P0 prints it with `dwarfdump --uuid`.

---

## 5. SMAppService: daemon, agents, login item, and Tauri bundling

### 5a. Plist locations and approval: confirmed
- https://developer.apple.com/documentation/servicemanagement/smappservice (macOS 13.0): "use `SMAppService` to register and control `LoginItems`, `LaunchAgents`, and `LaunchDaemons` as helper executables for your app."
- `agent(plistName:)`: "must correspond to a property list in the calling app's `Contents/Library/LaunchAgents` directory." `daemon(plistName:)`: "... `Contents/Library/LaunchDaemons` directory". `loginItem(identifier:)`: "... `Contents/Library/LoginItems` directory".
- `register()`:
  > "If the service corresponds to a LaunchAgent, the LaunchAgent is immediately bootstrapped and may begin running."
  > "If an app needs to register a LaunchAgent for multiple users, you must call the API once per user while that user is running the app."
  > "If the service corresponds to a LaunchDaemon, the system won't bootstrap the LaunchDaemon until an admin approves the LaunchDaemon in System Preferences."
- `Status.requiresApproval`: "the user needs to take action in System Settings before the service is eligible to run. The framework also returns this status if the user revokes consent".
- https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos
  > "replace the `Program` key with the `BundleProgram` key and make the path relative to the bundle"
  > "If the user authorizes the `LaunchDaemon`, the system approves all the other helper executables present in the app bundle"
  > "call openSystemSettingsLoginItems() to suggest that they update the System Settings."
  > Legacy plists: "The Team Identifier of the `Program` or `ProgramArguments` executable in the legacy property list must match that of the app bundle for the `AssociatedBundleIdentifiers` key."

### 5b. A LoginWindow agent through SMAppService: refuted as a reliable path
- Apple DTS, forum thread 726864 (Mar 2023), on an agent plist with `LimitLoadToSessionType` `LoginWindow` registered through `SMAppService.agent(plistName:).register()` that failed with "Service cannot load in requested session" while the same plist worked when copied to `/Library/LaunchAgents`:
  > "I suspect that the `SMAppService` API wasn't designed to do this." ... "I doubt that I'll be able to find a workaround for this (well, other than to not use `SMAppService` and continue with the code you're currently using)." (https://developer.apple.com/forums/thread/726864)
- No later Apple page says SMAppService supports LoginWindow agents. `register()` documents only per-user agent bootstrap. Probe P3 re-tests on the owner's macOS.
- Consequence (**plan change**): the daemon and the Aqua agent use SMAppService. The LoginWindow agent is a **legacy plist** in `/Library/LaunchAgents` (root:wheel, 0644), written by the approved root daemon when the owner turns on "unattended", with `AssociatedBundleIdentifiers` = the app's bundle id and the same Team ID. macOS 27 adds: "`launchd` no longer supports loading `launchd` property list files with the quarantine extended attribute." (macOS 27 release notes, Launch Daemons and Agents), so the daemon writes the file itself rather than copying it out of a downloaded bundle.

### 5c. Where helper executables live: confirmed
- https://developer.apple.com/documentation/bundleresources/placing-content-in-a-bundle (table): "help app, helper tool | macOS | `Contents/MacOS/`" or "`Contents/Helpers/`"; "provisioning profile | macOS | `Contents/embedded.provisionprofile`".
- https://developer.apple.com/documentation/xcode/creating-distribution-signed-code-for-the-mac
  > "Sign code from the inside out."
  > "`codesign` only signs nested code when you use the `--deep` option if that code is in particular locations in the top-level bundle. If you put code in a place where `codesign` expects to find data, `codesign --deep` doesn't sign it."
  > "If you're signing a main executable for Developer ID distribution, add the `-o runtime` option" / "add the `--timestamp` option"
- Note: Apple's SMAppService article's example puts the helper in `Contents/Resources`; the bundle-layout page puts code in `Contents/MacOS` or `Contents/Helpers`. Use `Contents/MacOS` (daemon binary) and `Contents/Library/LoginItems/` or `Contents/Helpers/` for the nested agent `.app`.

### 5d. How a Tauri 2 `.app` carries them: confirmed for placing files, unproven for nested signing
- https://v2.tauri.app/distribute/macos-application-bundle/
  > `bundle > macOS > files`: keys are destination paths and values are source paths; files land in `<product-name>.app/Contents` (example `"embedded.provisionprofile": "./profile-name.provisionprofile"`).
  > Entitlements: `"bundle": { "macOS": { "entitlements": "./Entitlements.plist" } }`.
  > "This Info.plist file is merged with the values generated by the Tauri CLI."
  > Default minimum macOS 10.13; set `minimumSystemVersion`.
- https://v2.tauri.app/reference/config/ (MacConfig): `files`, `entitlements`, `hardenedRuntime` (default `true`), `signingIdentity`, `infoPlist`, `frameworks`; BundleConfig `externalBin` ("binaries to embed", target-triple suffixed names).
- https://v2.tauri.app/distribute/sign/macos/: identity via `signingIdentity` or `APPLE_SIGNING_IDENTITY`; "To configure an ad-hoc signature, provide the pseudo-identity `-`"; notarization env vars `APPLE_API_ISSUER`/`APPLE_API_KEY`/`APPLE_API_KEY_PATH` or `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID`; "You also need an Apple device where you perform the code signing."
- **UNPROVEN:** whether `tauri build` signs executables placed through `files` (a nested `.app` with its own entitlements and profile). Consequence: M8 builds with `tauri build` unsigned (or ad-hoc), then runs a Swapdesk script that signs inside-out (agent `.app` with its entitlements and profile, daemon binary, main app) and notarizes. The probe uses the same script, so M8 inherits a proven path.

---

## 6. Input injection: CGEventPost, keyboard events, Unicode text, key codes

### 6a. Physical keys and Unicode text: confirmed
- `CGEvent.post(tap:)`: "This function posts the specified event immediately before any event taps instantiated for that location, and the event passes through any such taps." (https://developer.apple.com/documentation/coregraphics/cgevent/post(tap:))
- `CGEventCreateKeyboardEvent` (https://developer.apple.com/documentation/coregraphics/cgevent/init(keyboardeventsource:virtualkey:keydown:)):
  > "All keystrokes needed to generate a character must be entered, including modifier keys. For example, to produce a 'Z', the SHIFT key must be down, the 'z' key must go down, and then the SHIFT and 'z' key must be released"
- `keyboardSetUnicodeString` (https://developer.apple.com/documentation/coregraphics/cgevent/keyboardsetunicodestring(stringlength:unicodestring:)):
  > "This function allows you to manually override this string. Note that application frameworks may ignore the Unicode string in a keyboard event and do their own translation based on the virtual keycode and perceived event state."
- `CGKeyCode` (https://developer.apple.com/documentation/coregraphics/cgkeycode): "Pressing a given key always generates the same virtual key code on any supported keyboard."
- `CGEventSource(stateID:)`: "If two or more event sources are using the same source state and one of them is released, the remaining event sources will behave as if all keys and buttons on input devices are up". Use one long-lived source per session (`.hidSystemState`).
- Consequence: Unicode mode is best-effort (some apps re-translate from the key code). Unicode mode sends a key code that maps to no character in the active layout plus the string; physical mode sends kVK codes with explicit modifier events. Probe P6 checks Unicode text in a native text field.

### 6b. kVK values and the ISO swap: values confirmed (crate source), swap unproven at Apple
- The `kVK_*` constants live in the SDK's `HIToolbox/Events.h`, which has no public doc page. The `core-graphics` crate 0.25.0 copies them with a link to that header: `ANSI_GRAVE: CGKeyCode = 0x32`, `ISO_SECTION: CGKeyCode = 0x0A`, `JIS_YEN = 0x5D`, `RIGHT_COMMAND = 0x36` (https://docs.rs/core-graphics/latest/src/core_graphics/event.rs.html).
- The swap (on ISO keyboards macOS reports 0x0A for the key left of `1` and 0x32 for the key right of left Shift) is documented only by browser and emulator projects (Mozilla bug 1263302, QEMU keycodemapdb), not by Apple. **UNPROVEN.** Probe P9 records both keys on the owner's MacBook keyboard (Brazilian MacBooks use an ISO layout; **GUESS**).
- Consequence: C4 key tables keep the swap as a data flag per keyboard type (ISO/ANSI/JIS), to be filled from P9.

---

## 7. VideoToolbox encode and AVSampleBufferDisplayLayer decode

### 7a. Low-latency rate control: confirmed
- https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol (macOS 11.3)
  > "Specifies to select an encoder that supports low-latency operation and enables low-latency mode."
  > "Infinite GOP (all P frames following the beginning IDR). No frame reordering (B frame) or looking ahead. Only High profiles. The encoder sets the levels automatically. Temporal Layer structure."
- `kVTCompressionPropertyKey_RealTime` (macOS 10.9): "For real-time compression, clients may set this property to kCFBooleanTrue". `kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality` (macOS 11.0). `kVTCompressionPropertyKey_AllowFrameReordering`: "Set this value to `false` to prevent frame reordering."
- Consequence: Mac-hosted streams are H.264 **High** profile. The Windows MF decoder takes High (windows.md 3b). Infinite GOP means recovery relies on keyframe requests, as planned. **UNPROVEN:** whether the temporal-layer structure needs anything special on the Windows decoder; the M5 cross-machine proof shows it.

### 7b. Keyframe forcing: confirmed, best effort
- https://developer.apple.com/documentation/videotoolbox/kvtencodeframeoptionkey_forcekeyframe: set in the `frameProperties` passed to `VTCompressionSessionEncodeFrame`; "Note that it may not be possible for the encoder to accommodate all requests."
- Consequence: the host checks the output sample's sync flag and repeats the request on the next frame if it was not honoured.

### 7c. AVSampleBufferDisplayLayer: confirmed, with a deprecation
- https://developer.apple.com/documentation/avfoundation/avsamplebufferdisplaylayer (macOS 10.8): "An object that displays compressed or uncompressed video frames."
- `enqueue(_:)` is deprecated at macOS 15.0: "Use sampleBufferRenderer's enqueueSampleBuffer: instead". It also documents `kCMSampleAttachmentKey_DisplayImmediately` ("the decoded image will be displayed as soon as possible, replacing all previously enqueued images regardless of their timestamps").
- `sampleBufferRenderer` (macOS 14.0): "This object allows you to safely enqueue sample buffers from a background thread."
- Consequence: the Mac renderer enqueues through `layer.sampleBufferRenderer` (macOS 14+) with `DisplayImmediately` on every frame for lowest latency. That sets a macOS 14 floor for the viewer.

---

## 8. Signing, hardened runtime, notarization, entitlements, open-source builds

### 8a. Developer ID and notarization: confirmed
- https://developer.apple.com/developer-id/: "you must be the Account Holder of your development team in the Apple Developer Program in order to generate this cerificate." (sic)
- https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution:
  > "Beginning in macOS 10.15, all software built after June 1, 2019, and distributed with Developer ID must be notarized."
  > Requirements: Developer ID certificate ("Don't use a Mac Distribution, ad hoc, Apple Developer, or local development certificate."), "Enable the Hardened Runtime capability", "Include a secure timestamp", no `com.apple.security.get-task-allow`.
- https://developer.apple.com/documentation/security/customizing-the-notarization-workflow: `xcrun notarytool store-credentials "<profile>" --apple-id ... --team-id ...`, `xcrun notarytool submit <zip> --keychain-profile "<profile>" --wait`, `xcrun stapler staple "<App>.app"`; "While you can notarize a ZIP archive, you can't staple to it directly."
- https://developer.apple.com/documentation/security/hardened-runtime: "You add entitlements only to executables." / "Don't include an entitlement if the value is false."
- Notarization uploads leave this PC: each run needs the owner's yes.

### 8b. Entitlements and Info.plist keys Swapdesk needs: confirmed per item, set as a GUESS
| Code item | Entitlements (hardened runtime on) | Info.plist |
|---|---|---|
| `Swapdesk.app` (UI + viewer) | none expected; **UNPROVEN** whether WKWebView in a hardened app needs `com.apple.security.cs.allow-jit` (Tauri's pages do not say) | `NSLocalNetworkUsageDescription`, `NSBonjourServices` = [`_swapdesk._udp`], `LSMinimumSystemVersion` |
| `SwapdeskAgent.app` (nested; capture, audio, input; Aqua and LoginWindow) | `com.apple.developer.persistent-content-capture` only if Apple grants it, with `Contents/embedded.provisionprofile` | `NSScreenCaptureUsageDescription`, `LSUIElement` or `LSBackgroundOnly` (PreLoginAgents: "Your agent must not display a menu bar.") |
| `swapdesk-daemon` (root) | none | none (exempt from Local Network, 4a) |
- No App Sandbox anywhere (daemon, root agent, terminal spawning). System audio through SCK needs no microphone entitlement (`captureMicrophone` unused). **GUESS**, checked by the probe running under hardened runtime.

### 8c. Builds from source by others: confirmed limits
- Apple silicon runs only signed code, but ad-hoc is enough to run (https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes):
  > "the operating system enforces that any executable must be signed before it's allowed to run. There isn't a specific identity requirement for this signature: a simple ad-hoc signature is sufficient."
  > "given that these signatures do not bear any valid identity, binaries signed this way cannot pass through Gatekeeper."
- Downloaded ad-hoc or unnotarized apps need "Privacy & Security ... Open Anyway" (https://support.apple.com/en-us/102445, published 2026-05-27).
- Limits of an ad-hoc build (from 3b, 4d, 1e): every rebuild re-asks Screen Recording, Accessibility and Input Monitoring; Local Network tracking is unreliable; no Persistent Content Capture (it needs a profile); SMAppService behaviour with ad-hoc code is **UNPROVEN** (probe P2/P7).
- Consequence: README documents three tiers. Official release: Developer ID + notarized. Self-built with a free or paid Apple account: works, but Local Network and TCC are only reliable with an Apple-issued identity. Ad-hoc: works for a session on the machine that built it; grants reset on rebuild. Probe P7 decides whether a self-signed certificate is a usable middle tier.

---

## 9. FileVault pre-boot: confirmed out of reach for any app, with one Apple path (SSH) on macOS 26+

- https://support.apple.com/guide/security/volume-encryption-with-filevault-sec4c6dc1b6e/web (published 2026-01-28): "After a user turns on FileVault on a Mac, their credentials are required during the boot process."
- https://support.apple.com/guide/deployment/intro-to-filevault-dep82064ec40/web (published 2026-09-17) and https://support.apple.com/guide/security/managing-filevault-sec8447f5049/web: "On a Mac with Apple silicon with macOS 26 or later, FileVault can be unlocked over SSH after a restart" ... "if Remote Login is turned on and a network connection is available."
- No Apple page describes third-party code running before the FileVault unlock; the data volume holding `/Applications` and `/Library/LaunchAgents` is locked until then. "Cannot be reached by any app" is therefore **confirmed by the boot design**, not by an explicit Apple sentence.
- Consequence (**plan change, documentation only**): Swapdesk still cannot reach pre-boot. On macOS 26+ the owner may enable Remote Login and unlock FileVault from Windows with `ssh <user>@<mac-ip>`; after that the LoginWindow agent takes over. Enabling Remote Login changes the Mac's attack surface: the owner decides (optional probe P11).

---

## 10. Wake-on-LAN / "Wake for network access" on Apple silicon MacBooks: unproven

- https://support.apple.com/guide/mac-help/set-sleep-and-wake-settings-mchle41a6ccd/mac (macOS 27): on a laptop, "Click the pop-up menu next to "Wake for network access," then choose an option." Described as "Allow your Mac to wake briefly so users can access shared services (if applicable)."
- https://support.apple.com/guide/mac-help/share-your-mac-resources-when-its-in-sleep-mh27905/mac: "If you're sharing your computer's resources, such as a shared printer or Music playlist, you can set your computer to wake from sleep when other users access those resources." / "To use this feature, you need an Apple wireless device that supports 802.11n and that has the latest firmware installed."
- No Apple page says a MacBook (Wi-Fi only) wakes on a raw magic packet. **UNPROVEN.**
- Consequence: Wake-on-LAN in v1 is promised for Windows targets; waking the MacBook is "best effort", reported per the probe P10 result.

---

## 11. Rust bindings: objc2 and its framework crates

### 11a. Status: confirmed
- crates.io API (read 2026-11-07): `objc2` 0.6.5 (updated 2026-10-05); `objc2-screen-capture-kit`, `objc2-video-toolbox`, `objc2-core-graphics`, `objc2-av-foundation`, `objc2-core-media`, `objc2-core-video`, `objc2-service-management`, `objc2-application-services`, `objc2-core-foundation`, `objc2-app-kit` all 0.3.2 (2025-10-04); `block2` 0.6.2; `dispatch2` 0.3.1 (2026-02-26). MSRV 1.71. Repository https://github.com/madsmtm/objc2.
- https://docs.rs/objc2/0.6.5/objc2/: "The framework bindings are generated from the SDKs in Xcode 16.4." The `main` branch says Xcode 27.1 (`crates/objc2/src/lib.rs`), so newer bindings are coming but not released.
- docs.rs `objc2-screen-capture-kit` 0.3.2: `SCStreamConfiguration` "Available on crate feature SCStream only" and has `setCapturesAudio`, `excludesCurrentProcessAudio`. Docs are built for `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- Alternative: the `screencapturekit` crate 11.1.2 (https://github.com/doom-fish/screencapturekit-rs, MIT/Apache) has a `build.rs` and a `swift-bridge` folder, so it needs a Swift toolchain to build. **GUESS:** it cannot even `cargo check` on Windows. Prefer the objc2 crates (pure Rust, generated, one maintainer project for every framework needed).
- Every crate above is a new third-party dependency: the owner's yes, recorded in `docs/DECISIONS.md` (D4 list).

### 11b. `cargo check --target aarch64-apple-darwin` on Windows: unproven, likely yes
- https://docs.rs/objc2/latest/objc2/topics/cross_compiling/index.html: linking needs "A cross-linker" and "Linker stubs (`*.tbd` files) ... located inside the SDK"; "The required MacOSX.sdk ... are bundled with Xcode. Please make sure to review the Xcode SLA, especially the sections regarding required use on Apple-branded computers."
- GitHub contents API: `framework-crates/objc2-screen-capture-kit`, `objc2-core-foundation`, `crates/objc2`, `block2`, `dispatch2` have **no `build.rs`**. `cargo check` does not link. **GUESS:** `cargo check --target aarch64-apple-darwin` of crates that depend only on objc2 crates needs no SDK.
- On this PC the target is not installed (`rustup target list --installed` shows only `x86_64-pc-windows-msvc`). Not run here: installing a target changes the toolchain, outside this stage's brief.
- Consequence: M0 step 7 runs `rustup target add aarch64-apple-darwin` and `cargo check -p swapdesk-capture-mac --target aarch64-apple-darwin` (crate name from C3), and records the result. Linking and tests stay on the MacBook (SLA and SDK).

---

## 12. M0-mac probe design

### 12a. Shape
- A separate Cargo workspace at `probes/mac/` (own `Cargo.lock`, excluded from the main workspace), so the probe does not touch `[workspace.dependencies]`. It is throwaway: once its table is in `docs/RESEARCH.md`, M5 moves only proven code into the `*-mac` crates.
- One binary, `swapdesk-probe`, mac-only parts behind `cfg(target_os = "macos")`; the `net-echo` and `wol` subcommands use `std` only so the same source builds on Windows as the peer.
- Dependencies (owner's yes needed, D4): `objc2`, `objc2-foundation`, `objc2-core-foundation`, `objc2-core-graphics`, `objc2-screen-capture-kit`, `objc2-core-media`, `objc2-core-video`, `objc2-service-management`, `objc2-application-services`, `block2`, `dispatch2`, `libc`. Optional P12 adds `objc2-video-toolbox`.
- Output: one JSON line per check on stdout **and** appended to a log file (`--log <path>`; default `~/Library/Logs/SwapdeskProbe/<mode>.log`, root modes `/Library/Logs/SwapdeskProbe/<mode>.log`): `{"t":"<iso time>","check":"sck.frames","ok":true,"detail":{...},"uid":501,"session":"aqua|loginwindow","build":"<nonce>","signer":"<devid|adhoc|selfsigned>"}`.
- `PROBE_NONCE` (read with `option_env!` at build time) changes the binary, so a rebuild has a new cdhash (for P7).
- Placeholder ids (the real bundle id is a naming decision for the owner): bundle `local.swapdesk.probe`, agent `local.swapdesk.probe.agent`, daemon label `local.swapdesk.probe.daemon`, LoginWindow label `local.swapdesk.probe.loginwindow`.

### 12b. Subcommands
| Subcommand | What it does | Pass looks like |
|---|---|---|
| `info` | `sw_vers`-equivalent, arch, uid/euid, `CGSessionCopyCurrentDictionary` (on console, login done), own code signature summary, main executable path | `session` = `aqua` or `loginwindow` |
| `tcc [--request]` | preflight Screen Capture, PostEvent, ListenEvent, `AXIsProcessTrusted`; with `--request` calls the request functions | four booleans |
| `sck [--seconds N] [--audio]` | `SCShareableContent` (10 s timeout), stream main display for N s, count frames, size, mean luma of the last frame; with `--audio` count audio buffers and peak | `frames > 0`, `audio_buffers > 0` |
| `cgstream [--seconds N]` | fallback: `CGDisplayStreamCreateWithDispatchQueue`, frame count (only if `sck` fails at the login window) | `frames > 0` |
| `inject [--text S]` | after 5 s: mouse square (4 moves, 400 ms each) at screen centre, Shift down/up, then S through `keyboardSetUnicodeString`, no Return | pointer visibly moves; S appears in the focused field |
| `keytap --seconds N [--active]` | CGEventTap (listen-only, or active with `--active` swallowing Cmd+Tab), prints kVK code, flags and the character for each key | key codes printed |
| `net --peer IP --port P --seconds N` | connected UDP socket to the peer, sends 1 datagram/s, logs each `Ok`/`Err(os error)`, never exits early | first `Ok` time and errors before it |
| `bonjour --seconds N` | `DNSServiceRegister` `_swapdesk._udp` (libSystem `dns_sd.h`), then browse; logs callbacks and errors | registration callback without error |
| `smapp <register|status|unregister> <daemon|agent|loginwindow>` | `SMAppService` calls; prints status enum and error text | see P2/P3 |
| `daemon-run` / `agent-run --tests list --every S` | modes started by launchd: loop running the listed checks every S seconds; `daemon-run` also runs `net-echo` | log lines every S seconds |
| `net-echo --port P` (Windows and Mac) | UDP echo server; logs peers | sees the Mac's datagrams |
| `wol --mac AA:BB:CC:DD:EE:FF --broadcast A.B.C.255` (Windows) | sends the magic packet 3 times | n/a |

### 12c. Bundle layout (built by `probes/mac/make-app.sh <signer> [identity]`)
```
/Applications/SwapdeskProbe.app/Contents/
  Info.plist                      CFBundleIdentifier local.swapdesk.probe, CFBundleExecutable swapdesk-probe,
                                  LSUIElement true, NSLocalNetworkUsageDescription, NSBonjourServices [_swapdesk._udp],
                                  NSScreenCaptureUsageDescription
  MacOS/swapdesk-probe            main executable (also the daemon program)
  Helpers/SwapdeskProbeAgent.app  nested agent app (own Info.plist, id local.swapdesk.probe.agent, LSUIElement,
                                  NSScreenCaptureUsageDescription; executable = a copy of swapdesk-probe)
  Library/LaunchDaemons/local.swapdesk.probe.daemon.plist      BundleProgram Contents/MacOS/swapdesk-probe, daemon-run
  Library/LaunchAgents/local.swapdesk.probe.agent.plist        BundleProgram Contents/Helpers/SwapdeskProbeAgent.app/Contents/MacOS/swapdesk-probe,
                                                               agent-run, LimitLoadToSessionType Aqua
  Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist  same, LimitLoadToSessionType LoginWindow (SMAppService attempt)
  Resources/legacy/local.swapdesk.probe.loginwindow.plist      ProgramArguments with the absolute path, LimitLoadToSessionType LoginWindow,
                                                               AssociatedBundleIdentifiers local.swapdesk.probe, KeepAlive false, RunAtLoad true
```
- `make-app.sh` signs inside-out: agent executable, agent `.app`, main executable, main `.app`, each with `-f -o runtime` and, for Developer ID, `--timestamp`. Signers: `devid` ("Developer ID Application: <Name> (<TEAMID>)"), `adhoc` (`-`), `selfsigned` (a certificate the owner creates in Keychain Access > Certificate Assistant, type "Code Signing"). No notarization needed: the probe is built locally and is not quarantined (**GUESS**; P1 checks `xattr`).
- Agents run with `KeepAlive` false and `agent-run --every 15` so they stay alive (TN3179 short-lifespan note) and write their logs.

### 12d. Owner checklist on the MacBook

Run in Terminal unless a step says otherwise. Paste each result line (or "no") into the table in 12e. A step that fails does not stop the others.

**P0. Machine facts** (2 min)
1. `sw_vers; uname -m; fdesetup status; pmset -g | grep -Ei 'womp|powernap|tcpkeepalive'`
   Expect `ProductVersion` 14.4 or later, `arm64`, `FileVault is On/Off`, `womp 1` or `0`.
2. `xcode-select -p && xcrun --show-sdk-path` (installs: `xcode-select --install` if missing). Then `grep -l 'CGWindowListCreateImage\|CGDisplayStreamCreate' "$(xcrun --show-sdk-path)/System/Library/Frameworks/CoreGraphics.framework/Headers/"*.h` and `grep -n 'CGWindowListCreateImage' "$(xcrun --show-sdk-path)/System/Library/Frameworks/CoreGraphics.framework/Headers/CGWindow.h"`
   Expect: the declaration line with its `API_DEPRECATED` or `API_UNAVAILABLE`/obsoleted annotation; copy it (settles 2c).
3. `grep -n 'kVK_ISO_Section\|kVK_ANSI_Grave' "$(xcrun --show-sdk-path)/System/Library/Frameworks/Carbon.framework/Frameworks/HIToolbox.framework/Headers/Events.h"` → expect `0x0A` and `0x32`.

**P1. Build and sign** (5 min, per signer)
1. `cd <repo>/probes/mac && PROBE_NONCE=1 cargo build --release` → `Finished release`.
2. `./make-app.sh devid "Developer ID Application: <Name> (<TEAMID>)"` (copies to `/Applications/SwapdeskProbe.app`).
3. `codesign --verify --deep --strict --verbose=2 /Applications/SwapdeskProbe.app` → `valid on disk` and `satisfies its Designated Requirement`.
4. `codesign -d -r - /Applications/SwapdeskProbe.app; codesign -dvvv /Applications/SwapdeskProbe.app 2>&1 | grep -E 'CDHash=|Authority=|TeamIdentifier=|flags='` → DR line, `flags=0x10000(runtime)`. `dwarfdump --uuid /Applications/SwapdeskProbe.app/Contents/MacOS/swapdesk-probe` → one UUID (4d).
5. `xattr -lr /Applications/SwapdeskProbe.app | grep -c quarantine` → `0`.

**P2. SMAppService: daemon and Aqua agent** (5 min)
1. `open -W /Applications/SwapdeskProbe.app --args smapp register daemon` then `open -W /Applications/SwapdeskProbe.app --args smapp status daemon`; read `~/Library/Logs/SwapdeskProbe/app.log`.
   Expect `requiresApproval` and a notification. Approve in System Settings > General > Login Items & Extensions (toggle under "Allow in the Background"). Run `status` again → `enabled`.
2. `sudo launchctl print system/local.swapdesk.probe.daemon | grep -E 'state|program'` → `state = running`.
3. `open -W /Applications/SwapdeskProbe.app --args smapp register agent` → `enabled` (or `requiresApproval`); `launchctl print gui/$(id -u)/local.swapdesk.probe.agent | grep state` → `running`.
   Record: did approving the daemon also approve the agent (5a says it should)?

**P3. SMAppService: LoginWindow agent** (2 min)
1. `open -W /Applications/SwapdeskProbe.app --args smapp register loginwindow` → record the error text. Expected from 5b: "Service cannot load in requested session" or `enabled` that never runs.
2. `open -W /Applications/SwapdeskProbe.app --args smapp unregister loginwindow`.

**P4. Aqua agent: TCC prompts, SCK video and audio, lock screen** (10 min)
1. `tail -f ~/Library/Logs/SwapdeskProbe/agent.log` in a second Terminal window.
2. The agent's first `sck` run should raise the Screen Recording prompt. Record **the name shown** (probe app or agent) and what appears in System Settings > Privacy & Security > Screen & System Audio Recording. Grant it; System Settings may ask to quit and reopen: run `launchctl kickstart -k gui/$(id -u)/local.swapdesk.probe.agent`.
3. Expect `sck.frames ok:true frames>0` lines. While they run: `afplay /System/Library/Sounds/Ping.aiff` → `sck.audio audio_buffers>0`, peak > 0.
4. Accessibility: the agent's `tcc --request` raises it; grant under Privacy & Security > Accessibility; `tcc` shows `post_event:true, ax_trusted:true`.
5. Lock test: press Control-Command-Q, wait 45 s, unlock. In the log, lines from the locked period: `frames>0`? luma close to the lock screen's? Record.
6. Lock + inject: `launchctl kickstart -k gui/$(id -u)/local.swapdesk.probe.agent` with `agent-run --tests inject --text abc` configured (the plist has a second label `...agent.inject`, register it with `smapp register agent-inject`), lock within 5 s, watch: does the pointer move and do 3 dots appear in the password field? Do **not** press Return. Record.

**P5. LoginWindow agent: SCK** (10 min)
1. `sudo mkdir -p /Library/Logs/SwapdeskProbe && sudo cp /Applications/SwapdeskProbe.app/Contents/Resources/legacy/local.swapdesk.probe.loginwindow.plist /Library/LaunchAgents/ && sudo chown root:wheel /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && sudo chmod 644 /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && sudo xattr -c /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && plutil -lint /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist`
   Expect `OK`. (macOS 27 refuses quarantined plists, 5b.)
2. Apple menu > Log Out. Stay at the login window 90 s. Watch for a pointer square (P6 runs in the same agent after 30 s).
3. Log in; `sudo cat /Library/Logs/SwapdeskProbe/loginwindow.log`.
   Expect `session:"loginwindow"`, `uid:0`, then `sck.shareable` and `sck.frames` lines. Record ok/err text. If SCK failed, set `agent-run --tests cgstream,inject` in the legacy plist, repeat (fallback answer).

**P6. LoginWindow agent: CGEventPost** (same logout as P5)
- Record: did the pointer draw a square? Did `swapdesk` appear in the name field (if "Display login window as: Name and password") or dots in the password field? Never press Return.
- If no: rebuild with `cargo build --release --features prelogin-section` (adds `-sectcreate __CGPreLoginApp __cgpreloginapp /dev/null`), `make-app.sh` again, repeat P5 steps 2 and 3. Record whether the section changed the result.

**P7. TCC across rebuilds: Developer ID, ad-hoc, self-signed** (15 min)
For each signer S in `devid`, `adhoc`, `selfsigned`:
1. `tccutil reset ScreenCapture local.swapdesk.probe; tccutil reset Accessibility local.swapdesk.probe; tccutil reset ListenEvent local.swapdesk.probe` (service names are a **GUESS**; if `tccutil` rejects one, remove the app's entry in System Settings with "-").
2. `PROBE_NONCE=1 cargo build --release && ./make-app.sh S [identity]`; `open -W /Applications/SwapdeskProbe.app --args tcc --request`; grant Screen Recording and Accessibility; `open -W ... --args tcc` → all granted.
3. `PROBE_NONCE=2 cargo build --release && ./make-app.sh S [identity]`; check `codesign -dvvv ... | grep CDHash` changed.
4. `open -W /Applications/SwapdeskProbe.app --args tcc` → record the four booleans. Expected from 3b: `devid` keeps them; `adhoc` loses them; `selfsigned` unknown.
5. For `adhoc` and `selfsigned` also run P2 step 1 once: does `smapp register daemon` work at all? Record.

**P8. Local Network** (15 min; needs the Windows PC)
1. On Windows (this PC): `swapdesk-probe.exe net-echo --port 47999` (port is a **GUESS**, any free UDP port), allow it on the Private profile if Windows asks. Note the PC's IPv4.
2. Mac, from Terminal: `/Applications/SwapdeskProbe.app/Contents/MacOS/swapdesk-probe net --peer <PC-IP> --port 47999 --seconds 20` → expect no prompt and `Ok` from the first second (Terminal exempt, 4a).
3. Mac, as an app: `open -W /Applications/SwapdeskProbe.app --args net --peer <PC-IP> --port 47999 --seconds 60`. Expect the Local Network alert with the usage text. Wait 10 s before clicking Allow. In `app.log`: were the sends before the click `Err` (which os error)? After Allow: `Ok`? Windows shows the datagrams.
4. `open -W /Applications/SwapdeskProbe.app --args bonjour --seconds 30`; from Terminal `dns-sd -B _swapdesk._udp` → the probe's instance appears.
5. Agent: add `net` to the Aqua agent's tests (plist argument `--tests net --peer <PC-IP>`), kickstart. Expect a second alert (agents are not exempt) or none if the app's grant covers it (SMAppService attribution). Record the name in the alert and in System Settings > Privacy & Security > Local Network.
6. Daemon: `daemon-run` already sends to the peer every 15 s if `--peer` is set: expect no alert and `Ok` in `/Library/Logs/SwapdeskProbe/daemon.log`.
7. LoginWindow agent (root): add `net` in P5's plist; expect `Ok` with no alert.
8. Agent to daemon over the Unix socket `/var/run/swapdesk-probe.sock`: `agent-run --tests ipc` → `ok:true` with no alert (4a GUESS about loopback/local IPC).
- Before each retest on macOS before 27.2 there is no reset: use a new standard user account for repeats (TN3179).

**P9. Keys: ISO swap and the viewer's tap** (5 min)
1. `open -W /Applications/SwapdeskProbe.app --args keytap --seconds 30` → Input Monitoring prompt; grant; run again.
2. Press, in order: the key left of `1`, the key right of left Shift (if the keyboard has one), `ç` (ABNT2), Right Option. Record the four kVK codes and characters. `defaults read com.apple.HIToolbox AppleSelectedInputSources` → record the layout.
3. `open -W ... --args keytap --seconds 20 --active` and press Cmd+Tab: does the app switcher stay hidden (event swallowed)? Which permission did it ask for (Input Monitoring or Accessibility)?

**P10. Wake for network access** (10 min; needs the Windows PC)
1. MacBook on the power adapter; System Settings > Battery > Options > "Wake for network access": record the options listed and set the most permissive. `pmset -g | grep womp`.
2. Note the Wi-Fi MAC: `networksetup -getmacaddress Wi-Fi` (or `ifconfig en0 | grep ether`). `sudo pmset sleepnow`; wait 2 min.
3. Windows: `swapdesk-probe.exe wol --mac <MAC> --broadcast <subnet>.255`, then `ping -n 20 <MAC-IP>`.
4. Mac, after waking it by hand if needed: `pmset -g log | grep -E 'Wake from|DarkWake' | tail -5` → record whether a wake with reason `WOL`/`Network` appears. Repeat with the lid closed (adapter on).

**P11. FileVault unlock over SSH** (optional; only if the owner decides to enable Remote Login; macOS 26+)
1. System Settings > General > Sharing > Remote Login on. Restart. From Windows: `ssh <user>@<MAC-IP>` → the unlock prompt; enter the password; the session drops and the Mac boots to the login window; P5's agent log starts. Turn Remote Login off afterwards if not kept.

**P12. VideoToolbox low latency** (optional, 5 min)
- `swapdesk-probe vt --frames 600 --size 1920x1080 --force-key 100,300` (synthetic frames) → record: session created with `EnableLowLatencyRateControl`, profile reported, keyframes at 0/100/300 (sync flag), mean and p95 encode ms.

**Cleanup** (as soon as the probe is done)
- `open -W /Applications/SwapdeskProbe.app --args smapp unregister agent`; same for `agent-inject` and `daemon`.
- `sudo rm /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist` (a single file, not recursive).
- `tccutil reset All local.swapdesk.probe` (and for `local.swapdesk.probe.agent`).
- Move `/Applications/SwapdeskProbe.app` and `/Library/Logs/SwapdeskProbe` to the Trash after copying the logs to the repo's `probes/mac/results/` (or attach them to the batch report). Remove any extra user account made for P8.

### 12e. Results table to fill (goes into `docs/RESEARCH.md`)
| Probe | Question | Result (yes/no + log line) | If "no" |
|---|---|---|---|
| P2 | SMAppService registers the daemon (after approval) and the Aqua agent | | Fall back to a pkg-installed legacy daemon: owner decides |
| P3 | SMAppService registers a LoginWindow agent | expected no | Legacy plist written by the daemon (5b), already the plan change |
| P4 | SCK video + audio in the Aqua agent | | Mac host off v1: owner decides |
| P4.5-6 | Capture and injection at the lock screen | | Lock screen unattended off v1 on Mac |
| P5 | SCK in the LoginWindow agent (and `cgstream` fallback) | | Mac login window view-only or off v1 |
| P6 | CGEventPost at the login window (plain / with section) | | Login window view-only; document |
| P7 | TCC survives a rebuild: devid / adhoc / selfsigned | expected yes / no / ? | README tiers (8c) |
| P8 | Local Network: Terminal / app / agent / daemon / root agent / IPC | expected no prompt / prompt / ? / none / none / none | Move the agent's or viewer's traffic per result |
| P9 | ISO swap codes; tap kinds and their permissions | | C4 table data |
| P10 | MacBook wakes on a magic packet (lid open / closed) | | WoL to Mac marked unsupported |
| P11 | FileVault unlock over SSH (optional) | | Document only |
| P12 | VT low latency, forced keyframes (optional) | | M5 measures instead |

---

## 13. Verdict table

| # | Question | Verdict | Consequence for the design | Source |
|---|---|---|---|---|
| 1a | SCK minimum macOS: video, system audio | confirmed (12.3; 13.0) | floor is set by other items (14.4, below) | developer.apple.com/documentation/screencapturekit; .../scstreamconfiguration/capturesaudio |
| 1b | Exclude own audio | confirmed (`excludesCurrentProcessAudio`, own process only) | set it in the agent; same-Mac viewer exclusion by app filter (GUESS) | .../scstreamconfiguration/excludescurrentprocessaudio |
| 1c | CGDisplayStream/CGWindowListCreateImage on 15+ | confirmed deprecated, raise alerts | SCK only; CGDisplayStream as probe fallback only | macos-15-release-notes; macos-15_1-release-notes |
| 1d | Periodic re-prompt cadence | unproven | UI explains re-prompts; unattended Mac degrades without the entitlement | support.apple.com/guide/mac-help/mchld6aa7d23 (silent) |
| 1e | Persistent Content Capture: who, how | confirmed (VNC apps, request form behind Apple sign-in, needs a profile, macOS 14.4) | agent becomes a nested `.app` with its own profile; owner submits the form | .../entitlements/com.apple.developer.persistent-content-capture; .../xcode/signing-a-daemon-with-a-restricted-entitlement |
| 2a | Pre-login agent model | confirmed (LoginWindow session, runs as root) | daemon for network, agents for GUI, as planned; agent never runs the full UI | TN2083; PreLoginAgents Read Me; forum 768146 (DTS) |
| 2b | SCK in a LoginWindow agent | confirmed by Apple DTS for 14.4+ | probe P5 proves it; minimum macOS 14.4 for unattended | forum 814152 (DTS, Jan 2026) |
| 2c | CG legacy APIs in the current SDK | unproven | P0 greps the headers | Core Graphics JSON pages (silent) |
| 2d | CGEventPost at the login window | unproven | probe P6 (plain and `__CGPreLoginApp` variants) | PreLoginAgents Read Me (taps only) |
| 2e | Other vendors' docs | weak: RustDesk uses legacy `/Library/LaunchAgents` + CGDisplayStream; AnyDesk/TeamViewer KB silent on login window | nothing to copy beyond the plist shape | github.com/rustdesk/rustdesk agent.plist, install.scpt, capturer.rs |
| 3a | TCC gates for capture, post, listen | confirmed (APIs, PPPC keys); unproven (active tap needs Accessibility) | host agent: Screen Recording + Accessibility; viewer: Input Monitoring (+ Accessibility, P9) | .../cgpreflightscreencaptureaccess(); .../privacypreferencespolicycontrol/services-data.dictionary |
| 3b | Grants follow the DR; ad-hoc vs Developer ID | confirmed | Developer ID keeps grants across rebuilds; ad-hoc loses them; self-signed tested in P7 | TN3127 |
| 3c | MDM PPPC for Screen Recording | confirmed (can only deny) | no managed bypass | .../services-data.dictionary |
| 4a | Local Network: prompted vs exempt | confirmed (daemons, root, Terminal exempt; agents not) | agents never touch the LAN; daemon owns network; viewer is prompted | TN3179 |
| 4b | NSLocalNetworkUsageDescription, NSBonjourServices `_swapdesk._udp` | confirmed | both keys in the main app's Info.plist; no multicast entitlement on macOS | TN3179; .../nsbonjourservices |
| 4c | First denied attempt | confirmed (may fail before the answer; short-lived processes get no alert) | viewer retries with backoff; agents stay alive; trigger in context | TN3179 |
| 4d | Identity and reset | confirmed (Apple-issued identity; unique UUID; reset only from 27.2) | one installed copy; new user account for retests on older macOS | TN3179 |
| 5a | SMAppService plists and approval | confirmed | daemon + Aqua agent via SMAppService; `BundleProgram`; register the daemon first | .../smappservice; .../updating-helper-executables-from-earlier-versions-of-macos |
| 5b | LoginWindow agent via SMAppService | refuted (DTS: not designed for it) | **plan change:** legacy plist in `/Library/LaunchAgents` written by the daemon, `AssociatedBundleIdentifiers`, no quarantine | forum 726864 (DTS); macos-27-release-notes (launchd) |
| 5c | Nested code placement and signing | confirmed (inside-out, `Contents/MacOS` or `Contents/Helpers`) | own signing script after `tauri build` | .../placing-content-in-a-bundle; .../creating-distribution-signed-code-for-the-mac |
| 5d | Tauri bundle: files, Info.plist, entitlements | confirmed (files under `Contents`, merged Info.plist, entitlements, hardenedRuntime) ; unproven (nested signing) | plists and agent `.app` through `bundle.macOS.files`; sign with the script | v2.tauri.app/distribute/macos-application-bundle; /reference/config; /distribute/sign/macos |
| 6a | CGEventPost, keyboard events, Unicode string | confirmed (Unicode may be ignored by some apps) | physical mode primary; Unicode best effort | .../cgevent/post(tap:); .../init(keyboardeventsource:virtualkey:keydown:); .../keyboardsetunicodestring |
| 6b | kVK values; ISO swap of Grave/Section | values confirmed (0x32, 0x0A via crate copy of Events.h); swap unproven | swap as data per keyboard type, filled by P9 | docs.rs/core-graphics event.rs |
| 7a | VT low-latency rate control | confirmed (High profile only, infinite GOP, no B-frames) | Mac streams are High profile; recovery by keyframe request | .../kvtvideoencoderspecification_enablelowlatencyratecontrol |
| 7b | Keyframe forcing | confirmed, best effort | check the sync flag, repeat the request | .../kvtencodeframeoptionkey_forcekeyframe |
| 7c | AVSampleBufferDisplayLayer | confirmed; `enqueue` deprecated in 15 | use `sampleBufferRenderer` (14+), `DisplayImmediately` | .../avsamplebufferdisplaylayer/enqueue(_:); .../samplebufferrenderer |
| 8a | Developer ID, hardened runtime, notarytool | confirmed | M8: Account Holder makes the certificate; `notarytool submit --wait`, `stapler staple`; each upload needs the owner's yes | developer.apple.com/developer-id; .../notarizing-macos-software-before-distribution; .../customizing-the-notarization-workflow |
| 8b | Entitlements needed | confirmed per item; set is a GUESS | table 8b; JIT for WKWebView unproven | .../hardened-runtime; TN3179; PreLoginAgents |
| 8c | Source builds by others (ad-hoc) | confirmed (runs; no Gatekeeper; grants reset; LN unreliable; no restricted entitlement) | README tiers; P7 decides on self-signed | big-sur universal-apps release notes; support.apple.com/102445; TN3127; TN3179 |
| 9 | FileVault pre-boot | confirmed out of reach for apps; macOS 26+ Apple silicon unlocks over SSH | document the SSH path as an option (owner decides on Remote Login) | support.apple.com/guide/security/sec4c6dc1b6e; /guide/deployment/dep82064ec40 |
| 10 | Wake on LAN to a MacBook | unproven | WoL promised for Windows targets; Mac best effort per P10 | support.apple.com/guide/mac-help/mchle41a6ccd; mh27905 |
| 11a | objc2 framework crates | confirmed (0.3.2, Xcode 16.4 SDK, all frameworks needed) | use objc2 crates (owner's yes, D4); avoid the Swift-bridged `screencapturekit` crate | crates.io API; docs.rs/objc2/0.6.5; github.com/madsmtm/objc2 |
| 11b | `cargo check --target aarch64-apple-darwin` on Windows | unproven, likely yes (no build scripts; check does not link) | M0 step 7 installs the target and records the result | docs.rs/objc2 topics/cross_compiling; GitHub contents API |
