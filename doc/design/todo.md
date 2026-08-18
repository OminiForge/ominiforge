<!-- status: current -->
<!-- owner: @OminiForge -->

# Todo 系统

> **新架构定位**：Todo 是 turn 内工作清单，属 agent loop 插件的 planning/execution policy。组合运行时与拓展机制见 [`runtime-architecture.md`](./runtime-architecture.md)。

## 8. Todo 系统（turn 内工作清单）

Todo 是 **turn 内**的工作清单机制，把一个较长目标拆成有序步骤，让 agent 在多 round 推进中不迷失方向。

### 8.1 定位与边界

Todo 属于架构 `Core Agent Layer` 中 `planning / execution policy` 的落地。名字刻意叫 `todo` 而非 `plan`：它只是一份执行中的打勾清单，"规划"这个名字留给后续更大的系统。

**Todo vs Task（Phase 4）**：

| 维度 | Todo | Task（Phase 4） |
|------|------|-----------------|
| 生命周期 | turn 内，turn 结束即销毁 | 跨 session，持久化 |
| 作用 | agent 自我提醒、防跑偏 | 组织层任务管理 |
| 校验 | 无 reviewer | reviewer agent 验证交付 |
| 状态机 | step 五态 | backlog→running→pending_review→delivered |
| 存储 | SessionRuntime 内存 + events.jsonl | 任务库 + workspace 展示 |

Todo 不负责跨 context 的拆分。当任务大到单个 context 装不下时，那是 subagent（Phase 4/5）的边界，不是 Todo 的职责。

### 8.2 设计原则

- Todo tool 只管理清单状态，**不执行任何动作**（不读文件、不跑命令）。planning 与 doing 分离。
- Todo 状态权威副本存于 `SessionRuntime`（内存，跨 turn 存活），事实记录仍是 events.jsonl 中的 `ToolEvent`，不违反历史不可变。
- 操作式（op-based）增量更新，不整表替换：便于审计、便于 TurnState 维护、省 token。
- 使用规范写在 tool descriptor 的 `description` 里，不写进 profile 的 system prompt。
- 所有 step 必须到达终态才能结束 turn。
- 系统注入文本用 `<reminder>...</reminder>` 包裹，与用户输入区分。

### 8.3 Step 语义

五态：非终态 `Pending` / `InProgress`；终态 `Completed` / `Cancelled` / `Blocked`。

模型偷懒（把难做的 step 直接取消）是真实风险，必须严格区分两种合法停止：

- `Cancelled` = 步骤**本身**客观不可达（调用了不存在的 tool、写入无权限路径）。
- `Blocked` = 步骤**可达但前提缺失**，需用户补全（缺 API key、需设环境变量、需用户决策）。

两者都要求 `reason` 且必须具体。tool descriptor 明确声明：**不允许因"太难"或"不想做"而 cancel/block，只有客观障碍才合法**。

### 8.4 Turn 退出条件：完成度门（completion gate）

**核心规则：todo 清单存在时，所有 step 必须到达终态才能结束 turn。**

模型的合法退出路径：
1. 无清单（琐碎任务）
2. 所有 step 终态

### 8.5 早期卡死检测

某 `in_progress` step 连续多 round 无进展时，注入一次性提醒建议取消或重构清单。"早发现"是语义级别（某步推进停滞），不是 token 耗尽级别。

### 8.6 Round-Budget Reminder（软预算提醒）

每个 turn 和每个 todo step 运行在一个**软 round 预算**下。不同于 `max_rounds` 的绝对硬顶，软预算只**提醒**，不强制停止——模型自己决定是收尾、拆步骤，还是继续。

### 8.7 多 turn 行为与恢复

todo 是 session-scoped，跨 turn 存活。turn 结束只销毁 `TurnState`（turn_id、round、计数器），todo 与 context 留在 `SessionRuntime`。

- **blocked 步骤跨 turn 延续**（todo 必须跨 turn 的核心理由）：turn 因 `blocked` 干净结束并把原因带给前端。下个 turn 模型同时看到用户新输入 + 当前清单状态，自行判断继续还是覆盖。
- **恢复（resume）**：每个 todo op 都已写入 events.jsonl。resume 时按时间序回放 todo op，确定性重建当前清单——符合"events 是 source of truth，内存结构可重建"。

