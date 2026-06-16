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

Agent loop 看到的 tool 接口：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    async fn invoke(&self, input: ToolInput) -> ToolResult;
}

pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub struct ToolInput {
    pub call_id: String,
    pub input: serde_json::Value,
    pub timeout: Duration,
}

pub struct ToolOutput {
    pub content: Vec<Content>,
    pub is_error: bool,
    pub error_code: Option<String>,
}

pub type ToolResult = Result<ToolOutput, ToolError>;
```

Built-in tool 直接 impl `Tool` trait。MCP tool 通过 MCP client adapter impl `Tool` trait。

## 4. Built-in Tool

### 4.1 注册

Built-in tool 在 agent 启动时静态注册：

```rust
pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    registry.register(ShellTool::new(config));
    registry.register(ReadTool::new());
    registry.register(WriteTool::new());
    registry.register(SearchTool::new());
    // ...
}
```

### 4.2 特点

- 直接访问 OS 能力（文件系统、进程、网络）。
- 无沙箱限制（信任自身代码）。
- 最低延迟（无 IPC 开销）。
- 随 ominiforge 版本发布更新。

## 5. MCP Tool

### 5.1 MCP Server 配置

```toml
# .omini/config/mcp.toml

[[servers]]
name = "filesystem-extra"
command = "npx"
args = ["-y", "@anthropic/mcp-filesystem"]
env = { ROOT = "/home/user/projects" }

[[servers]]
name = "github"
command = "gh-mcp-server"
transport = "stdio"

[[servers]]
name = "remote-rag"
url = "https://my-rag.example.com/sse"
transport = "sse"
```

### 5.2 生命周期

```text
Agent 启动
  → 读取 mcp.toml
  → 启动配置的 MCP server 子进程（或连接远程）
  → MCP initialize handshake
  → tools/list → 注册到 ToolRegistry
  → 正常服务

Agent 关闭
  → 通知 MCP server shutdown
  → 终止子进程
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

```rust
pub enum Content {
    Text(String),
    Image { media_type: String, data: Vec<u8> },
    ArtifactRef { artifact_id: String, media_type: String },
}
```

超过 64KB 时，runtime 自动存入 artifact store，替换为 ArtifactRef。Tool 本身不感知。

## 9. 与 Event Schema 的关系

Tool 调用产生以下事件序列：

```text
ModelEvent::ToolCallDelta (model 产生 tool call)
  → ToolEvent::Started { tool_name, input, source }
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

| | 旧方案（已废弃） | 新方案 |
|--|---|---|
| 扩展机制 | WASM Component + WIT | MCP server（任意语言） |
| 沙箱 | wasmtime 内存隔离 | 无（信任用户安装的 MCP server） |
| 通信 | WIT 类型直传 | JSON-RPC over stdio/SSE |
| 开发门槛 | Rust + wasm32-wasip3 编译 | 任意语言，实现 MCP 协议即可 |
| OS 能力 | 受限，需 host 代理 | 完整（MCP server 是普通进程） |
| SDK | ominiforge-sdk (wasm) | 不需要，用 MCP SDK（各语言已有） |

## 12. 待后续完善

- Built-in tool 的权限控制（哪些 tool 在哪些 profile 下可用）。
- MCP server 健康检查和自动重启策略。
- Tool 热加载（运行中添加/移除 MCP server）。
- Tool 版本管理（MCP server 升级时行为变化检测）。
