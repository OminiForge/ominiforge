# Tool Streaming：工具调用的流式渲染管线

定义 tool call 从「模型开始吐 args」到「结果定稿」之间，后端如何向前端提供**可渲染的
中间形态**。核心原则与 `doc/tool-streaming.md` 一脉相承：**前端只渲染；一切对流式 args 的
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

## 3. 后端管线

```
provider ──StreamEvent──▶ collector ──▶ StreamSink ──▶ GatewayEvent::Delta ──SSE──▶ 前端
                              │             │
              累积 args ──────┤   on_tool_call_progress(index, view)
                              ▼             ▼
                    StreamPresenter.render(累积args) ──▶ Delta::ToolProgress
                    （args 流式：write/edit）

工具执行（spawn 链）──exec_streaming──▶ progress channel ──▶ sink
                    （result 流式：shell 输出，§4.1）
```

- `StreamSink::on_tool_call_progress(index, view)`：统一的 live 快照回调，两种来源——
  args 流式（`StreamPresenter`，执行前，`write`/`edit`）与 result 流式（`shell` 输出，
  执行中，经 progress channel，§4.1）。`view` 都是与阶段三同构的 TextView 信封。
- `Delta::ToolProgress { index, view }`：SSE 事件，`ts-rs` 导出到前端。
- `BroadcastSink::on_tool_call_progress`：把快照广播出去。

骨架期的 `on_tool_call_delta` / `Delta::ToolArgs`（原始 args 透传 + 前端裸显）已在
**Phase 6 删除**——live 通道只载 render-ready 的 view 快照，完整 args 随
阶段三的 ContentBlock 进 debug 折层。

## 4. `StreamPresenter` 与 `stream_args`（Phase 2 已落地）

流式能力按工具逐个接入，框架目标是**新增工具的默认成本为零**。`Tool` trait 加了
`stream_presenter()`（默认 `None` = 无流式，自动正确），`ToolRegistry::stream_presenter(name)`
按名查找：

快照契约（精确签名见 [`src/tool/mod.rs`](../src/tool/mod.rs)）：`Tool::stream_presenter()` 默认返回 `None`（新工具零成本获得正确行为）；`StreamPresenter::render(accumulated_args)` 输入是【完整累积 args】（非 delta），输出是与阶段三同构的 TextView 信封，在节流下被调用、绝不逐 token，返回 `None` 表示此刻还无法渲染（调用方保留上一帧）。

**已实现的组件**：

- `src/tool/stream_args.rs` — `PartialArgs`：**面向已知 schema 的增量提取器**（非通用
  partial-JSON parser）。回答两个问题：`complete_string(field)`（字段已闭合的值，用于
  `path` 这类必须先等到的字段）；`streaming_string(field)`（字符串字段的已接收前缀，用于
  `content` 这类生长载荷）。容错于任意字节截断（未闭合字符串、半截 `\u` 转义都安全丢弃）。
  评估过 `serde_json` 的 `StreamDeserializer` 与现成 crate，均不匹配「半个 JSON 值」需求，故自研。
- `src/tool/write_stream.rs` — `WriteStreamPresenter`：`path` 闭合后读一次旧文件并缓存；
  新文件生长 `code` 视图、覆盖场景对**截断到前缀行数**的旧文件 diff（避免把未到达的尾部
  误显为删除，且只 diff 完整行——末行可能被流式截断）。
- `src/tool/edit_stream.rs` — `EditStreamPresenter`（Phase 4，**累积渲染**）：每个 entry 闭合
  就用 `find_matches` 对原始文件定位、把 splices **加入**按绝对路径分组的累积集合（首现序）；
  正在流的 entry 每帧贡献一个随 `new` 生长的临时 splice。因所有 entry 与 `plan_path` 一样锚定
  同一原始快照，累积 view 在 args 闭合时**逐字节收敛于**阶段三的 `plan_view`（文件流式期间
  不变的前提下；阶段三仍对活文件重算，外部改动不会出错）。`A B A` 合并为两组（A 含两 entry、
  B），不会重复出现 A。多 entry 单调累积、不闪没。乐观渲染：ambiguous 取第一个匹配、重叠
  照渲染——`not_found`/`ambiguous`/`overlapping_edits` 由阶段三权威报告。
- `src/agent/collector.rs` — 节流驱动：`ToolCallDelta` 累积后在节流下调用 presenter
  （`PROGRESS_MIN_INTERVAL` 120ms + `PROGRESS_MIN_GROWTH` 64B），`BlockStop` 前强制 flush
  最后一帧。presenter 经 `collect_round(.., tools: Option<&ToolRegistry>)` 注入；无 registry
  （测试/headless）则 presenter 从不挂载，行为同骨架期。

每个后续工具的流式方案写在它自己的源文件头部注释里（`shell.rs` 等），实现时对照即可。

## 4.1 结果流式（Phase 5，与 args 流式不同的通道）

`write`/`edit` 流式的是 **args**（模型生成输入的过程，在 collector、执行之前）；`shell` 流式的是
**result**（命令运行的 stdout/stderr，在工具执行**期间**、`Sandbox::exec` 内部）。两者共用
`Delta::ToolProgress`/`view` 快照协议与前端同一条渲染路径，但**产生位置与通道不同**：

- `src/tool/terminal.rs` — 最小终端屏幕模型。shell 命令不一定是增长输出：进度条（`\r` 覆盖）、
  spinner、全屏面板（ANSI 光标/清屏）会**原地刷新**。流式 `terminal` view 渲染「当前屏幕」
  自包含快照，而非字节拼接——普通命令表现为增长，面板命令表现为原地刷新。处理 `\n`/`\r`/
  常见 ANSI（清屏、光标、行覆盖）；全屏 TUI 的复杂寻址降级为「大致可读」，不做像素级还原。
- `src/sandbox/mod.rs` + `passthrough.rs` — `Sandbox::exec_streaming(command, timeout, on_output)`：
  边读管道边调 `on_output(chunk)`。默认实现忽略回调走 `exec`（`BoxLite` 自动退化为只在结束时
  输出，TODO 接流式）；Passthrough 用 reader task 把两管道 chunk 经 mpsc 合并转发。`exec` 复用
  `exec_streaming`（丢回调）。
- `src/tool/shell.rs` — `OutputStream`：有 `progress` 时用 `exec_streaming`，字节喂终端模型、
  节流（120ms）渲染 `terminal` view；无则走原 `exec`。
- `ToolInput.progress: Option<Box<dyn FnMut(String)+Send>>` — 工具执行内部到前端的进度回调。
  与 `stream_presenter`（流 args，执行前）区分：这个流 result（执行中）。
- **progress channel**（`src/agent/mod.rs`）：并发派发路径根本拿不到 `&mut sink`（spawn 的任务
  无法共享可变借用），故加 `progress_tx/rx: UnboundedChannel<(block_index, view)>`——与既有
  `verdict_tx` 同构。三条并发路径的 progress 闭包把 `(index, view)` 发到 channel；写回 select!
  循环加一支 `progress_rx.recv()` 调 `sink.on_tool_call_progress(index, view)`。block index 由
  collector 的 `RoundOutcome::tool_call_block_index`（call.id → index）提供。
- **仅并发路径生效**：串行派发路径（审批门 `supports_concurrent_requests()=false`，即
  headless/eval）`progress=None`——该场景无人看流式，且保持「一套 progress 机制」而非两套。

## 5. 阶段计划（拆 session 执行）

| Phase | 内容 | 状态 |
|---|---|---|
| **1** | **协议骨架**（本改动）：`on_tool_call_progress` + `Delta::ToolProgress` + 前端 `case 'tool_progress'` + 本文档。行为零变化。 | ✅ 本次 |
| **2** | **`stream_args` 提取器 + write presenter**：渐进 view（新文件生长 code 视图、覆盖场景对截断旧文件 diff），节流在 collector。第一个端到端可用的流式工具。 | ✅ 本次 |
| **3** | **审批与 view 解耦**：删除整个 preview 机制（`Permission::Requested.preview`、`Tool::preview()`、前端 `Item.preview`）。审批不再自算自存 diff，人在卡片已有的阶段二 view 上审批。 | ✅ 本次 |
| **4** | **edit presenter（累积渲染）**：entry 闭合即对原文件定位并按 abs_path 分组累积 splices，活跃 entry 随 new 生长；`A B A` 合并为一组；累积收敛 ≡ 阶段三 `plan_view`。 | ✅ 本次 |
| **5** | **shell 输出流式**：结果流式（非 args 流式）。终端模型渲染「当前屏幕」快照（面板命令原地刷新）；progress channel 复用 verdict_tx 模式；仅并发派发路径生效。 | ✅ 本次 |
| **6** | **删除 `Delta::ToolArgs` / `on_tool_call_delta`**：移除原始 args 透传与前端裸显路径，args 改由阶段三 debug 折层一次性给出。 | ✅ 本次 |
| **7** | **收尾**：read/find 确认无需流式；MCP/未知工具维持默认无流式（渲染形态未知，不做字段级兜底——那是被 Phase 6 删除的裸显 args 的变体）；清理骨架期注释与 TUI 残余。 | ✅ 本次 |

每个 Phase 独立可合、可回滚；Phase 2 落地前框架本身不引入任何行为变化。

**MCP/未知工具的流式决策**：不做通用兜底。其渲染形态未知——`stream_presenter` 默认 `None`
（骨架卡片 → 阶段三 result + summary）就是正确行为。所谓「字段级进度」（逐字段推 `key: 摘要`）
本质是 Phase 6 刚删除的「裸显 args」的变体，与「渲染有意义的 view」的方向相悖。若未来出现
「某 MCP 工具 args 巨大、流式有明确价值」的具体案例，再为它单独设计（那时有真实 schema 与
需求）。`read`/`find` 的 args 极小（`{path,range?}`/`{pattern}`），瞬间完整，同样无需流式。

## 6. 不变量

- **view 永不进模型上下文**：与本文档 §2 阶段三同一边界。`ToolProgress` 是 live
  delta（不持久化、不回放、不进 `render_output`），天然满足。
- **快照自包含**：任何一帧 `ToolProgress` 都可独立渲染，丢弃任意前序帧不影响正确性。
- **阶段二 ≡ 阶段三同构**：前端用同一个 `view` 字段、同一个渲染组件贯穿流式与定稿。
- **节流在后端**：前端不感知帧率，极端 token 突发也不会逐字节渲染。
