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

# 设计铁律的机器强制（doc/gpui-design.md §2）：theme.rs 是唯一允许出现字面色值
# 的文件；ui crate 其余任何文件出现 rgb()/rgba()/hsla() 字面构造即失败。这逼着
# 「用到颜色」时回 theme.rs 加语义 token，而不是随手写魔法值。
design-lint:
    ! grep -rnE '\b(rgb|rgba|hsla)\s*\(' crates/ominiforge-ui/src --include='*.rs' | grep -v 'src/theme.rs'

nix-check:
    nix flake check

ci: fmt-check check clippy test audit deny machete design-lint nix-check
