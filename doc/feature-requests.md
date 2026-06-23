# Ominiforge Feature Requests（延后功能清单）

集中记录已决策延后、待需求出现再立项的功能。每条注明：现状、方案、延后理由、接入成本。
深入设计细节仍在各自来源文档（`todo.md` / `phase2-plan.md`），此处只做索引 + 摘要，方便快速回看。

---

## FR-1. MCP 远程 transport（Streamable HTTP + OAuth）

- **状态**：延后（2026-06-20 记录）。
- **来源**：`todo.md` §11。
- **现状**：`src/mcp/client.rs` 只实现 stdio；`config.rs` 的 `url` 字段解析但不连接，非 stdio
  server 在 `connect` 处被 `McpError::NotStdio` 拒绝。
- **方案**：实现 **Streamable HTTP**（2025-03-26 起现行远程 transport，单端点 `/mcp`，SSE 为其
  响应流模式，支持无状态部署 + `Last-Event-ID` 断线重连）+ **OAuth 2.1** client（2025 spec 已
  将授权写进规范）。
  - **不做 SSE**（2024-11-05 老 transport，双端点 `GET /sse` + `POST /messages`，已于 2025-03-26
    废弃）。要做远程直接上 Streamable HTTP。
- **延后理由**：远程 server 拖入一坨正交的独立工程——OAuth flow、token 刷新、TLS、SSE 流解析、
  断线重连。当前无远程 server 需求；Phase 2 "可用" 目标（agent 能调外部 tool）已由 stdio 达成。
- **接入成本**：`url` 字段占位已留，接入时数据模型不动，只加一个 transport 实现 + OAuth client。

## FR-2. 配置可发现性（TOML + JSON Schema）

- **状态**：待实现（与功能解耦，可顺手做，不占主线）。
- **来源**：`todo.md` §16、`phase2-plan.md` §D。
- **现状**：配置用 TOML（人工编辑友好），但用户不知道有哪些字段可配；TOML 无独立 schema 标准。
- **方案**：用 `schemars` 从 Rust 配置类型（providers / profile / pricing / limits / mcp）自动
  生成 JSON Schema（与代码同步不漂移）；随仓库发布 schema 文件；`ominiforge init` 模板顶部写入
  `#:schema` 指向它（Taplo LSP 即可自动补全 + 校验 + 悬停文档）；新增 `config schema`（导出）与
  `config validate`（校验）命令。
- **延后理由**：纯开发体验改进，不阻塞任何功能主线。
- **接入成本**：在经过相关配置类型的步骤里顺手给类型 derive `JsonSchema` 即可，低。

## FR-3. 多节点协同（K8s 风格 edge nodes）

- **状态**：延后，需独立研究（2026-06-22 记录）。
- **来源**：`todo.md` Phase 9。
- **现状**：各 Ominiforge server 实例完全独立，客户端手动切换。
- **目标方向**：多个实例作为 edge nodes，任务可跨节点调度，支持按算力/数据本地性/隔离需求路由。类比 K8s scheduler + controller 架构。
- **延后理由**：当前无多机器协同需求；架构复杂度高，需独立研究（集群控制面、节点注册/健康检查、跨节点 session/数据同步协议）。
- **启动条件**：Gateway + Scheduler + 至少一个客户端（Web/桌面）稳定后，有实际多机器场景再立项。
