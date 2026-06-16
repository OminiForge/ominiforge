# Context View & Compaction 设计

本文档定义 context view 的生成方式、compaction 机制、动态注入策略和 prefix cache 命中率保障措施。

## 1. 核心概念

```text
Session Event Log    — 不可变真实历史（events.jsonl）
Context View         — 本轮发给 model 的 messages 数组（运行时内存结构）
Context Snapshot     — 新 session 启动时的初始上下文（context_snapshot.json）
```

- Context view 不独立落盘。运行时 agent loop 持有内存结构，每轮只追加。
- 只在创建新 session（fork/compaction/reconfiguration）时才物化为 context_snapshot.json。
- Session 冷启动时从 events.jsonl 重建 context view（或从 context_snapshot.json 加载）。

## 2. Context View 结构

从前到后按稳定性排列，保障 prefix cache 命中率：

```text
┌─────────────────────────────────┐
│ system prompt (from profile)     │  ← 稳定前缀
│ tool schemas (按 name 字母序)    │
├─────────────────────────────────┤
│ context_snapshot (if non-new)    │  ← session 内不变
├─────────────────────────────────┤
│ [injection_1]                    │
│ user_1                           │
│ assistant_1 (含 tool calls)      │
│ [injection_2]                    │  ← 只追加，不改写
│ user_2                           │
│ assistant_2                      │
│ ...                              │
│ [injection_N]                    │
│ user_N                           │
└─────────────────────────────────┘
```

## 3. Prefix Cache 命中规则

1. System prompt 不含动态内容（不注入当前时间、随机 ID 等）。
2. Tool schema block 按 name 字母序排列，不按加载顺序。
3. 历史消息只追加不改写，中间消息不被修改或删除。
4. 动态注入内容留在历史原位不动，不剥离。
5. Compaction 后新 session 的 snapshot 成为新稳定前缀。
6. Monitor 跟踪每次 request 的 cache_hit_tokens / total_input_tokens 比率。

## 4. Compaction 机制

### 4.1 触发方式

- **自动触发**：context view token 数超过 threshold 时触发。
- **手动触发**：用户执行 `/compact` 命令。

### 4.2 Threshold 配置

```toml
[context]
auto_compact_threshold = 0.8   # 占 model context window 的比例
```

实际上限 = threshold × context_window - max_output_tokens，留出 model 回复空间。

用户可通过 profile 或全局配置修改此值。

### 4.3 行为

Compaction 总是创建新 session，不修改原 session。保证历史不可变。

```text
sess_A (original)
└─ sess_A2 (compaction)
   ├─ origin.kind = "compaction"
   ├─ origin.parent_id = sess_A
   └─ context_snapshot.json = LLM 摘要（messages 数组格式）
```

创建后自动切换到新 session 继续对话。原 session 完整保留，可回查。

### 4.4 手动压缩命令

```text
/compact                — 全量摘要，创建新 session 并切换
/compact --keep-last 3  — 保留最近 3 轮完整对话，其余摘要
```

### 4.5 Origin 元数据

session.toml 中记录压缩来源信息：

```toml
[origin]
kind = "compaction"
parent_id = "01J5M2..."
source_seq_range = [0, 150]     # 被摘要的事件范围
model_used = "deepseek-r1"      # 执行摘要的模型
prompt_template = "default"     # 压缩 prompt 模板标识
created_by = "auto"             # "auto" | "manual"
```

### 4.6 质量评估

初期不做自动评估。Monitor 记录 compaction 事件，供 evolution worker 后续分析。

后续可选方案：
- 关键事实抽取对比（retention rate）。
- 回归测试（对压缩后 context 问历史问题）。
- 用户行为信号（compaction 后快速 fork 回去 = 质量差）。

## 5. 动态注入（Injection）

### 5.1 注入者

Runtime 的 Context Manager 组件。在 agent loop 构建本轮 model request 前触发：

```text
Agent Loop 准备发 model request
  → Context Manager 触发注入流程
    → Memory 检索
    → RAG 召回（如果有）
    → ACP 推送的编辑器状态（如果有）
  → 注入内容追加到 context view
  → 发出 model request
```

Hook（`model:request:before`）也可间接 modify 注入内容。

### 5.2 持久化

注入内容同时写入 events.jsonl 和保留在 context view 中：

```rust
enum EventPayload {
    // ... existing ...
    Injection(InjectionEvent),
}

enum InjectionEvent {
    ContextInjected {
        source: InjectionSource,  // Memory | RAG | ACP | Hook
        content: String,
        token_count: u32,
    },
}
```

- Context view 中历史 injection 不移除，保障 cache 命中。
- Events.jsonl 完整记录，保障 replay 和分析。
- Compaction 时所有历史 injection 被摘要浓缩。

### 5.3 成本控制策略

动态注入必须节制，以降低上下文膨胀和成本：

```toml
[context.injection]
max_tokens_per_turn = 2000          # 单轮注入 token 上限
max_items_per_source = 5            # 单来源最多条目数
min_relevance_score = 0.75          # 相关性阈值
dedupe_by_hash = true               # 内容已在 context 中则跳过
prefer_references_over_full_content = true  # 优先摘要/引用
```

执行规则：

- 能不注入就不注入：只有当前 turn 明确需要才加。
- 不重复注入：同 hash 内容已在当前 context 可见则跳过。
- 只注入最小必要片段：优先 snippet / summary / artifact ref，不塞全文。
- 大内容进 artifact store：context 里只放摘要 + artifact 引用。
- Source 排序稳定：Memory → RAG → ACP → Hook，避免无意义 cache 变化。
- Monitor 记录被丢弃的候选（dropped count / reason），events 只记录实际注入内容。

## 6. Agent Loop 中 Context View 的生命周期

```text
Session 创建
  → 加载 context_snapshot.json（如有）或构建空 context
  → 设置 system prompt + tool schemas 作为稳定前缀

每轮：
  → Context Manager 执行 injection
  → 追加 injection 到 context view
  → 追加 user message 到 context view
  → 检查 token 数是否超 threshold
    → 超过：触发 compaction → 创建新 session → 切换
    → 未超过：发 model request
  → 追加 assistant response 到 context view（含 tool calls / results）
  → events.jsonl 同步写入对应事件
```

## 7. 与其他子系统的关系

- **Session Storage**：compaction 创建新 session，遵循已定义的 session.toml + context_snapshot.json 格式。
- **Event Schema**：新增 InjectionEvent payload 类型。
- **Monitor**：跟踪 cache hit rate、injection token count、compaction 频率。
- **Evolution Worker**：分析 compaction 事件、cache hit 趋势，可建议调整 threshold 或 injection 策略。
- **Profile**：threshold 和 injection 配置可在 profile 级别覆盖。
