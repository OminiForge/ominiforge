<!-- status: current -->
<!-- owner: @OminiForge -->

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
├── Built-in（Rust 代码，编译进 ominiforge binary，随版本发布）
└── MCP（外部 MCP server，stdio/SSE 通信）
    ├── 用户自建 MCP server
    ├── 社区 MCP server
    └── 第三方 SaaS MCP server
```

内置工具的权威清单以代码为准（`register_builtin`，见
[`src/tool/mod.rs`](../src/tool/mod.rs)），不在此手列——避免清单与实现漂移。
各工具的行为契约在各自源文件头部注释里定义（如 `edit` 见 §11）。
LSP 诊断不是独立工具，而是 `edit`/`write` 结果的附加块，见
[`lsp.md`](./lsp.md)。

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

## 11. edit 工具：内容锚定替换

`edit` 是 `write` 的局部替代：`write` 重写整文件，`edit` 对已有文件做定点替换，
token 消耗更少。定位不靠行号、不靠 snapshot tag，而是靠**被替换文本本身**——
模型引用要改的确切现有内容（`old`），工具在文件里查找它并换成 `new`。不知道现有
内容就无法引用，所以“必须先读”是这个设计的自然结果，而非额外记账；文件在别处被
改动也不会误伤——只要 `old` 引用的那几行还在且唯一，替换就成立。

### 11.1 使用流程

```sh
# 1. read — 获取 [path] 和行号（行号仅供人/模型定位，不作 edit 锚点）
read path="src/lib.rs"
# 输出：
# [src/lib.rs]
# 1:fn main() {
# 2:    println!("hello");
# 3:}

# 2. edit — 引用要替换的确切文本（old），不带行号、不带 tag
edit edits='[
  { "path": "src/lib.rs",
    "old": ["    println!(\"hello\");"],
    "new": ["    println!(\"world\");"] }
]'
```

### 11.2 结构化输入

顶层是 `edits` 数组，每项 `{ path, old, new, replace_all? }`：

| 字段 | 必填 | 说明 |
|---|---|---|
| `path` | 是 | 相对 workspace 根的文件路径 |
| `old` | 是 | 要替换的确切现有内容，一项一行（不能内嵌换行）。逐字引用自 `read` 输出，或你刚发出的工具参数（`write` 的 `content`、上一次 `edit` 的 `new`——`edit`/`write` 成功只回一行简报，其输出无可引用内容）。非空 |
| `new` | 是 | 替换后的内容，一项一行。空数组表示删除 `old` |
| `replace_all` | 否 | 默认 `false`。true 时替换所有不重叠的匹配，而非要求唯一 |

`old` / `new` 用数组（一项一行）而非内嵌 `\n` 的字符串，避免整段 patch 塞进一个
JSON string 后被模型或 provider 双重转义。

**解析宽容性（实现行为，非放宽契约）**：数组始终是唯一规范形态；工具在解析时对
两类常见笔误做规范化而非拒绝——`edits` 给单个对象时自动包装成单元素数组，`old` /
`new` 给单个字符串时按换行拆成行。规范化只对无歧义的输入生效，schema 描述与
multi-line item 拆分（`split_lines`）维持不变。

**语义要点：**

- **替换** = `old` 给现有行，`new` 给替换行。
- **删除** = `new: []`。
- **插入** = 在 `old` 和 `new` 里都保留一行不变的锚点行，`new` 额外带上要插入的行。
  例：`old:["a"]`, `new:["a","A1"]` 在 `a` 之后插入 `A1`。
- **唯一性**：`old` 必须在文件里恰好匹配一处，否则报 `ambiguous`；除非
  `replace_all: true`，此时替换每一处不重叠的匹配。
- **多行 `old`** 按**连续行**匹配。
- 同一 `path` 可有多个 entry；跨 entry 触碰同一行 → `overlapping_edits`，整体拒绝。

### 11.3 匹配与原子性

- 每个 path 只读一次，所有 entry 的 `old` 都对**这一次读到的原始内容**定位（边匹配
  边不修改），因此多个 entry 的行区间互不漂移——等价于旧行号方案的“锚定同一快照”，
  只是这里的快照就是“本次调用开头读到的文件”。
- **定位阶段**是 all-or-nothing：任一 entry 定位失败（`not_found` / `ambiguous` /
  `overlapping_edits`），**任何文件都不写**。全部通过后才逐 path 写入。但**写入阶段**
  按 path 顺序执行且不回滚：中途 I/O 失败（`write_failed`）时，已写的文件保持新内容，
  未写的不再写。
- 已知限制：`edit` 按行切分并以 LF 重新拼接，CRLF 文件经编辑后行尾会统一为 LF。
- **`not_found` 报定位诊断**：报错指出首个无法在锚点之后匹配的行号、文件中与其
  最接近的一行、以及首个差异字符，模型据此精修引用而非整段重读。若 `old` 每行
  都能在文件里找到但并不连续，报错会明说「行散落各处，并非相邻」，与「某行根本
  不存在」区分开。
- 错误码：`not_found`（无匹配，含 stale——文件在别处改过导致 `old` 不再存在，与普通
  找不到同类）、`ambiguous`（多处匹配且未开 `replace_all`）、`overlapping_edits`、
  `invalid_path`、`read_failed`、`write_failed`。均为 `is_error=true` 的 business
  error，模型可据此调整重试；空 `old`、空 `edits` 是协议错（malformed input）。

### 11.4 结果是简报，diff 由后端以 view 下发

成功时 `edit` 只回一行简报，**不在 result 里回 diff**：

```
edited src/lib.rs (1 replacement)
```

理由：模型在自己的 tool call 参数里已经写了 `old` 和 `new`，结果再回一份 diff 就是
把它刚写的东西读给它自己听（token 浪费）。`write` 同理——成功回
`wrote PATH (new, N lines)` / `wrote PATH (~, +A -B)` / `wrote PATH (no change)`，
不带正文。

UI 需要的 diff **不再由前端构建**，而是后端在执行时（握着真实 pre-edit 内容）产出，
作为 `ToolEvent::Completed` 的 `view` 字段随事件下发——见
[`tool-streaming.md`](./tool-streaming.md)。旧方案（前端复刻匹配算法 + 文件缓存自建 diff）
已废弃，废弃理由与该契约的完整定义见该文档。

### 11.5 尚未实现

- `replace block N`（tree-sitter 语法块替换）：需引入 tree-sitter 依赖，暂缓。

## 12. 与之前 WASM 方案的对比

WASM Component + WIT 扩展方案已废弃，统一改用 MCP（任意语言进程，JSON-RPC over
stdio/SSE，完整 OS 能力，无需 ominiforge-sdk）。废弃理由见
[`architecture.md`](./architecture.md) §2.3。

## 12. 待后续完善

- Built-in tool 的权限控制（哪些 tool 在哪些 profile 下可用）。
- MCP server 健康检查和自动重启策略。
- Tool 热加载（运行中添加/移除 MCP server）。
- Tool 版本管理（MCP server 升级时行为变化检测）。
