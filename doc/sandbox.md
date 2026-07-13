# Sandbox 设计

本文档定义 Ominiforge 的沙箱抽象、实现策略、和快照/fork 语义。

**设计目标**：为所有 session 提供隔离的执行环境（workspace + shell + 工具），支持 snapshot/fork/release 生命周期，跨平台（Linux + macOS Apple Silicon，Windows 后续），可嵌入（链入主进程）或服务式部署，后端可插拔。

---

## 1. 设计原则

1. **统一抽象**：所有沙箱后端隐藏在 `trait Sandbox` 后，上层（agent/session/CLI）只依赖 trait。
2. **静止态快照契约**：快照在回合之间、沙箱无执行中工具时捕获；restore 产出等价的就绪沙箱。内存热恢复作为可选能力，不在核心契约假设。
3. **session 拥有其文件系统视图**：沙箱生命周期绑 **session**（不绑驱动它的 thread），由 `SandboxManager` 统一持有、跨重启存活（§3）。workspace 是用户给定路径、app 零 VCS 假设；代码的 fork/合并走 **git**（用户层），app 只 fork/管理它自己拥有的工作数据。
4. **release 判定归后端**：抽象层的 `release` 只表达「我用完了，可回收」；「是否真能删」（是否还有 fork 子依赖同一快照）由后端决定，不是抽象层的职责。CoW 快照链的孤儿回收（引用计数/级联）是 **boxlite 内部机制**，上层不实现、不跟踪、看不见。
5. **双形态部署（服务式为 future）**：抽象为嵌入式（链入 Rust 进程作为库）和服务式（独立 daemon/REST）两种形态设计；当前只做**嵌入式**，服务式是 future feature（§5.3）。
6. **后端可扩展（先只做一个）**：当前只实现 BoxLite 一个后端并打磨细节；抽象层为可插拔而设计，但第二后端 / 服务式 / 协议后端（E2B）均为 future feature（§5.3）。抽象层的价值在此阶段是**隔离单后端的风险**，不是现在就多后端。

---

## 2. 核心抽象：`trait Sandbox`

```rust
/// 沙箱生命周期抽象。
///
/// 每个沙箱封装一个隔离的执行环境（workspace 文件系统 + shell + 网络策略）。
/// 快照/fork 操作假设沙箱处于【静止态】（无执行中的工具进程，shell 停在提示符）。
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// 创建一个新沙箱，从 rootfs 镜像冷启动。
    async fn create(config: SandboxConfig) -> Result<Self>;

    /// 在沙箱中执行命令，返回 stdout/stderr + exit code。
    async fn exec(&self, cmd: &str) -> Result<ExecOutput>;

    /// 捕获当前沙箱状态为快照。
    ///
    /// **契约**：调用者必须确保沙箱处于静止态（无执行中进程）。
    /// 捕获内容：文件系统状态（最低保证）；内存/进程状态为可选能力（见 `capabilities()`）。
    /// 返回快照 ID，可用于 `restore` 或 `fork`。
    async fn snapshot(&self) -> Result<SnapshotId>;

    /// 从快照恢复/克隆出一个新沙箱。
    ///
    /// **契约**：产出的沙箱具有快照时的文件系统状态，处于就绪（ready）态。
    /// 对于仅文件系统快照的后端，这是冷启动；对于内存快照后端，可能是热恢复。
    /// 使用者只感知性能差异（启动延迟、密度），行为等价。
    async fn restore(id: SnapshotId) -> Result<Self>;

    /// 释放此沙箱及其关联资源。
    ///
    /// **契约**：声明「我用完了」。后端可回收此沙箱**及其独占**的资源；
    /// 若它是从快照 fork 来的、父快照仍被别的沙箱依赖，后端负责不误删父快照。
    /// 「是否还有依赖」如何判定是后端内部细节（boxlite 用引用计数），抽象层不关心。
    async fn release(self) -> Result<()>;

    /// 查询此后端的能力标志。
    fn capabilities(&self) -> SandboxCapabilities;
}

/// 沙箱创建配置。
pub struct SandboxConfig {
    /// Rootfs 镜像（OCI ref 或本地路径）。
    pub rootfs: String,
    /// 资源限制。
    pub resources: ResourceLimits,
    /// 网络策略（隔离 / allow-list / 全开）。
    pub network: NetworkPolicy,
    /// 初始文件挂载（可选，用于注入 workspace 初始内容）。
    pub volumes: Vec<VolumeMount>,
}

/// 后端能力标志（可选特性）。
pub struct SandboxCapabilities {
    /// 是否支持内存快照（live checkpoint/restore），而非仅文件系统快照。
    pub live_snapshot: bool,
    /// 是否支持热 fork（秒级从快照克隆），而非冷启动。
    pub hot_fork: bool,
    /// 快照链是否自动 GC（引用计数级联回收）。
    pub refcounted_gc: bool,
}
```

**设计要点**：

- **最小契约 = 文件系统快照 + 静止态**。所有后端必须兑现这个；内存快照/热 fork 是加分项（通过 `capabilities` 声明）。
- **fork = `restore`**。没有单独的 `fork()` 方法——fork 就是"从快照 ID restore 出一个新沙箱"。session fork 会先 `snapshot` 父沙箱，再用返回的 ID `restore` 出子沙箱（§4.2）。
- **`release` 不计数、不推理**。抽象层调 `release` 表示「用完」，仅此。快照被多少沙箱引用、何时能真删，全在后端内部（boxlite 自带引用计数 + 级联 GC）——上层不维护平行的计数，避免和后端打架。

> **实现形态说明**：上面的单 trait 是概念契约。落地时拆成两个 object-safe trait（`src/sandbox/mod.rs`）——`SandboxBackend`（工厂：`create`/`restore` → `Arc<dyn Sandbox>`）+ `Sandbox`（实例：`exec`/`snapshot`/`release`/`capabilities`，全 `&self`）——因为上层要把沙箱藏在 trait object 后（§3.2），而 `create/restore -> Result<Self>` + `release(self)` 不是 object-safe。语义不变，只是接收者形态变。`capabilities` 实际还带一个 `filesystem_snapshot` 标志（passthrough 为 false，boxlite 为 true），用于 fork 的能力门控。

---

## 3. Session 沙箱：一等公民

沙箱不是「session 顺带持有的一个句柄」，而是 **session 拥有一套由挂载组合成的文件系统视图**——沙箱只是兑现这套视图的机制。本节定义所有权、生命周期、落盘、持久化。

### 3.1 session（静态） vs thread（动态）

一个 **session** 落在磁盘上，是**静态记录**：对话事件 + 元数据 + 它拥有的工作目录。一个 **thread** 是驱动某个 session 往前跑的**动态执行流**（当前实现即 `SessionActor`）——可死可重生。

- **session = 盘上的真相**，扛得住进程重启/中断。
- **thread = 短命驱动器**，从 session 重建；idle 驱逐杀掉 thread 不影响 session。
- 「随时拉回一个之前的任务继续」= 对着盘上的 session 起一个新 thread。

（把 `SessionActor` 类型真改名成 `Thread` 是可选收尾、爆炸半径大，不阻塞；下文用 thread 指这个动态角色。）

### 3.2 所有权模型：`SandboxManager` 按 session 持有

沙箱**不再埋在 `ShellTool` 里、也不绑在 thread 上**，而是 session 的核心属性，由一个网关级组件统一持有：

```
SandboxManager（挂在 RegistryInner，与 actors 平级）
  backend: Arc<dyn SandboxBackend>            // 从 config 选：passthrough 默认 / boxlite 可配
  live:    Map<SessionId, Arc<dyn Sandbox>>   // 活句柄，跨 thread 生死存活
  ├ create(session, cfg)  建会话时造沙箱，描述符写 meta
  ├ get(session)          thread/工具取本 session 的沙箱
  ├ attach(session)       进程重启后从 meta 描述符重建活句柄
  ├ fork(parent, child)   = fork_sandbox(backend, 父句柄)（能力门控，见 §4.2）
  └ release(session)      有方法，但 Step 4 无调用点（当前无「删除 session」路径）；触发时机 = Step 5
```

**关键：沙箱生命周期绑 session，不绑 thread。** idle 驱逐 thread 时沙箱留在 `live`；只有会话**被删除**才 release。这消除了「actor drop = 销毁活 microVM」的语义陷阱（actor 退出 ≠ 会话结束）。而「删除 session」这条路径当前**不存在**——所以 `release` 有方法、无触发点，其时机与 GC 一并留到 Step 5（§8），现在定义它 = 为不存在的事件写投机代码。

`assemble` **不再自己造沙箱**——它接收 `Arc<dyn Sandbox>`（网关从 manager 拿；CLI 单发自己造一个），只负责把它接进 `ShellTool`（及以后的 MCP 工具）。**后端选择上移到 manager**（顺带消化早期推迟的「CLI 后端切换」）。

### 3.3 workspace = 用户给定路径，app 零 VCS 假设

session 构建时传入的 workspace 是**什么路径就是什么路径**：传真实仓库 → 就地改真仓库；传一个 worktree → 在那个 worktree 里工作。**app 把 workspace 当普通目录，不建 worktree、不做任何 VCS 假设。**

- 两个后端都把 workspace 兑现为 **cwd**（相对路径逐字一致）——这是当前唯一已兑现的挂载。passthrough 直接 `current_dir(workspace)`（host cwd 就是 workspace）；boxlite 把 workspace 作**读写 FUSE bind 挂载**到 guest `/workspace` 并设 `working_dir=/workspace`，宿主路径在 guest 内不存在故换成固定可移植路径，`pwd`/相对路径行为与 passthrough 一致。boxlite 的 workspace 挂载是宿主目录**实时直通**，不进 box 的 CoW 盘：fork 出的子沙箱（`clone_box` 复用父 `BoxOptions`）继承该挂载、写穿到同一宿主 workspace——这正是本条契约（见下）。
- 想「fork 出隔离分支去跑」的用户，自己 `git worktree add` 再把那个路径作为新 session 的 workspace 传进来——**VCS 编排是用户层的事，不是 app 的事**。这也保证了鲁棒性：app 对 workspace 唯一的假设是「它是普通目录」，谁在外部动它（编辑器、rebase、另一 session）都不会违反这个假设。
- **代码合并永远走 git**（用户层 3-way / PR）。文件系统隔离层**不做、也做不了语义合并**（块级 CoW 只知道「哪些块不同」，不知道「哪个文件改了」）。不同数据在不同层：代码 → git，非版本化产物 → 目录隔离。

### 3.4 落盘布局

session 落盘不再只是 jsonl + toml，多出它**自己拥有**的工作目录，统一在 `sessions/<id>/` 下：

```
.omini/sessions/<id>/
  session.toml     # meta + 挂载规格 + 沙箱描述符（后端类型 + 持久 id）
  events.jsonl     # 对话事件（append-only）
  work/            # app 拥有的工作数据（私有、非版本化）
    …              # tmp / delivery 等具体内容 = future（§3.7）
```

- **app 拥有 `sessions/<id>/`**——只要它在，对话/交付/沙箱描述符永远能恢复。
- **app 不拥有 workspace**（你的仓库/worktree）——那是你给的外部路径。
- **跨 session、频繁变更的机器状态**（若有）进 sqlite/专门 json，**不进** `session.toml`（后者是写一次的 meta）。注：CoW 快照的引用计数**不属于**这类——它在 boxlite 内部，我们不落盘、不跟踪。

### 3.5 持久化与 workspace 悬垂

**持久化取最强版**：静态 session 完整落盘（events + meta + 挂载规格 + 沙箱描述符），扛进程重启/中断。沙箱描述符落 meta，让 session 的环境重启后能 `attach` 重建。常见场景：review 代码后，唤醒交付那个 session、让它按 review 结果继续修——thread 是新起的，session 是盘上原来那个。

**workspace 悬垂**——用户删了自己的 workspace/worktree 怎么办：恢复不了的是 **workspace（代码）**，不是 **session**。app 拥有的部分（对话/delivery/描述符）照常恢复。处理三步，fail loud（Guideline 12）：

1. **检测**：起 thread 时校验 workspace 存在；不存在 → 明确报「session `<id>` 的 workspace `<path>` 不见了」，不静默不瞎猜。
2. **保住能保的**：对话/delivery 照常可读——你仍能回看这个 session 干了什么、拿它的交付。丢的只是「在原代码上继续跑」的能力。
3. **可重绑定**：把 session 的 workspace 指到新路径（重新 `git worktree add` 出来的），继续。描述符是数据，重绑定 = 改一个字段。

**默认纯引用，不内化 workspace。**「把代码复制/CoW 进 `sessions/<id>/` 归 app 管」作为远期 opt-in、当前不建——它会把 CoW-merge 复杂度请回来，且需求未证实。用户删自己的目录，无法恢复是其自身操作的必然结果。

### 3.6 自进化底料 = events.jsonl

平台已定「事件日志是持久真相，沙箱可弃」（§3.1 静止态契约 + `architecture.md`）。**自进化的参照物是 events.jsonl**（+ app 拥有的 delivery），**不依赖活文件系统**：日志有推理 + 工具调用 + 结果的因果链（「为什么」），文件系统只有冻结终态（没有「为什么」）。所以 workspace 悬垂对自进化无影响。

> **实现约束**：日志须把**工具结果**（read 的内容 / write 的 diff / shell 输出）持久化到足够保真度——这是让日志成为自进化底料的前提，实现阶段须确认；且不论自进化与否都是该保证的正确方向。

### 3.7 挂载策略 = Future feature

以下明确推迟（当前不确定要挂什么，现在设计等于猜）：

- **session 私有 temp / delivery 目录**的具体内容与 guest 路径。
- **跨 session 共享目录 / delivery 交接**机制。
- **挂载统一面**：passthrough 无独立命名空间，做不到「同名绝对路径」（把 host 目录搬到 `/workspace` 需要 root/namespace）。统一须靠 **cwd + 命名环境变量**（名字可移植、值随后端不同），配**命名挂载集**模型。此为落地时的既定方向，但具体挂载清单待需求明确再定。
- **公共依赖挂什么进 boxlite guest**：flake 用户可只读挂 `/nix/store` + `.envrc`；非 flake 用户走 base image 或显式 host bind。配置继承一律显式 opt-in、默认不继承敏感项（对齐 secret-store 威胁模型）。
- `SandboxConfig` 的**资源限制下发**（§6.2）与挂载同批接（无明确需求前推迟）。**网络策略下发已单独落地**（不再与挂载同批）：见 §6.2「下发现状」，从 profile `[network]` > gateway 兜底派生。

**当前已兑现的挂载只有 workspace——两后端一致且已在真机验证**（passthrough=host cwd；boxlite=RW bind→`/workspace`+`working_dir`，`#[ignore]` 集成测试 `workspace_is_cwd_and_passes_through_to_host` 在 KVM 上确认 `pwd==/workspace`、宿主文件可见、guest 写回宿主）。

---

## 4. Snapshot 语义与 Session Fork 同步

### 4.1 静止态快照契约

**静止态 = 沙箱无执行中的工具进程，shell 停在提示符。** 在此时刻：
- Agent 的会话状态（message history）在 `events.jsonl` 里，**不在沙箱内存**。
- 沙箱只是一个文件系统 + 一个等待输入的 shell。

**快照在静止态捕获 → 仅文件系统快照和内存快照，产出的 restore 沙箱行为完全相同**（都是一个 ready 的、具有相同初始文件的沙箱）。差异只在性能（内存快照热恢复更快）。这让抽象层可以围绕"较弱的文件系统快照"定契约，所有后端都能兑现。

**若需要"冻结执行中的长任务、原地续跑"**（如 build 到一半、REPL 内存变量），那是**内存快照的独有能力**，不在核心契约里。若哪天真需要，通过 `capabilities.live_snapshot` 查询、只在支持的后端启用该特性。

### 4.2 与 Session Fork 同步

Session 已有 `create_fork(parent_id, fork_at_seq, …)` 逻辑（`src/session/mod.rs:281`），它 fork 会话状态（events + message snapshot）。沙箱侧的同步由 `SandboxManager`（§3.2）承接——因为它按 session id 持有活句柄，fork 天然够得着父沙箱：

```
registry.fork(parent, at_seq) 沙箱侧：
  1. parent_sb = sandbox_manager.get(parent)        // manager 按 session id 寻址，不需穿透 actor
  2. child_sb  = sandbox_manager.fork(parent, child) // = fork_sandbox(backend, parent_sb)
  3. assemble(child, injected_sandbox = child_sb)    // 子 agent 用 restore 出的沙箱
```

`fork_sandbox(backend, parent)`（`src/sandbox/mod.rs`，已实现）：`parent.capabilities().filesystem_snapshot` 为真 → `parent.snapshot()` → `backend.restore(id)`，产出独立可写、CoW 共享父快照的子沙箱；为假（如 **passthrough**）→ fail-loud `Unsupported`，此时 fork **回退到当前行为**（子会话在继承的 workspace 上就地工作），并记录。

**边界重申（§3.3）**：这里 fork 的是**沙箱拥有的文件系统状态**（工作数据），不是 workspace 代码——代码的 fork/合并由用户经 git worktree 完成，app 不介入。

### 4.3 CoW 与磁盘负担

**问题**：频繁 snapshot 是否会爆盘？

**答案**：取决于后端的 CoW 机制。BoxLite 用 QCOW2 backing-chain CoW：

| CoW 机制 | 频繁 snapshot 成本 |
|---|---|
| **QCOW2 backing-chain**（文件系统无关，不依赖 host FS reflink） | ~1ms / ~200KB 稀疏子盘，读共享父镜像 ✅ |

**核心洞察**：BoxLite 的 CoW 在 qcow2 镜像内部，**与 host 文件系统无关**——普通 ext4 上也是 ~200KB 稀疏子盘，不会退化成全量拷贝。频繁 snapshot **几乎不产生新磁盘占用**（只有改动块才占空间）。

> 对比：某些 microVM 方案（如 microsandbox）的 CoW 依赖 host FS reflink（APFS/btrfs/XFS-reflink），在普通 ext4 Linux 上退化为全量拷贝。BoxLite 无此问题——这是它被选为唯一后端的关键原因之一（§5.1）。

**长期成本**：CoW 快照链会累积（父镜像不能删，子依赖它），需要 GC。**这套 GC 全在后端内部**，抽象层不实现、不跟踪：

- **引用计数**：每个快照记录"被多少沙箱引用"。
- **级联回收**：当快照引用计数降为 0 时，删除它并递归检查其父快照。
- **Flatten**（可选）：合并过深的快照链，防止链过长拖慢读。

BoxLite 的 `base_disk_ref` 表 + `try_gc_base()` 就是这套的参考实现——**它自带,我们白拿**。抽象层的 `release` 只声明「用完」，何时真删、如何保护仍被依赖的父快照，全归 boxlite。上层不维护平行计数。

---

## 5. 后端：BoxLite（唯一实现）

**范围决定**：当前只实现**一个后端**并打磨细节。抽象层（`trait Sandbox`）从第一天起就为可插拔而设计，但第二后端、服务式后端、协议后端都是 **future feature**（见 §5.3），不在当前范围。单后端让我们集中把一条路走通、走透，而不是过早铺开。

### 5.1 选型：BoxLite

`boxlite`（github.com/boxlite-ai/boxlite，Apache-2.0）——libkrun microVM，原生 Rust crate。

**选型逻辑**（作为唯一后端，标准是"能长期托付 + 技术能力精准命中需求"）：

1. **CoW/fork 最完整**：qcow2 backing-chain（**文件系统无关**，不依赖 reflink）+ 引用计数 GC（`base_disk_ref` + `try_gc_base` 级联）+ `flatten()` 压链。fork = ~1ms / ~200KB 稀疏子盘，读共享父镜像。**直接命中 §4 的"频繁 per-session fork 不爆盘"** —— 这是唯一把这套做全的候选。
2. **原生 Rust crate**：`boxlite` v0.9.7 on crates.io，async/Tokio。`BoxliteRuntime::create` 返回 `LiteBox`，公开 `SnapshotHandle` / `SnapshotInfo` / `SnapshotOptions`，依赖 `reflink-copy` + `qcow2-rs`。API 形状贴近 `trait Sandbox`。
3. **跨平台**：Linux + macOS(Apple Silicon) + Win(WSL2)。
4. **`#![forbid(unsafe_code)]` 兼容**：unsafe 局限在 `boxlite` 依赖（libkrun FFI）内，本 crate 不破例。

### 5.2 已知风险（唯一后端下必须正视）

- **bus-factor ≈ 1**：DorianZheng 主导（455 提交 vs 次 52，8.8x），无公开机构背书（疑 RisingWave 系但未证实）。
- **pre-1.0 API churn**：v0.9.7，7 个月历史，892 下载，1.0 无时间表，archive 格式已到 v3。

**缓解 = 抽象层本身**：这正是 `trait Sandbox` 存在的理由。BoxLite 的所有依赖都锁在 `src/sandbox/boxlite.rs` 一个文件里，上层只见 trait。若 BoxLite 停摆或 API 破坏，换后端只改这一个文件，上层零改动。**风险被抽象层隔离，不扩散到平台。** 这也是为什么"只做一个后端"仍要先立抽象——不是为了现在多后端，而是为了把这个单点风险关进盒子。

### 5.3 Future features（不在当前范围）

以下明确推迟，抽象层为它们预留接口但**当前不实现**：

- **第二后端**（如 microsandbox）：验证可插拔 + 生产对比基准。
- **服务式后端**：独立 daemon + REST，用于 server 部署 / 零信任 / 多租户。
- **协议后端**（`ProtocolBackend` + E2B SDK）：让用户用任何语言实现 E2B-compatible 服务接入，跨语言扩展、零改 Rust——与现有 MCP 工具扩展同构。优先采纳 E2B 协议而非自造。
- **内存快照 / live checkpoint**：见 §9 open question 3。

这些是 `capabilities()` 与后端注册机制的自然延伸，加它们不需要改抽象层契约——这正是抽象层"为可插拔而设计、但先只做一个"的价值。

---

## 6. 工具执行与资源限制

### 6.1 Shell Tool 执行路径

```
Agent 调用 shell tool:
  1. 从 session 获取绑定的 sandbox 实例（SandboxManager.get）
  2. sandbox.exec(command).await
  3. 捕获 stdout/stderr + exit code
  4. 写入 ToolEvent (Started / Completed)
  5. 若输出 > 64KB，存 artifact store
```

**核心改变**：从直接 `tokio::process::Command` 变成 `sandbox.exec()`——隔离由后端（BoxLite microVM）负责，上层统一接口。**已落地**（`ShellTool` 持有 `Arc<dyn Sandbox>`，默认注入 passthrough）。

### 6.2 资源限制

| 资源 | 控制方式 | 谁负责 |
|---|---|---|
| 执行时间 | `SandboxConfig.resources.timeout`（默认 120s） | 后端 + host 侧 tokio timeout 兜底 |
| 内存 | `SandboxConfig.resources.memory_mb` | 后端（libkrun 配额 / cgroup） |
| CPU | `SandboxConfig.resources.cpus` | 后端 |
| 输出大小 | 64KB inline 上限（超出存 artifact） | Host 侧（agent 层） |
| 网络 | `SandboxConfig.network`（Isolated / AllowList / Open） | 后端（BoxLite: DNS sinkhole / 代理） |

**网络策略重点**（BoxLite 能力）：
- **Isolated**：无网络（microVM 不配 NIC）。
- **AllowList**：`allow_net` 域名白名单 + DNS sinkhole，阻断未列出主机；可选 MITM 密钥注入（真实密钥不进 VM）。
- **Open**：unrestricted egress（eval/本地 CLI 可选）。

**下发现状**：
- **后端映射已完成**：`box_options()` 把 `SandboxConfig.network`（Isolated→无 NIC / AllowList→白名单 / Open）和 `resources.cpus`/`memory_mb` 逐字段翻译成 boxlite `BoxOptions`；passthrough 按契约忽略（宿主无隔离）。
- **network 已接分层下发**（本步）：策略从 **profile `[network]` > gateway `default_network` 兜底 > 硬编码 `Open`** 派生，写进 `SandboxConfig.network`。profile 层归属见 `doc/profile.md` §7（network = agent 能力，同 tool set）。effective 兜底 = `Open`——一个新 boxlite session 默认能联网，锁死交给显式配置（否则 `NetworkPolicy::default()=Isolated` 会让每个未配置 session 断网）。策略名非法 **fail loud**（Karpathy §12），不静默回退。**持久化**：策略由 profile 派生、`profile_id` 已落 `session.toml`，重启 `attach` 沿同一链重新派生 → 天然还原,无需在 `SessionMeta` 另存 network 字段（§3.5）。
- **resources（cpu/mem）下发面**：后端映射就绪，但 profile/gateway 尚未暴露配置入口——无明确需求前不做（§3.7「现在设计等于猜」）。
- **workspace 层**：network 未来可下沉到 workspace 级配置（后续权限门控同一落点）；解析链已按 `workspace(future) > profile > gateway` 预留，当前只实现 `profile > gateway`。

---

## 7. MCP Server 监控

MCP server 作为子进程的额外监控指标（启动耗时、崩溃/重启次数、调用延迟、错误率、可用状态）由 monitor module 从 event stream 派生，不在 tool invoke 路径计算。指标清单见 [`monitor.md`](./monitor.md) §7。

**注**：MCP tool 的执行**也走 sandbox**（`sandbox.exec` 最终调 MCP server 的 tool），隔离策略统一。

---

## 8. 实现路线图

单后端范围。分步把 BoxLite 一条路走透：

| Step | 目标 | 交付物 | 状态 |
|---|---|---|---|
| **Step 1** | `trait Sandbox` 抽象 | `src/sandbox/mod.rs`：trait + 类型；`PassthroughSandbox`（零隔离）验证契约 | ✅ 完成 |
| **Step 2** | BoxLite 后端 | `src/sandbox/boxlite.rs`：create/exec/snapshot/restore/release 打通（feature `sandbox-boxlite`） | ✅ 完成（映射单测绿；运行期需真机） |
| **Step 3** | 接入 shell tool | shell tool 从 `tokio::Command` 改走 `sandbox.exec`，默认注入 passthrough | ✅ 核心完成 |
| **Step 4** | **Session 沙箱一等公民** | `SandboxManager`（按 session 持有、跨 thread 存活）+ `assemble` 建/接收沙箱并注入 `shell` + 描述符落 meta（`bind_sandbox`）+ `fork` 先 `fork_from` 父沙箱再注入子 agent（能力门控，passthrough→fallback）<br>**不接 release/GC**（当前无「删除 session」路径，无触发点）<br>验证：passthrough 全绿单测（367 测试绿）；**boxlite 真机 fork 已在 KVM 上验证通过**（3 个 `#[ignore]` 集成测试全绿，含 `manager_fork_from_yields_isolated_child`：子环境继承父 FS、写时分离、父不变） | ✅ 完成（真机验证通过） |
| **Step 5** | 打磨细节 | ✅ **网关侧 boxlite 后端选择**（`gateway.toml` `sandbox_backend = passthrough\|boxlite\|auto`，`SandboxManager::from_choice`，boxlite fail-loud / auto WARN 回退）+ ✅ **生产 flake**（`packages.default`：boxlite release，用 **nixpkgs 的 `libkrun`/`libkrunfw`/`bubblewrap`** 供库，`BOXLITE_DEPS_STUB=1` 让 boxlite 不下载自带 blob，运行期库/bwrap wrap 进 PATH——**零硬编 URL/哈希，依赖归 nixpkgs**）+ ✅ **boxlite workspace 挂载**（`box_options` 把 workspace 作 RW bind→`/workspace`+`working_dir`，fork 经 `clone_box` 继承；真机 `#[ignore]` 测试确认 `pwd==/workspace`、宿主↔guest 双向直通；§9 Q6 已定）+ ✅ **session archive + `release` 触发**（`POST /sessions/{id}/archive`：拒绝运行中(409)→停 actor→`SandboxManager::release`→`.archived` sidecar；`list()` 过滤 archived、文件保留供分析；`unarchive` 反向；§9 Q5 已定，`doc/session-storage.md` §9）<br>剩余：**boxlite-on-NixOS jailer**（证书布局等宿主适配，去掉 `OMINI_BOXLITE_INSECURE` 开发 hack）+ 挂载策略（§3.7 私有 tmp/delivery/共享）+ 资源/网络下发（§6.2）+ **hard delete 路径**（物理删目录，危险、需确认机制）+ snapshot 粒度（§9 Q4）+ edge case | ⏳ 进行中 |

**Future**（§5.3，不在当前范围）：第二后端、服务式后端、协议后端(E2B)、内存快照。

**当前状态**：**Step 4 完成 + Step 5 推进（跨平台后端选择 + 生产 flake + boxlite workspace 挂载）。** Step 4：`SandboxManager` 按 session 持有沙箱、跨 thread 存活；`assemble` 接 `sandbox_backend` + 可选 `injected_sandbox`；`SessionMeta.sandbox` 落盘；`registry.fork` 先 `fork_from(parent)` 再注入子 agent（boxlite CoW，passthrough fallback）。Step 5：`gateway.toml` 的 `sandbox_backend`（`passthrough` 默认 / `boxlite` fail-loud / `auto` WARN 回退）经 `SandboxManager::from_choice` 选后端，`SessionRegistry::new` 据此构造（boxlite 起不来则明确报错）；生产 flake `packages.default` 编译 boxlite release 并把 `bubblewrap` wrap 进运行期 PATH（jailer 依赖，跨发行版通用，NixOS 由 flake 提供）；**boxlite workspace 挂载**（`box_options` RW bind→`/workspace`+`working_dir`，fork 经 `clone_box` 继承，§9 Q6 已定）。默认测试绿、双 flavor clippy 干净；boxlite 5 个 `#[ignore]` 集成测试在本机 KVM 全绿（新增 `workspace_is_cwd_and_passes_through_to_host` 验证 `pwd==/workspace` + 宿主↔guest 双向直通；`choice_boxlite_builds_working_manager` 验证 config→boxlite 端到端）。**Step 5 剩余**：boxlite-on-NixOS 证书适配（去 `OMINI_BOXLITE_INSECURE`）、挂载策略（§3.7）、资源/网络下发（§6.2）、release/GC（§9 Q5）。

**生产 flake（`packages.default`，nix build 已通过）**：
- boxlite 的 crates.io build.rs 进「stub」模式（只出 FFI 声明、`links="krun"` 动态链接、期望宿主提供库）。我们用 **nixpkgs 维护的** `libkrun`（1.17.4）/`libkrunfw`（5.3.0，恰好匹配 boxlite 钉的 v5.3.0，ABI 对齐）/`bubblewrap`，设 `BOXLITE_DEPS_STUB=1` 让 boxlite **不下载**自带 blob，`RUSTFLAGS` 链接期指库、`wrapProgram` 运行期把库放 `LD_LIBRARY_PATH`、bwrap 放 `PATH`。
- **零硬编**：我们的树里没有任何 boxlite 下载物的 URL/哈希/版本——nixpkgs 拥有它们，boxlite 升级只是 Cargo.toml 版本号。这是「nix 原生」解，非镜像 boxlite 的下载清单。
- 前提：`src/sandbox/` 需被 git 跟踪（flake 从 git 树读源码；本会话新文件曾漏 `git add`，`git add -N` 即可让 flake 见到，不必 commit 内容）。

**真机验证记录（本机 = NixOS x86_64 + AMD-V/KVM）**：
- 跑法（开发/测试）：`LD_LIBRARY_PATH=<nixpkgs libcap>/lib OMINI_BOXLITE_INSECURE=1 cargo test --features sandbox-boxlite -- --ignored`。
- **boxlite 在 NixOS 上 jailer-on 起不来，是两层 blocker（本会话真机逐层实测坐实，此前文档对第二层的描述有误，已更正）**：
  1. **libcap（裸 `cargo test`）**：裸环境 PATH 无系统 bwrap → boxlite 回退**自带** bwrap（`bwrap.rs:53` 优先用 PATH 里的系统 bwrap，失败才用自带）→ 自带 bwrap 缺 `libcap.so.2`（NixOS 无 FHS lib 路径）→ preflight `--unshare-user` fail。**生产 flake 已解决**：`wrapProgram` 把 nixpkgs `bubblewrap` 塞进 PATH，boxlite 用系统 bwrap，libcap 问题消失。实测 nixpkgs bwrap 独立 `--unshare-user` OK。
  2. **CA 双绑（flake 产物 jailer-on 仍失败）**：`bwrap: Can't create file at /etc/ssl/certs/ca-certificates.crt`。根因 = boxlite `jailer/mod.rs` 的 `system_ca_paths()` 把宿主**存在的全部** CA 路径塞进只读绑定，而 NixOS 上 `/etc/ssl/certs`（目录）**和** `/etc/ssl/certs/ca-certificates.crt`（该目录内、symlink→`/etc/static`→`/nix/store` 的文件）**都 `exists()`**，于是 bwrap 先把目录 ro-bind 成只读，再想在其内为文件建挂载点 → 只读父目录无法建文件 → 失败。（`BOXLITE_DEBUG_PRINT_SEATBELT=1` dump 实测确认这四条：`/etc/ssl/certs`、`/etc/pki/tls/certs` 两个目录 + 各自内部的 `.crt` 文件同时被绑。）标准 FHS 发行版 `.crt` 就在目录里、不触发；NixOS 的 symlink-farm `/etc` 触发。
- **这是 boxlite 0.9.7 的上游缺陷**（绑目录后又绑其内的悬垂 symlink），**不在我们的 Rust 代码里**：`system_ca_paths` 硬编码于 boxlite，`SecurityOptions` 无 cert 字段可覆盖，我们无法在集成层修掉它而不整体关 jailer。真根因（真机 bwrap 复现）：NixOS 上 `/etc/ssl/certs/ca-certificates.crt` 是 symlink→`/etc/static`→`/nix/store`，boxlite 先把父目录 `/etc/ssl/certs` 只读绑入，该 symlink 便以悬垂态露出（目标未绑入沙箱），bwrap 为它建挂载点时 `open()` 跟随悬垂链到不存在目标 → `Can't create file`。标准 FHS 那里是真实文件、不悬垂，故不炸。
- **决策（2+3）——因为 NixOS 就是我们的部署目标，必须开箱即用**：
  1. **代码侧自动降级**：`advanced_options()` 检测到 `/etc/NIXOS`（或 `OMINI_BOXLITE_INSECURE=1`）即自动关**宿主** jailer，并在**首次**大声 WARN（`OnceLock` 去重）：宿主侧加固（seccomp/chroot/降权）关闭，但 **microVM/KVM 主隔离仍在**——跑不可信 guest 代码的边界不受影响，丢的只是 libkrun 万一被攻破时的第二道防线。这样 NixOS 上不设任何 env 就能起 box（真机验证：不带 INSECURE，`choice_boxlite_builds_working_manager` 绿 + WARN 打出）。env override 保留给其他受影响宿主。
  2. **上游修复**：issue 草稿在 `doc/boxlite-nixos-jailer-issue.md`（建议 `system_ca_paths` 消费者去掉「已被父目录覆盖」的路径）。合并升级后即可去掉本地自动降级、恢复完整 jailer。
- **非 NixOS 标准 FHS 部署不受影响**，jailer 默认完整开启。
- **修了一个 Step 2 遗留的测试 bug**：旧 `#[ignore]` 测试写 `/tmp/marker` 验证快照——但 `/tmp` 是 tmpfs（RAM），**文件系统快照不捕获**，restore 后子环境看不到。改写持久 rootfs 路径 `/marker` 后三测全绿。这正是「运行期必须真机验证」的价值：纯映射单测发现不了。

**Step 4 期间的发现**：`get_or_spawn` 恢复 session 时用的是网关**默认** workspace（`self.assemble()`），不是 `meta.workspace`——这是既有限制，与 Step 4 正交；因此 §3.5 的「悬垂检测/重绑定」精细版entangled于此，未在 Step 4 强做。workspace 缺失本身已由 `resolve_workspace`（canonicalize）fail-loud；「保住对话」天然成立（`read_events`/`read_meta` 不碰 workspace）。

---

## 9. Open Questions

1. **Intel Mac 支持**：libkrun/BoxLite/microsandbox 在 macOS 上都只支持 Apple Silicon(HVF/ARM64)。Intel Mac 用户无 microVM 隔离，需退回 OS 级（srt 模式）或放弃隔离。当前决策：**不支持 Intel Mac microVM**（老机器，可接受）。

2. **Windows 原生**：libkrun 的 WHPX 后端是实验性；BoxLite 只支持 WSL2。当前决策：**Windows = WSL2**，原生 Windows microVM 后续视 libkrun 2.0 成熟度。

3. **内存快照需求**：若哪天需要"冻结执行中长任务、原地续跑"（live checkpoint），只有支持内存快照的后端能做。当前决策：**不是硬需求**，agent 记忆在事件日志、不在沙箱内存，文件系统快照够用。若需要，通过 `capabilities.live_snapshot` 查询、仅在支持后端启用。

4. **Snapshot 粒度**：每次 tool 调用都 snapshot？只对写操作？手动 checkpoint？
   **待定**：先做手动 `sandbox.snapshot()`（CLI 命令或 session compaction 时），自动 snapshot 逻辑等 Step 5 实测开销后定。

5. **release 触发时机**：`release` 何时被调用？前提是先有「退役 session」路径。**已定（已实现）**：session 的 **archive**（`POST /sessions/{id}/archive`，`doc/session-storage.md` §9）就是 release 触发点——归档时依次「拒绝运行中(409) → 停 actor → `SandboxManager::release(id)` → 写 `.archived` 标记」。archive 是**单向终态**（无 unarchive）：沙箱一旦 release 便无法重建（workspace 文件在用户外部目录本就完好，但 CoW 盘里的沙箱内部状态没了），archived session 的运行入口一律 410 Gone、只读入口照常。boxlite 的 `try_gc_base` 在 release 时自查：若父快照仍被 fork 子依赖则不真删，**退役 session 天然安全，我们无需自己算**。抽象层只管「用完就 release」，判定与回收归后端。hard delete（物理删目录）走同样的 release 前序，留作独立切。

6. **boxlite workspace 挂载模式**：~~启动快照 vs virtiofs 实时直通~~。**已定（真机实测）**：workspace 走 **FUSE bind 实时直通**（RW），外部编辑对 guest 立即可见、guest 写立即回宿主。理由：workspace = 用户外部路径，app 不 CoW 它（§3.3）；fork 隔离只覆盖 box 的 CoW 盘（沙箱自有非版本化产物），代码合并走 git。子沙箱经 `clone_box` 复用父 `BoxOptions` 自然继承该挂载，fork 侧零改动。

---

## 10. 废弃内容（历史记录）

WASM 沙箱（wasmtime `StoreLimits` / preopens、路径变量系统、guest 文件系统映射、WASI capability 权限）已废弃，替换为 microVM/容器沙箱 + `trait Sandbox` 抽象。废弃理由见 [`architecture.md`](./architecture.md) §2.3：WASM 隔离强但工具生态受限（需编译到 wasm32-wasi），与"支持任意 shell 命令 / MCP 工具"目标冲突。

旧 Phase 1–3 设计（无沙箱 → 容器 namespace → CRIU checkpoint）已被新架构（trait + 可插拔后端 + 静止态快照）替代。本文档为新架构的权威定义。
