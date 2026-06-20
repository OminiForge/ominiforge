# Ominiforge 待深入讨论清单

本文记录架构设计中仍需要进一步讨论、验证或决策的事项。清单按优先级组织，目标是先稳定底层协议和数据模型，再推进 workspace 拆分和实现。

## P0：实现前必须明确

### 1. Core event schema ✅

已完成。输出物：[`doc/event-schema.md`](./event-schema.md)。

决策摘要：
- 统一 envelope + 分域 payload enum（Turn/Model/Tool/Session/Artifact/Error）。
- Envelope 必填：schema_version、seq、session_id、timestamp、source。可选：parent_event_id、turn_id。
- Event ID 用 session_id:seq 复合，session 内用 seq，跨 session 用全称。
- Source 用 kind enum + id string 二层结构。
- Streaming 用 start/delta/stop 三段。Model tool_call 和 Tool execution 分离。
- Turn 有显式状态机（running/completed/failed/interrupted），支持 resume。
- 不引入显式 span；monitor 从 event stream 派生 span 树。
- Payload 上限 64KB，超限存 artifact store + 引用。
- Schema 演进以追加兼容为主，极少 breaking change 才升版本。
- 内部 Rust enum 与外部 JSON 初期 1:1 + serde rename，后续按需引入转换层。

### 2. Session event log schema ✅

已完成。输出物：[`doc/session-storage.md`](./session-storage.md)。

决策摘要：
- 目录扁平存放于 `.omini/sessions/{session_id}/`，不按时间分片。Session_id 采用 ULID。
- session.toml 只存纯元数据（id、profile_id、created_at、origin），无 status 字段，无 system prompt。
- events.jsonl 每行省略 session_id（从目录名获取），首条事件为 SessionEvent::Created 记录初始 config 快照。
- 不生成 transcript.md，人类可读展示由前端渲染。
- 非 "new" session 有 context_snapshot.json，格式统一为 messages 数组（含 system role），agent loop 直接加载。
- System prompt 就是 messages 数组中的 system role message，无独立存储机制。
- Session 诞生方式四种：new / fork / compaction / reconfiguration。
- Fork snapshot 存 context view（发给模型的内容），不存原始 events。
- 子 session 完全自包含，父 session 可被删除不影响子 session 运行。
- Compaction 回溯引用为 optional，仅用于审计和调试。
- 待后续讨论：索引数据库字段设计、artifact 引用细节、session 清理策略。

### 3. Tool/Hook/Extension protocol ✅ (revised)

已完成。输出物：[`doc/tool-protocol.md`](./tool-protocol.md)、[`doc/hook-protocol.md`](./hook-protocol.md)、[`doc/plugin-protocol.md`](./plugin-protocol.md)（已更名为 Extension Model）。

**重大修订（2026-06-15）：废弃 WASM 方案，改用 Built-in + MCP。**

修订原因：
- WASM 沙箱无法 spawn process → agent tool 能力严重受限。
- Agent 平台的 tool 天然需要完整 OS 能力（shell、LSP、format、test）。
- WASM plugin 只能覆盖纯计算 + API 调用，覆盖面太窄，扩展性不足。
- MCP 已是行业标准扩展协议，无需自定义 plugin protocol。

当前决策：
- **Tool 分两类：Built-in（Rust impl）和 MCP（标准 MCP server）。**
- **Agent loop 对两类 tool 使用统一 Tool trait，不区分来源。**
- **MCP 是唯一外部扩展机制，不自定义 plugin 协议。**
- Tool 不支持 streaming，一次执行完整返回。
- Artifact 由 runtime 代管存储，tool 只返回内容，超 64KB 存 artifact store。
- Tool error：业务错误走 is_error，协议错误走 Err。

Hook protocol 当前决策：
- Hook 分 before（同步，pass/modify/block）和 after（异步，observe-only）。
- Hook 实现为 Rust trait（内置）或 shell command（用户扩展）。
- 固定 hook point 预定义列表，不允许挂任意 event。
- 全量事件观察由 host 侧 EventBus 完成（tokio broadcast channel）。
- Fail 策略：open / closed，用户可覆盖。
- 多 hook 按 priority 排序，默认 100。

已废弃：
- ~~WASM Component runtime (wasmtime)~~
- ~~WIT 接口定义~~
- ~~ominiforge-sdk crate~~
- ~~WASI 0.3 capability-based 权限~~
- ~~Plugin manifest (plugin.toml / tool.toml)~~

### 4. Context view 与 compaction 设计 ✅

已完成。输出物：[`doc/context-management.md`](./context-management.md)。

决策摘要：
- Context view 不独立落盘，运行时内存结构，只追加。仅在创建新 session 时物化为 context_snapshot.json。
- Compaction 总是创建新 session，不修改原 session。自动切换到新 session 继续。
- 自动触发 threshold 默认 80% context window，用户可配置。
- 手动命令：`/compact`（全量摘要）、`/compact --keep-last N`（保留最近 N 轮）。
- Origin 元数据记录 source_seq_range、model_used、prompt_template、created_by。
- 初期不做自动质量评估，monitor 记录 compaction 事件供后续分析。
- Prefix cache 保障：system prompt + tool schemas 稳定在前，历史只追加不改写，tool schemas 按 name 字母序。
- 动态注入（Memory/RAG/ACP/Hook）由 Context Manager 执行，保留在 context view 历史中不移除（保 cache），同步写 InjectionEvent 到 events.jsonl。
- 注入必须节制：max_tokens_per_turn、dedupe、prefer references over full content。
- Event Schema 新增 Injection payload 类型。

### 5. Workspace crate 边界 ✅ (revised)

已完成。输出物：[`doc/workspace-plan.md`](./workspace-plan.md)。

**修订（2026-06-15）：废弃 WASM 后只剩 1 crate。**

决策摘要：
- 只有一个 crate：`ominiforge`（library + binary）。
- ~~ominiforge-sdk~~ 已废弃（无 WASM，MCP server 用各语言标准 SDK）。
- Module 布局：core/ session/ context/ llm/ provider/ tool/ mcp/ hook/ skill/ memory/ monitor/ evolution/ agent/ gateway/ cli/ tui/。
- `extension/` 拆为 `tool/` + `mcp/` + `hook/`（独立关注点）。
- Feature flags 不变：gateway、tui、provider-openai、provider-xiaomi。

## P1：架构原型阶段需要明确

### 6. Sandbox 与监控 ✅ (revised)

已完成。输出物：[`doc/sandbox.md`](./sandbox.md)。

**修订（2026-06-15）：废弃 WASM 沙箱，聚焦监控 + 分阶段 shell 沙箱。**

决策摘要：
- 不使用 WASM 沙箱，安全性靠用户信任 + 全量 event 监控。
- 所有 tool 执行（built-in + MCP）统一写 ToolEvent 到 events.jsonl。
- Shell tool 分阶段：Phase 1 无沙箱直接 spawn，Phase 2 可选容器，Phase 3 可复现快照。
- 可恢复性（Phase 2）：git-based workspace snapshots，写操作前自动 tag/stash。
- 资源限制初期只做超时（tokio timeout），后续 cgroup。

### 7. Monitor trace model ✅

已完成。输出物：[`doc/monitor.md`](./monitor.md)。

决策摘要：
- Trace = event stream 本身，不引入独立 trace_id / span_id。通过 turn_id + parent_event_id + seq 重建嵌套。
- ModelEvent 记录：input/output tokens、cache read/write tokens、duration、TTFT、stop_reason、cost。
- Cache 命中率标准化为 `CacheMetrics { read_tokens, write_tokens }`，provider adapter 负责映射。
- Tool 聚合指标（calls/failures/latency/output_bytes）由 monitor 内存维护，按需持久化。
- 成本实时估算（每次 response 后），pricing table 用户可配置。
- 预算控制通过 cost limiter before hook 实现（session/daily 上限）。
- Monitor 是 EventBus subscriber，纯 observe，不侵入 core。

### 8. Profile schema ✅

已完成。输出物：[`doc/profile.md`](./profile.md)。

决策摘要：
- Provider 配置独立（`.omini/config/providers.toml`）：endpoint、protocol、model 元数据、pricing。
- Profile 定义 agent 身份和能力：system prompt、model 引用（provider_name/model_id）、tools、skills、memory、budget、hooks。
- Model 引用格式：`"openai-main/gpt-4o"`（完整）或 `"gpt-4o"`（短引用）。
- 单继承，字段级覆盖（子字段出现则完整替换父字段）。
- Session 绑定 profile，运行中切换 = 创建新 session（reconfiguration）。
- Context window 从 provider model 配置继承，compaction_threshold 在 profile 设置。
- Budget 在 profile（不同角色不同预算），pricing 在 provider。

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

### 10. Skill 生命周期 ✅

已完成。输出物：[`doc/skill.md`](./skill.md)。

决策摘要：
- Skill = Markdown + TOML frontmatter，人类可读可编辑。
- 渐进式披露：skill 索引（name + description）在 system prompt，model 自主决定是否 load。
- `load_skill` 是 built-in tool call：读取 skill → 执行动态命令 → 替换模板 → 返回完整内容。
- 模板语法：`{{exec "cmd"}}`、`{{now}}`、`{{workspace}}`、`{{env "VAR"}}` 等。
- 动态命令全部执行不 fail-fast，收集所有错误一起返回。
- 监控：load_success/partial/failure + task_completed/failed，供 evolution 分析。
- Evolution 提议新 skill 或修改，用户可 approve/reject/revise（多轮循环）。

## P2：扩展能力阶段需要明确

### 11. MCP / ACP / A2A 适配优先级 — 部分决策

已决策：
- **MCP client（stdio transport）**：Day 1，已实现（Phase 2 Step 5）。本地 server 全走 stdio
  子进程，零网络/零认证，覆盖当前全部场景。
- **MCP client 远程 transport（Streamable HTTP + OAuth）**：延后。理由如下。
- **MCP server 对外暴露**：延后，暂不暴露，等有明确对外能力需求时再设计。
- **A2A**：延后。当前无跨系统 agent 协作需求。内部多 agent 协作用 subagent/task system 解决。
- **ACP**：延后。编辑器集成初期通过 gateway HTTP/WS 顶替。

**MCP 远程 transport 延后记录（2026-06-20）：**
- 现状：`src/mcp/client.rs` 只实现 stdio；`config.rs` 的 `url` 字段解析但不连接，非 stdio server
  在 `connect` 处被 `McpError::NotStdio` 拒绝。
- 传输代际：MCP 远程传输有两代。**SSE**（2024-11-05 引入，双端点 `GET /sse` + `POST /messages`，
  已于 2025-03-26 标记废弃，不再单独实现）；**Streamable HTTP**（2025-03-26 起现行，单端点 `/mcp`，
  SSE 降级为其一种响应流模式，支持无状态部署 + `Last-Event-ID` 断线重连）。要做远程，直接上
  Streamable HTTP，跳过 SSE。
- 为何延后：远程 server 拖入一坨独立工程——OAuth 2.1 授权 flow（2025 spec 已写进规范）、token
  刷新、TLS、SSE 流解析、断线重连。每条都与 stdio 路径正交，且当前无远程 server 需求。Phase 2
  "可用" 目标（agent 能调外部 tool）已由 stdio 达成。
- 接入成本：`url` 字段占位已留，接 Streamable HTTP 时数据模型不动，只加一个 transport 实现 +
  OAuth client。属独立任务，需求出现时立项。

协议 adapter 位置：`src/mcp/`（已有）、`src/a2a/`、`src/acp/`（后续按需添加）。

### 12. Gateway API — 部分决策

已决策：
- Gateway 面向精美 Web 前端，最终公网暴露。
- HTTP API 覆盖：session CRUD/fork/message/cancel、profiles、tools、skills、monitor、evolution。
- WebSocket：`/sessions/:id/events` 实时 event stream。
- 认证模型：
  - GitHub OAuth 登录。
  - 管理员邀请制注册（关闭公开注册）。
  - 管理员可随时收回账号。
  - HTTPS 强制。
- 部署模型：用户级 systemd service，`ominiforge serve`。

待后续深入：
- OAuth flow 具体实现。
- 权限粒度（不同用户可访问不同 session/profile？）。
- Rate limiting 策略。
- Web 前端技术选型和 UI 设计。

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

### 14. Web/TUI 信息架构 — 部分决策

已决策：
- `ominiforge` 命令直接进入 TUI（不需要子命令）。
- TUI 主打简洁明了好用，复杂功能放 Web。
- Web 是主要操作界面，监控观测都在 Web 上。
- UI 设计后续单独深入讨论。

待后续深入：
- TUI 面板布局。
- Web 前端框架选型。
- Web UI 信息架构和交互设计。

### 15. Plugin marketplace — 延后

延后。等 MCP server 生态成熟后再设计 marketplace。
当前用户直接配置 mcp.toml 手动管理。

### 16. 配置可发现性（TOML + JSON Schema）— 待实现

TOML 保留作为配置格式（人工编辑友好、带注释、与 Rust 生态一致）。问题是用户不知道有哪些
字段可配。TOML 无独立 schema 标准，但 **Taplo**（事实标准 TOML LSP）支持用 **JSON Schema
校验/补全 TOML**：在 TOML 顶部加 `#:schema <url/path>` 指令，编辑器即可自动补全 + 校验 +
悬停文档（Cargo.toml 的补全即如此）。

方案（后续实现，与功能解耦）：
- 用 `schemars` 从 Rust 配置类型（providers / profile / pricing / limits / mcp）自动生成
  JSON Schema，与代码同步不漂移。
- 随仓库发布 schema 文件；`ominiforge init` 模板顶部写入 `#:schema` 指向它。
- 新增 `ominiforge config schema`（导出 schema）与 `config validate`（校验配置文件）命令。

不占用 Phase 2 主线；在经过相关配置类型的步骤里顺手给类型 derive `JsonSchema`。

## 建议实施顺序（修订）

基于 P0-P2 讨论结果，建议实施顺序：

1. ~~Core event schema~~ ✅
2. ~~Session event log schema~~ ✅
3. ~~Tool/Hook/Extension protocol~~ ✅ (revised: built-in + MCP)
4. ~~Context view and compaction~~ ✅
5. ~~Workspace crate boundary~~ ✅ (revised: 1 crate)
6. ~~Sandbox + monitor~~ ✅ (revised: event journal + 分阶段 shell sandbox)
7. ~~Monitor trace model~~ ✅
8. ~~Profile schema~~ ✅ (含 provider 配置)
9. ~~Memory~~ 延后（需独立研究）
10. ~~Skill lifecycle~~ ✅

**开始实现的优先顺序：**

```text
Phase 1 — 跑通 agent loop
  core event types + session storage + agent loop + LLM provider + built-in tools (shell/read/write)

Phase 2 — 可用
  context management + compaction + MCP client + monitor + TUI

Phase 3 — 强大
  profile + skill + hook + gateway + Web 前端

Phase 4 — 智能
  scheduler + task workspace + reviewer agent + evolution worker

Phase 5 — 生态
  memory system + MCP server + marketplace + A2A
```
