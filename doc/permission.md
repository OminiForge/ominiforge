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

`PermissionPolicy`（`src/permission/mod.rs`）两个有序规则列表：

```rust
pub struct PermissionPolicy {
    pub deny: Vec<Rule>,  // 命中即阻断，最高优先级
    pub ask:  Vec<Rule>,  // 命中且无 deny 命中 → 需人工审批
}
pub struct Rule {
    pub tool: String,          // 工具名；"*" 通配任意工具，否则精确匹配
    pub contains: Vec<String>, // 子串模式；空 = 匹配该工具任意输入
}
```

**求值顺序（固定）**：`deny` 命中 → `Deny`；否则 `ask` 命中 → `Ask`；否则 `Allow`。
deny 永远压过 ask（"请确认"不能降级一条禁令）。

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

`field`/`mode`/`negate` 是配置 UI 的「每工具卡片」编译出来的结构化形态（§3.2）；手写 `contains = [...]` 老规则原样有效（`contains` 是 `patterns` 的磁盘别名，读写都用 `contains`）。

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
- **`ask` = 自上而下覆盖**：高层设了 ask 则替换低层的 ask 列表，未设则继承。

解析函数 `app::resolve_permission(workspace, profile, gateway)`，等价于 `workspace.layer_over(profile.layer_over(gateway))`。

三层都是 **gateway 可信或部署方拥有**的配置——**没有一层**读自 agent 可写的项目目录，所以 workspace 层加 `deny` 是安全的（见 `doc/workspace-config.md`「为何在 gateway 侧」）。

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

### 3.2 工具目录与配置 UI（零心智负担）

用户配置门控时**不手输工具名/字段名**。`GET /tools` 返回内置工具目录（`crate::tool::builtin_catalog`：read/write/edit/shell 的友好标签 + 可作为 `field` 的输入字段列表 + 字段是否为路径）。前端据此为每个工具渲染一张卡片：主控件是三档开关（allow/ask/deny，覆盖多数需求），例外区用该工具专属的字段下拉 + 大白话措辞（shell→命令、read/write→路径），把用户操作编译成上面的结构化 `Rule`。

- 磁盘 = 机器读，结构化/规范化（`Rule` 全字段）；用户层 = 卡片，简单零负担。二者转换是纯函数。
- 内置 4 种工具目录是静态的（无需子进程），故 profile 层与 gateway 层配置界面都能用（`GET /tools`）。
- MCP 动态工具按 workspace best-effort 列举：`GET /workspaces/{id}/tools` 起 MCP 子进程读 `tools/list`，失败则跳过该 server、仍返回内置项（不报错）。仅 workspace 配置弹窗用它（gateway/profile 层无具体 workspace 上下文）。MCP 工具无字段元数据，回退成「整输入」通用卡片，仍可门控。

**overlay 继承（安全语义，有意区别于一般 field-level override）**：
- `deny` **并集继承**——子 profile 可*增加*禁令，但**永不**静默丢弃从父级继承的 deny。
  （否则子 profile 顺手加一条无关 `ask` 就可能重新打开父级禁掉的工具 = 隐蔽提权。）
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

`ApprovalGate`（`src/agent/approval.rs`）是前端无关的注入点，同 `StreamSink`：

```rust
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    // 三态而非二态：区分「人拒绝」与「无人应答的兜底拒绝」，让审计不撒谎（M2）。
    async fn request(&self, req: ApprovalRequest) -> ApprovalResolution; // Approved | RejectedByUser | AutoDenied
}
```

`ApprovalResolution` 三态映射到审计 `decided_by`：`Approved`/`RejectedByUser` = 人做了决定（`"user"`）；`AutoDenied` = 没人应答的 fail-closed 兜底（`"gate"`），绝不记成用户拒绝。

- **NullGate**（默认）：fail-closed，`ask` 一律 `AutoDenied`。用于 headless / eval / 测试——
  `ask` 绝不因没接 gate 而变成隐式 allow。
- **CLI（`ominiforge run`）**：`CliApprovalGate`（`src/cli.rs`）同步终端提示，打印 tool + 参数到 stderr，
  读 stdin 一行；`y`/`yes` → `Approved`，明确的其它输入 → `RejectedByUser`，**EOF/io 错误 → `AutoDenied`**
  （无人应答，非用户拒绝）。非 tty stdin（管道）直接 `AutoDenied`。stdin 阻塞读取走 `spawn_blocking`。
- **Gateway（Web）**：`GatewayApprovalGate`（`src/gateway/approval.rs`）挂起-恢复闭环：
  1. turn task 在 `dispatch_tool` 建 `oneshot`、插入共享 `PendingApprovals` 表（keyed by call_id）；
  2. publish `ActivityStatus::AwaitingApproval` + 发 `GatewayEvent::ApprovalRequested`；
  3. `rx.await` 挂起——此时 actor 的 `run_turn_phase` select-loop 仍在监听 inbox；
  4. 客户端回 `Command::Approve { call_id, decision }` → actor 查表 send → turn 原地恢复。
  - 每 session 独立 Agent（spawn 时 `Arc::new` 一次），gate 与 actor 共享该表。
  - cancel/shutdown 时 `clear_pending`：挂起的 rx 被 drop → gate 返回 `AutoDenied`（fail-closed）。
    turn 被 abort 后可能来不及写 `Decided`，故僵尸审批卡由前端在 `Turn::Interrupted/Failed` fold 时清理（race-free，`conversation.ts`）。

## 6. 事件与审计

`EventPayload::Permission(PermissionEvent)`（`src/core/payload.rs`，`doc/event-schema.md` §3.9）：

```rust
pub enum PermissionEvent {
    Requested { call_id, tool_name, input },
    Decided   { call_id, outcome, decided_by },  // outcome: Approved|Rejected|AutoDenied
}
```

- 持久化写 log：既是完整审计轨，又让前端在**刷新/重连**后从事件流 fold 重建待审批提示
  （committed 事件会 replay）。
- `GatewayEvent::ApprovalRequested` 是 ephemeral 的**即时提示**（点亮列表状态灯 + 低延迟弹面板），
  但 UI 的**权威**来源是持久化的 `PermissionEvent`。

## 7. Wire 协议

- `GatewayEvent::ApprovalRequested { call_id, tool_name, input }`（SSE/WS，ephemeral）。
- `POST /sessions/{id}/approve`，body `{ call_id, decision: "approve"|"reject" }` → 202。
- WS `{ "type": "approve", "call_id", "decision" }`。
- 幂等：未知/已决 call_id 被 actor 忽略。
- ts 绑定：`PermissionEvent.ts` / `PermissionOutcome.ts` / `PermissionPolicy.ts` / `Rule.ts`，改 wire 类型后跑 `just ts-export`。

## 8. 前端（Web）

- 会话流内联 `ApprovalPrompt.svelte`（`frontend/DESIGN.md` §4.9）：待审批 = 琥珀脉冲边框卡片 +
  tool 名 + JSON 参数（语法高亮）+ Approve（acid-lime 主操作）/ Reject（secondary）。
- `conversation.ts` 从 `Permission::Requested` fold 出 pending item，`Decided` 就地翻 approved/rejected。
- 会话列表状态灯 `SessionStatusIcon.svelte` 的 `awaiting` 态（琥珀脉冲点）已存。

## 9. 实现状态与待完善

已实现：策略内核 + 三层解析（workspace > profile > gateway，`deny` 并集）+ dispatch 接入
（allow/deny/ask）+ CLI 终端 gate + gateway 挂起-恢复闭环 + 持久化 `PermissionEvent` 审计 +
Web 审批面板。

待后续：
- **三层的 Web 配置 UI**：profile `[permission]` 编辑器（批次 2）、gateway `default_permission`
  与 workspace `[permission]` 编辑器 + 写端点（批次 3）。当前三层后端已通，profile 层可经
  profile TOML 手写，gateway/workspace 层可手写对应 toml。
- **TUI 交互审批**（当前 TUI 走默认 gate，ask 即 fail-closed 拒绝）。
- 规则匹配升级：`substring`/`prefix` + `field` 定位 + `negate` 白名单**已实现**（§3 规则模型）；未来可加 glob / regex。
- monitor 层聚合审批/拒绝计数进 `SessionSummary`（当前审计已由 event log 覆盖）。
- 内置 permission-guard hook 与 profile `[hooks]` 的 name 绑定（`doc/hook-protocol.md` §13）。
