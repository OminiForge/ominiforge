# Monitor Trace Model

## 1. 设计原则

- Trace = event stream 本身，不引入独立 trace ID 或 span 体系。
- Monitor 从 events.jsonl 派生聚合指标，不侵入 core 执行路径。
- 成本实时估算（用于预算控制），后处理可重新计算。
- Provider 差异由 provider adapter 屏蔽，monitor 层只看统一类型。

## 2. Model Request 记录

> **权威定义在 [`event-schema.md`](./event-schema.md) §5。** 本节描述 monitor 如何消费这些字段，字段集以 event-schema 为准（`RequestStarted` / `RequestCompleted` / `RequestFailed` + `Usage`）。下方片段为示意，若与 event-schema 冲突以 event-schema 为准。

### 2.1 RequestStarted

每次发送 model request 时写入（字段见 event-schema §5）：携带 `model`、`provider`、`temperature`、`max_tokens`、`tool_schemas_count`、`input_tokens_estimate`。

### 2.2 RequestCompleted

收到完整 response 后写入：携带 `stop_reason`、`usage`（input/output/cache_read/cache_write tokens）、`duration_ms`、`time_to_first_token_ms`、`provider_request_id`。

**`cost` 不存入 event** —— monitor 从 `usage` + pricing table 实时派生（见 §6）。这样历史可用最新 pricing 重算，不被写入时的价格锁死。

### 2.3 RequestFailed

请求失败时写入：携带 `duration_ms` 和结构化 `error: ErrorDetail`（含 `retryable`）。

## 3. Cache 命中率标准化

不同 provider 返回格式不同，统一为：

```rust
pub struct CacheMetrics {
    pub read_tokens: u32,   // 命中 prefix cache 读取的 tokens
    pub write_tokens: u32,  // 本次写入 cache 的 tokens
}
```

Provider 映射：

| Provider | 原始字段 | 映射 |
|----------|---------|------|
| OpenAI | `usage.cached_tokens` | read_tokens |
| Anthropic | `cache_read_input_tokens` | read_tokens |
| Anthropic | `cache_creation_input_tokens` | write_tokens |
| 无 cache 信息 | - | 全部为 0 |

**Cache 命中率指标**：`cache_read_tokens / input_tokens`

Monitor 按 session 和全局两个维度追踪此比率。

## 4. Tool 监控

### 4.1 单次记录（events.jsonl）

已在 ToolEvent 中定义：

```rust
ToolEvent::Completed {
    duration_ms: u64,
    output_bytes: usize,
    artifacts_created: Vec<ArtifactRef>,
}

ToolEvent::Failed {
    duration_ms: u64,
    error: ErrorDetail,
}
```

### 4.2 聚合指标（monitor 内存维护）

```rust
pub struct ToolMetrics {
    pub tool_name: String,
    pub source: ToolSource,         // Builtin | Mcp { server_name }
    pub total_calls: u64,
    pub total_failures: u64,
    pub avg_duration_ms: f64,
    pub p95_duration_ms: u64,
    pub total_output_bytes: u64,
    pub failure_reasons: HashMap<String, u64>,
}
```

聚合在内存中维护，按需持久化（session 结束时写入、定期快照、或由 evolution 请求）。

## 5. Trace 结构

不引入 OpenTelemetry-style trace_id / span_id。Event seq 是全序的，嵌套关系通过已有字段重建：

```text
Turn N (turn_id)
├── ModelEvent::RequestStarted   (seq=N+1, turn_id=N)
├── ModelEvent::RequestCompleted  (seq=N+2, turn_id=N)
├── ToolEvent::Started            (seq=N+3, turn_id=N, parent_event_id=N+2)
├── ToolEvent::Completed          (seq=N+4, turn_id=N, parent_event_id=N+3)
├── ModelEvent::RequestStarted   (seq=N+5, turn_id=N)
└── TurnEvent::Completed          (seq=N+M, turn_id=N)
```

重建规则：
- `turn_id` 关联同一 turn 内所有 event。
- `parent_event_id` 表达因果关系（tool execution 因 model tool_call 而起）。
- Monitor 从 event stream 按 turn_id 分组，按 seq 排序，即可绘制 waterfall 视图。

## 6. 成本估算

### 6.1 实时估算

每次 ModelEvent::RequestCompleted 后立即计算（不写回 event，仅 monitor 内存 + 聚合）：

```rust
pub struct CostEstimate {
    pub input_cost: f64,        // USD
    pub output_cost: f64,
    pub cache_read_cost: f64,
    pub cache_write_cost: f64,
    pub total_cost: f64,
}
```

- 输入为 `RequestCompleted.usage` + pricing table。
- 用于 cost limiter hook（before hook 检查累计 cost 是否超预算）。
- **不写入 event**。历史成本可用最新 pricing 从 `usage` 重算。

### 6.2 Pricing Table

```toml
# .omini/config/pricing.toml

[models."gpt-4o"]
input_per_million = 2.50
output_per_million = 10.00
cache_read_per_million = 1.25
cache_write_per_million = 2.50

[models."gpt-4o-mini"]
input_per_million = 0.15
output_per_million = 0.60
cache_read_per_million = 0.075

[models."mimo-7b"]
input_per_million = 0.00
output_per_million = 0.00
```

- 用户可更新 pricing（模型降价时）。
- 未配置的 model → cost = 0（不报错，只跳过）。
- Evolution worker 可用最新 pricing 重算历史成本。

### 6.3 预算控制

```toml
# .omini/config/limits.toml

[cost]
session_max_usd = 5.00       # 单 session 上限
daily_max_usd = 20.00        # 每日上限
warn_at_percent = 80         # 达到 80% 时警告
```

Cost limiter 作为 built-in before hook（`model:request:before`），检查累计 cost，超限时 block。

## 7. MCP Server 监控

MCP server 作为子进程，额外追踪：

| 指标 | 来源 |
|------|------|
| 启动耗时 | spawn → initialize 完成 |
| 崩溃次数 | 进程非正常退出 |
| 重启次数 | 自动重启计数 |
| 调用延迟 | 包含在 ToolEvent.duration_ms |
| 错误率 | ToolEvent::Failed / total |
| 可用状态 | running / crashed / disabled |

这些由 MCP client module 维护，monitor 消费 event stream 聚合。

## 8. Session 级汇总

Session 结束时生成汇总（写入 events.jsonl 最后一条或独立文件）：

```rust
pub struct SessionSummary {
    pub total_turns: u32,
    pub total_model_requests: u32,
    pub total_tool_calls: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub cache_hit_rate: f64,
    pub duration_seconds: u64,
    pub tools_used: HashMap<String, u32>,  // tool_name → call_count
    pub errors: Vec<ErrorSummary>,
}
```

用于：
- CLI/TUI session 结束时展示
- Evolution worker 分析
- Web dashboard 报告

## 9. Monitor 实现架构

```text
events.jsonl (source of truth)
      │
      ▼
EventBus (tokio broadcast channel)
      │
      ├── Monitor subscriber
      │     ├── 实时聚合（内存 HashMap）
      │     ├── 成本累计
      │     └── 异常检测（连续失败、延迟飙升）
      │
      ├── Cost limiter（before hook 查询 monitor 累计值）
      │
      └── Evolution collector（采样、统计、写入分析数据）
```

Monitor 是 EventBus 的一个 subscriber，纯 observe，不修改任何 event。

## 10. 待后续完善

- Trace 可视化格式（导出为 Chrome Trace JSON 或其他格式）。
- 告警规则配置（连续 N 次失败、cost 超限、latency 超阈值）。
- 历史数据保留和清理策略。
- 跨 session 聚合报告（周报、月报）。
