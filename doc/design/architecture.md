<!-- status: current -->
<!-- owner: @OminiForge -->

# Ominiforge 架构设计

## 1. 项目目标

Ominiforge 目标是构建一个使用 Rust 实现的高性能、强执行能力、高扩展性的 agent 平台。系统应能够通过扩展成为 coding agent、个人研究助手、自动化助手，并逐步接入日常生活、软件开发、知识管理、外部应用协作等场景。

系统的唯一用户界面是 GPUI 客户端（见 §3.2）。GPUI 客户端可以运行在本地模式（直接链接 core）或远程模式（连接远程 Gateway）。命令行只作为运维工具（`serve` / `eval`），不是对话入口。

系统还需要具备自我进化能力。自我进化不是指系统未经确认自动修改自身，而是指系统能够基于 session 历史、失败记录、使用频率、成本数据和 tool/skill 运行结果，生成可审查的优化建议、skill 草案、配置变更建议或代码 patch。所有影响系统行为的进化结果都应由用户批准后再应用。

## 2. 核心设计原则

### 2.1 核心无 UI

核心 agent 运行时不应依赖任何 UI 环境。核心只负责执行任务、管理状态、发出事件。UI 层（GPUI 客户端）只负责收集用户输入、渲染事件流、展示状态和提交控制指令。

这样设计是因为核心需要支持本地模式（GPUI 客户端直接链接 core）和远程模式（GPUI 客户端连接远程 Gateway）。如果 agent 逻辑与 UI 绑定，两种模式无法共享核心逻辑。

### 2.2 历史不可变

Session 的原始历史应采用 append-only 模型保存。任何压缩、fork、修正、总结或视图变化，都不应直接改写原始 session 历史，而应创建新的 session 节点、snapshot 或 context view。

这样设计是因为 agent 系统需要支持回放、审计、失败分析、任意位置 fork、自我进化分析和高质量调试。如果历史被原地修改，后续很难判断真实执行过程，也很难从某个旧状态恢复或比较不同分支。

### 2.3 扩展通过 MCP

外部 tool 通过 MCP（Model Context Protocol）标准协议接入。MCP server 是普通进程，拥有完整 OS 能力，任何语言均可实现。系统内置 tool 直接用 Rust 实现，无额外协议开销。

这样设计是因为 agent tool 天然需要完整 OS 能力（shell、LSP、文件操作），WASM 沙箱限制过大无法满足。MCP 是行业标准，生态成熟，无需自定义扩展协议。

### 2.4 可读历史优先，数据库作为索引

完整 session 历史不应只存在 SQLite 等数据库中。数据库适合索引、查询和缓存，但不适合作为唯一历史来源。系统应保存机器可读的 event log（events.jsonl），人类可读展示由 GPUI 客户端从 event log 解析渲染。索引数据库可从 event log 重建。

这样设计是因为把所有历史放入单个数据库会增加迁移、损坏恢复和长期扩展成本。Event log 作为 source of truth，索引数据库随时可重建。

### 2.5 事件驱动

核心执行过程应通过事件流表达，例如文本增量、思考增量、tool call、tool result、usage、artifact 创建、状态变更和错误。GPUI 客户端、gateway、监控系统和外部协议适配层都消费同一套事件。

事件协议采用统一 envelope + 分域 payload enum 设计。所有事件共享 schema_version、seq、session_id、timestamp、source 等信封字段，payload 按 Turn/Model/Tool/Session/Artifact/Error 分域。详见 [`doc/event-schema.md`](./event-schema.md)。

这样设计是因为 streaming agent、事件订阅、监控和回放都天然需要事件流。事件协议稳定后，本地模式和远程模式可以共享行为语义。

### 2.6 进化只生成提案

自我进化系统可以分析历史、发现失败模式、生成优化建议、提出 skill、修改 profile 或生成 patch，但不应默认直接应用这些变化。

这样设计是因为 self-evolution 会影响系统长期行为。如果没有用户确认，系统可能引入错误优化、过拟合某些任务，或修改用户并不希望改变的行为。

## 3. 入口形态

### 3.1 命令行（运维工具）

命令行是运维/自动化入口，不是对话界面。对话交互一律走 GPUI 客户端（最终唯一 UI）。

```text
ominiforge serve            # 起 Gateway（GPUI 客户端远程模式的后端）
```

CLI 应保持可组合、可脚本化、输出结构清晰。未来会在同一二进制中加入 TUI（对话界面），与 `serve` 并列。

**已移除的子命令**：
- `init`：配置通过 GPUI 图形界面管理（见 §24）
- `inspect`：session 分析通过 GPUI 客户端的监控面板（见 [`gpui-app.md`](./gpui-app.md)）
- `eval`：eval 能力仍在 core（`src/eval/`），但 CLI 入口已移除，未来经 TUI/GUI 暴露

### 3.2 GPUI 客户端（唯一 UI）

GPUI 客户端是唯一的用户界面，替代了原计划的 Web 前端和 Tauri 桌面壳。

**核心特性**：
- 基于 GPUI 框架（Zed 的 UI 框架；本体 Apache-2.0，我们经 **zed git pin** 使用，其带入的 ztracing/zlog 为 GPL-3.0-or-later——本项目已转 GPL-3.0-or-later，合法，见 `architecture-decisions.md` §1/§9）
- Agent 对话、session 管理、监控面板为核心功能
- 多机连接（Direct/Tunnel/P2P，自动切换）
- 本地模式（直接链接 core，零网络开销）和远程模式（连接远程 Gateway）

Editor 嵌入（vim 编辑体验）为后置的高级功能，见 §22 与 [`migration-plan.md`](../operation/migration-plan.md) Phase 7。详见 [`gpui-app.md`](./gpui-app.md)。

### 3.3 Web 前端（过渡期保留，最终移除）

Web 前端（SvelteKit）在过渡期保留，用于在 GPUI 客户端完成前提供可用的用户界面。

**过渡期策略**：
- GPUI 客户端功能完备前，Web 前端继续维护
- GPUI 客户端功能完备后，Web 前端停止新功能开发，标记为 deprecated
- 最终移除或保留为只读/轻量入口

详见 [`migration-plan.md`](../operation/migration-plan.md) Phase 6。

### 3.4 Mobile（后续）

手机端作为后续入口，独立开发原生 App，不通过 Web 前端。

主要负责：审批操作、监控查看、通知接收。

详见 [`architecture-decisions.md`](../decisions/architecture-decisions.md) §Feature Request。

## 4. 总体分层

```text
UI Layer
└─ GPUI 客户端（唯一 UI）
   ├─ 本地模式（直接链接 core）
   └─ 远程模式（连接远程 Gateway）

Gateway Layer（仅远程模式需要）
├─ HTTP API（Web 前端过渡期保留）
├─ WebSocket event stream
└─ auth / permission boundary

Service Runtime Layer
├─ session manager
├─ scheduler
├─ event bus
├─ config manager
├─ profile manager
├─ permission manager   # 已实现：工具调用门控 deny/ask/allow，见 doc/permission.md
└─ runtime orchestration

Core Agent Layer
├─ agent loop
├─ planning / execution policy
├─ context manager
├─ memory interface
├─ tool invocation interface
└─ model interface

Extension Layer
├─ built-in tool host
├─ MCP client (server lifecycle, JSON-RPC adapter)
├─ hook registry (Rust trait + shell hook runner)
├─ skill manager
├─ MCP adapter (外部 MCP server → 内部 Tool trait)
├─ ACP adapter
└─ A2A adapter

Infrastructure Layer
├─ session storage
├─ sandbox
├─ monitor / trace / cost accounting
├─ artifact store
├─ search index
└─ evolution worker
```

## 5. Crate 拆分方案

采用多 crate 结构，通过 trait 通信，组合优先。

**拆分原则**：
1. **模块化**：每个功能域是独立 crate
2. **超低耦合**：模块间通过 trait 通信，不直接依赖实现
3. **组合优先**：Application 层组合需要的模块
4. **不重复维护**：共享功能在 core，统一接口

**Crate 结构**：

```text
crates/
  ominiforge/              # core lib：agent、session、event、tool、lsp、format、
                           # gateway、eval 等。纯库，无 GUI 依赖，可独立编译和测试。
                           # crates.io 上发布为 `ominiforge`。
  
  ominiforge-net/          # ClientProtocol 抽象：LocalProtocol（直链 core）/
                           # WebSocketProtocol（连远程 Gateway）。前端连 core 的统一接口。
  
  ominiforge-ui/           # UI 组件库（依赖 gpui）：theme、components、panels。
                           # 含 gpui git 依赖，不发 crates.io。
  
  ominiforge-cli/          # CLI（lib+bin `ominiforge`）：serve 子命令，未来 TUI。
                           # 依赖 core+net；发 crates.io + GitHub Release。
  
  ominiforge-gui/          # GPUI 桌面应用（bin `ominiforge`，占位）：复用 cli 的
                           # 命令面 + GPUI 界面，最终唯一 UI。publish=false，
                           # 只走 GitHub Release 桌面安装包。
```

**依赖方向**：

```text
ominiforge-gui → ominiforge-cli → ominiforge-net → ominiforge
ominiforge-gui → ominiforge-ui → gpui
ominiforge-ui → ominiforge-net → ominiforge

ominiforge（core）不依赖任何上层 crate
```

**Core 内部 module 布局**：

```text
crates/ominiforge/src/
├── core/          # event schema, state machine, core traits
├── session/       # storage, fork, DAG
├── context/       # compaction, injection, prefix cache
├── llm/           # model trait, provider trait
├── provider/      # openai/, xiaomi/
├── tool/          # built-in tool 实现, ToolRegistry, Tool trait
├── mcp/           # MCP client, adapter, server lifecycle
├── hook/          # hook registry, built-in hooks, shell hook runner
├── skill/         # skill lifecycle
├── memory/        # memory interface + stores
├── monitor/       # trace, usage, cost (EventBus subscriber)
├── evolution/     # session analysis, proposal generation
├── agent/         # agent loop, orchestration
├── gateway/       # HTTP/SSE/WS server（GPUI 客户端远程模式的后端）
├── lsp/           # LSP Service（Editor 和 Agent 共享）
├── parsing/       # Tree-sitter Service（Editor 和 Agent 共享）
├── format/        # Formatter Service（Editor 和 Agent 共享）
└── sandbox/       # Sandbox Service（预留，feature request）
```

### 5.1 Feature flags

| Feature   | 控制范围              | 默认 |
|-----------|----------------------|------|
| `provider-openai`  | OpenAI provider | on   |
| `provider-xiaomi`  | Xiaomi MiMo provider | on   |
| `sandbox-boxlite`  | microVM 沙箱后端（Linux + Apple Silicon） | off |

Gateway（axum 栈）是无条件编译的——`ominiforge-cli` 的主用途就是 `serve`。

### 5.2 何时调整 crate 结构

满足任一条件时考虑调整：
- 某 crate 编译时间过长，影响开发效率
- 某 crate 需被外部项目独立引用
- 某 crate 的依赖树过于庞大，影响其他 crate

Module boundary 已画好，调整是机械操作。

## 6. Session 管理

Session 是系统核心能力之一。它不仅是对话历史，还包含执行事件、tool 调用、artifact、监控数据、fork 关系和后续自我进化分析依据。

### 6.1 Session Fork

系统需要支持从任意 session 的任意对话点 fork 出新 session。用户可能在一次对话中出现多个分支问题，如果都放在同一个 session 中，模型上下文会变得混乱。Fork 可以让用户从某个上下文状态开始探索分支，同时保留原 session 继续深入。

Fork 应采用 DAG 结构，而不是复制完整历史。

```text
sess_A
├─ sess_B  # from sess_A event 42
└─ sess_C  # from sess_A event 77
```

### 6.2 Session 存储

采用 append-only event log + index database。

#### 目录结构

```text
.omini/sessions/
  {session_id}/
    session.toml
    events.jsonl
    context_snapshot.json   # 仅 fork/compaction/reconfiguration 时存在
    artifacts/
  {session_id}/
    ...
  index/
    sessions.sqlite         # 查询索引，可从文件重建
    search/                 # 全文检索索引，可从文件重建
```

- 目录名即 session_id，扁平存放，不按时间分片。
- session_id 采用 ULID 格式（时间排序 + 随机，26 字符），`ls` 时自然按创建时间排列。
- 索引数据库可从 session 文件重建，不承担唯一真相角色。

#### session.toml

纯元数据，不含运行时状态，不含 system prompt。

```toml
id = "01J5M3HKEA7V2X3P1YKRN9C4WG"
profile_id = "coding-agent"
created_at = 2026-06-11T10:00:00Z
workspace = "/home/user/project/foo"  # 可选，无则 filesystem tools 受限

[origin]
kind = "new"  # "new" | "fork" | "compaction" | "reconfiguration"
parent_id = "01J5M2..."   # 非 new 时存在
fork_at_seq = 42           # 仅 fork 时存在
```

设计决策：

- **无 status 字段**。Session 不需要显式生命周期状态。Session 存在即可用，任何 session 随时可被 fork。UI 需要区分"当前在用"与"历史"时，从 `last_event_at`（索引数据库缓存）或是否有子 session 派生判断。**例外：归档标记不进 `session.toml`**，而是 session 目录下的 sidecar 文件 `.archived`（§6.2.9），刻意让元数据保持无生命周期状态——归档是带外的"退役"信号，不是 session 身份的一部分。
- **无 reason 字段**。Kind 已足够表达来源语义。
- **parent_id 统一**。Fork、compaction、reconfiguration 都用同一个 parent_id 字段，kind 区分语义。
- **workspace 可选**。CLI 默认填 CWD，GPUI 客户端由用户显式选择，研究/聊天类 session 可不设置。workspace = None 时 filesystem tools 不可用或受限。

#### events.jsonl

每行一个事件。省略 session_id（从目录名获取）。

```json
{"schema_version":"ominiforge.event.v1","seq":0,"timestamp":"2026-06-11T10:00:00Z","source":{"kind":"Runtime","id":"ominiforge"},"parent_event_id":null,"turn_id":null,"payload":{"Session":{"Created":{"profile_id":"coding-agent","tools":["shell","read_file"]}}}}
```

设计决策：

- **不含 session_id**。避免同一 session 内每行重复，节省存储。Session_id 从目录名获取。
- **首条事件为 SessionEvent::Created**。记录初始 config 快照（profile_id、tool list 等），使 replay 自包含。
- **不生成 transcript.md**。人类可读展示由 GPUI 客户端从 events.jsonl 解析渲染。
- **业务事件不进系统日志**。`events.jsonl` 是 agent loop 的唯一真相（source of truth）；gateway/serve 的运维诊断（启动行、MCP 连接失败、sandbox 降级、direnv 慢等）走 `tracing` 到 stderr，由 `RUST_LOG` 控制级别，不写进 session 目录。两者刻意分离：事件日志回答"agent 做了什么"，系统日志回答"服务进程发生了什么"。

#### context_snapshot.json

仅在 origin.kind 非 "new" 时存在。内容为完整 messages 数组，agent loop 启动时直接加载作为初始上下文。

```json
[
  {"role": "system", "content": "你是一个 coding agent..."},
  {"role": "user", "content": "帮我写个函数"},
  {"role": "assistant", "content": "好的，这是实现..."}
]
```

设计决策：

- **格式统一为 messages 数组**。无论 fork、compaction 还是 reconfiguration，context_snapshot 都是同一格式。Agent loop 加载时不关心 origin kind。
- **System prompt 就是 messages 数组中的 system role message**。无独立存储机制。
- **自包含**。子 session 不依赖父 session 即可运行。父 session 可被删除而不影响子 session 功能。

#### Session 诞生方式

| origin.kind | 触发场景 | context_snapshot | 与父 session 关系 |
|-------------|---------|-----------------|------------------|
| `new` | 用户开始新对话 | 无 | 无父 session |
| `fork` | 用户从某点分叉探索 | 父 session 在 fork 点的 context view | 独立运行，可选回查父 session |
| `compaction` | 上下文超限自动/手动压缩 | LLM 生成的摘要（messages 数组格式） | 独立运行，可选回查父 session 细节 |
| `reconfiguration` | system prompt / tool set 变更 | 当前 context view + 新 system prompt | 独立运行 |

所有非 "new" session 共享相同加载机制：读 context_snapshot.json → 作为初始上下文 → 追加新事件。

#### Fork 与 Compaction 语义区分

- **Fork**：精确上下文复制。Context snapshot = 父 session 在 fork 点发给模型的完整 messages。目的是从同一状态探索不同方向。
- **Compaction**：有损压缩。Context snapshot = LLM 对父 session 历史的摘要。目的是在上下文超限时延续对话。
- **Reconfiguration**：配置变更。Context snapshot = 当前 context view 替换 system prompt 后的 messages。目的是保持历史不可变的前提下更新 agent 能力。

三者区别在语义，不在机制。运行时行为一致。

#### 父子 Session 依赖关系

- 子 session 完全自包含。Context snapshot 存储了启动所需的全部上下文。
- 父 session 可被用户删除。删除后子 session 仍可正常运行。
- Compaction 的回溯引用（"之前的细节在父 session 里"）是 **optional**。用于审计和调试，不影响运行。
- Session 之间无硬依赖。不需要维护依赖图来判断"哪些旧 session 不能删"。

#### 并发控制

**Session 级互斥**：

一个 session 同一时刻只允许一个 writer（agent loop）。

```text
.omini/sessions/{session_id}/events.jsonl   # flock(EXCLUSIVE) on this file
```

- Agent loop 启动时对 events.jsonl 执行 flock(EXCLUSIVE)。
- 拿不到锁 → 报错 "session in use by another process"。
- 进程退出或 crash → 内核自动释放 flock，无 stale lock 问题。
- 读取（如 GPUI 客户端 tail events.jsonl）不需要排他锁，flock(SHARED) 或直接读均可，append-only 文件对 reader 安全。

适用场景：CLI 和 Gateway 同时运行时，防止两者对同一 session 写入冲突。

**SQLite 索引并发**：

sessions.sqlite 使用 WAL mode + busy_timeout（5s）。

- 多 reader 并行，单 writer 排队。
- 短暂写冲突自动重试。
- 索引可从 session 文件重建，非关键路径。

**Gateway 内部并发**：

Gateway 是 tokio async 运行时，多请求并发到达：

- 每个 session 一个 agent loop 实例。
- 同一 session 的请求串行化（per-session Mutex 或 actor model）。
- 不同 session 完全并行，无竞争。

#### Session 生命周期：归档与删除

Session 的退役分两级——**archive（归档，安全、日常）** 与 **hard delete（硬删除，危险、罕用）**——两者都是沙箱 `release` 的触发点（[`sandbox.md`](./sandbox.md)：session 被退役才 release，不绑 thread）。

**Archive（已实现）**：

`POST /sessions/{id}/archive`：把 session **永久退役**——从活跃列表移除、释放沙箱，但**保留全部文件**供用户或 agent 之后只读分析。

**Archive 是单向终态，没有 unarchive。** 理由：归档时沙箱环境已被 release，而它无法重建——`events.jsonl` 能重放对话流、workspace 文件本就在用户外部目录完好无损，但**沙箱内部状态**（装的包、guest 的 `/tmp` `/root` `/etc`、运行态）随 release 一并回收。所以"恢复"只能给出一个全新空沙箱，不是当时那个装好环境的 microVM——提供一个名不副实的 `unarchive` 是误导，故不提供。archived session 的全部运行/流入口返回 **410 Gone**；只读入口照常；基于其内容 fork 出**新** session 分析也照常（fork 产生新 session，不复活旧的）。

设计决策：

- **归档标记 = sidecar 文件，不是 `session.toml` 字段**。保住"无 status 字段"原则，schema 零改、向后兼容天然。标记的**存在**即信号，内容不用。
- **`list()` 过滤 archived**。两个活跃枚举入口都不再返回 archived；但按 id 读 `session.toml` / `events.jsonl` 仍可用——这正是"保留供分析"。
- **单点运行门控**。archived 的运行拦截只加在运行入口，覆盖全部、且不误伤只读路径。文件系统是唯一真相源，外部手动增删 `.archived` 下次即生效——但注意手动 `touch` 只切换"列表可见性 + 运行门控"，不触发 release；完整退役须走 API。
- **release 失败则中止归档**（fail loud），不泄漏沙箱环境。
- **幂等**。重复 archive 是 no-op 成功。

**Hard delete（已实现）**：

`DELETE /sessions/{id}`：物理 `rm -rf` 整个 session 目录（`session.toml` + `events.jsonl` + snapshot + artifacts），**不可逆**。

**确认机制 = 必须先 archive。** 未归档的 session 直接 DELETE 返回 **409**（"archive it first"）——这个两步屏障（先 archive 再 delete）就是不可逆操作的显式确认，不需要单独的 confirm token。而且因为 archived session **已经**停了 actor、释放了沙箱（archive 的前序），delete 本身退化成**纯文件系统删除**，不重复 stop/release 逻辑。

设计决策：

- **授权门放在 store 原语层**。`SessionStore::delete` 自身检查 `is_archived`，未归档返回 typed error——一个能 `rm` 掉活 session 的原语是灾难级 footgun，不可逆 fs 操作值得纵深防御，不把安全性只押在上层 handler。
- **删父 session 安全，无级联**。子 session 完全自包含，删父不影响子，故不需要依赖图/引用计数检查。
- **ghost → 404**（先 `read_meta` 存在性检查），二次 delete 自然是 404。

## 7. 上下文管理

系统需要区分 session log 和 context view。

### 7.1 核心概念

```text
Session Event Log    — 不可变真实历史（events.jsonl）
Context View         — 本轮发给 model 的 messages 数组（运行时内存结构）
Context Snapshot     — 新 session 启动时的初始上下文（context_snapshot.json）
```

- Context view 不独立落盘。运行时 agent loop 持有内存结构，每轮只追加。
- 只在创建新 session（fork/compaction/reconfiguration）时才物化为 context_snapshot.json。
- Session 冷启动时从 events.jsonl 重建 context view（或从 context_snapshot.json 加载）。

### 7.2 Context View 结构

从前到后按稳定性排列，保障 prefix cache 命中率：

```text
┌─────────────────────────────────┐
│ system prompt (from profile)     │  ← 稳定前缀
│ tool schemas (按 name 字母序)    │
├─────────────────────────────────┤
│ context_snapshot (if non-new)    │  ← session 内不变
├─────────────────────────────────┤
│ [injection_1]                    │
│ user_1                           │
│ assistant_1 (含 tool calls)      │
│ [injection_2]                    │  ← 只追加，不改写
│ user_2                           │
│ assistant_2                      │
│ ...                              │
│ [injection_N]                    │
│ user_N                           │
└─────────────────────────────────┘
```

### 7.3 Prefix Cache 命中规则

1. System prompt 不含动态内容（不注入当前时间、随机 ID 等）。
2. Tool schema block 按 name 字母序排列，不按加载顺序。
3. 历史消息只追加不改写，中间消息不被修改或删除。
4. 动态注入内容留在历史原位不动，不剥离。
5. Compaction 后新 session 的 snapshot 成为新稳定前缀。
6. Monitor 跟踪每次 request 的 cache_hit_tokens / total_input_tokens 比率。

### 7.4 Compaction 机制

#### 触发方式

- **自动触发**：context view token 数超过 threshold 时触发。
- **手动触发**：用户执行 `/compact` 命令。

#### Threshold 配置

实际上限 = threshold × context_window - max_output_tokens，留出 model 回复空间。

用户可通过 profile 或全局配置修改此值。

#### 行为

Compaction 总是创建新 session，不修改原 session。保证历史不可变。

```text
sess_A (original)
└─ sess_A2 (compaction)
   ├─ origin.kind = "compaction"
   ├─ origin.parent_id = sess_A
   └─ context_snapshot.json = LLM 摘要（messages 数组格式）
```

创建后自动切换到新 session 继续对话。原 session 完整保留，可回查。

#### 手动压缩命令

```text
/compact                — 全量摘要，创建新 session 并切换
/compact --keep-last 3  — 保留最近 3 轮完整对话，其余摘要
```

#### Origin 元数据

session.toml 中记录压缩来源信息：

```toml
[origin]
kind = "compaction"
parent_id = "01J5M2..."
source_seq_range = [0, 150]     # 被摘要的事件范围
model_used = "deepseek-r1"      # 执行摘要的模型
prompt_template = "default"     # 压缩 prompt 模板标识
created_by = "auto"             # "auto" | "manual"
```

#### 质量评估

初期不做自动评估。Monitor 记录 compaction 事件，供 evolution worker 后续分析。

后续可选方案：
- 关键事实抽取对比（retention rate）。
- 回归测试（对压缩后 context 问历史问题）。
- 用户行为信号（compaction 后快速 fork 回去 = 质量差）。

### 7.5 动态注入（Injection）

#### 注入者

Runtime 的 Context Manager 组件。在 agent loop 构建本轮 model request 前触发：

```text
Agent Loop 准备发 model request
  → Context Manager 触发注入流程
    → Memory 检索
    → RAG 召回（如果有）
    → ACP 推送的编辑器状态（如果有）
  → 注入内容追加到 context view
  → 发出 model request
```

Hook（`model:request:before`）也可间接 modify 注入内容。

#### 持久化

注入内容同时写入 events.jsonl（`InjectionEvent::ContextInjected`，含 source / content /
token_count）并保留在 context view 中。

- Context view 中历史 injection 不移除，保障 cache 命中。
- Events.jsonl 完整记录，保障 replay 和分析。
- Compaction 时所有历史 injection 被摘要浓缩。

#### 成本控制策略

动态注入必须节制，以降低上下文膨胀和成本：

执行规则：

- 能不注入就不注入：只有当前 turn 明确需要才加。
- 不重复注入：同 hash 内容已在当前 context 可见则跳过。
- 只注入最小必要片段：优先 snippet / summary / artifact ref，不塞全文。
- 大内容进 artifact store：context 里只放摘要 + artifact 引用。
- Source 排序稳定：Memory → RAG → ACP → Hook，避免无意义 cache 变化。
- Monitor 记录被丢弃的候选（dropped count / reason），events 只记录实际注入内容。

### 7.6 Agent Loop 中 Context View 的生命周期

```text
Session 创建
  → 加载 context_snapshot.json（如有）或构建空 context
  → 设置 system prompt + tool schemas 作为稳定前缀

每轮：
  → Context Manager 执行 injection
  → 追加 injection 到 context view
  → 追加 user message 到 context view
  → 检查 token 数是否超 threshold
    → 超过：触发 compaction → 创建新 session → 切换
    → 未超过：发 model request
  → 追加 assistant response 到 context view（含 tool calls / results）
  → events.jsonl 同步写入对应事件
```

### 7.7 与其他子系统的关系

- **Session Storage**：compaction 创建新 session，遵循已定义的 session.toml + context_snapshot.json 格式。
- **Event Schema**：新增 InjectionEvent payload 类型。
- **Monitor**：跟踪 cache hit rate、injection token count、compaction 频率。
- **Evolution Worker**：分析 compaction 事件、cache hit 趋势，可建议调整 threshold 或 injection 策略。
- **Profile**：threshold 和 injection 配置可在 profile 级别覆盖。

## 8. Todo 系统（turn 内工作清单）

Todo 是 **turn 内**的工作清单机制，把一个较长目标拆成有序步骤，让 agent 在多 round 推进中不迷失方向。

### 8.1 定位与边界

Todo 属于架构 `Core Agent Layer` 中 `planning / execution policy` 的落地。名字刻意叫 `todo` 而非 `plan`：它只是一份执行中的打勾清单，"规划"这个名字留给后续更大的系统。

**Todo vs Task（Phase 4）**：

| 维度 | Todo | Task（Phase 4） |
|------|------|-----------------|
| 生命周期 | turn 内，turn 结束即销毁 | 跨 session，持久化 |
| 作用 | agent 自我提醒、防跑偏 | 组织层任务管理 |
| 校验 | 无 reviewer | reviewer agent 验证交付 |
| 状态机 | step 五态 | backlog→running→pending_review→delivered |
| 存储 | SessionRuntime 内存 + events.jsonl | 任务库 + workspace 展示 |

Todo 不负责跨 context 的拆分。当任务大到单个 context 装不下时，那是 subagent（Phase 4/5）的边界，不是 Todo 的职责。

### 8.2 设计原则

- Todo tool 只管理清单状态，**不执行任何动作**（不读文件、不跑命令）。planning 与 doing 分离。
- Todo 状态权威副本存于 `SessionRuntime`（内存，跨 turn 存活），事实记录仍是 events.jsonl 中的 `ToolEvent`，不违反历史不可变。
- 操作式（op-based）增量更新，不整表替换：便于审计、便于 TurnState 维护、省 token。
- 使用规范写在 tool descriptor 的 `description` 里，不写进 profile 的 system prompt。
- 所有 step 必须到达终态才能结束 turn。
- 系统注入文本用 `<reminder>...</reminder>` 包裹，与用户输入区分。

### 8.3 Step 语义

五态：非终态 `Pending` / `InProgress`；终态 `Completed` / `Cancelled` / `Blocked`。

模型偷懒（把难做的 step 直接取消）是真实风险，必须严格区分两种合法停止：

- `Cancelled` = 步骤**本身**客观不可达（调用了不存在的 tool、写入无权限路径）。
- `Blocked` = 步骤**可达但前提缺失**，需用户补全（缺 API key、需设环境变量、需用户决策）。

两者都要求 `reason` 且必须具体。tool descriptor 明确声明：**不允许因"太难"或"不想做"而 cancel/block，只有客观障碍才合法**。

### 8.4 Turn 退出条件：完成度门（completion gate）

**核心规则：todo 清单存在时，所有 step 必须到达终态才能结束 turn。**

模型的合法退出路径：
1. 无清单（琐碎任务）
2. 所有 step 终态

### 8.5 早期卡死检测

某 `in_progress` step 连续多 round 无进展时，注入一次性提醒建议取消或重构清单。"早发现"是语义级别（某步推进停滞），不是 token 耗尽级别。

### 8.6 Round-Budget Reminder（软预算提醒）

每个 turn 和每个 todo step 运行在一个**软 round 预算**下。不同于 `max_rounds` 的绝对硬顶，软预算只**提醒**，不强制停止——模型自己决定是收尾、拆步骤，还是继续。

### 8.7 多 turn 行为与恢复

todo 是 session-scoped，跨 turn 存活。turn 结束只销毁 `TurnState`（turn_id、round、计数器），todo 与 context 留在 `SessionRuntime`。

- **blocked 步骤跨 turn 延续**（todo 必须跨 turn 的核心理由）：turn 因 `blocked` 干净结束并把原因带给前端。下个 turn 模型同时看到用户新输入 + 当前清单状态，自行判断继续还是覆盖。
- **恢复（resume）**：每个 todo op 都已写入 events.jsonl。resume 时按时间序回放 todo op，确定性重建当前清单——符合"events 是 source of truth，内存结构可重建"。

## 9. Tool 与 Hook 系统

系统支持两类 tool 和两类 hook，统一通过 ToolRegistry 和 HookRegistry 管理。

### 9.1 Tool 分类

```text
Tool
├─ Built-in（Rust 代码，编译进 binary）
│  ├─ read, write, shell, search, lsp, ...
└─ MCP（外部 MCP server，stdio/SSE 通信）
   └─ 用户自建 / 社区 / 第三方 SaaS
```

Agent loop 对两类 tool 使用统一 Tool trait，不区分来源。MCP 是唯一外部扩展机制。

### 9.2 Hook 分类

```text
Hook
├─ Built-in hook（Rust trait impl）
└─ User hook（shell command / 脚本）
```

Hook 通过 stdin/stdout JSON 协议与 shell hook 通信。Before hook 可 pass/modify/block，after hook 仅 observe。

### 9.3 MCP Server 管理

MCP server 配置在 `.omini/config/mcp.toml`。Agent 启动时自动启动配置的 server，通过 MCP 标准协议（JSON-RPC over stdio/SSE）通信。一个 MCP server 可暴露多个 tools。

## 9a. Provider 系统

系统需要支持多个模型 provider，包括 Xiaomi MiMo、OpenAI-compatible provider、主流云模型和自部署模型。

Provider 不应把私有 DTO 泄漏到 core agent。Provider adapter 负责把外部协议转换成内部稳定事件和消息类型。

```text
External provider response
→ provider adapter
→ core AgentEvent / ModelEvent
→ agent loop
```

这样可以避免 agent loop 依赖某个 provider 的 JSON shape，也便于新增 provider。

**Provider 来源与装配解耦**：session 装配（`app::assemble`）不绑定 provider 的*来源*。`ProviderSource` 把「provider 从哪来」显式化——`Configured`（正常路径：解析 `providers.toml` 并 `provider::build` 构建适配器）或 `Injected`（注入一个已构建的 `Arc<dyn Provider>`，跳过配置文件与凭证要求）。注入路径服务于集成测试与本地合成运行：经 `SessionRegistry::new_with_provider` / `LocalProtocol::new_with_provider` 注入 `llm::ScriptedProvider`（按脚本回放 `StreamEvent`，零网络），即可驱动一轮完整 agent 对话做端到端验证。装配本身（工具/沙箱/环境/LSP）对两种来源一视同仁。

## 10. Skill 系统

Skill 是对高频任务、最佳实践、工具组合、提示词流程和执行策略的封装。Skill 应支持创建、更新、失效检测和优化。

自我进化系统可以根据 session 历史发现高频任务并提出 skill 草案，也可以发现失败或过期 skill 并提出修改建议。

## 11. Hook 系统

Hook 用于在特定事件前后插入轻量逻辑，例如 session start、tool call before/after、model request before/after、artifact created、task completed 等。

Hook 实现为 Rust trait（内置 hook）或 shell command（用户自定义 hook）。详见 [`doc/hook-protocol.md`](./hook-protocol.md)。

## 12. MCP、ACP 和 A2A

MCP、ACP、A2A 应作为 protocol adapter 层，而不是侵入 core。Core 使用自己的事件和 trait，协议层负责转换。

```text
core events/types
├─ MCP adapter
├─ ACP adapter
├─ A2A adapter
├─ JSON-RPC adapter
└─ WebSocket adapter
```

ACP 主要用于让编辑器或外部应用接入 Ominiforge。A2A 用于 agent 间协作，后续可能成为多 agent 协作核心。

## 13. Memory 系统

Memory 系统需要支持 agent 跨 session 记忆。它应与 session 历史区分：session 是完整事实记录，memory 是经过提炼、可检索、可更新的长期知识。

Memory 应支持不同作用域：

- user memory
- project memory
- profile memory
- skill memory
- tool memory
- global memory

Memory 写入应可追溯来源 session，避免无法解释的记忆污染。

## 14. Profile 系统

Profile 用于定义不同 agent 身份和能力组合。例如 coding agent、research agent、daily assistant。Profile 应组合以下内容：

- system prompt
- model/provider preference
- tool set
- skill set
- permission policy
- sandbox policy
- memory scope
- context policy
- cost policy

Profile 不应复制核心逻辑。它是运行时配置组合。

## 15. 配置管理

配置系统管理全局 / 项目 / profile / provider / tool 配置与安全策略，支持层级覆盖（`default → user → project → profile → session override`）。配置语言、分层、图形界面与多机同步的定义见 [`config-lua.md`](./config-lua.md) 与 §25。

敏感信息（API key、token）不进普通配置文件，用环境变量、secret store 或受控凭据管理（见 [`config-lua.md`](./config-lua.md) §8）。

## 16. 监控系统

监控系统是核心能力，不是附加功能。它需要记录：

- token usage
- cache hit / cache miss
- provider latency
- model request/response metadata
- tool call latency
- tool failure reason
- sandbox resource usage
- session duration
- cost estimate
- event trace
- artifact lineage

这些数据用于三类目标：

1. 成本统计和控制。
2. 调试和复盘。
3. 自我进化分析和优化。

监控记录应与 session event log 关联，但可在独立索引中聚合。

## 17. 监控与沙箱

所有 tool 执行统一经过 event journal 记录（ToolEvent），支持全量审计和后续分析。详见 [`doc/sandbox.md`](./sandbox.md)。

Shell tool 沙箱分阶段：初期无沙箱直接 spawn（本地使用），后续可选容器隔离（server 部署），远期支持可复现快照。

MCP server 作为普通进程运行，安全性靠用户信任（安装行为即授权）。未来 marketplace 可通过签名 + 审核 + 可选容器隔离增强安全。

## 18. Gateway 与 Scheduler

Gateway 是 GPUI 客户端远程模式的后端，不应承担核心调度逻辑。GPUI 客户端（远程模式）、Web 前端（过渡期保留）、第三方应用和通知系统都通过 gateway 接入。

日常任务应由 scheduler 触发，由 service runtime 创建或恢复 session，再由 agent 执行。Gateway 负责把执行状态和结果推送给外部应用。

```text
scheduler
→ service runtime
→ session manager
→ agent execution
→ monitor
→ gateway notification
```

### 18.1 部署模型

Gateway 以用户级服务运行，不是系统级服务。

```bash
systemctl --user enable ominiforge-gateway
loginctl enable-linger $USER   # 确保 logout 后服务继续运行
```

- 用户级服务与 CLI 共享同一 UID、home 目录、`.omini/` 数据。
- CLI 不连接 Gateway。CLI 和 Gateway 各自独立执行 agent loop，通过共享文件系统保持数据一致。
- `ominiforge serve` 可作为前台模式运行（开发/临时使用）。
- 多用户/服务器级部署（系统级服务 + tenant 隔离）为后续扩展，初期不支持。

Gateway 存在的意义是 CLI 无法覆盖的场景：GPUI 客户端远程模式、定时任务（scheduler 需要常驻进程）、多设备同时访问 session。

### 18.2 Workspace（工作目录）

工作目录是 session 属性，不是 runtime 属性。

```toml
# session.toml
[origin]
kind = "new"
workspace = "/home/user/project/foo"   # 可选
```

各入口的 workspace 来源：

| 入口 | workspace |
|------|-----------|
| GPUI 客户端创建 session | 用户显式选择，或不指定 |
| Scheduler 触发 | 任务定义中声明 |

- workspace = None 时，filesystem tools 不可用或受限（研究、聊天、规划类任务）。
- workspace = 具体路径时，tool 沙箱范围 = workspace + 额外授权路径。
- 不存在全局"运行时工作目录"概念。每个 session 自己知道自己在哪。

## 19. 自我进化系统

自我进化系统应作为后台 worker 和手动命令共同存在。

触发方式：

- cron-like 定期分析。
- 用户手动触发。
- 后续可支持达到一定 session 数量或失败率后触发。

产物目录建议：

```text
.omini/
  evolution/
    runs/
      evo_2026-06-10_020000/
        report.md
        failures.md
        skill_candidates/
        stale_skills.md
        cost_analysis.json
        proposals/
          proposal_001.toml
          patch.diff
```

生命周期：

```text
observed → proposed → approved → applied → evaluated
```

系统可以生成报告、skill 草案、profile 修改建议、tool 改进建议和 patch diff。应用阶段必须经过用户批准。

## 20. AGENTS.md / 项目指引文件

`AGENTS.md` 是放在仓库里、专门写给 AI agent 的指引文件（构建/测试命令、代码规范、注意事项），
与 `README.md`（写给人）互补。规范见 <https://agents.md/>。内容是自由格式 Markdown，无强制字段。

本项目按目录解析文件名：**优先 `AGENTS.md`，缺失则回退 `CLAUDE.md`**，让既有 Claude 项目零改动可用。

### 20.1 两层模型

指引文件可散落在 workspace 各级目录。注入分两层，避免「每读一个文件就注入一次」的开销：

1. **根目录（always-on）**：`<workspace>/AGENTS.md`（或 `CLAUDE.md`）在 `assemble` 时读取一次，
   追加到 system prompt 末尾（在 skill index 之后）。它始终在前缀缓存里，零 per-round 成本。

2. **子目录（懒加载，一次性）**：agent 通过 `read`/`write`/`edit` 触碰某个文件时，从该文件所在
   目录向上查找**最近**的指引文件（到 workspace 根之前为止——根目录那份已在 system prompt），
   命中且**本 session 尚未加载过**则作为一条 `InjectionEvent` 注入，去重后不再重复。

`shell`、MCP tool、`todo` 控制 tool 没有单一路径，不触发子目录加载。

### 20.2 注入时机与去重

- **时机**：子目录指引在一个 round 的**所有 tool 结果落库之后**才注入（注入是一条 `User` 消息，
  必须排在 assistant 的 tool_calls 与对应 tool 结果之后，否则破坏 provider 要求的配对）。
- **去重键**：指引文件相对 workspace 的路径（如 `src/api/AGENTS.md`），存于
  `SessionRuntime.loaded_guidance: HashSet<String>`。在发现时**同步**检查并写入，因此：
  - 同一 round 内多个 tool call 命中同一目录 → 只注入一次；
  - 跨 round 再次触碰同一子树 → 不再注入。
- **路径选择器**：`read` 的 `:N-M` / `:N+C` / `:raw` 后缀在发现前剥除。
- **越界路径**：解析后逃出 workspace 的路径不注入（绝不读 workspace 外的文件）。

### 20.3 注入格式

正文逐字透传，仅包一层定界符，`path` 属性既给 model 标注来源，又供 resume 还原去重键：

```
<project-guidance path="src/api/AGENTS.md">
<AGENTS.md 正文>
</project-guidance>
```

`InjectionSource::ProjectGuidance`（见 `doc/event-schema.md` §3.6）标识此类注入，便于 monitor 路由。

### 20.4 Resume

system prompt 不入事件日志，根目录指引每次 resume 由 `assemble` 重新读取——始终最新。

子目录指引的正文作为 `InjectionEvent` 已在日志里，`rebuild_runtime` 照常重放为 `User` 消息；
`rebuild_loaded_guidance` 额外扫描 `ProjectGuidance` 注入、用 `label_from_wrapped` 解析回 `path`
标签填充去重集，使 resume 后的 session 不会对同一子树重复注入。

### 20.5 边界与已知限制

- 子目录发现从「被碰文件的父目录」起步：对一次 `read` 目录的调用，用的是该目录的父级，不含目录自身的
  指引文件。属边角情况，v1 不特殊处理。
- 「最近优先」是注入语义（只注入最近一份），不做父级覆盖合并；根目录那份恒在 system prompt，因此
  实际模型同时看到「根 + 最近子目录」两份，子目录在后（按新近度优先级更高）。
- 本特性纯读取，`AGENTS.md` 由用户自行编写。

## 21. 环境集成（direnv）

workspace 的开发环境由它自己的 `.envrc` 声明——nix flake、uv、或别的什么都无所谓，统一以 direnv 为公分母。ominiforge 在会话组装（`app::assemble`）时把 `direnv export json` 的结果作为**环境 overlay** 应用到该 session 派生的一切子进程：shell 沙箱、MCP 服务器、LSP 语言服务器。设计目标：**环境求值的成本永远不让用户感知**。

### 21.1 链路

```
POST /workspaces（record_workspace）
   └─ 后台预准备：direnv export json（≤300s）→ 写快照（顺带预热 direnv 自己的 .direnv/）

assemble（每次会话冷启动 / resume；CLI 与 gateway 同一入口）
   ├─ 无 .envrc → 空 overlay，零开销
   ├─ direnv export json（≤2s 快通道）
   │    ├─ 成功 → 过滤 DIRENV_* → 写快照 → 使用
   │    └─ 超时/失败 → 读快照：
   │         ├─ 命中 → 使用快照（warn 标注快照年龄）+ 后台刷新
   │         └─ 未命中 → 空环境 + warn（提示 direnv allow / 在 shell 里验证 .envrc）+ 后台预准备
   └─ overlay → sandbox（shell 工具）/ MCP / LSP
```

实现：`src/env.rs`。`session_env` 是 assemble 的热路径；`refresh_cache` 是后台任务体；`record_workspace`（`src/gateway/registry.rs`）在登记 workspace 后 fire-and-forget 触发后者。

### 21.2 两层缓存

- **direnv 自己的缓存**（项目内 `.direnv/`）：`use flake` 等 stdlib 按 watch 文件（`flake.nix`/`flake.lock`）指纹缓存求值结果。我们每次都走 `direnv export json`，没有任何绕过；热缓存下亚秒返回，输入变了 direnv 会正确地失效重估。后台预准备/刷新的首要作用就是**预热这层缓存**——昂贵的 nix 求值在没有用户等待的地方付掉。
- **快照缓存**（`<config root>/workspaces-env/<workspace-id>.json`，id = 工作区路径的 FNV-1a，与 `WorkspaceId` 同源）：上次成功导出的副本（带 `prepared_at`），只在 direnv 当下慢/失败时兜底。「可能陈旧但完整」严格好于「空环境」。

**新鲜度语义**：`.envrc`/`flake.nix` 刚改 → 当次 assemble 拿到快照（旧环境），后台刷新完成后**下一个 session** 拿到新环境。staleness 上限 = 一次重估的时长，且对用户无感。快通道每次都问 direnv，所以只要 direnv 缓存是热的，拿到的就是新鲜值。

### 21.3 信任与开关

- 信任模型完全交给 direnv：`.envrc` 必须 `direnv allow`，ominiforge 不绕过、不代答。
- direnv 未安装 / `.envrc` 未 allow / 求值失败：warn 后按「无 workspace 环境」运行，绝不让无关 workspace 的会话挂掉。
- `--no-dotenv` 是总开关（命名是历史原因，实际同时关闭 direnv 激活与 `.env` 加载）。
- 活跃会话持有启动时的环境快照；改 `.envrc` 不追溯已在运行的会话（resume/冷启动重新求值）。

### 21.4 GC

快照与 workspace 配置同生命周期：`DELETE /workspaces/config/{id}` 在删 `<id>.toml` 时一并删除快照；孤儿处理沿用 [`gateway.md`](./gateway.md) 的显式 GC 模型。

### 21.5 已知局限

- **boxlite 沙箱后端不应用 env overlay**：overlay 值是宿主路径（如 `/nix/store/...`），guest 里没有挂载点了无意义——待 [`sandbox.md`](./sandbox.md) 的 `/nix/store` 挂载设计落地后再接。passthrough（默认后端）正常。
- env 求值的是**宿主**环境；服务器类子进程（MCP/LSP）本就跑在宿主，与 sandbox 内的 shell 共享同一份 overlay。

## 22. Editor 系统（后置）

Editor 嵌入是**后置的高级功能**，不在当前架构主线（见 [`migration-plan.md`](../operation/migration-plan.md) Phase 7）。原 Neovim `nvim --embed` 子进程方案已否决（非自包含、与产品定位冲突），详细调研与候选路线见 `doc/research/editor_embed_report.agent.final.md`。启动条件与选型待 Phase 7 重新评估。

## 23. 通信协议

GPUI 客户端与 Core 之间的通信通过统一的 `ClientProtocol` trait 抽象。

底层传输可插拔：本地模式（`LocalProtocol`，直接链接 `ominiforge`（core），零网络开销）与远程模式（`WebSocketProtocol`，连接远程 Gateway；QUIC 传输为未来优化）。操作集与协议定义见 [`network.md`](./network.md) §2-§4。

## 24. 配置系统

配置系统以 **GPUI Settings 面板图形化编辑为主入口**，配套多机配置同步（Last-Write-Wins + 字段级合并）。Lua 作为统一配置语言的完整方案（`ominiforge.lua` + `lua-language-server` 类型定义 + 双向同步）为**高级可选项后置**，与 Editor 嵌入一并评估（见 [`migration-plan.md`](../operation/migration-plan.md) Phase 5、Phase 7）。界面与同步的定义见 [`config-lua.md`](./config-lua.md)。

## 25. 多机连接

多机连接通过 `ConnectionManager` 管理，支持多种传输，自动切换。

支持 Direct / Tunnel / P2P 多种可插拔传输并自动切换（P2P 渐进升级、断开自动降级），配套设备发现（mDNS / Relay / 手动）与权限模型（连接 ≠ 授权，token 认证，per-peer 配置）。机制定义见 [`network.md`](./network.md) §5。
