# Tool Streaming：工具调用的流式渲染管线

定义 tool call 从「模型开始吐 args」到「结果定稿」之间，后端如何向前端提供**可渲染的
中间形态**。核心原则与 `doc/tool-view.md` 一脉相承：**前端只渲染；一切对流式 args 的
加工（partial-JSON 提取、diff 构建、节流）都在后端。** 前端看到的永远是一个已经处理好、
可直接渲染的 view，而不是需要自己拼凑的原始 JSON 碎片。

本文档是流式 tool-call 重构的权威契约。骨架（协议）已落地，各 tool 的流式 presenter
按 §5 的阶段计划逐个实现。

## 1. 动机：为什么不再透传流式 args

旧管线把模型吐出的**原始 JSON 字符串片段**（`Delta::ToolArgs` / `on_tool_call_delta`）
原样转发给前端。前端对这些半截 JSON 唯一能做的就是拼字符串裸显——它无法解析、无法渲染、
无法从中得到任何用户可读的信息。这是把传输层的分帧细节泄漏成了展示层事件，带来三个问题：

1. **无价值**：滚动的 JSON 碎片对用户不是进展，是噪音。
2. **复杂度前倾**：前端为这套裸显维护了一条「流式占位 → commit 时 truncate 重放」的
   脆弱路径（`open` map、`streaming` 标记、`seq=-1` 占位卡片）。
3. **正确价值在后端**：工具调用真正可渲染的中间形态（一行摘要、diff 预览）后端都有
   现成能力（`summarize_by_name`、diff 构建），只是送达时机太晚（commit / 审批 /
   完成时才到）。

新管线把「加工」收回后端：后端在流式阶段就产出**与最终同构的 view**，前端用一条渲染
路径贯穿始终。

## 2. 三阶段契约

一个 tool call 的生命周期对前端呈现为三个阶段，**阶段二的 view 与阶段三同构**：

```
阶段一  BlockStart { kind: "tool_call", tool: name }
        └─ 前端立即渲染卡片骨架（现状，不动）

阶段二  Delta::ToolProgress { index, view }        ← 新增
        ├─ view 是与阶段三 TextView 完全同构的信封，只是内容在生长
        ├─ 自包含快照（非增量 patch）：可 coalesce（丢旧留新），迟到客户端无需补历史
        ├─ 后端节流：~100–150ms 间隔 + 行数阈值，block_stop 前强制 flush 最后一帧
        └─ 无 presenter 的工具不发此事件，卡片从骨架直接跳到阶段三

阶段三  Tool::Completed { result 内含 TextView(ui), diagnostics, … }
        ├─ 现状，协议不动
        ├─ LSP diagnostics 本就是执行结果的一部分（append_diagnostics），随结果一起
        │  到达——模型与前端同时拿到，不构成额外的「晚发」阶段
        └─ 前端把完整 args 作为 debug 折层展示（替代被删的流式 args 裸显）

审批门  维持在 args 完整之后（现状，不改）。审批与 view 已解耦（Phase 3）：门不再
        自算自存 preview，人在卡片已有的阶段二 view（BlockStop flush 的完整快照）上审批。
```

**关键设计：阶段二写进和阶段三相同的 `view` 字段。** 前端 fold 里 `tool_progress`
直接更新卡片的 `view`，阶段三到达时 settled view 覆盖它——一条渲染路径，无缝衔接。
工具没有 presenter 时前端零改动（骨架 → settled view）。

## 3. 后端骨架（已落地）

```
provider ──StreamEvent──▶ collector ──▶ StreamSink ──▶ GatewayEvent::Delta ──SSE──▶ 前端
                              │             │
              累积 args（已有）│             │ on_tool_call_progress(index, view)
                              ▼             ▼
                    [未来] StreamPresenter.render(累积args) ──▶ Delta::ToolProgress
```

- `StreamSink::on_tool_call_progress(index, view)`：新增回调，**默认 no-op**。注释标明
  「NOT YET PRODUCED」——协议就位，尚无 presenter 调用它，流式行为与之前完全一致。
- `Delta::ToolProgress { index, view }`：新增 SSE 事件，`ts-rs` 已导出到前端。
- `BroadcastSink::on_tool_call_progress`：把快照广播出去。

骨架刻意**不触碰** `on_tool_call_delta` / `Delta::ToolArgs`——它们仍被前端调试折层
使用，删除是 Phase 6 的事（见 §5），且要等第一个 presenter 上线证明 `ToolProgress`
通路可用之后。

## 4. `StreamPresenter` 与 `stream_args`（Phase 2 已落地）

流式能力按工具逐个接入，框架目标是**新增工具的默认成本为零**。`Tool` trait 加了
`stream_presenter()`（默认 `None` = 无流式，自动正确），`ToolRegistry::stream_presenter(name)`
按名查找：

```rust
// src/tool/mod.rs —— 默认 None，新工具零成本获得正确行为
fn stream_presenter(&self) -> Option<Box<dyn StreamPresenter>> { None }

// 快照契约：输入是【完整累积 args】（非 delta），输出是与阶段三同构的 TextView 信封。
// 在节流下被调用，绝不逐 token。返回 None = 此刻还无法渲染，调用方保留上一帧。
#[async_trait]
pub trait StreamPresenter: Send {
    async fn render(&mut self, accumulated_args: &str) -> Option<String>;
}
```

**已实现的组件**：

- `src/tool/stream_args.rs` — `PartialArgs`：**面向已知 schema 的增量提取器**（非通用
  partial-JSON parser）。回答两个问题：`complete_string(field)`（字段已闭合的值，用于
  `path` 这类必须先等到的字段）；`streaming_string(field)`（字符串字段的已接收前缀，用于
  `content` 这类生长载荷）。容错于任意字节截断（未闭合字符串、半截 `\u` 转义都安全丢弃）。
  评估过 `serde_json` 的 `StreamDeserializer` 与现成 crate，均不匹配「半个 JSON 值」需求，故自研。
- `src/tool/write_stream.rs` — `WriteStreamPresenter`：`path` 闭合后读一次旧文件并缓存；
  新文件生长 `code` 视图、覆盖场景对**截断到前缀行数**的旧文件 diff（避免把未到达的尾部
  误显为删除，且只 diff 完整行——末行可能被流式截断）。
- `src/agent/collector.rs` — 节流驱动：`ToolCallDelta` 累积后在节流下调用 presenter
  （`PROGRESS_MIN_INTERVAL` 120ms + `PROGRESS_MIN_GROWTH` 64B），`BlockStop` 前强制 flush
  最后一帧。presenter 经 `collect_round(.., tools: Option<&ToolRegistry>)` 注入；无 registry
  （测试/headless）则 presenter 从不挂载，行为同骨架期。

每个后续工具的流式方案写在它自己的源文件头部注释里（`edit.rs` / `shell.rs` 等），实现时
对照即可。

## 5. 阶段计划（拆 session 执行）

| Phase | 内容 | 状态 |
|---|---|---|
| **1** | **协议骨架**（本改动）：`on_tool_call_progress` + `Delta::ToolProgress` + 前端 `case 'tool_progress'` + 本文档。行为零变化。 | ✅ 本次 |
| **2** | **`stream_args` 提取器 + write presenter**：渐进 view（新文件生长 code 视图、覆盖场景对截断旧文件 diff），节流在 collector。第一个端到端可用的流式工具。 | ✅ 本次 |
| **3** | **审批与 view 解耦**：删除整个 preview 机制（`Permission::Requested.preview`、`Tool::preview()`、前端 `Item.preview`）。审批不再自算自存 diff，人在卡片已有的阶段二 view 上审批。 | ✅ 本次 |
| **4** | **edit presenter（深方案）**：`edits[i]` 按 path→old→new 流式，锚点定位 + 渐进替换。先留浅方案（结构化进度）作为中间态。 | 待做 |
| **5** | **shell 输出流式**：结果流式（非 args 流式），独立特性，协议可复用快照模式。 | 待做 |
| **6** | **删除 `Delta::ToolArgs` / `on_tool_call_delta`**：第一个 presenter 证明 ToolProgress 通路后，移除原始 args 透传与前端裸显路径，args 改由阶段三 debug 折层一次性给出。 | 待做 |
| **7** | **通用兜底 + 收尾**：MCP/未知工具的字段级进度；read/find 确认无需流式；TUI 对齐。 | 待做 |

每个 Phase 独立可合、可回滚；Phase 2 落地前框架本身不引入任何行为变化。

### 5.1 冗余删除清单（Phase 6/7 收尾时照勾，不靠记忆）

骨架与后续 Phase 会留下一批「过渡期代码」，全部完成后**必须**删除。以下为完整清单，
收尾 session 逐项核实后勾掉；任何一项的删除前提都标注在括号里。

**后端（Phase 6，前提：至少一个 presenter 已上线且 ToolProgress 通路被验证）**

- [ ] `StreamSink::on_tool_call_delta`（`src/agent/sink.rs`）
- [ ] `Delta::ToolArgs` 变体（`src/gateway/actor.rs`，删后重跑 `just ts-export`）
- [ ] `BroadcastSink::on_tool_call_delta`（`src/gateway/actor.rs`）
- [ ] collector 里 `sink.on_tool_call_delta(...)` 调用点（`src/agent/collector.rs`，
      `StreamEvent::ToolCallDelta` 分支）——注意：`arguments.push_str` 的累积逻辑**保留**，
      它是 presenter 和持久化的输入，只删 sink 转发那一行
- [ ] `RecordingSink.tool_args` 测试字段及相关断言（`src/agent/collector.rs` tests）

**前端（Phase 6，前提同上）**

- [ ] `applyDelta` 的 `case 'tool_args'` 分支（`frontend/src/lib/conversation.ts`）
- [ ] `tool` item 的流式 args 拼接（`seq=-1` 占位卡片上的 `args` 累积）——阶段三起 args
      只进 debug 折层，折叠形态随 Phase 6 重新定义

**需逐个核实、不能盲删（text/reasoning 仍在复用，只剥 tool 专用部分）**

- [ ] `open` map 中 tool_call 的 index 追踪（text/reasoning 的 temporal 路径保留）
- [ ] `streaming` 标记、`requestStart`/`requestCommitted`、`commitBlock` 的 truncate
      路径中**仅服务于 tool args 流式占位**的分支——剥离前先确认 text/reasoning 的
      streaming 预览不受影响

**Phase 7 收尾**

- [ ] 骨架期注释的「NOT YET PRODUCED / skeleton only / inert until phase 2」字样全部
      摘除（`sink.rs` / `actor.rs` / `conversation.ts`），文档 §3 的「尚无 presenter」
      表述更新为现状
- [ ] 本清单勾完后，删除本小节（§5.1）

> 注：旧前端 diff-builder 方案（`tool-view.md` §1）的既存死代码 `parseReadResult` /
> `cacheWriteArgs` / `splitFileLines` / `writePrevLinesFor` 已于骨架期独立清理（与本
> 重构无关的顺手清理，故不列入上方清单）。

## 6. 不变量

- **view 永不进模型上下文**：与 `tool-view.md` §3 同一边界。`ToolProgress` 是 live
  delta（不持久化、不回放、不进 `render_output`），天然满足。
- **快照自包含**：任何一帧 `ToolProgress` 都可独立渲染，丢弃任意前序帧不影响正确性。
- **阶段二 ≡ 阶段三同构**：前端用同一个 `view` 字段、同一个渲染组件贯穿流式与定稿。
- **节流在后端**：前端不感知帧率，极端 token 突发也不会逐字节渲染。
