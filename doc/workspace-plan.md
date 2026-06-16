# Workspace 拆分方案

## 拆分原则

只在满足以下条件时拆为独立 crate：

1. **不同编译目标** — 当前无此需求（WASM 已废弃）
2. **其余情况** — 用 module 分层，不拆 crate

不拆的场景：
- "架构整洁" 不是拆 crate 的理由，module boundary 已够
- Provider 有外部依赖但当前数量少，feature flag 管理
- 编译时间未成瓶颈前不做物理拆分

## Crate 结构

```text
crates/
  ominiforge/                  # library + binary（唯一 crate）
```

初期只有一个 crate。不再有 ominiforge-sdk（WASM 已废弃，MCP server 用各语言标准 SDK）。

## 主 crate 内部 module 布局

```text
src/
├── core/                      # event schema, state machine, core traits
├── session/                   # storage, fork, DAG, context snapshot
├── context/                   # compaction, injection, prefix cache
├── llm/                       # model trait, provider trait, shared types
├── provider/                  # 具体 provider 实现
│   ├── openai/
│   └── xiaomi/
├── tool/                      # built-in tool 实现
│   ├── shell.rs
│   ├── read.rs
│   ├── write.rs
│   ├── search.rs
│   └── mod.rs                 # ToolRegistry, Tool trait
├── mcp/                       # MCP client, adapter, server lifecycle
├── hook/                      # hook registry, built-in hooks, shell hook runner
├── skill/                     # skill lifecycle, discovery
├── memory/                    # memory interface + stores
├── monitor/                   # trace, usage, cost (EventBus subscriber)
├── evolution/                 # session analysis, proposal generation
├── agent/                     # agent loop, orchestration
├── gateway/                   # HTTP/WS server (feature "gateway", 默认开启)
├── cli/                       # 命令解析、输出格式化、子命令
├── tui/                       # 终端 UI 渲染、面板、按键
└── main.rs                    # 入口：run / tui / serve 子命令分发
```

### 与旧方案对比

| 旧 | 新 | 说明 |
|---|---|---|
| `extension/` (wasmtime host) | `tool/` + `mcp/` + `hook/` | 拆为独立关注点 |
| `sandbox/` | 移除 | 监控在 monitor/，shell 沙箱在 tool/shell.rs |
| ominiforge-sdk crate | 移除 | MCP server 用标准 SDK |

## 依赖方向

```text
main.rs → cli/ / tui/ / gateway/
cli/ tui/ gateway/ → agent/ + session/ + monitor/
agent/ → core/ + llm/ + tool/ + mcp/ + hook/ + context/ + memory/ + skill/
tool/ → core/ (event types)
mcp/ → core/ + tool/ (Tool trait, ToolRegistry)
hook/ → core/ (event types)
monitor/ → core/ (EventBus subscriber, 不侵入 core)
provider/ → llm/ (实现 model trait)
session/ → core/ (event types)
context/ → session/ + core/
evolution/ → session/ + monitor/ + skill/
```

core 不依赖任何上层 module。

## Feature flags

| Feature   | 控制范围              | 默认 |
|-----------|----------------------|------|
| `gateway` | gateway/ module 编译 | on   |
| `tui`     | tui/ 相关依赖(ratatui 等) | on   |
| `provider-openai`  | OpenAI provider | on   |
| `provider-xiaomi`  | Xiaomi MiMo provider | on   |

## 何时再拆

满足任一条件时考虑物理拆分：

- 增量编译时间超过可接受阈值
- 某 module 需被外部项目独立引用（如 event schema 被其他工具消费）
- Provider 数量增多且各自依赖树庞大

Module boundary 已画好，物理拆分是机械操作。
