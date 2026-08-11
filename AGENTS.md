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

Every test must encode _why_ the behavior matters. A test that can't fail when
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

## 13. Code is documentation, kept consistent

`doc/` holds framework- and design-level guidance, never implementation detail
(specific interfaces, classes, function signatures — those live in code and
comments). Keeping detail out of docs avoids two drifting sources.

Consistency is a **per-change obligation, not an afterthought**. Whenever a change
touches behavior, structure, workflow, or conventions that any document describes, the
same change must reconcile every affected doc — `doc/`, `README.md`, code comments,
justfile / workflow / config comments — not just the files under `doc/`. Remove three
failure modes when reconciling:

- **Stale** — the doc still describes the pre-change behavior.
- **Duplicated** — the same fact is restated in two places (violates §12); keep one
  source of truth and reference it elsewhere.
- **Over-detailed** — the doc has drifted into a copy of the code; lift it back to
  design intent and let the code carry the specifics.

This is a judgment call, so it is not fully machine-checkable. Contributors and agents
follow it directly while working; an agent workflow provides backstop review to catch
what humans and agents miss.

## 14. English for everything except prose docs

This project collaborates internationally. The rule:

- **English** — code, comments, commit messages, PR/issue templates, config
  files, CI/workflow definitions, changelog, and any text a tool parses or a
  global contributor must read. Comments in code are always English.
- **Either language** — long-form prose documentation under `doc/`, where the
  goal is to be read, not parsed. It currently stays primarily Chinese; new
  doc may add an English version over time.
  When in doubt: if it isn't narrative documentation, write it in English.

---

_Contribution mechanics (branching, commit format, PR flow, releases) are in
[CONTRIBUTING.md](CONTRIBUTING.md). Architecture and design contracts are in
[doc/](doc/README.md)._
