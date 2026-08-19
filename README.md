<div align="center">

# Ominiforge

**A high-performance, extensible agent platform built in Rust.**

[![CI](https://github.com/OminiForge/ominiforge/actions/workflows/ci.yml/badge.svg)](https://github.com/OminiForge/ominiforge/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

[Documentation](https://ominiforge.github.io/ominiforge/) ·
[Architecture](doc/design/runtime-architecture.md) ·
[Contributing](CONTRIBUTING.md) ·
[Discussions](https://github.com/OminiForge/ominiforge/discussions)

</div>

---

Ominiforge is a platform for building capable, long-running agents. Through
extension it can serve as a coding agent, a personal research assistant, or an
automation assistant, integrating into software development, knowledge
management, and collaboration with external applications.

The target form is **one person, many machines, N+ agents working continuously**.
Each machine runs an ominiforge node that hosts long-running agent sessions. The
core is UI-agnostic and ships **no built-in UI** — presentation is delegated to
facades over the protocol (editors via ACP, project-management tools, IM, a TUI);
nodes interconnect over iroh (QUIC + NAT traversal). The command line is operator
tooling (`serve`), not the conversational entry point.

> **Status:** early development (`0.x`). The platform is under active design and
> breaking changes between minor releases are expected.

## Design principles

These are the load-bearing ideas; the full rationale lives in
[doc/design/runtime-architecture.md](doc/design/runtime-architecture.md).

- **Thin core, everything a plugin.** The core is only a composition runtime
  (context / fiber / registry / event bus) plus event log, hooks/approvals, and
  the extension loader. Even the agent loop is a plugin.
- **Immutable history.** Session history is append-only (`events.jsonl`).
  Compaction, forking, correction, and summarization produce new nodes or views —
  they never rewrite the original record. Task state is a replayable projection
  of the log, so components can be replaced without losing the current task.
- **Self-evolution through extensions.** Behavior, skills, tools, and strategies
  are hot-pluggable extensions (data files / MCP subprocesses / wasm), so the
  agent can improve the system without restarting or interrupting the current
  task.
- **Extension over MCP.** External tools plug in through the Model Context
  Protocol, an industry standard with a mature ecosystem. Built-in tools are
  written in Rust with no protocol overhead.
- **Event-driven execution.** Every step — text deltas, tool calls, results,
  usage, state changes, errors — is a typed event in one shared stream consumed
  by facades, gateways, and monitoring.
- **Transparency as an export.** State is queryable and every process is
  exportable to a standard format; the core does no presentation of its own.

## Repository layout

| Crate | Responsibility |
| ----- | -------------- |
| [`crates/ominiforge`](crates/ominiforge) | The core: the UI-agnostic agent runtime — tools, providers, sessions, gateway, LSP/MCP integration. A pure library, published to crates.io as `ominiforge`. |
| [`crates/ominiforge-cli`](crates/ominiforge-cli) | The `ominiforge` command line (a TUI later). Published to crates.io and as binaries on GitHub Releases. |

> The workspace is being restructured toward the thin composition runtime +
> plugins layout (`ofg-*` crates) described in
> [doc/design/runtime-architecture.md](doc/design/runtime-architecture.md); the
> three crates above are the current, pre-restructure state.

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
[doc/design/runtime-architecture.md](doc/design/runtime-architecture.md).

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) for
the workflow, [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for expected behavior,
and [AGENTS.md](AGENTS.md) for the working agreements that apply to both human
and AI contributors.

- Questions and ideas → [Discussions](https://github.com/OminiForge/ominiforge/discussions)
- Bug reports and feature requests → [Issues](https://github.com/OminiForge/ominiforge/issues)
- Security vulnerabilities → report privately, see [SECURITY.md](SECURITY.md)

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with
an English subject line. Releases are fully automated with release-plz —
merging to `master` is all it takes.

## License

Ominiforge is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT), at your option.
