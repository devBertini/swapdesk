# Swapdesk on Android: spec, version 1

Status: the owner's picks of 22/11/2026 are in section 8. Version 1 holds every slice of this document and is released once, when all of them are done (the owner: everything in one version); it is still built slice by slice, each closed on its own proof.
Contracts it builds on: `docs/SPEC.md` (flows F-..., R-UI-1 to R-UI-13, B1 to B10), `docs/PROTOCOL.md`
(C1, protocol minors), `docs/CONTRACTS.md` (C2 to C7, crates), `docs/DECISIONS.md` (D1 to D77).

Marks, as in the SPEC: **GUESS** = a design number or inference to confirm in its slice; **not verified** = no
primary page read on 22/11/2026 settles it. Every Android, NDK, Rust-target, store or library fact cites a source of
the list at the end ([S1] to [S49]), all read on 22/11/2026.

## 1. Purpose and version 1

- **For:** the owner, with an Android phone or tablet on the same home network as the Windows PC and the Mac; later
  anyone who builds the open-source repository.
- **Problem:** reaching the PC or the Mac from the sofa or another room without a cloud service, with the promise the
  desktop app keeps: nothing leaves the local network (D36).
- **Success:** the owner controls the PC and the Mac from the phone for a month instead of walking to them, within
  the budgets of section 6.

### Version 1: the main flow first, then the rest, released together

The main flow: open Swapdesk on the phone, pick the PC (or the Mac) from the list of the network, pair once by
comparing the six digits, then see its screen, move its pointer with the finger, type with the soft keyboard and hear
its sound. It is built first (slices A0 to A7). The other functions follow in the same version (slices A8 to A16),
the phone as a controlled device included, within what Android allows (section 2); nothing is released before the
last slice (decision 8.6).

| ID | Flow | Criteria |
|---|---|---|
| F-AND-01 | First run and permissions | When the app starts for the first time, it shall ask for each Android permission only at the step that needs it, say why in the app's own words (pt-BR, en), and when a permission is refused shall show what stops working and a button to the system's settings for the app (the Android form of F-FIND-04). |
| F-AND-02 | LAN guard on the phone | When the phone's networks change, Swapdesk shall approve only a Wi-Fi or Ethernet network that is not a VPN, never a cellular one, bind every socket it opens to that network, send QUIC with TTL 1, and refuse any peer that is not on-link in that network's subnet, loopback or the phone's own address (F-FIND-02 on Android, section 4). |
| F-AND-03 | Find and pair | When the phone is on an approved network, it shall list the Swapdesk computers there within 5 s (GUESS, as F-FIND-01) and pair with one by the SAS ceremony of PROTOCOL.md 3 (F-PAIR-01 to F-PAIR-04 unchanged), the phone as the initiator. When a computer's record says a protocol minor below 3, the phone shall list it as "needs an update" and start no pairing or session with it. When the person types an address (F-FIND-03), the phone shall try it, and when the computer there ends the pairing because it does not know an Android peer, shall say that the computer needs an update. |
| F-AND-04 | Session | When the person taps "Controlar" or "Assistir" on a paired computer, the phone shall run the connect wait of D46 (R-UI-13) and enter as a desktop viewer does (prompted, or without a prompt where the computer stored `UNATTENDED` for this phone, F-PERM-03 and D52), resume a lost link without a new prompt inside B5 (F-NET-01, D72), offer "Elevar controle" after Android's confirmation of the phone's owner (decision 8.8), and offer "Voltar à sessão" instead of a second connection (F-SESS-06). |
| F-AND-05 | The picture | When a session is active, the phone shall show the computer's display decoded by the system's H.264 decoder straight into a surface, fitted to the screen with pinch to zoom and drag to pan, with "Escolher tela" and the quality presets (F-VIEW-02, F-VIEW-03), ask the host to encode at the size shown (F-VIEW-06), ask for a keyframe and rebuild its decoder when frames give no picture (F-VIEW-04, F-VIEW-11), inside BA2 and BA4. |
| F-AND-06 | Touch to pointer | When the person touches the picture, the phone shall turn the gestures of section 3 row "Pointer" into pointer and wheel records of PROTOCOL.md 7.3 (no wire change), in the touch mode the person chose (decision 8.9), inside BA3. |
| F-AND-07 | Keyboard | When the soft keyboard is open, the phone shall send what is typed as text records (F-INPUT-02 text mode) and the keys of its special-key row (Esc, Tab, Ctrl, Alt, Win or Cmd, arrows, F1 to F12, Ctrl+Alt+Del in an elevated session, decision 8.8) as key records; when a hardware keyboard types, the phone shall send HID usages from the Android key code (C4 gets an Android column); when the app leaves the front, it shall release every held key and button on the host (F-INPUT-04). |
| F-AND-08 | Sound | When `AUDIO` is granted and sound is on, the phone shall play the host's Opus stream with low-latency output inside B3 (F-AUDIO-01). |
| F-AND-09 | Leaving the front | When the session's screen leaves the front (home, another app, the screen off), the phone shall stop asking for video and sound at once and keep the session resumable for 60 s (F-NET-01 rules); back in front within that time, the picture shall return inside BA2 without a new prompt. |
| F-AND-10 | Traces and activity | Swapdesk on the phone shall keep its trace and activity log in the app's private storage, never holding a never-logged value (F-SEC-01), list the activity in Atividade and export it to a file the person picks (F-SESS-04). |
| F-NET-03 | Android peers on the desktop (protocol minor 3) | When a phone pairs with a desktop of minor 3 or newer, the desktop shall pin it and list it in Pareados as an Android device that reads "Não conectado" except while a session with it is open (a phone announces nothing in version 1), whose page offers Permissões, Histórico and Esquecer and no Controlar, Assistir or Acordar, and shall offer no Inverter in a session with it until the phone can host; it shall keep the phone's system where a build below minor 3 still loads the trust store (`systems.bin`, D83); and it shall send `Os::Android` or the `android` record value to no peer it knows to be below minor 3: a host of that system answers such a viewer with `Bye { UNSUPPORTED_VERSION }`, and where the minor cannot be known before the first message (a `Hello` to a typed address, and pairing) the older peer ends the exchange as a protocol violation, which the phone shows as "needs an update" (F-NET-02). |

Flows of the SPEC that hold on the phone unchanged, by their own tests in the shared crates: F-PAIR-01 to F-PAIR-04,
F-PERM-02, F-SESS-06, F-NET-01, F-NET-02, F-SEC-01, F-CHAT-01 (when chat arrives, slice A8).

**What "every feature and optimization built" maps to.** The work of the maintenance version on top of 1.1.0 (D71 to
D77) lives in shared crates, so the phone gets it with them: resume without the idle timeout and the stale-session
replace (D72) in `swapdesk-session` and `swapdesk-host`; the dial at a moved device's newest record (D74) in
`swapdesk-host`; crossing pairings (D76) in `swapdesk-trust`; the chat ring (D75) in `swapdesk-extras`; file finishing
without a folder read (D77) and the copy queue with its progress view (F-FILE-10) in `swapdesk-files` and
`swapdesk-ui`/`swapdesk-paint`. Three need an Android half written on purpose: the viewer's stall net (rebuild the
decoder after failed decodes and on a stall, F-VIEW-11) in the Android decoder; the clipboard read retry in the
Android clipboard provider (slice A10); the banner out of the controller's reach (F-SESS-05, D73), which Android does not
allow on a phone host (section 3).

### The rest of version 1 (after the main flow, in slice order)

Chat; quick actions toward the computer; Wake-on-LAN of a computer; clipboard text and images; the terminal panel;
the files explorer with the copy queue, pause and resume; the update notice; the phone as a host that shows its
screen and plays its sound to the controller; control of the phone through an accessibility service; swap.

### Later (after version 1)

HEVC on the phone; a tablet layout of two panes; a stylus and the hardware mouse as a pointer.

Not possible on Android and not planned (reasons in section 3): unattended access to a phone; a host banner the
controller cannot reach; the phone's clipboard while it is controlled; blocking a device by MAC on a phone host;
waking a phone by Wake-on-LAN; a user or administrator terminal on a phone host.

### Slices, in order

| Slice | What | Flows | Risk |
|---|---|---|---|
| A0 | Protocol minor 3 on the desktop: `Os::Android = 2`, TXT `os=android`, the phone's system kept beside the trust record so an older build still loads the store, a paired phone's page on the desktop; released to the PC and the Mac before the phone's first build reaches them | F-NET-03, F-NET-02 | risky (wire change, stored data) |
| A1 | Foundation spike: the Rust core cross-compiled for `aarch64-linux-android`, an APK that loads it, discovery and one handshake from the phone to the PC on a socket bound to the Wi-Fi network; 16 KB alignment read back | (spike) | risky (real unknowns, section 7) |
| A2 | LAN guard on Android and pairing | F-AND-02, F-AND-03, F-AND-01 (network part) | risky (security boundary) |
| A3 | The shared screens on the phone: Painter on an Android canvas, the compact layout, accessibility, text fields; Início, Pareados, Na rede, a device's page, Ajustes | F-AND-01, F-UI-02 on Android | normal (look: the owner validates on the canvas) |
| A4 | Session and picture: connect wait, decoder to surface, fit, zoom, displays, quality, stall net, resume | F-AND-04, F-AND-05 | risky (frame path, concurrency) |
| A5 | Touch and keyboard, the Android column of C4 | F-AND-06, F-AND-07 | normal |
| A6 | Sound | F-AND-08 | normal |
| A7 | Leaving the front, traces, activity and export | F-AND-09, F-AND-10 | normal |
| A8 | Chat | F-CHAT-01 on the phone | small |
| A9 | Quick actions toward the computer and Wake-on-LAN of a computer | F-TERM-03, F-WAKE-01 on the phone | small |
| A10 | Clipboard text and images while the session's screen is in front | F-CLIP-01 on the phone | normal |
| A11 | The terminal panel | F-TERM-01, F-TERM-02 as the viewer | normal |
| A12 | Files: the explorer, the copy queue and its progress, pause and resume, the phone's own `Fs` | F-FILE-01 to F-FILE-10 as the viewer | risky |
| A13 | The update notice (outside Play: the signed manifest says a version exists and opens the release page) | F-UPD-01 on the phone | risky (the internet) |
| A14 | The phone as a host, its screen and sound: media projection, the platform encoder, playback capture, the foreground service and its notification, announcing on the network | F-SESS-01 as a host, with Android's limits said in the interface | risky |
| A15 | Control of the phone through an accessibility service: gestures, global actions, text from API 33 | F-INPUT as a host, with its limits | risky |
| A16 | Swap between the phone and a computer | F-SWAP-01 on the phone | risky |

## 2. What a phone does, as viewer and as host

**As viewer (version 1).** A phone or tablet controls a Windows PC or a Mac as a desktop viewer does, with the same
pairing, grants, prompts, resume and LAN guard. What differs is Android's: the picture is small, so the host encodes
at the size shown (F-VIEW-06) and the person zooms; there is no mouse, so gestures stand for the pointer; the soft
keyboard types text, not physical keys; the system keeps the home and quick-switch gestures for itself, so "Capturar
atalhos do sistema" covers less than on a desktop [S27]; the session lives while its screen is in front (F-AND-09),
since a background app is stopped by Android at its will (not verified in detail; the session is designed not to need
it). Raising a session to the computer's machine level ("Elevar controle", the administrator terminal) needs the
system's confirmation on the viewer's machine (F-PERM-03). A phone has no administrator; what Android offers is its own
confirmation of the phone's owner (the screen-lock credential, API 30) [S24]. This matters: a viewer that enters
without a prompt pauses at the computer's lock screen, sign-in screen and UAC until it elevates (F-SESS-02), so
without "Elevar controle" a locked PC cannot be used from the phone at all. Decision 8.8 weighs it; the
recommendation offers "Elevar controle" after that confirmation and leaves the administrator terminal out.

**As host (slices A14 to A16, decision 8.6), stated per Android's limits.**
- *Screen:* a third-party app captures the screen only through media projection, which asks the person's consent for
  every session; a consent token is used once, and from Android 14 reusing it throws [S9]. On Android 15 QPR1 and
  later a status-bar chip shows the sharing and lets the person stop it, and "screen projection automatically stops
  when the device screen is locked" [S9]. So a phone hosts only while someone at the phone said yes and the phone is
  unlocked: **unattended access to a phone is not possible**, nor a session that survives the phone locking.
- *Sound:* only what other apps play with usage media, game or unknown, and only where their capture policy allows
  it, with `RECORD_AUDIO` and the same consent [S10]. Calls and many system sounds are out.
- *Control:* no public permission lets a third-party app inject key or touch events (`INJECT_EVENTS` is not among the
  public constants of `Manifest.permission` [S2]). An accessibility service can dispatch gestures (API 24, declared
  with `canPerformGestures`), run global actions (back, home, lock screen from API 28) and, from API 33, type text as
  a limited input method [S12][S13]. That is touch-level control, not a keyboard, and Google Play allows it only with
  an in-app prominent disclosure and a declaration it approves; the policy page does not mention remote control [S14].
- *Being controlled:* the person sees Swapdesk's notification and Android's own sharing chip [S9]; F-SESS-05 cannot
  hold (section 3, row "Banner").

## 3. Features carried over, one by one

API levels are Android's API levels; "v1" is the main flow (slices A0 to A7); "Later" is a later slice of the same version (A8 to A16); "not possible" says why.

| Feature | What Android allows | API it needs (level) | Consent or permission | Where |
|---|---|---|---|---|
| Discovery | Viewer: browse `_swapdesk._udp` with the existing `mdns-sd` code over multicast sockets; Wi-Fi drops multicast unless a lock is held, with "a noticeable battery drain" [S6], so the lock is held only while a list is on screen. A system-run picker of `NsdManager` finds one device without the local network permission [S1] (fallback if the person refuses it). Host: announcing comes with hosting. | `WifiManager.MulticastLock` (4) [S6]; `ACCESS_LOCAL_NETWORK` (37) [S2] | `CHANGE_WIFI_MULTICAST_STATE` (manifest) [S2]; local network permission at run time once the app targets API 37 [S1] | v1 (browse); Later (announce) |
| Pairing (SAS) | The ceremony runs in Rust unchanged; in v1 the phone is the initiator (the computer opens "Parear este computador"). It needs protocol minor 3, since a 1.1.0 desktop refuses an `os` it does not know (section 5). | none beyond discovery | a secure lock screen on the phone (`isDeviceSecure`, 23) [S23], recommended in section 4 | v1 |
| Permissions per device | Viewer: the phone's Permissões shows what each computer lets it do (read-only, GRANTS-SYNC); it grants nothing while it hosts nothing. D7 (`UNATTENDED` and `ADMIN_TERMINAL` need a local administrator) has no Android equivalent: there is no administrator on a phone. | none | none | v1 (read-only); Later with hosting |
| Session: prompt, unattended, resume | Viewer: prompted entry, entry without a prompt toward a computer that stored `UNATTENDED` for the phone, resume after a lost link (D72), the stale-session replace, "Voltar à sessão". "Elevar controle" after Android's confirmation of the phone's owner (decision 8.8); without it, the computer's lock screen stays out of reach (F-SESS-02). Host: prompted only; unattended is not possible and a session does not survive a lock [S9]. | foreground activity; elevation: the device credential authenticator (30) [S24]; host: foreground service type `mediaProjection` (34) [S11] | host: the projection consent each session [S9] | v1 (viewer); host Later; unattended host not possible |
| Screen | Viewer: H.264 decoded by the platform decoder straight into a `Surface` ("much more efficient" than byte buffers) [S15], from native code through `AMediaCodec` (21) [S17], with the optional low-latency key from API 30 [S16]; HEVC only where the device lists a decoder (Capabilities, minor 2). Host: media projection into a surface, then the platform encoder; windows marked `FLAG_SECURE` stay out of it [S25]; Android 14 lets the person share one app instead of the screen [S9]. | `MediaCodec` (16) [S15], `AMediaCodec` (21) [S17], `KEY_LOW_LATENCY` (30) [S16]; host: `MediaProjection` (21) [S9] | host: consent each session; `FOREGROUND_SERVICE_MEDIA_PROJECTION` (34) [S2][S11] | v1 (H.264 viewer); Later (HEVC, host) |
| System audio | Viewer: Opus decoded by `swapdesk-audio`, played through AAudio in its low-latency performance mode [S18] (AAudio came with Android O [S18]: API 26, not stated on that page). Host: playback capture of other apps (Android 10), limited by usage and capture policy [S10]. | AAudio (Android O) [S18]; host: `AudioPlaybackCaptureConfiguration` (29) [S10] | host: `RECORD_AUDIO` and the projection consent [S10] | v1 (viewer); Later (host) |
| Pointer | Viewer, touchpad mode: one finger moves the pointer relative to its start, tap = left click, two-finger tap = right click, two-finger drag = wheel, tap then drag = left drag, pinch = local zoom of the picture. Direct mode: tap where the finger is. Both send absolute positions (PROTOCOL 7.3), so no wire change. The back gesture can be excluded over the picture (Android 10); the home and quick-switch gestures cannot [S27]. The 200 dp limit on excluded height is not verified. Host: gestures through an accessibility service (24) [S12]. | `View.setSystemGestureExclusionRects` (29) [S27]; host: `dispatchGesture` (24) [S12] | host: the person turns the accessibility service on in Settings; Play declaration [S14] | v1 (viewer); Later (host) |
| Keyboard | Viewer: soft keyboard through a native text field (IME), sent as text records; a special-key row; a hardware keyboard by its Android key code, since `getScanCode` is "not reliable" [S26], so C4 gets an `android_keycode` column. Shortcuts the system keeps (home, quick switch) stay the system's [S27]. Host: no key injection for a third-party app [S2]; text only, through an accessibility input method from API 33 [S12][S13]. | `KeyEvent` (1) [S26]; host: `getInputMethod` (33) [S12], `FLAG_INPUT_METHOD_EDITOR` (33) [S13] | host: the accessibility service | v1 (viewer); Later (host text); host keys not possible |
| Terminal | Viewer: the shared terminal panel (`swapdesk-termview`, `swapdesk-paint`) over the soft keyboard and the special-key row. Host: not offered: an app's shell runs as the app's own sandboxed account, not as a user of the device, so it serves nothing F-TERM-01 promises (not verified in detail). | none | none | Later (viewer); host not offered |
| Files | Viewer: files to send are picked with the system picker (`ACTION_OPEN_DOCUMENT`) and opened by the Java side, whose file descriptor goes to Rust (no path crosses) [S20]; received files go to `Download/Swapdesk` through `MediaStore.Downloads` (Android 10), which an app writes without a storage permission and holds exclusively while `IS_PENDING` is 1 [S21]; a folder tree cannot be granted at the root of storage or at `Download` from Android 11 [S20]. The explorer, the copy queue and its progress (F-FILE-10), pause and resume, free space and names across systems (F-FILE-03, D77) reuse `swapdesk-files`; the name check of D77 becomes one MediaStore query by name and folder (GUESS: whether MediaStore renames a duplicate itself is not verified). Host: only a folder the person grants with `ACTION_OPEN_DOCUMENT_TREE` (21) [S20]. | `ACTION_OPEN_DOCUMENT` (19), `ACTION_OPEN_DOCUMENT_TREE` (21) [S20]; `MediaStore.Downloads` (29) [S21] | the person's pick; no storage permission [S21] | Later (viewer, then host) |
| Clipboard text and images | An app reads the clipboard only while it is the app in focus or the default keyboard (Android 10) [S8]; from Android 12 the system shows "APP pasted from your clipboard" on a read [S19]; content flagged `EXTRA_IS_SENSITIVE` (33) is treated as concealed (F-CLIP-01) [S19]; an image is a content URI clip [S19]. Viewer: synced while the session's screen is in front. Host: a controlled phone shows another app in front, so Swapdesk cannot read its clipboard. | `ClipboardManager` [S19] | none | Later (viewer); host not possible |
| Chat | Rust (`swapdesk-extras`) and the shared drawer; nothing Android-specific. A host notification needs `POST_NOTIFICATIONS` (33) [S2]. | none (viewer) | host: notifications | Later |
| Quick actions | Viewer: lock, sleep, restart and the task monitor of the computer, as on a desktop. Host: lock through an accessibility global action (28) [S12]; sleep and restart are not offered (no public API read). | host: `GLOBAL_ACTION_LOCK_SCREEN` (28) [S12] | host: the accessibility service | Later (viewer); host lock Later |
| Wake-on-LAN | Viewer: the magic packet to a computer's reported MACs, TTL 1, on the approved Wi-Fi's broadcast; it is local network traffic [S1]. Host: not possible: Android sends randomized MACs [S8] and an app cannot read the device's MAC [S7], so a phone's `WakeInfo` is empty. | UDP broadcast | local network permission [S1] | Later (viewer); host not possible |
| Swap of roles | Needs the phone to host. Toward a phone it can offer at most what the phone hosts (decision 8.6). The desktop already swaps across Windows and Mac (F-XSWAP-01). | as hosting | as hosting | Later |
| "Being controlled" banner | Host: a foreground-service notification and Android's own sharing chip [S9]. F-SESS-05 does not hold: a gesture the service dispatches is "dispatched as if it were performed directly on the screen by a user" [S12], and no page read tells such a gesture apart, so the controller can reach the notification shade, the chip and Swapdesk's own screens. A Swapdesk window marked `FLAG_SECURE` stays out of the picture [S25]. | `FLAG_SECURE` (1) [S25] | none | host Later, with this limit said in the interface |
| Block a device by MAC (D65) | Viewer: not needed (the phone listens for nothing in v1). Host: not possible: `/proc/net` is closed to apps (Android 10) [S8] and apps cannot bind `NETLINK_ROUTE` sockets (Android 11) [S7], so the neighbour table is out of reach. | none | none | not possible on a phone host |
| Traces and audit | The own `TraceSink` and the audit log in the app's private storage, same formats; "Exportar registro" through `ACTION_CREATE_DOCUMENT`, which never overwrites [S20]; the viewer's failure trace (decode, present) as on the desktops. | `ACTION_CREATE_DOCUMENT` [S20] | the person's pick | v1 |
| Update | On Google Play only Play updates the app: "An app distributed via Google Play may not modify, replace, or update itself ... using any method other than Google Play's update mechanism" [S38] (quoted as two fragments the page reader returned), and `REQUEST_INSTALL_PACKAGES` "may not be used to perform self updates" [S37]. Outside Play, the D53 signed-manifest check can tell the person a version exists and open the release page; Android's installer and its rules for unregistered apps then apply [S35]. | none (notice); `REQUEST_INSTALL_PACKAGES` (23) only outside Play [S2] | the person installs | Later (notice outside Play); Play: none |

## 4. LAN-only guard and threat model

### The guard on Android

The rule is the desktop's (D36, F-FIND-02): physical Ethernet or Wi-Fi only, peers on-link, hop limit 1, no relay or
cloud. On Android it is kept this way:

- **Which networks count.** The phone's networks come from `ConnectivityManager` callbacks with their capabilities
  and link properties, the higher-level API Android points apps to instead of `getifaddrs` or netlink [S7]. A network
  is approved when its capabilities hold `TRANSPORT_WIFI` or `TRANSPORT_ETHERNET` and not `TRANSPORT_VPN` [S5];
  cellular, USB and any other transport are never approved (fail closed, as `swapdesk-lan` does). Android's own
  definition of the local network for its permission matches: a broadcast-capable interface "such as Wi-Fi or
  Ethernet, but excludes cellular (WWAN) or VPN connections" [S2].
- **Sockets stay on the approved network.** Each socket Swapdesk opens is bound to the approved network with
  `android_setsocknetwork` (API 23) before use, so "all data traffic on the socket ... will be sent on this Network,
  irrespective of any process-wide network binding" [S3][S4]. Whether an always-on VPN set to block other traffic
  still lets such a socket out is not verified: the guard then has no network to approve and says so.
- **On-link, own and loopback.** The subnet is the prefix of the approved network's IPv4 link address; the existing
  guard checks every peer against it, refuses loopback and the phone's own addresses, and sets TTL 1 on QUIC sockets
  (its read-back test runs on the device in A2).
- **Local network permission.** Apps that target Android 17 (API 37) are blocked from the local network until the
  person grants `ACCESS_LOCAL_NETWORK`, a runtime permission of the Nearby devices group; a refused UDP send fails
  with `EPERM`, and `android_getnetworkblockedreason` names the cause on a socket [S1][S3]. Below target 37 the
  `INTERNET` permission grants it implicitly and the app must not ask [S1]. The app asks at the first list of the
  network and shows what is blocked when refused (F-AND-01).
- **Mobile data and Wi-Fi changes.** When Wi-Fi drops and the phone moves to mobile data, no network is approved:
  discovery stops and a live session pauses into "Reconectando" (F-NET-01), resuming when an approved network
  returns.
- **No relay, no cloud, no account.** Unchanged. The phone announces nothing in version 1, so it is not visible on a
  network it visits (it listens on no port either).
- **The emulator is not a LAN.** An emulated device sits behind a virtual router in `10.0.2.0/24` and the emulator
  does not support IGMP [S45]; the guard refuses its peers as off-link, as it should. Discovery, pairing and sessions
  are proven on a real device on the Wi-Fi; the emulator serves screens and unit-level checks.

### Threat model, sized to a phone

Assets: the phone's device key and trust store (paired computers, and what each lets this phone do, `UNATTENDED`
included); the computers' screens shown on the phone; later, the phone's own screen.

| Entry point | Threat | Mitigation |
|---|---|---|
| A lost or stolen phone | Someone holding the phone enters a computer that lets it in without a prompt | Pairing needs a secure lock screen (`isDeviceSecure`, 23) [S23]. The device key rests encrypted with a key of the Android Keystore, whose material "never enters the application process" and can live in secure hardware [S22]; that Keystore key is authorized only after the person unlocked or confirmed recently (`setUserAuthenticationParameters`, the device credential from API 30) [S22][S24]. Whether Ed25519 itself can live in the Keystore is not verified, so the key is unwrapped into the process for the TLS handshake, as the Mac app holds its key in its own process (D31). The unwrapped key is dropped when no session runs and the app has been out of the front for longer than the resume window (60 s, F-NET-01), so after that a locked phone cannot open a session until it is unlocked; while a session runs, the key stays in memory. Each computer keeps "Esquecer" (F-PAIR-04), which ends the phone's reach at once, and its activity log names the phone's entries. |
| A shared phone | Another person of the household uses the unlocked phone | Entry without a prompt asks for the device credential when the last unlock is older than a window (GUESS 5 minutes; decision 8.8). Sessions always show who is controlled on the computer (its banner). |
| The computer's picture on the phone | Screenshots, screen recording or a non-secure display copy what the phone shows | The session's window sets `FLAG_SECURE` ("preventing it from appearing in screenshots or from being viewed on non-secure displays") [S25]. |
| Other apps on the phone | An app reads Swapdesk's keys, store or trace | App-private storage and the Keystore wrap above; no file in shared storage holds secrets; traces hold no never-logged value (F-SEC-01). |
| A visited Wi-Fi network | Devices of a café network are on-link | Only pinned keys reach a session and the phone dials only paired computers (PROTOCOL 2); the phone announces nothing. Residual: an `mDNS` query reveals that a Swapdesk viewer browses there (accepted, as the desktop's TXT record is). |
| A malicious computer, to the phone | The host sends crafted video or files | Video is decoded by the platform decoder into a surface; `swapdesk-proto` limits as everywhere; a received file only answers the phone's own request, into `Download/Swapdesk` (Later). |
| Hosting (slices A14 to A16) | The controller reaches the phone's settings, Swapdesk's own grants or the stop chip; the accessibility service stays on between sessions | F-SESS-05 cannot hold on Android (section 3), said in the interface before the person enables hosting; consent each session and the stop on lock [S9] keep a person in the loop; the service can turn itself off after a session (`disableSelf`, 24) [S12], and turning it on again is the person's step in Settings. |

## 5. Architecture

### Process and the Rust core through the NDK

One Android app process, modelled on the Mac app (D27, D32): the service core (`ServiceCore` and the UI hub of
`swapdesk-host`) runs inside the app, owns every LAN socket, the identity, the trust store and the audit log, and the
shared interface talks to it through `ServiceBackend` over an in-memory pipe, as `swapdesk-app-mac` does today. D6's
"the service owns every socket" becomes "the core in the app owns every socket", as on the Mac. The Rust code is one
shared library built for `aarch64-linux-android` (Rust tier 2 [S40]) with an NDK of r28 or later, which aligns to
16 KB by default; Google Play requires 16 KB support for apps targeting Android 15 or higher, with updates refused
from 1 February 2027 without it [S30]. Rust's own link step through NDK r28 gives 16 KB segments: A1 read them back
(`llvm-readelf -lW`: every LOAD segment of both ABIs is aligned to 0x4000, in the library and in the one taken out of
the APK, where `zipalign -c -P 16` also verifies), and the build script refuses a library that is not. Rust supports the API levels
the newest LTS NDK supports [S40]; the NDK's lowest level for r26 and later is not stated on the page read beyond
"API 19 and older unsupported" [S41] (not verified), far below the minimum of decision 8.3.

**Crates reused unchanged:** `swapdesk-session`, `swapdesk-video`, `swapdesk-audio` (libopus built with the NDK's
CMake: section 7), `swapdesk-ipc`, `swapdesk-files` (the phone brings its own `Fs` for MediaStore, slice A12),
`swapdesk-extras`, `swapdesk-swap`, `swapdesk-term`, `swapdesk-termview`, `swapdesk-update` (the notice, slice A13),
`swapdesk-fakes` (tests).

**Crates reused with a change, each named:**

| Crate | Change |
|---|---|
| `swapdesk-proto` | minor 3: `Os::Android`, its `min_minor`, the toward-older mapping, a wire sample of minor 3 (PROTOCOL.md 11 keeps the 1.0 fixtures) |
| `swapdesk-lan` | TXT `os=android` in `txt.rs`; nothing else, since the guard already takes its interfaces from `NetInterfaces` |
| `swapdesk-trust` | the phone's system beside the trust record (A0) |
| `swapdesk-platform` | C3: a `NativeSurface::Android` variant (the session surface's native window) and `OsGrant` values for the Android permissions the notices name (local network, notifications) |
| `swapdesk-keys` | C4: the `android_keycode` column and its lookups |
| `swapdesk-ui` | strings of the Android notices and permission texts in both languages; the touch-mode setting |
| `swapdesk-paint` | the compact layout (decision 8.7), touch hit testing, long press |
| `swapdesk-host` | the in-memory pipe and the in-process service, now inside `swapdesk-app-mac` (`pipe::bounded_duplex`, `service.rs`), move here first so the Mac app and the phone share one copy |

Not built for Android: every `*-win` and `*-mac` crate, the service and helper binaries, `swapdesk-measure`,
`swapdesk-crossdrive`, `swapdesk-release-tools`.

**New crates**, each compiling to nothing off Android (the D29 pattern, so the desktop gate and its 10-minute budget
do not change), and named `*-android` so the workspace rule that only `*-win` and `*-mac` crates hold `unsafe` grows
by one suffix (a change to the project's rules and lints, made in A1):

| Crate | Holds |
|---|---|
| `swapdesk-app-android` | the shared library and its JNI entry points; assembles the core, the backend and the viewer pipeline (as `swapdesk-app-mac` `app.rs`) |
| `swapdesk-sys-android` | `NetInterfaces` from the network facts Java hands over, socket binding (`android_setsocknetwork`), `SecretStore` (Keystore-wrapped files), `TraceSink`, the `Clock` |
| `swapdesk-codec-android` | `VideoDecoder` and `Renderer` in one: `AMediaCodec` decoding into the session surface's native window, `map_pointer` |
| `swapdesk-audio-android` | `AudioSink` on AAudio |
| `swapdesk-input-android` | the key and touch records from Java to `InputEvent`s (the C3 `InputGrabber` role); later `ClipboardProvider` |
| `swapdesk-shell-android` | the Android `Painter` (section 5, interface), the edit slots, the accessibility bridge, the effects table |

**C3 traits with an Android implementation in version 1:** `Clock`, `VideoDecoder`, `Renderer`, `AudioSink`,
`InputGrabber`, `NetInterfaces`, `TraceSink`, `SecretStore`, and `SessionWatcher` for network changes and leaving the
front. In the later slices: `ClipboardProvider`; hosting adds `ScreenSource`, `VideoEncoder`, `AudioSource` and `InputSink`
(accessibility gestures). Never: `PtySpawner` (no terminal host) and `ActionRunner` beyond lock.

**What crosses JNI, and what stays in Rust.** Crosses, as small records in byte buffers: the activity and surface
life cycle (a surface becomes a native window in Rust, so **video never crosses JNI as bytes**); touch, key and
committed-text events; the network facts of each callback (network handle, transports, IPv4 address and prefix);
permission results; picked files and folders as file descriptors (never a path, C2.3); Keystore wrap and unwrap; the
paint of each frame (below) and the accessibility tree. Stays in Rust: the network, the crypto, pairing, sessions,
the guard, the codecs' configuration, Opus, the files engine, the state, the strings and tokens, layout and hit
testing.

**Protocol minor 3 (slice A0).** `Os` is `{0 Windows, 1 MacOS}` and an unknown enum tag is a protocol violation
(PROTOCOL.md 1 and 9.2); the TXT decoder returns `BadOs` for any `os` other than `windows` and `macos`, and discovery
then drops the record. `PairHello` carries `os` but no minor. So a 1.1.0 desktop can neither list nor pair with a
phone that tells the truth about its system. Minor 3 adds `Os::Android = 2` and the TXT value `android`, sent only
where minor 3 is known; the phone reads a computer's `minor` key (PROTOCOL.md 10) and, below 3, shows "needs an
update" and dials nothing (F-AND-03). A typed address has no record and so no `minor` hint: the phone tries it, and
the older desktop ends the ceremony at `PairHello` with the QUIC close code `PROTOCOL_VIOLATION` and no `PairResult`
(read in the 1.1.0 code: the unknown tag is a codec error, its pairing mode stays open and its audit says
`PairRefused`), which the phone shows as "needs an update". A phone that hosts (slice A14) answers a viewer below
minor 3 with `Bye { UNSUPPORTED_VERSION }` in place of the `HelloAck` that would name its system. The phone never claims to be Windows or a Mac. Everything else the phone sends exists in minors
0 to 2.

Stored data: `PairedDevice.os` is part of the trust record (C5.1) and the record's decoder rejects an unknown tag
(`Os::get` in `swapdesk-trust` `records.rs`), so a trust store holding a phone as `Android` would not load in a build
below minor 3 (a downgrade). A0 keeps the trust record's format (D83): the phone's system goes to a side file,
`systems.bin` (`PeerSystems`, C5.1), as `PeerPorts` did, and the trust record says `Windows` for it. A store that holds
a phone is byte for byte the store 1.1.0 writes with a Windows computer in its place, and the record's code differs
from 1.1.0's by that one line, so the older build loads it and lists the phone as a Windows device it never reaches.
A lost or damaged side file fails nothing: the phone reads as a computer until its next session, where the record
follows the system the device says over the connection its pinned key authenticated.

On the desktop a paired phone announces nothing, so it reads "Não conectado" except while a session with it is
open: its page shows Permissões (where the desktop's person gives the phone `CONTROL`, `AUDIO` and, with an
administrator, `UNATTENDED`), Histórico and Esquecer, the wire of the overview points at the computer ("Controla este
computador") with the note that the phone starts the connection, and none of the actions that dial a device is
there. A session with a phone offers no Inverter until the phone can host (slice A16), and a phone is never asleep
or woken, whatever its record holds. The desktop's grants push toward a device that "appears" (PROTOCOL.md 1)
never fires for a phone; the phone learns the grants in each session (`StoredGrants`) and by its own sync when the
computer appears to it.

### The interface

The rule is one interface on every system (D37, F-UI-02). Three ways to honour it on a phone:

1. **The shared screens of `swapdesk-paint` painted on an Android canvas (recommended).** A Java `View` replays each
   frame's paint operations, batched by Rust into one buffer per frame (GUESS: one JNI crossing per frame plus one per
   new text label is cheap enough; measured against the frame time in A3), with `android.graphics.Canvas` and text
   laid out by Android in the embedded Instrument Sans, returned to Rust as the `Label` handle and its size, cached.
   Text inputs are native `EditText` fields placed in the edit slots the screens report, as on Windows and the Mac
   (IME included). Accessibility comes from the shared tree (`swapdesk_ui::a11y`) through AccessKit's Android adapter,
   which AccessKit lists ("implements the Java-based Android accessibility API") [S43], or the framework's
   `AccessibilityNodeProvider` (16) [S29] if that adapter does not fit. Keeps one set of screens, texts, tokens and
   behaviour, and adds no third-party UI library. Cost: the Painter's Java half, the label round trip, and a compact
   layout.
2. **Rust paints into the native window with its own 2D and text stack.** No Java painter, but a rasterizer and a
   text shaper as new crates, and the IME and accessibility still need Java. More code, new dependencies, no gain in
   sameness over option 1.
3. **Native Android views or Jetpack Compose.** The most Android-looking result, but a third interface kept in step
   with the other two by hand (it breaks D37), and Compose brings Kotlin and AndroidX into the APK. Not recommended.

**What a phone-sized layout changes** (decision 8.7; inside `swapdesk-paint`, leaving Windows and the Mac as they
are): below a width class (GUESS 600 dp) the screens take a compact layout: one pane at a time (the sidebar's list,
then the device's page pushed over it, with back), the session as the whole screen with the toolbar folded to its
handle (F-UI-05) at the top edge; every touch target at least 48 dp by 48 dp, as Android recommends [S28]; hover
hints (D42) become a long press; context menus (F-FILE-07) open on a long press; the files explorer shows one
computer at a time with a switch. Unchanged: tokens, texts in both languages, button order (positive on the right,
negative on the left), D51's centring, light and dark following the system, and R-UI-1 to R-UI-13 (the UI thread is
Android's main thread: no blocking call there, R-UI-8; the slow-operation threshold of R-UI-10 is the display's frame
time, 16 ms at 60 Hz, 8 ms at 120 Hz).

**Build (slice A1, D84).** `android/` is a plain Gradle project in Java (Android Gradle plugin 9.1.0 on Gradle 9.3.1,
a JDK 17 or later, no AndroidX and no third-party Gradle plugin); it packs the APK and never calls cargo.
`scripts/android/build-core.sh` builds `swapdesk-app-android` for each ABI with the NDK (`scripts/android/env.sh` holds
the compiler, the archiver and the CMake toolchain for libopus), copies `libswapdesk_android.so` into the app's
`jniLibs` and reads its alignment back. `scripts/android/check.sh [--apk]` is the Android gate: clippy of every
`*-android` crate for both targets with warnings denied, the libraries, and with `--apk` the debug APK. It runs beside
the desktop gate, never inside it: off Android those crates compile to nothing. The app's version is the workspace's,
read from `Cargo.toml` by the Gradle script. ABIs: `arm64-v8a` for phones and `x86_64` for the emulator image; which of
them the released APK carries is settled at the release slice (GUESS: `arm64-v8a` only; the owner's phone to confirm).
A release build of the library uses the profile that unwinds (`release-service`), since a panic must reach Java as an
exception and never end the app's process.

**Proof without the phone (slice A1).** The project's own emulator image (Android 16, `x86_64`, with `arm64-v8a`
through the image's translation) runs the app; the core says what it finds in the log (`adb logcat -s Swapdesk:I`).
The emulator's network is its own (10.0.2.0/24 behind the host): the computer side of a proof is a development
instance of the service started with `--dev-loopback`, which the guest reaches at `10.0.2.2:<port>`, driven by
`examples/cross_host_driver` on its UI pipe; that address is on-link in the guest's network, so the phone's own guard
needs no exception for it. From the host to the guest the emulator console redirects a UDP port (`redir add
udp:<host port>:<guest port>`). Multicast does not cross, so discovery is proven on the phone only.

## 6. Budgets

Measured on the owner's phone on the home Wi-Fi, against the PC's synthetic source (C7) where a picture is involved.
Today only `swapdesk-measure` serves that source, and only on loopback; A4 adds the harness that lets a development
instance of the PC serve it to a viewer on the LAN (a development build only, never the installed one). All numbers
are GUESS until A4 measures them once.

| ID | What | Number | Measured by |
|---|---|---|---|
| BA1 | Cold start to the first painted frame (Início with its placeholders, R-UI-6) | ≤ 800 ms | the app's trace line at the first paint against the process start time it reads; the launch time Android reports (`am start -W`, not verified on a page today) as a cross-check |
| BA2 | Time to the first picture | ≤ 1.5 s from "Controlar" on a computer that lets the phone in without a prompt (from the host's grant for a prompted one) to the first decoded frame rendered | trace timestamps on the phone, one clock |
| BA3 | Input round trip | median ≤ 50 ms, p95 ≤ 100 ms from a touch move to the host's cursor position for it coming back | the phone's own clock: the touch's time against the arrival of the `CursorPos` that matches the sent position, so no cross-machine clock is needed |
| BA4 | Frame pacing at 1080p60 | ≥ 58 frames rendered per second (median of 1 s windows) and decode time p95 ≤ 20 ms over 10 min | the decoder's output callbacks and frame ids on the phone; the host's own counters for what it sent |
| BA5 | Battery during a session | ≤ 15% of the battery per hour of a 1080p60 session with the screen on at mid brightness | the battery level the system reports at the start and the end (method fixed in A4) |

Tracked, not budgeted: the APK size of `arm64-v8a` (GUESS ≤ 15 MB), read from the release APK at every build.

## 7. Risks

| ID | Risk | What shows it early |
|---|---|---|
| AR1 | The dependency tree does not cross-compile or misbehaves on Android: `ring` (its page does not list Android as a target [S44]; not verified either way), `quinn`, `tokio`, `mdns-sd` and its `if-addrs`, libopus through CMake | **Spike A1**: build the core for `aarch64-linux-android` and run discovery plus one TLS handshake to the PC on the phone. Fallback for libopus: `opus-rs` (D16). **Result (24/11/2026):** every crate of the core cross-compiled for both targets at the first try and links into one library. On the emulator image the library of each ABI generated device keys (`ring`), completed a pinned-key session handshake over loopback with bytes both ways (`rustls`, `quinn`, `tokio`) and coded a frame with libopus; from the emulator a QUIC handshake completed with a desktop development instance's pairing endpoint, and timed out with pairing closed. Nothing needed a fallback. Still owed to the phone: discovery (`mdns-sd` on real Wi-Fi), the socket bound to the Wi-Fi network, and the same checks on its hardware |
| AR2 | `getifaddrs` on Android 11 and later shows only interfaces with an IP address [S7], and `mdns-sd`'s interface handling may expect more | A1 prints what `if-addrs` returns on the phone; fallback: hand `mdns-sd` the approved network's interface, or the `NsdManager` picker [S1] |
| AR3 | A VPN on the phone routes or blocks Swapdesk's traffic despite the socket binding | A1 repeats the handshake with a VPN app on (always-on and blocking modes) |
| AR4 | Decode latency varies by device; the low-latency key is optional [S16] | A4 measures BA4 and BA3 on the owner's phone first |
| AR5 | Painting through Java costs more than a frame at 120 Hz | A3 traces the UI thread per frame (R-UI-10) on the busiest screen |
| AR6 | The compact layout is a design job of its own | before A3: captures of Início, a device's page and the session in the compact layout on the project's design canvas for the owner |
| AR7 | The phone is useless with a 1.1.0 desktop (minor 3) | A0 ships to both computers before A1 reaches the phone |
| AR8 | Android's developer verification is rolling out (Brazil from 30/09/2026 for the listed stores, global in 2027) and its rules still move [S32][S35] | register the package name before the first build leaves this machine (decision 8.4) |
| AR9 | The local network permission is refused, or its prompt is mistaken for location | A2 runs the refused path on an Android 17 device or image (the emulator's own network is not a LAN [S45]; that a refusal shows there is a GUESS) |
| AR10 | Google Play review of an accessibility service used for control; the policy page is silent on remote control [S14] | not met while the app stays off Google Play (decision 8.4); a declaration draft before any Play listing |

## 8. Decisions

Taken by the owner on 22/11/2026 (8.1 to 8.6), and, on the owner's order of 24/11/2026 to go to the end of the
project without further questions, the recommended option of the three that were still open (8.7 to 8.9); each of
those three is one setting or one layout to change, said with it.

**8.1 Name.** Swapdesk; package `io.github.devbertini.swapdesk`, the Mac's bundle identifier.

**8.2 Where the project lives.** In this repository: the `*-android` crates and an `android/` Gradle project. One
`Cargo.lock`; the minor 3 change and the phone land in one pull request; CI from the first stage (D23).

**8.3 Minimum Android version.** Android 11 (API 30), target API 37: the decoder's low-latency key (30) [S16] and the
device-credential confirmation (30) [S24] exist everywhere, and the local network permission is designed in [S1].
Still needed from the owner for the first proof on a device: the phone's model and Android version.

**8.4 Distribution.** A signed APK on the project's GitHub release; on the owner's own phone, an install over the
cable (ADB), which needs no registration [S35]. The package is registered under the free limited-distribution account
before the 2027 rollout so that other people can install it [S34][S35]. No store fee (D22).

**8.5 Third-party dependencies.** `accesskit_android` 0.9.1 and `jni` 0.22.4 (both MIT OR Apache-2.0) [S48][S49]:
the screen reader through the AccessKit the Windows shell already uses, and one JNI layer for the app's own calls
too. The NDK bindings (AMediaCodec, AAudio, the native window, multinetwork) are declared by hand, the Java half is
Java (no Kotlin), and no AndroidX library is used. Build tools, not shipped: Gradle (Apache-2.0 [S46]) and the
Android Gradle Plugin (Apache License 2.0 [S47]).

**8.6 What a phone hosts.** Everything Android allows, in the same version as the rest (the owner: everything in one
version): the screen and the sound (slice A14), control through an accessibility service (A15) and swap (A16), with
the limits of section 2 said in the interface before the person turns hosting on: consent for every session, no
session without someone at the phone, the session ends when the phone locks, touch and text but no keys, and no
banner the controller cannot reach.

**8.7 The phone's layout.** A compact layout inside the shared screens: the rule "same layout on every system" reads
"same screens, tokens, texts and behaviour; the layout follows the window's width class"; the desktop's layout does
not change. To change it: the width class and the compact arrangement live in `swapdesk-paint` alone.

**8.8 Machine-level reach and entry without a prompt from a phone.** "Elevar controle" after Android's confirmation
of the phone's owner (the screen-lock credential, API 30) [S24], asked every time; the administrator terminal is not
offered from a phone; entry without a prompt asks for the credential when the last unlock is older than 5 minutes
(GUESS). Without elevation the phone cannot reach a locked PC at all (F-SESS-02). To change it: one rule in the
phone's session entry.

**8.9 Touch mode by default.** Touchpad (relative pointer): a computer screen shown at a quarter of its size is hard
to hit with a finger. Direct touch ships too; the default is one setting.

## Sources (all read 22/11/2026)

- [S1] Local network permission: https://developer.android.com/privacy-and-security/local-network-permission
- [S2] `Manifest.permission` reference: https://developer.android.com/reference/android/Manifest.permission
- [S3] NDK networking (`multinetwork.h`): https://developer.android.com/ndk/reference/group/networking
- [S4] `android.net.Network`: https://developer.android.com/reference/android/net/Network
- [S5] `NetworkCapabilities`: https://developer.android.com/reference/android/net/NetworkCapabilities
- [S6] `WifiManager.MulticastLock`: https://developer.android.com/reference/android/net/wifi/WifiManager.MulticastLock
- [S7] Best practices for unique identifiers (MAC, Android 11): https://developer.android.com/identity/user-data-ids
- [S8] Android 10 privacy changes: https://developer.android.com/about/versions/10/privacy/changes
- [S9] Media projection: https://developer.android.com/media/grow/media-projection
- [S10] Capture video and audio playback: https://developer.android.com/media/platform/av-capture
- [S11] Foreground service types: https://developer.android.com/develop/background-work/services/fgs/service-types
- [S12] `AccessibilityService`: https://developer.android.com/reference/android/accessibilityservice/AccessibilityService
- [S13] `AccessibilityServiceInfo`: https://developer.android.com/reference/android/accessibilityservice/AccessibilityServiceInfo
- [S14] Google Play, use of the AccessibilityService API: https://support.google.com/googleplay/android-developer/answer/10964491
- [S15] `MediaCodec`: https://developer.android.com/reference/android/media/MediaCodec
- [S16] `MediaFormat`: https://developer.android.com/reference/android/media/MediaFormat
- [S17] NDK media (`AMediaCodec`): https://developer.android.com/ndk/reference/group/media
- [S18] AAudio: https://developer.android.com/ndk/guides/audio/aaudio/aaudio
- [S19] Copy and paste: https://developer.android.com/develop/ui/views/touch-and-input/copy-paste
- [S20] Access documents and other files from shared storage: https://developer.android.com/training/data-storage/shared/documents-files
- [S21] Access media files from shared storage: https://developer.android.com/training/data-storage/shared/media
- [S22] Android Keystore system: https://developer.android.com/privacy-and-security/keystore
- [S23] `KeyguardManager`: https://developer.android.com/reference/android/app/KeyguardManager
- [S24] `BiometricManager.Authenticators`: https://developer.android.com/reference/android/hardware/biometrics/BiometricManager.Authenticators
- [S25] `WindowManager.LayoutParams`: https://developer.android.com/reference/android/view/WindowManager.LayoutParams
- [S26] `KeyEvent`: https://developer.android.com/reference/android/view/KeyEvent
- [S27] Gesture navigation: https://developer.android.com/develop/ui/views/touch-and-input/gestures/gesturenav
- [S28] Make apps more accessible: https://developer.android.com/guide/topics/ui/accessibility/apps
- [S29] `AccessibilityNodeProvider`: https://developer.android.com/reference/android/view/accessibility/AccessibilityNodeProvider
- [S30] Support 16 KB page sizes: https://developer.android.com/guide/practices/page-sizes
- [S31] Google Play target API level requirements: https://support.google.com/googleplay/android-developer/answer/11926878
- [S32] Android developer verification: https://developer.android.com/developer-verification
- [S33] Developer verification guides: https://developer.android.com/developer-verification/guides
- [S34] Limited distribution accounts: https://developer.android.com/developer-verification/guides/limited-distribution
- [S35] Developer verification FAQ: https://developer.android.com/developer-verification/guides/faq
- [S36] Register an app on an open-source repository: https://developer.android.com/developer-verification/guides/open-source-app-registration
- [S37] Google Play, permissions and APIs that access sensitive information: https://support.google.com/googleplay/android-developer/answer/16585319
- [S38] Google Play, Device and Network Abuse: https://support.google.com/googleplay/android-developer/answer/9888379
- [S39] F-Droid inclusion policy: https://f-droid.org/docs/Inclusion_Policy/
- [S40] Rust, `*-linux-android` targets: https://doc.rust-lang.org/rustc/platform-support/android.html
- [S41] Android NDK compatibility: https://github.com/android/ndk/wiki/Compatibility
- [S42] jni-rs: https://github.com/jni-rs/jni-rs
- [S43] AccessKit: https://github.com/AccessKit/accesskit
- [S44] ring: https://github.com/briansmith/ring
- [S45] Emulator network address space: https://developer.android.com/studio/run/emulator-networking-address
- [S46] Gradle repository (license Apache-2.0): https://github.com/gradle/gradle
- [S47] Android Gradle Plugin POM (license): https://dl.google.com/android/maven2/com/android/tools/build/gradle/maven-metadata.xml and the POM of its release listed there
- [S48] AccessKit Android adapter manifest (`accesskit_android` 0.9.1, its dependencies) and the workspace manifest (license MIT OR Apache-2.0): https://github.com/AccessKit/accesskit/blob/main/adapters/android/Cargo.toml, https://github.com/AccessKit/accesskit/blob/main/Cargo.toml
- [S49] jni crate manifest (`jni` 0.22.4, its dependencies): https://github.com/jni-rs/jni-rs/blob/master/crates/jni/Cargo.toml
