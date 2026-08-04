# Ominiforge LSP 集成

本文档描述 LSP（Language Server Protocol）集成。Phase 1 把语言服务器的**诊断（diagnostics）**作为 `edit`/`write` 的辅助信息返回给模型。它不是模型直接调用的工具——`doc/tool-protocol.md` §2 里 `lsp` 作为 built-in 工具类目是给后续阶段预留的。

## 1. 定位（Phase 1）

- **辅助，非工具**：模型不显式调用 LSP。`edit`/`write` 成功后，若该文件有启用的语言服务器，把诊断追加到工具结果里。
- **面向增长**：客户端层（`src/lsp/`）做得足够通用——后台读取任务、id 解复用、通用 `request()`——后续可直接接 `definition`/`references`/`hover` 等真正的 LSP 工具，无需重写。
- **性能是核心约束**：语言服务器（rust-analyzer 等）索引慢，文件操作绝不能卡在索引上（见 §4）。

**本阶段不做**：独立的 `lsp` 工具、rename/format/code-actions、补全、SSE/远程服务器、在 sandbox 内运行服务器。

> 自动格式化（format）**不属于** LSP——它是无状态一次性进程调用，失败语义与本系统相反，见 `doc/format.md`。

## 2. 与 MCP 的异同

`src/lsp/` 结构上镜像 `src/mcp/`（子进程 + JSON-RPC + `Tool`-trait 适配），但 LSP 有四处关键差异：

| 关注点 | MCP | LSP |
|---|---|---|
| 分帧 | 换行分隔 JSON | `Content-Length:` 头 |
| 读取 | 同步「读到我的 id 为止」 | 后台读取任务：服务器**主动推送** `publishDiagnostics` |
| 状态 | 无状态调用 | 查询前必须 `didOpen`/`didChange` 同步文档 |
| 位置 | 无 | 0-based，默认 UTF-16（协商 UTF-8） |

语言服务器在**宿主**上启动（同 MCP，非 sandbox）。

## 3. 配置：分层 + 内置注册表

开箱即用的核心是：**常见语言无需任何配置**。配置分四层，高优先级 shadow 低优先级（与 `mcp.toml` 的合并语义一致，按 `name` 判同）：

```text
内置注册表（编译进 binary，最低层）   rust-analyzer / pyright / ruff / typescript-language-server / …
  ↑ 被 shadow
全局用户配置  <全局 root>/config/lsp.toml   可禁用内置条目（enabled=false）、改 command/超时
  ↑ 被 shadow
workspace 配置  .omini/config/lsp.toml      项目级覆盖 / 新增自定义服务器（最高层）
```

- **内置注册表**为常见语言提供 `command` + `args` + `extensions`。用户什么都不写，碰到对应扩展名即用（二进制经 PATH / direnv env-overlay 解析，见 `doc/env.md`）。
- **`enabled`（墓碑语义）**：高优先级层写一条同名 `enabled = false`，使该服务器在合并结果中**整条消失**——这是"关闭某个内置默认"的方式，不是新增一个缺 `command` 的畸形条目。
- 内置条目里的 `command`/`extensions`/`args` 可被更高层同名字段覆盖。

### 一个语言可挂多个 server

`extensions` 路由是**多对多**：一个文件匹配到**所有**声明其扩展名的启用服务器，不是"第一个命中"。典型组合是 Python 同时开 `pyright`（类型/语言服务）+ `ruff`（linter）。

```toml
[[servers]]
name = "pyright"
command = "pyright-langserver"
args = ["--stdio"]
extensions = ["py", "pyi"]

[[servers]]
name = "ruff"
command = "ruff"
args = ["server"]
extensions = ["py", "pyi"]
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✓ | 唯一标识，用于日志与分层 shadow |
| `command` | ✓ | 可执行文件（stdio 传输） |
| `args` | ✗ | 命令行参数 |
| `env` | ✗ | 额外环境变量 |
| `extensions` | ✓ | 该服务器处理的文件扩展名（不带点）。文件路由到**所有**扩展名匹配的启用服务器 |
| `enabled` | ✗ | 默认 `true`。高优先级层设 `false` 以禁用（含禁用内置条目） |
| `diag_timeout_ms` | ✗ | 默认 400。同步文档后等待新诊断的硬上限 |
| `init_timeout_ms` | ✗ | 默认 2000。首次触碰该语言时等握手的上限；全量索引在后台继续，不受此限 |

## 4. 性能模型

1. **懒启动、非阻塞初始化**：某语言的首个文件触发服务器启动；`initialize`/索引在后台跑。触发操作最多等 `init_timeout_ms`，随后照常返回；诊断没就绪就不附带，绝不卡在全量索引上。
2. **热复用**：服务器与打开的文档存活整个 session（`LspManager` 由 `Assembled` 持有，见 §5），预热后一次 `didChange` → 单文件重新发布是亚秒级。
3. **有界等待**：每次操作对**每个**匹配服务器最多等 `diag_timeout_ms`；多服务器并发收集，实际等待是单窗口（max，非 sum）。
4. **版本门控**：接受版本 ≥ 我们发出的 `didChange` 的发布（或该 uri 自我们同步后出现的新发布），绝不附带编辑前的陈旧诊断。服务器省略版本时，取窗口内我们同步后的首个发布（best-effort）。
5. **不支持扩展名快路径**：无对应启用服务器 ⇒ 立即返回，不启动、不等待。
6. **fail loud，不静默也不打扰**：附带真实诊断，绝不编造。拿不到诊断时（无服务器 / 启动失败 / 超时 / 服务器确认干净）不附带任何内容——模型侧「无诊断块」即「当前无可说」，不占用 token。启动失败与运行中死亡都进入 30s 冷却；运行中死亡会丢弃缓存的 client，冷却结束后下个文件操作自动重生服务器。
   - **显式配置 vs 自动启用**：用户**显式配置**的服务器启动失败记 `tracing::warn!`（fail-loud）；**内置注册表自动启用**的服务器因二进制缺失而失败，只记 `tracing::debug!` 并标记 `not-installed`——自动检测不该为没装的工具刷警告。

## 5. 数据流与前后端契约

诊断作为**独立的 `Content` 条目**追加到 `ToolOutput.content`（主结果为 `content[0]`，诊断为其后条目）。**多服务器时聚合为一个块**，每条诊断标注来源服务器（`via pyright` / `via ruff`），统一受渲染上限截断（不是每个服务器各一份）：

- **给模型**：`src/agent/mod.rs` 的 `render_output` 把整个 `content` 数组扁平化进 tool_result 消息——诊断照常进入模型上下文。
- **给用户**：前端 `conversation.ts` 的 `pairResult` 把 `content[0]`（主结果）与其后条目（诊断）分开：主结果进 `item.result`，诊断进 `item.diagnostics`，**只在 `RawArgs` 调试折叠区渲染**，标注「发送给模型」。诊断进了模型，但不污染主视图。

> 前端的逻辑改动只有 `pairResult` 的结果拆分与 `Item.diagnostics` 新字段；未动状态机（`DESIGN.md` §7 铁律）。

## 5.1 状态暴露（RuntimeInfo）

每个服务器的状态在 Detail Rail INFO 的 LSP section 展示，数据来自 `RuntimeInfo.lsp`：

```text
{ name, extensions, state }
state = "starting" | "running" | "failed"
```

**列出本 session 所在 root 激活的服务器**（共享语义见 §5.2）：`status()` 只返回触碰过对应语言文件的服务器（spawn 中的 `starting`、应答中的 `running`，或启动/同步失败进冷却的 `failed`）。从未触碰的服务器——包括内置注册表里本项目用不到的语言的默认条目——一律不列。因此 rust+ts 项目只会看到 rust-analyzer / typescript-language-server，绝不显示 clangd / gopls。LSP 区在 Detail Rail 最底部（Info/Context/Stats 之后）；无激活服务器时整区不渲染。

- `starting`：已 spawn、`initialize` 握手完成但尚未就绪（启发式 (a1)：未首次成功返回诊断）。琥珀色，是「索引中」的瞬态指示——输入区上方的「`<name>` 索引中…」读的就是它。
- `running`：client 存活，正在应答诊断。
- `failed`：上次启动/同步失败，处于 30s 重试冷却（`doc/lsp.md` §4.6）。

**接线**：`LspService.status(root)` 生成快照（root 级共享服务，见 §5.2）→ `registry.lsp_status(root)` → `session_runtime` handler 按 `meta.workspace` 查出 root 并入 `RuntimeInfo.lsp`。root 无激活服务器时 `lsp` 为空——服务器本就懒启动，无可报。

**为什么走 RuntimeInfo 而非新建事件流**：LSP 状态只在文件操作（`edit`/`write`）时变化，操作之间完全静止。前端 `runtime` 在 session 挂载时加载一次、并在每次 `turn_settled` 后重取——一轮里的文件操作可能 spawn/改变了服务器状态，turn 结束正是刷新的时机。例外是 `starting` 瞬态：它可能在两次 turn 之间（后台索引中）就绪，故前端在有 `starting` 服务器时每 2s 轮询一次 `runtime`，就绪/失败后停轮询、提示消隐。除此之外挂在 RuntimeInfo 上**对用户而言即实时**，无需给 `LspService` 接事件总线、无需新 SSE 通道。

## 5.2 Root 级共享与生命周期

**共享粒度 = 服务器 `root_uri`（worktree 根），不是 session。** 此前每个 session 的 `Assembled` 各持一份 `LspManager`，N 个 session 开同一 workspace 的 rust-analyzer 就 spawn N 次、付 N 份索引与内存。现在服务器实例由进程级 **`LspService`**（挂 gateway registry，仿 `SandboxManager` 分层）按 `(root_uri, server-name)` 单例持有；每个 session 的 `LspManager` 只是路由到共享实例的薄壳（工具签名不变）。同一 root 的 N 个 session 共享同一份索引与内存；同一 workspace 的不同 git worktree（将来）**绝不共享**——root_uri 不同，文件内容与诊断状态不同。

### 并发（共享下的正确性）

- **同键单 spawn**：`get_or_spawn` 持有该服务器的 `client` 锁跨 `connect().await`，并发首触只 spawn 一个。
- **per-uri 串行**：`sync_document` 先拿该 uri 的 `doc_locks` 锁再发 `didOpen`/`didChange`——同一文件版本单调递增，不同文件并行。
- **崩溃重生共用同键锁**：运行中死亡丢 client（`note_died`），下一次 `diagnostics` 经 `get_or_spawn` 重生，N 个并发发现者只一个真重生。运行中崩溃**不等** 30s 冷却（冷却是给 spawn/握手失败的）。
- **查询并发安全**：诊断走 id 解复用 + 版本门控（既有），锁粒度 per-uri / per-(root,server)，不全局串行。

### 回收：结构事件 + 宽限期，绝不用 server 空闲时间

服务器索引慢（rust-analyzer 几十秒~几分钟），**绝不能**因「server 几分钟没动」就杀——那会让它永远停在「索引没就绪就被杀」。回收用的是结构事件：

- **触发 = 该 root 无活跃 session**（所有 actor idle-evicted/关闭）→ 此时不会有文件操作，杀掉安全。后台 sweeper（`registry.start_lsp_sweeper`，60s 一拍）算出活跃 root 集合，调 `LspService.reclaim_inactive`。
- **宽限期（默认 30 min，`gateway.toml` 的 `lsp_reclaim_grace_secs` 可配）**：root 冷掉后不立刻杀，宽限期内（`last_touched` 距今 < grace）保留——用户切出去一下回来不重索引。server 自身启动慢靠 rust-analyzer 的磁盘持久化索引缓存兜底，ominiforge 能做的是「减少不必要回收」（宽限期），不是「让启动变快」。
- **idle 关文档（内存管理，与杀 server 是两回事）**：常驻 server 跨 session 累积 `didOpen` 的文档会涨内存。sweeper 顺带对存活 server 调 `close_idle_documents`：一个文档超 `doc_idle_close`（默认 15 min）没被 edit/read 就发 `didClose`，server 释放该文档的文本/语法树；**workspace 索引仍在 server 里不丢**，下次触碰重新 `didOpen`（亚秒级）。per-uri 锁表条目随之清。

### 状态机与等待模型

废弃「`init_timeout` 同步等待就绪」的看运气模型（2s 几乎等不到真实 server），改「**不阻塞 + 持续状态指示**」：文件操作永不阻塞在 server 上（铁律不变），server 状态持续可见，让用户分清「还在索引 ≠ 没问题 ≠ 坏了」。状态机 `starting(琥珀) → running(绿) / failed(红)`：

- **`starting` 就绪信号**用启发式 (a1)：握手完成后、首次成功返回诊断前视为 starting。**(a3) 扩展点预留**：对发布了精确「索引完成」信号的服务器（rust-analyzer 的 `experimental/serverStatus`、`$/progress` work-done），可接该信号翻转 starting→running 而不等首次查询——只对存在且值得的服务器接，不一刀切。
- **展示 = 常驻清单 + 瞬态等待指示**：Detail Rail LSP 区（语义为 root 级）+ 输入区上方「`<name>` 索引中…」瞬态行（starting 时出现，就绪/失败后消隐；有 starting 时前端 2s 轮询 runtime）。

## 6. 模块地图

| 文件 | 职责 |
|---|---|
| `src/lsp/config.rs` | `LspConfig`/`LspServerConfig` + `load(roots)`（含内置注册表合并 + `enabled` 墓碑） |
| `src/lsp/registry.rs` | 内置服务器注册表（name/command/args/extensions） |
| `src/lsp/protocol.rs` | 手写的最小 JSON-RPC + LSP 线类型，全部 `serde(default)` 便于增长 |
| `src/lsp/client.rs` | `LspClient`：spawn、`Content-Length` 分帧、后台读取任务、`initialize`、`sync_document`、`diagnostics`、通用 `request`、`close_idle_docs`（idle 关文档） |
| `src/lsp/service.rs` | `LspService`：进程级共享服务（§5.2）——`(root_uri, server)` 单例 map、同键单 spawn、per-uri `doc_locks`、崩溃重生、`starting` 状态机、宽限期回收 + idle 关文档 |
| `src/lsp/mod.rs` | `LspManager`：per-session 薄壳——扩展名路由、`diagnostics()`（聚合多来源 + per-uri 串行）、`render_diagnostics()`；实例生命周期委托 `LspService` |

工具接线：`write`/`edit` 各加 `Option<Arc<LspManager>>` 字段 + `with_lsp()` builder；`app.rs::register_profile_tools` 注入。`LspManager` 由 `app::assemble` 经 registry 传入的进程级 `Arc<LspService>` 构造；`Assembled.lsp_manager` 持有存活（同 `mcp_clients`）。回收由 registry 的 `start_lsp_sweeper` 后台任务驱动（§5.2）。**`read` 不挂诊断**——read 是定位/检查操作，存量问题通常是用户自己的半成品代码，附带会诱导模型去修没人让它修的东西。
## 7. 后续

- 真正的 LSP 工具：`definition`/`references`/`hover`/`document_symbols`（客户端层已就绪）。
- 位置编码消费：当前诊断直接渲染服务器行号，未来 request 类工具需用 `position_encoding` 做 UTF-8/UTF-16 偏移换算。
- `languageId` 映射：内置常见扩展名→`languageId` 映射（`src/lsp/mod.rs` 的 `language_id_for`），可按需并入注册表。
- **Web 配置编辑器**：分层配置的图形化编辑（等价 `PermissionRulesEditor` 的三层解析 + round-trip 纯函数 + 双主题），单独排期，不在本次。
- profile/UI 层暴露 LSP 总开关与超时配置。
