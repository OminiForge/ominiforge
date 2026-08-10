<!--
The PR title must follow Conventional Commits (see CONTRIBUTING.md) — it becomes the
squash-merge commit message and is parsed by release-please for the CHANGELOG.
Example: feat(ui): add chat panel theme tokens
-->

## Motivation

<!-- What problem does this solve? Link issues: Closes #123 -->

## What changed

<!-- What you did and the key tradeoffs. For architectural changes, also update
     doc/ and record the decision in doc/decisions/architecture-decisions.md. -->

## Verification

- [ ] `just ci` passes locally (fmt / check / clippy / test / audit / deny / machete / design-lint / nix-check)
- [ ] New behavior has corresponding tests
- [ ] `doc/` updated if architecture / protocol / config changed

## Breaking changes

- [ ] No breaking changes
- [ ] Breaking changes (migration path described above and in CHANGELOG)
