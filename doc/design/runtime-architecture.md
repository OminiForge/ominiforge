<!-- status: current -->
<!-- owner: @OminiForge -->

# Ominiforge 架构（运行时 · 网络 · 门面 · 协议）

本文是 ominiforge 的**唯一核心架构契约**：它定义系统是什么、怎么组织、各部分如何协作，
足以指导开发。子系统级协议/实现契约（事件 schema、工具、hook、权限、LSP、monitor 等）各有
独立文档，本文在对应处索引。架构决策的理由（为什么不是别的样子）见
[`decisions/architecture-direction.md`](../decisions/architecture-direction.md)。

---

## 1. 定位与目标

Ominiforge 是一个用 Rust 实现的**个人 agent 节点系统**：**一个人，多台机器，N+ agent 持续工作。**

每台机器跑一个带身份的 ominiforge 实例（**节点**），节点上运行长时 agent 会话（thread），
做编码、研究、自动化等任务。系统的四条根本属性：

- **零 UI**：系统不自带任何用户界面。core 只做「干活的大脑」，UI 全部外包给现成软件（编辑器、
  项目管理工具、IM），经「门面（facade）」接入；自身只保留 CLI/TUI（运维与随身查看）与远期手机端
  （原生壳）。
- **多机互联**：节点经 iroh（QUIC + NAT 穿透 + 自建 relay）互联，人可远程操作任一节点上的活
  thread，agent 也可在权限允许下委派另一节点的 agent（见 §6）。
- **透明性**：系统透明 = 「一切状态皆可经结构化接口查询、一切过程皆可导出为标准格式」，而非自带
  展示界面。核心只做「可查询 + 可导出」，展示由门面与外部工具（Perfetto 等）分担。

## 2. 核心架构：cordis 组合运行时

### 2.1 core 按 Liedtke 判据划薄

判据（seL4 微内核）：「一个概念只有在移出内核会导致系统必需功能无法实现时，才被容忍留在内核内。」

core 只保留五类，编译成 Rust 二进制长期不动：

1. **组合运行时**：context（父子派生）、fiber（生命周期状态机）、registry（inject 声明式依赖）、
   loader（装配/卸载原语）。
2. **事件/服务机制**：五种分发（emit/parallel/serial/bail/waterfall）+ coeffect 键（类型即 key 的
   服务定位）。
3. **拓展装载原语**：三形态拓展的统一装载/卸载/代际 + 可重入 reload 通道。
4. **hook/审批原语**：hook 点注册、四态 decision（allow/deny/ask/defer）、ask 挂起-外部解挂通道，
   以及**不可覆盖的硬门控 deny 地板**（具体规则内容可下放为数据）。
5. **append-only 事件日志**：events.jsonl 的写入与回放原语（「Model-visible means logged」）。

「薄」指**职责面与决策/策略层**薄，不是 harness 工程量小——harness（权限/沙箱/路由/持久化/恢复）
必然是系统大头。**连 agent loop 都是插件**；新进 core 的任何模块先回答「为什么它不能是插件」。

### 2.2 组合运行时机制（cordis 范式在 Rust 的落地）

借鉴 cordis 的**组合模型**（不借鉴其 JS 动态外壳：Proxy/声明合并/HMR/loader 插件树）：

- **effect / disposer（可逆副作用）**：一切经 context 的注册（监听/服务/定时器/子插件）都返回
  disposer，卸载时逆序（LIFO）施加。Rust 用 `Box<dyn FnOnce()>` 表达，幂等由所有权保证。
- **fiber（生命周期状态机）**：`Pending/Loading/Active/Failed/Unloading/Disposed` + **惯性**
  （transition 落地后才响应新目标）+ **drain 守卫**（provider 撤销后等全部 dependent quiesce 才跑逆）。
- **inject（声明式依赖 + 就绪编排）**：插件声明依赖的服务 key，未就绪则 Pending，就绪反应式激活、
  消失自动卸载。**启动期拓扑排序 + 环检测**（修复 cordis 循环依赖静默死等的暗坑）+ 运行期
  PENDING 看门狗（禁止静默）。
- **coeffect 键（类型即 key 的服务定位）**：每个能力域一个 `ServiceDef`（`TypeId` 即 key），
  注册/取用强制共用同一泛型参数，downcast 失败被构造性排除。
- **事件双平面**：**决策平面**（waterfall 环绕中间件，可 modify/block，类型级分发模式）+
  **观察平面**（append-only 日志走可靠通道，broadcast 只喂可容忍丢失的观察者）。

**组合运行时极薄**：core 里 grep 不到 `fs`/`tool`/`agent`/`session`/`llm` 等领域名词——它只含
组合机制，不含领域逻辑。

### 2.3 crate 结构

```
crates/
  ofg-core/         # 组合运行时：Ctx/Registry/Fiber/EffectCollector/EventBus + 基 trait（无领域词汇）
  ofg-macros/       # derive(Plugin)/derive(Inject)/事件注册辅助宏（可选，小）

  # 域定义 crate（def，域簇）：只有 trait/struct/事件类型，每域独占一个 ServiceDef key
  ofg-def-agent/    # Agent 句柄/注册表（不含 loop）+ SessionStore + Llm seam + agent/*/session/* 事件
  ofg-def-tool/     # ToolRegistry + 决策事件（ToolBefore/HookVerdict）+ 权限词汇
  ofg-def-exec/     # Fs/Proc/Net/ExecEnv trait + FsTarget 不透明句柄 + fs/* 事件
  ofg-def-net/      # NodeLink（iroh 抽象）+ Facade trait + 委派协议词汇

  # 实现/策略/边缘 crate（进程内服务插件，编译期注册；同域多后端并入单 crate + feature）
  ofg-agent-loop/   # 具体 agent loop 驱动器，实现 AgentFactory 注册进 AgentRegistry
  ofg-session-jsonl/# events.jsonl append-only 实现 + 回放重建 + 投影
  ofg-tool-runtime/ # 工具注册表 + 执行流水线；内置与 MCP 工具统一注册视图
  ofg-llm/          # llm 服务 + provider 注册表（各 provider 为模块/feature）
  ofg-exec/         # local/container/remote 三实现为模块 + feature
  ofg-net-iroh/     # iroh 互联实现（QUIC/NAT 穿透/relay），提供 NodeLink
  ofg-mcp-host/     # MCP 子进程宿主：spawn（经 ExecEnv.proc）/握手/能力 provide/守护重启
  ofg-policy/       # 权限门控（ToolBefore waterfall）+ 外部 shell hook 桥接
  ofg-edge/         # monitor + gateway + 各门面（facade）适配器（feature 控制）

  ofg-cli/          # 组合根 + CLI/TUI：注册清单 + 启动期拓扑校验 + 激活 + 配置加载
```

依赖方向由 cargo 编译期强制无环：`ofg-core ← ofg-def-* ← ofg-{impl,policy}-* ← ofg-cli`。
def crate 间默认禁止互相依赖；进程插件不产生新 crate（MCP 子进程由 `ofg-mcp-host` 代为建模为 fiber）。

## 3. 拓展系统：自我迭代的落点

> **🟡 开放点（待深入讨论，暂定为当前工作假设）**：本节与 §9 的「自我迭代/自演化」设计是
> **暂定方案，不是定稿**。方向（自我迭代落拓展层而非改宿主二进制）是确定的，但**拓展的具体形态、
> 三形态划分、自我迭代的机制细节**仍需进一步讨论与验证后调整。实施时请把本节当作「当前最佳假设」，
> 预留重构空间，不要把拓展层的抽象写死成不可变的内部约定。

**核心认知**：Rust 产物是二进制，agent/用户改不了宿主框架。因此自我迭代（AI 改框架不重启不打断
任务）的落点不是改宿主，而是改**拓展**——core 极致薄，一切可变能力做成可热插拔的拓展。

### 3.1 三形态拓展（无主流系统改宿主二进制）

| 形态 | 自我迭代的对象 | 热更机制 | 隔离级别 |
|---|---|---|---|
| **数据/配置型**（SKILL.md 式目录） | skill/prompt/策略/规则/AGENTS.md | watcher + 可重入重扫 + 代际注册；progressive disclosure（description 常驻、正文触发加载） | 安全边界在「谁把什么文本塞进 context」 |
| **进程型**（MCP/ACP 子进程） | 工具/门面/外部集成/需 OS 能力的自写逻辑 | 重启子进程 + 代际（generation）+ `list_changed` 热通知；in-flight 超时+取消+退避重连 | 稳定性隔离（崩溃只带走该工具），**非安全隔离** |
| **wasm 组件**（wasm32-wasip2+WIT，⚠️ 自拓展、待讨论细化） | agent 自写的新逻辑（纯计算/变换/策略）、不可信代码 | 预实例化 + hot-swap 零停机 | 指令级（线性内存外无 syscall、WASI 能力型授权、fuel+epoch 双闸限资源） |

- **动态库否决**：Rust 无稳定 ABI、与宿主同地址空间零隔离、dlclose 有 UB 风险。
- **自我迭代对象分层**：改 skill/prompt→数据型；换工具/门面→进程型；自写新逻辑→wasm；
  **改 core 机制本身→新进程接管**（见 [`decisions/architecture-direction.md`](../decisions/architecture-direction.md) 第二部分）。

### 3.2 拓展的注册 / 依赖 / 生命周期 / 热插拔

- **注册与依赖**：`ctx.plugin()` 返回 fiber；`inject` 声明依赖，未就绪 Pending、就绪反应式激活、
  消失自动卸载（启动期环检测 + 运行期看门狗）。
- **生命周期**：fiber 六态 + 惯性 + drain 守卫。
- **热插拔**：dispose 旧 fiber（LIFO 跑逆）+ 同 config 实例化新 fiber；事务化（失败回滚、旧树持续服务）。
- **版本兼容**：wasm 侧 WIT semver 装载期校验；进程侧 MCP 按日期协商 + `list_changed`；
  cordis 侧 provider-uid epoch 保证「被替换者不会被误认为前任」。
- **实验→固化双通道**：agent 的实验性拓展先挂为**内存态临时插件**（不落盘、卸载等 quiesce）在
  沙箱演练，验证通过才落盘固化——把「AI 写拓展的正确性风险」变成「先演练、再事务化晋升」。

## 4. 状态与事件

### 4.1 状态外置 = 任务状态是 events.jsonl 的只读投影

**任务状态不放任何组件内存里，完全由 append-only 的 events.jsonl 推导出来**（fold/回放得到的
当前视图）。events.jsonl 是**唯一事实源**；内存状态只是投影的缓存，可随时丢弃、随时从日志重建。

**不破坏 append-only**：append-only = 只追加、永不修改/删除已有事件——新事件 append 进去、状态
推进一格，正是 append-only 的本意。**不写**会过时的「快照」进日志；快照（`context_snapshot.json`）
是可选加速缓存、可删可重建，不是 source of truth。正因为日志不可变，它才能充当「跨组件替换也不丢」
的状态载体。

### 4.2 事件模型

- **三原语**：Thread（一次持续会话）/ Turn（一轮 agent 工作）/ Item（turn 内带类型的原子输入输出）。
- **生命周期事件**：`item/started → item/*/delta → item/completed`；`turn/started → turn/completed
  {completed|interrupted|failed}`。事件即日志，回放即重建时间线。事件 schema 详见
  [`event-schema.md`](./event-schema.md)。
- **审批注册表（一等公民）**：权限门控命中 `ask` 时产生 pending-approval，**挂起即落 events.jsonl**
  （跨重连/跨门面可查询、可应答）；任一已连接门面提交决议后核心写入 `Permission::Decided` 并广播
  resolved。审批从任何单一控制通道的存活中解耦。权限模型详见 [`permission.md`](./permission.md)。
- **重放游标（断线续传）**：每条持久化事件带 thread 内全序 `seq`；订阅带 last-seen seq，先重放缺口
  再接 live，不重不漏。live delta 瞬态不重放；大历史游标分页，禁止全量载入内存。

## 5. 执行环境：双层 seam

「seam」= 可整体替换实现、使用者无感知的接口层。执行环境有**两层** seam，**两个都用，各管一件事**。

### 5.1 执行世界 seam（整世界替换）

「一个执行世界」= agent thread 干活所在的「文件 + 进程 + 网络」的同一空间。**文件与进程必须绑定为
同一 provider**（否则 shell 在容器、文件读写在宿主，视图分裂）。

```
        agent loop（不关心自己在哪）
                  │
      ┌───────────▼───────────┐
      │ 执行世界 seam：fs()+proc()+net() │
      └───────────┬───────────┘
        ┌─────────┼─────────┐
        ▼         ▼         ▼
   本地宿主    容器      远程节点(iroh)
 (passthrough) (容器后端)  (多机互联)
```

- **agent loop 一份代码，三种世界随便切**：换 provider 实现，Bash/PTY/LSP/MCP 全部随之迁移，上层零感知。
- **core 内一切文件读写唯一合法路径** = `ctx.get::<ExecEnvDef>().fs()`；用 clippy `disallowed-methods`
  禁用 `tokio::fs`，让「绕过执行环境」成为编译期错误（治愈现存 bug）。
- **MCP 子进程经当前 ExecEnv 的 `proc().spawn()` 派生**——容器世界下 MCP server 跑在容器内，
  「core 看到的文件系统 = 工具子进程看到的文件系统」由同一 spawn 通道保证。

### 5.2 进程沙箱 seam（argv 包裹）

`confine(argv, policy) -> ConfinedArgv`——**不改 spawn 逻辑，只包裹 argv**（runner + profile + 原
argv）。决定单条命令以多大权限跑（read-only / workspace-write / danger-full-access），逐次决定。
**fail-closed**：无可用后端抛错，绝不静默直跑。

### 5.3 两层的关系与组合（按信任度）

两层是 **sibling 不是嵌套**——容器不是进程沙箱的一种，容器是执行世界的一种。

| 任务信任度 | 执行世界（在哪） | 进程沙箱（权限） | 开销 |
|---|---|---|---|
| 本地快速任务 | 本地宿主 | argv 包裹（workspace-write） | 零开销（µs 级） |
| 不太信任的任务 | 容器 | 容器内再 argv 包裹 | 容器启动 ~200-500ms |
| 远程任务 | iroh 节点 | 远端 argv 包裹 | 网络开销 |

执行世界负责「在哪跑」的粗粒度隔离与开销，进程沙箱负责「怎么跑」的细粒度权限。本地任务追求零开销，
危险任务追求强隔离，按信任度自由组合。

### 5.4 环境声明（EnvSpec）与预设档位

环境（执行世界选择 + 挂载 + 网络 + 资源 + 镜像 + 生命周期钩子）统一由 **EnvSpec** 在 `create` 时
**声明**（一次性、结构化、可序列化）。**预设档位**把两层 seam 的决策打包成开箱即用的名字：

| 档位 | 执行世界 | 进程沙箱 | 网络 |
|---|---|---|---|
| `local` | 本地宿主 | workspace-write | open |
| `guarded` | 本地宿主 | workspace-write + 只读系统目录 | allowlist |
| `container` | 容器 | 容器内 workspace-write | agent 期断网 |
| `remote:x` | iroh 节点 x | 远端默认 | — |

日常只选档位；高级用户才展开改细项。

### 5.5 权限边界与依赖/环境准备

**权限边界**：默认拒绝、显式放行；**写权限极窄**（只有 workspace），**读权限按需给**（依赖缓存、
工具链，以只读为主）；**敏感目录硬编码拒绝**（`~/.ssh` 等，任何配置都放不进去——硬门控地板）。
**看依赖源码**：依赖缓存目录显式声明为只读挂载；**装新依赖**：默认装进 **workspace 内**。

**环境准备（不枚举，direnv 统一入口）**：不枚举语言/系统的缓存目录（`~/.cargo`/`~/.npm`…枚举不完）。
容器/thread 启动时**在容器内**执行 `direnv export`，读项目的 `.envrc` 准备环境（`use flake` /
`use uv` / `source .env`）。ominiforge 只面向 direnv 这一个公分母，项目用什么由 `.envrc` 自决。
选择容器内跑 direnv（而非复用宿主环境）是为了通用——环境与容器同一个世界。

**容器后端与缓存**：首个后端 = Docker（bollard 直连，成熟）+ 工作区 bind mount；容器后端是执行
世界 seam 下的可替换实现，未来可换启动更快的专门 agent 容器（Firecracker microVM、E2B 类），
agent loop 不用改。**容器缓存三层**（环境声明不变时「准备环境只慢第一次」）：镜像层缓存（MVP 免费）
→ 依赖缓存 → 容器状态缓存（复用已 setup 容器一段时间，改 setup 即失效，**后置**）。改环境声明
（flake.lock/package.json/Dockerfile）→ 缓存失效重建。

## 6. 节点网络：多机互联

### 6.1 传输底座：iroh

连接平面采用 **iroh**（QUIC + NAT 打洞 + relay 兜底）。`connect(node_addr, ALPN)` 返回标准 QUIC
双向流；一条连接可并发多条流。**断线无感切换**（QUIC multipath：换网不断连）；打洞失败自动走 relay
（出站 TCP 443 WebSocket），对应用透明。**relay 仅依赖出站 TCP 443 ≡ HTTPS 可用性**——中国大陆
网络环境下「最坏是退化（丢直连优化），不是失败」。

**relay 必须自建**（官方公共 relay 限速且在境外）：≥2 个、不同可用区，涉中国节点至少 1 个境内/香港。
relay 无状态、只转发密文，CPU/内存需求极低，**唯一要紧的是带宽**（agent 事件流/终端字节流流量小，
一台便宜 VPS 即可）。务必开 access 控制（节点 NodeId allowlist）+ 每客户端限速。**地址发现去依赖化**：
不依赖默认 `dns.iroh.link`（中国可达性未证实），节点配对时经既有控制通道交换 `NodeAddr`/ticket。

### 6.2 三个平面

| 平面 | 干什么 | 承载 |
|---|---|---|
| **会话平面** | attach 到活 thread，收发消息，看事件流 | 协议语义层（§8）over iroh/stdio/WS |
| **连接平面** | 节点身份、发现、传输协商、无感切换 | iroh（§6.1） |
| **委派平面** | 节点 A 的 agent 委派/调用节点 B 的 agent | MCP（Tasks 扩展）over iroh 流 |

- **会话平面：attach 不迁移**。thread 归属产生它的节点（workspace/环境/进程在那）；换设备 = 从新地方
  attach 到仍在运行的 thread，历史不动。「直接接管」（迁移 thread）是罕见需求，后续用 fork-transfer。
- **连接平面：身份≠授权**。公钥即身份（连接端到端加密），连接成功不代表可操作；远程操作需 token
  认证 + per-peer 配置 + per-thread 权限。同一套权限模型同时约束「人远程操作」与「agent 委派」。
- **委派平面**：节点把可委派能力暴露为 **MCP server**（任务型 tool）；调用方经其 MCP client 调用。
  **派任务**（异步）：服务端按 Tasks 扩展立即返回 taskId（落进该节点 events.jsonl），进度轮询/断线
  凭 taskId 恢复/取消用 `tasks/cancel`；**当 subagent 调**（同步）：阻塞式普通 tool，中途审批时任务
  进 `input_required`，经统一审批注册表收集后由调用方 `tasks/update` 回传。鉴权复用 iroh 节点身份。
- **拓扑涌现**：星型与 mesh 是同一套代码在不同可达性下的两种形态，**不为星型写专门代码**。

### 6.3 通道（一条连接上的多字节流）

在一条已建立的 iroh 连接上复用多条通道（channel），每类是一种流。**通道不需独立建连**——NAT 穿透
只在建连时做一次。这消解了「agent 能连上、单独开 terminal 却连不上」的矛盾。

| 通道 | 内容 | 用途 |
|---|---|---|
| `events` | 结构化事件流 | 会话平面，门面消费 |
| `pty` | 字节流（远程 PTY） | 调试逃生舱终端（TUI 面板） |
| `tcp-forward` | 通用 TCP 转发 | 「命令行操作机器、看系统信息、临时连端口」等运维诉求 |

PTY 通道定位**调试逃生舱**（非远程工作流）：远端节点内置 PTY daemon 持有 PTY（zmx/shpool 模式），
断线重连后重放屏幕再接增量字节流；不做 tmux 式分屏、mosh 式预测回显。通用 TCP 转发**只做自有节点**，
不做通用 overlay VPN 产品。

## 7. 门面层：UI 外包

### 7.1 能力抽象，而非平台集成

**不把「平台」做成插件，把「能力」做成接缝，让平台去适配能力。** 三类能力覆盖全部交互：

| 能力 | 干什么 | 承接平台（可替换） |
|---|---|---|
| **任务（Task）** | 跨 thread 的持久任务管理 | Linear（首选）/ Plane / GitHub Issues |
| **对话（Thread）** | 发消息、看流式过程、attach 活 thread | 编辑器（ACP）/ TUI / IM |
| **决断（Decision）** | 审批的通知与应答（批准/拒绝 × 作用域） | IM（Mattermost）/ 编辑器（ACP）/ TUI / 手机 |

门面只做「语义映射 + 传输适配」，消费核心的统一审批注册表与事件流；门面互不依赖、可独立增删。

### 7.2 各门面

- **编辑器门面（ACP）**：ominiforge 节点 = ACP agent（server）；Zed/JetBrains/Neovim 等现成 ACP
  client 即成为 UI。`events.jsonl` → `session/load` 重放为 `session/update`；权限门控 →
  `session/request_permission`；attach 活 thread 用 `session/resume`（不重放）。标准 ACP 仅 stdio，
  本机放极薄 stdio shim 经 iroh 连远端节点。
- **任务门面（Linear 首选）**：跨 thread 持久任务托管给 Linear issue；自定义 state 表达任务状态机；
  评论/状态经 webhook（HMAC 签名，Cloudflare Tunnel 暴露的 HTTPS 端点）闭环，agent 回写进度。
  真相源仍是 events.jsonl，Linear 只是门面（元数据用约定 label + 描述承载，Linear 无自定义字段）。
- **IM/审批门面（Mattermost Team Edition）**：审批通知 + 应答（决断能力），兼作手机端前夜兜底。
  用「消息内按钮 + 下拉（作用域）」，回调 HMAC 签名，应答写回审批注册表。务必用 Team Edition
  （非默认 Entry 版——后者有消息上限）。
- **TUI 门面**：本地结构化查看与操作（thread 列表、状态、用量、审批）+ 调试逃生舱终端（ratatui +
  portable-pty + iroh 流 + 远端 PTY daemon）。擅长结构化数据，不承担精美时序瀑布（那走导出 + Perfetto）。
- **手机端（未来高级特性）**：一步到位做原生壳（不用 GPUI）；当下由 IM 门面（Mattermost App）兜底
  「看进度 + 一键审批」。协议约束：「手机 = 协议子集」，每个能力标注「手机可用/桌面专属」。

### 7.3 传输绑定

| 绑定 | 用途 |
|---|---|
| stdio JSONL | 同机 CLI/TUI/编辑器（ACP shim） |
| iroh/QUIC 流 | 节点↔节点、跨机客户端、委派 |
| WebSocket/HTTP + SSE | 本机/局域网门面、webhook 集成（现有 gateway 保留为此用途） |

**传输分工**：SaaS 出站 webhook（Linear 等）走 Cloudflare Tunnel 的 HTTPS 端点；自有节点间流量走
iroh。两者互不依赖。

## 8. 协议语义层（对外契约）

- **三原语 + 生命周期**（见 §4.2）：Thread/Turn/Item；事件即日志、回放即重建。
- **版本化**：不打协议版本号，而是 **schema codegen（Rust 单源定义产出 JSON Schema/TS 绑定）+
  initialize 能力协商 + stable/experimental 分层**（实验面需 opt-in）；golden fixture 兼容性测试进 CI。
- **多客户端语义**：按连接订阅（可按 threadID 服务端过滤）；每连接通知 opt-out；末订阅者离开后
  thread 保持加载、空闲超时卸载。
- **背压**：入口有界队列，饱和返回结构化「server overloaded」错误，客户端指数退避。

## 9. 待验证假设与诚实边界

1. **wasm 编译链嵌入宿主的工程成本**——超预期则 agent 自写逻辑退到进程型子进程（粒度变粗）。
2. **「秒级重启/接管」实测**——events.jsonl 回放耗时 + 外设（iroh/MCP/execenv）重建耗时；
   这是「新进程接管」路线的可接受性判据。
3. **fuel 管不住异步 host 等待**——wasm 资源限制必须 fuel + epoch/壁钟双闸，每次调用重置限额。

**诚实边界**：cordis 论文为 preprint（无同行评审）；「无人监督的稳定自演化」无任何业界先例——
omniforge 的自我迭代目标应表述为「**人/agent 发起的、有审批与回滚护栏的热更**」，不承诺无人监督
自演化。

## 10. 落地顺序（增量，每步可编译可运行）

1. **组合运行时骨架**（`ofg-core` + `ofg-def-exec`）；**先修现存 bug**：文件工具改走
   `ctx.get::<ExecEnvDef>()`（治愈「绕过执行环境」，顺带验证组合运行时）。
2. **核心能力插件化**：session → append-only + 回放（接管地基）；tool/hook/permission → 决策平面
   waterfall + policy 插件。
3. **拓展三形态**：数据型（skill 系统）→ 进程型（MCP 纪律：超时/代际/退避/回收）→（wasm 后置，
   先 spike）。
4. **执行环境 + 门面**：双层 seam（argv 包裹先做，容器 provider 后做）；monitor/gateway 拆出、
   门面适配器化。

每步完成判据：workspace 编译通过 + 该步行为集成测试通过 +（第 2 步起）events.jsonl 回放一致性
测试通过。最坏情况停在中点仍是改进。
