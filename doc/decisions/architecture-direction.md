<!-- status: current -->
<!-- owner: @OminiForge -->

# ADR：架构方向（零 UI 节点网络 + 自演化运行时）

本文是 ominiforge **架构方向的单一事实源**，记录两轮方向收敛的完整决策线：

1. **零 UI 的个人 agent 节点网络**（2026-08 初）：不自带 UI，UI 外包门面；多机互联；协议化。
2. **自演化运行时**（2026-08 末）：照 cordis 组合模型把 core 做薄，自我迭代落在拓展层。

具体契约见 [`design/runtime-architecture.md`](../design/runtime-architecture.md)（唯一核心架构契约）。
本文只记录「为什么这么定」与调研依据，不复述契约内容（单一事实源，见 AGENTS.md 规则 12）。

---

## 第一部分：零 UI 的个人 agent 节点网络

### 动因

早期投入的 UI（SvelteKit Web 前端 ~2.1 万行 + GPUI crate ~5 千行）挤占了 agent 核心（多机互联、
自我迭代）的精力。作者明确：真实形态是「一个人，多台机器，N+ agent 持续工作」，核心诉求是个人
开发效率；不愿再做 UI——把 UI 外包给现成工具，集中精力打磨核心。

### 决策

- **不自带任何 UI**（Web/GPUI 均移除），只保留 CLI/TUI 与远期手机端（原生壳，非 GPUI）。
- **门面（facade）= 协议之上的可插拔适配器**：任务（Linear）/ 对话（编辑器 ACP / TUI / IM）/
  决断（IM 审批 / 编辑器 / 手机）。协议而非点对点集成，平台死了换门面、核心不动。
- **多机互联用 iroh**（QUIC + 打洞 + 自建 relay；relay 仅依赖出站 TCP 443 ≡ HTTPS 可用性）。
- **委派平面 = MCP（Tasks 扩展）over iroh**，A2A 不进核心（官方明示非 sub-agent 协议、Rust 生态
  不成熟、绑 HTTP）。
- **审批是一等公民的统一注册表**（挂起即落 events.jsonl，跨重连/跨门面可应答）。
- **重放游标写进协议规范**（断线续传，不事后补）。
- **透明性重定义**：可查询 + 可导出，非自带展示界面。

### 修正的错误判断（诚实记录）

- 收回「人配 Tailscale、agent 用 ominiforge 两张网正交」——个人少量机器要复用一张网，故在 iroh
  连接上内建通用 TCP/PTY 字节流通道（自有节点，非通用 VPN 产品）。
- hub-and-spoke 作为终态是错的心智模型——星型/mesh 是 iroh 在不同可达性下的涌现形态，不为星型
  写专门代码。

## 第二部分：自演化运行时

### 动因

自我迭代（agent 改框架不重启不打断任务）是核心目标，但 Rust 产物是二进制、改不了宿主。同时现有
core 职责混乱（gateway/monitor 混在 core）、crate 切分不清、缺统一组合机制。经三轮调研（cordis
范式 / 执行环境 / 现代 agent 架构）定下方向。

### 决策

- **core 按 Liedtke 判据划薄**（组合运行时 + 事件/服务 + 拓展装载 + hook/审批硬地板 + 事件日志），
  连 agent loop 都是插件。照 cordis 组合模型，不照其 JS 动态外壳。
- **自我迭代落在三形态拓展**（数据型/进程型/wasm），动态库否决；改 core → 新进程接管 + drain。
- **执行环境双层 seam**：执行世界（整世界替换：本地/容器/远程）+ 进程沙箱（argv 包裹），sibling
  按信任度组合。
- **任务状态全外置**（= events.jsonl 只读投影，不破坏 append-only）。

> **🟡 开放点**：「自我迭代 / 拓展系统」的具体设计（拓展形态、三形态划分、自演化机制细节）是
> **暂定方案，待进一步讨论后调整**——方向（落拓展层）确定，但机制不是定稿。

### 三份调研重点结论

- **cordis 范式**：fiber 状态机 + effect-disposer + drain 守卫经 Koishi（4000+ 插件）、DeepSeek
  Harness 生产验证；连 cordis 自己改框架文件也走进程重启。论文 native 路径指向 traits/proc-macros/
  Wasmtime embedder。
- **执行环境**：文件系统一致性的解法是**进程位置**（执行进文件侧、权威文件系统唯一、跨边界只走
  语义级 RPC）；性能阶梯（bind mount µs → virtio-fs 40-160µs → 同机 RPC 0.3-5ms → 云端 20-60ms），
  高频文件读写不进跨边界核心接口。ExecEnv = 薄 trait（create/spawn→流式 stdio/release + Capabilities
  协商 + create 时声明）。容器首选 Docker(bollard)+bind mount；boxlite 保留为可选后端不扩展为平台。
- **现代 agent**：自我修改收敛于三形态（数据/进程/wasm），无主流系统改宿主二进制（六维互证）；
  审批 = 挂起式反向请求（五家收敛）；硬门控+软引导分层；append-only 事件日志 + resume/fork 工程
  可行且开销可忽略（OpenHands 实测 0.20ms/事件）；隔离外包给容器是官方推荐路径；harness 是系统
  大头（「极薄 core」薄的是职责面不是工程量）。

## 待验证假设与诚实边界

1. **wasm 编译链嵌入宿主的工程成本**——超预期则自写逻辑退到进程型子进程。
2. **「秒级重启/接管」实测**——回放 + 外设重建耗时，是「新进程接管」路线的可接受性判据。
3. **fuel 管不住异步 host 等待**——wasm 资源限制必须 fuel + epoch/壁钟双闸。

**诚实边界**：cordis 论文为 preprint（无评审）；「无人监督稳定自演化」无业界先例——目标表述为
「人/agent 发起的、有审批与回滚护栏的热更」。

## 范围边界（明确不做的）

- 不做自带 UI（Web/GPUI）；手机端一步到位原生壳，作未来高级特性。
- 不做进程内替换宿主二进制（Rust 无稳定 ABI，cordis 自己也不做）。
- 不做无人监督自演化；不做通用 overlay VPN 产品。
- A2A 不进核心（远期对接外部第三方 agent 再加门面）。
- wasm 不作首发（先数据型+进程型）；容器状态缓存后置；在线自改权重（自我训练）无先例，不列入近期。

## 落地顺序

见 [`design/runtime-architecture.md`](../design/runtime-architecture.md) §10：组合运行时骨架 + 修
文件工具 bug → 核心能力插件化 → 拓展三形态 → 执行环境 + 门面。每步可编译可运行。
