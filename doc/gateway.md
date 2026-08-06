# Gateway 系统

Gateway 是 GPUI 客户端远程模式的后端（`doc/architecture.md` §18）。Gateway 不实现 agent 逻辑——它是
core 之上又一个 event 流消费者，复用同一套 `Agent` / `SessionStore` / `EventBus`。

**精确类型与签名以代码为准**：配置见 [`src/gateway/config.rs`](../crates/ominiforge-core/src/gateway/config.rs)；
session actor 见 [`src/gateway/actor.rs`](../crates/ominiforge-core/src/gateway/actor.rs)；registry 见
[`src/gateway/registry.rs`](../crates/ominiforge-core/src/gateway/registry.rs)；HTTP/SSE/WS server 见
[`src/gateway/server.rs`](../crates/ominiforge-core/src/gateway/server.rs)。本文只讲设计意图与契约。

## 1. 核心约束：单写者锁决定一切

`SessionStore::open` / `create_*` 返回的 `SessionWriter` 持有该 session events.jsonl 的
OS 文件锁，直到 writer 被 drop。**一个 session 同一时刻只能在一处可写**。这不是限制，
是 append-only 历史不可变（§2.2）的执行保障：Gateway 打开一个已被其它进程持有的
session 会拿到 `Locked`——靠 flock 强制而非约定。

推论：网络侧多客户端 fan-in 到一个 session，必须串行经过单一所有者。→ **session-actor
模型**（被锁逼出来的，不是选出来的）。

## 2. 组件

```text
ominiforge serve
  ├─ axum HTTP/SSE/WS server（HTTP/SSE 保留，WebSocket 新增）
  ├─ auth middleware（单用户静态 bearer token）
  ├─ SessionRegistry          # session_id → 活跃 SessionActor handle
  └─ 每 session 一个 SessionActor task
       ├─ owns (SessionWriter, SessionRuntime)   # 轮间持有
       ├─ mpsc inbox: Send | Cancel | Compact | Shutdown
       ├─ 每 session 一条 outbound broadcast（committed events + live deltas）
       └─ idle 超时 → 自我关停 → drop writer → 释放 flock
```

### 2.1 SessionActor

一个 tokio task 拥有一个活跃 session。轮间持有 `(SessionWriter, SessionRuntime)`，
从 mpsc inbox 顺序处理命令，**保证一个 session 上两个 turn 永不交错**。

turn 在 spawn 出的子 task 上运行（writer+runtime move 进去、跑完 move 回来），因此 `Cancel`
能 `abort` 它；abort 后 writer 被 drop（锁释放），actor 从 event log 重建 runtime 续跑——
根植于“log 是 source of truth”。

两路输出合并到一条 broadcast（`GatewayEvent`）：

- **committed events**：每条持久化的 `CoreEvent`，带 `seq`，供 SSE `Last-Event-ID` 续传。
  来自 session `EventBus`（publish-after-durable-append，订阅者只见已提交事件）。
- **live deltas**：token 级流式（`Delta`），瞬态，**不重放**（重连从 committed events 重建）。
- **live context**：每个 model round 校准 ledger 后发 `ContextUpdated{tokens, window, threshold}`
  —— 上下文占用快照（gauge 为 `tokens/window`，`threshold` 是压缩刻度，非分母）。
  同 `Delta` 瞬态、**不重放**，运行时值不落 log。

turn 跑完发 `TurnSettled`；超阈值自动 compaction 并发 `Compacted{new_session_id}`，actor
跟随新 session。turn 进行中收到的 `Send`/`Compact` 入队延后执行。

### 2.2 SessionRegistry

`session_id → ActorHandle`。冷 session 查找时即时 spawn：assemble 一个**每 session 隔离的**
agent（独立 provider + 独立 MCP 子进程），`open` 取锁，从 log 重建 runtime。锁已被占用
（另一个在跑的 actor）→ `open` 失败 → 查找上报冲突（server 映射为 HTTP 409）。

spawn 前先 **reconcile**：gateway 若在 turn 进行中被 kill，log 尾部停留在一个未终结的
`Turn::Started`（tool call 可能悬空）。`open` 之后、重建 runtime 之前，为悬空的 tool call
补写 `Tool::Failed{code:"interrupted"}` 并追加 `Turn::Interrupted` 终结符（与 cancel 路径
同一套收尾，仅错误文案不同）。没有这一步，view fold 会把这些 call 和整个 turn 渲染为
`running` 永不结束，客户端因 `turn_running=true` 只排队不发送，session 从 UI 上无法恢复。
不自动续跑：tool 的执行上下文随进程消失，副作用可能已部分生效，重放比中断更危险——
恢复语义是「日志闭合 + 用户发下一条消息继续」。

`create`（新 session）/ `fork`（在某 seq 分叉）各自 assemble agent、铸造 session、spawn
actor。fork 用父 session 截至 `at_seq` 重建的 context 做 snapshot，自包含（父可删，§6.2）。

逐出隐式：idle actor 自我关停，其 `ActorHandle` 变 dead，下次查找剪除死条目并重 spawn——
registry 不会被陈旧 handle 撑爆。spawn 用 async mutex 串行化，防两个并发查找为同一 session
建两个 actor（各去抢锁）。

### 2.3 per-session 隔离（已决策）

每个 session 拥有自己的 agent + MCP 子进程，零跨 session 耦合。代价：启动慢（每 session
spawn MCP）、进程多。换来完全隔离。共享池（按 profile 复用 agent/MCP）是后续优化项。

## 3. HTTP API

完整路由与请求/响应以 [`src/gateway/server.rs`](../crates/ominiforge-core/src/gateway/server.rs) 为准。

session API 统一挂在 `/api/*` 下，避免与前端 SPA 自身的 client-side 路由（同名
`/sessions` 等）在同源托管时撞车（见 §10）。`/healthz` 留在根，不鉴权。

Gateway 同时提供 HTTP/SSE（Web 前端过渡期保留）和 WebSocket（GPUI 客户端远程模式）：

| Method | Path | 说明 |
|--------|------|------|
| GET  | `/healthz` | 健康检查，**不鉴权**，**不在 `/api` 下** |
| GET  | `/api/sessions` | 列出 session id（最新优先） |
| POST | `/api/sessions` | 新建 session → `201 {session_id}` |
| GET  | `/api/sessions/{id}` | session 元数据 |
| POST | `/api/sessions/{id}/fork` | body `{at_seq}` → 在该 seq 分叉，`201 {session_id}` |
| POST | `/api/sessions/{id}/message` | body `{text, model?, think_effort?}` → 入队一个 turn，`202 Accepted`（不阻塞） |
| POST | `/api/sessions/{id}/cancel` | abort 正在跑的 turn |
| POST | `/api/sessions/{id}/compact` | body 可选 `{keep_last}` → 摘要并切换 compaction session |
| GET  | `/api/sessions/{id}/events` | SSE event 流（见 §4，Web 前端用） |
| GET  | `/ws` | WebSocket 连接（GPUI 客户端远程模式用，见 §4.1） |

`message` 立即返回 202；turn 在 actor 内跑，输出走 event 流。这把“提交”与“观察”解耦。

模型与推理强度是**每轮参数**而非重配置项：`model`（`provider/model_id`）/ `think_effort` 随单条 message 生效一轮，不写回会话配置；无法解析的 model、未声明的档位被丢弃并降级到会话配置（跨 provider 的覆盖由 actor 现 resolve 一个一轮性 provider；同 provider 复用连接只换模型 id）。只有 **profile** 切换才走 `POST /sessions/{id}/reconfigure`（换 system prompt / 工具集，必须开新 session）。`GET /api/models` 只返回**已配置凭证**的 provider 的模型：内置 catalog provider 需要 secret store 里有 key（即设置页粘贴配置），自定义 provider 则是 secret store 或其 `api_key_env` 已设置；未配置的不提供会在 resolve 时才失败的选项。

## 4. 重连 / 续传

每条 committed event 带 session `seq`。SSE 把每个 event 的 `id:` 设为该 seq；客户端断线后
带 `Last-Event-ID: <seq>` 重连，server 先从**持久 log** 重放该 seq 之后的 committed events，
再挂上 live 流——无缝、不重不漏（§monitor §9，log 是 source of truth）。live deltas 瞬态，
故意不重放。broadcast `Lagged` 的慢订阅者跳过缺口，靠 log 重放补齐。

### 4.1 WebSocket 协议（GPUI 客户端远程模式）

GPUI 客户端远程模式通过 WebSocket 连接 Gateway（`/ws` endpoint）。

**消息格式**：
- JSON 消息（与 HTTP API 一致）
- 请求-响应模式（同步操作）
- 流式模式（事件订阅）

**连接管理**：
- 单一 WebSocket 连接
- 心跳保活
- 断线重连（带 last_seq 重放）

**与 SSE 的区别**：
- SSE 是单向流（服务器→客户端），WebSocket 是双向流
- SSE 需要单独的 HTTP 请求（客户端→服务器），WebSocket 单一连接
- SSE 有 Last-Event-ID 续传，WebSocket 需要应用层实现

详见 [`network.md`](./network.md) §4.1。

## 5. 认证

单用户静态 bearer token。`gateway.toml` 的 `api_key_env` 指定环境变量名（密钥不入配置文件，
§15）；配置了才启用鉴权，`/healthz` 永远开放，其余路由要 `Authorization: Bearer <token>`。
未配置 = 开放网关（仅在 loopback + 可信反代后安全，启动会告警）。GitHub OAuth + 多用户隔离
延后。

## 6. TLS / 暴露模型（已决策）

Gateway 默认 bind `127.0.0.1`，**不**自己做 TLS。公网暴露由反向代理（caddy/nginx）终结
TLS（§18.1）。理由：少代码、标准运维、证书续期归代理。`bind` 可经 `gateway.toml` 或
`--bind` 覆盖。

## 7. 配置

`.omini/config/gateway.toml`（多 root 合并，最高优先 root 整份胜出，mirror mcp.toml 加载）：

```toml
#:schema 见 FR-2（待 JSON Schema 接入）
bind = "127.0.0.1:7878"          # loopback 示例；实际默认见 `src/gateway/config.rs` 的 `DEFAULT_BIND`
api_key_env = "OMINI_GATEWAY_KEY" # 可选；不设=开放网关
idle_timeout_secs = 1800          # 默认 30 分钟无活动逐出 actor（释放锁）
sandbox_backend = "passthrough"   # 会话执行环境后端（见下）
```

**`sandbox_backend`**（`doc/sandbox.md` §3.2）——宿主级、跨平台的选择，同一取值在各系统语义一致：
- `passthrough`（默认）：宿主直跑、零隔离、全平台。默认即此，避免部署误以为有隔离。
- `boxlite`：要求 microVM 后端；起不来（无 KVM / jailer 依赖缺失 / feature 未编译）则**响亮报错**，不静默降级。
- `auto`：优先 boxlite，起不来则 WARN 日志后退回 passthrough。异构机群的「尽力隔离」opt-in。

`boxlite` 需 `--features sandbox-boxlite` 编译（生产 flake `packages.default` 已开），且宿主需 KVM。

## 8. 部署

用户级前台进程（`doc/architecture.md` §18.1）：

```bash
ominiforge serve                          # 前台（开发）
systemctl --user enable ominiforge-gateway # 常驻
loginctl enable-linger $USER               # logout 后续跑
```

与 CLI 共享同一 UID / home / `.omini/` 数据。CLI 不连 Gateway；二者各自独立跑 agent loop，
经共享文件系统（+ flock）保持一致。

## 9. Workspace 配置

per-workspace 的沙箱策略覆盖层，位于 profile 与 gateway 默认之间。

### 9.1 解析链

沙箱策略沿四档派生，高档覆盖低档：

```text
workspace.toml  >  profile [network]  >  gateway default_network  >  Open（硬编码兜底）
```

- 任一档命中即用该值；`Open` 是一个新 boxlite session 不至于默认断网的兜底。
- 任一档策略名非法 → **fail loud**，建 session 失败，不静默回退到弱默认。
- `permission` 走平行的三层解析（workspace > profile > gateway），但 `deny` 是**并集**（安全底线，非覆盖）、`ask` 覆盖——见 [`permission.md`](./permission.md)。

### 9.2 位置：网关侧，不在项目目录

```text
<gateway_workspace>/.omini/workspaces/<workspace_id>.toml
```

- `workspace_id` = `WorkspaceId::from_path(canonical_path)`（FNV-1a 路径哈希，与 `workspaces.json` 同一套 id，版本稳定、可持久化）。
- 与 `workspaces.json` 同目录家族——per-workspace 的服务端状态集中在一处可信目录。

**为什么不放项目目录（如 `<project>/.omini/`）：** 项目目录是 **agent 可读写**的。从 agent 可写的地方读安全策略 = agent 能给自己放开网络/权限 = 权限提升。网关目录由**部署者掌控、可信**。

### 9.3 结构

workspace.toml 是一个 **workspace 命名空间**——不止网络策略，还承载共享挂载，以后 workspace memory 也放这。

```toml
# <gateway>/.omini/workspaces/<workspace_id>.toml
[network]
policy = "allowlist"                 # isolated | allowlist | open
allow  = ["crates.io", "pypi.org"]   # 仅 allowlist 生效

[[mounts]]
anchor = "workspace"                 # session | workspace | gateway
path   = "cache"                     # 锚点根内相对子路径(可空=根本身)
guest  = "/cache"                    # guest 内绝对挂载点
ro     = false                       # 只读挂载,默认 false(RW)

[[permission.deny]]                  # 本 workspace 追加的工具禁令(最高层)
tool     = "shell"
contains = ["git push"]
```

- `[network]` 缺省或无 `policy` 键 → 不构成覆盖，落到 profile/gateway 档。
- `[permission]` = 本 workspace 的工具门控，三层解析的**最高层**；合并语义（`deny` 并集 / `ask` 覆盖）见 [`permission.md`](./permission.md) §3.1。缺省=空=不贡献规则。
- `[[mounts]]`：命名锚点辅助挂载。锚点命名**共享范围**，不是用途：

  | anchor | host 根 | 共享范围 |
  |---|---|---|
  | `session` | `<gateway>/.omini/sessions/<session_id>/work/` | session 私有 |
  | `workspace` | `<gateway>/.omini/workspaces/<workspace_id>/shared/` | 同 workspace 跨 session |
  | `gateway` | `<gateway>/.omini/shared/` | 全局 |

- 未知键忽略，向前兼容。

### 9.4 生命周期与 GC

配置可比其项目活得久：项目被移走/删掉，但策略文件还在 `<gateway>/.omini/workspaces/`。

**原则：绝不自动物理删。** 路径消失可能是**瞬时的**（盘未挂载、项目 mid-move、worktree 临时删）；静默删一个用户手写的策略 = 不可回退的数据丢失。所以：

| 操作 | 语义 |
|------|------|
| `GET /api/workspaces/config/orphans` | **只读**列出「路径已不可解析」的配置（含它曾对应的 path，供人识别）。不删任何东西。 |
| `DELETE /api/workspaces/config/{workspace_id}` | **显式**删单个配置。幂等（不存在也返回 204）。GC 的唯一删除路径。 |

无自动 GC 触发器——对齐 session archive 的「显式、one-way」退休哲学。

## 10. 待后续深入

- API key 存储与轮换机制（当前静态 env）。
- Rate limiting 策略。
- 共享 agent/MCP 池（per-session 隔离的性能优化）。
