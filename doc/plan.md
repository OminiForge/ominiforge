# Plan 系统（turn 内执行规划）

代码：plan 状态与操作见 [`src/agent/plan.rs`](../src/agent/plan.rs)（`PlanStep`、`StepStatus`、
`PlanOp`、`apply_plan_op`、`render`、`descriptor`）；`SessionRuntime` / `TurnState` 门类、
完成度门、卡死检测见 [`src/agent/mod.rs`](../src/agent/mod.rs)。本文讲设计意图与边界，结构体
与控制逻辑细节以代码及其注释为准。

## 1. 定位与边界

Plan 是 **turn 内**的执行规划机制，把一个较长目标拆成有序步骤，让 agent 在多 round 推进中
不迷失方向。它属于架构 `Core Agent Layer` 中 `planning / execution policy` 的落地。

**必须和 Phase 4 的 Task 系统切清楚：**

| 维度 | Plan（本文） | Task（Phase 4，`todo.md §13`） |
|------|--------------|-------------------------------|
| 生命周期 | turn 内，turn 结束即销毁 | 跨 session，持久化 |
| 作用 | agent 自我规划、防跑偏 | 组织层任务管理 |
| 校验 | 无 reviewer | reviewer agent 验证交付 |
| 状态机 | step 五态 | backlog→running→pending_review→delivered |
| 存储 | SessionRuntime 内存 + events.jsonl | 任务库 + workspace 展示 |

Plan 不负责跨 context 的拆分。当任务大到单个 context 装不下时，那是 subagent（Phase 4/5）的
边界，不是 Plan 的职责。

## 2. 设计原则

- Plan tool 只管理计划状态，**不执行任何动作**（不读文件、不跑命令）。planning 与 doing 分离。
- Plan 状态权威副本存于 `SessionRuntime`（内存，跨 turn 存活），事实记录仍是 events.jsonl 中
  的 `ToolEvent`，不违反历史不可变。
- 操作式（op-based）增量更新，不整表替换：便于审计、便于 TurnState 维护、省 token。
- 使用规范写在 tool descriptor 的 `description` 里，不写进 profile 的 system prompt。
- 所有 step 必须到达终态才能结束 turn（见 §6）。
- 系统注入文本用 `<reminder>...</reminder>` 包裹，与用户输入区分。

## 3. TurnState：turn 运行时状态的统一门类

agent loop 的运行时状态收编成单一门类，loop 逻辑成为它的方法，消除参数层层透传。状态按
生命周期分四类，**核心判据是"是否跨 turn"**：

| 类别 | 生命周期 | 归属 | 内容 |
|------|---------|------|------|
| turn-invariant | 整个 agent 存活期 | `Agent`（不变） | provider、tools registry、config |
| **session-scoped** | **跨 turn，会话级** | **`SessionRuntime`（调用方持有）** | **context（对话视图）、plan、ledger** |
| turn-scoped | 单个 turn | `TurnState`（借用 runtime） | turn_id、round、gate_count、step_stuck_rounds |
| round-ephemeral | 单次 model round / tool call | 方法内局部 | request_id、计时、parent EventId、解析后 args |

关键判断：**plan 是 session-scoped，不是 turn-scoped**。一个 `blocked` 步骤的本质是"挂起、
等用户介入"，天然跨 turn——用户下个 turn 补充信息时，agent 必须仍记得 plan 卡在哪、为什么。
若随 turn 销毁，blocked 状态就失去意义。因此 plan 与 context 同级放进 `SessionRuntime`；只有
控制计数器（round、gate_count、step_stuck_rounds）是真正 turn-scoped，每 turn 清零。

`Agent` 退化为 turn-invariant 依赖容器 + turn 入口。后续会话级状态（skill 加载缓存、memory
注入缓存）加进 `SessionRuntime`，后续 turn 级控制状态加进 `TurnState`——各有明确的家。

## 4. Step 语义

五态：非终态 `Pending` / `InProgress`；终态 `Completed` / `Cancelled` / `Blocked`。

模型偷懒（把难做的 step 直接取消）是真实风险，必须严格区分两种合法停止：

- `Cancelled` = 步骤**本身**客观不可达（调用了不存在的 tool、写入无权限路径）。
- `Blocked` = 步骤**可达但前提缺失**，需用户补全（缺 API key、需设环境变量、需用户决策）。

两者都要求 `reason` 且必须具体。tool descriptor 明确声明：**不允许因"太难"或"不想做"而
cancel/block，只有客观障碍才合法**。`Blocked` 让 turn 能在"需要用户介入"时干净结束并把原因
带给前端，而不是死循环或假装完成。

## 5. Plan Tool（操作式）

单个 built-in tool（名 `plan`），通过 `op` 字段区分操作：`init` / `start` / `complete` /
`cancel`(reason) / `block`(reason) / `add`(after_id?)。每次操作返回当前完整清单的渲染作为
tool result，模型始终看得到最新状态。非法输入（schema 错、缺 reason、id 不存在）走现有 tool
error 回路，模型下一 round 自行改正。

### 与叶子工具的区别：loop 拦截

`plan` 是首个**操作 agent 自身状态**的控制工具，read/write/shell 是纯 I/O 叶子工具。叶子
工具的 `Tool::invoke` 拿不到 TurnState（也不应该）。落地方式：

- `plan` 不进 `ToolRegistry`（registry 只放叶子工具）。其 descriptor 由 `TurnState` 在组装
  tool schemas 时直接贡献，与叶子工具一同广播给模型。
- agent loop 在 dispatch 时**按工具名拦截** `plan`：不走 `tool.invoke()`，改调 `apply_plan_op`
  把变更写进 `TurnState.plan`，再渲染当前清单回灌。
- 事件一致性：plan 调用照常用 `ToolEvent::Started` / `Completed` 包裹（`ToolSource::Builtin`），
  与叶子工具在事件流里同构，replay/monitor 无需特例。

后续 `load_skill` / `spawn_subagent` 同属控制工具，沿用同一模式：贡献 descriptor + loop
拦截 + 操作 TurnState，不进叶子 registry。

## 6. Turn 退出条件：完成度门（completion gate）

**核心规则：plan 存在时，所有 step 必须到达终态才能结束 turn。** `tool_calls.is_empty()`
不再充分。模型停止调用工具时，门检查是否还有非终态（pending/in_progress）step：

- 无 plan，或全部终态 → 干净退出。
- 仍有非终态 → 注入一条 `<reminder>` 列出未完成步骤，继续下一 round（不退出）。
- 连续注入达 `MAX_GATE`（=2）仍无 tool call 且仍有非终态 → 模型卡住或误用 plan，发
  `TurnEvent::Failed(retryable=true)` 退出，交调用方处理。既不静默放行也不死循环。

模型的合法退出路径：(1) 无 plan（琐碎任务）；(2) 所有 step 终态。

## 7. 早期卡死检测（替代 max_rounds 兜底）

长任务的 `max_rounds` 可能很大，用它兜底浪费 token。`max_rounds` 保留为绝对安全网，但**主要
机制是步骤级卡死检测**：某 `in_progress` step 连续 `STUCK_THRESHOLD`（如 5）round 无进展时，
注入一次性提醒建议取消或重构计划；step 被 complete/cancel/block 时重置其计数器。"早发现"是
语义级别（某步推进停滞），不是 token 耗尽级别。

## 8. 注入与缓存纪律

所有系统注入（完成度门、卡死警告）遵循 [`context-management.md`](./context-management.md) §5：

- 用 `<reminder>...</reminder>` 包裹，文本英文，与用户输入区分。
- 作为真实消息**永久留在 context 历史中不移除**（保 prefix cache）。
- 同步写一条 `InjectionEvent`（`InjectionSource::Runtime`）到 events.jsonl，replay 可还原
  模型每轮所见。

## 9. Tool Descriptor 中的使用规范

behavioral guidance 写进 `plan` tool 的 `description`（英文），不进 profile system prompt：
复杂任务先 `init` 再逐步推进，琐碎单步任务无需建 plan；每步开始 `start`、完成 `complete`；
仅客观不可达时 `cancel`、需用户介入时 `block`（均须具体说明）；禁止因困难而 cancel/block；
所有步骤须到达终态方可结束任务。

## 10. 多 turn 行为与恢复

plan 是 session-scoped，跨 turn 存活。turn 结束只销毁 `TurnState`（turn_id、round、计数器），
plan 与 context 留在 `SessionRuntime`。

- **blocked 步骤跨 turn 延续**（plan 必须跨 turn 的核心理由）：turn 因 `blocked` 干净结束并把
  原因带给前端。下个 turn 模型同时看到用户新输入 + 当前 plan 状态，自行判断：用户在回应
  blocked → 把对应步骤 `start` 改回继续；用户开了新话题 → `init` 覆盖或 `cancel` 旧步骤。
  完成度门只看非终态，`blocked` 是终态不触发门——所以"挂起等用户"能干净交还控制权又不丢状态。
- **陈旧 plan 不纠缠新话题**：全终态的 plan 不触发门，模型在新 turn 可无视或 `init` 覆盖。
  模型始终能看到 plan 现状并自主决定去留，不被旧计划绑架。
- **恢复（resume）**：`SessionRuntime` 是内存结构，进程退出即失，但每个 plan op 都已写入
  events.jsonl。resume 时按时间序回放 plan op 依次 `apply_plan_op`，确定性重建当前清单——
  符合"events 是 source of truth，内存结构可重建"。Phase 1 单 turn CLI 每次新建空 runtime；
  多 turn 延续与 resume 重建在 Phase 2 落地，接口现在就立好，单 turn 是其退化情形。

## 11. 不在 MVP 中

- 中途 nag（连续 N round 没碰 plan 就提醒）：完成度门 + 卡死检测已覆盖防跑偏诉求。
- 完成度驱动的 verification nudge（"快做完了，该自检/跑测试"）：留待接入 reviewer 阶段。
- first-class `PlanEvent`：`ToolEvent::Started.input` 已落完整 plan 操作，replay/monitor 可还原。
- 跨 turn 权威 plan 状态。
</content>
