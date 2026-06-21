# Ominiforge 待深入讨论清单

本文记录架构设计中仍需进一步讨论、验证或决策的事项。

**已决策事项**（P0/P1 全部完成）已落到各自规范文档，下表只留索引；本文正文仅保留
**未决策 / 待深入**的开放项。已决策延后、待需求再立项的功能集中在
[`feature-requests.md`](./feature-requests.md)。

## 已决策索引（详见各规范文档）

| # | 主题 | 输出物 |
|---|------|--------|
| 1 | Core event schema | [`event-schema.md`](./event-schema.md) |
| 2 | Session event log schema | [`session-storage.md`](./session-storage.md) |
| 3 | Tool / Hook / Extension protocol（built-in + MCP，废弃 WASM） | [`tool-protocol.md`](./tool-protocol.md)、[`hook-protocol.md`](./hook-protocol.md) |
| 4 | Context view 与 compaction | [`context-management.md`](./context-management.md) |
| 5 | Workspace crate 边界（1 crate） | [`architecture.md`](./architecture.md) §5 |
| 6 | Sandbox 与监控（event journal + 分阶段 shell sandbox） | [`sandbox.md`](./sandbox.md) |
| 7 | Monitor trace model | [`monitor.md`](./monitor.md) |
| 8 | Profile schema（含 provider 配置） | [`profile.md`](./profile.md) |
| 10 | Skill 生命周期 | [`skill.md`](./skill.md) |

## 开放项

### 9. Memory 系统 — 延后，需独立研究

高级特性，复杂度高，需独立研究后设计。当前不定义接口，不约束实现。

已明确方向：
- Memory 是独立子系统，不是简单文件存储。
- 需支持复杂逻辑结构（图关系、级联更新、矛盾检测）。
- 应能接入主流 memory 系统（mem0、Zep、LangMem 等）作为可选 backend。
- 后端可替换（文件 / 向量库 / 图数据库 / 第三方 service）。
- Profile 中 `memory.scopes` 和 `memory.auto_write` 预留了配置位。

未决定：
- 接口定义（需深入研究后确定）。
- 存储格式。
- 图结构设计。
- 推理层（LLM vs 代码规则 vs hybrid）。
- 与 evolution 系统的协作方式。

启动条件：agent loop + session + tool 基本跑通后，再独立立项研究。

### 11. MCP / ACP / A2A 适配优先级 — 部分决策

已决策：
- **MCP client（stdio transport）**：已实现（Phase 2 Step 5）。本地 server 全走 stdio
  子进程，零网络/零认证，覆盖当前全部场景。
- **MCP client 远程 transport（Streamable HTTP + OAuth）**：延后，见
  [`feature-requests.md`](./feature-requests.md) FR-1。
- **MCP server 对外暴露**：延后，暂不暴露，等有明确对外能力需求时再设计。
- **A2A**：延后。当前无跨系统 agent 协作需求。内部多 agent 协作用 subagent/task system 解决。
- **ACP**：延后。编辑器集成初期通过 gateway HTTP/WS 顶替。

协议 adapter 位置：`src/mcp/`（已有）、`src/a2a/`、`src/acp/`（后续按需添加）。

### 12. Gateway API — 部分决策

已决策：
- Gateway 面向精美 Web 前端，最终公网暴露。
- HTTP API 覆盖：session CRUD/fork/message/cancel、profiles、tools、skills、monitor、evolution。
- WebSocket：`/sessions/:id/events` 实时 event stream。
- 认证模型：
  - GitHub OAuth 登录。
  - 管理员邀请制注册（关闭公开注册）。
  - 管理员可随时收回账号。
  - HTTPS 强制。
- 部署模型：用户级 systemd service，`ominiforge serve`。

待后续深入：
- OAuth flow 具体实现。
- 权限粒度（不同用户可访问不同 session/profile？）。
- Rate limiting 策略。
- Web 前端技术选型和 UI 设计。

### 13. Scheduler 与任务工作区 — 部分决策

已决策：
- Scheduler 不是简单 cron，是 **任务管理系统 + agent 执行引擎**。
- 任务状态机：

```text
backlog（待办）
  → running（执行中，agent 正在处理）
    → pending_review（待交付，reviewer agent 验证中）
      → delivered（已交付，验证通过）
      → blocked（验证失败，打回修改 → 重新 running）
```

- 每个任务有交付标准（delivery criteria）。
- "待交付" 由 reviewer agent 自动验证：
  - 代码任务：功能正确性、无回归、风格一致、简洁性、最优实现。
  - 其他任务：按任务定义的交付标准检查。
- 验证通过 → 已交付。验证失败 → blocked，附带失败原因，打回修改。
- 任务来源：用户手动创建、Scheduler cron、Evolution 建议。
- 日常定时任务也在工作区中展示。

待后续深入：
- 任务 schema（字段、优先级、依赖关系）。
- Reviewer agent 的 profile 和验证策略。
- 任务分配（手动 vs 自动）。
- 并发执行限制。
- 工作区 UI 设计。

### 14. Web/TUI 信息架构 — 部分决策

已决策：
- `ominiforge` 命令直接进入 TUI（不需要子命令）。
- TUI 主打简洁明了好用，复杂功能放 Web。
- Web 是主要操作界面，监控观测都在 Web 上。
- UI 设计后续单独深入讨论。

待后续深入：
- TUI 面板布局。
- Web 前端框架选型。
- Web UI 信息架构和交互设计。

### 15. Plugin marketplace — 延后

延后。等 MCP server 生态成熟后再设计 marketplace。
当前用户直接配置 mcp.toml 手动管理。

### 16. 配置可发现性（TOML + JSON Schema）— 待实现

待实现，与功能解耦，可顺手做。方案与接入成本见
[`feature-requests.md`](./feature-requests.md) FR-2。
