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

# Regenerate the TS type bindings the frontend consumes from the Rust wire
# types (doc/frontend.md §4, §6). Writes frontend/src/lib/types/*.ts.
ts-export:
    TS_RS_EXPORT_DIR=frontend/src/lib/types cargo test --features ts-export export_bindings

# Drift gate: regenerate, then fail if the committed TS bindings differ from
# what the Rust types now produce (doc/frontend.md §6).
ts-check: ts-export
    git diff --exit-code frontend/src/lib/types

nix-check:
    nix flake check

ci: fmt-check check clippy test audit deny machete ts-check nix-check
