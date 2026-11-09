# Security policy

Swapdesk is pre-alpha software and has had no independent security audit. See the [threat model](docs/THREAT-MODEL.md) for what is designed, what is out of scope and what is not yet verified.

## Reporting a vulnerability

Report it through GitHub's private vulnerability reporting: on the repository page, open the **Security** tab and choose **Report a vulnerability**. This keeps the report private between you and the maintainers until a fix is ready.

Please do not open a public issue or pull request for a security problem.

A useful report includes:

- what you found and which component it affects (the crate or process, for example the service, a helper or the interface);
- the steps to reproduce it, and the Windows or macOS version you used;
- what an attacker gains (for example: read the screen, inject input, run code as SYSTEM, bypass pairing);
- whether you needed to be on the same LAN, to be a local user, or to run code as the same user (see the attacker classes in the threat model).

Do not include secrets, real device keys or captures of someone else's screen.

## What to expect

This is a small open-source project. The maintainers will try to acknowledge a report and keep the reporter informed, but no response time is promised yet.

A confirmed problem is fixed on the default branch first and published as a GitHub security advisory once the fix is out, crediting the reporter unless they ask otherwise.

## Supported versions

Only the latest release and the latest commit on the default branch get fixes.

## Scope

In scope: the code in this repository, the wire protocol and the security design in `docs/`. Out of scope: the items listed under "Out of scope" in the threat model, and vulnerabilities in third-party dependencies that are not reachable through Swapdesk (report those upstream).
