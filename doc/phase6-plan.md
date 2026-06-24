# Phase 6 实施计划（Web 前端）

本文把 [`frontend.md`](./frontend.md) §11 四个"实施前待深入"决策点钉死，并给出
分步实施顺序与依赖关系。设计原则、分层、类型链路以 `frontend.md` 为准；本文只补
实施前必须确定的契约与拆分。

## 0. 已完成前置

- ts-rs 类型桥 + `just ts-check` drift gate（commit `154306b`）。34 个 wire 类型已
  生成到 `frontend/src/lib/types/`。

## 1. 决策 A — client-core 接口签名

`SessionClient` 是 transport 无关接口，UI 只依赖它。签名直接映射 Gateway 已有契约
（`gateway.md` §3–§4），类型全部引用 ts-rs 生成物。

```ts
// frontend/src/lib/client-core/types.ts
import type { SessionMeta } from "$lib/types/SessionMeta";
import type { GatewayEvent } from "$lib/types/GatewayEvent";

/** 一次事件订阅的句柄，调用 close() 解除。 */
export interface EventSubscription {
  close(): void;
}

export interface SessionClient {
  listSessions(): Promise<string[]>;
  createSession(): Promise<string>;               // → session_id
  getSession(id: string): Promise<SessionMeta>;
  forkSession(id: string, atSeq: number): Promise<string>; // → new session_id
  sendMessage(id: string, text: string): Promise<void>;    // 202，不等 turn 完成
  cancel(id: string): Promise<void>;
  compact(id: string, keepLast?: number): Promise<void>;

  /**
   * 订阅 session 事件流。先按 lastSeq 重放 committed events，再挂 live 流
   * （gateway.md §4）。onEvent 收每条 GatewayEvent；lastSeq 省略=从头。
   */
  subscribeEvents(
    id: string,
    handlers: {
      onEvent: (ev: GatewayEvent) => void;
      onError?: (err: unknown) => void;
    },
    lastSeq?: number,
  ): EventSubscription;
}
```

要点：
- **REST 响应当前是无类型 `json!`**（`server.rs` 的 `{session_id}` / `{sessions}` /
  `{error}`）。不走 ts-rs。transport 实现内联解析这几个小壳，集中在 transport 层一处，
  不污染 UI。`getSession` 返回的 `SessionMeta` 是 ts-rs 类型（`get_session` 直接
  `Json(meta)`）。
- **端点路径集中常量**（`client-core/endpoints.ts`），防散落漂移。
- `subscribeEvents` 重连语义：`EventSource` 原生带 `Last-Event-ID`，断线自动带最后 seq
  重连；server 重放 committed-after-seq + 挂 live。deltas 不重放（gateway.md §4）。

### GatewayTransport 实现

- REST：`fetch`，bearer token 注入 `Authorization` header（远程模式必填，见决策 D
  鉴权 UI）。
- 事件：`EventSource`（SSE）。`GatewayEvent` discriminated union 按 `type` 分派：
  `event` / `delta` / `turn_settled` / `compacted` / `notice`。
- `compacted` → 调用方应跟随 `new_session_id` 重新订阅（同 TUI/actor 逻辑）。

> `TauriTransport` 属 Phase 9，不在 Phase 6 实现，但接口为它预留。

## 2. 决策 B — 图表库

**选 layerchart**（基于 LayerCake，Svelte 原生）。

| 维度 | layerchart ✅ | echarts |
|------|--------------|---------|
| Svelte 集成 | 原生组件，无 wrapper | 命令式，需 action 包裹 + 手动 resize |
| 包体 | 轻（按需 import） | 重（~1MB core） |
| 定制 | 组合式，吃 design token | 主题对象，色值另管 |
| dashboard 够用度 | 折线/柱/面积/分布够 | 更全（地图/3D，用不上） |
| SSR/static | 干净 | 需防 window 引用 |

监控 dashboard 的图（token 趋势、cost 累计、tool 延迟分布、cache 命中率）layerchart
全覆盖。echarts 的超集能力（地图/桑基/3D）此项目用不到，体积代价不划算。若后续出现
layerchart 撑不住的复杂图，再局部引 echarts，不整体替换。

## 3. 决策 C — UI 信息架构 / 页面拆分

开发者控制台定位（`frontend.md` §9）。四区，但按依赖分两批落地。

```text
路由（adapter-static SPA）
/                       → 重定向到 /sessions
/sessions               → session 列表（list/create/fork 入口）
/sessions/[id]          → 对话区（主线，事件流渲染）
/monitor                → 监控 dashboard         ← 依赖 monitor REST 端点
/monitor/[id]           → 单 session trace/summary
/evolution              → 进化审批              ← 依赖 Phase 8 evolution worker
```

**批次一（主线，无外部阻塞）**：
- `/sessions` 列表 + create + fork。
- `/sessions/[id]` 对话流：token 增量、tool call 流、cancel、compact、重连重放。

**批次二（有前置依赖，本 Phase 末或延后）**：
- `/monitor*`：**阻塞于 Gateway 缺 monitor 端点**（见 §5）。数据结构 `SessionSummary`
  已现成（`src/monitor.rs`），只差 REST 暴露。
- `/evolution`：**阻塞于 Phase 8**。evolution worker 是 stub，提案数据不存在。本 Phase
  只占位（空状态页），实质审批待 Phase 8。

布局：左侧固定导航（sessions / monitor / evolution），主区路由内容。Bento dashboard
风格用在 `/monitor`。

## 4. 决策 D — 交互设计

- 用 `ui-ux-pro-max` skill 跑一次，触发词明确 **"developer tool console"**，避免默认
  消费风（`frontend.md` §9）。
- 产出 `frontend/src/lib/styles/tokens.css`（色值/间距/字号单一来源）+ 基础组件
  （button / panel / list-item / stream-block / stat-card）。
- 组件强制引用 token，禁止手抄色值（`frontend.md` §6 设计漂移防线）。
- 风格基线：Minimalism / Swiss / Dark Mode / Bento Dashboard。
- 对话流交互：流式 append（不闪烁）、tool call 可折叠、reasoning 块弱化显示、
  错误/notice 显著但不打断。

## 5. 隐藏前置：Gateway monitor 端点（dashboard 阻塞项）

`gateway.md` §3 当前只覆盖 session 工作流主线。监控 dashboard 需要新端点暴露
`SessionSummary`：

```text
GET /sessions/{id}/summary   → SessionSummary（单 session 聚合）
GET /monitor/overview        → 跨 session 汇总（待定字段）
```

`SessionSummary` 需加 `#[derive(TS)]`（含其内部 `HashMap` 字段，ts-rs 映射为
`Record<string, ...>`）才能进类型桥。这是一个独立小任务，建议在批次二前作为
"Phase 5 补全"单独做，或并入 Phase 6 批次二开头。

> 决策：**批次一不依赖此项**，可立即开工。monitor 端点 + dashboard 归批次二。

## 6. 实施顺序

```text
批次一（主线，可立即开始）：
1. flake.nix devShell 加 nodejs + pnpm         → verify: nix develop 进 shell 有 pnpm
2. frontend/ SvelteKit 脚手架 + adapter-static  → verify: pnpm build 出 SPA
3. CI 加前端 job（tsc/lint/svelte-check/build）  → verify: CI 绿
4. client-core 接口 + GatewayTransport          → verify: 对 serve 实例跑通 list/create
5. design token + 基础组件（uipro skill）        → verify: tokens.css 落地，组件引用
6. /sessions 列表 + create/fork                  → verify: 浏览器跑通
7. /sessions/[id] 对话流 + 事件渲染 + 重连        → verify: 发消息见流式输出，断线重连不丢

批次二（有前置依赖）：
8. Gateway monitor 端点 + SessionSummary 加 TS    → verify: GET summary 返回类型化 JSON
9. /monitor dashboard（layerchart）              → verify: 图表渲染真实 session 数据
10. /evolution 占位页                            → 实质待 Phase 8

每步独立可验证；批次一 7 步走完即"浏览器端可用对话工作流"。
```

## 7. 依赖与风险

- **批次二阻塞**：monitor 端点（本仓库可补）+ Phase 8（evolution，未排期）。批次一不受影响。
- **pnpm-in-nix**：初期 devShell 提供 pnpm，前端构建非沙箱（`frontend.md` §7）。沙箱化
  纯构建作为后续优化，不进 Phase 6。
- **类型漂移**：已被 `just ts-check` gate 拦住；新增 wire 类型（如 SessionSummary）记得
  加 `#[derive(TS)]` 并重生成提交。
