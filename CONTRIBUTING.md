# Contributing to Swapdesk

Swapdesk is pre-alpha and its design is still settling. Before a large change, open an issue to discuss it. The design documents ([docs/SPEC.md](docs/SPEC.md), [docs/CONTRACTS.md](docs/CONTRACTS.md), [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/DECISIONS.md](docs/DECISIONS.md)) are the source of truth, and a code change that contradicts them needs a document change in the same pull request. By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Build

Windows is built and tested here. The macOS crates are not written yet, and when they are they can only be built and tested on a Mac.

Requirements: Rust stable (pinned by `rust-toolchain.toml`, with `rustfmt` and `clippy`), Visual Studio 2022 Build Tools with the Windows SDK, and `cargo-nextest`. The `opus` crate builds libopus, which needs CMake; a long build directory path can break that, so use a short `CARGO_TARGET_DIR` if it does.

```sh
cargo build --release
cargo build -p swapdesk-service --profile release-service
```

The development mode of the service and the interface is described in the [README](README.md#development-mode).

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

## Code rules

- **`unsafe` only in platform crates.** Crates named `*-win` or `*-mac` may use `unsafe`. Every other crate forbids it through the workspace lints. `swapdesk-proto` has no OS, no I/O and no `unsafe`.
- **No new dependency without discussion.** A new third-party crate needs the maintainers' agreement and a note in [docs/DECISIONS.md](docs/DECISIONS.md). `[workspace.dependencies]` and `Cargo.lock` change only with that agreement. Prefer the standard library and the OS bindings already used.
- **LAN-only is a rule, not a setting.** Nothing may add a relay, cloud service, telemetry, NAT traversal or any path that leaves the local network. The guard (physical interfaces only, on-link peers, hop limit 1) must not be weakened.
- **Privilege.** The viewer runs as the logged-in user, never SYSTEM or root. The terminal spawns as the logged-in user. No local IPC message carries a file path to the service.
- **Never log secrets:** the private key, pairing nonces and codes, the TLS exporter output, tokens, terminal bytes, text input, clipboard, chat or file contents.
- **User-facing strings live only in `swapdesk-ui`** (languages pt-BR and en), together with the design tokens. Shells and other crates do not hold their own strings.
- Every screen follows the responsiveness rules R-UI-1 to R-UI-12 in the spec.
- Code, comments and documents are written in English.
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
