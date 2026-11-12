# Swapdesk on Windows: development install

Scripts that put Swapdesk on this PC the production way: per machine under `%ProgramFiles%`, the Windows service `SwapdeskService` as LocalSystem, firewall rules, the Start menu shortcut, the entry in the installed apps list (Settings > Apps). They exist to run the production path (SCM service, SYSTEM token in the console session, install-folder check) that `--console` cannot exercise.

Read the scripts before running them: each is one file with the changes listed in its header, and `-DryRun` prints every action without doing any.

| File | What it is |
|---|---|
| `build-release.cmd` / `.ps1` | Builds the release binaries, stages them and zips them. No administrator rights. |
| `install.cmd` / `.ps1` | Installs or upgrades. One UAC click. |
| `uninstall.cmd` / `.ps1` | Removes everything and checks that nothing is left. One UAC click. |

Encoding: the `.ps1` files are UTF-8 with a BOM and CRLF line ends (Windows PowerShell 5.1 needs the BOM for any non-ASCII text; the files are ASCII anyway); the `.cmd` files are ASCII with CRLF. They run on the PowerShell 5.1 that ships with Windows; the `.cmd` beside each `.ps1` calls it with `-NoProfile -ExecutionPolicy Bypass` because the execution policy refuses plain `.ps1` files here.

## Build

```
packaging\windows\build-release.cmd
```

Runs `cargo build --profile release -p swapdesk-app -p swapdesk-helper` and `cargo build --profile release-service -p swapdesk-service` (D15: the service keeps `panic = unwind`, the rest `abort`), stages the files in `target\package\Swapdesk\` and writes `target\package\Swapdesk-windows-x64-<version>.zip` (the version is `workspace.package.version` in `Cargo.toml`) plus a `.sha256` file; it prints the SHA-256 of the zip at the end (for the release notes). Before zipping it writes `SHA256SUMS` into the staged folder: lowercase hex, two spaces, the file name, one line per shipped file, LF line ends (`sha256sum -c SHA256SUMS` reads it). `install.ps1` verifies against it.

Set `CARGO_TARGET_DIR` to a short folder (for example `C:\Users\<you>\t39`) in a deep worktree: libopus is built by CMake and overruns MAX_PATH otherwise. The package then lands under that folder. `-SkipBuild` only stages and zips.

The zip holds one folder, `Swapdesk\`: `Swapdesk.exe`, `swapdesk-service.exe`, `swapdesk-helper.exe`, the two licence files, `VERSION` (the number alone, read for the apps list), `SHA256SUMS`, `install.ps1`, `install.cmd`, `uninstall.ps1`, `uninstall.cmd` and this file. Nothing else is needed to run: the font is embedded in the exe and the manifest is linked in at build time. No `.pdb` is shipped.

## Install

1. Run `packaging\windows\install.cmd` (from the repository, after the build) or `Swapdesk\install.cmd` (from the unzipped package). Double-click works: the window waits for a key at the end.
2. Approve the Windows (UAC) prompt. The window that asked stays open and shows what the elevated one did.

`install.cmd -DryRun` shows the plan without any change and without the prompt. `-Source <folder>` installs from another folder (it must hold `SHA256SUMS`); `-EnableSas` opts in to the Ctrl+Alt+Del policy (item 6; without it the policy is not touched); `-SkipSasPolicy` is kept for old command lines and wins over `-EnableSas`.

`install.ps1` reads `SHA256SUMS` from the source folder before the UAC prompt and hands its text to the elevated copy, so both sides use the same text. A missing or malformed `SHA256SUMS` stops the install before the prompt.

Minimum Windows: the scripts check only that the OS is 64-bit (and that PowerShell is 64-bit for a real run). They use `Get-NetFirewallRule`, `New-Service` and `Get-CimInstance`, which exist from Windows 8 / Server 2012, but nothing older than Windows 11 has been run: Windows 10 1809 and later is untested, Windows 11 (Home, Portuguese) is what was tested.

What it changes on this machine, in order:

1. **Stops** the service `SwapdeskService` if it exists and any `Swapdesk`, `swapdesk-helper` or `swapdesk-service` process that runs from `%ProgramFiles%\Swapdesk` (an upgrade; an open Swapdesk window closes).
2. **Folder** `%ProgramFiles%\Swapdesk` (the 64-bit folder, also from a 32-bit shell): owner Administrators, inheritance from Program Files switched off, explicit entries: SYSTEM, Administrators and TrustedInstaller full control, Users read and execute. This is the shape `InstallPolicy::production` accepts (CONTRACTS C2.1): nobody but those three can write. Program Files itself already grants the same (read here with `icacls "C:\Program Files"`: TrustedInstaller, SYSTEM and Administrators full, Users read and execute, CREATOR OWNER full control inherit-only, ALL APPLICATION PACKAGES read), so inheriting would also pass, but the script sets the entries itself so the result does not depend on the parent. It then reads the ACL back with `Get-Acl`, prints the owner, the entries and the SDDL, and fails the install if the folder or any file grants a write-like right to anyone else or has an untrusted owner (the same bits `check_no_other_writers` tests).
3. **Files** copied from the source folder: `Swapdesk.exe`, `swapdesk-service.exe`, `swapdesk-helper.exe`, `uninstall.cmd`, `uninstall.ps1` (so the folder holds its own uninstaller, writable by administrators only), `LICENSE-MIT`, `LICENSE-APACHE`; each copy is checked against the SHA-256 in `SHA256SUMS` (not against the source file); a mismatch or a missing entry stops the install and deletes what it copied. An existing install folder is first searched recursively: if it holds a file that is neither in `SHA256SUMS` nor one of the installed names, the install stops before changing anything (move the file, or run `uninstall -Force`). The ACL check also walks every file recursively. Every file is reset to inherit the folder's ACL and owned by Administrators.
4. **Service** `SwapdeskService`, display name `Swapdesk`, binary `"%ProgramFiles%\Swapdesk\swapdesk-service.exe"` with no argument, account LocalSystem, start type automatic; recovery (`sc.exe failure ... reset= 86400 actions= restart/1000/restart/5000/restart/30000`, then `sc.exe failureflag ... 1` so it also applies when the service exits with an error code without crashing): restart after 1 s, 5 s, 30 s, counter reset after one day; the exit code of each `sc.exe` call is checked. The Event Log source `Swapdesk` is registered in the Application log (`New-EventLog`, only when it does not exist yet). An existing service is updated in place (path, display name, start type); one running under another account makes the script stop and ask for an uninstall first.
5. **Firewall**, group `Swapdesk`, two inbound allow rules for `%ProgramFiles%\Swapdesk\swapdesk-service.exe`, UDP, profiles Private and Domain only (a network marked Public gets no rule), remote address LocalSubnet only: `Swapdesk-UDP-Session-Pairing` (ports 47820 to 47883: the session and pairing ports of each account's slot, 32 accounts at most, ACCOUNTS.md 5.5) and `Swapdesk-UDP-mDNS` (port 5353, the discovery of `mdns-sd`; `docs/research/windows.md` 7 designs both). Nothing is added for outbound traffic or the Public profile. The service runs as LocalSystem and gets no firewall prompt, so without these rules inbound traffic is blocked.
6. **`SoftwareSASGeneration`** (F-SYS-01), **only with `-EnableSas`**: the old value (present or absent, and the number) is saved in `HKLM\SOFTWARE\Swapdesk\Installer`, then `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration` is set to `1` ("Services": services may simulate Ctrl+Alt+Del) and the marker `SasSetValue` is stored beside the saved value. The script prints a warning that this lets services simulate Ctrl+Alt+Del and that the setting is unproven. Without `-EnableSas` the value is left untouched and one line says that unattended Ctrl+Alt+Del needs `-EnableSas`. A later install keeps the first saved value. The number is **unproven**: the policy page names the key and the four options but not their numbers, and `WinLogon.admx` is not on this Windows 11 Home PC (SPEC R5, `docs/research/windows.md` 5c). 1 comes from memory of the ADMX order (None 0, Services 1, Ease of Access 2, both 3), not from a page read now: check it when the unattended stage tests the policy. Whether the value takes effect on Home is tested there too.
7. **Shortcut** `Swapdesk.lnk` in the all-users Start menu programs folder, to `%ProgramFiles%\Swapdesk\Swapdesk.exe`.
8. **Installed apps entry** `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Swapdesk` (the 64-bit view): `DisplayName` Swapdesk, `DisplayVersion` (the package's `VERSION`, accepted only when `SHA256SUMS` has its hash), `Publisher`, `InstallLocation`, `DisplayIcon` (`Swapdesk.exe,0`), `UninstallString` (`"%ProgramFiles%\Swapdesk\uninstall.cmd"`), `QuietUninstallString` (the same with `-Quiet`; there is no prompt to hide, the flag only names it), `InstallDate`, `NoModify` 1, `NoRepair` 1, `EstimatedSize` (KB of the installed files). Settings > Apps lists Swapdesk with an Uninstall button that runs `uninstall.cmd` (one UAC click).
9. **Starts** the service and prints its state (status, account, pid), the rules, the policy value, the shortcut, the apps entry and the number of helper processes (two expected once a console session exists).
10. **Event Log**: one Application event, source `Swapdesk`, id 1000, at the end of a successful run (a failure to write it only warns).

Both scripts only count a process as running from the install folder when its path starts with `<install folder>\` (so `Swapdesk-other\` never matches).

Exit codes: 0 done, 1 failed (nothing is rolled back; running it again is safe), 2 installed but the service did not start (the script prints the last lines of `%ProgramData%\Swapdesk\trace.log`, which never holds secrets), 1223 the UAC prompt was cancelled.

Not touched: the identity, trust store, settings and logs in `%ProgramData%\Swapdesk` (the service creates that folder itself, C5.2); the development folder `%LOCALAPPDATA%\Swapdesk-dev`.

Updates (1.1, D53): after this install, the service checks GitHub once a day for a newer signed release and installs it on a person's click, with no script and no UAC. Its applier stops the service and every process of the install folder, puts each new file in with the folder's ACL reset and the same install policy checked, and restores the old files when the new service does not start within 60 seconds. Its files live in `%ProgramData%\Swapdesk\update`, which `uninstall.cmd` removes; `install.cmd` stays the way to install by hand, and to go from 1.0 to 1.1.

## Test

1. Open the Start menu, type Swapdesk, start it. `Swapdesk.exe` finds the service with `service_pid("SwapdeskService")` and connects to the UI pipe `\\.\pipe\swapdesk\ui-<session>`; without the service it reports `NoService`.
2. A second PC on the same subnet with the network marked Private sees this one (UDP 5353) and can pair (UDP 47821) and connect (UDP 47820).
3. `Get-Service SwapdeskService` shows Running; `Get-Process swapdesk-helper` shows two processes (system and user role); `%ProgramData%\Swapdesk\trace.log` shows `running; console session ...` (read it as Administrator).

### Smart App Control

On a PC with Smart App Control on (`VerifiedAndReputablePolicyState` 1 under `HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy`; this PC has it on), unsigned programs are blocked: the service may fail to start (error 1260 or 4551) or `Swapdesk.exe` may not open. The script warns when it sees the state. Decision D22 (no paid certificate): until a signed build exists, the only way round it is turning Smart App Control off (recent Windows updates let it be turned back on later without reinstalling Windows), or building from source on a PC where it is off.

## Uninstall

```
packaging\windows\uninstall.cmd            removes everything, data included
packaging\windows\uninstall.cmd -KeepData  keeps %ProgramData%\Swapdesk (identity, trust store, settings, logs)
```

One UAC click, same as install; `-DryRun` shows the plan. In order it stops and deletes the service, removes the firewall rules, the shortcut, the saved policy (restores `SoftwareSASGeneration` to the saved value, or removes it when it was absent, but only when the install recorded the `SasSetValue` marker, i.e. it was run with `-EnableSas`; if someone changed it since, it is left and reported) and the key `HKLM\SOFTWARE\Swapdesk`, writes one Application event (id 1001) and removes the Event Log source `Swapdesk` (`Remove-EventLog`), deletes the installed files and then the install folder, and unless `-KeepData` the C5 data files and their folder. Only names the contracts list are deleted (CONTRACTS C5.2: `identity.bin`, `trust.bin`, `settings.bin`, `audit-sessions.log`, `audit-network.log`, `trace.log` and, from 1.1, `accounts.bin`; in each account folder `accounts\<SID>\` (ACCOUNTS.md 10.6): `identity.bin`, `trust.bin`, `settings.bin`, `account.bin`, `peers.bin` and the two audit logs; all with `.1`, `.tmp` and `.damaged-*` copies), and a folder is removed only when it is empty afterwards; `-Force` deletes both folders whole. A folder that is a link (reparse point) is never entered. The data files are deleted, never opened.

It also removes the installed apps entry and, for every user profile on the PC (from `HKLM\...\ProfileList`, real accounts only), the per-user Run value `Swapdesk` ("open at login"; the hive of a user who is not signed in is loaded under `HKU\SwapdeskUninstall`, changed and unloaded) and that user's logs, `%LOCALAPPDATA%\Swapdesk\logs` (a link is never entered; the folder `Swapdesk\` goes when empty). The user's logs go even with `-KeepData`. Run from `%ProgramFiles%\Swapdesk` (the apps list's button, or `uninstall.cmd` there) the elevated script first copies itself to a folder under `%WINDIR%\Temp` that only SYSTEM and Administrators can use and runs from there, so the install folder, `uninstall.cmd` and `uninstall.ps1` can be deleted; `uninstall.cmd` leaves the install folder as its working directory and ends without reading itself again.

It ends by checking: `Get-Service`, `sc.exe query` (1060 = does not exist), the firewall group, the two folders, the shortcut, the registry key, the Event Log source and the processes. Anything left is listed under `NOT CLEAN` and the exit code is 1. Exit code 0 means nothing is left.

Not removed: `%ProgramData%\Swapdesk.untrusted-*` folders (the service sets an untrusted folder of that name aside; look at one before deleting it by hand), `%LOCALAPPDATA%\Swapdesk-dev`, the per-user preferences file `%APPDATA%\Swapdesk\ui.bin`.

## Open at login and one window

`Swapdesk.exe` writes the Run value `Swapdesk` (`"%ProgramFiles%\Swapdesk\Swapdesk.exe"`) under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when the switch "Abrir ao ligar" is on and removes it when it is off (`swapdesk-sys-win/src/login.rs`); at each start the value is made to match the person's saved switch. Only the installed copy writes it: a development build leaves the registry alone. A second `Swapdesk.exe` in the same Windows session finds the mutex `Local\Swapdesk-ui` taken, brings the first one's window forward and exits (`instance.rs`).

## Facts the scripts rely on

- Install folder rule and the trusted SIDs (SYSTEM `S-1-5-18`, Administrators `S-1-5-32-544`, TrustedInstaller `S-1-5-80-956008885-...`): `crates/swapdesk-sys-win/src/launch.rs` (`InstallPolicy::production`, `check_no_other_writers`). The scripts use SIDs, not names: this PC's Windows is in Portuguese.
- Service name `SwapdeskService`, helpers launched from the service exe's own folder: `crates/swapdesk-service/src/app.rs`. The production service takes no argument and rejects `--instance` and `--dev-loopback` (`options.rs`).
- Ports: session 47820, pairing 47821 for the first account's slot (`DEFAULT_SESSION_PORT`, `DEFAULT_PAIR_PORT`); account slot k listens on 47820 + 2k and 47821 + 2k.
- Data files and rotation names: CONTRACTS C5.2, `crates/swapdesk-sys-win/src/trace.rs`, `crates/swapdesk-host/src/ui_hub/store.rs`.
- `SendSAS` needs a service and the policy: `SendSAS` page on Microsoft Learn (read 2026-11-08).
