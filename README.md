# ominiforge

a multi agent app

## 开发环境

本项目使用 Nix flake 管理 Rust toolchain、开发工具和验证工具。

首次进入仓库：

```sh
direnv allow
```

或手动进入环境：

```sh
nix develop
```

## Rust toolchain

`rust-toolchain.toml` 是 Rust channel/components 的单一来源。`flake.nix` 通过 oxalica rust-overlay 读取它：

```nix
rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
```

默认组件：

- rust-src
- rust-analyzer
- rustfmt
- clippy

## 常用命令

```sh
just fmt        # 格式化 Rust/Nix/TOML
just fmt-check  # 检查格式
just check      # cargo check
just clippy     # clippy -D warnings
just test       # cargo nextest run
just audit      # cargo audit
just deny       # cargo deny check
just machete    # cargo machete
just nix-check  # nix flake check
just ci         # 本地完整检查
```

## Zed

Zed 使用项目内 wrapper 启动 flake 环境里的 rust-analyzer：

```text
.zed/rust-analyzer.sh
```

这样 Zed、终端、CI 使用同一套 Rust toolchain 和依赖环境。

## CI

GitHub Actions 位于：

```text
.github/workflows/ci.yml
```

CI 会运行格式检查、cargo check、clippy、nextest、audit、deny、machete 和 nix flake check。

## License

MIT OR Apache-2.0

## 致谢

Ominiforge 的 TUI 设计参考了以下项目，特此感谢：

- [oh-my-pi](https://github.com/can1357/oh-my-pi)（[omp.sh](https://omp.sh)）—— 卡片式工具渲染、状态栏与整体交互风格。
- [Pi](https://github.com/badlogic/pi-mono) by [@mariozechner](https://github.com/mariozechner) —— oh-my-pi 的上游，简洁的 agent 终端界面范式。
