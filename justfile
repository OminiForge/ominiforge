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

nix-check:
    nix flake check

ci: fmt-check check clippy test audit deny machete nix-check
