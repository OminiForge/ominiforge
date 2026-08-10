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
    ! grep -rnE '\b(rgb|rgba|hsla)\s*\(' crates/ominiforge-ui/src --include='*.rs' | grep -v 'src/theme.rs'

nix-check:
    nix flake check

ci: fmt-check check clippy test audit deny machete design-lint nix-check
# Preview the documentation site locally (mdbook).
doc:
    mdbook serve doc --open
# Build the static documentation site into doc/book/.
doc-build:
    mdbook build doc
# Build the FULL multi-version doc site (every release tag + dev) into doc/site/.
doc-site:
    doc/build-all-versions.sh
