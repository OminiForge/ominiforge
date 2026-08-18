<!-- status: current -->
<!-- owner: @OminiForge -->

# Tool Streaming：流式契约（零 UI 转向后的定论）

本文原是「后端预渲染流式 tool-call 渲染管线」的契约。零 UI 转向后该管线已**移除**，
本文改为记录**保留下来的流式契约**与**移除的理由**。

## 1. 保留的流式（门面协议必需品）

对前端/门面，agent 保留**文本流式**与**原始事件流**两条，仅此而已：

- **文本/思考流式**：模型的文本（`StreamSink::on_text`）与思考（`on_reasoning`）逐块
  推给门面。任何门面（ACP 编辑器 / IM / TUI）都要「看着 agent 边说边写」，这是硬需求。
- **原始事件流**：tool 的 `Started`/`Completed`/`Failed`、以及模型产生的 tool-call
  `ContentBlock`（含完整 args）照常落 `events.jsonl` 并经事件流下发。门面据此自行渲染。

## 2. 已移除：后端预渲染 view 层

零 UI 转向前，后端曾把 write/edit 的 args、`shell` 的 stdout **预先渲染成自包含的
`TextView` 快照**流式发给自带前端（`StreamPresenter`、`terminal.rs` 屏幕模型、
`stream_args.rs` 半截-JSON 提取、`edit_stream`/`write_stream` 累积 diff、
`Delta::ToolProgress`）。转向后**整套移除**，理由：

- **终端消费者已不存在**：它服务的是已删除的自带 SPA；新门面（ACP/IM/TUI）各自有
  渲染逻辑，不消费 ominiforge 私有的 view 格式。
- **与「核心不做展示」冲突**：后端替前端算 diff 是在核心/harness 里做展示，违背
  透明性原则（[`runtime-architecture.md`](./runtime-architecture.md) §1）。
- **ACP 协议根本不收渲染结果**：经 ACP 官方 schema（`agent-client-protocol-schema`
  crate）确认，其 `Diff` 内容只携带 `path` + `old_text` + `new_text` 两个**原始字符串**，
  diff 的计算与高亮由 **Client**（如 Zed）完成；`Terminal` 内容只携带 `terminal_id`，
  输出流由 Client 内嵌实时终端自显示。**Agent 没有、也不需要「发渲染好的 diff」的通道。**

移除范围：`StreamPresenter` trait、`Tool::stream_presenter`、`ToolInput.progress`、
collector 的流式渲染驱动、`shell` 的 OutputStream/exec_streaming 流式、actor 的
`Delta::ToolProgress`，及 `tool/{edit_stream,write_stream,stream_args,terminal}.rs`。

## 3. 不变量

- **模型只看简报**：工具的 `Tool::Completed` 结果对模型只暴露 `Content::Text` 简报
  （如 `edited src/lib.rs (1 replacement)`），模型从不见 diff/terminal 的呈现。
- **后端不产出任何渲染好的 view**：`Content` 只有 `Text` / `Image` / `ArtifactRef`，
  不存在 `TextView`。edit/write/shell 的 diff、terminal 呈现由门面根据原始事件流
  自行渲染（面向 ACP 的映射见 §4）。

## 4. 面向 ACP 的映射（未来门面实现时）

- 文本/思考流式 → `session/update` 的 `agent_message_chunk` / `agent_thought_chunk`。
- tool 进度/结果 → `tool_call` / `tool_call_update`（`status`/`content`）。
- write/edit 的改动 → `ToolCallContent::Diff { path, old_text, new_text }`（给原文，Client 算 diff）。
- shell 的输出 → `ToolCallContent::Terminal { terminal_id }`（配 `terminal/*` 能力，Client 实时显示）。

**ACP 的 wire 类型直接复用官方 `agent-client-protocol-schema` crate，不在 ominiforge
重新自定义**；core 内部事件类型（`src/core/payload.rs`）保持独立、不随 ACP 漂移，两者在
ACP facade 适配层做一次转换。详见 [`runtime-architecture.md`](./runtime-architecture.md) §7.2。
