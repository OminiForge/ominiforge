<!-- status: current -->
<!-- owner: @OminiForge -->

# MSRV 政策

**MSRV（Minimum Supported Rust Version）** = 能编译本项目所需的最低 Rust 版本。
它在 `Cargo.toml` 的 `workspace.package.rust-version` 声明（当前 `1.89`）。

## 与 toolchain 的关系（区分两个概念）

| 概念 | 文件 | 含义 | 本项目 |
| ---- | ---- | ---- | ------ |
| **toolchain** | `rust-toolchain.toml` | 开发/CI **实际用**的编译器 | 固定版本（pinned stable） |
| **MSRV** | `Cargo.toml` | 用户/下游**至少要**有的版本 | `1.89` |

`rust-toolchain.toml` 会固定到一个具体 stable 版本，`flake.nix` 通过 rust-overlay 读取它，
保证「开发者、终端、CI 用同一套工具链」。MSRV 是下限承诺，toolchain 是实际使用版本，两者独立演进。

## 政策：用 pinned stable，不用 nightly

参考 Zed：`rust-toolchain.toml` 固定到具体 stable 版本（如 `channel = "1.97.1"`），**不用 nightly**。

理由：

- **nightly 不稳定**：feature 会变、会破，CI 随机失败，协作摩擦大。
- **pinned stable 可复现**：所有人（含 CI、Nix）锁定同一版本，避免「在我机器上能编」。
- **edition 2024 已在 stable**（1.85+），无需 nightly feature。

因此：**不要用 nightly**。除非确需某个 nightly-only feature，此时应先在
架构决策记录（`decisions/`）记录此类决策，而不是悄悄切到 nightly。

## 何时 bump

- **toolchain bump**：需要新版编译器特性/修复时，改 `rust-toolchain.toml` 的 `channel`，CI 通过后合并。
- **MSRV bump**：当代码用到比当前 MSRV 更新的 stable API 时才提升，并：
  1. 更新 `Cargo.toml` 的 `rust-version`
  2. 在 CHANGELOG 记为 breaking change

MSRV bump 对下游是破坏性变更，不随意做；跟随 stable 节奏即可，不追求「越新越好」。
