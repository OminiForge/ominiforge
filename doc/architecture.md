# Ominiforge 架构设计

## 1. 项目目标

Ominiforge 目标是构建一个使用 Rust 实现的高性能、强执行能力、高扩展性的 agent 平台。系统应能够通过扩展成为 coding agent、个人研究助手、自动化助手，并逐步接入日常生活、软件开发、知识管理、外部应用协作等场景。

系统需要支持多种使用入口，包括 CLI、TUI、Web 页面，并为后续桌面应用、移动应用、第三方应用接入预留架构空间。不同入口不应各自实现 agent 逻辑，而应共享同一个核心运行时、同一套 session 管理、同一套事件协议和同一套监控记录。

系统还需要具备自我进化能力。自我进化不是指系统未经确认自动修改自身，而是指系统能够基于 session 历史、失败记录、使用频率、成本数据和 tool/skill 运行结果，生成可审查的优化建议、skill 草案、配置变更建议或代码 patch。所有影响系统行为的进化结果都应由用户批准后再应用。

## 2. 核心设计原则

### 2.1 核心无 UI

核心 agent 运行时不应依赖 CLI、TUI、Web 或桌面环境。核心只负责执行任务、管理状态、发出事件。UI 层只负责收集用户输入、渲染事件流、展示状态和提交控制指令。

这样设计是因为项目需要支持多个前端形态。如果 agent 逻辑与某个 UI 绑定，后续接入 Web、移动端、桌面端或第三方应用时会重复实现逻辑，并导致行为不一致。

### 2.2 历史不可变

Session 的原始历史应采用 append-only 模型保存。任何压缩、fork、修正、总结或视图变化，都不应直接改写原始 session 历史，而应创建新的 session 节点、snapshot 或 context view。

这样设计是因为 agent 系统需要支持回放、审计、失败分析、任意位置 fork、自我进化分析和高质量调试。如果历史被原地修改，后续很难判断真实执行过程，也很难从某个旧状态恢复或比较不同分支。

### 2.3 扩展通过 MCP

外部 tool 通过 MCP（Model Context Protocol）标准协议接入。MCP server 是普通进程，拥有完整 OS 能力，任何语言均可实现。系统内置 tool 直接用 Rust 实现，无额外协议开销。

这样设计是因为 agent tool 天然需要完整 OS 能力（shell、LSP、文件操作），WASM 沙箱限制过大无法满足。MCP 是行业标准，生态成熟，无需自定义扩展协议。

### 2.4 可读历史优先，数据库作为索引

完整 session 历史不应只存在 SQLite 等数据库中。数据库适合索引、查询和缓存，但不适合作为唯一历史来源。系统应保存机器可读的 event log（events.jsonl），人类可读展示由前端（TUI/Web/App）从 event log 解析渲染。索引数据库可从 event log 重建。

这样设计是因为把所有历史放入单个数据库会增加迁移、损坏恢复和长期扩展成本。Event log 作为 source of truth，索引数据库随时可重建。

### 2.5 事件驱动

核心执行过程应通过事件流表达，例如文本增量、思考增量、tool call、tool result、usage、artifact 创建、状态变更和错误。不同 UI、gateway、监控系统和外部协议适配层都消费同一套事件。

事件协议采用统一 envelope + 分域 payload enum 设计。所有事件共享 schema_version、seq、session_id、timestamp、source 等信封字段，payload 按 Turn/Model/Tool/Session/Artifact/Error 分域。详见 [`doc/event-schema.md`](./event-schema.md)。

这样设计是因为 streaming agent、TUI、WebSocket、监控和回放都天然需要事件流。事件协议稳定后，CLI/TUI/Web/移动端可以共享行为语义。

### 2.6 进化只生成提案

自我进化系统可以分析历史、发现失败模式、生成优化建议、提出 skill、修改 profile 或生成 patch，但不应默认直接应用这些变化。

这样设计是因为 self-evolution 会影响系统长期行为。如果没有用户确认，系统可能引入错误优化、过拟合某些任务，或修改用户并不希望改变的行为。

## 3. 入口形态

### 3.1 CLI

CLI 是命令行接口，适合一次性命令、脚本、管道和自动化场景。例如：

```text
ominiforge run "summarize this file"
ominiforge session list
ominiforge tool install ...
```

CLI 应保持可组合、可脚本化、输出结构清晰。CLI 不等同于 TUI。

### 3.2 TUI

TUI 是终端全屏交互界面，适合长任务、多面板、快捷键、实时日志、session 切换、tool 执行状态展示等场景。TUI 可以参考 oh-my-pi 类产品中“命令执行”和“agent 对话”自由切换的体验。

TUI 仍然不应实现核心 agent 逻辑。它应通过 service runtime 或本地 API 驱动 agent，并订阅事件流渲染界面。

### 3.3 Web

Web 页面用于远程访问 agent、查看 session、监控执行状态、浏览报告、审批自我进化提案，以及管理配置、profile、tool 和 skill。

Web 前端应通过 gateway 连接服务层。它不应直接操作底层 session 文件或插件运行时。

### 3.4 Desktop 和 Mobile

桌面端和移动端作为后续入口，应复用 gateway/service runtime 的协议。它们主要负责用户体验、通知、离线能力和本地系统集成，不应复制核心 agent 执行逻辑。

## 4. 总体分层

```text
UI / Integration Layer
├─ CLI
├─ TUI
├─ Web
├─ Desktop
├─ Mobile
└─ Feishu / WeChat / QQ / other integrations

Gateway Layer
├─ HTTP API
├─ WebSocket event stream
├─ JSON-RPC
├─ external webhook adapters
└─ auth / permission boundary

Service Runtime Layer
├─ session manager
├─ scheduler
├─ event bus
├─ config manager
├─ profile manager
├─ permission manager
└─ runtime orchestration

Core Agent Layer
├─ agent loop
├─ planning / execution policy
├─ context manager
├─ memory interface
├─ tool invocation interface
└─ model interface

Extension Layer
├─ built-in tool host
├─ MCP client (server lifecycle, JSON-RPC adapter)
├─ hook registry (Rust trait + shell hook runner)
├─ skill manager
├─ MCP adapter (外部 MCP server → 内部 Tool trait)
├─ ACP adapter
└─ A2A adapter

Infrastructure Layer
├─ session storage
├─ sandbox
├─ monitor / trace / cost accounting
├─ artifact store
├─ search index
└─ evolution worker
```

## 5. Workspace 拆分方案

拆分原则：只在满足以下条件时拆为独立 crate：

1. **不同编译目标** — 当前无此需求（WASM 已废弃）。
2. **其余情况** — 用 module 分层，不拆 crate。

"架构整洁"不是拆 crate 的理由，module boundary 已够；编译时间未成瓶颈前不做物理拆分。
因此初期只有一个 crate：

```text
crates/
  ominiforge/              # library + binary（唯一 crate）
```

> **Polyglot monorepo**：自 Phase 6 起仓库新增 `frontend/`（SvelteKit + TS，Node 项目，
> 不进 Cargo workspace），与 Rust `src/` 物理隔离，唯一接触点是 ts-rs 生成的类型文件。
> Rust 侧仍维持**单 crate**，本节原则不破。详见 [`frontend.md`](./frontend.md) §5。

主 crate 内部 module 布局：

```text
src/
├── core/          # event schema, state machine, core traits
├── session/       # storage, fork, DAG
├── context/       # compaction, injection, prefix cache
├── llm/           # model trait, provider trait
├── provider/      # openai/, xiaomi/
├── tool/          # built-in tool 实现, ToolRegistry, Tool trait
├── mcp/           # MCP client, adapter, server lifecycle
├── hook/          # hook registry, built-in hooks, shell hook runner
├── skill/         # skill lifecycle
├── memory/        # memory interface + stores
├── monitor/       # trace, usage, cost (EventBus subscriber)
├── evolution/     # session analysis, proposal generation
├── agent/         # agent loop, orchestration
├── gateway/       # HTTP/WS server (feature-gated)
├── cli/           # 命令解析、子命令
├── tui/           # 终端 UI 渲染
└── main.rs        # 入口分发
```

依赖方向：

```text
main.rs → cli/ / tui/ / gateway/
cli/ tui/ gateway/ → agent/ + session/ + monitor/
agent/ → core/ + llm/ + tool/ + mcp/ + hook/ + context/ + memory/ + skill/
tool/ → core/
mcp/ → core/ + tool/
hook/ → core/
monitor/ → core/ (EventBus subscriber, 不侵入 core)
provider/ → llm/
core → 不依赖任何上层 module
```

### 5.1 Feature flags

| Feature   | 控制范围              | 默认 |
|-----------|----------------------|------|
| `gateway` | gateway/ module 编译 | on   |
| `tui`     | tui/ 相关依赖(ratatui 等) | on   |
| `provider-openai`  | OpenAI provider | on   |
| `provider-xiaomi`  | Xiaomi MiMo provider | on   |

### 5.2 何时再拆

满足任一条件时考虑物理拆分：增量编译时间超过可接受阈值；某 module 需被外部项目独立引用
（如 event schema 被其他工具消费）；provider 数量增多且各自依赖树庞大。Module boundary
已画好，物理拆分是机械操作。

## 6. Session 管理

Session 是系统核心能力之一。它不仅是对话历史，还包含执行事件、tool 调用、artifact、监控数据、fork 关系和后续自我进化分析依据。

### 6.1 Session Fork

系统需要支持从任意 session 的任意对话点 fork 出新 session。用户可能在一次对话中出现多个分支问题，如果都放在同一个 session 中，模型上下文会变得混乱。Fork 可以让用户从某个上下文状态开始探索分支，同时保留原 session 继续深入。

Fork 应采用 DAG 结构，而不是复制完整历史。

```text
sess_A
├─ sess_B  # from sess_A event 42
└─ sess_C  # from sess_A event 77
```

### 6.2 Session 存储

采用 append-only event log + index database。详见 [`doc/session-storage.md`](./session-storage.md)。

```text
.omini/sessions/
  {session_id}/
    session.toml
    events.jsonl
    context_snapshot.json   # 仅 fork/compaction/reconfiguration 时存在
    artifacts/
  index/
    sessions.sqlite
    search/
```

文件职责：

```text
session.toml          # 纯元数据：id, profile_id, created_at, origin(kind + parent_id)
events.jsonl          # 事件流，source of truth，每行省略 session_id
context_snapshot.json # 非 "new" session 的初始上下文（messages 数组，含 system prompt）
artifacts/            # tool 输出、中间产物
sessions.sqlite       # 查询索引，可从文件重建
search/               # 全文检索索引，可从文件重建
```

设计要点：

- 目录扁平，目录名即 session_id（ULID 格式），不按时间分片。
- 无 transcript.md，人类可读展示由前端渲染。
- 无 status 字段。Session 存在即可用，任何 session 随时可被 fork。
- 子 session 完全自包含，父 session 可被删除不影响子 session 运行。
- SQLite 不承担唯一真相角色，可从文件重建。

## 7. 上下文管理

系统需要区分 session log 和 context view。详见 [`doc/context-management.md`](./context-management.md)。

```text
Session Log   # 不可变真实历史（events.jsonl）
Context View  # 本轮发给模型的 messages 数组（运行时内存结构，不落盘）
```

Context view 不独立落盘，只在创建新 session（fork/compaction/reconfiguration）时物化为 context_snapshot.json。运行时 agent loop 持有内存结构，每轮只追加。

### 7.1 上下文压缩

上下文压缩总是创建新 session，不修改原 session 历史。

```text
sess_A original history
└─ sess_A2 (compaction)
   ├─ origin.kind = "compaction"
   ├─ origin.parent_id = sess_A
   ├─ origin.source_seq_range = [0, 150]
   └─ context_snapshot.json = 摘要 messages 数组
```

压缩触发方式：

- 自动触发：context view token 数超过 threshold（默认 80% context window）时触发。
- 手动触发：`/compact`（全量摘要）、`/compact --keep-last N`（保留最近 N 轮）。

### 7.2 缓存命中率

Context view 从前到后按稳定性排列，保障 prefix cache 命中率：

```text
┌─────────────────────────────────┐
│ system prompt + tool schemas     │  ← 稳定前缀
├─────────────────────────────────┤
│ context_snapshot (if non-new)    │  ← session 内不变
├─────────────────────────────────┤
│ conversation messages + injection│  ← 只追加，不改写
└─────────────────────────────────┘
```

规则：

- System prompt 不含动态内容。
- Tool schema block 按 name 字母序排列。
- 历史消息只追加不改写。
- 动态注入（Memory/RAG/ACP/Hook）保留在历史中不移除（保 cache），同步写 InjectionEvent 到 events.jsonl。
- 注入必须节制（token 上限、去重、优先引用而非全文）。
- Monitor 跟踪 cache_hit_tokens / total_input_tokens 比率。

## 8. Tool 与 Hook 系统

系统支持两类 tool 和两类 hook，统一通过 ToolRegistry 和 HookRegistry 管理。

### 8.1 Tool 分类

```text
Tool
├─ Built-in（Rust 代码，编译进 binary）
│  ├─ read, write, shell, search, lsp, ...
└─ MCP（外部 MCP server，stdio/SSE 通信）
   └─ 用户自建 / 社区 / 第三方 SaaS
```

Agent loop 对两类 tool 使用统一 Tool trait，不区分来源。MCP 是唯一外部扩展机制。

### 8.2 Hook 分类

```text
Hook
├─ Built-in hook（Rust trait impl）
└─ User hook（shell command / 脚本）
```

Hook 通过 stdin/stdout JSON 协议与 shell hook 通信。Before hook 可 pass/modify/block，after hook 仅 observe。

### 8.3 MCP Server 管理

MCP server 配置在 `.omini/config/mcp.toml`。Agent 启动时自动启动配置的 server，通过 MCP 标准协议（JSON-RPC over stdio/SSE）通信。一个 MCP server 可暴露多个 tools。

## 9. Provider 系统

系统需要支持多个模型 provider，包括 Xiaomi MiMo、OpenAI-compatible provider、主流云模型和自部署模型。

Provider 不应把私有 DTO 泄漏到 core agent。Provider adapter 负责把外部协议转换成内部稳定事件和消息类型。

```text
External provider response
→ provider adapter
→ core AgentEvent / ModelEvent
→ agent loop
```

这样可以避免 agent loop 依赖某个 provider 的 JSON shape，也便于新增 provider。

## 10. Skill 系统

Skill 是对高频任务、最佳实践、工具组合、提示词流程和执行策略的封装。Skill 应支持创建、更新、失效检测和优化。

自我进化系统可以根据 session 历史发现高频任务并提出 skill 草案，也可以发现失败或过期 skill 并提出修改建议。

## 11. Hook 系统

Hook 用于在特定事件前后插入轻量逻辑，例如 session start、tool call before/after、model request before/after、artifact created、task completed 等。

Hook 实现为 Rust trait（内置 hook）或 shell command（用户自定义 hook）。详见 [`doc/hook-protocol.md`](./hook-protocol.md)。

## 12. MCP、ACP 和 A2A

MCP、ACP、A2A 应作为 protocol adapter 层，而不是侵入 core。Core 使用自己的事件和 trait，协议层负责转换。

```text
core events/types
├─ MCP adapter
├─ ACP adapter
├─ A2A adapter
├─ JSON-RPC adapter
└─ WebSocket adapter
```

ACP 主要用于让编辑器或外部应用接入 Ominiforge。A2A 用于 agent 间协作，后续可能成为多 agent 协作核心。

## 13. Memory 系统

Memory 系统需要支持 agent 跨 session 记忆。它应与 session 历史区分：session 是完整事实记录，memory 是经过提炼、可检索、可更新的长期知识。

Memory 应支持不同作用域：

- user memory
- project memory
- profile memory
- skill memory
- tool memory
- global memory

Memory 写入应可追溯来源 session，避免无法解释的记忆污染。

## 14. Profile 系统

Profile 用于定义不同 agent 身份和能力组合。例如 coding agent、research agent、daily assistant。Profile 应组合以下内容：

- system prompt
- model/provider preference
- tool set
- skill set
- permission policy
- sandbox policy
- memory scope
- context policy
- cost policy

Profile 不应复制核心逻辑。它是运行时配置组合。

## 15. 配置管理

配置系统需要管理全局配置、项目配置、profile 配置、provider 配置、tool/plugin 配置和安全策略。

配置应支持层级覆盖：

```text
default config
→ user config
→ project config
→ profile config
→ session override
```

敏感信息如 API key 不应进入普通配置文件，应使用环境变量、secret store 或受控凭据管理。

## 16. 监控系统

监控系统是核心能力，不是附加功能。它需要记录：

- token usage
- cache hit / cache miss
- provider latency
- model request/response metadata
- tool call latency
- tool failure reason
- sandbox resource usage
- session duration
- cost estimate
- event trace
- artifact lineage

这些数据用于三类目标：

1. 成本统计和控制。
2. 调试和复盘。
3. 自我进化分析和优化。

监控记录应与 session event log 关联，但可在独立索引中聚合。

## 17. 监控与沙箱

所有 tool 执行统一经过 event journal 记录（ToolEvent），支持全量审计和后续分析。详见 [`doc/sandbox.md`](./sandbox.md)。

Shell tool 沙箱分阶段：初期无沙箱直接 spawn（本地使用），后续可选容器隔离（server 部署），远期支持可复现快照。

MCP server 作为普通进程运行，安全性靠用户信任（安装行为即授权）。未来 marketplace 可通过签名 + 审核 + 可选容器隔离增强安全。

## 18. Gateway 与 Scheduler

Gateway 是外部入口和出口，不应承担核心调度逻辑。Web、移动端、第三方应用和通知系统都通过 gateway 接入。

日常任务应由 scheduler 触发，由 service runtime 创建或恢复 session，再由 agent 执行。Gateway 负责把执行状态和结果推送给外部应用。

```text
scheduler
→ service runtime
→ session manager
→ agent execution
→ monitor
→ gateway notification
```

### 18.1 部署模型

Gateway 以用户级服务运行，不是系统级服务。

```bash
systemctl --user enable ominiforge-gateway
loginctl enable-linger $USER   # 确保 logout 后服务继续运行
```

- 用户级服务与 CLI 共享同一 UID、home 目录、`.omini/` 数据。
- CLI 不连接 Gateway。CLI 和 Gateway 各自独立执行 agent loop，通过共享文件系统保持数据一致。
- `ominiforge serve` 可作为前台模式运行（开发/临时使用）。
- 多用户/服务器级部署（系统级服务 + tenant 隔离）为后续扩展，初期不支持。

Gateway 存在的意义是 CLI 无法覆盖的场景：Web 远程访问、定时任务（scheduler 需要常驻进程）、手机/桌面应用接入、多设备同时访问 session。

### 18.2 Workspace（工作目录）

工作目录是 session 属性，不是 runtime 属性。

```toml
# session.toml
[origin]
kind = "new"
workspace = "/home/user/project/foo"   # 可选
```

各入口的 workspace 来源：

| 入口 | workspace |
|------|-----------|
| CLI `ominiforge run` | 默认 CWD，可 `--workspace` 覆盖 |
| CLI `ominiforge tui` | 默认 CWD |
| Web/Gateway 创建 session | 用户显式选择，或不指定 |
| Scheduler 触发 | 任务定义中声明 |

- workspace = None 时，filesystem tools 不可用或受限（研究、聊天、规划类任务）。
- workspace = 具体路径时，tool 沙箱范围 = workspace + 额外授权路径。
- 不存在全局"运行时工作目录"概念。每个 session 自己知道自己在哪。

## 19. 自我进化系统

自我进化系统应作为后台 worker 和手动命令共同存在。

触发方式：

- cron-like 定期分析。
- 用户手动触发。
- 后续可支持达到一定 session 数量或失败率后触发。

产物目录建议：

```text
.omini/
  evolution/
    runs/
      evo_2026-06-10_020000/
        report.md
        failures.md
        skill_candidates/
        stale_skills.md
        cost_analysis.json
        proposals/
          proposal_001.toml
          patch.diff
```

生命周期：

```text
observed → proposed → approved → applied → evaluated
```

系统可以生成报告、skill 草案、profile 修改建议、tool 改进建议和 patch diff。应用阶段必须经过用户批准。

## 20. 初步实施顺序

建议先从底层边界开始，而不是先做 UI：

1. 定义 core event schema。
2. 定义 session event log schema。
3. 定义 tool/plugin protocol。
4. 重构当前 agent loop，使其只依赖 core 类型。
5. 拆分 workspace 基础 crates。
6. 建立 session storage 原型。
7. 接入 monitor trace 和 usage 记录。
8. 做 CLI/TUI 入口。
9. 做 gateway 和 Web 入口。
10. 做 evolution worker 原型。

这个顺序能保证系统越做越稳，而不是先把 UI 做出来后再反向拆核心逻辑。
