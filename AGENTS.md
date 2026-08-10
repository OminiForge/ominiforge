# Repository Guidelines

These rules govern how work happens in this repo. They apply to both human
contributors and AI agents. **This file is the single source of truth** for
working agreements; `CLAUDE.md` and other agent-specific files point here.

## 1. Think before coding

State assumptions explicitly. If multiple interpretations exist, present them —
don't pick silently. If a simpler approach exists, say so. If something is
unclear, stop and ask. Don't hide confusion; surface tradeoffs.

## 2. Simplicity first

Minimum code that solves the problem. No speculative features, no abstractions
for single-use code, no unrequested configurability, no error handling for
impossible scenarios. If 200 lines could be 50, rewrite it.

## 3. Surgical changes

Touch only what you must. Match existing style even if you'd do it differently.
Don't "improve" adjacent code or refactor what isn't broken. Remove only the
imports/variables/functions your own change orphaned — not pre-existing dead
code. Every changed line should trace to the request.

## 4. Goal-driven execution

Define verifiable success criteria and loop until met. "Add validation" → write
failing tests for invalid inputs, then make them pass. "Fix the bug" → write a
reproducing test, then make it pass.

## 5. Use judgment calls, not plumbing

Use models for classification, drafting, summarization, extraction. Use plain
code for routing, retries, status-code handling, deterministic transforms.

## 6. Surface conflicts, don't average them

If two codebase patterns contradict, pick one (the more recent / more tested),
explain why, and flag the other for cleanup. Don't blend them.

## 7. Read before you write

Before adding code, read the file's exports, its callers, and shared utilities.
If you don't understand why existing code is structured as it is, ask first.

## 8. Tests verify intent, not just behavior

Every test must encode *why* the behavior matters. A test that can't fail when
business logic changes is worthless.

## 9. Checkpoint after significant steps

After each step in a multi-step task: summarize what's done, what's verified,
what's left. If you lose track, stop and restate.

## 10. Match codebase conventions

Conform to existing naming and structure. Disagreement is a separate
conversation — don't fork conventions silently.

## 11. Fail loud

If you can't be sure something worked, say so. "Tests pass" is wrong if you
skipped any. Default to surfacing uncertainty, not hiding it.

## 12. Don't repeat yourself

One source of truth. If something is documented elsewhere, reference it — don't
restate. Favor composition and reuse; check whether a thing (or something
similar) already exists before building it. Low coupling between modules.

## 13. Code is documentation

`doc/` holds framework- and design-level guidance, never implementation detail
(specific interfaces, classes, function signatures — those live in code and
comments). Keeping detail out of docs avoids two drifting sources.

---

*Contribution mechanics (branching, commit format, PR flow, releases) are in
[CONTRIBUTING.md](CONTRIBUTING.md). Architecture and design contracts are in
[doc/](doc/README.md).*
