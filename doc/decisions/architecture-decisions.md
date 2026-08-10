<!-- status: current -->
<!-- owner: @OminiForge -->

# Ominiforge 架构决策记录

本文档记录 2026-08-06 架构讨论的核心决策，作为后续设计和实施的基础。

---

## 背景

### 当前状态
- Web 前端（SvelteKit）+ Gateway（axum HTTP/SSE）+ Rust Core
- 用户主要通过 `cargo run -- serve` + `pnpm run dev` 开发和使用
- 已有 LSP、Formatter、Tree-sitter 等基础设施

### 核心需求
1. **极致编辑体验**：完整的 vim 功能，全局 vim 键绑定
2. **分布式架构**：多台机器（有/无图形界面），P2P 连接，自动降级
3. **长期稳定运行**：单一用户，多设备，间歇性连接
4. **模块化设计**：超低耦合，组合优先，不重复维护多套

### 关键约束
- 用户是 vim 核心玩家，但产品不应强制所有用户用 vim
- 用户有 GUI 开发经验，愿意学习 GPUI
- 接受 GPL 许可证（可以复用 Zed 的 GPL crates）
- 已有冻结版本（dist/freeze），可以在仓库中破坏性改动

---

## 核心架构决策

### 1. UI 框架：GPUI（唯一 UI）

**决策**：使用 GPUI 作为唯一的 UI 框架，替代 SvelteKit Web 前端。

**理由**：
- GPUI 是 Zed 的 UI 框架，Apache-2.0 许可证
- GPU 加速渲染，平台原生文本光栅化（DirectWrite/Core Text）
- 内置 Keymap/KeyContext 系统，天然支持 modal 键绑定
- 为编辑器应用设计，适合我们的场景

**替代方案**：
- egui：生产力高，但键盘处理原始，全局 vim 需要自己实现
- Tauri + SvelteKit：Web 技术栈，文本渲染和 vim 键绑定有天花板

**关键认知**：
- GPUI 不是"产品化框架"，文档稀缺，API 不稳定
- 需要读 Zed 源码作为文档
- 组件层自建（不用 gpui-component，见下「来源与组件库」）

**来源与组件库**（2026-08-08 调研后定，详见调研报告 `research/gpui_sustainability_report.md`）：
- **来源 = zed git pin**（钉 release tag，月度 bump）。crates.io 0.2.2 已被官方放弃（2025-12 起停更、Linux 仍是已废弃的 Blade 渲染器、官方示例不兼容），属慢性失血；git 拿到 wgpu 渲染器 + 新 API + 生态轨道，实测 API 漂移温和（同类项目 10 个月仅几处一行改动）
- **不引 gpui-component**：其生产级编辑器虽强，但满足 Phase 7 的 vim 需求需改其 `InputState` 底层源码——"为改底层而引入依赖"不成立；通用组件（VirtualList/TextView/Tree/DataTable 等）的提速不值绑 git 轨道 + bus factor≈2。组件自研，聚焦 agent 领域特定组件（消息列表/工具卡/流式块）。markdown 正文渲染不从零写解析器——用独立小 crate（comrak/pulldown-cmark 类）映射为 gpui 元素
- 风险对冲：core/net 保持零 GPUI 依赖，UI 依赖收敛在 ui/app 两 crate，触发条件命中（见调研报告 §6.2 重估信号）时迁移面可控

### 2. 编辑器：后置（原 Neovim 嵌入方案已否决）

**决策**：Editor 嵌入**明确后置**为高级功能（见 `migration-plan.md` Phase 7），不在当前架构主线。

**原方案已否决**：依赖系统 nvim 的 `nvim --embed` 子进程方案，因「非自包含、与产品定位冲突」被否决。详细调研见 `doc/research/editor_embed_report.agent.final.md`：
- libnvim 静态库嵌入：官方不支持，唯一生产用户 VimR 已放弃
- zed editor crate：技术耦合（~40 内部 crate）+ GPL 双否决
- `nvim --embed` 子进程：需用户预装 neovim，非自包含

**当前方向**（启动条件满足后再细化，现在不展开）：
- 组件底座倾向 gpui-component CodeEditor（tree-sitter + ropey + LSP）
- vim 键位层倾向 hjkl-engine 或自研模态引擎
- 验收规格：red VIM_COMPATIBILITY.md；对测：headless nvim golden-file

**关键认知**：vim 键位手感是需求，完整 vim 插件生态不是。Editor 与 agent 核心解耦，后置不影响 agent 对话、session、监控等核心功能先行。

### 3. 全局 vim 键绑定：GPUI Keymap 系统

**决策**：使用 GPUI 的 Keymap/KeyContext 系统实现全局 vim 键绑定。

**理由**：
- GPUI 内置 Keymap、KeyContext、Keystroke、Action 系统
- 与 Zed 的 vim mode 相同的架构
- 支持 context-aware 键绑定（"Editor && vim_mode == normal"）
- 不需要自己实现 modal 状态机

**架构**：
```
用户按键
  → GPUI Keymap（全局路由）
  → 判断焦点面板
  → 编辑器面板：转发给 nvim
  → 其他面板：应用自己的 modal 引擎
```

**关键认知**：
- 编辑器面板内：nvim 处理 vim 模式（真正的 vim）
- 编辑器面板外：应用处理 vim 导航（j/k、gg/G、/、Ctrl+W hjkl）
- 状态栏显示当前模式（编辑器内来自 nvim RPC，编辑器外来自应用状态）
- 键绑定可配置（哪些键全局生效、哪些编辑器优先）

### 4. 多机连接：P2P + 自动降级

**决策**：实现 ConnectionManager，支持 Direct/Tunnel/P2P 多种传输，自动切换。

**理由**：
- 用户有多台机器，无公网 IP
- 需要支持间歇性连接（上线/下线）
- P2P 优先（低延迟），Tunnel/Relay 降级（保证可用性）

**架构**：
```
ConnectionManager
  ├─ Direct（局域网直连）
  ├─ Tunnel（Cloudflare Tunnel）
  └─ P2P（iroh，QUIC）
  
自动状态机：
  Disconnected → Connecting → Connected (Direct)
                              ↓
                         Connected (P2P)
                              ↓ (P2P 断开)
                         Connected (Tunnel)
```

**关键认知**：
- 传输层抽象，上层无感知
- P2P 建立是渐进升级（先 Tunnel，后台尝试 P2P，成功后切换）
- P2P 断开是自动降级（检测断开，回退 Tunnel）
- 设备发现：mDNS（局域网）+ Relay 注册（广域网）
- 权限模型：连接 ≠ 授权，需要 token 认证

### 5. 通信协议：ClientProtocol trait + 多实现

**决策**：定义统一的 ClientProtocol trait，底层实现可插拔（Local/WebSocket/QUIC）。

**理由**：
- 本地模式（GPUI App 链接 core）：零网络开销，编译期类型安全
- 远程模式（GPUI App 连接远程 Gateway）：WebSocket 双向流
- 未来优化（QUIC）：更高性能，更低延迟
- 统一接口，可演化

**架构**：定义统一的 `ClientProtocol` trait，多个实现可插拔——`LocalProtocol`（直接调用 core）、`WebSocketProtocol`（远程，第一阶段）、`QuicProtocol`（QUIC，未来优化）。签名以代码为准，详见 [`network.md`](../design/network.md) §2。

**关键认知**：
- 本地模式：GPUI App 直接链接 ominiforge-core 作为库，无网络
- 远程模式：WebSocket（第一阶段），QUIC（未来优化）
- Web 前端：HTTP/SSE（过渡期保留，最终移除）
- Gateway 需要添加 WebSocket endpoint（与 HTTP/SSE 并存）

### 6. 配置系统：图形界面为主，Lua 后置

**决策**：配置以**图形界面为主入口**，Lua 配置作为**高级可选项后置**（非必须）。

**理由**：
- 图形界面是普通用户的主要入口
- Lua 配置与 Neovim 嵌入强相关（Neovim 后置，Lua 随之置后）
- 初期用图形界面 + 简单格式即可满足需求

**延后**：Lua 作为统一配置语言（Neovim 配置 + 系统配置）的完整方案，见 `config-lua.md`，待 Editor 嵌入启动时一并评估。

**架构**：
```
ominiforge.lua（用户配置）
  ↓
LSP 支持（类型定义文件 ominiforge.d.lua）
  ↓
图形界面（GPUI Settings 面板）
  ↓
双向同步（图形界面 ↔ Lua 代码）
```

**关键认知**：
- LSP 支持是关键（补全、验证、文档）
- 需要提供类型定义文件（EmmyLua 注解）
- 图形界面是主要入口，Lua 是高级用户的入口
- 双向同步是挑战（图形界面修改 → 更新 Lua 文件，Lua 文件修改 → 刷新图形界面）
- 配置同步：Last-Write-Wins + 字段级合并（轻量，自动）

### 7. 模块拆分：多 crate + trait 通信

**决策**：拆分为多个 crate，通过 trait 通信，组合优先。

**理由**：
- 模块化：每个功能域是独立 crate
- 超低耦合：模块间通过 trait 通信
- 组合优先：Application 层组合需要的模块
- 不重复维护：共享功能在 core，统一接口

**Crate 结构**：
```
crates/
  ominiforge-core/      # 核心：agent、session、event、tool、lsp、parsing、format
  ominiforge-config/    # Config Service（Phase 5 建）
  ominiforge-net/       # Network Service（Phase 3.3 建）：ClientProtocol + 传输
  ominiforge-ui/        # UI 组件库：theme、components、panels
  ominiforge-app/       # GPUI 桌面应用（组合所有模块）
  ominiforge-cli/       # CLI 工具（只有 serve 子命令，后续拆出）
```

**注**：`ominiforge-editor` 已随 Editor 后置移除，启动时重建。

**关键认知**：
- Core 无 GUI 依赖，可以独立编译和测试
- Editor/Config/Net 是 Service 层，通过 trait 通信
- App 是组装层，组合所有模块
- CLI 只依赖 Core，不包含 GUI

### 8. LSP/语法高亮/格式化：共享 Service

**决策**：LSP、语法高亮、格式化都在 ominiforge-core 中，Editor 和 Agent 共享。

**理由**：
- 不重复维护多套
- Agent 需要 LSP（代码分析、诊断）
- Editor 需要 LSP（补全、跳转、悬停）
- 共享 LSP 连接（rust-analyzer 只启动一次）

**架构**：
```
ominiforge-core/
  ├── lsp/              # LSP Service
  │   └── trait LspService
  ├── parsing/          # Tree-sitter Service
  │   └── trait SyntaxService
  └── format/           # Formatter Service
      └── trait FormatService

Editor → LspService
Agent  → LspService
```

**关键认知**：
- Tree-sitter = 语法解析（parsing），提供语法树、高亮、折叠
- LSP = 语言智能（language intelligence），提供补全、诊断、跳转
- 两者不同，都需要
- Neovim 的 LSP 可以通过 ominiforge-core 的 LspService 桥接（统一连接）

### 9. 许可证：GPL-3.0-or-later（主代码）

**决策**（2026-08-08 修订，原为 MIT OR Apache-2.0）：主代码采用 GPL-3.0-or-later。

**理由**：
- 单人长期个人项目，无商业化/闭源分发诉求，不介意他人闭源复用的门槛（作者判断：非大公司、无竞争顾虑）
- **解锁 zed git 版 gpui**：其经 `sum_tree → ztracing/zlog/ztracing_macro`（GPL-3.0-or-later）传染；项目本身为 GPL 后这些依赖合法，无需 vendor+patch 切断
- **解锁 zed GPL crate 复用的评估空间**（editor/language/lsp 等，原为 GPL 否决）——为 Phase 7 编辑器路线留门（是否实际复用，届时单独评估）

**关键认知**：
- GPL 传染性对**下游分发者**生效，不约束唯一版权持有者本人；可双重授权
- **一旦有外部 GPL 贡献即被锁死**：此后无法转回宽松许可证（除非逐贡献者征得同意或签 CLA）。本项目默认单人主导，外部贡献需签 CLA
- zed 的 gpui 本体是 Apache-2.0，但其 git 版拖入的 ztracing/zlog 是 GPL-3.0-or-later——转 GPL 前需 patch 切断，转 GPL 后合法
- `deny.toml` 的 licenses allow 已含 `GPL-3.0-or-later`（附注：仅在项目本身为 GPL 时成立）

**变更记录**：2026-08-08 由 MIT OR Apache-2.0 改为 GPL-3.0-or-later。同日 gpui 来源从 crates.io 0.2.2（冻结、Linux 旧 Blade 渲染器）切到 zed git pin（tag v1.14.2，wgpu 渲染器 + 新 API + 生态轨道）。

### 10. Sandbox：Feature Request（预留）

**决策**：Sandbox 作为 feature request，预留接口，后续专门讨论。

**理由**：
- Sandbox 是大功能，需要专门设计
- 当前 boxlite 是妥协版（Linux + Apple Silicon only）
- 未来需要轻量、可复现环境（统一开发/Agent/编辑环境）

**关键认知**：
- Sandbox Service 预留 trait
- 未来可以实现多种 runtime（boxlite、Nix、Docker、Firecracker）
- 与 Nix 理念一致（声明式、可复现）
- 需要在架构中预留位置

---

## 关键设计原则

### 1. 模块化 + 超低耦合 + 组合优先
- 每个功能域是独立 crate
- 模块间通过 trait 通信
- 替换实现不影响其他模块
- Application 层组合需要的模块

### 2. 不重复维护多套
- 所有共享功能在 core
- Editor 和 Agent 都是 core 的客户端
- 统一接口（trait）
- 避免"Agent 一套，Editor 一套"

### 3. Vim 核心体验
- Neovim 嵌入是唯一编辑器后端
- 全局 vim 键绑定（GPUI Keymap）
- 不做通用编辑器
- 状态栏显示 vim 模式

### 4. 分布式优先
- 多机平等（每个实例都是 peer）
- P2P 优先，Tunnel/Relay 降级
- 自动切换，上层无感知
- 设备发现（mDNS + Relay）

### 5. 文档先行
- 先更新文档定义清楚
- 再按文档执行
- 文档是单一事实来源

### 6. 大刀阔斧
- 不考虑过渡兼容
- 该删的直接删
- 冻结版本保底（dist/freeze）

---

## 不变的部分

- Rust Core 架构（agent、session、event、tool）
- Gateway 协议语义（HTTP/SSE/WS）
- Session 存储模型（append-only event log）
- MCP 扩展机制
- Hook 系统
- Permission 系统

---

## 新增的部分

- GPUI UI 层（替代 SvelteKit）
- NeovimBackend（编辑器集成）
- ConnectionManager（传输抽象 + 自动切换）
- P2P 传输（iroh）
- 设备发现与权限模型
- Lua 配置系统（LSP 支持 + 图形界面）
- ConfigSync（轻量配置同步）
- ClientProtocol trait（通信抽象）

---

## 移除的部分

- Web 前端（SvelteKit）— 过渡期保留，最终移除
- Tauri 桌面壳 — 不再需要（GPUI 直接是原生应用）
- CLI init/inspect 子命令 — 配置通过 Lua + 图形界面管理
- ts-rs 类型导出 — Web 前端移除后不需要

---

## Feature Request 记录

- **Sandbox**：轻量、可复现环境，统一开发/Agent/编辑环境，后续专门讨论
- **Eval 系统**：作为高级 feature，后续启用
- **手机 App**：独立开发，不通过 Web 前端

---

**本文档是架构决策的单一事实来源。后续设计和实施都应遵循本文档定义的决策和原则。**

---

## 文档生命周期说明

**本文档是临时文档**，用于记录架构转型的决策过程和理由。实施完成后，本文档将被删除。

**临时文档 vs 持久化文档**：

| 类型 | 文档 | 生命周期 | 内容 |
|------|------|---------|------|
| **临时文档** | `architecture-decisions.md`（本文档） | 实施完成后删除 | 决策理由、替代方案、权衡分析 |
| **临时文档** | `migration-plan.md` | 实施完成后删除 | 任务清单、时间线、风险 |
| **持久化文档** | `architecture.md` | 长期保留 | 系统架构、设计规范 |
| **持久化文档** | `editor.md`、`gpui-app.md` 等 | 长期保留 | 子系统设计、接口定义 |

**关键原则**：
- 本文档记录"为什么这么做"（决策理由、替代方案、权衡）
- 持久化文档只记录"是什么"和"怎么做"（架构、接口、协议）
- 实施完成后，本文档删除，决策的最终结果体现在持久化文档和代码中
