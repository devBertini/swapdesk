# Contributing to Swapdesk

Swapdesk 1.0 is out, and what is planned next is listed in the [README](README.md#not-in-10-yet). Before a large change, open an issue to discuss it. The design documents ([docs/SPEC.md](docs/SPEC.md), [docs/CONTRACTS.md](docs/CONTRACTS.md), [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/DECISIONS.md](docs/DECISIONS.md)) are the source of truth, and a code change that contradicts them needs a document change in the same pull request. By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Build on Windows

Requirements:

- Rust stable, as pinned by [rust-toolchain.toml](rust-toolchain.toml) (channel `stable`, with `rustfmt` and `clippy`). The workspace sets `rust-version = "1.99"` and edition 2024 in [Cargo.toml](Cargo.toml).
- Visual Studio 2022 Build Tools and the Windows SDK.
- [cargo-nextest](https://nexte.st/): `cargo install cargo-nextest`.
- The `opus` crate builds libopus with CMake, and a long build path can break that build on Windows: if a build fails on path length, use a short `CARGO_TARGET_DIR`.

```sh
cargo nextest run --workspace          # the unit tests
cargo build --release                  # UI, helpers (panic = abort)
cargo build -p swapdesk-service --profile release-service   # service (panic = unwind)
```

The service is built with its own profile because it must isolate a panic to one session, while the interface and helpers abort (decision D15 in [docs/DECISIONS.md](docs/DECISIONS.md)).

### Development mode

The real service runs as LocalSystem under the Service Control Manager and needs the elevated install in [packaging/windows](packaging/windows/README.md). For development, run the same code as your own user.

```sh
# terminal 1: a development service (stop it by typing "stop", or creating a file named "stop" in its dev folder)
swapdesk-service --console [--instance N] [--dev-loopback]

# terminal 2: the interface (a development build), told which process to trust as the service
Swapdesk.exe --instance N --service-pid <pid of that swapdesk-service>
```

- `--console` runs without the SCM, with its stores and trace log under `%LOCALAPPDATA%\Swapdesk-dev\<instance>`, and starts helpers from its own folder.
- `--instance N` (0 to 99) lets several development copies share one PC. Pipe names get the suffix `-N` and both UDP ports move up by `10 * N`.
- `--dev-loopback` listens on 127.0.0.1 only and accepts loopback peers, so two instances on one PC can talk. Never use it on a machine that should be reachable.
- `Swapdesk.exe --service-pid` and `--instance` exist only in development builds: a plain `cargo build` (debug) has them, and a release build has them only with `cargo build --release -p swapdesk-app --features dev`. In a normal release they are unknown arguments (the program prints the accepted ones and exits with code 2), so a shipped interface always trusts the service the SCM reports.
- `Swapdesk.exe --service-pid <pid>` makes the interface accept that process as the pipe server instead of the service the SCM reports. `Swapdesk.exe --mock <scenario>` (development builds) runs the interface against a scripted backend.

In this mode the system helper does not have secure-desktop reach, because it runs as your user. Source: [crates/swapdesk-service/src/main.rs](crates/swapdesk-service/src/main.rs) and [crates/swapdesk-app/src/cli.rs](crates/swapdesk-app/src/cli.rs).

### Unsigned builds and Windows Smart App Control

Windows Smart App Control (and App Control for Business) can block unsigned executables, including freshly built test binaries (os error 4551). Release binaries stay unsigned until a signing path is chosen. Decision D22: no paid certificate or signing service. The free options under study are free code signing for open-source projects, a Microsoft Store package signed by Microsoft, and building from source. None is settled. For a blocked test binary, see [Test](#test) below.

## Build on macOS

The macOS crates (`*-mac`) are built and tested on a Mac with Xcode and Rust:

```sh
cargo nextest run -p swapdesk-shell-mac -p swapdesk-app-mac -p swapdesk-capture-mac -p swapdesk-codec-mac
bash scripts/mac/build-app.sh            # target/bundle/Swapdesk.app, signed with a local development certificate
bash packaging/macos/build-release.sh    # the release zip, signed ad hoc
```

`build-app.sh` creates a self-signed code-signing certificate in its own keychain file on the first run, so the macOS permissions survive rebuilds (decision D25).

## Test

```sh
cargo nextest run --workspace
```

Read the result by the command's own exit code and keep the log. Budgets for the loops:

- the unit cases a change reaches: under 30 seconds;
- a slice's loopback check: under 5 minutes;
- the whole gate before a milestone: under 10 minutes.

Run only the tests that reach what you changed while you work, and the whole suite before you open a pull request.

**Interactive tests.** A test that injects real input or moves the pointer is marked `#[ignore = "interactive: ..."]` and is never part of the default run. Run them only on an idle PC:

```sh
cargo nextest run --run-ignored only -E 'test(/interactive/)'
```

or per crate.

**Windows blocking a test binary (os error 4551).** Windows App Control or Smart App Control can block a freshly built unsigned binary. Deleting that executable and rebuilding clears it (recipe from [docs/CONTRACTS.md](docs/CONTRACTS.md)). If it keeps happening, the cause is the policy on your machine and not the code.

**Measurements** use the synthetic sources of the test fakes (contract C7), never a capture that films its own viewer.

## Repository layout

Crates live in `crates/`. Only the platform crates (`*-win`, `*-mac`) contain `unsafe`; the workspace forbids it everywhere else. Crate names and traits are fixed in [docs/CONTRACTS.md](docs/CONTRACTS.md).

| Crate | What it holds |
|---|---|
| `swapdesk-proto` | The wire protocol types and codec. No OS, no I/O, no `unsafe`. |
| `swapdesk-platform` | The traits platform crates implement (capture, codec, input, audio, and so on). |
| `swapdesk-lan` | LAN guard, DNS-SD discovery, socket setup (TTL 1), Wake-on-LAN packet. |
| `swapdesk-trust` | Device identity, pairing, TLS setup, trust store, lockout, audit log format. |
| `swapdesk-session` | Control stream state machine, permission enforcement, presence, resume. |
| `swapdesk-video` | Video sender and receiver over QUIC, staleness, rate control, keyframe requests. |
| `swapdesk-audio` | Opus encode and decode, jitter buffer, audio packets. |
| `swapdesk-keys` | Key tables between HID, Windows and macOS keys. |
| `swapdesk-swap` | The swap state machine (request, accept, reverse session). |
| `swapdesk-term` | Terminal and quick-action relay, terminal open policy and consent. |
| `swapdesk-termview` | Terminal model: VT parser, cell grid, scrollback, selection, key encoding. |
| `swapdesk-files` | File transfer with resume, SHA-256 check, safe receiving, remote folder listing. |
| `swapdesk-extras` | Clipboard sync, chat, Wake-on-LAN sender, reconnect policy. |
| `swapdesk-ipc` | The local message set and framing between service, helpers and interface. |
| `swapdesk-host` | OS-free process logic of the helper loop and the service relays. |
| `swapdesk-ui` | Interface state, command API, strings (pt-BR, en), design tokens, accessibility tree. |
| `swapdesk-fakes` | Test fakes: synthetic screen, recording input sink, sine source, log capture, and others. |
| `swapdesk-netsim` | A UDP socket wrapper that injects loss, delay and reordering for tests. |
| `swapdesk-crossdrive` | Test harness for runs between two machines. |
| `swapdesk-net-win`, `swapdesk-net-mac` | Network interface listing (physical only), network category and firewall checks. |
| `swapdesk-secret-win`, `swapdesk-secret-mac` | Secret store: DPAPI plus folder permissions on Windows, files readable only by the user on macOS (D31). |
| `swapdesk-sys-win` | Windows service control, helper launch, named pipes and their ACLs, session watcher, trace log. |
| `swapdesk-capture-win` | Screen capture with Desktop Duplication. |
| `swapdesk-codec-win`, `swapdesk-codec-mac` | H.264 with Media Foundation and NVENC on Windows, VideoToolbox on macOS. |
| `swapdesk-render-win`, `swapdesk-render-mac` | Video renderer: DirectComposition swap chain on Windows, sample buffer layer on macOS. |
| `swapdesk-input-win`, `swapdesk-input-mac` | Input injection and the keyboard grab (`SendInput` and a low-level hook; a CGEventTap). |
| `swapdesk-audio-win`, `swapdesk-audio-mac` | System audio: WASAPI loopback and playback; Core Audio playback. |
| `swapdesk-shell-win`, `swapdesk-shell-mac` | The native shells: Win32 with Direct2D, AppKit. |
| `swapdesk-core-mac` | The in-process service core of the macOS app (D27, D32). |
| `swapdesk-capture-mac` | Screen and system audio capture with ScreenCaptureKit. |
| `swapdesk-app-mac` | Binary: `Swapdesk.app`, interface, viewer and host. |
| `swapdesk-service` | Binary: the Windows service (and the `--console` development mode). |
| `swapdesk-helper` | Binary: the session helper started by the service. |
| `swapdesk-app` | Binary: `Swapdesk.exe`, the interface and viewer. |

Other folders: `packaging/windows/` (the install scripts), `packaging/macos/` (the release zip), `probes/mac/` (the MacBook probe).

## Code rules

- **`unsafe` only in platform crates.** Crates named `*-win` or `*-mac` may use `unsafe`. Every other crate forbids it through the workspace lints. `swapdesk-proto` has no OS, no I/O and no `unsafe`.
- **No new dependency without discussion.** A new third-party crate needs the maintainers' agreement and a note in [docs/DECISIONS.md](docs/DECISIONS.md). `[workspace.dependencies]` and `Cargo.lock` change only with that agreement. Prefer the standard library and the OS bindings already used.
- **LAN-only is a rule, not a setting.** Nothing may add a relay, cloud service, telemetry, NAT traversal or any path that leaves the local network. The guard (physical interfaces only, on-link peers, hop limit 1) must not be weakened.
- **Privilege.** The viewer runs as the logged-in user, never SYSTEM or root. The terminal spawns as the logged-in user. No local IPC message carries a file path to the service.
- **Never log secrets:** the private key, pairing nonces and codes, the TLS exporter output, tokens, terminal bytes, text input, clipboard, chat or file contents.
- **User-facing strings live only in `swapdesk-ui`** (languages pt-BR and en), together with the design tokens. Shells and other crates do not hold their own strings.
- Every screen follows the responsiveness rules R-UI-1 to R-UI-12 in the spec.
- Code, comments and documents are written in English. The one exception is [README.pt-BR.md](README.pt-BR.md), the Portuguese copy of the README: a change to the README changes it in the same pull request.
- Run `cargo clippy` and `cargo fmt` before you push.

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org/): `<type>(<scope>): <what>`, one line, for example `fix(trust): refuse a pairing while locked out`. Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `chore`.
- Branch names look like `<type>/<kebab-slug>`.
- Keep a pull request to one change, with a test that fails without it for a bug fix.
- Fill in the pull request template.

## Licensing of contributions

Swapdesk is licensed under either the Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT license ([LICENSE-MIT](LICENSE-MIT)), at the user's option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions. By opening a pull request you confirm that you have the right to submit the work under these terms.

No Developer Certificate of Origin sign-off is required.
