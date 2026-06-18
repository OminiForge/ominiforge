# Plan 系统（turn 内执行规划）

## 1. 定位与边界

Plan 是 **turn 内**的执行规划机制，用于把一个较长的目标拆成有序步骤，让 agent 在多 round 推进中不迷失方向、不忘记自己要做什么。

它属于架构 `Core Agent Layer` 中的 `planning / execution policy` 一项的落地。

**必须和 Phase 4 的 Task 系统切清楚：**

| 维度 | Plan（本文） | Task（Phase 4，`todo.md §13`） |
|------|--------------|-------------------------------|
| 生命周期 | turn 内，turn 结束即销毁 | 跨 session，持久化 |
| 作用 | agent 自我规划、防跑偏 | 组织层任务管理 |
| 校验 | 无 reviewer | reviewer agent 验证交付 |
| 状态机 | step 五态 | backlog→running→pending_review→delivered |
| 存储 | SessionRuntime 内存 + events.jsonl | 任务库 + workspace 展示 |

Plan 不负责跨 context 的拆分。当任务大到单个 context 装不下时，那是 subagent（Phase 4/5）的边界，不是 Plan 的职责。

## 2. 设计原则

- Plan tool 只管理计划状态，**不执行任何动作**（不读文件、不跑命令）。planning 与 doing 分离。
- Plan 状态的权威副本存于 `SessionRuntime`（内存，跨 turn 存活），事实记录仍是 events.jsonl 中的 `ToolEvent`，不违反历史不可变。
- 操作式（op-based）增量更新，不整表替换：便于审计变更、便于 TurnState 维护、省 token。
- 使用规范写在 tool descriptor 的 `description` 里，不写进 profile 的 system prompt。tool 的用法归 tool 管。
- 所有 step 必须到达终态才能结束 turn（见 §6）。
- 系统注入文本用 `<reminder>...</reminder>` 包裹，与用户输入区分。

## 3. TurnState：turn 运行时状态的统一门类

当前 agent loop 的运行时状态全部散落为 `run_turn_with_sink` / `run_model_round` / `dispatch_tool` 的局部变量与自由函数参数：turn_id 层层透传、`fail_tool` 八个参数、round 计数器、各处现构造 source。TurnState 的目标不是"再加一个 plan 字段"，而是把**一个 turn 生命周期内的全部可变状态收编成单一门类**，loop 逻辑成为它的方法。

### 3.1 状态分类

把现状所有状态按生命周期分四类，明确归属。**核心判据是"是否跨 turn"**：

| 类别 | 生命周期 | 归属 | 内容 |
|------|---------|------|------|
| turn-invariant | 整个 agent 存活期 | `Agent`（不变） | provider、tools registry、config(model/temperature/max_tokens/tool_timeout/max_rounds) |
| **session-scoped** | **跨 turn，会话级** | **`SessionRuntime`（调用方持有）** | **context（对话视图）、plan（工作计划）** |
| turn-scoped | 单个 turn | `TurnState`（借用 runtime） | turn_id、round、gate_count、step_stuck_rounds |
| round-ephemeral | 单次 model round / tool call | 方法内局部 | request_id、请求计时 Instant、tool 计时、parent EventId、解析后的 args |

关键修正：**plan 是 session-scoped，不是 turn-scoped**。一个 `blocked` 步骤的本质是"挂起、等用户介入"，天然跨 turn——用户下个 turn 补充信息时，agent 必须仍记得 plan 卡在哪、为什么。若随 turn 销毁，blocked 状态就失去意义。

因此 plan 与 context 同级，都放进会话级的 `SessionRuntime`。只有**控制计数器**（round、gate_count、step_stuck_rounds）是真正 turn-scoped——每 turn 清零本就正确，它们都是单 turn 内的瞬时控制量。

### 3.2 结构

```rust
/// Session-scoped runtime state that survives across turns. Owned by the
/// interactive loop / CLI, borrowed by each TurnState. Rebuilt from
/// events.jsonl when resuming a session.
pub struct SessionRuntime {
    /// Conversation view sent to the model; appended each turn.
    pub context: Vec<Message>,
    /// Working plan; survives across turns until every step reaches a terminal
    /// state or the model replaces it via `init`.
    pub plan: Vec<PlanStep>,
}

/// All mutable state threaded through one turn of the agent loop.
///
/// Constructed when a turn starts, dropped when it ends. Owns the turn-scoped
/// counters and output accumulation, and borrows the session-scoped runtime plus
/// the shared resources the turn drives. Turn-invariant deps stay on `Agent`;
/// round-ephemeral values stay local to the round.
struct TurnState<'a> {
    // turn-invariant deps (provider, tools, config)
    agent: &'a Agent,
    // session-scoped state, borrowed for the turn (context + plan live here)
    runtime: &'a mut SessionRuntime,
    // shared resources, borrowed for the turn's duration
    writer: &'a mut SessionWriter,
    sink: &'a mut dyn StreamSink,

    // turn identity
    turn_id: TurnId,

    // turn output accumulation — updated each round, consumed by TurnOutcome on exit
    round: u32,
    answer: String,
    stop_reason: StopReason,
    accumulated_usage: Usage,

    // turn-scoped plan control counters, reset every turn
    gate_count: u8,
    step_stuck_rounds: HashMap<String, u32>,
}
```

`plan` 通过 `self.runtime.plan` 访问；`context` 通过 `self.runtime.context`。turn 结束 `TurnState` 销毁，但 `SessionRuntime`（含 plan + context）仍在调用方手中，下个 turn 接着用。

每轮结束时 `TurnState` 更新 `stop_reason`、`accumulated_usage`（累加）、`answer`（最后一次产文本的轮）。`TurnState::run()` 最终由内部状态构造 `TurnOutcome`，调用方不再自行组装。

### 3.3 loop 逻辑收编为方法

现有自由函数 / `Agent` 方法迁移为 `TurnState` 方法，消除参数透传：

- `Agent::run_turn_with_sink` 只负责构造 `TurnState`（绑定一个 `&mut SessionRuntime`）并调用 `turn.run(input)`。
- `run_model_round` / `dispatch_tool` / `fail_tool` → `TurnState` 方法，turn_id、writer、source 经 `self` 取得，不再层层传。
- `runtime_source()` / `model_source()` → `self.*`。
- 新增 `apply_plan_op` / `completion_gate` / `check_stuck` 均为 `self` 方法。

`Agent` 退化为 turn-invariant 依赖容器 + turn 入口。离散状态被根除；后续会话级状态（skill 加载缓存、memory 注入缓存等）加进 `SessionRuntime`，后续 turn 级控制状态加进 `TurnState`——各有明确的家。

## 4. Step Schema

```rust
struct PlanStep {
    id: String,             // 稳定 id，runtime 在 init 时自动分配："1","2","3",...
    content: String,
    status: StepStatus,
    reason: Option<String>, // cancelled / blocked 必填，其他可选
}

enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,  // 客观不可达：工具不存在、无权限等。reason 必填且须具体
    Blocked,    // 需用户介入：缺 env var、需配置、需决策。reason 必填
}
```

**终态**：`Completed` / `Cancelled` / `Blocked`。
**非终态**：`Pending` / `InProgress`。

### Cancelled 与 Blocked 的边界

模型偷懒（把难做的 step 直接取消）是真实风险，必须严格区分两种合法停止：

- `Cancelled` = 步骤**本身**客观不可达。例：调用了不存在的 tool、写入无权限的路径。
- `Blocked` = 步骤**可达但前提缺失**，需要用户补全。例：缺 API key、需设置环境变量、需运行某外部命令、需用户决策。

两者都要求 `reason`，且 tool descriptor 明确声明：**不允许因"太难"或"不想做"而 cancel/block，只有客观障碍才合法**。`reason` 必须具体。

`Blocked` 的存在让 turn 能在"需要用户介入"时干净结束并把原因带给前端，而不是死循环或假装完成。

## 5. Plan Tool（操作式）

单个 built-in tool（名 `plan`），通过 `op` 字段区分操作：

| op | 参数 | 说明 |
|----|------|------|
| `init` | `steps: [{content}]` | 建立计划，id 由 runtime 分配；已有 plan 时重置 |
| `start` | `id` | 标为 `in_progress` |
| `complete` | `id` | 标为 `completed` |
| `cancel` | `id, reason` | 标为 `cancelled`，reason 必填 |
| `block` | `id, reason` | 标为 `blocked`，reason 必填 |
| `add` | `content, after_id?` | 末尾（或指定步骤后）插入新步骤，status=pending |

- 每次操作返回**当前完整清单的渲染**作为 tool result，模型始终看得到最新状态。
- 非法输入（schema 错、cancel/block 缺 reason、id 不存在）走现有 tool error 回路：返回 `is_error` 的 ToolOutput，模型下一 round 自行改正。

### 与叶子工具的区别：loop 拦截

`plan` 是首个**操作 agent 自身状态**的控制工具，read/write/shell 是纯 I/O 叶子工具。叶子工具的 `Tool::invoke` 拿不到 TurnState（也不应该）。

落地方式：
- `plan` 不进 `ToolRegistry`（registry 只放叶子工具）。它的 descriptor 由 `TurnState` 在组装 `tool_schemas()` 时直接贡献，与叶子工具的 schema 一同广播给模型。
- agent loop 在 dispatch 时**按工具名拦截** `plan`：不走 `tool.invoke()`，改调 `self.apply_plan_op(args)` 把变更写进 `TurnState.plan`，再渲染当前清单作为 tool result 回灌。
- 事件一致性：plan 调用照常用 `ToolEvent::Started` / `ToolEvent::Completed` 包裹（`ToolSource::Builtin`），与叶子工具在事件流里同构，replay/monitor 无需特例。

这是"控制工具 vs 叶子工具"区分的落地处。后续 `load_skill` / `spawn_subagent` 同属控制工具，沿用同一模式：贡献 descriptor + loop 拦截 + 操作 TurnState，不进叶子 registry。

## 6. Turn 退出条件：完成度门（completion gate）

**核心规则：plan 存在时，所有 step 必须到达终态才能结束 turn。**

当前退出条件 `tool_calls.is_empty()` 不再充分。新逻辑：

```rust
// inside a TurnState method (`self`): plan/context live in self.runtime
if tool_calls.is_empty() {
    let incomplete = self.runtime.plan.iter()
        .filter(|s| matches!(s.status, Pending | InProgress))
        .collect::<Vec<_>>();

    if incomplete.is_empty() {
        // no plan, or every step terminal -> clean exit
        return Ok(TurnOutcome { ... });
    }

    if self.gate_count >= MAX_GATE {  // MAX_GATE = 2
        // model keeps not responding -> record TurnEvent::Failed(retryable=true), exit
        break;
    }

    // inject a reminder, continue to the next round (do not exit)
    self.runtime.context.push(Message::User {
        content: format!(
            "<reminder>The following plan steps are not in a terminal state. \
             Continue working on them, or mark them cancelled/blocked with a \
             reason, then give your final answer:\n{}</reminder>",
            render_incomplete(&incomplete)
        ),
    });
    // also write InjectionEvent(InjectionSource::Runtime) to events.jsonl
    self.gate_count += 1;
    continue;
}
```

- `MAX_GATE = 2`：两次注入后模型仍无 tool call 且仍有非终态 step → 模型卡住或误用 plan，发 `TurnEvent::Failed(retryable=true)` 退出，交调用方处理。既不静默放行也不死循环。
- 模型的合法退出路径：(1) 无 plan（琐碎任务）；(2) 所有 step 终态。

## 7. 早期卡死检测（替代 max_rounds 兜底）

后续会有长任务，`max_rounds` 可能设得很大。用它做兜底会浪费 token，应在早期语义层面发现不可行就停。

`max_rounds` 保留为绝对安全网，但**主要机制是步骤级卡死检测**：

```rust
// inside a TurnState method (`self`), at the end of each round
for step in self.runtime.plan.iter().filter(|s| s.status == InProgress) {
    let stuck = self.step_stuck_rounds.entry(step.id.clone()).or_insert(0);
    *stuck += 1;
    if *stuck == STUCK_THRESHOLD {  // e.g. 5
        // inject a one-shot stuck warning
        self.runtime.context.push(Message::User {
            content: format!(
                "<reminder>Step \"{}\" has been in progress for {} rounds \
                 without progress. Consider cancelling it or restructuring \
                 the plan.</reminder>",
                step.content, STUCK_THRESHOLD
            ),
        });
        // also write InjectionEvent(InjectionSource::Runtime)
    }
}
// reset a step's counter when complete/cancel/block is applied to it
```

"早发现"是语义级别（某步推进停滞），不是 token 耗尽级别。

## 8. 注入与缓存纪律

所有系统注入（完成度门、卡死警告）遵循 `context-management.md §5`：

- 用 `<reminder>...</reminder>` 包裹，文本英文，与用户输入区分。
- 作为真实消息**永久留在 context 历史中不移除**（保 prefix cache）。
- 同步写一条 `InjectionEvent` 到 events.jsonl，replay 可还原模型每轮所见。
- 需给 `InjectionSource` 追加 `Runtime` 变体（append-compatible，需改 `event-schema.md §9`）。

## 9. Tool Descriptor 中的使用规范

behavioral guidance 写进 `plan` tool 的 `description`（英文），不进 profile system prompt。要点：

- 复杂任务先 `init` 建立清单，再逐步推进；琐碎单步任务无需建 plan。
- 每步开始前 `start`，完成后 `complete`。
- 仅在步骤客观不可达时 `cancel`（须具体说明原因）；需用户介入时 `block`（须说明用户要做什么）。
- 禁止因步骤困难而 cancel/block。
- 所有步骤须到达终态（completed/cancelled/blocked）方可结束任务。

## 10. 多 turn 行为与恢复

plan 是 session-scoped，存于 `SessionRuntime`，跨 turn 存活。turn 结束只销毁 `TurnState`（turn_id、round、计数器），plan 与 context 留在 `SessionRuntime` 中。

### 10.1 blocked 步骤跨 turn 延续

这是 plan 必须跨 turn 的核心理由。一个 turn 因 `blocked` 步骤停下（需用户介入：补 env var、做决策、跑外部命令），turn 干净结束并把原因带给前端。下个 turn：

- 模型同时看到**用户新输入** + **当前 plan 状态**（哪些 blocked、reason 是什么）。
- 模型自行判断：
  - 用户在回应 blocked → 把对应步骤 `start` 改回 in_progress，继续执行。
  - 用户开了新话题 → `init` 新计划覆盖，或把旧 blocked 步骤 `cancel`（说明方向已变）。

完成度门只看非终态（pending/in_progress），`blocked` 是终态，不触发门——所以"挂起等用户"能干净结束 turn、交还控制权，又不丢状态。这正是 turn-scoped 设计会丢失、而 session-scoped 能保住的信息。

### 10.2 陈旧 plan 不纠缠新话题

若上个 turn 正常完成，plan 要么全部 `completed`/`cancelled`（全终态），要么含 `blocked`。全终态的 plan 不触发完成度门，模型在新 turn 可无视或 `init` 覆盖。含 blocked 的 plan 按 10.1 处理。模型始终能看到 plan 现状并自主决定去留，不会被旧计划强制绑架。

### 10.3 恢复（resume）

`SessionRuntime` 是内存结构，进程退出即失。但每个 plan op 都已写入 events.jsonl 的 `ToolEvent`。resume 一个旧 session 时，回放 events 重建 `SessionRuntime`：

- `context`：从 model/tool 事件重建对话视图（与 context 模块的快照/回放机制一致）。
- `plan`：按时间序回放 plan 的 `ToolEvent::Started.input`（即每次 op），依次 `apply_plan_op` 到空 plan，确定性重建当前清单状态。

符合"events 是 source of truth，内存结构可重建"。

### 10.4 Phase 落地

当前 Phase 1 CLI 是单 turn 命令：每次 `run` 新建空 `SessionRuntime`，turn 结束即弃。多 turn 延续（同进程连续 turn 复用同一 `SessionRuntime`）和 resume 重建是 Phase 2（TUI / 交互循环）落地。但 `SessionRuntime` 这层接口现在就立好，单 turn 是其退化情形，不需要将来回填。

## 11. 不在 MVP 中

- 中途 nag（连续 N round 没碰 plan 就提醒）：完成度门 + 卡死检测已覆盖防跑偏诉求，观察效果后再决定是否加。
- 完成度驱动的 verification nudge（"快做完了，该自检/跑测试"）：留待接入 reviewer / verification 阶段。
- first-class `PlanEvent`：`ToolEvent::Started.input` 已落完整 plan 操作，replay/monitor 可还原。需要干净的"计划进度"语义信号时再 append。
- 跨 turn 权威 plan 状态。

## 12. 涉及的代码改动概览

- `core/payload.rs`：`InjectionSource` 追加 `Runtime` 变体。
- 新增 plan 类型与操作（建议 `agent/plan.rs`）：`PlanStep`、`StepStatus`、`PlanOp`、`apply_plan_op`、清单渲染、`plan` descriptor。纯状态、无 I/O，不实现 `Tool` trait、不进 `ToolRegistry`。
- 新增 `SessionRuntime`（建议 `agent/` 内）：会话级 `context` + `plan`，由调用方持有、跨 turn 存活。当前 `run_turn_with_sink` 的 `context: &mut Vec<Message>` 参数收进 `SessionRuntime`。
- `agent/mod.rs`：引入 `TurnState<'a>` 门类（借用 `&mut SessionRuntime`），把 `run_model_round` / `dispatch_tool` / `fail_tool` / source 构造收编为其方法；`run_turn_with_sink` 仅构造 TurnState 并 `turn.run(input)`。新增 plan 拦截、`completion_gate`、`check_stuck`、reminder 注入。
- `tool_schemas()`：合并叶子工具 descriptors 与 plan descriptor。
- `cli.rs`：`run` 每次新建空 `SessionRuntime`（单 turn 退化情形）。
- `doc/event-schema.md §9`：记录 `InjectionSource::Runtime`。
- 默认 profile / system prompt **不改**（plan 使用规范在 tool descriptor）。
- resume 重建（回放 plan op 重建 `SessionRuntime.plan`）留待 Phase 2，本次只立接口。

测试覆盖：plan op 应用与渲染、非法 op 回路、完成度门（未终态续跑 / MAX_GATE 退出）、卡死检测注入、cancelled/blocked 缺 reason 拒绝、含 plan 的多 round turn 端到端、blocked 步骤跨 turn 在 `SessionRuntime` 中留存。
