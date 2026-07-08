# Sandbox 设计

本文档定义 Ominiforge 的沙箱抽象、实现策略、和快照/fork 语义。

**设计目标**：为所有 session 提供隔离的执行环境（workspace + shell + 工具），支持 snapshot/fork/release 生命周期，跨平台（Linux + macOS Apple Silicon，Windows 后续），可嵌入（链入主进程）或服务式部署，后端可插拔。

---

## 1. 设计原则

1. **统一抽象**：所有沙箱后端隐藏在 `trait Sandbox` 后，上层（agent/session/CLI）只依赖 trait。
2. **静止态快照契约**：快照在回合之间、沙箱无执行中工具时捕获；restore 产出等价的就绪沙箱。内存热恢复作为可选能力，不在核心契约假设。
3. **fork 与 session fork 同步**：session 的 `create_fork` 必须能 fork 沙箱环境（文件系统状态），使子会话获得独立的、可写的环境副本。
4. **引用计数 + GC**：快照支持 CoW（copy-on-write）共享，通过引用计数回收孤儿链，防止磁盘累积。
5. **双形态部署（服务式为 future）**：抽象为嵌入式（链入 Rust 进程作为库）和服务式（独立 daemon/REST）两种形态设计；当前只做**嵌入式**，服务式是 future feature（§4.3）。
6. **后端可扩展（先只做一个）**：当前只实现 BoxLite 一个后端并打磨细节；抽象层为可插拔而设计，但第二后端 / 服务式 / 协议后端（E2B）均为 future feature（§4.3）。抽象层的价值在此阶段是**隔离单后端的风险**，不是现在就多后端。

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
    /// **契约**：引用计数递减；若此沙箱是从快照 fork 来的，其父快照的引用计数 -1；
    /// 当快照引用计数降为 0 时，后端 GC 可回收该快照及其占用的磁盘空间。
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
- **fork = `restore`**。没有单独的 `fork()` 方法——fork 就是"从快照 ID restore 出一个新沙箱"。session 的 `create_fork` 会先 `snapshot` 父沙箱，再用返回的 ID `restore` 出子沙箱。
- **引用计数在 `release` 里隐式处理**。后端跟踪每个快照被多少沙箱引用；最后一个引用释放时，快照可 GC。

---

## 3. Snapshot 语义与 Session Fork 同步

### 3.1 静止态快照契约

**静止态 = 沙箱无执行中的工具进程，shell 停在提示符。** 在此时刻：
- Agent 的会话状态（message history）在 `events.jsonl` 里，**不在沙箱内存**。
- 沙箱只是一个文件系统 + 一个等待输入的 shell。

**快照在静止态捕获 → 仅文件系统快照和内存快照，产出的 restore 沙箱行为完全相同**（都是一个 ready 的、具有相同初始文件的沙箱）。差异只在性能（内存快照热恢复更快）。这让抽象层可以围绕"较弱的文件系统快照"定契约，所有后端都能兑现。

**若需要"冻结执行中的长任务、原地续跑"**（如 build 到一半、REPL 内存变量），那是**内存快照的独有能力**，不在核心契约里。若哪天真需要，通过 `capabilities.live_snapshot` 查询、只在支持的后端启用该特性。

### 3.2 与 Session Fork 同步

Session 已有 `create_fork(parent_id, fork_at_seq, …)` 逻辑（`src/session/mod.rs:281`），它 fork 会话状态（events + message snapshot），但**没有 fork workspace 环境**——两个会话指向同一物理目录。

**补齐点**：session fork 时，**必须同步 fork 沙箱**：

```
session.create_fork(parent_id, at_seq) 执行流程：
  1. 找到父会话的 sandbox 实例（或重建到 at_seq 时的状态）
  2. snapshot_id = parent_sandbox.snapshot().await?
  3. child_sandbox = Sandbox::restore(snapshot_id).await?
  4. 创建子会话，绑定 child_sandbox
  5. 返回 child SessionWriter
```

这样子会话获得**独立的、可写的环境副本**（CoW 共享父快照，写时分离），满足"根据对应位置的内容直接 fork 出新环境"。

### 3.3 CoW 与磁盘负担

**问题**：频繁 snapshot 是否会爆盘？

**答案**：取决于后端的 CoW 机制。BoxLite 用 QCOW2 backing-chain CoW：

| CoW 机制 | 频繁 snapshot 成本 |
|---|---|
| **QCOW2 backing-chain**（文件系统无关，不依赖 host FS reflink） | ~1ms / ~200KB 稀疏子盘，读共享父镜像 ✅ |

**核心洞察**：BoxLite 的 CoW 在 qcow2 镜像内部，**与 host 文件系统无关**——普通 ext4 上也是 ~200KB 稀疏子盘，不会退化成全量拷贝。频繁 snapshot **几乎不产生新磁盘占用**（只有改动块才占空间）。

> 对比：某些 microVM 方案（如 microsandbox）的 CoW 依赖 host FS reflink（APFS/btrfs/XFS-reflink），在普通 ext4 Linux 上退化为全量拷贝。BoxLite 无此问题——这是它被选为唯一后端的关键原因之一（§4.1）。

**长期成本**：CoW 快照链会累积（父镜像不能删，子依赖它）。**必须有 GC 策略**：

- **引用计数**：每个快照记录"被多少沙箱引用"。
- **级联回收**：当快照引用计数降为 0 时，删除它并递归检查其父快照。
- **Flatten**（可选）：合并过深的快照链，防止链过长拖慢读。

BoxLite 的 `base_disk_ref` 表 + `try_gc_base()` 就是这套的参考实现。抽象层的 `release` 契约保证引用计数语义；具体 GC 逻辑在后端。

---

## 4. 后端：BoxLite（唯一实现）

**范围决定**：当前只实现**一个后端**并打磨细节。抽象层（`trait Sandbox`）从第一天起就为可插拔而设计，但第二后端、服务式后端、协议后端都是 **future feature**（见 §4.3），不在当前范围。单后端让我们集中把一条路走通、走透，而不是过早铺开。

### 4.1 选型：BoxLite

`boxlite`（github.com/boxlite-ai/boxlite，Apache-2.0）——libkrun microVM，原生 Rust crate。

**选型逻辑**（作为唯一后端，标准是"能长期托付 + 技术能力精准命中需求"）：

1. **CoW/fork 最完整**：qcow2 backing-chain（**文件系统无关**，不依赖 reflink）+ 引用计数 GC（`base_disk_ref` + `try_gc_base` 级联）+ `flatten()` 压链。fork = ~1ms / ~200KB 稀疏子盘，读共享父镜像。**直接命中 §3 的"频繁 per-session fork 不爆盘"** —— 这是唯一把这套做全的候选。
2. **原生 Rust crate**：`boxlite` v0.9.7 on crates.io，async/Tokio。`BoxliteRuntime::create` 返回 `LiteBox`，公开 `SnapshotHandle` / `SnapshotInfo` / `SnapshotOptions`，依赖 `reflink-copy` + `qcow2-rs`。API 形状贴近 `trait Sandbox`。
3. **跨平台**：Linux + macOS(Apple Silicon) + Win(WSL2)。
4. **`#![forbid(unsafe_code)]` 兼容**：unsafe 局限在 `boxlite` 依赖（libkrun FFI）内，本 crate 不破例。

### 4.2 已知风险（唯一后端下必须正视）

- **bus-factor ≈ 1**：DorianZheng 主导（455 提交 vs 次 52，8.8x），无公开机构背书（疑 RisingWave 系但未证实）。
- **pre-1.0 API churn**：v0.9.7，7 个月历史，892 下载，1.0 无时间表，archive 格式已到 v3。

**缓解 = 抽象层本身**：这正是 `trait Sandbox` 存在的理由。BoxLite 的所有依赖都锁在 `src/sandbox/boxlite.rs` 一个文件里，上层只见 trait。若 BoxLite 停摆或 API 破坏，换后端只改这一个文件，上层零改动。**风险被抽象层隔离，不扩散到平台。** 这也是为什么"只做一个后端"仍要先立抽象——不是为了现在多后端，而是为了把这个单点风险关进盒子。

### 4.3 Future features（不在当前范围）

以下明确推迟，抽象层为它们预留接口但**当前不实现**：

- **第二后端**（如 microsandbox）：验证可插拔 + 生产对比基准。
- **服务式后端**：独立 daemon + REST，用于 server 部署 / 零信任 / 多租户。
- **协议后端**（`ProtocolBackend` + E2B SDK）：让用户用任何语言实现 E2B-compatible 服务接入，跨语言扩展、零改 Rust——与现有 MCP 工具扩展同构。优先采纳 E2B 协议而非自造。
- **内存快照 / live checkpoint**：见 §8 open question 3。

这些是 `capabilities()` 与后端注册机制的自然延伸，加它们不需要改抽象层契约——这正是抽象层"为可插拔而设计、但先只做一个"的价值。

---

## 5. 工具执行与资源限制

### 5.1 Shell Tool 执行路径

```
Agent 调用 shell tool:
  1. 从 session 获取绑定的 sandbox 实例
  2. sandbox.exec(command).await
  3. 捕获 stdout/stderr + exit code
  4. 写入 ToolEvent (Started / Completed)
  5. 若输出 > 64KB，存 artifact store
```

**核心改变**：从直接 `tokio::process::Command` 变成 `sandbox.exec()`——隔离由后端（BoxLite microVM）负责，上层统一接口。

### 5.2 资源限制

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

---

## 6. MCP Server 监控

MCP server 作为子进程的额外监控指标（启动耗时、崩溃/重启次数、调用延迟、错误率、可用状态）由 monitor module 从 event stream 派生，不在 tool invoke 路径计算。指标清单见 [`monitor.md`](./monitor.md) §7。

**注**：MCP tool 的执行**也走 sandbox**（`sandbox.exec` 最终调 MCP server 的 tool），隔离策略统一。

---

## 7. 实现路线图

单后端范围。分步把 BoxLite 一条路走透：

| Step | 目标 | 交付物 |
|---|---|---|
| **Step 1** | `trait Sandbox` 抽象 | `src/sandbox/mod.rs`：trait + `SandboxConfig` / `SnapshotId` / `SandboxCapabilities` 等类型<br>`PassthroughSandbox`（直通现状 `tokio::Command`，零隔离）验证抽象契约、且不阻塞 |
| **Step 2** | BoxLite 后端 | `src/sandbox/boxlite.rs`：`impl Sandbox for BoxliteSandbox`<br>create/exec/snapshot/restore/release 打通<br>手动测试：Linux + Apple Silicon 各跑通 |
| **Step 3** | 接入 shell tool | shell tool 从 `tokio::Command` 改走 `sandbox.exec`<br>CLI `run` 绑定沙箱实例<br>资源限制/网络策略接通（§5.2） |
| **Step 4** | Session fork 同步 | `session::create_fork` 调 `sandbox.snapshot` + `restore`<br>golden test：fork 会话 → 子环境独立可写、CoW 共享父镜像<br>release 触发引用计数 GC 验证 |
| **Step 5** | 打磨细节 | snapshot 粒度策略（§8 Q4）实测定夺<br>GC 触发时机调优<br>错误路径/超时/清理 edge case |

**Future**（§4.3，不在当前范围）：第二后端、服务式后端、协议后端(E2B)、内存快照。

**当前状态**：Step 0（无沙箱，直接 `tokio::Command`）。下一步：Step 1。

---

## 8. Open Questions

1. **Intel Mac 支持**：libkrun/BoxLite/microsandbox 在 macOS 上都只支持 Apple Silicon(HVF/ARM64)。Intel Mac 用户无 microVM 隔离，需退回 OS 级（srt 模式）或放弃隔离。当前决策：**不支持 Intel Mac microVM**（老机器，可接受）。

2. **Windows 原生**：libkrun 的 WHPX 后端是实验性；BoxLite 只支持 WSL2。当前决策：**Windows = WSL2**，原生 Windows microVM 后续视 libkrun 2.0 成熟度。

3. **内存快照需求**：若哪天需要"冻结执行中长任务、原地续跑"（live checkpoint），只有 CubeSandbox(Linux+XFS) 能做。当前决策：**不是硬需求**，agent 记忆在事件日志、不在沙箱内存，文件系统快照够用。若需要，通过 `capabilities.live_snapshot` 查询、仅在支持后端启用。

4. **Snapshot 粒度**：每次 tool 调用都 snapshot？只对写操作？手动 checkpoint？  
   **待定**：Step 2 先做手动 `sandbox.snapshot()`（CLI 命令或 session compaction 时），自动 snapshot 逻辑等 Step 5 实测开销后定。

5. **GC 触发时机**：引用计数降为 0 时立即删 vs 定期批量 GC vs 磁盘压力触发？  
   **建议**：后端内部决策，抽象层只保证 `release` 时引用计数 -1。BoxLite 的 `try_gc_base` 在每次 release 时检查——足够简单。

---

## 9. 废弃内容（历史记录）

WASM 沙箱（wasmtime `StoreLimits` / preopens、路径变量系统、guest 文件系统映射、WASI capability 权限）已废弃，替换为 microVM/容器沙箱 + `trait Sandbox` 抽象。废弃理由见 [`architecture.md`](./architecture.md) §2.3：WASM 隔离强但工具生态受限（需编译到 wasm32-wasi），与"支持任意 shell 命令 / MCP 工具"目标冲突。

旧 Phase 1–3 设计（无沙箱 → 容器 namespace → CRIU checkpoint）已被新架构（trait + 可插拔后端 + 静止态快照）替代。本文档为新架构的权威定义。
