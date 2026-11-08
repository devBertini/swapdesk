## What and why

## Checklist

- [ ] The commit message follows Conventional Commits.
- [ ] Tests that reach the change pass (`cargo nextest run`), and a bug fix has a test that failed before.
- [ ] `cargo clippy` and `cargo fmt` are clean.
- [ ] No `unsafe` outside `*-win` or `*-mac` crates.
- [ ] No new dependency (or it was discussed and recorded in `docs/DECISIONS.md`).
- [ ] Nothing leaves the LAN, and nothing secret is logged.
- [ ] User-facing strings are only in `swapdesk-ui`.
- [ ] Documents that the change affects are updated (`docs/SPEC.md`, `docs/PROTOCOL.md`, `docs/CONTRACTS.md`).
- [ ] I submit this under the project's licenses (MIT OR Apache-2.0).
