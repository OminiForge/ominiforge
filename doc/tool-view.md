# Tool View：工具调用的前后端分工契约

定义 tool 调用结果在**模型上下文**与**用户界面**两条通道上的数据契约。核心原则：
**前端只做渲染与交互；一切复杂逻辑（diff 构建、内容缓存、状态重建）都在后端。**
前端对 tool 的了解收敛为「后端给它什么形状的 view，它就渲染什么」。

本文档是 `doc/tool-protocol.md` §11.4 的替代：旧方案（后端回简报、前端复刻匹配
算法自行构建 diff）废弃，理由见 §1。

## 1. 原则与旧方案的问题

- **前端 = 渲染 + 交互。** 它不重建文件状态、不复刻后端算法、不推导业务数据。
- **后端 = 全部复杂逻辑。** 它握着真实的 pre-edit 文件内容，只有它能产出**精确**的
  diff；前端用事件流残片重建的永远只是近似。
- **全过程透明。** tool call 的原始输入（args）与原始输出（model-facing result）
  始终可在卡片的 debug 折层中查看——view 是「更易读的呈现」，不是信息的替代品，
  用户随时能看到底下真实发生了什么。

旧方案（前端 `diff-builder.ts` + `fileCache` + `prevLines`）的具体问题：

1. **双实现必漂移**：前端复刻了 `edit.rs` 的匹配/拼接/hunk 算法（代码注释自己写明
   "Mirrors `find_matches` in `edit.rs`"），违反 `doc/frontend.md` §6「生成优于手写」。
2. **已知正确性缺陷**：连续 `edit` 中间无 `read` 时缓存陈旧、preview 取首个匹配、
   write 失败需手工回滚缓存——都是「前端拿不到真实文件」这一结构性事实的症状。
3. **复杂度前倾**：手写流式部分 JSON 解析器（`parsePartialEdits`）、文件缓存、
   快照传递、失败回滚，约 600 行前端代码只为近似后端一执行就知道的答案。

## 2. 事件契约：`Content::TextView`（audience = ui）

`view` 不挂在事件上，而是 `ToolOutput.content` 的一个新内容块——它是 tool 执行体
（握着真实 pre-edit 内容的那层）直接产出、随 result 一起持久化在事件日志里的：

```rust
pub enum Content {
    Text(String),                                  // 既有：进模型上下文
    TextView { text: String, audience: String },   // 新增：audience = "ui"，只给 UI
    Image { .. },
    ArtifactRef { .. },
}
```

- **模型上下文**：`render_output`（`src/agent/mod.rs`）只拼接 `Content::Text`，
  跳过 `TextView`。`Message::Tool`、replay、fork/compaction 的 context snapshot、
  fork-preview 全部派生自它，天然不含 view（§3 的不变量 + 回归测试）。
- **UI**：view 块随 `ToolEvent::Completed.result.content` 持久化，重连/刷新重放
  历史时 diff 不丢——这是选 content 块而非事件字段的关键原因（事件字段方案在
  旧日志上为 `None`，历史 diff 会整个消失）。
- **谁产出**：`edit`/`write` 在成功执行时 append 一个 `TextView` 块；其余 tool
  （read/find/shell/MCP）不产出，前端按原有方式渲染 result 本体。
- **向后兼容**：旧日志无此块，读回正常（前端无 view 就渲染 result）；旧前端遇到
  未知 content 变体按 `[binary]` 兜底——单向升级，新旧客户端互不破。
- **ts-rs**：`Content` 已在导出链路，前端 TS union 自动获得 `TextView` 变体，
  渲染分支 exhaustive 兜底。

### 语义约束（防滥用）

`TextView` 只允许是**同一调用 result 内容的另一种呈现**（diff / 高亮基底），不允许
承载 result 里没有的信息。理由：view 不进模型上下文、不在 debug 折层的「args +
result = 全过程」透明承诺内；一旦它开始承载独有信息，该承诺就破了。需要新信息 =
改 `Text` result（进上下文、可被模型用），不是塞进 view。

## 3. 渲染边界：view 永不到达模型

模型上下文的 tool 结果**只**由 `render_output(&result)` 生成（`src/agent/mod.rs`），
它对 content 块的取舍是：

| 块 | `render_output` | 消费者 |
|---|---|---|
| `Text` | 拼进文本 | LLM（+ UI debug 折层） |
| `TextView` | **跳过** | UI 主呈现 |
| `Image` / `ArtifactRef` | 占位符 `[image …]` / `[artifact …]` | LLM |

不变量由既有代码路径保证：

- `write_execution_result`（`src/agent/mod.rs`）：`Message::Tool` 只取
  `render_output(&output)`。
- `resume.rs` / `rebuild_runtime`：replay 同样只走 `render_output(result)`——
  fork / compaction / reconfiguration 的 context snapshot 与 fork-preview 端点都
  由此派生，天然不含 view。
- 新增回归测试：构造带 `TextView` 块的 `Completed` 事件 replay，断言 rebuild 出的
  `Message::Tool.content` 与无 view 时逐字相同。

## 4. 后端：edit/write 产出精确 view

diff 构建逻辑从「前端复刻」移回后端，直接长在 tool 已有的执行结果上：

- **edit**：定位阶段已算出每个 entry 的精确 splice（`find_matches` 的真身），在其上
  直接渲染 hunk——`stripCommon`、上下文 3 行、相邻 hunk 合并，即现
  `diff-builder.ts` 的算法，回到它和 `edit.rs` 同侧、共享匹配结果的位置。
  多文件 edit 的 view 拼接为各文件 unified-diff（带 `--- a/PATH` / `+++ b/PATH`
  头，沿用旧前端 `Diff.svelte` 已能解析的形态）。
- **write**：覆盖时新内容来自 args、旧内容来自真实文件（执行体此刻两者都有），
  行级 diff 用已有依赖 `similar`（`write_summary` 已在用，不新增 crate）；
  新建文件的 view 就是全文（前端按 code view 渲染，不走 diff）。
- **失败不产 view**：`is_error` 或 protocol error 时只有 `Text` 错误简报——
  失败结果本身就是全部信息，debug 折层可见。
- **大小**：view 与 result 同受 64KB artifact spill 约束（`doc/tool-protocol.md` §8），
  超大 diff 由 runtime spill 成 `ArtifactRef`，前端按引用取——与 result 同一机制。
- **TUI**：忽略 `TextView` 块（其 render 同样只挑 `Text`），行为不变；后续可选择
  消费 view 渲染 diff，非本次范围。

## 5. 前端：映射式渲染 + debug 折层

```
后端 ToolView（ts-rs 生成 TS union）
  → 一行映射：view.kind → 渲染组件（仿现有 tools/registry.ts，保持纯查表）
  → view: None → GenericResult（args + result，今日 MCP tool 的兜底形态）
```

- **EditResult / WriteResult** 从「args + fileCache 重建 diff」改为「渲染
  `view.lines`」。`Diff.svelte` 解析 unified-diff 文本的渲染器**保留**——它是纯
  呈现组件，只是输入从「前端构建」换成「后端给的 lines」。
- **ToolBlock 增加 debug 折层**（复用 `RawArgs.svelte`）：所有 tool 的卡片都可展开
  查看原始 args JSON + model-facing result 文本 + diagnostics。这兑现「全过程透明」：
  主呈现是 view，底层事实随时可查。
- **删除**：`diff-builder.ts`（+测试）、`conversation.ts` 的 `fileCache` /
  `prevLines` / `parseReadResult` / `cacheWriteArgs` / `writePrevLinesFor` / 失败回滚、
  `parsePartialEdits`、registry `ResultProps` 里的 `fileCache`/`prevLines` 传递。
  净删约 600 行。
- **行为变化（显式确认项）**：running 阶段不再渲染增量 diff 预览（此前靠
  `parsePartialEdits` 边收 args 边算）。running 卡片照常**实时显示流入的 args 文本**
  （过程透明不丢），`Completed` 落地时替换为后端精确 diff。换来的是 diff 永远精确：
  「缓存陈旧」「取首个匹配」「write 失败回滚」整类 bug 随机制一起删除。
- **stream delta**：`tool_args` delta 继续只喂 args 文本展示；diff 预览机制及其对
  delta 的消费随 `parsePartialEdits` 一起删除。

## 6. 其余前端逻辑的归属（本次只动 tool view）

按同一原则复核过、本次**不动**但记录在案：

- `conversation.ts` 的 apply/commitBlock/delta 折叠状态机：**保留在前端**。它是
  「事件流 → 可交互对话视图」的交互态管理（SSE 重连、流式增量、竞态折叠），属于
  交互层本职；DESIGN.md §7 的「不动状态机」约束继续有效。
- `permission-rules.ts` 的 `resolveEffective`：前端复刻三层权限解析，与 diff-builder
  同病（双实现）。后续应加 gateway resolved-permission 端点后删除，单独立项。
- `stats.ts` / `minimap.ts`：纯展示派生，符合分工。

## 7. 迁移步骤

1. `payload.rs`：`Content::TextView { text, audience }` 变体，`render_output` 跳过它，
   ts-rs 导出。验证：`cargo test --features ts-export`，提交生成的 TS。
2. `edit.rs`/`write.rs`：产出 view（edit 复用定位结果渲染 hunk；write 用已有
   `similar` 依赖做行级 diff）。验证：单测断言 diff 文本与旧 `diff-builder.test.ts`
   的等价 fixture 一致（fixture 先从前端测试搬进 Rust）。
3. `resume.rs`：回归测试断言 view 不进 model-facing 文本。验证：`cargo nextest run`。
4. 前端：view 组件映射 + debug 折层；删除 §5 删除清单。验证：`pnpm check` 0/0、
   `pnpm test`（conversation 状态机测试不受删改影响，diff-builder 测试随文件删除）。
5. 文档：重写 `doc/tool-protocol.md` §11.4 指向本文档；`doc/frontend.md` 低维护表
   加 view 契约一行；DESIGN.md §4.1 tool 条目改为「diff 由后端 view 提供」。
6. 端到端：起 gateway + dev server，edit/write/shell/MCP-fallback 四类卡片肉眼验。

## 8. 显式边界（不做的事）

- 不给 view 做 streaming（§5 行为变化，已确认接受）。
- 不改 `result` 的简报形态（模型 token 经济性不变）。
- 不动 plan 卡片的折叠逻辑（它是前端交互态，且 plan 不产生 view）。
- 不动 permission 的 `resolveEffective`（§6，单独立项）。
- 不改 `ToolEvent::Completed` 的事件形状（view 走 content 块，不加事件字段——见 §2 的旧日志/重放理由）。
- 不给 TUI 加 view 消费（向后兼容，后续可选）。
