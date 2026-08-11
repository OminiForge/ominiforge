<div align="center">

# Ominiforge

**A high-performance, extensible agent platform built in Rust.**

[![CI](https://github.com/OminiForge/ominiforge/actions/workflows/ci.yml/badge.svg)](https://github.com/OminiForge/ominiforge/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

[Documentation](https://ominiforge.github.io/ominiforge/) ·
[Architecture](doc/design/architecture.md) ·
[Contributing](CONTRIBUTING.md) ·
[Discussions](https://github.com/OminiForge/ominiforge/discussions)

</div>

---

Ominiforge is a platform for building capable, long-running agents. Through
extension it can serve as a coding agent, a personal research assistant, or an
automation assistant, integrating into software development, knowledge
management, and collaboration with external applications.

The core runtime is UI-agnostic and event-driven; the single user interface is a
GPUI client that runs either locally (linked directly against the core) or
remotely (connected to a Gateway). The command line is operator tooling
(`serve`, `eval`), not the conversational entry point.

> **Status:** early development (`0.x`). The platform is under active design and
> breaking changes between minor releases are expected.

## Design principles

These are the load-bearing ideas; the full rationale lives in
[doc/design/architecture.md](doc/design/architecture.md).

- **UI-agnostic core.** The agent runtime executes tasks, manages state, and
  emits events without any UI dependency, so the same core powers both local and
  remote modes.
- **Immutable history.** Session history is append-only. Compaction, forking,
  correction, and summarization produce new nodes or views — they never rewrite
  the original record. This enables replay, audit, failure analysis, and
  branching from any point.
- **Extension over MCP.** External tools plug in through the Model Context
  Protocol, an industry standard with a mature ecosystem. Built-in tools are
  written in Rust with no protocol overhead.
- **Readable history first, database as index.** The machine-readable event log
  (`events.jsonl`) is the source of truth; the index database is rebuildable
  from it at any time.
- **Event-driven execution.** Every step — text deltas, tool calls, results,
  usage, state changes, errors — is a typed event in one shared stream consumed
  by the UI, gateway, and monitoring.
- **Evolution by proposal only.** The system can analyze its own history and
  propose optimizations, skill drafts, or patches, but every change that affects
  behavior requires explicit user approval before it is applied.

## Repository layout

| Crate | Responsibility |
| ----- | -------------- |
| [`crates/ominiforge`](crates/ominiforge) | The core: the UI-agnostic agent runtime — tools, providers, sessions, gateway, LSP/MCP integration. A pure library, published to crates.io as `ominiforge`. |
| [`crates/ominiforge-net`](crates/ominiforge-net) | The client protocol abstraction (`ClientProtocol`) connecting any front-end to a local or remote core. |
| [`crates/ominiforge-cli`](crates/ominiforge-cli) | The `ominiforge` command line — `serve` today, a TUI later. Published to crates.io and as binaries on GitHub Releases. |
| [`crates/ominiforge-ui`](crates/ominiforge-ui) | The GPUI component library and theme system. |
| [`crates/ominiforge-gui`](crates/ominiforge-gui) | The GPUI desktop app — the full `ominiforge` CLI plus a graphical interface. Ships as desktop packages on GitHub Releases (placeholder while the UI matures). |

## Getting started

Prerequisites: [Nix](https://nixos.org/) with flakes enabled;
[direnv](https://direnv.net/) is recommended.

```sh
direnv allow     # enter the dev shell (or: nix develop)
```

The Nix flake provides the Rust toolchain, all developer tools, and the language
servers the project's own LSP integration consumes. `rust-toolchain.toml` is the
single source of truth for the toolchain channel.

Common tasks:

```sh
just ci          # run the full local check suite — the same set CI runs; green here means green CI
just doc         # preview the documentation site locally
```

See `just --list` for every available task.

## Documentation

Design contracts, operational runbooks, and decision records live under
[`doc/`](doc/README.md) and are published as a versioned site at
<https://ominiforge.github.io/ominiforge/>. The rendered site lets you read the
docs for a specific release; start with
[doc/design/architecture.md](doc/design/architecture.md).

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) for
the workflow, [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for expected behavior,
and [AGENTS.md](AGENTS.md) for the working agreements that apply to both human
and AI contributors.

- Questions and ideas → [Discussions](https://github.com/OminiForge/ominiforge/discussions)
- Bug reports and feature requests → [Issues](https://github.com/OminiForge/ominiforge/issues)
- Security vulnerabilities → report privately, see [SECURITY.md](SECURITY.md)

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with
an English subject line. Releases are fully automated with release-please —
merging to `master` is all it takes.

## License

Ominiforge is licensed under [GPL-3.0-or-later](LICENSE).
