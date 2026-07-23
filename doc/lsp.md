# Ominiforge LSP 集成

本文档描述 LSP（Language Server Protocol）集成的 Phase 1：把语言服务器的**诊断（diagnostics）**作为 `read`/`edit`/`write` 的辅助信息返回给模型。它不是模型直接调用的工具——`doc/tool-protocol.md` §2 里 `lsp` 作为 built-in 工具类目是给后续阶段预留的。

## 1. 定位（Phase 1）

- **辅助，非工具**：模型不显式调用 LSP。`read`/`edit`/`write` 成功后，若该文件有配置的语言服务器，把诊断追加到工具结果里。
- **面向增长**：客户端层（`src/lsp/`）做得足够通用——后台读取任务、id 解复用、通用 `request()`——后续可直接接 `definition`/`references`/`hover` 等真正的 LSP 工具，无需重写。
- **性能是核心约束**：语言服务器（rust-analyzer 等）索引慢，文件操作绝不能卡在索引上（见 §4）。

**本阶段不做**：独立的 `lsp` 工具、rename/format/code-actions、补全、SSE/远程服务器、在 sandbox 内运行服务器。

## 2. 与 MCP 的异同

`src/lsp/` 结构上镜像 `src/mcp/`（子进程 + JSON-RPC + `Tool`-trait 适配），但 LSP 有四处关键差异：

| 关注点 | MCP | LSP |
|---|---|---|
| 分帧 | 换行分隔 JSON | `Content-Length:` 头 |
| 读取 | 同步「读到我的 id 为止」 | 后台读取任务：服务器**主动推送** `publishDiagnostics` |
| 状态 | 无状态调用 | 查询前必须 `didOpen`/`didChange` 同步文档 |
| 位置 | 无 | 0-based，默认 UTF-16（协商 UTF-8） |

语言服务器在**宿主**上启动（同 MCP，非 sandbox）。

## 3. 配置：`lsp.toml`

放在 `.omini/config/lsp.toml`，跨 roots 合并，高优先级 root 同名服务器覆盖低优先级（与 `mcp.toml` 一致）。

```toml
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
extensions = ["rs"]
# 可选，默认值如下：
# diag_timeout_ms = 400    # 每次文件操作等待诊断的上限
# init_timeout_ms = 2000   # 首次触碰某语言时等待 initialize 握手的上限

[[servers]]
name = "pyright"
command = "pyright-langserver"
args = ["--stdio"]
extensions = ["py", "pyi"]
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✓ | 唯一标识，用于日志 |
| `command` | ✓ | 可执行文件（stdio 传输） |
| `args` | ✗ | 命令行参数 |
| `env` | ✗ | 额外环境变量 |
| `extensions` | ✓ | 该服务器处理的文件扩展名（不带点）。文件路由到**第一个**扩展名匹配的服务器 |
| `diag_timeout_ms` | ✗ | 默认 400。同步文档后等待新诊断的硬上限 |
| `init_timeout_ms` | ✗ | 默认 2000。首次触碰该语言时等握手的上限；全量索引在后台继续，不受此限 |

## 4. 性能模型

1. **懒启动、非阻塞初始化**：某语言的首个文件触发服务器启动；`initialize`/索引在后台跑。触发操作最多等 `init_timeout_ms`，随后照常返回；诊断没就绪就不附带，绝不卡在全量索引上。
2. **热复用**：服务器与打开的文档存活整个 session（`LspManager` 由 `Assembled` 持有，见 §5），预热后一次 `didChange` → 单文件重新发布是亚秒级。
3. **有界等待**：每次操作最多等 `diag_timeout_ms`，无无界 await。
4. **版本门控**：接受版本 ≥ 我们发出的 `didChange` 的发布（或该 uri 自我们同步后出现的新发布），绝不附带编辑前的陈旧诊断。服务器省略版本时，取窗口内我们同步后的首个发布（best-effort）。
5. **不支持扩展名快路径**：无对应服务器 ⇒ 立即返回，不启动、不等待。
6. **fail loud，不静默也不打扰**：附带真实诊断，绝不编造。拿不到诊断时（无服务器 / 启动失败 / 超时 / 服务器确认干净）不附带任何内容——模型侧「无诊断块」即「当前无可说」，不占用 token。启动失败与运行中死亡都在 stderr 报一次并进入 30s 冷却；运行中死亡会丢弃缓存的 client，冷却结束后下个文件操作自动重生服务器。

## 5. 数据流与前后端契约

诊断作为**独立的 `Content` 条目**追加到 `ToolOutput.content`（主结果为 `content[0]`，诊断为其后条目）：

- **给模型**：`src/agent/mod.rs` 的 `render_output` 把整个 `content` 数组扁平化进 tool_result 消息——诊断照常进入模型上下文。
- **给用户**：前端 `conversation.ts` 的 `pairResult` 把 `content[0]`（主结果）与其后条目（诊断）分开：主结果进 `item.result`（`read` 的文件体、`write`/`edit` 的 diff 基底、fileCache 都只吃它），诊断进 `item.diagnostics`，**只在 `RawArgs` 调试折叠区渲染**，标注「发送给模型」。这样保证：诊断进了模型，但不污染主视图；同时用户能在 debug 区看到发给模型的确切内容（透明性）。

> 前端的逻辑改动只有 `pairResult` 的结果拆分与 `Item.diagnostics` 新字段；`ToolBlock`/`registry`/三个 Result 组件只是把该 props 透传到 `RawArgs`。未动状态机（`DESIGN.md` §7 铁律）。

## 6. 模块地图

| 文件 | 职责 |
|---|---|
| `src/lsp/config.rs` | `LspConfig`/`LspServerConfig` + `load(roots)` |
| `src/lsp/protocol.rs` | 手写的最小 JSON-RPC + LSP 线类型（`Incoming`/`Diagnostic`/`Position` 等），全部 `serde(default)` 便于增长 |
| `src/lsp/client.rs` | `LspClient`：spawn、`Content-Length` 分帧、后台读取任务（解复用响应/推送/服务器请求）、`initialize`、`sync_document`、`diagnostics`、通用 `request` |
| `src/lsp/mod.rs` | `LspManager`：懒启动、扩展名路由、`diagnostics()`、`render_diagnostics()` |

工具接线：`read`/`write`/`edit` 各加 `Option<Arc<LspManager>>` 字段 + `with_lsp()` builder（`::new(workspace)` 签名不变）；`app.rs::register_profile_tools` 注入；`Assembled.lsp_manager` 持有存活（同 `mcp_clients`）。

## 7. 后续

- 真正的 LSP 工具：`definition`/`references`/`hover`/`document_symbols`（客户端层已就绪，`request()` + 位置编码已记录）。
- 位置编码消费：当前诊断直接渲染服务器行号，未来 request 类工具需用 `position_encoding` 做 UTF-8/UTF-16 偏移换算。
- `languageId` 映射：内置常见扩展名→LSP `languageId` 映射（`src/lsp/mod.rs` 的 `language_id_for`），未命中回退为扩展名本身；接入更严格的服务器时按需扩充。
- profile/UI 层暴露 LSP 开关与超时配置。
