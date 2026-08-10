# Security Policy

## Supported versions

This project is in early development (`0.x`). **Only the latest release receives
security fixes**; older versions are not patched individually. Always use the latest
release.

| Version | Supported |
| ------- | --------- |
| latest  | ✅        |
| older   | ❌        |

## Reporting a vulnerability

**Please do not disclose vulnerabilities publicly.** Report privately via either:

- A [private vulnerability report](https://github.com/OminiForge/ominiforge/security/advisories/new) on GitHub
- Contacting the repository owner directly

### Please include

- Affected version / commit
- Vulnerability type and impact (e.g. RCE, privilege bypass, information disclosure)
- Reproduction steps or a PoC
- Possible fix direction (optional)

## Response targets

| Stage                | Target            |
| -------------------- | ----------------- |
| Acknowledge report   | within 72 hours   |
| Initial assessment   | within 7 days     |
| Fix and release      | severity-dependent |

We disclose details only after a fix is available (coordinated disclosure).

## How security fixes ship

- Security fixes go out through the normal release flow (see `doc/operation/release.md`)
- They are flagged **Security** in the CHANGELOG and the GitHub Release notes

## Scope

Ominiforge is an agent platform that executes shell commands, reads/writes files, and
accesses the network. Capabilities that work *as designed* (within the configured
permission and sandbox boundaries) are not vulnerabilities.

The security issues we care about are **boundary violations**, for example:

- Permission-model bypass (see `doc/design/permission.md`)
- Sandbox escape (see `doc/design/sandbox.md`)
- Secret / credential leakage (see `crates/ominiforge-core/src/secrets.rs`)
- Unauthorized access to a remote Gateway

If you're unsure whether a behavior is a vulnerability, report it — we'd rather assess
one extra report than miss a real one.
