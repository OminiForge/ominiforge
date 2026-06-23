# Ominiforge 待深入讨论清单

本文记录架构设计中仍需进一步讨论、验证或决策的事项。

**已决策事项**（P0/P1 全部完成）已落到各自规范文档，下表只留索引；本文正文仅保留
**未决策 / 待深入**的开放项。已决策延后、待需求再立项的功能集中在
[`feature-requests.md`](./feature-requests.md)。

## 已决策索引（详见各规范文档）

| # | 主题 | 输出物 |
|---|------|--------|
| 1 | Core event schema | [`event-schema.md`](./event-schema.md) |
| 2 | Session event log schema | [`session-storage.md`](./session-storage.md) |
| 3 | Tool / Hook / Extension protocol（built-in + MCP，废弃 WASM） | [`tool-protocol.md`](./tool-protocol.md)、[`hook-protocol.md`](./hook-protocol.md) |
| 4 | Context view 与 compaction | [`context-management.md`](./context-management.md) |
| 5 | Workspace crate 边界（1 crate） | [`architecture.md`](./architecture.md) §5 |
| 6 | Sandbox 与监控（event journal + 分阶段 shell sandbox） | [`sandbox.md`](./sandbox.md) |
| 7 | Monitor trace model | [`monitor.md`](./monitor.md) |
| 8 | Profile schema（含 provider 配置） | [`profile.md`](./profile.md) |
| 10 | Skill 生命周期 | [`skill.md`](./skill.md) |

## 已完成阶段

### Phase 1 — 核心运行时（单轮可跑通）

**目标**：最小可运行 agent，单轮 CLI 能跑通一次完整 turn。

| 模块 | 内容 |
|------|------|
| `src/core/` | Event envelope + payload enum，SessionId/TurnId/EventId |
| `src/session/` | append-only JSONL event log，SessionStore，SessionWriter，session meta |
| `src/agent/` | 单轮 agent loop，plan 控制工具，completion gate，stuck 检测 |
| `src/provider/openai/` | OpenAI 兼容 streaming provider |
| `src/llm/` | Message 类型，Provider trait，EventStream |
| `src/tool/` | ToolRegistry，built-in：read、write、shell |
| `src/config/` | Profile、provider 配置、pricing |
| `src/cli.rs` | `ominiforge run` 单轮 CLI |

### Phase 2 — 可用阶段（多轮 + 可观测 + MCP + TUI）

**目标**：从"能跑通单轮"变成"可用"：多轮对话、上下文超限自动压缩、可观测、可通过 MCP 扩展、有简洁 TUI。详见 [`phase2-plan.md`](./phase2-plan.md)。

| 步骤 | 内容 | 产出模块 |
|------|------|---------|
| Step 1 | 多轮交互循环 + session resume（从 events.jsonl 重建 context） | `src/context.rs`，`src/agent/resume.rs` |
| Step 2 | Token 使用追踪 + context 估算（provider 授权值 calibrate 本地估算） | `src/context.rs`（ContextLedger） |
| Step 3 | Context compaction（超阈值自动 summarize → 新 session） | `src/agent/mod.rs`（`compact`） |
| Step 4 | Monitor + EventBus（纯 fold over events，在线/离线两路消费） | `src/monitor.rs`，`src/session/bus.rs` |
| Step 5 | MCP client（stdio transport，子进程管理） | `src/mcp/` |
| Step 6 | TUI（streaming 渲染、session picker、resume、auto-compaction） | `src/tui/` |

## 规划阶段

以下各 Phase 尚未实现，顺序按依赖关系排列。每个 Phase 实施前需先完成详细拆分。

### Phase 3 — Skill 系统 ✅（核心完成）

**目标**：可复用任务模板，model 自主渐进式加载。协议见 [`skill.md`](./skill.md)。

已实现（`src/skill.rs`）：
- `SkillStore`：扫描 `.omini/skills/*.md`，解析 frontmatter（name/description），`_` 前缀跳过。
- `skill_index_block`：生成 system prompt 的 `## Available Skills` 索引（仅 name + description）。
- `LoadSkillTool`（built-in `load_skill`）：定位 skill → 渲染 body → 返回完整内容。
- 模板渲染：`{{exec "cmd"}}`（5s 超时）、`{{now}}`、`{{workspace}}`、`{{env "VAR"}}`、`{{profile}}`；
  全执行、收集错误、不 fail-fast，partial 失败在末尾附错误摘要。
- CLI 接入（`prepare()`）：按 profile `[skills].enabled`（空=全部）列出，注入索引 + 注册工具。

延后（依赖 monitor/evolution）：
- Skill metrics（load 成功/部分/失败、task 完成/失败）、生命周期状态判定。
- `{{session_id}}` 模板（需 per-invocation context，当前展开为空）。
- 显式 `/skill` 命令（list/edit/disable）、skill 间组合、参数化。

### Phase 4 — Hook 系统（下一步）

**目标**：在固定 pipeline 位置拦截/观察事件。协议已定，见 [`hook-protocol.md`](./hook-protocol.md)。

已决策：
- Hook point 为固定预定义集合，不允许订阅任意 event。
- Before hook 同步，可 pass/modify/block；After hook 异步，仅 observe。
- 实现为 host 侧 Rust trait（内置）或 shell command（用户）。
- 全量 event 订阅需求由 EventBus 满足，不属 hook 系统。
- 所有 hook 执行写入 event log。

产出模块：`src/hook.rs`（当前 stub），agent loop 接入 hook point，profile `[hooks]` section wire。

验证：注册一个 before hook 在 `turn:start` block 某输入，turn 被拦截；一个 after hook 在 `turn:end` observe，event log 有记录。

### Phase 5 — Gateway（网络访问层）

**目标**：让 Web/桌面/手机通过网络访问 agent。TUI 本地使用不经过 Gateway。

已决策：
- axum HTTP server，feature-gated（`gateway`）。
- REST API 覆盖完整工作流：session CRUD/fork/message/cancel、profiles、tools、skills、monitor、evolution。
- WebSocket / SSE：`/sessions/:id/events` 实时 event stream。
- 认证：**单用户 API key**（静态 token，配置在 profile 或单独配置文件）。GitHub OAuth 延后，等多用户需求出现时再加。
- 部署：`ominiforge serve`，用户级 systemd service。
- HTTPS 强制（公网暴露时）。

待后续深入（实施前拆分）：
- REST 路由完整列表与请求/响应 schema。
- WebSocket / SSE 协议细节。
- API key 存储与轮换机制。
- Rate limiting 策略。

### Phase 6 — Web 前端

**目标**：浏览器端完整 agent 工作流（对话、session 管理、监控、任务、进化审批）。

已决策：
- 通过 Gateway API 操作，不直接调 Rust core。
- 主要面向跨机器随时访问场景。
- 前端框架待选型（Phase 5 完成后决策）。

待后续深入：
- 框架选型（SvelteKit / Next.js / Leptos）。
- UI 信息架构与页面拆分。
- 交互设计。

### Phase 7 — Scheduler（任务管理系统）

**目标**：任务管理 + agent 自动执行 + reviewer 自动验收。

已决策（见 §13）：任务状态机、reviewer agent 自动验证、任务来源。

待后续深入（实施前拆分）：
- 任务 schema（字段、优先级、依赖关系）。
- Reviewer agent profile 与验证策略。
- 并发执行限制。

### Phase 8 — Evolution worker

**目标**：分析 session 历史，生成可审批的优化建议（skill 草案、profile 变更、patch）。

已决策（见 `architecture.md` §19）：只生成提案，不自动应用。

待后续深入：实施前独立设计。

### Phase 9 — 桌面端（Tauri）

**目标**：本地原生应用，最佳本地体验，同时支持连接远程 server。

已决策：
- 本地模式：Tauri shell + Rust core 直接调用，读本地配置启动，无需 `serve`。
- 远程模式：通过 Gateway API 连接注册的远程 server（URL + token）。
- 客户端维护 server 注册列表，支持切换。当前阶段各 server 完全独立。

### Phase 10 — 手机端

**目标**：移动端快速查看任务状态、审批、临时操作。

已决策：
- 通过 Gateway API 连接远程 server，完整 agent 工作流。
- 适合轻量操作：任务审批、快速查询、通知。

### Phase 11 — 多节点协同（延后，需独立研究）

**目标**：多 Ominiforge 实例作为 edge nodes，任务可跨节点调度，类 K8s 架构。

当前阶段各 server 完全独立，客户端手动切换。此 Phase 是高级 feature，需独立研究后设计。
见 [`feature-requests.md`](./feature-requests.md) FR-3。

## 开放项

### 9. Memory 系统 — 延后，需独立研究

高级特性，复杂度高，需独立研究后设计。当前不定义接口，不约束实现。

已明确方向：
- Memory 是独立子系统，不是简单文件存储。
- 需支持复杂逻辑结构（图关系、级联更新、矛盾检测）。
- 应能接入主流 memory 系统（mem0、Zep、LangMem 等）作为可选 backend。
- 后端可替换（文件 / 向量库 / 图数据库 / 第三方 service）。
- Profile 中 `memory.scopes` 和 `memory.auto_write` 预留了配置位。

未决定：
- 接口定义（需深入研究后确定）。
- 存储格式。
- 图结构设计。
- 推理层（LLM vs 代码规则 vs hybrid）。
- 与 evolution 系统的协作方式。

启动条件：agent loop + session + tool 基本跑通后，再独立立项研究。

### 11. MCP / ACP / A2A 适配优先级 — 部分决策

已决策：
- **MCP client（stdio transport）**：已实现（Phase 2 Step 5）。本地 server 全走 stdio
  子进程，零网络/零认证，覆盖当前全部场景。
- **MCP client 远程 transport（Streamable HTTP + OAuth）**：延后，见
  [`feature-requests.md`](./feature-requests.md) FR-1。
- **MCP server 对外暴露**：延后，暂不暴露，等有明确对外能力需求时再设计。
- **A2A**：延后。当前无跨系统 agent 协作需求。内部多 agent 协作用 subagent/task system 解决。
- **ACP**：延后。编辑器集成初期通过 gateway HTTP/WS 顶替。

协议 adapter 位置：`src/mcp/`（已有）、`src/a2a/`、`src/acp/`（后续按需添加）。

### 12. Gateway API — 已决策，待 Phase 5 实施拆分

已决策：
- Gateway 是所有非 TUI 入口（Web/桌面/手机）的唯一后端；TUI 本地使用直接调 Rust core，不经 Gateway。
- HTTP API 覆盖完整工作流：session CRUD/fork/message/cancel、profiles、tools、skills、monitor、evolution。
- WebSocket / SSE：`/sessions/:id/events` 实时 event stream。
- 认证模型：**单用户 API key**（静态 token）。GitHub OAuth + 多用户隔离延后，等多用户需求出现时再立项。
- 部署模型：用户级 systemd service，`ominiforge serve`，HTTPS 强制（公网时）。
- 框架：axum，feature-gated（`gateway`）。

待 Phase 5 实施拆分时深入：
- REST 路由完整列表与请求/响应 schema。
- API key 存储与轮换机制。
- Rate limiting 策略。

### 13. Scheduler 与任务工作区 — 部分决策

已决策：
- Scheduler 不是简单 cron，是 **任务管理系统 + agent 执行引擎**。
- 任务状态机：

```text
backlog（待办）
  → running（执行中，agent 正在处理）
    → pending_review（待交付，reviewer agent 验证中）
      → delivered（已交付，验证通过）
      → blocked（验证失败，打回修改 → 重新 running）
```

- 每个任务有交付标准（delivery criteria）。
- "待交付" 由 reviewer agent 自动验证：
  - 代码任务：功能正确性、无回归、风格一致、简洁性、最优实现。
  - 其他任务：按任务定义的交付标准检查。
- 验证通过 → 已交付。验证失败 → blocked，附带失败原因，打回修改。
- 任务来源：用户手动创建、Scheduler cron、Evolution 建议。
- 日常定时任务也在工作区中展示。

待后续深入：
- 任务 schema（字段、优先级、依赖关系）。
- Reviewer agent 的 profile 和验证策略。
- 任务分配（手动 vs 自动）。
- 并发执行限制。
- 工作区 UI 设计。

### 14. 多平台信息架构 — 已决策

已决策：
- 各入口**独立且完整**，互不依赖补全功能。每个平台都能独立完成完整 agent 工作流。
- 平台定位：
  - **TUI**：本地命令行，功能受终端限制，`ominiforge` 直接进入。
  - **Web**：跨机器随时访问，无需安装，通过 Gateway API。
  - **桌面端**：本地最佳体验，也支持连接远程 server。
  - **手机端**：移动端快速操作（查任务/审批/临时操作），通过 Gateway API。
- 运行模式：
  - **本地模式**：读本地配置直接启动 Rust core，无需 `serve`（TUI/桌面端本地时）。
  - **远程模式**：通过 Gateway API 连接注册的 server（URL + token）。
- 客户端维护 server 注册列表，支持多 server 管理。当前阶段各 server 完全独立。

待后续各 Phase 深入：
- TUI 面板布局（Phase 2 已完成基础版，后续按需迭代）。
- Web 前端框架选型（Phase 6 实施前决策）。
- 桌面端技术方案（Tauri，Phase 9 实施前拆分）。
- 手机端技术方案（Phase 10 实施前拆分）。

### 15. Plugin marketplace — 延后

延后。等 MCP server 生态成熟后再设计 marketplace。
当前用户直接配置 mcp.toml 手动管理。

### 16. 配置可发现性（TOML + JSON Schema）— 待实现

待实现，与功能解耦，可顺手做。方案与接入成本见
[`feature-requests.md`](./feature-requests.md) FR-2。
