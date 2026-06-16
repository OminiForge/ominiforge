# Ominiforge Extension Model

本文档定义 ominiforge 的扩展机制。Plugin 概念已废弃，扩展统一通过 MCP server 实现。

## 1. 设计原则

- 不自定义 plugin 协议。MCP 是唯一的外部扩展机制。
- MCP server 是普通进程，拥有完整 OS 能力。
- 一个 MCP server 可暴露多个 tools（等价于旧方案的 plugin 容器）。
- 安全性靠用户信任（安装行为即授权），不靠沙箱。
- 未来 marketplace 可通过签名 + 审核 + 可选容器隔离增强安全。

## 2. 与旧 Plugin 方案的对应

| 旧概念 | 新概念 |
|--------|--------|
| Plugin（WASM Component） | MCP server |
| Plugin manifest (plugin.toml) | MCP server 配置（mcp.toml） |
| Tool (WASM) | MCP tool |
| Plugin lifecycle (init/shutdown) | MCP server 进程 start/stop |
| Plugin 间隔离 | MCP server 天然进程隔离 |
| Plugin 签名校验 | 后续 marketplace 签名 |
| ominiforge-sdk | 不需要，用各语言 MCP SDK |

## 3. MCP Server 配置

```toml
# .omini/config/mcp.toml

[[servers]]
name = "code-sandbox"
description = "Isolated code execution"
command = "code-sandbox-mcp"
args = ["--workspace", "$WORKSPACE"]
env = { SANDBOX_MODE = "namespace" }
transport = "stdio"
auto_start = true

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

### 3.1 配置字段

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

### 3.2 变量替换

配置中支持以下变量：

```text
$WORKSPACE      # 当前 session 的 workspace 路径
$SESSION_ID     # 当前 session ID
$OMINI_HOME     # .omini/ 目录路径
$HOME           # 用户 home 目录
```

环境变量引用（`$GITHUB_TOKEN`）从进程环境继承。

## 4. MCP Server 生命周期

```text
Agent 启动
  → 读取 mcp.toml
  → 对 auto_start = true 的 server：
    → spawn 子进程 / 连接远程
    → MCP initialize handshake
    → tools/list → 注册到 ToolRegistry
  → 正常服务

Session 进行中
  → MCP server 持续运行
  → tool 调用通过 JSON-RPC 路由

Agent 关闭
  → 通知 MCP server shutdown
  → 等待 graceful shutdown（超时 kill）
```

## 5. MCP Server 健壮性

| 场景 | 处理 |
|------|------|
| Server 启动失败 | 记录错误，该 server 的 tools 不可用，不阻塞 agent 启动 |
| Server 运行中崩溃 | 自动重启（最多 3 次），连续失败则标记为不可用 |
| 调用超时 | 返回 ToolError::Timeout，记录到 monitor |
| Server 返回错误 | 转为 ToolOutput.is_error = true，传给 model |

## 6. 与 Built-in Tool 的共存

```text
ToolRegistry
├── Built-in tools（直接 Rust impl）
│   ├── read
│   ├── write
│   ├── shell
│   └── ...
└── MCP tools（通过 MCP adapter）
    ├── github.create_issue
    ├── github.search_code
    ├── code-sandbox.execute
    └── ...
```

命名规则：
- Built-in tool: 短名（`read`, `write`, `shell`）
- MCP tool: `{server_name}.{tool_name}`（避免冲突）

Agent loop 不区分来源，统一按 name 选择和调用。

## 7. 安全模型

当前（Phase 1）：
- MCP server 由用户主动安装/配置，安装行为 = 信任。
- MCP server 拥有与用户相同的 OS 权限。
- 类比：VS Code extension、npm package、MCP server in Claude Code。

未来（Marketplace）：
- 签名校验：验证 server binary 来源。
- 权限声明：manifest 声明所需能力，安装时展示。
- 可选容器隔离：高风险 server 运行在 namespace/container 中。
- 社区审核：官方/社区 review 标记。

## 8. 文件系统布局

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

## 9. 开发自定义 MCP Server

用户开发 MCP server 无需 ominiforge SDK，使用各语言标准 MCP SDK：

```text
Python:  pip install mcp
Node.js: npm install @modelcontextprotocol/sdk
Rust:    cargo add mcp-sdk
Go:      go get github.com/mark3labs/mcp-go
```

开发完成后在 mcp.toml 中添加配置即可使用。

## 10. 废弃内容

以下概念已废弃，不再存在于系统中：

- WASM Component runtime (wasmtime)
- WIT 接口定义
- ominiforge-sdk crate
- Plugin manifest (plugin.toml / tool.toml)
- WASI capability-based 权限
- Plugin 间隔离（WASM instance 隔离）

这些被替换为：MCP 标准协议 + 进程级隔离 + 用户信任模型。
