# Parity between Windows and macOS

Status: read from the code on 2026-11-12 (version 1.1 in progress). This is the source of the README's status table: every control of the viewer's toolbar, its side panel and Settings, per direction, as **proven**, **built** or **missing**. It changes when milestone M3 proves or builds a row.

- **Proven:** a run between two separate computers (one Windows PC, one Mac) on a real LAN, by the cross-machine harness or by hand.
- **Built:** the code exists on both sides and its unit and loopback tests pass, but no run between the two systems has shown it yet.
- **Missing:** a side has no code for it; the stage that adds it is named.

Both systems paint the same screens (`swapdesk-paint`) from the same state (`swapdesk-ui`), and both run the same hub (`swapdesk-host`: in the Windows service, in process in the Mac app). So a control looks and behaves the same on both; what differs is whether each system's viewer and host do what the control asks. "Mac → Win" is the Mac controlling the PC; "Win → Mac" is the PC controlling the Mac.

## Viewer toolbar

| Control | What it does | Gate | Mac → Win | Win → Mac | Notes |
|---|---|---|---|---|---|
| Disconnect | ends the session | none | Proven | Proven | |
| Fit to window / 1:1 | scales the picture, locally | none | Built | Built | |
| Audio | turns the host's system sound on or off | `AUDIO` | Proven | Proven | |
| Capture keyboard | sends system keys to the host | `CONTROL` | Proven | Proven | |
| Ctrl+Alt+Del | the secure attention sequence on a Windows host | `UNATTENDED` and the elevation step | Built | n/a | only Ctrl+Alt+Del is sent; see "Not wired on either system" |
| Elevate control | raises an entry without a prompt to the sign-in screen, the lock screen and administrator prompts | `UNATTENDED`, the system's confirmation on the controlling computer | Built | Built | the real UAC prompt and the Mac password dialog are a hand check |
| Terminal | opens the side panel's terminal | `TERMINAL`, consent | **Missing** (M3: the Mac viewer has no terminal pane) | Built (M3) | the Mac host's shell runs; the Mac viewer cannot show one |
| Files | opens the side panel's files | `FILES`, consent | Built (M3) | Built (M3) | |
| Chat | opens the side panel's chat | none | Built (M3) | Built (M3) | |
| Swap | inverts the roles when the other side accepts | both sides able to host | **Missing** (M3: refused toward another OS) | **Missing** (M3) | the lock is in `swapdesk-ui` `interact.rs` `swap_offered`, the painter and the hub's `extras.rs` |
| Choose display | switches the host's display | none | Not wired | Not wired | see below |
| Full screen | the viewer window on a whole monitor | none | Built | Built | |

## Side panel

| Control | What it does | Gate | Mac → Win | Win → Mac | Notes |
|---|---|---|---|---|---|
| Terminal grid: keys, selection, copy, paste, scrollback, resize | the remote shell | `TERMINAL` | **Missing** (M3) | Built (M3) | |
| Administrator terminal | a shell as SYSTEM on a Windows host | `ADMIN_TERMINAL` | Built (M3) | n/a | a Mac host refuses it by design: no daemon in 1.x |
| Quick actions: lock, task monitor, sleep, restart | run on the host | `CONTROL` | Built (M3) | Built (M3) | the Mac host runs them through `osascript`, `open` and `pmset` |
| Folder listing, up, open, download | the host's files | `FILES` | Built (M3) | Built (M3) | the Mac host serves the user's home folder |
| Send files, receive folder | the system's file pickers | `FILES` | Built (M3) | Built (M3) | both pickers are proven on the Mac by `app-session.sh --unlocked` |
| Resume, cancel a transfer | | `FILES` | Built (M3) | Built (M3) | |
| Chat message | | none | Built (M3) | Built (M3) | |
| Clipboard sync | text and images both ways | `CLIPBOARD` | Proven | Proven | `drive-proof.sh --clip`, `cross-session.sh --clip` |

## Settings

| Control | Windows | Mac | Notes |
|---|---|---|---|
| Name, language, theme | Built | Built | |
| Open when the computer starts | **Missing** (version 1.2: the installer writes and removes the Run key) | Built (the login item) | on Windows the service starts with the system; the switch does not change the interface's start yet |
| Receive connections | Built in the hub, no system effect found on either | the same | to check in milestone M3 |
| Lock the computer when a session ends, keep awake | Built | Built | |
| Updates: the switch, check now | Built | Built | the Mac swap waits for the rehearsal of a real update |
| Default quality, show statistics, keyboard mode, Ctrl and Cmd, release key, full-screen monitor | Built | Built | |

## Not wired on either system

These controls exist in the shared state but no shell opens them, so they do nothing on Windows or on the Mac. They are not a parity gap and are not listed as working:

- **Choose display:** the button asks the host for its displays, but no list opens to pick one (switching displays is in 1.2, F-VIEW-02).
- **Other special keys:** the keys button sends only Ctrl+Alt+Del; Alt+Tab, the Windows or Cmd key, Alt+F4 or Cmd+Q and Print Screen have no menu.
- **Quality and view-only in the toolbar:** never drawn (Settings holds the default quality, and "Assistir" is chosen when connecting).
- **Notice buttons and a few commands the service answers "not supported":** opening System Settings or the firewall page from a network notice, asking for the macOS permissions, blocking an address from the alert, cancelling a swap request, and exporting the activity log.

## Proof

The cross-machine harness takes one switch per feature: `--type`, `--clip` and, as milestone M3 builds them, `--files`, `--term`, `--swap`, `--chat` and `--reconnect`; a switch whose proof is not built yet ends the run with its own exit code (10 to 14) and says so. The flow-by-flow proof map is [FLUXOS.md](FLUXOS.md).
