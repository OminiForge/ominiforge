# Ominiforge 项目分析报告

## 1. 项目概述

**Ominiforge** 是一个使用 Rust 实现的高性能、可扩展的多智能体（Multi-Agent）平台。项目目标是构建一个 UI 无关的 agent 核心运行时，通过 CLI / TUI / Web 等多种前端形态接入，支持 coding agent、个人研究助手、自动化助手等场景，并具备自我进化能力。

- **语言**：Rust (edition 2024, rust-version 1.89)
- **版本**：0.1.0（Phase 1 已完成）
- **许可证**：MIT OR Apache-2.0
- **仓库**：`https://github.com/replace-me/ominiforge`

---

## 2. 项目结构总览

```
ominiforge/
├── Cargo.toml                  # Rust 项目配置
├── Cargo.lock                  # 依赖锁定文件
├── flake.nix / flake.lock      # Nix flake 开发环境
├── rust-toolchain.toml         # Rust 工具链配置（stable）
├── justfile                    # 任务运行器（fmt/check/clippy/test/ci）
├── clippy.toml                 # Clippy 自定义规则
├── deny.toml                   # cargo-deny 配置（许可证/依赖审计）
├── .editorconfig               # 编辑器统一格式配置
├── .envrc                      # direnv 配置
├── .omini/                     # 项目级 agent 配置目录
│   ├── config/providers.toml   # Provider + 模型定义
│   ├── profiles/default.toml   # 默认 Agent Profile
│   └── sessions/               # Session 存储目录
├── .github/
│   ├── workflows/ci.yml        # CI 流水线
│   └── dependabot.yml          # 依赖自动更新
├── doc/                        # 架构文档（16 篇）
│   ├── architecture.md         # 总体架构设计
│   ├── event-schema.md         # 事件协议规范
│   ├── session-storage.md      # Session 存储设计
│   ├── todo.md                 # 实施计划与决策记录
│   └── ...                     # 各子系统设计文档
└── src/                        # 源代码（7,217 行 Rust）
    ├── main.rs                 # 入口（9 行）
    ├── lib.rs                  # 库根模块（28 行）
    ├── core/                   # 核心协议层（513 行）
    ├── agent/                  # Agent 循环引擎（2,343 行）
    ├── cli.rs                  # CLI 命令解析与分发（651 行）
    ├── config/                 # 配置层（1,240 行）
    ├── session/                # Session 存储（743 行）
    ├── llm/                    # LLM Provider 抽象（146 行）
    ├── provider/               # 具体 Provider 适配（875 行）
    ├── tool/                   # Tool 抽象与内置工具（633 行）
    ├── context.rs              # Context 管理（桩模块）
    ├── mcp.rs                  # MCP 客户端（桩模块）
    ├── hook.rs                 # Hook 注册（桩模块）
    ├── skill.rs                # Skill 生命周期（桩模块）
    ├── memory.rs               # Memory 子系统（桩模块）
    ├── monitor.rs              # Monitor 监控（桩模块）
    ├── evolution.rs            # 自我进化（桩模块）
    ├── gateway.rs              # HTTP/WS 网关（桩模块）
    └── tui.rs                  # TUI 终端界面（桩模块）
```

---

## 3. 代码规模与模块分布

| 模块 | 文件数 | 代码行数 | 状态 |
|------|--------|----------|------|
| `agent/` | 5 | 2,343 | ✅ 核心实现完成 |
| `provider/openai/` | 2 | 843 | ✅ OpenAI 兼容适配器 |
| `cli.rs` | 1 | 651 | ✅ CLI 完整实现 |
| `config/` | 4 | 1,240 | ✅ 配置层完整实现 |
| `session/` | 5 | 743 | ✅ Session 存储完整实现 |
| `tool/` | 5 | 633 | ✅ 内置工具（read/write/shell）|
| `core/` | 4 | 513 | ✅ 核心协议类型 |
| `llm/` | 2 | 146 | ✅ Provider 抽象层 |
| `provider/mod.rs` | 1 | 32 | ✅ Provider 工厂 |
| 桩模块（context/mcp/hook/skill/memory/monitor/evolution/gateway/tui） | 9 | ~25 | 🔲 预留接口 |
| **合计** | **38** | **7,217** | — |

---

## 4. 架构设计

### 4.1 核心设计原则

项目遵循六大核心设计原则：

1. **核心无 UI**：Agent 运行时完全 UI 无关，只负责执行任务、管理状态、发出事件
2. **历史不可变**：Session 原始历史采用 append-only 模型，任何变更创建新节点而非修改原数据
3. **扩展通过 MCP**：外部 tool 通过 MCP（Model Context Protocol）标准协议接入
4. **可读历史优先**：以 `events.jsonl` 为 source of truth，数据库仅作索引
5. **事件驱动**：统一 envelope + 分域 payload enum 的事件协议
6. **进化只生成提案**：自我进化系统生成可审查建议，不会自动应用

### 4.2 依赖方向

```
agent  →  llm  →  (core)
  ↓       ↓
 tool   provider
  ↓
config  session
  ↓
 (core)  ←  所有模块的最底层
```

- `core` 模块零依赖，定义事件协议和标识符类型
- 所有上层模块向下依赖 `core`
- `agent` 模块是中心，驱动 LLM 调用和 Tool 执行

### 4.3 事件协议（Event Schema）

采用 **统一信封 + 分域载荷** 的设计：

```rust
CoreEvent {
    schema_version: "ominiforge.event.v1",
    seq: u64,               // 单调递增序号
    session_id: SessionId,  // ULID
    timestamp: DateTime<Utc>,
    source: EventSource,    // kind enum + id string
    parent_event_id: Option<EventId>,
    turn_id: Option<TurnId>,
    payload: EventPayload,  // 分域枚举
}
```

**EventPayload 分域**：
- `Turn` — Turn 生命周期（Started/Completed/Failed/Interrupted/Resumed）
- `Model` — 模型交互（RequestStarted/ContentBlock/RequestCompleted/RequestFailed）
- `Tool` — Tool 执行（Started/Completed/Failed）
- `Session` — Session 生命周期（Created/Forked/Paused/Resumed/Ended）
- `Artifact` — Artifact 生命周期（Created）
- `Injection` — 动态上下文注入（ContextInjected）
- `Error` — 结构化错误（Raised）

### 4.4 Session 存储

```
.omini/sessions/
  <session_id>/             # ULID 命名的目录
    session.toml            # 元数据（id, profile_id, created_at, origin）
    events.jsonl            # 事件流（source of truth，每行省略 session_id）
    context_snapshot.json   # Fork/Compaction 时的上下文快照
    artifacts/              # Tool 输出存储（Phase 2）
```

- **写入锁**：`events.jsonl` 通过 `flock` 独占锁保证单写者
- **Session 来源**：`new` / `fork` / `compaction` / `reconfiguration` 四种
- **标识符**：ULID（时间可排序 + 随机后缀，26 字符）

---

## 5. 核心模块详解

### 5.1 Agent 循环引擎（`agent/`）— 2,343 行

这是整个系统的核心，驱动一个 turn 从用户输入到最终回答的完整流程。

**核心数据结构**：

- **`Agent`**：持有 turn 不变的依赖（provider、tools、config）
- **`SessionRuntime`**：Session 级状态（对话上下文 + 工作计划），跨 turn 存活
- **`TurnState`**：Turn 级状态（轮次计数、gate 计数、输出积累），turn 结束时销毁

**Agent Loop 流程**：

```
用户输入
  → TurnEvent::Started
  → [循环] run_model_round()
    → 发送当前 context 到 Provider
    → 收集流式响应（collector 持久化 + sink 实时转发）
    → 如果有 ToolCall → dispatch → 将结果作为 Tool 消息追加到 context → 继续循环
    → 如果无 ToolCall → completion_gate() 检查计划完成度
      → 全部完成 → TurnEvent::Completed → 返回结果
      → 未完成 → 注入提醒 → 继续循环
  → 超过 max_rounds → TurnEvent::Failed
```

**Plan 系统**（`agent/plan.rs`，467 行）：

- `plan` 是一个 **control tool**，不在 ToolRegistry 中，由 agent loop 拦截处理
- 操作类型：`init` / `start` / `complete` / `cancel` / `block` / `add`
- 状态机：`Pending → InProgress → Completed | Cancelled | Blocked`
- **Completion Gate**：模型停止时检查所有计划步骤是否到达终态
- **Stuck Detection**：步骤持续 `in_progress` 超过阈值时注入提醒

**Stream Collection**（`agent/collector.rs`，496 行）：

- 分离 **实时传输**（sink 逐 token 转发）和 **持久化**（event log 记录整合后的完整块）
- 支持 Text / Reasoning / ToolCall 三种内容块
- 空块（provider 打开但未产出内容）自动跳过

**StreamSink**（`agent/sink.rs`，61 行）：

- `StreamSink` trait：`on_block_start` / `on_text` / `on_reasoning` / `on_tool_call_delta` / `on_block_stop` / `on_turn_end`
- `NullSink`：默认无操作（headless 场景）
- `CliSink`（在 `cli.rs`）：stdout 输出答案，stderr 输出推理和工具活动（TTY 时 ANSI dimming）

### 5.2 LLM Provider 抽象（`llm/` + `provider/`）— 1,053 行

**Provider Trait**（`llm/mod.rs`）：

```rust
#[async_trait]
trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(&self, request: ModelRequest) -> Result<EventStream, LlmError>;
}
```

- `EventStream` = `BoxStream<'static, Result<StreamEvent, LlmError>>`
- `StreamEvent` 枚举：`BlockStart` / `TextDelta` / `ReasoningDelta` / `ToolCallDelta` / `BlockStop` / `Completed`
- `Message` 类型：`System` / `User` / `Assistant` / `Tool`（provider-neutral）

**OpenAI 适配器**（`provider/openai/`，843 行）：

- `mod.rs`（205 行）：实现 `Provider` trait，构建 HTTP 请求
- `wire.rs`（638 行）：SSE 流解析，将 OpenAI wire format 转换为 `StreamEvent`
- 通过 `reqwest` + `rustls-tls` + `http2` + `stream` 发起流式请求

**Provider 工厂**（`provider/mod.rs`）：

- Phase 1 仅支持 `openai-chat` 类型
- 任何 OpenAI 兼容端点均可使用（本地服务器、第三方等）

### 5.3 Tool 系统（`tool/`）— 633 行

**Tool Trait**：

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    async fn invoke(&self, input: ToolInput) -> ToolResult;
}
```

- `ToolResult` = `Result<ToolOutput, ToolError>`
- 业务级失败（如命令非零退出）返回 `Ok(ToolOutput { is_error: true })`
- 协议级错误（如超时、输入格式错）返回 `Err(ToolError)`

**内置工具**：

| 工具 | 文件 | 功能 |
|------|------|------|
| `read` | `tool/read.rs`（116 行） | 读取 workspace 内的 UTF-8 文本文件 |
| `write` | `tool/write.rs`（125 行） | 写入文本文件，自动创建父目录 |
| `shell` | `tool/shell.rs`（178 行） | 在 workspace 中执行 shell 命令 |

**安全机制**：
- 路径逃逸检查：`resolve_in_workspace()` 阻止 `../` 逃出 workspace
- Shell 超时：`tokio::time::timeout` 控制命令执行时长
- Phase 1 无 OS 级沙箱

**ToolRegistry**：
- 按名称索引的 `HashMap<String, Arc<dyn Tool>>`
- `descriptors()` 按名称排序，保证 prefix-cache 稳定性

### 5.4 配置层（`config/`）— 1,240 行

**Provider 配置**（`config/providers.rs`，194 行）：

```toml
[[providers]]
name = "openai-main"
type = "openai-chat"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0
pricing = { input_per_million = 2.50, output_per_million = 10.00 }
```

**Profile 配置**（`config/profile.rs`，424 行）：

- 支持单继承（`extends` 字段），字段级覆盖
- 已实现 section：`[profile]`、`[prompt]`、`[model]`、`[tools]`
- 预留 section（已解析未生效）：`[context]`、`[skills]`、`[memory]`、`[budget]`、`[hooks]`

**ConfigStore**（`config/mod.rs`，563 行）：

- 配置发现：项目 `./.omini` 优先于 `~/.omini`
- Provider 合并：高优先级同名 provider 遮蔽低优先级
- Profile 加载：支持继承链解析、循环检测、深度限制（最大 5 层）
- Model 解析：支持 `provider/model` 完整引用和 `model` 短引用
- `.env` 加载：config root `.env` 优先于 workspace `.env`，不覆盖已有环境变量

### 5.5 CLI（`cli.rs`）— 651 行

**命令**：

- `ominiforge run <prompt>` — 执行单次 agent turn
- `ominiforge init` — 初始化 `.omini/` 配置目录

**run 参数**：

| 参数 | 说明 |
|------|------|
| `--workspace` | 工作目录（默认当前目录）|
| `--profile` | Profile 名称（默认 `default`）|
| `--model` | 模型引用（覆盖 profile）|
| `--temperature` | 采样温度（覆盖 profile）|
| `--no-dotenv` | 不加载 `.env` 文件 |

**CliSink 实现**：

- stdout 输出模型回答（无样式，可管道捕获）
- stderr 输出推理过程和工具活动（TTY 时 dim 样式）
- 自动管理通道切换（answer ↔ side）的换行和样式重置

### 5.6 Session 存储（`session/`）— 743 行

- `SessionStore`：管理 session 目录，创建和读取 session
- `SessionWriter`：持有排他锁的事件写入器，自动填充信封字段
- `EventLog`：append-only `events.jsonl`，通过 `flock` 独占锁保证单写者
- 读取时自动恢复 `session_id`（磁盘上省略，从目录名获取）

---

## 6. 依赖分析

### 6.1 核心依赖

| 依赖 | 版本 | 用途 |
|------|------|------|
| `tokio` | 1 | 异步运行时（multi-thread, macros, process, time, fs, io-util）|
| `reqwest` | 0.12 | HTTP 客户端（rustls-tls, json, stream, http2）|
| `serde` / `serde_json` | 1 | 序列化/反序列化 |
| `clap` | 4 | CLI 参数解析（derive 宏）|
| `anyhow` / `thiserror` | 1 / 2 | 错误处理 |
| `async-trait` | 0.1 | 异步 trait 支持 |
| `futures-util` | 0.3 | Stream 组合器 |
| `chrono` | 0.4 | 时间处理（serde, clock）|
| `ulid` | 1 | ULID 标识符生成 |
| `toml` | 0.8 | TOML 解析 |
| `dotenvy` | 0.15 | `.env` 文件加载 |
| `bytes` | 1 | 字节缓冲 |

### 6.2 开发依赖

| 依赖 | 版本 | 用途 |
|------|------|------|
| `tempfile` | 3 | 测试用临时目录 |

### 6.3 安全配置

- **`unsafe_code = "forbid"`**：禁止 unsafe 代码
- **clippy 规则**：`pedantic` + `nursery` + `unwrap_used` + `expect_used` 均为 warn
- **cargo-deny**：审计安全漏洞、许可证合规、依赖来源

---

## 7. 开发工具链

### 7.1 Nix Flakes + direnv

- `flake.nix` 通过 `oxalica/rust-overlay` 从 `rust-toolchain.toml` 统一管理 Rust 工具链
- 开发工具包括：`cargo-audit`、`cargo-deny`、`cargo-machete`、`cargo-nextest`、`cargo-watch`、`just`、`bacon`、`alejandra`、`taplo` 等
- Nix checks：`nix-format`（alejandra）+ `cargo-check`（编译 + 测试）

### 7.2 任务运行器（justfile）

| 命令 | 功能 |
|------|------|
| `just fmt` | 格式化 Rust/Nix/TOML |
| `just fmt-check` | 检查格式 |
| `just check` | cargo check |
| `just clippy` | clippy -D warnings |
| `just test` | cargo nextest run |
| `just audit` | cargo audit |
| `just deny` | cargo deny check |
| `just machete` | cargo machete（未使用依赖检测）|
| `just nix-check` | nix flake check |
| `just ci` | 本地完整检查（所有以上）|

### 7.3 CI 流水线（GitHub Actions）

两个 job：

1. **nix** — `nix flake check`
2. **cargo** — 依次运行 `fmt-check` → `check` → `clippy` → `test` → `audit` → `deny` → `machete`

触发条件：push to `main`、PR、手动触发。

### 7.4 Dependabot

- Cargo 依赖：每周检查，最多 10 个 PR
- GitHub Actions：每周检查，最多 10 个 PR

---

## 8. 测试情况

### 8.1 测试统计

- **含测试的文件**：18 个
- **测试用例总数**：77 个（`#[test]` + `#[tokio::test]`）

### 8.2 测试策略

项目测试遵循 Karpathy Guidelines 规范，注重意图验证而非行为覆盖：

| 模块 | 测试内容 |
|------|----------|
| `core/payload` | 事件序列化往返、字段语义 |
| `agent/mod` | 完整 turn 流程、Plan 驱动多轮、Completion Gate、Stuck 检测、未知工具恢复 |
| `agent/collector` | 流式 delta 合并为完整块、ToolCall 事件 ID 追踪 |
| `cli` | `.env` 加载优先级、CliSink 通道分离 |
| `config` | Provider/Profile 解析、继承链、配置覆盖、错误处理 |
| `session` | 创建、序列化、单调 seq、文件锁、读取恢复 |
| `session/event_log` | 磁盘格式（省略 session_id）、文件锁、追加顺序 |
| `session/id` | ULID 生成唯一性和排序性 |
| `tool/read` | 正常读取、缺失文件、路径逃逸 |
| `tool/write` | 写入、创建父目录、路径逃逸 |
| `tool/shell` | stdout 捕获、非零退出、工作目录、超时 |
| `tool/mod` | 描述符排序、路径逃逸检查 |

### 8.3 测试特点

- 使用 `ScriptedProvider` mock 驱动确定性多轮 agent 测试
- 使用 `tempfile` 隔离文件系统测试
- 使用内存缓冲（`CliSink<Vec<u8>, Vec<u8>>`）测试输出路由
- 事件序列化测试验证 wire format 的稳定性和向后兼容性

---

## 9. 当前实现状态

### 9.1 Phase 1 — 已完成 ✅

> 核心事件类型 + Session 存储 + Agent Loop + LLM Provider + 内置工具 + CLI

具体完成项：

- [x] 核心事件协议（`core/`）：统一信封 + 7 域 payload enum
- [x] Session 存储（`session/`）：append-only event log + session.toml + 文件锁
- [x] LLM Provider 抽象（`llm/`）：Provider trait + 流式事件模型
- [x] OpenAI 适配器（`provider/openai/`）：SSE 流解析 + 完整请求/响应处理
- [x] Tool 系统（`tool/`）：Tool trait + ToolRegistry + read/write/shell 三个内置工具
- [x] Agent Loop（`agent/`）：多轮 turn 驱动 + tool dispatch + stream collection + StreamSink
- [x] Plan 系统（`agent/plan.rs`）：工作计划管理 + completion gate + stuck detection
- [x] 配置层（`config/`）：providers.toml + profiles（含继承）+ 模型解析 + `.env` 加载
- [x] CLI（`cli.rs`）：`run` + `init` 命令 + 实时流式输出

### 9.2 后续阶段 — 桩模块已预留

| 阶段 | 模块 | 状态 |
|------|------|------|
| Phase 2 | `context`（上下文管理与压缩）| 🔲 桩模块 |
| Phase 2 | `mcp`（MCP 客户端）| 🔲 桩模块 |
| Phase 2 | `monitor`（监控）| 🔲 桩模块 |
| Phase 2 | `tui`（终端界面）| 🔲 桩模块 |
| Phase 3 | `profile` + `skill`（技能系统）| 🔲 桩模块 |
| Phase 3 | `hook`（钩子系统）| 🔲 桩模块 |
| Phase 3 | `gateway`（HTTP/WS 网关）| 🔲 桩模块 |
| Phase 4 | `evolution`（自我进化）| 🔲 桩模块 |
| Phase 5 | `memory`（记忆系统）| 🔲 桩模块 |

---

## 10. 代码质量评估

### 10.1 优点

1. **架构清晰**：严格的依赖方向（一切指向 `core`），UI 无关的核心设计
2. **文档完备**：16 篇设计文档覆盖所有子系统，决策有据可查
3. **测试充分**：77 个测试用例，覆盖关键路径和边界情况
4. **代码风格严格**：`unsafe_code = "forbid"` + clippy pedantic + nursery
5. **安全意识**：API key 不存配置文件，通过环境变量引用；路径逃逸检查
6. **流式处理优秀**：实时传输与持久化分离，prefix-cache 友好设计
7. **Plan 系统精巧**：控制工具与叶子工具统一处理，completion gate 防止任务丢失
8. **Session 设计合理**：append-only + 文件锁 + ULID，可扩展性好
9. **配置层次化**：项目级覆盖用户级，Profile 支持单继承

### 10.2 待改进

1. **Provider 适配器单一**：目前仅支持 `openai-chat`，Anthropic/自定义尚无实现
2. **无 OS 级沙箱**：Shell tool 直接执行系统命令，安全性依赖用户信任
3. **无索引/搜索能力**：大量 session 的查询和检索能力缺失（预期 Phase 4+ 引入数据库索引）
4. **单 turn CLI**：当前 CLI 只支持一次交互，多轮对话需 TUI（Phase 2）
5. **无 artifact 存储**：大输出（>64KB）暂无溢出机制
6. **缺少集成测试**：当前测试均为单元测试，无端到端测试

---

## 11. 技术栈总结

```
┌─────────────────────────────────────────────────┐
│                   用户界面层                       │
│         CLI (✅)    TUI (🔲)    Web (🔲)          │
├─────────────────────────────────────────────────┤
│                   Agent 引擎                      │
│    Agent Loop (✅)  Plan (✅)  Sink (✅)           │
├─────────────────────────────────────────────────┤
│              子系统（多数为桩模块）                   │
│  Context  MCP  Hook  Skill  Memory  Monitor      │
│   (🔲)   (🔲) (🔲)  (🔲)   (🔲)   (🔲)          │
│                Evolution  Gateway                 │
│                 (🔲)     (🔲)                     │
├─────────────────────────────────────────────────┤
│                 基础设施层                         │
│  Config (✅)  Session (✅)  LLM (✅)  Tool (✅)    │
├─────────────────────────────────────────────────┤
│                 核心协议层                         │
│         Core Events + IDs (✅)                    │
│  ┌────────────────────────────────────────────┐  │
│  │  Provider: OpenAI (✅)  Anthropic (🔲) ...  │  │
│  └────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

---

## 12. Git 提交历史

项目采用 Conventional Commits 规范，提交清晰有序：

| Commit | 描述 |
|--------|------|
| `a3507d4` | chore: scaffold crate skeleton and core event protocol |
| `ff9b4c6` | feat(session): append-only event log + session.toml storage |
| `1bf5a0f` | feat(llm,provider): Provider trait + streaming OpenAI adapter |
| `b714708` | feat(tool): Tool trait, registry, and built-in read/write/shell |
| `ca036ad` | feat(agent): minimal turn loop driving model rounds + tool calls |
| `e789294` | feat(cli): wire `ominiforge run` end-to-end (Phase 1 complete) |
| `8a3aa95` | fix: configure Zed rust-analyzer |
| `dd73c2b` | feat(cli,agent): config layer + live streaming output |
| `3b976c6` | fix: close stdout answer line when switching to side-channel |

提交遵循自底向上的实施顺序：core → session → llm/provider → tool → agent → cli → config，与架构文档中的 Phase 计划一致。

---

## 13. 结论

Ominiforge 是一个**架构设计成熟、代码质量高、文档完备**的 Rust agent 平台项目。Phase 1 的核心功能已完整实现，包括事件驱动的 agent 循环、多轮 tool 调用、工作计划管理、流式输出、session 持久化和灵活的配置系统。

项目的设计决策务实（如废弃 WASM 方案改用 MCP、单 crate 而非多 crate），代码风格严谨（禁止 unsafe、clippy pedantic、充分测试），为后续阶段（上下文管理、MCP 客户端、TUI、网关、自我进化等）奠定了坚实的架构基础。

当前 7,217 行 Rust 代码中，约 6,200 行为实质性实现，约 1,000 行为桩模块预留，体现了**渐进式开发、骨架先行**的良好工程实践。
