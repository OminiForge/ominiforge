# Ominiforge Core Event Schema

定义内部统一事件协议。所有 UI、gateway、session log、monitor、replay 依赖该协议。

**精确类型定义在代码**：envelope 见 [`src/core/envelope.rs`](../src/core/envelope.rs)（`CoreEvent`、
`EventSource`、`SourceKind`）；payload 见 [`src/core/payload.rs`](../src/core/payload.rs)
（`EventPayload` 及各分域 enum、`Usage`、`ErrorDetail` 等）。本文只讲设计意图与不可变契约，
字段级细节以代码及其注释为准。

## 1. 设计原则

- 统一 envelope + 分域 payload enum。不做扁平大 enum，不做完全互不关联的事件体系。
- 不引入显式 span 概念。Monitor 层从 start/stop 配对事件自行派生 span 树。
- 事件一旦持久化即不可变。
- Schema 演进以追加兼容为主（只加字段、只加新 event type），极少情况做 breaking change 并升全局版本号。

## 2. Envelope

每个事件共享信封字段：`schema_version`、`seq`（session 内递增，严格排序）、`session_id`、
`timestamp`、`source`、可选 `parent_event_id`（因果上游）、可选 `turn_id`、`payload`。

- **Event ID** = `session_id` + `seq` 复合。Session 内只用 `seq`；跨 session 引用序列化为
  `"{session_id}:{seq}"`。
- **Event Source** = `kind`（Model / Tool / Runtime / User / System / External）+ `id`
  实例标识（如 `"shell"`、`"mcp://github-server"`）。`kind` 用于快速过滤路由。无 `Plugin`
  variant——WASM 已废弃，外部扩展统一走 MCP（`External`）。

## 3. Payload 分域

按 Turn / Model / Tool / Session / Artifact / Injection / Hook / Error 分域。MonitorEvent **不在**
核心 payload 中——monitor 是观测层，从核心事件派生 trace/span/cost，不污染 replay 必需语义。

### 3.1 Turn

Turn 是 agent loop 一次迭代的容器，显式状态机：`pending → running → completed | failed |
interrupted`。设计要点：

- `Started.input` 记录开场用户输入；无用户输入的 turn（scheduler、自动续跑）为 `None`，
  replay 据此重建开场 user message。
- **Failed ≠ 进程崩溃**。撞 `max_rounds` 或 plan 卡死是*优雅停止*：副作用（已写文件）依然
  成立，loop 写 `Failed` 后把部分结果交还调用方（`TurnOutcome.incomplete` 带同一 `reason`）。
- **硬错误**（provider / 持久化故障）以 `Result::Err` 冒泡，不作为 `TurnOutcome`；但抛出前
  loop 尽力先写一条 `ErrorEvent::Raised` 再写 `TurnEvent::Failed{reason:None}`。故 `reason:
  None` + 配对 `ErrorEvent` = 硬错误；有 `reason`、无配对 `ErrorEvent` = 优雅停止。持久化
  故障下补写可能再失败，此时静默放弃、原样抛首个错误，绝不掩盖或递归重试。
- Resume 不开新 turn，同 `turn_id` 从断点续跑，用户无需重新输入。

### 3.2 Model

流式与持久化分离：

- **流式传输**（provider 边界，`llm::StreamEvent`）用 start/delta/stop 三段逐 token 推送，
  供前端实时渲染。传输层概念，**不落盘**。
- **持久化**（`ModelEvent`）按 content block 合并：一个块的所有 delta 累积，块结束写一条
  `ContentBlock`。一条文本/推理/工具调用 = 一行，而非每 token 一行。

要点：

- model 产生 tool call 是 ModelEvent（`ContentBlock` 内含 `BlockContent::ToolCall`），tool
  实际执行是 ToolEvent，二者分离。`ToolEvent::Started.tool_call_event_id` 指向该 `ContentBlock`。
- 空文本/推理块（开了块没产出）不落盘——零信息量。tool call 块始终记录。
- **`cost` 不存入 event**：由 monitor 从 `usage` + 可配 pricing 实时派生（见
  [`monitor.md`](./monitor.md)）。存 usage（不可变事实）而非 cost（pricing 会变），历史可用
  最新 pricing 重算。`cache_*` token 的 provider 字段映射见 [`monitor.md`](./monitor.md) §3。

### 3.3 Tool

`source` 区分 Builtin / MCP（按 server 聚合监控）。`file_changes`（pre/post diff）为 Phase 2
能力，初期不记录，见 [`sandbox.md`](./sandbox.md) §2.2。单次 tool 指标随 event 落盘；聚合指标
（p95、失败率）由 monitor 内存维护。

### 3.4 Session

首条事件 `Created` 记录初始 config 快照（profile、tool list、workspace），使 replay 自包含；
快照内容随实现以追加字段扩展。另有 `Forked` / `Paused` / `Resumed` / `Ended`。详见
[`session-storage.md`](./session-storage.md) §3。

### 3.5 Artifact

只记录引用（id / kind / media_type / uri / size / sha256 / source_event_id），内容存 artifact store。

### 3.6 Injection

记录动态注入，用于 replay 还原每轮 model 实际所见的 context。来源：Memory / RAG / ACP / Hook /
**Runtime**。`Runtime` 用于 agent loop 自身注入的提醒（完成度门、卡死警告），见
[`plan.md`](./plan.md) §6–§8，文本用 `<reminder>...</reminder>` 包裹，作为真实消息永久留在
context 历史中（保 prefix cache）。Compaction 时历史 injection 被摘要浓缩。

### 3.7 Hook

记录 hook 在 pipeline point 的执行：`hook_name` / `hook_point`（如 `tool:invoke:before`）/
`outcome`（Pass / Modified / Blocked{reason} / Observed / Failed{error}）/ `duration_ms`。
协议要求所有 hook 执行写 event log，用于 replay、监控和审计，见
[`hook-protocol.md`](./hook-protocol.md) §1、§11。block 同时产生 point 专属失败事件（如
`tool:invoke:before` block → `ToolEvent::Failed { code: blocked_by_hook }`，§8）。

### 3.8 Error

结构化（非纯 string）：`code` / `message` / `severity`（Fatal/Error/Warning）/ `retryable` /
`source_event_id` / `provider_raw`。

## 4. Payload 大小限制

单个 event payload 不超过 64KB。超限内容存 artifact store，event 中只留引用——信息零损失。
传给 model 时的摘要/截断属 context management 层，不在 event schema 层处理。

## 5. Schema 演进策略

1. 只加新字段，不删不改已有字段语义。Consumer 用 `#[serde(default)]` 忽略不认识的字段。
2. 新事件类型随时可加。Consumer 遇到不认识的 event type 跳过，不 crash。
3. 极少情况需不兼容变更时升 `schema_version`（v1 → v2），写 migration guide。
4. 旧 event log 永远以原版本保留，不做回写迁移。

## 6. 因果关系表达

- 不引入显式 span 树作为一等概念。
- `parent_event_id` 表达直接因果（tool.started → 对应 model tool_call event）。
- start/stop 配对表达时间范围（request_started + request_completed）。
- Monitor 层从 event stream 构建 span 树用于 trace 可视化，派生视图。

## 7. 与外部 JSON Wire Protocol 的关系

- 内部 Rust enum 与外部 JSON 不要求完全一一绑定。
- 初期用同一套 struct + `#[serde(rename)]`。内部需优化（arena、zero-copy）时再引入转换层。
- **外部 JSON 格式是稳定承诺**；内部 Rust 类型可自由重构，只要转换层保持正确。
</content>
