fmt:
    cargo fmt
    alejandra flake.nix
    taplo fmt Cargo.toml rust-toolchain.toml

fmt-check:
    cargo fmt --check
    alejandra --check flake.nix
    taplo fmt --check Cargo.toml rust-toolchain.toml

check:
    cargo check

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo nextest run

audit:
    cargo audit

deny:
    cargo deny check

machete:
    cargo machete

# Enforces the design rule (doc/design/gpui-design.md §2): theme.rs is the only
# file allowed to hold literal color values. Any rgb()/rgba()/hsla() literal in
# the rest of the ui crate fails the build — forcing "need a color" to become a
# semantic token in theme.rs rather than a scattered magic value.
design-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    # Exclude theme.rs itself exactly (by filename, not path substring — the old
    # `grep -v 'src/theme.rs'` would also exclude theme.rsx / sub/theme.rs). Any hit
    # yields non-empty output -> fail.
    hits=$(grep -rnE '\b(rgb|rgba|hsla)\s*\(' \
      $(find crates/ominiforge-ui/src -name '*.rs' -not -name 'theme.rs') || true)
    if [ -n "$hits" ]; then
      echo "design-lint: color literal outside theme.rs:" >&2
      echo "$hits" >&2
      exit 1
    fi

nix-check:
    nix flake check

# Enforces AGENTS.md §14: everything a tool parses or a global contributor must read is
# English. Flags any non-ASCII *letter* (\p{L} outside a-z/A-Z) in code, comments,
# config, and CI — CJK, Cyrillic, Arabic, accented Latin, etc. Punctuation/symbols
# (§, →, —, ×) are allowed: they are not prose. Scope covers Rust sources, config, and
# CI; frontend/ is excluded (it is slated for removal and will go i18n later), and doc/
# prose may be any language per §14.
# Exemption: a line ending in `lint-english: allow` is skipped — for intentional non-ASCII
# *data*, e.g. tests feeding accented or CJK strings to verify Unicode handling. Never use
# it to excuse prose comments.
lint-english:
    #!/usr/bin/env bash
    set -euo pipefail
    # ripgrep, not GNU grep: rg's Unicode classes are locale-independent (grep's \p{L}
    # mis-flags multibyte symbols as letters under a C locale), and rg scans explicitly-listed
    # files (flake.nix, justfile) uniformly with recursed dirs — grep's --include silently
    # drops listed files that match no glob. `[\p{L}--\x{00}-\x{7F}]` = letter AND non-ASCII
    # (char-class difference; rg's default engine needs no PCRE2/look-around).
    hits=$(rg --no-config -n '[\p{L}--\x{00}-\x{7F}]' \
      -g '*.rs' -g '*.toml' -g '*.yml' -g '*.yaml' \
      -g 'justfile' -g 'flake.nix' -g 'flake.lock' -g 'deny.toml' \
      -g 'clippy.toml' -g 'rustfmt.toml' -g 'statix.toml' \
      crates .github justfile flake.nix flake.lock deny.toml clippy.toml rustfmt.toml statix.toml \
      2>/dev/null | rg -v 'lint-english: allow' || true)
    if [ -n "$hits" ]; then
      echo "lint-english: non-English letter found (AGENTS.md §14 requires English):" >&2
      echo "$hits" >&2
      exit 1
    fi

# Nix-code static lints, mirroring the flake checks of the same name (statix / deadnix /
# alejandra). Seconds-level; the hermetic twin lives in `checks` for CI. Standalone for
# quick iteration; `just ci` runs them via `nix flake check`.
nix-lint:
    statix check flake.nix
    deadnix -f --no-lambda-pattern-names flake.nix
    alejandra --check flake.nix

# Delete local branches whose PR was merged and prune stale remotes. Merged-ness is judged
# by PR state, NOT `git branch --merged`: this repo squash-merges only, so a merged PR's
# branch tip never lands on master and `--merged` always misses it. `gh pr list --author @me`
# covers only your own PRs; branches from others' PRs are left alone (delete by hand).
clean-branches:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "$(git status --porcelain)" ]; then
      echo "clean-branches: working tree is dirty; commit or stash first" >&2
      exit 1
    fi
    current=$(git branch --show-current)
    git checkout -q master
    git pull -q --ff-only
    git fetch -q --prune
    to_delete=$(
      comm -12 \
        <(git for-each-ref --format='%(refname:short)' refs/heads | grep -Fvx master | sort) \
        <(gh pr list --author @me --state merged --json headRefName --jq '.[].headRefName' | sort) \
        | grep -Fvx "$current" || true
    )
    if [ -z "$to_delete" ]; then
      echo "clean-branches: nothing to delete"
    else
      # No `xargs -r` here: -r (--no-run-if-empty) is a GNU extension missing from the BSD
      # xargs on macOS; the [ -z ] guard above already covers the empty case. -F (fixed
      # string) on the greps keeps branch-name dots from being read as regex.
      echo "$to_delete" | xargs git branch -D
    fi
    if [ "$current" != "master" ] && git show-ref --verify --quiet "refs/heads/$current"; then
      git checkout -q "$current"
    fi

# The single full check suite — run this before pushing. Static lints (format, nix-lint,
# toml-format, design-lint, lint-english) and the sandboxed cargo-check are all covered by
# `nix flake check`; the compile-type checks (clippy/test) and supply-chain gates
# (audit/deny/machete) run in the dev shell. Same set CI runs, so a green local `ci` means
# a green CI.
ci: fmt-check clippy test audit deny machete nix-check
# Preview the documentation site locally (mdbook).
doc:
    mdbook serve doc --open
# Build the static documentation site into doc/book/.
doc-build:
    mdbook build doc
# Build the FULL multi-version doc site (every release tag + dev) into doc/site/.
doc-site:
    doc/build-all-versions.sh
