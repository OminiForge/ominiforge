<!-- status: current -->
<!-- owner: @OminiForge -->

# Ominiforge 权限门控（Permission Gating）

本文档定义工具调用的权限门控系统：在工具执行前，由**代码**（而非模型）决定每次调用
allow / deny / ask。核心原则——安全不能靠信任模型，要靠代码。

> ⚠️ **威胁模型边界**：当前规则用**子串匹配**（§2）。它防的是"手滑/误触/模型无意中调用危险工具"，
> **不是**对抗一个主动规避的恶意模型——子串可被双空格、`base64`、命令分片、`printf` 拼接等平凡手段绕过，
> 且只搜 JSON value 不搜 key。把它当作"护栏"而非"防弹墙"。对抗性场景应叠加沙箱（`doc/sandbox.md`）
> 与网络策略，不要单靠门控。`prefix`/`field`/`negate` 已实现；glob/regex 是后续工作（§9）。

## 1. 设计原则

- 门控在 agent loop 的 `dispatch_tool` 里、`tool:invoke:before` hook 链**之后**、工具执行**之前**求值，
  评估的是 hook 可能改写过的最终输入。
- 三档行为：`allow`（直接执行）/ `deny`（阻断，回喂 model）/ `ask`（挂起等人工决定）。
- 策略是声明式规则表，纯逻辑、无 IO；空策略 = 全 allow（未配置的 profile 不受影响）。
- 所有门控决策写入 event log（`PermissionEvent`），用于审计与前端重建。
- `ask` 无 gate 时 fail-closed（拒绝），安全不依赖 gate 存在。

## 2. 策略模型

`PermissionPolicy`（精确字段见 [`src/permission/mod.rs`](../crates/ominiforge/src/permission/mod.rs)）持有**三个有序规则列表**：

- `deny`：命中即阻断，最高优先级。
- `allow`：命中且无 deny 命中 → 直接放行（固化的批准，§5.1），压过 ask。
- `ask`：命中且无 deny/allow 命中 → 需人工审批。

每条 `Rule` 以一个目标工具名（`"*"` 通配任意工具，否则精确匹配）加一组匹配模式表达；模式的匹配语义见 §3 规则模型。

**求值顺序（固定）**：`deny` 命中 → `Deny`；否则 `allow` 命中 → `Allow`；否则 `ask` 命中 → `Ask`；
否则 `Allow`。deny 永远压过 allow/ask（"已批准"与"请确认"都不能降级一条禁令）；allow 压过 ask——
它是作用域审批固化下来的规则（§5.1），命中即免问。

**匹配语义**：`contains` 中任一模式作为**子串**出现在输入 JSON 的**任意字符串值**里即命中
（递归数组/对象；只搜 value，不搜 key）。匹配模式含 `substring`/`prefix`（§3 规则模型），glob/regex 待后续。

## 3. 配置与三层解析

profile TOML 的 `[permission]` section 直接就是 `PermissionPolicy`（`doc/profile.md`）：

```toml
[[permission.deny]]
tool = "shell"
contains = ["rm -rf", "sudo"]

[[permission.deny]]
tool = "*"
contains = ["/etc/"]

[[permission.ask]]
tool = "write"          # 无 contains = 对 write 的任意调用都要审批

[[permission.ask]]
tool = "shell"
contains = ["curl", "wget"]
```

缺省 `[permission]` = 空策略 = 全 allow。

**规则模型（`Rule`）**：一条规则 = `tool` + 匹配条件。匹配条件字段（都可选，缺省即回退旧行为）：

| 字段 | 含义 | 缺省 |
|---|---|---|
| `tool` | 目标工具名；`"*"` = 任意工具 | 必填 |
| `contains` | 模式列表（磁盘字段名，代码里叫 `patterns`）。空 = 该工具的任意调用都匹配 | `[]` |
| `field` | 只测输入的这个顶层字段（如 `command`/`path`）；缺省 = 递归搜索全部字符串值（旧行为） | 全字段 |
| `mode` | `substring`（子串，缺省）/ `prefix`（前缀，用于路径目录白/黑名单） | `substring` |
| `negate` | `true` = **无**任一模式命中时才匹配。这是白名单的表达方式（"path 不以 src/ 等开头就 ask"）。空模式列表 + negate = 永不匹配（空白名单不锁死工具） | `false` |

`field`/`mode`/`negate` 是配置 UI 的规则行编译出来的结构化形态（§3.2）；手写 `contains = [...]` 老规则原样有效（`contains` 是 `patterns` 的磁盘别名，读写都用 `contains`）。

```toml
[[permission.deny]]
tool = "shell"
field = "command"           # 只测 command 字段,不会误伤其它字符串
contains = ["rm -rf", "sudo"]

[[permission.deny]]
tool = "read"
field = "path"
mode = "prefix"             # 路径前缀:禁读 /etc/ 下
contains = ["/etc/"]

[[permission.ask]]
tool = "write"
field = "path"
mode = "prefix"
negate = true               # 白名单:写入路径不在 src/ tmp/ 内则询问
contains = ["src/", "tmp/"]
```

### 3.1 三层解析（workspace > profile > gateway）

门控和 network 一样分三层，但**合并语义不同**——network 是「最具体层覆盖」，permission 的 `deny` 是**安全底线**，三层**并集**：

```text
workspace [permission]   （最高，gateway 可信目录 .omini/workspaces/<id>.toml）
   ▲ layer_over
profile [permission]     （profile TOML）
   ▲ layer_over
gateway default_permission （gateway.toml，最低基线）
```

- **`deny` = 三层并集**：任一层禁掉的工具，上层都无法静默放开。部署方在 `gateway.toml` 设一条 fleet-wide 禁令，任何 profile/workspace 都改不掉。
- **`allow` = 三层并集**（同 deny，去重）：低层固化的批准（§5.1）不会被上层静默丢弃。
- **`ask` = 自上而下覆盖**：高层设了 ask 则替换低层的 ask 列表，未设则继承。

解析函数 `app::resolve_permission(workspace, profile, gateway)`，等价于 `workspace.layer_over(profile.layer_over(gateway))`。

三层都是 **gateway 可信或部署方拥有**的配置——**没有一层**读自 agent 可写的项目目录，所以 workspace 层加 `deny` 是安全的（见 `doc/gateway.md`「为何在 gateway 侧」）。

gateway 基线（`gateway.toml`）：

```toml
[[default_permission.deny]]
tool = "shell"
contains = ["curl", "wget"]   # 全 gateway 禁止外发下载
```

workspace 覆盖（`<gateway>/.omini/workspaces/<id>.toml`）：

```toml
[[permission.deny]]
tool = "shell"
contains = ["git push"]       # 仅此 workspace 追加禁令
```

CLI 运行只经过 profile 层（gateway/workspace 两层都是 gateway 侧，CLI 传空策略）。

### 3.2 工具目录与配置 UI（增量规则列表）

用户配置门控时**不手输工具名/字段名**。`GET /tools` 返回内置工具目录（`crate::tool::builtin_catalog`：read/write/edit/shell 的友好标签 + 可作为 `field` 的输入字段列表 + 字段是否为路径）。前端配置界面是**增量规则列表**（`PermissionRulesEditor.svelte`）：每层只渲染用户真正添加的规则，空层 = 一行说明 +「添加规则」按钮，绝不预渲染全量工具列表。规则行折叠态是大白话摘要（「拒绝 运行命令：当 命令 包含 rm -rf」），展开后才出现决策 seg、工具下拉（目录 +「任意工具 (*)」）与条件区（field/mode/白名单/值，默认折叠）；无条件规则即该工具在本层的默认裁决。gateway 层额外提供折叠的**工具默认表**（每目录工具一行三态 seg），编辑的同样是 bare rules。

- 磁盘 = 机器读，结构化/规范化（`Rule` 全字段）；用户层 = 规则行，简单零负担。二者转换是纯函数（`permission-rules.ts` 的 `toRows`/`fromRows`）。
- **三层各归其位**（不再是单页三层并列）：gateway 基线在 Settings → **全局设置** tab（含工具默认表）；profile 层在 Settings → **Profiles** tab 随该 profile 编辑；workspace 层在工作区 `WorkspaceConfigDialog`（与 network 并列）。曾经的「生效结果视图」已删除——它只是按来源层罗列规则、并不做真实裁决计算，名不副实；将来若需要，应在能凑齐三层的 workspace 侧重写为真求值。
- 内置 4 种工具目录是静态的（无需子进程），故 profile 层与 gateway 层配置界面都能用（`GET /tools`）。
- MCP 动态工具按 workspace best-effort 列举：`GET /workspaces/{id}/tools` 起 MCP 子进程读 `tools/list`，失败则跳过该 server、仍返回内置项（不报错）。仅 workspace 层用它（gateway/profile 层无具体 workspace 上下文）。MCP 工具无字段元数据，条件退化为「任意输入」，仍可门控。

**overlay 继承（安全语义，有意区别于一般 field-level override）**：
- `deny` **并集继承**——子 profile 可*增加*禁令，但**永不**静默丢弃从父级继承的 deny。
  （否则子 profile 顺手加一条无关 `ask` 就可能重新打开父级禁掉的工具 = 隐蔽提权。）
- `allow` 同 `deny` **并集继承**（去重）——父级固化的批准不在子 profile 中丢失。
- `ask` 沿用替换语义——子 profile 设了任意 ask 规则则替换父级 ask 列表，否则继承。

（overlay 与三层解析共用同一套合并逻辑 `PermissionPolicy::layer_over`。）

## 4. 求值点与结果

`dispatch_tool`（`src/agent/mod.rs`）：
- `Allow` → 直接执行工具。
- `Deny` → 不执行，产生 `ToolEvent::Failed { code: "denied_by_policy" }` 回喂 model；审计写
  `PermissionEvent::Decided { AutoDenied, "policy" }`。
- `Ask` → 写 `PermissionEvent::Requested`，调 `ApprovalGate::request` 挂起等决定：
  - 批准 → 执行工具；写 `Decided { Approved, "user" }`。
  - 真人拒绝 → `ToolEvent::Failed { code: "denied_by_user" }`；写 `Decided { Rejected, "user" }`。
  - fail-closed（无人应答）→ `ToolEvent::Failed { code: "denied_no_approval" }`；写 `Decided { AutoDenied, "gate" }`。
    模型据 code 可区分"人说不"与"没接通审批"。

`denied_by_policy`/`denied_by_user` 的 `ToolEvent::Failed` 是给 **model** 看的结果（可据此调整行为）；
`PermissionEvent` 是并行的**审计**记录，二者语义不重复。

## 5. Ask 闭环（ApprovalGate）

`ApprovalGate`（精确签名见 [`src/agent/approval.rs`](../crates/ominiforge/src/agent/approval.rs)）是前端无关的注入点，同 `StreamSink`。两个方法：`request(req) -> ApprovalOutcome` 挂起等决定——三态 `Approved | RejectedByUser | AutoDenied` 区分「人拒绝」与「无人应答的兜底拒绝」，让审计不撒谎（M2）；返回值同时携带人选择的作用域（§5.1），无人决定时为 `None`。`supports_concurrent_requests()` 声明能否并发受理多个 request：gateway 为 true（走共享表路由），启用 §5.2 的两阶段并行分发；CLI/NullGate 为 false，保持逐条串行。

`ApprovalResolution` 三态映射到审计 `decided_by`：`Approved`/`RejectedByUser` = 人做了决定（`"user"`）；`AutoDenied` = 没人应答的 fail-closed 兜底（`"gate"`），绝不记成用户拒绝。

- **NullGate**（默认）：fail-closed，`ask` 一律 `AutoDenied`。用于 headless / eval / 测试——
  `ask` 绝不因没接 gate 而变成隐式 allow。
- **Gateway（Web / 桌面 / 手机）**：`GatewayApprovalGate`（`src/gateway/approval.rs`）挂起-恢复闭环：
  1. turn task 在 `dispatch_tool` 建 `oneshot`、插入共享 `PendingApprovals` 表（keyed by call_id）；
  2. publish `ActivityStatus::AwaitingApproval` + 发 `GatewayEvent::ApprovalRequested`；
  3. `rx.await` 挂起——此时 actor 的 `run_turn_phase` select-loop 仍在监听 inbox；
  4. 客户端回 `Command::Approve { call_id, decision, scope }` → actor 查表 send → turn 原地恢复。
  - 每 session 独立 Agent（spawn 时 `Arc::new` 一次），gate 与 actor 共享该表；gate 还持有 agent 的
    live policy handle（`Agent::permission_handle()`），供作用域审批固化规则（§5.1）。
  - cancel/shutdown 时 `clear_pending`：挂起的 rx 被 drop → gate 返回 `AutoDenied`（fail-closed）。
    turn 被 abort 后可能来不及写 `Decided`，故僵尸审批卡由前端在 `Turn::Interrupted/Failed` fold 时清理（race-free，`conversation.ts`）。

### 5.1 作用域审批（ApprovalScope）

每个审批决定带一个作用域（wire 字段 `scope`，缺省 `once`）：

| scope | 语义 |
|---|---|
| `once` | 仅此次调用（现状、默认）；不留痕 |
| `session` | 当前会话：规则写进该会话的内存 policy（`Agent.permission` 为 `Arc<RwLock<PermissionPolicy>>`，gate 与 agent 共享），立即生效——包括同轮仍 pending 的 ask（见下） |
| `profile` | 固化进 profile TOML（`permission.allow`/`permission.deny`）+ 当前会话 policy |
| `gateway` | 固化进 `gateway.toml`（`default_permission`）+ 当前会话 policy |

批准写 `allow` 规则，拒绝写 `deny` 规则。规则从 tool call 输入编译（`permission::rule_from_call`）：取工具主字段（内置工具目录的第一个 field：shell→`command`，read/write/edit→`path`）的字符串值，作 `field` 定位的 `substring` 模式——**完整值，不截断**（截断前缀会命中未经批准的同前缀调用，静默扩大授权面）；取不到非空字符串（字段缺失/非字符串/目录外工具如 MCP）则退化为工具级 bare rule（该工具任意调用都命中）。同一调用重复批准幂等（写前 `contains` 判重，profile/gateway 层已在则不重写文件）。

- 生效路径：gateway gate 收到非 once 决定 → 编译规则 → 写会话 live policy（去重）→ `profile`/`gateway` 再调 registry 注入的 `on_scoped` 回调持久化。profile 名取 session meta 的 `profile_id`，缺省回退 gateway 默认 profile（同 `runtime_info` 语义）；meta 读不到则放弃持久化而非瞎猜写入。
- **同轮重评估**：pin 落地后，该 session 仍 pending 的每条 ask（pending 表存有 tool/input）按更新后的 policy 重新求值——命中 `allow` 自动批准、命中 `deny` 自动拒绝（model 侧等同 policy deny），其余保持待人工。自动裁决走正常 `Decided` 审计，`decided_by` 记 **`"policy"`**（是规则而非人解决了这条 ask），`scope` 记该 pin 的作用域；人工答复与 pin 竞争同一条 ask 时，`HashMap::remove` 保证只有一个裁决生效。
- 内存与持久化一致：profile/gateway pin 先写会话 live policy（与 agent 同一 `Arc`，内存层同步更新），再走 `on_scoped` 持久化（registry 以 `config_write_lock` 串行化读-改-写、阻塞 I/O 走 `spawn_blocking`）。
- 持久化失败只记日志（`eprintln!`，gateway 层惯例），不影响审批本身的返回。
- 求值顺序保证 `deny > allow > ask`：固化的 allow 永远压不过一条 deny 禁令；固化后命中即免问。

### 5.2 并行分发（两阶段 dispatch）

gate 的 `supports_concurrent_requests()` 为 true（gateway）时，一轮里的多个 tool call 走两阶段分发；为 false（CLI/NullGate）时保持逐条串行（prepare→settle 内联，语义不变）。

- **阶段 A（按序、不等任何人）**：每个 call 依次写 `ToolEvent::Started` → `tool:invoke:before` hooks → 权限评估。deny 立即定案；ask 写 `Permission::Requested` 并 spawn `gate.request`——**所有审批提示一次性全部发出**，任一 call 的 `Requested` 不等其他 call。
- **每条 call 一条独立链**：await 自己的 gate 结果 → 批准即执行（例：先批 #2，#2 马上开始执行，不等 #1 被决定）；allow 链跳过 gate 直接执行。被拒/被 deny/失败的不执行。
- **审计即时**：链在 gate 出结果的当下经 verdict channel 回报，turn task 收到即写 `PermissionEvent::Decided`（按决定到达顺序）——前端 fold 到 `Decided` 立即清除等待卡。
- **结果事件按完成顺序写**：哪条链先完成，它的 `Tool::Completed/Failed` 先落盘——前端即时看到每次完成。失败/reject 的结果事件同样按各自链的完成时点落盘。
- **喂给模型的 tool result 仍严格按 `tool_call` 顺序**：turn task 把各链产出的 `Message` 按槽位收集，全部结束后按 call 序 `push_message`；resume 重建（`rebuild_runtime`）也把同一轮的结果按 assistant 的 `tool_call` 序重排，保证 live 与重建视图一致。provider 的 tool_call↔tool_message 配对约束在两条路径上都满足。
- writer 始终只在 turn task 上；链只跑 gate 等待与工具执行。**取消语义**：turn future 被 drop（cancel/硬错误）时 `ChainAbortGuard` abort 所有未完成链——`invoke` 中断、副作用不完成（而非旧 detach 行为：任务跑完、日志却写 cancelled）；gate task 由 actor 的 `clear_pending` fail-closed（pending 表清空，不留僵尸）；日志由 `record_cancelled_tool_calls` 补 `Failed(cancelled)`；前端在 turn 终态 fold 清理僵尸审批卡（不变）。乱序完成不改变这些语义。

## 6. 事件与审计

`EventPayload::Permission(PermissionEvent)`（`src/core/payload.rs`，`doc/event-schema.md` §3.9）：

两个变体：`Requested { call_id, tool_name, input }`（仅在 ask 挂起等人应答时写）与
`Decided { call_id, outcome, decided_by, scope }`（`outcome`: Approved|Rejected|AutoDenied）。
字段定义见 [`src/core/payload.rs`](../src/core/payload.rs)。

- **审批与 view 解耦**（`doc/tool-streaming.md` Phase 3）：`Requested` 不再携带 `preview`
  字段。历史上它存一份内容工具的 would-be diff（`Tool::preview` 在 ask 时 dry-run 算出），
  但阶段二铺平后，卡片在审批弹出时已通过流式管线的 `view`（BlockStop flush 的完整快照）
  展示了「这个文件将被改成这样」——审批回归纯决策门，不再自算自存一份与 view 同源的 diff。
  旧日志若带 `preview` 字段仍可正常反序列化（serde 忽略未知字段）。

- `scope: Option<ApprovalScope>`（serde default + None 不序列化）：人做了决定时记录其作用域
  （含 `once`，审计统一）；policy deny、fail-closed 兜底（无人应答的 `AutoDenied`）为 `None`。
  旧日志无此字段仍可正常反序列化。
- 持久化写 log：既是完整审计轨，又让前端在**刷新/重连**后从事件流 fold 重建待审批提示
  （committed 事件会 replay）。
- `GatewayEvent::ApprovalRequested` 是 ephemeral 的**即时提示**（点亮列表状态灯 + 低延迟弹面板），
  但 UI 的**权威**来源是持久化的 `PermissionEvent`。

## 7. Wire 协议

- `GatewayEvent::ApprovalRequested { call_id, tool_name, input }`（SSE/WS，ephemeral）。
- `POST /sessions/{id}/approve`，body `{ call_id, decision: "approve"|"reject", scope?: "once"|"session"|"profile"|"gateway" }` → 202（`scope` 缺省 `once`）。
- WS `{ "type": "approve", "call_id", "decision", "scope"? }`。
- 幂等：未知/已决 call_id 被 actor 忽略。
- ts 绑定：`PermissionEvent.ts` / `PermissionOutcome.ts` / `PermissionPolicy.ts` / `Rule.ts` / `ApprovalScope.ts`，改 wire 类型后跑 `just ts-export`。

## 8. 用户界面（GPUI 客户端）

权限审批和配置在 GPUI 客户端中实现：

- **审批界面**：tool 调用的审批在对话面板中进行，等待批准时显示审批按钮（批准/拒绝，含作用域选择）
- **配置界面**：三层配置在设置面板中进行（gateway 基线、profile 层、workspace 层）

GPUI 客户端的权限界面实现见代码。

## 9. 实现状态与待完善

已实现：策略内核（deny/allow/ask 三列表，`deny > allow > ask`）+ 三层解析（workspace > profile > gateway，`deny`/`allow` 并集）+ dispatch 接入
（allow/deny/ask）+ gateway 挂起-恢复闭环 + 持久化 `PermissionEvent` 审计（含 `Decided.scope`）+
作用域审批（once/session/profile/gateway：`rule_from_call` 完整值编译规则，session 写会话内存 policy 即时生效
（含同轮 pending ask 重评估、自动裁决记 `decided_by: "policy"`），profile/gateway 经 registry `on_scoped` 回调固化进
profile TOML / `gateway.toml`（`config_write_lock` 串行化 + `spawn_blocking`））+
并行分发（§5.2：两阶段 dispatch、独立链批准即执行、Decided 即时落盘、结果按完成序、模型消息按 call 序、
cancel 经 `ChainAbortGuard` 召回执行链）。

待后续：
- 规则匹配升级：`substring`/`prefix` + `field` 定位 + `negate` 白名单**已实现**（§3 规则模型）；未来可加 glob / regex。
- monitor 层聚合审批/拒绝计数进 `SessionSummary`（当前审计已由 event log 覆盖）。
- 内置 permission-guard hook 与 profile `[hooks]` 的 name 绑定（`doc/hook-protocol.md` §13）。
