# Contributing

Thanks for contributing! This project favors **automation over manual toil**: formatting,
lints, tests, labeling, and releases are all automated. Your job is the change itself and
a clear PR.

Working agreements that apply to humans and agents alike live in [AGENTS.md](AGENTS.md).
This file covers the mechanics.

## Development environment

See [README.md](README.md#getting-started). In short: Nix + `direnv allow`, then `just ci`.

## Branching model: single `master` trunk

- **`master` is the only long-lived branch** and stays releasable at all times.
- All changes go through a PR: branch off `master` → open PR → green CI → merge back.
- **Merge via squash**: the whole PR becomes one commit whose message is the PR title.
- `master` is protected: CI must pass and at least one approval is required; no
  force-push or direct commits.

## Commit messages: Conventional Commits (English subject)

Format: `type(scope): subject`

```text
feat(ui): add chat panel theme tokens
fix(agent): fold orphaned todo steps on resume
```

**Rules (enforced by CI on the PR title):**

- `type` and `scope` are **lowercase**.
- The **subject must be printable ASCII (English)** — Chinese subjects are rejected.
- The **body may be any language and any line length** — write freely there.

| type | purpose | version bump |
| ---- | ------- | ------------ |
| `feat` | new feature | minor |
| `fix` | bug fix | patch |
| `perf` | performance | patch |
| `refactor` | restructure, no behavior change | — |
| `docs` | documentation | — |
| `test`, `build`, `ci`, `chore`, `style` | misc | — |
| any + `!` or a `BREAKING CHANGE:` footer | breaking change | see [release.md](doc/operation/release.md) |

Suggested scopes: `core`, `ui`, `net`, `app`, `agent`, `lsp`, `gateway`, `mcp`, …

**Only the PR title must comply** (CI checks it): after squash-merge it becomes the
commit on `master`, and release-please parses it to build the CHANGELOG and version.
Intermediate commits inside your branch are free-form.

## Pull request flow

1. Branch off `master` and do **one thing** (keep PRs small and single-purpose).
2. Make sure `just ci` is green locally before pushing — it runs the same checks CI
   does. Then open a PR and fill in the template.
3. CI runs checks, auto-labels the PR by touched paths, and validates the title.
4. After approval, squash-merge. For architectural changes, also update `doc/design/`
   and record the decision in `doc/decisions/architecture-decisions.md`.

## Code conventions

- **Language**: everything except prose docs under `doc/` is written in English —
  code, comments, config, templates, CI (see [AGENTS.md](AGENTS.md) §14).
- Workspace lints (`Cargo.toml`): `unsafe_code` forbidden; clippy `pedantic`/`nursery`/
  `unwrap_used`/`expect_used` are warnings (CI denies warnings).
- UI colors come only from semantic tokens in `crates/ominiforge-ui/src/theme.rs`
  (enforced by `just design-lint`).
- One topic lives in exactly one doc; reference it elsewhere, don't restate
  (see [doc/README.md](doc/README.md)).

## Labels

Auto-applied by path: `core`, `ui`, `net`, `app`, `docs`, `ci`.
Applied manually: `bug`, `enhancement`, `documentation`, `good first issue`, `help wanted`.

## Where to ask

Usage questions and design discussions →
[Discussions](https://github.com/OminiForge/ominiforge/discussions).
Issues are for bugs and feature requests only.
