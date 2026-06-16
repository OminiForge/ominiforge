# Ominiforge Core Event Schema

本文档定义 Ominiforge 内部统一事件协议。所有 UI、gateway、session log、monitor 和 replay 都依赖该协议。

## 1. 设计原则

- 统一 envelope + 分域 payload enum。不做扁平大 enum，不做完全互不关联的事件体系。
- Event schema 不引入显式 span 概念。Monitor 层从 start/stop 配对事件自行派生 span 树。
- 事件一旦持久化即不可变。
- Schema 演进以追加兼容为主（只加字段、只加新 event type），极少情况做 breaking change 并升全局版本号。

## 2. Envelope 结构

每个事件共享以下信封字段：

```rust
struct CoreEvent {
    /// 协议版本，如 "ominiforge.event.v1"
    schema_version: String,

    /// Session 内递增序号，保证严格排序
    seq: u64,

    /// 所属 session
    session_id: SessionId,

    /// UTC 时间戳
    timestamp: DateTime<Utc>,

    /// 事件来源
    source: EventSource,

    /// 因果关联：指向触发本事件的上游事件（可选）
    parent_event_id: Option<EventId>,

    /// 所属 turn（可选，turn 启动后填充）
    turn_id: Option<TurnId>,

    /// 实际负载
    payload: EventPayload,
}
```

### 2.1 Event ID

```rust
struct EventId {
    session_id: SessionId,
    seq: u64,
}
```

- Session 内操作只用 `seq`。
- 跨 session 引用序列化为 `"{session_id}:{seq}"`。
- 全局唯一性靠 session_id + seq 复合保证。

### 2.2 Event Source

```rust
struct EventSource {
    kind: SourceKind,
    id: String,
}

enum SourceKind {
    Model,      // LLM provider
    Tool,       // tool 执行
    Runtime,    // ominiforge 内部运行时
    User,       // 用户操作
    System,     // scheduler、evolution 等系统级
    External,   // MCP server、A2A 远程 agent
}
```

- `kind` 用于快速过滤和路由。
- `id` 标识具体实例，如 `"shell"`、`"openai-compatible/deepseek-r1"`、`"mcp://github-server"`。
- 无 `Plugin` variant：WASM plugin 方案已废弃，外部扩展统一走 MCP（`External`）。

## 3. Payload 分域

```rust
enum EventPayload {
    Turn(TurnEvent),
    Model(ModelEvent),
    Tool(ToolEvent),
    Session(SessionEvent),
    Artifact(ArtifactEvent),
    Injection(InjectionEvent),
    Error(ErrorEvent),
}
```

MonitorEvent 不在核心 payload 中。Monitor 是观测层，从核心事件派生 trace/span/cost，不污染 replay 必需语义。

## 4. Turn 事件

Turn 是 agent loop 一次迭代的容器，有显式状态机：

```
pending → running → completed | failed | interrupted
```

```rust
enum TurnEvent {
    Started { turn_id: TurnId },
    Completed { turn_id: TurnId },
    Failed { turn_id: TurnId, failed_at_event_id: EventId, retryable: bool },
    Interrupted { turn_id: TurnId, interrupted_at_event_id: EventId },
    Resumed { turn_id: TurnId, resume_from_event_id: EventId },
}
```

- Turn failed 时记录断点位置 `failed_at_event_id`。
- Interrupted 时记录中断位置 `interrupted_at_event_id`。
- Resume 不开新 turn，同一 turn_id 继续，从断点续跑。
- 用户无需重新输入即可恢复。

## 5. Model 事件

采用 start/delta/stop 三段表达 streaming：

```rust
enum ModelEvent {
    RequestStarted {
        request_id: String,
        provider: String,            // "openai", "anthropic", "xiaomi"
        model: String,               // "gpt-4o", "mimo-7b"
        temperature: f32,
        max_tokens: Option<u32>,
        tool_schemas_count: u32,     // 本次请求携带多少 tool schema
        input_tokens_estimate: u32,  // 发送前估算（实际值在 RequestCompleted.usage）
    },
    ContentBlockStart {
        request_id: String,
        index: u32,
        block_type: ContentBlockType, // Text | Reasoning | ToolCall { id, name }
    },
    TextDelta {
        request_id: String,
        index: u32,
        text: String,
    },
    ReasoningDelta {
        request_id: String,
        index: u32,
        text: String,
    },
    ToolCallDelta {
        request_id: String,
        index: u32,
        json_delta: String,
    },
    ContentBlockStop {
        request_id: String,
        index: u32,
    },
    RequestCompleted {
        request_id: String,
        stop_reason: StopReason,
        usage: Usage,
        duration_ms: u64,                 // first byte → last byte
        time_to_first_token_ms: Option<u64>,
        provider_request_id: Option<String>, // provider 返回的 request id（调试用）
    },
    RequestFailed {
        request_id: String,
        duration_ms: u64,
        error: ErrorDetail,
    },
}

struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,   // 命中 prefix cache 读取的 tokens
    cache_write_tokens: u32,  // 本次写入 cache 的 tokens
}

enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
}
```

注意：

- model 产生 tool call 是 ModelEvent。tool 实际执行是 ToolEvent。二者分离。
- **`cost` 不存入 event**。Cost 由 monitor 层从 `usage` + 可配置 pricing table 实时派生（见 [`monitor.md`](./monitor.md)）。存 usage（不可变事实）而非 cost（依赖会变的 pricing），保证历史可用最新 pricing 重算。
- `cache_*` token 的 provider 字段映射见 [`monitor.md`](./monitor.md) §3。

## 6. Tool 事件

```rust
enum ToolEvent {
    Started {
        tool_call_event_id: EventId, // 指向 ModelEvent 中的 tool call
        tool_name: String,
        source: ToolSource,          // Builtin | Mcp { server_name }
        input: serde_json::Value,
        working_dir: Option<PathBuf>,
    },
    Completed {
        tool_call_event_id: EventId,
        result: ToolOutput,          // 内联内容或 artifact 引用（由 runtime 决定）
        duration_ms: u64,
        output_bytes: usize,
        artifacts_created: Vec<ArtifactId>,
    },
    Failed {
        tool_call_event_id: EventId,
        duration_ms: u64,
        error: ErrorDetail,
    },
}

enum ToolSource {
    Builtin,
    Mcp { server_name: String },
}
```

- `source` 区分内置 tool 与 MCP tool（按 server 聚合监控）。
- `file_changes`（pre/post diff）为 Phase 2 能力，初期不记录，见 [`sandbox.md`](./sandbox.md) §2.2。
- 单次 tool 指标随 event 落盘；聚合指标（p95、失败率等）由 monitor 内存维护。

## 7. Session 事件

```rust
enum SessionEvent {
    // 首条事件，记录初始 config 快照，使 replay 自包含
    Created {
        profile_id: Option<String>,
        tools: Vec<String>,        // 启动时可用 tool 名列表
        workspace: Option<PathBuf>,
    },
    Forked { parent_session_id: SessionId, fork_at_seq: u64 },
    Paused,
    Resumed,
    Ended { reason: SessionEndReason },
}
```

`Created` 的 config 快照内容随实现演进（model、context policy 等）以追加字段方式扩展。详见 [`session-storage.md`](./session-storage.md) §3。

## 8. Artifact 事件

Artifact 只记录引用，内容存 artifact store：

```rust
enum ArtifactEvent {
    Created {
        artifact_id: ArtifactId,
        kind: String,        // "file", "image", "code_block", ...
        media_type: String,
        uri: String,         // artifact store 路径或 URI
        size: u64,
        sha256: Option<String>,
        source_event_id: Option<EventId>,
    },
}
```

## 9. Injection 事件

动态注入内容记录，用于 replay 和分析时还原每轮 model 实际看到的 context：

```rust
enum InjectionEvent {
    ContextInjected {
        source: InjectionSource,
        content: String,
        token_count: u32,
    },
}

enum InjectionSource {
    Memory,
    RAG,
    ACP,
    Hook,
}
```

- 每轮 model request 前，Context Manager 执行注入并记录此事件。
- 注入内容保留在 context view 历史中（保 cache），同时持久化到 events.jsonl（保可审计）。
- Compaction 时历史 injection 被摘要浓缩。

## 10. Error 事件

独立结构化，不只 string：

```rust
struct ErrorDetail {
    code: String,
    message: String,
    severity: ErrorSeverity,     // Fatal | Error | Warning
    retryable: bool,
    source_event_id: Option<EventId>,
    provider_raw: Option<serde_json::Value>,
}
```

## 11. Payload 大小限制

- 设定阈值：单个 event payload 不超过 64KB。
- 超过阈值时，内容存入 artifact store，event 中只保留引用。
- 信息零损失——原始完整数据通过 artifact 可查。
- 传给 model 时的摘要/截断属于 context management 层，不在 event schema 层处理。

## 12. Schema 演进策略

采用追加兼容策略：

1. 只加新字段，不删不改已有字段语义。Consumer 用 `#[serde(default)]` 忽略不认识的字段。
2. 新事件类型随时可加。Consumer 遇到不认识的 event type 跳过，不 crash。
3. 极少情况需要不兼容变更时，升 `schema_version`（如 v1 → v2），写 migration guide。
4. 旧 event log 永远以原版本保留，不做回写迁移。

## 13. 因果关系表达

- 不引入显式 span 树作为一等概念。
- 用 `parent_event_id` 表达直接因果（如 tool.started → 对应 model tool_call event）。
- 用 start/stop 配对表达时间范围（如 model.request_started + model.request_completed）。
- Monitor 层从 event stream 构建 span 树用于 trace 可视化，这是派生视图。

## 14. 与外部 JSON Wire Protocol 的关系

- 内部 Rust enum 和外部 JSON 不要求完全一一绑定。
- 初期用同一套 struct + `#[serde(rename)]` 即可。
- 当内部需要优化（arena 分配、zero-copy）时再引入转换层。
- 外部 JSON 格式是稳定承诺；内部 Rust 类型可自由重构，只要转换层保持正确。
