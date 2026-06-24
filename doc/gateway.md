# Gateway 系统

Gateway 是所有非 TUI 入口（Web / 桌面 / 手机 / 第三方）的唯一后端。TUI/CLI 本地直接调
core，不经 Gateway（`doc/architecture.md` §18）。Gateway 不实现 agent 逻辑——它是
core 之上又一个 event 流消费者，复用同一套 `Agent` / `SessionStore` / `EventBus`。

**精确类型与签名以代码为准**：配置见 [`src/gateway/config.rs`](../src/gateway/config.rs)；
session actor 见 [`src/gateway/actor.rs`](../src/gateway/actor.rs)；registry 见
[`src/gateway/registry.rs`](../src/gateway/registry.rs)；HTTP/SSE/WS server 见
[`src/gateway/server.rs`](../src/gateway/server.rs)。本文只讲设计意图与契约。

## 1. 核心约束：单写者锁决定一切

`SessionStore::open` / `create_*` 返回的 `SessionWriter` 持有该 session events.jsonl 的
OS 文件锁，直到 writer 被 drop。**一个 session 同一时刻只能在一处可写**。这不是限制，
是 append-only 历史不可变（§2.2）的执行保障：CLI 在跑某 session 时，Gateway 打开同一
session 会拿到 `Locked`，反之亦然——这正是 §18.1“多入口经共享文件系统协调”的落地方式，
靠 flock 强制而非约定。

推论：网络侧多客户端 fan-in 到一个 session，必须串行经过单一所有者。→ **session-actor
模型**（被锁逼出来的，不是选出来的）。

## 2. 组件

```text
ominiforge serve
  ├─ axum HTTP/SSE/WS server（feature "gateway"）
  ├─ auth middleware（单用户静态 bearer token）
  ├─ SessionRegistry          # session_id → 活跃 SessionActor handle
  └─ 每 session 一个 SessionActor task
       ├─ owns (SessionWriter, SessionRuntime)   # 轮间持有，同 TUI
       ├─ mpsc inbox: Send | Cancel | Compact | Shutdown
       ├─ 每 session 一条 outbound broadcast（committed events + live deltas）
       └─ idle 超时 → 自我关停 → drop writer → 释放 flock
```

### 2.1 SessionActor

一个 tokio task 拥有一个活跃 session。轮间持有 `(SessionWriter, SessionRuntime)`（和 TUI
轮间持有方式一致），从 mpsc inbox 顺序处理命令，**保证一个 session 上两个 turn 永不交错**。

turn 在 spawn 出的子 task 上运行（writer+runtime move 进去、跑完 move 回来），因此 `Cancel`
能 `abort` 它；abort 后 writer 被 drop（锁释放），actor 从 event log 重建 runtime 续跑——
和 TUI 的 cancel 恢复同源，根植于“log 是 source of truth”。

两路输出合并到一条 broadcast（`GatewayEvent`）：

- **committed events**：每条持久化的 `CoreEvent`，带 `seq`，供 SSE `Last-Event-ID` 续传。
  来自 session `EventBus`（publish-after-durable-append，订阅者只见已提交事件）。
- **live deltas**：token 级流式（`Delta`），瞬态，**不重放**（重连从 committed events 重建）。

turn 跑完发 `TurnSettled`；超阈值自动 compaction 并发 `Compacted{new_session_id}`，actor
跟随新 session（同 TUI poll_turn 逻辑）。turn 进行中收到的 `Send`/`Compact` 入队延后执行。

### 2.2 SessionRegistry

`session_id → ActorHandle`。冷 session 查找时即时 spawn：assemble 一个**每 session 隔离的**
agent（独立 provider + 独立 MCP 子进程），`open` 取锁，从 log 重建 runtime。锁已被占用
（CLI/TUI 或未知的在跑 actor）→ `open` 失败 → 查找上报冲突（server 映射为 HTTP 409）。

`create`（新 session）/ `fork`（在某 seq 分叉）各自 assemble agent、铸造 session、spawn
actor。fork 用父 session 截至 `at_seq` 重建的 context 做 snapshot，自包含（父可删，§6.2）。

逐出隐式：idle actor 自我关停，其 `ActorHandle` 变 dead，下次查找剪除死条目并重 spawn——
registry 不会被陈旧 handle 撑爆。spawn 用 async mutex 串行化，防两个并发查找为同一 session
建两个 actor（各去抢锁）。

### 2.3 per-session 隔离（已决策）

每个 session 拥有自己的 agent + MCP 子进程，零跨 session 耦合。代价：启动慢（每 session
spawn MCP）、进程多。换来完全隔离。共享池（按 profile 复用 agent/MCP）是后续优化项。

## 3. HTTP API

完整路由与请求/响应以 [`src/gateway/server.rs`](../src/gateway/server.rs) 为准。

session API 统一挂在 `/api/*` 下，避免与前端 SPA 自身的 client-side 路由（同名
`/sessions` 等）在同源托管时撞车（见 §10）。`/healthz` 留在根，不鉴权。

| Method | Path | 说明 |
|--------|------|------|
| GET  | `/healthz` | 健康检查，**不鉴权**，**不在 `/api` 下** |
| GET  | `/api/sessions` | 列出 session id（最新优先） |
| POST | `/api/sessions` | 新建 session → `201 {session_id}` |
| GET  | `/api/sessions/{id}` | session 元数据 |
| POST | `/api/sessions/{id}/fork` | body `{at_seq}` → 在该 seq 分叉，`201 {session_id}` |
| POST | `/api/sessions/{id}/message` | body `{text}` → 入队一个 turn，`202 Accepted`（不阻塞） |
| POST | `/api/sessions/{id}/cancel` | abort 正在跑的 turn |
| POST | `/api/sessions/{id}/compact` | body 可选 `{keep_last}` → 摘要并切换 compaction session |
| GET  | `/api/sessions/{id}/events` | SSE event 流（见 §4） |
| GET  | `/api/sessions/{id}/ws` | WebSocket：events 出 + `{type:"send",text}` / `{type:"cancel"}` 入 |

`message` 立即返回 202；turn 在 actor 内跑，输出走 event 流。这把“提交”与“观察”解耦，
和 TUI（spawn turn + 订阅 bus）同构。

## 4. 重连 / 续传

每条 committed event 带 session `seq`。SSE 把每个 event 的 `id:` 设为该 seq；客户端断线后
带 `Last-Event-ID: <seq>` 重连，server 先从**持久 log** 重放该 seq 之后的 committed events，
再挂上 live 流——无缝、不重不漏（§monitor §9，log 是 source of truth）。live deltas 瞬态，
故意不重放。broadcast `Lagged` 的慢订阅者跳过缺口，靠 log 重放补齐。

## 5. 认证

单用户静态 bearer token。`gateway.toml` 的 `api_key_env` 指定环境变量名（密钥不入配置文件，
§15）；配置了才启用鉴权，`/healthz` 永远开放，其余路由要 `Authorization: Bearer <token>`。
未配置 = 开放网关（仅在 loopback + 可信反代后安全，启动会告警）。GitHub OAuth + 多用户隔离
延后（`feature-requests.md`）。

## 6. TLS / 暴露模型（已决策）

Gateway 默认 bind `127.0.0.1`，**不**自己做 TLS。公网暴露由反向代理（caddy/nginx）终结
TLS（§18.1）。理由：少代码、标准运维、证书续期归代理。`bind` 可经 `gateway.toml` 或
`--bind` 覆盖。

## 7. 配置

`.omini/config/gateway.toml`（多 root 合并，最高优先 root 整份胜出，mirror mcp.toml 加载）：

```toml
#:schema 见 FR-2（待 JSON Schema 接入）
bind = "127.0.0.1:7878"          # 默认 loopback
api_key_env = "OMINI_GATEWAY_KEY" # 可选；不设=开放网关
idle_timeout_secs = 1800          # 默认 30 分钟无活动逐出 actor（释放锁）
```

## 8. 部署

用户级前台进程（`doc/architecture.md` §18.1）：

```bash
ominiforge serve                          # 前台（开发）
systemctl --user enable ominiforge-gateway # 常驻
loginctl enable-linger $USER               # logout 后续跑
```

与 CLI 共享同一 UID / home / `.omini/` 数据。CLI 不连 Gateway；二者各自独立跑 agent loop，
经共享文件系统（+ flock）保持一致。

## 9. 待后续深入

- API key 存储与轮换机制（当前静态 env）。
- Rate limiting 策略。
- 共享 agent/MCP 池（per-session 隔离的性能优化）。
- WebSocket 协议细节扩展（当前仅 send/cancel 入站）。
- Web 前端（Phase 6）、桌面/手机（Phase 9/10）经此 API 接入。
