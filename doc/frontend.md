# 前端系统（Web + Desktop）

定义 Web 前端（Phase 6）与桌面端（Phase 9）的技术选型、代码组织、类型一致性链路和
低维护运行流程。核心结论：**Web 与 Desktop 共享同一套 UI 代码**，差异只在 transport
绑定层；仓库演进为 **polyglot monorepo**（Rust core 单 crate + `frontend/` Node 项目）。

## 1. 核心结论

1. **Web 与 Desktop 一体**：Tauri 用 webview 渲染，桌面端 = Web 前端 + 原生壳。UI 层
   100% 复用，唯一分叉点是 transport（网络 vs 本地 IPC）。
2. **框架 = SvelteKit + TypeScript**（adapter-static → SPA）。
3. **仓库 = polyglot monorepo**：`frontend/` 与 Rust `src/` 平级、物理隔离，唯一接触点
   是 ts-rs 生成的类型文件。Rust 侧维持单 crate（`architecture.md` §5 不破）。
4. **类型单一事实来源 = Rust**：跨 Rust↔TS 边界的类型/契约一律由 ts-rs 生成，CI 校验无
   diff。手改生成物是反模式。
5. **环境唯一来源 = nix flake**：任何新依赖（Node/pnpm/未来工具）必须进 `flake.nix`
   devShell，禁止依赖宿主机全局安装。

## 2. 分层架构

```text
┌─────────────────────────────┐
│ UI 层（组件 / 路由 / 渲染）   │  ← Web 与 Desktop 共享，100% 复用
├─────────────────────────────┤
│ client-core（transport 无关）│  ← 共享。SessionClient 接口：
│  list/create/fork/message/   │     listSessions, sendMessage,
│  cancel/compact + 事件订阅    │     subscribeEvents(lastSeq)...
├─────────────────────────────┤
│ transport 实现（可换）        │  ← 唯一分叉点
│  ├ GatewayTransport          │     fetch + SSE/WS → Gateway API
│  └ TauriTransport            │     Tauri invoke + event channel → Rust core 直调
└─────────────────────────────┘
```

UI 只依赖 client-core 接口，不知道底下是网络还是本地 IPC。启动时注入对应 transport：

| 入口 | transport | 说明 |
|------|-----------|------|
| Web | `GatewayTransport` | 浏览器无本地 core，纯远程 |
| Desktop 本地模式 | `TauriTransport` | Tauri `invoke` 直调 Rust core，无 `serve`、无网络、无 token |
| Desktop 远程模式 | `GatewayTransport` | 复用，连远程 server（server 注册列表） |

桌面端 = Web 前端 + TauriTransport + Rust 侧 `#[tauri::command]` 包装现有
[`src/app.rs`](../src/app.rs) assemble 层（CLI / Gateway 已共用）。

## 3. transport 契约

接口形状直接映射 Gateway 已有契约（[`gateway.md`](./gateway.md) §3–§4）：

| client-core 方法 | GatewayTransport | TauriTransport |
|------------------|------------------|----------------|
| listSessions | `GET /sessions` | `invoke("session_list")` |
| createSession | `POST /sessions` | `invoke("session_create")` |
| getSession | `GET /sessions/{id}` | `invoke("session_get")` |
| forkSession(atSeq) | `POST /sessions/{id}/fork` | `invoke("session_fork")` |
| sendMessage(text) | `POST /sessions/{id}/message` | `invoke("session_message")` |
| cancel | `POST /sessions/{id}/cancel` | `invoke("session_cancel")` |
| compact(keepLast?) | `POST /sessions/{id}/compact` | `invoke("session_compact")` |
| subscribeEvents(lastSeq) | SSE `/sessions/{id}/events` + `Last-Event-ID` | Tauri event channel |

**事件订阅是关键**，两实现都必须满足同一语义（`gateway.md` §4）：

- 先按 `lastSeq` 从持久 log **重放** committed events，再挂 **live 流**，不重不漏。
- live deltas（token 级）瞬态，**不重放**；重连靠 committed events 重建。
- **重连 = 带 lastSeq 重新订阅**，做成统一语义。Web SSE 有 `Last-Event-ID` 天然续传；
  Tauri channel 断线要自己实现 replay-from-seq（Rust 侧复用 EventBus subscriber 逻辑往
  webview emit）。

## 4. 类型一致性链路（防漂移核心）

单一事实来源：

```text
Rust core（event / message / REST DTO enum）   ← source of truth，唯一手写处
  │ #[derive(TS)]（ts-rs）
  ▼
frontend/src/lib/types/*.ts                    ← 生成物，提交入库，不手改
  │ import
  ▼
client-core 接口 + UI 组件                       ← 消费类型，漏 case 编译炸
```

- ts-rs 从 serde 表示生成 TS。与 [`event-schema.md`](./event-schema.md) §7 一致：**外部
  JSON wire 是稳定承诺，内部 Rust 类型可自由重构**——只要 `#[serde(rename)]` 保持 JSON
  稳定，生成的 TS 也稳定。
- 事件 payload 用 TS **discriminated union**，渲染分支做 **exhaustive check**（`never`
  兜底）；新增 event variant 未处理 → 编译失败（feature 不是 bug）。
- ts-rs 是已有 crate 的 dev-dependency + `#[cfg(test)]` 导出测试，**不新增 crate**。

## 5. 代码组织

```text
ominiforge/
├── src/                    # Rust 不动，零侵入
├── Cargo.toml
├── frontend/               # 新增，自包含 SvelteKit 项目，不进 Cargo workspace
│   ├── src/
│   │   ├── lib/
│   │   │   ├── types/      # ts-rs 生成物，提交入库供 CI diff
│   │   │   ├── client-core/ # SessionClient 接口 + transport 实现
│   │   │   └── components/
│   │   └── routes/
│   ├── package.json
│   ├── pnpm-lock.yaml
│   └── svelte.config.js
├── doc/
└── .github/workflows/      # CI 加前端 job
```

边界：`frontend/` 完全自包含，Rust `src/` 零侵入。逻辑单向依赖（前端依赖 Rust 生成类型，
反之不成立）。`architecture.md` §5 "单 crate" 仍成立——破的是隐含的"纯 Rust 仓库"假设，
而该假设本就因 Phase 6 Web 前端撑不住。仓库定性：**polyglot monorepo，Rust 侧单 crate**。

## 6. 低维护运行流程

原则：**把会腐烂的东西变成编译/CI 会拦的东西**。漂移从"运行时才发现"提前到"提交时被拦"。
人只在 schema 真变更时动一次手，且有明确提示。

| 易腐点 | 自动化机制 | 人介入时机 |
|--------|-----------|-----------|
| Rust event schema 改了前端没跟 | ts-rs 生成 + CI `git diff` gate | schema 真变时按提示重生成提交 |
| transport 契约漂移 | REST req/resp 也走 ts-rs；端点路径集中常量 | schema 变时一处改 |
| 新 event variant 没处理 | TS discriminated union + exhaustive `never` 兜底 | 加 UI 渲染分支 |
| 依赖腐烂 / CVE | renovate/dependabot 自动 PR + CI 验 | 仅 merge |
| 设计漂移 | design token 单一来源（uipro 出一次 → tokens.css），组件强制引用 | 加新组件时 |

CI pipeline：

```text
push / PR
 ├─ cargo build + test                          # Rust core
 ├─ cargo test --features ts-export             # 重新生成 TS 类型到 frontend/src/lib/types/
 ├─ git diff --exit-code frontend/src/lib/types # ← 有 diff = 类型没提交，FAIL
 ├─ pnpm tsc --noEmit                           # 前端类型检查（exhaustive union 在此炸）
 ├─ pnpm lint + svelte-check
 └─ pnpm build（adapter-static）                # 确保能出 SPA
```

支点 = 第 3 步 `git diff` gate：保证"Rust 改 schema 必须连带提交前端类型"，否则红。忘了
同步 = CI 拦，不靠人记。

### 两条文档化原则

1. **生成优于手写**：跨语言边界（Rust↔TS）的类型/契约一律生成，CI 校验无 diff。手改生成
   物 = 反模式。
2. **腐烂前移**：能编译期拦的不留到运行时，能 CI 拦的不靠人记。新增 event/字段触发编译错误
   是 feature 不是 bug。

## 7. nix flake 改动

环境唯一来源是 flake（原则 5）。引入前端要改 [`flake.nix`](../flake.nix)：

1. devShell 新增 `nodeTools = [ nodejs pnpm ];`，并入 `packages`。
2. **pnpm-in-nix 是已知硬点**：pnpm lockfile 与 nix sandbox 不天然兼容，纯 nix 沙箱构建前端
   需 `pnpm fetch` + fixed-output derivation 或 `npmlock2nix`/`pnpm2nix`。**初期绕开**：
   devShell 提供 pnpm，前端构建在 shell 内跑（非沙箱），flake `checks` 仍只管 Rust；前端
   CI job 用 devShell 的 pnpm 执行。沙箱化纯构建作为后续优化（simplicity first）。
3. ts-rs 是纯 Rust dev-dep，不影响 flake 结构。

## 8. Web 与 Desktop 分叉点（非 UI）

一体不等于完全相同，以下用 capability flag 条件渲染：

- **workspace 选择**：Desktop 有本地文件系统 → 原生目录选择器；Web 无 → 手填路径或远程已
  授权路径（`architecture.md` §18.2，workspace 是 session 属性）。
- **鉴权 UI**：Web / 远程模式要 bearer token 登录；Desktop 本地模式无需。
- **server 注册列表**：Desktop 特有（多 server 切换）；Web 自身即一个 server。
- **打包/分发**：Web = static SPA（Gateway 托管或独立 host）；Desktop = Tauri 打包资源进
  二进制。

## 9. 设计系统

开发者控制台定位（非营销/消费产品）。风格走 Minimalism / Swiss / Dark Mode / Bento
Dashboard，避免 gradient/storytelling 风。两类核心 UI：

- **agent 对话流**：token 增量流 + tool call 流，signal 细粒度更新（Svelte 无 vdom 包袱）。
- **监控 dashboard**：数据密集，对应 monitor 能力（[`monitor.md`](./monitor.md)）。

用 `.claude/skills/ui-ux-pro-max`（已装）在实现阶段出一次设计系统 → `tokens.css` + 组件库，
之后引用不手抄色值。明确以 "developer tool console" 触发，避免默认推消费风。

## 10. 框架选型理由（备查）

| 维度 | SvelteKit ✅ | React/Next | Leptos |
|------|-------------|------------|--------|
| Tauri 复用 | 官方一等 | static export | 生态嫩 |
| 类型一致性 | ts-rs 生成 | ts-rs 生成 | 天然共享 Rust 类型 |
| streaming reactive | signal 细粒度 | vdom，需手动优化 | signal 细粒度 |
| dashboard 生态 | 够 | 最大 | 几乎空，要造轮子 |
| 迭代/调试 | 快，HMR 好 | 成熟 | WASM 编译慢 |
| uipro skill 支持 | ✅ | ✅ | ❌ |

- **否决 Leptos**：唯一杀手锏=类型零漂移，但 dashboard 图表生态空白（监控是核心能力）、
  WASM 编译慢拖累单人迭代、uipro 不支持。代价太重。类型漂移用 ts-rs + CI gate 已可控解决。
- **否决 React**：生态大对单人项目边际收益低；vdom 对高频 token 流要额外优化。无压倒性理由。
- **选 SvelteKit**：命中全部约束。唯一让步=类型非天然共享，靠 ts-rs 生成兜，是可控工程问题
  非架构裂缝。

## 11. 待后续深入（Phase 6 实施前）

- client-core 接口精确签名（TS interface）+ 两 transport 实现细节。
- 图表库选型（layerchart / echarts）。
- UI 信息架构与页面拆分（对话 / session 管理 / 监控 / 进化审批）。
- Tauri 侧 `#[tauri::command]` 包装 `app.rs` 的具体形状（Phase 9）。
- 前端 nix 沙箱化构建（pnpm-in-nix）。
