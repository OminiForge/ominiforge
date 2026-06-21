# Ominiforge Tool Protocol

本文档定义 Tool 的分类、注册、调用协议和错误处理。

## 1. 设计原则

- Tool 分两类：Built-in（Rust 实现）和 MCP（外部 MCP server 提供）。
- Agent loop 对两类 tool 使用统一接口，不区分来源。
- 所有 tool 调用统一经过 event journal 记录。
- MCP 是唯一的外部扩展机制，不自定义 plugin 协议。
- Tool 是无状态的 request/response 操作，不支持 streaming。
- Tool 输出超 64KB 存 artifact store + 引用。

## 2. Tool 分类

```text
Tool
├── Built-in（Rust 代码，编译进 ominiforge binary）
│   ├── read        # 读取文件
│   ├── write       # 写入文件
│   ├── shell       # 执行 shell 命令
│   ├── search      # 代码搜索
│   ├── lsp         # Language Server Protocol 交互
│   └── ...         # 按需增加
└── MCP（外部 MCP server，stdio/SSE 通信）
    ├── 用户自建 MCP server
    ├── 社区 MCP server
    └── 第三方 SaaS MCP server
```

## 3. 统一 Tool Interface

Agent loop 通过单一 `Tool` trait 看待所有 tool：`descriptor()` 给出 name + description +
input schema，`invoke(input) -> ToolResult` 执行。Built-in tool 直接 impl `Tool`；MCP tool
通过 MCP client adapter impl `Tool`。两者对 agent loop 无差别。

trait、`ToolDescriptor` / `ToolInput` / `ToolOutput` / `ToolRegistry` 定义见
[`src/tool/mod.rs`](../src/tool/mod.rs)；`ToolOutput` / `Content` / `ToolSource` 等事件侧类型
见 [`src/core/payload.rs`](../src/core/payload.rs)。

## 4. Built-in Tool

Built-in tool 在 agent 启动时静态注册（`register_builtin` 见
[`src/tool/mod.rs`](../src/tool/mod.rs)）。特点：直接访问 OS 能力（文件系统、进程、网络）、
无沙箱限制（信任自身代码）、最低延迟（无 IPC 开销）、随 ominiforge 版本发布更新。

## 5. MCP Tool

MCP 是唯一的外部扩展机制（Plugin 概念已废弃，见 §11）。一个 MCP server 是普通进程，
拥有完整 OS 能力，可暴露多个 tool（等价于旧方案的 plugin 容器）。安全性靠用户信任
（安装行为即授权），见 §5.7。

### 5.1 MCP Server 配置

```toml
# .omini/config/mcp.toml

[[servers]]
name = "github"
description = "GitHub integration"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
transport = "stdio"
auto_start = true

[[servers]]
name = "remote-search"
description = "Semantic search service"
url = "https://search.example.com/mcp"
transport = "sse"
auto_start = true
```

配置字段：

| 字段 | 必填 | 说明 |
|------|------|------|
| name | ✓ | 唯一标识，用于路由和日志 |
| description | ✗ | 人类可读说明 |
| command | stdio 时必填 | 可执行文件路径 |
| args | ✗ | 命令行参数，支持变量替换 |
| env | ✗ | 环境变量，支持变量替换 |
| url | sse 时必填 | 远程 MCP server URL |
| transport | ✓ | stdio / sse |
| auto_start | ✗ | 默认 true，agent 启动时自动启动 |

变量替换：`$WORKSPACE`（当前 session workspace）、`$SESSION_ID`、`$OMINI_HOME`
（`.omini/` 目录）、`$HOME`。环境变量引用（如 `$GITHUB_TOKEN`）从进程环境继承。

### 5.2 生命周期

```text
Agent 启动
  → 读取 mcp.toml
  → 对 auto_start = true 的 server：spawn 子进程 / 连接远程
    → MCP initialize handshake
    → tools/list → 注册到 ToolRegistry
  → 正常服务

Session 进行中
  → MCP server 持续运行，tool 调用通过 JSON-RPC 路由

Agent 关闭
  → 通知 MCP server shutdown → 等待 graceful shutdown（超时 kill）
```

### 5.3 调用流程

```text
Agent loop 选择 MCP tool
  → ToolRegistry 路由到 MCP adapter
  → MCP adapter 发送 tools/call JSON-RPC
  → MCP server 执行并返回
  → MCP adapter 转换为 ToolOutput
  → 写入 ToolEvent
```

### 5.4 MCP Adapter 职责

- `tools/list` → 转为 `ToolDescriptor` 注册
- `tools/call` request/response → 转为 `ToolInput` / `ToolOutput`
- 管理 MCP server 子进程生命周期
- 处理 MCP server 崩溃和重连
- 超时控制

### 5.5 健壮性

| 场景 | 处理 |
|------|------|
| Server 启动失败 | 记录错误，该 server 的 tools 不可用，不阻塞 agent 启动 |
| Server 运行中崩溃 | 自动重启（最多 3 次），连续失败则标记为不可用 |
| 调用超时 | 返回 `ToolError::Timeout`，记录到 monitor |
| Server 返回错误 | 转为 `ToolOutput.is_error = true`，传给 model |

### 5.6 文件系统布局

```text
.omini/
├── config/
│   └── mcp.toml          # MCP server 配置
├── mcp/
│   ├── code-sandbox/     # 本地安装的 MCP server
│   │   ├── server        # 可执行文件
│   │   └── manifest.toml # 元数据（可选）
│   └── custom-tool/
│       └── server.py     # 脚本形式的 MCP server
└── sessions/{id}/
    └── mcp_data/
        └── {server_name}/ # MCP server 的 session 级数据（可选）
```

### 5.7 安全模型

当前（Phase 1）：MCP server 由用户主动安装/配置，安装行为 = 信任；server 拥有与用户相同
的 OS 权限（类比 VS Code extension、npm package）。未来 marketplace 可通过签名校验、
权限声明、可选容器隔离、社区审核增强。

### 5.8 开发自定义 MCP Server

无需 ominiforge SDK，使用各语言标准 MCP SDK，开发完成后在 mcp.toml 添加配置即可：

```text
Python:  pip install mcp
Node.js: npm install @modelcontextprotocol/sdk
Rust:    cargo add mcp-sdk
Go:      go get github.com/mark3labs/mcp-go
```

## 6. 调用流程（统一）

```text
Agent Loop
  → 选择 tool（不区分 built-in 或 MCP）
  → ToolDispatcher.invoke(tool_name, input)
    → 路由到对应 Tool impl
    → 执行
    → 检查 output 大小
      → ≤64KB: inline
      → >64KB: 存 artifact store，替换为 artifact_ref
  → 生成 ToolEvent（Started → Completed | Failed）
  → 结果返回 agent loop
```

## 7. Error 处理

### 7.1 业务错误（Tool 执行失败）

Tool 返回 `Ok(ToolOutput)` 但 `is_error = true`：

```text
ToolOutput {
    content: [Text("command not found: foo")],
    is_error: true,
    error_code: Some("execution_failed"),
}
```

### 7.2 协议错误

Tool 返回 `Err(ToolError)`：

```text
Err(ToolError::InvalidInput("missing required field: command"))
Err(ToolError::Timeout(duration))
Err(ToolError::ServerCrashed(reason))
```

### 7.3 错误分类

| 场景 | 表达方式 | 说明 |
|------|----------|------|
| Tool 执行失败 | Ok + is_error | 命令出错、超时、权限不足 |
| 输入不合法 | Err(InvalidInput) | Schema 验证失败 |
| MCP server 崩溃 | Err(ServerCrashed) | 进程退出 |
| 超时 | Err(Timeout) | 超过配置时限 |

## 8. Content 类型

Tool 输出内容为 `Content`（Text / Image / ArtifactRef），定义见
[`src/core/payload.rs`](../src/core/payload.rs)。超过 64KB 时 runtime 自动存入 artifact store，
替换为 `ArtifactRef`，tool 本身不感知。

## 9. 与 Event Schema 的关系

Tool 调用产生以下事件序列：

```text
ModelEvent::ContentBlock { content: BlockContent::ToolCall { id, name, arguments } }
  (model 产生 tool call；流式 delta 合并后的完整块)
  → ToolEvent::Started { tool_name, input, source }   (tool_call_event_id 指向上面的 ContentBlock)
  → ToolEvent::Completed { result } | ToolEvent::Failed { error }
```

source 字段标识 tool 来源（builtin / mcp:{server_name}）。

## 10. Tool Discovery

Agent loop 在每轮开始前收集可用 tool 列表：

```text
ToolRegistry
  → built-in tools (静态，启动时注册)
  → MCP tools (动态，server 启动后注册，可能变化)
  → 合并为 tool_schemas 发给 model
```

Tool schemas 按 name 字母序排列（保障 prefix cache 命中率）。

## 11. 与之前 WASM 方案的对比

WASM Component + WIT 扩展方案已废弃，统一改用 MCP（任意语言进程，JSON-RPC over
stdio/SSE，完整 OS 能力，无需 ominiforge-sdk）。废弃理由见
[`architecture.md`](./architecture.md) §2.3。

## 12. 待后续完善

- Built-in tool 的权限控制（哪些 tool 在哪些 profile 下可用）。
- MCP server 健康检查和自动重启策略。
- Tool 热加载（运行中添加/移除 MCP server）。
- Tool 版本管理（MCP server 升级时行为变化检测）。
