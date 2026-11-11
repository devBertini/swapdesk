# Updates: a new version in one click

Status: design note, written 2026-11-10 for version 1.1. Nothing of it is built. Paths are relative to the repository root. "Read" means the claim comes from reading the code or the scripts at the time of writing; nothing was run. **UNPROVEN** = no primary page read today settles it and a slice must prove it; **GUESS** = a design number or inference still to confirm.

The request (owner, 10/11/2026): can the software update itself when GitHub has a new release, without rebuilding anything on the machines that already have it installed?

**Decided by the owner, 2026-11-10.** Not reopened here.
1. The app warns that a new version exists, and one click downloads, checks and installs it. Never silently.
2. The check for a new version is on by default.
3. It ships in version 1.1.
4. It asks at start and every 24 hours.
5. Stable releases only in 1.1: no switch for previews.
6. The manifest signing key lives on the owner's PC, protected by the system, with one offline copy; never on GitHub. The rotation key is offline only.
7. The Mac release is signed with a stable self-signed release certificate whose key is a secret of the macOS job on GitHub; it keeps the macOS permissions across versions and is not the root of trust.

## 0. What exists today (read)

- No code asks for a version. `HelloAck` carries `app_version` between peers (`swapdesk-host/src/service/config.rs`), and nothing else reads a version.
- `.github/workflows/release.yml`, on a `v*` tag: the Windows job builds `Swapdesk-windows-x64-<v>.zip` (`packaging/windows/build-release.ps1`, deflate through `ZipFile.CreateFromDirectory`), writes `<zip>.sha256`, runs `actions/attest` on the zip and creates a draft release (`--prerelease` for a tag with `-`). The macOS job builds `Swapdesk-macos-arm64-<v>.zip` (`packaging/macos/build-release.sh`), attests it and uploads both files to the draft. The owner publishes the draft by `PATCH /releases/<id>`.
- The Windows zip holds `SHA256SUMS` beside the three programs; `install.ps1` checks every copied file against it, stops the service and every Swapdesk process that runs from the install folder, sets the folder ACL that `InstallPolicy::production` accepts (`swapdesk-sys-win/src/launch.rs`), registers the service with restart on failure, adds the firewall rules and starts the service. It rolls nothing back (exit code 2: installed, service did not start).
- **The Mac release is signed ad hoc** (`build-release.sh` passes `--adhoc` to `scripts/mac/build-app.sh`). Its designated requirement is the build's cdhash, so every new build is a stranger to macOS and loses its permissions; the README tells users to remove and re-grant them after each update. Development builds use the self-signed "Swapdesk Development" certificate, whose requirement stays the same across rebuilds (D25, `build-app.sh --compare`); the owner's installed Mac app is updated that way and keeps its permissions.
- `ring` 0.17 is in the workspace: `Ed25519KeyPair` signs the device identity (`swapdesk-trust/src/identity.rs`) and `ring::digest` hashes file transfers (`swapdesk-files`). Ed25519 verification today goes through rustls (`tls.rs`). `Cargo.lock` holds no JSON parser and no zip or deflate crate.
- 1.1 keeps `MachineSettings` read-only and moves settings per account (`ACCOUNTS.md` 7.2, 14.2). UI ops are used up to 50 (51 unassigned) and notify kinds up to 22 (`FILES-UI.md`).
- The public texts promise that nothing leaves the LAN, and the FAQ says "no update check over the internet" (section 11).

## 1. Decision in brief

1. **Source.** One small signed manifest per release, fetched from `https://github.com/devBertini/swapdesk/releases/latest/download/swapdesk-update.txt` and its `.sig`: published, non-draft, non-preview releases only, no REST API, no JSON, no rate limit of the API. Asked 2 to 10 minutes after start, then every 24 hours.
2. **Trust.** An Ed25519 signature by a release key whose public half is compiled into the app, over a manifest that names the version and, per system, the file, its size and its SHA-256. The private key lives on the owner's PC and never on GitHub, so a taken-over GitHub account cannot ship code. The package is checked against the manifest before it is opened; nothing older than the running version is ever installed.
3. **Who checks.** On Windows the service (SYSTEM) checks, downloads into its own folder that users cannot touch, verifies and installs; the click comes from the console session's interface and needs no UAC. On the Mac the app does it in process, as it hosts in process.
4. **The swap.** Windows: the verified files go to a staging folder, and a copy of the running service started with `--apply-update` stops the service and every Swapdesk process, replaces the three programs, starts the service and rolls back when the new one does not report healthy within 60 s. Mac: the new bundle, checked to carry the same code signing identity, is swapped atomically with the running one beside it in Applications; the old bundle's own binary waits, reopens and rolls back the same way.
5. **The Mac release gets a stable signing identity** (a self-signed release certificate), so that updates keep the macOS permissions; without it, every one-click update would cost the user three trips to System Settings.

## 2. Where the version comes from

### 2.1 The endpoint

| Option | What it is | For | Against |
|---|---|---|---|
| **A. `releases/latest/download/<asset>` (recommended)** | A plain HTTPS GET of a fixed asset name; GitHub redirects to the latest release's file | "Latest" is "the most recent non-prerelease, non-draft release" (fact 1); no JSON to parse; not the REST API, so not its 60-per-hour limit; the same request from every install | GitHub does not document a limit or an availability promise for these downloads (UNPROVEN); previews cannot be reached this way |
| B. REST `GET /repos/devBertini/swapdesk/releases/latest` | JSON with `tag_name`, `prerelease`, `draft`, `assets[].name`, `size`, `browser_download_url`, `digest` | Documented fields; an asset's `digest` | A JSON parser in a SYSTEM process (none in the tree: a hand-written one or a new crate); 60 requests per hour per IP address unauthenticated, shared behind a NAT; a `304` is free only for an authorized request, so conditional requests save nothing here (facts 4, 5) |
| C. REST `GET /repos/.../releases` (list) | The same, including prereleases | The only way to offer previews | B's costs; drafts are listed only to accounts with push access, so no draft leaks (fact 2) |

**Recommendation: A.** The manifest is the project's own text, which is parsed only after its signature is checked. The download URL of the package is built from the signed version and file name (`https://github.com/devBertini/swapdesk/releases/download/v<version>/<file>`), so the manifest cannot send the app to another host. GitHub's redirects are followed over HTTPS only (fact 9), at most 5. No host list is pinned beyond the first: the signature makes the host irrelevant, and a pinned CDN name would strand every install if GitHub moved it.

Release procedure note: GitHub picks "latest" by `created_at` and `make_latest` (fact 1). A fix release for an older line (1.0.x after 1.1.0) must be published with `make_latest: false`; the updater refuses a lower version anyway (4.5).

### 2.2 When, from which process

| Option | For | Against |
|---|---|---|
| **Windows: the service (recommended)** | Runs from boot, one check per machine whoever is signed in; the file lands directly in a folder only SYSTEM and Administrators can open, so no local user can race it; the service is the one that installs | An HTTPS client in a SYSTEM process. Bounded: the system's HTTP stack parses the headers, the app reads at most 4 KiB of manifest and 256 bytes of signature, and checks the signature before reading any field |
| Windows: the interface (user) downloads, the service copies and checks | No network code as SYSTEM | Runs only while a window is open, once per account; the file passes through a folder the user controls, so the service must copy it first and check its own copy; two processes and an extra hand-off for the same result |
| **Mac: the app (only option in 1.x)** | The app is the service (D32) | Same-user processes can touch its folder: accepted, they can replace the app itself (THREAT-MODEL "same user") |

Schedule: the first check 2 to 10 minutes after the service or app starts (a random delay, so a power cut does not line up every machine of a house), then every 24 hours ± 1 hour (owner decision 4). A failed check is silent and waits for the next period. A `403` or `429` waits for the next period too, and "Procurar agora" says that GitHub asked to wait. Development instances (`--instance`, `--console`, `--scratch`) never check and never install, as they never read `ui.bin` (C5.1).

### 2.3 What leaves the machine

- The machine's public IP address, as for any HTTPS request; the TLS server name `github.com`, then the redirect target's.
- `GET /devBertini/swapdesk/releases/latest/download/swapdesk-update.txt` and `.sig`, the same two requests from every install of every version and system.
- Headers: `Host`, `Accept: */*`, `User-Agent: Swapdesk` (the REST API requires a user agent, fact 6; the download path does not document one, and a constant one says the least). No version, no system, no cookie (`WINHTTP_DISABLE_COOKIES`; an ephemeral `NSURLSession`, fact 15), no `Authorization`, no query string, no machine or account identifier, no `If-None-Match` (an ETag could act as a cookie).
- Only after the user's click: the package request, whose file name tells GitHub the system and the new version.
- As built (U3, 2026-11-11): WinHTTP sends exactly these headers. On the Mac, CFNetwork adds `Connection: keep-alive` and `Accept-Encoding: gzip, deflate`, which identify nobody, and its `Accept-Language` (the user's locale) is sent empty. The body's bound and hash count the decoded bytes. In a test binary the first `NSURLSessionConfiguration` costs about 10 s (CoreFoundation scans the test's folder as its bundle); the app bundle does not pay it (UNPROVEN until measured in the app).

## 3. What is downloaded

| Asset today | Worth for an automatic update | Kept for |
|---|---|---|
| `Swapdesk-<os>-<arch>-<v>.zip` | Deflate (no decoder in the tree, nothing in Windows to call from a service without a shell); the Mac zip holds the bundle with its signature | Manual installs |
| `<zip>.sha256` | Nothing against a taken-over account: it comes from the same place as the zip | Users checking a manual download |
| Build provenance attestation | Proves the zip was built by this repository's workflow; a taken-over account builds a malicious tag that attests just as well (4.1) | `gh attestation verify` by hand |

**Added to every release:**
- `swapdesk-update.txt`, the manifest (4.2), and `swapdesk-update.txt.sig`, made on the owner's PC (4.4).
- `Swapdesk-windows-x64-<v>.sdpkg` and `Swapdesk-macos-arm64-<v>.sdpkg`: the update package, the same files as the zip's programs (Windows: the three `.exe`; Mac: the signed `Swapdesk.app` tree), in a trivial uncompressed format written and read by the new crate (`swapdesk-update`): a magic, a count, then per entry a relative path (ASCII letters, digits, `.`, `-`, `_`, `/`; no `..`, no leading `/`, no backslash), a mode (0644 or 0755), a size and the bytes. No symlinks: the packer refuses them. Size: about 12 MB on Windows instead of the zip's 5 MB (read: `target/package`), the cost of having no decoder. Attested like the zips.

## 4. Verification before anything runs

A wrongly trusted update is code as SYSTEM on every Windows install and code with Screen Recording and Accessibility on every Mac. Everything below happens before a byte of the package is unpacked, and the Windows checks run in the service on its own copy.

### 4.1 What can carry the trust

| Option | What it proves | Against a taken-over GitHub account | Cost |
|---|---|---|---|
| **Ed25519 signature over a manifest, public key compiled in (recommended)** | That the holder of the release key approved exactly this version, these file names, sizes and hashes | **Holds**, if the key is not on GitHub (4.4) | `ring` already in the tree (`UnparsedPublicKey::new(&ED25519, pk).verify(msg, sig)`, fact 12); one manual step per release |
| GitHub's SHA-256 (`.sha256`, the asset `digest`) | Integrity of the transfer | Fails: same source | none |
| Artifact attestation verified offline | Built by a workflow of this repository from some commit | Fails: the attacker pushes a tag and the workflow attests it | Sigstore bundles, Fulcio certificates, the transparency log, DSSE and a trusted root to refresh; the trusted root has no expiry and carries no news of revocation (fact 8); several new crates |
| Authenticode (Windows) | A certificate's owner signed the binary | Depends on who triggers signing; SignPath signs builds that the GitHub workflow submits (D41) | Not available: the binaries are unsigned (D22), the free route is still an application (D41). A later extra layer (5.3), never the root |
| Mac code signature against the running app's designated requirement | Same identity as the installed app, so macOS keeps the permissions | Holds only as well as the Mac signing key is kept; that key sits in CI (open point 4) | `objc2-security` `SecStaticCode`, `SecCode`, `SecRequirement` features, in the tree (fact 13). Required on the Mac as a second check (7.2), not as the root |
| Notarization | Apple scanned it | n/a | Needs Apple's paid account; out (D3, D41) |

**Recommendation: the Ed25519 manifest is the root of trust on both systems.** An attacker needs both the release key and control of what `github.com/devBertini/swapdesk` serves (or a broken TLS) to install anything: either alone is not enough.

### 4.2 The manifest

ASCII, LF, at most 4 KiB, one `key value` per line, every key exactly once except `file`, unknown keys refused (a new key means a new first line):

```
swapdesk-update 1
version 1.1.1
channel stable
apply auto
file windows-x64 Swapdesk-windows-x64-1.1.1.sdpkg 12401664 <64 hex>
file macos-arm64 Swapdesk-macos-arm64-1.1.1.sdpkg 13107200 <64 hex>
```

- `channel` is `stable` or `preview`; an install accepts only `stable` (owner decision 5: 1.1 has no switch for previews; the key stays in the manifest so a later version can add one).
- `apply` is `auto` or `manual`. `manual` marks a release that needs `install.cmd` (a new firewall rule, a service setting) or a manual step on the Mac: the app then warns with "Abrir a página da versão" instead of the install button.
- The size is the exact package size; the hard cap compiled in is 256 MiB (GUESS: 20 times today's size).
- The signature file: `swapdesk-update-sig 1`, then `key <16 hex>` (the first 8 bytes of the SHA-256 of the public key) and `sig <128 hex>`; at most 256 bytes. The signed message is `"swapdesk-update-manifest\n"` followed by the manifest's exact bytes, so no signature made for another purpose can be replayed as one.

### 4.3 The order of the checks

1. Fetch the signature (256 bytes at most) and the manifest (4 KiB at most); anything longer is cut and refused.
2. Find the key by its id among the compiled keys (4.4); verify. A failure ends here: nothing of the manifest is read, the trace says "invalid update signature" (never the bytes) and shown only on "Procurar agora".
3. Parse; refuse a wrong first line, an unknown or repeated key, a malformed version, a missing entry for this system.
4. Offer only when `version` is greater than the running version and the channel matches (4.5).
5. After the click: check free space for twice the package plus 32 MiB; download into the update folder with the size from the manifest as the bound (one byte over aborts); hash while writing.
6. Size and SHA-256 equal to the manifest's, or the file is deleted.
7. Unpack into an empty staging folder; refuse a bad path, a duplicate, a missing expected file (Windows: exactly `Swapdesk.exe`, `swapdesk-service.exe`, `swapdesk-helper.exe`; Mac: `Swapdesk.app/Contents/MacOS/Swapdesk`, `Info.plist`, `_CodeSignature/CodeResources`).
8. Mac only: the staged bundle's code signature is valid and satisfies the running app's designated requirement, and its `CFBundleIdentifier` and `CFBundleShortVersionString` match (7.2).

### 4.4 The release key

| Option | Who signs, when | A taken-over GitHub account | Cost |
|---|---|---|---|
| **The owner's PC (recommended)** | After the workflow has built the draft, a tool on this PC downloads the two `.sdpkg` from the draft, hashes them itself, writes the manifest, signs it, and the two files are uploaded to the draft before it is published | Cannot sign | One more step per release, inside the release yes the owner already gives; this PC becomes a key holder |
| A repository secret used by a `release.yml` step, in a protected environment | The workflow, on the tag | Can sign: whoever controls the account can push a tag, approve the environment and change the workflow (fact 10) | No manual step; the signature then adds nothing over HTTPS |
| An offline machine or hardware token | The owner, by hand | Cannot sign | No such device in the project; more steps |

The owner signs what the workflow built. That protects against a taken-over account, not against a malicious commit merged by the owner or a poisoned build runner; the attestation and the public history stay the defence there.

**Keys.** The tool `swapdesk-release-sign` (a binary of `swapdesk-update`, never shipped) makes two Ed25519 keys once, in a step the owner runs:
- **A**, the signing key: a PKCS#8 file on this PC, protected with DPAPI user scope through `swapdesk-secret-win`, outside every repository, plus one offline copy (owner decision 6). The session refers to it by path only.
- **B**, the rotation key: generated at the same time, kept offline only, never used until A is retired.
- Both public halves are compiled into `swapdesk-update`; a manifest signed by either verifies.

**Rotation, loss, leak.** Lost A: the next release is signed with B and compiles in B and a new C (B's successor), without A. Leaked A: the same at once, plus a public note in the release and in `SECURITY.md`; until each install updates, a leaked A is usable only together with control of GitHub's answers. Both lost: no automatic path; the release notes ask for a manual install, and the new build compiles new keys. A test key exists only under `cfg(test)` and in the rehearsal feature (slice U9), never in a release build.

### 4.5 Downgrade and replay

- Never install a version lower than or equal to the running one (`CARGO_PKG_VERSION`), compared as numbers (`1.10.0 > 1.9.0`); a stable install never takes a `-preview` version.
- An old, validly signed manifest replayed by someone in the path only says "nothing new": it delays, it cannot downgrade. Accepted, like no network.
- A version that was installed and rolled back (6.4, 7.3) is remembered in `update\update-state.txt` and not offered again by itself; "Tentar de novo" stays.

## 5. The HTTPS client

| Option | For | Against |
|---|---|---|
| **The system's: WinHTTP on Windows, `NSURLSession` on the Mac (recommended)** | No new crate: `windows-sys` gains the feature `Win32_Networking_WinHttp` (in the 0.61.2 source, fact 11; a feature, not a crate), `objc2-foundation` gains `NSURLSession`, `NSURLRequest`, `NSURLResponse` (fact 14). TLS, certificate validation, proxies and TLS versions are the system's (fact 9: Windows 11 enables TLS 1.2 and 1.3 by default; ATS requires HTTPS, fact 16) | Two implementations behind one trait; tests of the real clients need the network or a loopback server |
| A crate (`ureq` with the rustls already in the tree, plus roots from `webpki-roots` or `rustls-platform-verifier`) | One client for both systems | New crates and the owner's yes (D4, D28); a bundled root list goes stale; the platform verifier brings more crates; proxies are ours to handle |

**Recommendation: the system's.** Details:
- WinHTTP: `WinHttpOpen` with `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` (system and per-user proxy settings, Windows 8.1 and newer; the default-proxy mode is deprecated, fact 9) and `WINHTTP_FLAG_SECURE_DEFAULTS` (TLS 1.2 or newer, no fallback; it implies asynchronous mode, so the client runs on its own thread with a status callback); redirect policy left at its default (never https to http), at most 5 redirects; cookies disabled; certificate errors never ignored. In `swapdesk-sys-win` (new `http.rs`), where the `unsafe` stays.
- Mac: an ephemeral session (fact 15), `downloadTaskWithRequest:completionHandler:` for the package, data tasks for the manifest. In `swapdesk-sys-mac` (new `http.rs`), which gains `objc2-foundation` and `block2` as dependencies: both already in `Cargo.lock`, a main-session change.
- The trait, in `swapdesk-update`: `get(url, max_bytes, sink, progress, cancel) -> Result<Status, HttpError>`; a production constructor accepts `https` only; a test constructor also accepts `http://127.0.0.1` for a loopback server.

## 6. The swap on Windows

### 6.1 Who installs

| Option | Clicks | For | Against |
|---|---|---|---|
| **The service starts an applier: a copy of the running service run as `--apply-update` (recommended)** | One | SYSTEM already; the code that does the swap and the rollback is the version that has been running here; it survives the service's stop | A second mode of the service binary to keep small and tested |
| The service replaces its own files and restarts through its recovery actions (exit with an error) | One | No second process | A running image cannot be replaced; recovery counts the restart as a failure (`sc failure`, `install.ps1` item 4); no rollback |
| The verified package's `install.ps1`, run as SYSTEM | One | One path for install and upgrade | PowerShell from a service; no rollback (exit code 2 leaves a broken install); the firewall and shortcut steps redo work that does not change between versions |
| The elevated instance behind UAC (`ELEVATION.md`) | Two (the click and UAC); a standard account needs an administrator's password | UAC sees the change | Breaks the one click; adds nothing to a payload that is already signed and newer |

**Recommendation: the applier.** The click is accepted from the console session's interface of any account, the rule of 1.0 for everything that reaches out (THREAT-MODEL, `ACCOUNTS.md` 7.1): a newer signed release of software already installed widens nobody's reach.

### 6.2 The sequence

1. The interface sends `UpdateInstall { version }`. The service refuses a version other than the one offered, an install already running, or a UI that is not the console's.
2. **A live session** (this machine hosting or viewing, any account): the reply is `NeedsConfirm { sessions }` and the interface shows the confirmation (10.3). Only `UpdateInstall { version, end_sessions: true }` goes on.
3. Download and checks 4.3 steps 5 to 7, in `%ProgramData%\Swapdesk\update\` (6.3), with progress notifications.
4. The service copies its own image to `update\applier\swapdesk-service.exe`, stages the package's files in `update\staging\`, tells every interface `UpdateRestarting` (each saves its window state and exits), records which Windows sessions had an interface, and starts the applier as SYSTEM, outside the helpers' job object (`CREATE_BREAKAWAY_FROM_JOB` if the service runs in one; GUESS: the SCM does not end a stopped service's children).
5. The applier: writes `applying <old> <new>` to the journal `update\journal`; stops `SwapdeskService` through the SCM (30 s, then ends the process); ends every `Swapdesk`, `swapdesk-helper` and `swapdesk-service` process whose path starts with `<install folder>\` (the rule of `install.ps1` item 1) after 10 s; copies the installed files to `update\rollback\<old>\`; puts each new file into place: a copy in `update\swap\` is moved over the installed one (`MoveFileExW` with replace and write-through, a copy across volumes), so the staged and backed-up files stay whole for a second try; before each program the service is stopped again and the folder swept, because the SCM's recovery actions may have restarted it, retrying for 10 s when a scanner holds a handle (UNPROVEN that one does); resets each file to inherit the folder's ACL and checks it with `InstallPolicy::production().verify`; starts the service.
6. The new service, once its pipes and endpoints are up, writes `committed <new>` into the journal. The applier waits 60 s for it (GUESS).
7. Committed: the applier deletes the staging copy and exits; the new service relaunches `Swapdesk.exe --after-update` in the recorded sessions as their users (the `WTSQueryUserToken` and `CreateProcessAsUserW` path that starts the user helper), writes the Event Log entry 1002 and deletes `rollback\` at its next start.

### 6.3 Files and permissions

- `%ProgramData%\Swapdesk\update\` is made by the service with the data folder's rules (C5.2): owner SYSTEM, SYSTEM and Administrators only, a folder with another owner or a reparse point moved aside, never followed. A standard user can neither read the download nor swap it between check and install.
- `update\update-state.txt` (text, first line `swapdesk-update-state 1`, machine-wide): `check` (default on), `last` (the last check, Unix ms), `dismissed <version>`, `rolled-back <version>` lines. Missing or unreadable reads as the defaults. Beside it: `journal` (one line per step: `applying <old> <new>`, `committed <new>`, `failed <old> <new>`, each written through), `staging\`, `rollback\<old>\`, `applier\` and `applier.log`.
- The install folder only ever holds the five installed names, so `install.cmd` (which stops on an unknown file) and `uninstall.cmd` keep working; `uninstall.ps1` deletes the folder `update\` whole (a link never entered).

### 6.4 Rollback and interruptions

- No commit in 60 s, or the service stops again: the applier stops it, puts the files of `rollback\<old>\` back, starts the old service and writes `failed <old> <new>` to the journal (before the old service starts, so that service does not resume the apply); the old service reads it at its start and records the version as rolled back in `update-state.txt`. Event Log entries 1002 to 1004 are not written yet: `update\applier.log` holds the applier's outcome. The old service tells the interface "A versão X não iniciou; a Y foi restaurada".
- The rollback fails too: (Event Log entry 1004, not written yet) the trace names the folder of the old files; the service's recovery actions keep trying; the way out is `install.cmd` from the zip, which the failure text names.
- Power lost mid-way: the install folder holds a complete set at every moment (each file is replaced, never removed first), maybe of two versions. Whichever service starts finds `applying` without `committed` and no applier alive, and starts the applier again from staging (`--apply-update <old> <new>`), or with `--roll-back <old> <new>` when staging is gone; a service never commits over a set no applier checked (`swapdesk_update::apply::at_start`). A second try keeps the rollback copy of the first (made under another name, renamed once complete), so a rollback never restores a mixed set.
- The restore fails: the journal says `broken <old> <new>` and the rollback folder is kept; the starting service logs where the old files are.
- Data: the update never opens `identity.bin`, `trust.bin`, `settings.bin`, the account folders or the logs (section 9).

## 7. The swap on the Mac

### 7.1 Keeping the permissions: the release signing identity

macOS records an app's designated requirement with each privacy grant and checks every later version against it; an ad hoc signature's requirement is tied to that one build (fact 17). With the 1.0 release signing, a one-click update would leave Screen Recording, Accessibility and Input Monitoring granted to a requirement the new build does not meet. **So the Mac release is signed from 1.1 on with a self-signed "Swapdesk Release" certificate**, made once like the development one (`build-app.sh` already does this for "Swapdesk Development"), with the identifier `io.github.devbertini.swapdesk`. Its requirement is then the same for every release, as `build-app.sh --compare` proves for the development certificate.
- Its private key is an environment secret of the `macos` job, used only by the signing step (owner decision 7). It is not the root of trust: the manifest key is.
- Gatekeeper still refuses the first manual open of a downloaded app (no Developer ID): "Open Anyway" stays in the README for 1.1.0. Files the app writes itself are quarantined only when the app asks for it (`LSFileQuarantineEnabled`, fact 18; that the default is "no" is UNPROVEN, slice U6 reads `xattr` on the swapped bundle).
- Moving from the ad hoc 1.0 to the release certificate costs one more round of grants, at the manual install of 1.1.0, which the 1.0 README already announces for every update.

### 7.2 The sequence

1. Same click, same live-session confirmation (10.3); the app is the host, so its sessions end.
2. Download into `~/Library/Application Support/Swapdesk/updates/` (0700, like `secrets`), checks 4.3 steps 5 to 8. Step 8: `SecStaticCodeCreateWithPath` on the staged bundle, `SecStaticCodeCheckValidity` with strict validation and the requirement from `SecCodeCopySelf` and `SecCodeCopyDesignatedRequirement`.
3. Where the running bundle is: refuse a path under `AppTranslocation` ("Mova o Swapdesk para Aplicativos") and a parent folder the user cannot write (a standard account and an app copied by an administrator: "Só um administrador deste Mac pode substituir o Swapdesk", with the release page).
4. Unpack into `<parent>/.Swapdesk-<new>.app` beside the running bundle (same volume), check again (step 8), then `renamex_np(staged, running, RENAME_SWAP)` (fact 19; its atomicity on the Applications folder is UNPROVEN): the new bundle now has the running path and the old one the hidden name.
5. Start the watcher: the old bundle's own binary at its hidden path with `--update-watch <pid> <new version>`, detached (`setsid`), no window, no `NSApplication`. The app writes the journal and quits.
6. The watcher waits for the old process to end, opens the app at the running path (`/usr/bin/open`, UNPROVEN flags), and waits 60 s for the new app's `committed <new>` in the journal, written once its service core is up.
7. Committed: the watcher deletes the hidden old bundle and exits. Not committed: it ends the new app, swaps back with `RENAME_SWAP`, opens the old one and writes `failed <new>`.

### 7.3 What may still break on the Mac (UNPROVEN, each a check in slice U6)

- **App Management.** Apple's page says macOS lets apps "sharing the same Team ID" modify an app's bundle (fact 20); a self-signed app has no Team ID. Whether a self-signed app may swap its own bundle, and whether swapping the whole directory counts as modifying it, is unknown. If macOS refuses, it asks the user for App Management for Swapdesk once; the failure text then says so (10.4).
- The login item (`SMAppService.mainApp`, fact 21) after the bundle at its path is replaced.
- Other accounts of the Mac running the same bundle keep running the old code until they quit; their app sees a bundle newer than itself and says "Uma nova versão já foi instalada; reabra o Swapdesk" instead of offering an install.

## 8. Reaching 1.1, and what 1.0 does

1.0 has no updater: it never learns of 1.1. The 1.1.0 release notes and both READMEs say, in their first lines:

- en: "Version 1.0 cannot update itself. Install 1.1.0 by hand once: on Windows, run `install.cmd` from the new zip (it upgrades in place); on the Mac, replace the app in Applications and allow its permissions again (1.0 was signed in a way macOS cannot recognise across versions). From 1.1.0 on, Swapdesk tells you when a new version is out and installs it in one click."
- pt-BR: "A versão 1.0 não se atualiza sozinha. Instale a 1.1.0 à mão uma vez: no Windows, rode o `install.cmd` do novo zip (ele atualiza no lugar); no Mac, substitua o app em Aplicativos e permita de novo as permissões (a 1.0 foi assinada de um jeito que o macOS não reconhece entre versões). A partir da 1.1.0, o Swapdesk avisa quando sai uma versão nova e a instala com um clique."

The notes also carry 1.1's own news (`ACCOUNTS.md`: every account starts without pairings). The first real one-click update is from 1.1.0 to the next release; section 14 says how it is rehearsed before.

## 9. Protocol and data

- Two machines update at different times. PROTOCOL section 1 already covers it: a message or value of minor N goes only to a peer that announced N or more, and the major is the ALPN. The updater changes nothing on the wire and adds no message: no peer can announce, offer or carry an update (a LAN update channel is rejected, section 16).
- A release offered with `apply auto` must not raise the protocol major, or the machines updated first could no longer reach the others; a major change ships as `apply manual` with a note.
- Data 1.0 and 1.1 wrote must still load (a rule of the project since 1.0.0 was published); the updater never opens them. New rule for every later release: a version may rewrite a data file in a format the previous version cannot read only after it has written `committed`, so a rollback always finds data it can read.
- On the Mac the data in `~/Library/Application Support/Swapdesk` is outside the bundle and untouched by the swap; the bundle identifier and the signing identity stay, so the permissions and the login item stay (7.1, 7.3).

## 10. The interface

### 10.1 Where it shows

- **Início:** a new first card `HomeCard::Update` among the page's cards (`home.rs`; painted with `card.rs` `Look::Inline`, centred content, D51). It shows when a version is offered and not dismissed, during the install, after a failure, and once after the restart.
- **Ajustes:** a section "Atualizações": the version line, the switch, "Procurar agora", the last check, and the same state lines as the card while something runs.
- Nothing else: no window comes to the front, no system notification (never silently, and never louder than a card). The Windows tray and the Mac menu bar item can carry a line in 1.2, when the tray exists (D38), so both systems stay alike.
- The same screens and texts on both systems (one interface on both systems, D37).

### 10.2 States (R-UI-13: what it waits for, the time left, a way out, the result or the reason)

| State | Card | Buttons (negative left, positive right) |
|---|---|---|
| Offered | title, body, "O que mudou" link | "Agora não", "Atualizar agora" |
| Live session | the confirmation (10.3) | "Cancelar", "Encerrar e atualizar" |
| Downloading | bar, "Baixando 4,1 de 12,4 MB", time left from the measured rate | "Cancelar" (deletes the partial file) |
| Verifying | spinner, "Conferindo a assinatura" (about a second) | none |
| Installing | spinner, "Instalando…", "até 1 minuto" counted from the applier's 60 s | none (past the point of no return) |
| Restarted | "Swapdesk atualizado para 1.1.1." | "O que mudou", "Fechar" |
| Failed | the reason (10.4) | its way out |

The install button is disabled while an install runs, from any interface of the machine. "Agora não" hides the card for that version only; Ajustes keeps showing it.

### 10.3 The confirmation with a live session

Shown when any session is live. When this machine is being controlled, it adds a line for the person who may be clicking from elsewhere: the session ends, and coming back needs "Entrar sem perguntar" or someone at this computer to accept. That is the answer for a machine nobody is at: it is updated only by a person's click, at it or through a session, never by itself (owner decision 1).

### 10.4 Texts (approved by the owner, 2026-11-11, D58)

| Key | pt-BR | en |
|---|---|---|
| `UpdTitle` | "Nova versão: {0}" | "New version: {0}" |
| `UpdBody` | "Você usa a {0}. A atualização vem do GitHub, é conferida pela assinatura do Swapdesk e instalada; o Swapdesk fecha e abre de novo." | "You have {0}. The update comes from GitHub, is checked against the Swapdesk signature and installed; Swapdesk closes and opens again." |
| `UpdInstall` / `UpdLater` / `UpdNotes` | "Atualizar agora" / "Agora não" / "O que mudou" | "Update now" / "Not now" / "What changed" |
| `UpdLiveBody` | "Há uma sessão ativa com {0}. Atualizar agora encerra a sessão." | "A session with {0} is active. Updating now ends it." |
| `UpdLiveRemote` | "Se você controla este computador de longe, conecte-se de novo em um minuto. Sem \"Entrar sem perguntar\", alguém aqui terá de aceitar." | "If you control this computer from elsewhere, connect again in a minute. Without \"Enter without asking\", someone here has to accept." |
| `UpdLiveConfirm` | "Encerrar e atualizar" | "End and update" |
| `UpdDownloading` | "Baixando {0} de {1}" | "Downloading {0} of {1}" |
| `UpdVerifying` | "Conferindo a assinatura" | "Checking the signature" |
| `UpdInstalling` | "Instalando. O Swapdesk vai fechar e abrir de novo." | "Installing. Swapdesk will close and open again." |
| `UpdDone` | "Swapdesk atualizado para {0}." | "Swapdesk was updated to {0}." |
| `UpdErrNetwork` (way out: "Tentar de novo") | "Não foi possível falar com o GitHub. Confira a conexão com a internet." | "Could not reach GitHub. Check the internet connection." |
| `UpdErrWait` ("Tentar mais tarde" closes) | "O GitHub pediu para esperar. Tente mais tarde." | "GitHub asked to wait. Try again later." |
| `UpdErrSignature` ("Abrir a página da versão") | "O que foi baixado não tem a assinatura do Swapdesk. Foi apagado e nada foi instalado." | "What was downloaded does not carry the Swapdesk signature. It was deleted and nothing was installed." |
| `UpdErrMismatch` ("Tentar de novo") | "O arquivo baixado não confere com a versão anunciada. Foi apagado e nada foi instalado." | "The downloaded file does not match the announced version. It was deleted and nothing was installed." |
| `UpdErrDisk` ("Tentar de novo") | "Falta espaço em disco: a atualização precisa de {0}." | "Not enough disk space: the update needs {0}." |
| `UpdErrRolledBack` ("Baixar à mão", "Tentar de novo") | "A versão {0} não iniciou. A {1} foi restaurada e continua funcionando." | "Version {0} did not start. {1} was restored and keeps working." |
| `UpdErrMacAdmin` ("Abrir a página da versão") | "Só um administrador deste Mac pode substituir o Swapdesk em Aplicativos." | "Only an administrator of this Mac can replace Swapdesk in Applications." |
| `UpdErrMacLocation` | "Mova o Swapdesk para a pasta Aplicativos para que ele possa se atualizar." | "Move Swapdesk to the Applications folder so it can update itself." |
| `UpdErrMacAppManagement` ("Abrir Ajustes do Sistema") | "O macOS não deixou o Swapdesk se substituir. Permita em Privacidade e Segurança > Gerenciamento de Apps e tente de novo." | "macOS did not let Swapdesk replace itself. Allow it in Privacy & Security > App Management and try again." (only if 7.3 proves it needed) |
| `UpdManual` ("Abrir a página da versão") | "Esta versão precisa ser instalada à mão." | "This version must be installed by hand." |
| `UpdSection` / `UpdVersion` | "Atualizações" / "Versão {0}" | "Updates" / "Version {0}" |
| `UpdSwitch` | "Procurar novas versões no GitHub" | "Check GitHub for new versions" |
| `UpdSwitchHint` | "Uma vez por dia este computador pergunta ao GitHub qual é a versão mais recente. Nada sobre você ou este computador é enviado." | "Once a day this computer asks GitHub which version is the latest. Nothing about you or this computer is sent." |
| `UpdCheckNow` / `UpdLast` / `UpdLatest` | "Procurar agora" / "Última procura: {0}" / "Você tem a versão mais recente." | "Check now" / "Last check: {0}" / "You have the latest version." |

The switch is machine-wide on Windows (one install, one check) and may be changed from the console session's interface of any account without UAC: turning a privacy switch off must be easy, and turning it on reaches nobody.

## 11. The LAN-only promise and the public texts

The session guard is not loosened. It applies to the peer sockets: discovery and the two QUIC endpoints, on physical interfaces, on-link peers, TTL 1, checked before any handshake (PROTOCOL 1). The update check is a different thing: an outgoing HTTPS request to one fixed host, made by the system's HTTP stack through the system's routing and proxy, with no listening socket; nothing it receives can open a session, reach a peer or change a grant; it carries no session data. `swapdesk-update` does not depend on `swapdesk-lan` or the `net` crates, and the guard's tests stay as they are. A test asserts that turning the switch off makes the updater send nothing.

| Place | Today | Proposed (owner's to approve) |
|---|---|---|
| README "What it is and why" | "...and nothing should leave it." | "...and your screen, keyboard and files should never leave it." |
| README, same section | "In 1.x nothing goes beyond the LAN, and the code enforces it" | "In 1.x no session, pairing or discovery goes beyond the LAN, and the code enforces it. The one request that leaves the LAN is the check for a new version, a plain HTTPS request to GitHub that you can turn off ([Updates](#updates))." |
| README "How it stays on the LAN", new last bullet | none | "The only connection beyond the LAN is the update check: HTTPS to github.com, with no identifier of you or the machine, turned off in Settings > Updates. It is not a peer connection and the rules above do not apply to it, nor does it change them." |
| README, new section "Updates" | none | what is fetched, the signature, the click, the switch, the 1.0 note of section 8 |
| README "Install on macOS" | "...after an update, remove Swapdesk from each list in Privacy & Security and allow it again." | "From 1.1 on, updates keep the permissions; installing 1.1 over 1.0 asks for them once more." |
| README.pt-BR, lines 16, 20, 50 and section "Como ele fica na rede local" | the same sentences in Portuguese | the same changes: "...e sua tela, teclado e arquivos nunca devem sair dela."; "Na 1.x nenhuma sessão, pareamento ou descoberta sai da rede local... A única requisição que sai da rede local é a procura por uma versão nova, um pedido HTTPS simples ao GitHub que você pode desligar ([Atualizações](#atualizações))." |
| docs/FAQ.md "Does anything leave my network?" | "No. There is no account, no relay, no cloud, no telemetry and no update check over the internet." | "Your sessions, screen, input and files never do: there is no account, no relay, no cloud and no telemetry. The only request beyond the LAN is the update check, once a day to github.com, with no identifier of you or the computer; turn it off in Settings > Updates." |
| THREAT-MODEL | assets, boundaries, attackers, out of scope | asset "the release signing key"; boundary 5 "the internet to the updater"; a section "The update channel" with section 12's rows; "Out of scope: anything beyond the LAN" gains "except the update check, covered above" |
| SPEC "Problem" | "...with nothing leaving the LAN." | "...with no session data leaving the LAN." |
| ARCHITECTURE | processes | the update check in the service (Windows) and the app (Mac), the applier and the watcher |

## 12. Threats

| Threat | What stops it, or what remains |
|---|---|
| Taken-over GitHub account (assets replaced, a malicious tag built and attested) | The manifest key is not on GitHub: no signature, no install. Remains: the attacker can withhold releases (users stay on their version) and can make the manual-download path serve anything (as today) |
| Leaked release key | Useful only together with control of what github.com serves for this repository, or a broken TLS. Rotation to B (4.4). Remains: installs that have not updated keep trusting A until they do |
| Man in the middle | TLS validated by the system; even a broken TLS cannot forge the signature. A replayed old manifest only delays (4.5) |
| Rollback to an old vulnerable version | The version must be greater than the running one; a rolled-back version is not offered again by itself |
| A forged "new version" that only annoys | Unsigned: never shown (4.3 step 2). Signed but replayed: never newer than what runs |
| Disk filled by a huge download | The size comes from the signed manifest and is the download's bound, under a 256 MiB cap; free space checked before; partial files deleted on cancel or failure |
| Local standard user racing the downloaded file (Windows) | The file, the staging folder and the applier copy live in a folder only SYSTEM and Administrators can open, made with C5.2's reparse-point rules; the service checks its own copy; the moved files are checked with `InstallPolicy` |
| Same-user process on the Mac | Can touch the update folder between check and swap: accepted, it can already replace the app (THREAT-MODEL "same user"); step 8 runs again on the staged bundle just before the swap |
| A standard user triggering an install, or a remote viewer clicking it | Allowed by design: a newer signed release; sessions end only after the confirmation |
| A peer on the LAN | No wire message about updates exists; the guard is unchanged |
| The machine nobody is at | Never installs by itself; updated only by a click, locally or through a session (10.3) |
| A new version that does not start | Rollback within 60 s (6.4, 7.2) |
| Parser bugs in a privileged process | Manifest and signature are bounded and parsed after the signature check; the package is opened only after its hash matches; both readers are property-tested and join the `fuzz/` targets of D40 |

## 13. Exact additions

- **New crate `swapdesk-update`** (OS-free, `unsafe` forbidden; a CONTRACTS entry): `Version`, `Manifest` (parse, verify with the compiled keys), `Package` (read, write), `Policy` (offer or not), `UpdateManager<H: HttpGet, S: Store, C: Clock>` (the schedule and the state machine, `UpdateView`), the bins `swapdesk-pack` and `swapdesk-release-sign`. Depends on `ring` only.
- **`swapdesk-sys-win`:** `http.rs` (WinHTTP), `update_apply.rs` (SCM stop and start, the process sweep by install folder, moves, ACL reset, journal). Feature `Win32_Networking_WinHttp` added to its `windows-sys` entry.
- **`swapdesk-service`:** the `--apply-update <staging>` mode, accepted only from the protected folder and only as SYSTEM; the schedule.
- **`swapdesk-host` `ui_hub`:** one machine-wide update module shared by the account hubs, its ops and notifications.
- **`swapdesk-ipc`:** ops `UpdateGet`, `UpdateCheckNow`, `UpdateInstall { version, end_sessions }`, `UpdateCancel`, `UpdateDismiss { version }`, `UpdateSetCheck { on }`; notify `UpdateChanged { view }` and `UpdateRestarting`. Numbers: the next free ones when built (52 and 23 onward as read today), assigned by the main session.
- **`swapdesk-sys-mac`:** `http.rs`, `update_mac.rs` (signature check, translocation and write checks, `renamex_np`, the watcher). Dependencies `objc2-foundation`, `block2`, and `objc2-security` features `SecStaticCode`, `SecCode`, `SecRequirement`, `CSCommon` (all in `Cargo.lock`).
- **`swapdesk-app` / `swapdesk-app-mac`:** `--after-update`, `--update-watch`.
- **`swapdesk-ui`, `swapdesk-paint`, both shells:** C6 commands and `UiEvent::UpdateChanged`, `HomeCard::Update`, the Ajustes section, the texts appended at the end of the strings table.
- **Release:** `release.yml` packs and attests the two `.sdpkg`; `build-release.sh` signs with the release certificate; the release procedure gains the signing step.

## 14. Build plan

"Red before" = the test fails, or does not compile, on the tree before the slice. U6 and U7 may run as parallel lanes after U4 (different crates; `swapdesk-paint` and `swapdesk-ipc` stay with the main session).

| Slice | Files | Tests (each red before) | Proven where |
|---|---|---|---|
| **U1 core** | `crates/swapdesk-update` (new) | `a_manifest_signed_by_a_release_key_verifies_and_any_flipped_byte_fails`; `a_manifest_signed_by_an_unknown_key_is_refused`; `a_signature_without_the_domain_prefix_is_refused`; `a_version_equal_or_lower_than_the_running_one_is_never_offered`; `ten_is_greater_than_nine_in_every_position`; `a_preview_manifest_is_never_offered_on_the_stable_channel`; `an_oversized_manifest_or_an_unknown_or_repeated_key_is_refused`; `a_package_round_trips_with_its_modes`; `a_package_path_with_dotdot_absolute_backslash_or_a_duplicate_is_refused`; `a_package_whose_size_or_hash_differs_is_refused_before_it_is_opened`; proptest `random_bytes_never_panic_the_manifest_signature_or_package_readers` | this PC, `cargo nextest run -p swapdesk-update`; the Mac compiles it |
| **U2 release tools** [owner: key ceremony] | `swapdesk-update/src/bin/{swapdesk-pack,swapdesk-release-sign}.rs`, `release.yml`, `build-release.ps1`, `packaging/macos/build-release.sh` | `pack_then_read_gives_the_same_tree`; `the_pack_tool_refuses_a_symlink`; `sign_then_verify_with_the_compiled_key_format`; `the_sign_tool_hashes_the_files_it_was_given_not_their_sha256_files` | this PC and the Mac pack their own builds; the workflow only on a preview tag (owner's yes) |
| **U3 HTTP clients** | `swapdesk-sys-win/src/http.rs`, `swapdesk-sys-mac/src/http.rs` | `a_body_longer_than_the_bound_is_cut_and_reported`; `cancel_stops_a_download_within_a_second`; `a_redirect_from_https_to_http_is_refused`; `no_cookie_or_identifier_header_is_sent` (loopback server, request bytes asserted); `#[ignore = "network: ..."]` `fetches_a_published_asset_through_the_redirect` (`releases/download/v1.0.0/Swapdesk-windows-x64-1.0.0.zip.sha256`, its known bytes) | Windows tests on this PC; Mac tests on the Mac; the network test by hand on both |
| **U4 the service's manager** | `swapdesk-update` (manager), `swapdesk-host` `ui_hub/update.rs`, `swapdesk-ipc`, `swapdesk-service` | `the_first_check_waits_its_delay_then_runs_every_period`; `the_switch_off_sends_nothing`; `development_instances_never_check`; `a_failed_or_rate_limited_check_is_silent_and_waits_for_the_next_period`; `install_is_refused_from_a_ui_outside_the_console_session`; `install_with_a_live_session_needs_the_confirmation`; `the_download_lands_only_in_the_update_folder_and_is_deleted_on_any_failure`; `a_rolled_back_version_is_not_offered_again_by_itself`; ipc `update_ops_round_trip` | this PC, fakes for HTTP and the clock |
| **U5 Windows applier** | `swapdesk-sys-win/src/update_apply.rs`, `swapdesk-service` (`--apply-update`), `uninstall.ps1` | `apply_replaces_every_file_and_starts_the_service` (fake SCM, temporary folders); `a_service_that_does_not_commit_in_time_is_rolled_back`; `an_interrupted_apply_is_finished_or_rolled_back_from_the_journal`; `moved_files_inherit_the_folder_acl_and_pass_the_install_policy`; `the_sweep_ends_only_processes_inside_the_install_folder` | units on this PC; the real install on the VirtualBox guest (D41) or the owner's install on the owner's order: hand check (a) |
| **U6 Mac** | `swapdesk-sys-mac/src/update_mac.rs`, `swapdesk-app-mac` | `a_bundle_signed_by_another_identity_is_refused`; `a_translocated_or_unwritable_location_is_refused_with_its_reason`; `swap_and_swap_back_leave_the_original_bundle`; `the_watcher_reopens_the_old_bundle_when_the_new_one_does_not_commit` | the Mac (`remote.sh test`); hand check (b) with the owner at the Mac for 7.3 and the permissions |
| **U7 interface** | `swapdesk-ui` (`home.rs`, `state.rs`, settings, `strings.rs`), `swapdesk-paint`, both shells, mock scenarios `update-offered`, `update-downloading`, `update-live-session`, `update-failed-signature`, `update-rolled-back`, `updated` | `the_update_card_comes_first_when_a_version_is_offered`; `each_failure_has_its_reason_and_a_way_out`; `the_install_button_is_disabled_while_an_install_runs`; `not_now_hides_the_card_until_a_newer_version`; `the_settings_switch_sends_update_set_check`; `the_wait_states_are_a_polite_live_region` | this PC and the Mac (window checks); one capture per state on the design canvas, owner batch |
| **U8 documents** | README (both), FAQ, THREAT-MODEL, SPEC, ARCHITECTURE, CONTRACTS, FLUXOS (new F-UPD flows), DECISIONS (the owner's three decisions as a new entry), SECURITY (key rotation note), `packaging/windows/README.md` | `check_flows.py --check` exits 0 | this PC |
| **U9 rehearsal** | feature `update-rehearsal` (test key, base URL of a test server), never in `release.yml` | a build A (`1.1.0-rc.1`) updates to B (`1.1.0-rc.2`) served from a loopback or LAN server, then a rollback with a B that never commits | the VirtualBox guest and the Mac; then once against a GitHub prerelease (`releases/download/<tag>/`), which needs the owner's yes for the tag |

What only a real published release proves: `releases/latest/download` serving the real manifest, signed with the real key, from 1.1.0 to the next version. The first release after 1.1.0 is that proof; until then U9 is the evidence, and the README says so.

Hand checks (owner batch): (a) Windows, installed service: offered card, the click, the restart, the window back, Atividade intact, then a forced rollback; (b) Mac: the same, then Screen Recording, Accessibility and Input Monitoring still granted, the login item still on, no Gatekeeper prompt; (c) the live-session confirmation from the Mac controlling the PC.

## 15. Facts relied on (all read 2026-11-10)

1. "The latest release is the most recent non-prerelease, non-draft release, sorted by the `created_at` attribute"; `make_latest`: "Drafts and prereleases cannot be set as latest", default `true`. https://docs.github.com/en/rest/releases/releases
2. List releases: "Information about published releases are available to everyone. Only users with push access will receive listings for draft releases." Same page.
3. "To link directly to a download of your latest release asset that was manually uploaded", the suffix is `/releases/latest/download/asset-name.zip`. https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases
4. "The primary rate limit for unauthenticated requests is 60 requests per hour", associated "with the originating IP address"; over it, `403` or `429` with `x-ratelimit-remaining` 0, and no retry before `x-ratelimit-reset`. https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
5. "Making a conditional request does not count against your primary rate limit if a `304` response is returned and the request was made while correctly authorized with an `Authorization` header." https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
6. "All API requests must include a valid `User-Agent` header"; "Requests with no `User-Agent` header will be rejected." https://docs.github.com/en/rest/using-the-rest-api/getting-started-with-the-rest-api
7. Release asset download: with `Accept: application/octet-stream` the API "will either redirect the client to the location, or stream it directly"; clients "should handle both a 200 or 302 response". https://docs.github.com/en/rest/releases/assets
8. Offline attestation verification needs the GitHub CLI, the artifact, the bundle and the trusted root; "Artifact attestations uses the Sigstore public good instance for public repositories"; "The key material in `trusted_root.jsonl` does not have a built-in expiration date ... You will not know if key material has been revoked". https://docs.github.com/en/enterprise-cloud@latest/actions/how-tos/secure-your-work/use-artifact-attestations/verify-attestations-offline
9. `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` "Uses system and per-user proxy settings ... Supported in Windows 8.1 and newer"; `DEFAULT_PROXY` deprecated; `WINHTTP_FLAG_SECURE_DEFAULTS` requires TLS 1.2 or newer, disables fallback and sets `WINHTTP_FLAG_ASYNC`; the agent string "is used as the user agent". https://learn.microsoft.com/en-us/windows/win32/api/winhttp/nf-winhttp-winhttpopen . Redirect policy default: "All redirects are followed, except those that originate from a secure (https) URL to an unsecure (http) URL"; at most 10 redirects by default; on Windows 11 "only TLS1.2 and TLS1.3 are enabled" by default. https://learn.microsoft.com/en-us/windows/win32/winhttp/option-flags
10. Environment secrets "are only available to workflow jobs that use the environment", after its rules such as required reviewers; "Users with GitHub Free plans can only configure environments for public repositories." https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments
11. `windows-sys` 0.61.2 has the feature `Win32_Networking_WinHttp` (and `Win32_Security_WinTrust`), `WinHttpOpen`, `WINHTTP_DISABLE_COOKIES`, `WINHTTP_FLAG_SECURE_DEFAULTS`, `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`: the crate's source, `~/.cargo/registry/src/.../windows-sys-0.61.2` (`Cargo.toml`, `Win32/Networking/WinHttp/mod.rs`).
12. `ring` 0.17.14: `signature::UnparsedPublicKey::verify(&self, message, signature)` and `signature::ED25519`: the crate's source (`src/signature.rs`, `ec/curve25519/ed25519/verification.rs`).
13. `objc2-security` 0.3.2 features `SecStaticCode`, `SecCode`, `SecRequirement`, `CSCommon` (which needs `bitflags`, already in `Cargo.lock` 2.13.2): the crate's `Cargo.toml` and `src/generated/SecStaticCode.rs`.
14. `objc2-foundation` 0.3.2 features `NSURLSession`, `NSURLRequest`, `NSURLResponse`, with `downloadTaskWithRequest_completionHandler` and `dataTaskWithRequest_completionHandler`: the crate's source (`src/generated/NSURLSession.rs`).
15. An ephemeral session configuration "uses no persistent storage for caches, cookies, or credentials". https://developer.apple.com/documentation/foundation/urlsessionconfiguration/ephemeral
16. App Transport Security "requires that all HTTP connections made with the" system's URL loading classes (the linked names did not survive the page's text export) "use HTTPS", and "blocks connections that fail to meet minimum security specifications". https://developer.apple.com/documentation/bundleresources/information-property-list/nsapptransportsecurity
17. "macOS solves this problem by recording your app's DR in its database of apps authorized to access the microphone. Each time your app tries to access the microphone, macOS checks that this version of the app satisfies the original DR." "Ad hoc signed code ... has a DR but it's tied to that specific version of the code." https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements
18. `LSFileQuarantineEnabled`: "A Boolean value indicating whether the files this app creates are quarantined by default." The page read does not state the default: UNPROVEN that files are not quarantined without it. https://developer.apple.com/documentation/bundleresources/information-property-list/lsfilequarantineenabled
19. `libc` 0.2.191 declares `renamex_np(from, to, flags)` and `RENAME_SWAP` for Apple targets: the crate's source (`src/unix/bsd/apple/mod.rs`). Apple's man page was not read: the swap's atomicity is UNPROVEN.
20. `NSUpdateSecurityPolicy`: "For signed apps, macOS allows apps from the same developer — those sharing the same Team ID — to modify the app's bundle." https://developer.apple.com/documentation/bundleresources/information-property-list/nsupdatesecuritypolicy
21. `SMAppService.mainApp`: "An app service object that corresponds to the main application as a login item." Its behaviour after the bundle is replaced is not described: UNPROVEN. https://developer.apple.com/documentation/servicemanagement/smappservice/mainapp

Not settled by any page read today (UNPROVEN): a rate limit or availability promise for `releases/latest/download`; App Management for a self-signed app swapping its own bundle (7.3); whether the SCM leaves an applier child running after the service stops (GUESS: yes); scanners holding handles on new files; the exact `open` flags for the watcher.

## 16. Options rejected

- **Silent or scheduled installs**, including "at night when idle": against owner decision 1.
- **Updates passed between peers over the LAN:** it would make every paired machine an update source and add a wire message a peer could abuse; the guard's promise is that peers exchange sessions only.
- **The GitHub artifact attestation as the root of trust:** a taken-over account produces valid attestations (4.1).
- **The signing key in a repository or environment secret:** the same account that a takeover controls can use it (4.4).
- **Reusing the zip:** needs a deflate decoder (a crate or a shell tool run as SYSTEM).
- **`install.ps1` as the swap:** PowerShell from a service, no rollback (6.1).
- **UAC for the install:** two clicks for a payload already signed and newer (6.1).
- **Replace-on-reboot (`MOVEFILE_DELAY_UNTIL_REBOOT`):** not one click; the machine runs the old version until someone reboots.
- **Keeping the Mac release ad hoc:** every update would cost the three permissions again (7.1).
- **A host allowlist for downloads:** would strand old installs if GitHub moved its CDN; the signature already makes the host irrelevant (2.1).

## 17. Still open for the owner

Answered in the dialog box on 2026-11-10 (decisions 4 to 7 at the top): how often it asks, previews, where the keys live.

1. **The texts of 10.4** and the README, FAQ and SPEC sentences of section 11: proposals until the owner approves them. They are shown with the real captures of each state on the design canvas (slice U7) and approved there, as the project's other texts were.

Checked by the main session on 2026-11-10: section 0 against `release.yml`, `packaging/` and `scripts/mac/build-app.sh` (the Mac release is signed ad hoc); the lock file holds `ring` 0.17.14, `objc2-foundation`, `objc2-security`, `block2` and `libc`, and no JSON, zip or deflate crate; `windows-sys` 0.61.2 has the feature `Win32_Networking_WinHttp`; fact 3 read again on GitHub's page. Fact 17 could not be read again (the page's text did not load): the project's own experience agrees with it (D25: the development certificate keeps the permissions, the ad hoc build loses them).

What is missing from this note: nothing of the brief is left unwritten. The facts marked UNPROVEN in section 15 are settled only by slices U3, U5, U6 and U9.
