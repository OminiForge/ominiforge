# Ominiforge 架构转型实施规划

本文档定义从当前架构（Web 前端 + Gateway）到新架构（GPUI 客户端 + 多机分布式）的完整实施计划。

**当前进度**：Phase 0-1 ✅ 已完成 | Phase 2（Core 重构）⏳ 待开始

**核心原则**：
- 文档先行：先更新文档定义清楚，再按文档执行
- 大刀阔斧：不考虑过渡兼容，该删的直接删
- 模块化 + 超低耦合 + 组合优先
- 不重复维护多套

---

## 已完成 Phase

### Phase 0: 文档更新 ✅

已完成的架构文档：
- `doc/architecture.md`：更新 §3、§5、§18，新增 §21-26
- `doc/editor.md`：新建（EditorBackend、NeovimBackend、Grid 渲染、IME 桥接）
- `doc/gpui-app.md`：新建（GPUI 架构、UI 组件库、全局 vim 键绑定）
- `doc/config-lua.md`：新建（Lua 配置、LSP 支持、图形界面、配置同步）
- `doc/network.md`：新建（ClientProtocol、Local/WebSocket/QuicProtocol、ConnectionManager）
- `doc/gateway.md`：更新（WebSocket endpoint、QUIC endpoint）
- 删除：`doc/frontend.md`、`doc/tool-streaming.md`

### Phase 1: 代码清理 ✅

已删除的冗余代码：
- CLI 子命令：`init`、`inspect`（保留 `serve`、`eval`）
- ts-rs 相关：依赖、feature、89 处 `#[derive(TS)]` 标记、justfile targets
- CI 前端步骤：`ts-check`、`pnpm build`、整个 `frontend` job
- 保留：`frontend/` 目录（过渡期方案，见 `frontend/README.md`）

---

## Phase 2: Core 重构（3-5 天）

**目标**：按 `doc/architecture.md` §5 定义，重构 Core 为 Service 架构

### 2.1 定义 Service Traits

**按 `doc/architecture.md` §5 定义**

**任务**：
- [ ] 定义 `LspService` trait
- [ ] 定义 `SyntaxService` trait
- [ ] 定义 `FormatService` trait
- [ ] 定义 `SandboxService` trait（预留）

**文件**：
- `src/lsp/mod.rs`：定义 `LspService` trait
- `src/parsing/mod.rs`：新建，定义 `SyntaxService` trait
- `src/format/mod.rs`：定义 `FormatService` trait
- `src/sandbox/mod.rs`：定义 `SandboxService` trait（预留）

### 2.2 重构现有实现

**按 `doc/architecture.md` §5 定义**

**任务**：
- [ ] 重构 LSP 代码为 `LspService` 实现
- [ ] 重构 Tree-sitter 代码为 `SyntaxService` 实现
- [ ] 重构 Formatter 代码为 `FormatService` 实现

**文件**：
- `src/lsp/`：重构现有代码，实现 `LspService` trait
- `src/parsing/`：新建，从现有代码抽取 Tree-sitter 相关功能
- `src/format/`：重构现有代码，实现 `FormatService` trait

### 2.3 更新 Agent 使用 Service

**按 `doc/architecture.md` §5 定义**

**任务**：
- [ ] Agent 通过 `LspService` trait 访问 LSP
- [ ] Agent 通过 `SyntaxService` trait 访问语法解析
- [ ] Agent 通过 `FormatService` trait 访问格式化

**文件**：
- `src/agent/`：更新 Agent 代码，使用 Service traits

### 2.4 验证

**任务**：
- [ ] 运行所有测试（`cargo test`）
- [ ] 运行 clippy（`cargo clippy`）
- [ ] 确保 Agent 功能正常

---

## Phase 3: GPUI 技术验证（1-2 周）

**目标**：按 `doc/gpui-app.md` 和 `doc/editor.md` 定义，验证 GPUI + Neovim 可行性

### 3.1 创建 Workspace

**按 `doc/architecture.md` §5 定义**

**任务**：
- [ ] 创建 `crates/` 目录
- [ ] 移动现有代码到 `crates/ominiforge-core/`
- [ ] 创建 `crates/ominiforge-ui/`
- [ ] 创建 `crates/ominiforge-editor/`
- [ ] 创建 `crates/ominiforge-app/`

**文件**：
- `Cargo.toml`：改为 workspace
- `crates/ominiforge-core/Cargo.toml`：新建
- `crates/ominiforge-ui/Cargo.toml`：新建
- `crates/ominiforge-editor/Cargo.toml`：新建
- `crates/ominiforge-app/Cargo.toml`：新建

### 3.2 GPUI Hello World

**按 `doc/gpui-app.md` 定义**

**任务**：
- [ ] 创建最小 GPUI 窗口
- [ ] 渲染文本和 UI 元素
- [ ] 处理键盘输入

**文件**：
- `crates/ominiforge-app/src/main.rs`：GPUI 应用入口
- `crates/ominiforge-ui/src/lib.rs`：基础 UI 组件

### 3.3 Neovim 嵌入原型

**按 `doc/editor.md` 定义**

**任务**：
- [ ] 启动 `nvim --embed`
- [ ] 建立 msgpack-rpc 连接
- [ ] 接收 grid 事件
- [ ] 在 GPUI 中渲染 grid

**文件**：
- `crates/ominiforge-editor/src/neovim/mod.rs`：Neovim 连接管理
- `crates/ominiforge-editor/src/neovim/grid.rs`：Grid 渲染
- `crates/ominiforge-editor/src/neovim/rpc.rs`：RPC 通信

### 3.4 键盘输入路由

**按 `doc/editor.md` 定义**

**任务**：
- [ ] GPUI 键盘事件 → nvim 输入
- [ ] GPUI Keymap 系统
- [ ] 焦点管理

**文件**：
- `crates/ominiforge-app/src/keymap.rs`：键绑定系统
- `crates/ominiforge-editor/src/neovim/input.rs`：输入路由

### 3.5 IME 桥接

**按 `doc/editor.md` 定义**

**任务**：
- [ ] GPUI IME 事件 → nvim 输入
- [ ] IME composition 显示

**文件**：
- `crates/ominiforge-editor/src/neovim/ime.rs`：IME 桥接

### 3.6 验证

**任务**：
- [ ] 能在 GPUI 窗口中用 vim 编辑文件
- [ ] 能保存文件
- [ ] 能输入中文
- [ ] 全局键绑定工作（如 Ctrl+W hjkl 切换面板）

---

## Phase 4: GPUI 核心功能（2-3 周）

**目标**：GPUI 客户端可以完成基本的 Agent 对话 + 文件编辑

### 4.1 文件树面板

**按 `doc/gpui-app.md` 定义**

**任务**：
- [ ] 文件树 UI 组件
- [ ] 文件浏览和选择
- [ ] 打开文件到编辑器
- [ ] vim 键绑定（j/k 导航、/ 搜索、gg/G 跳转）

**文件**：
- `crates/ominiforge-ui/src/panels/file_tree.rs`：文件树面板

### 4.2 Agent 对话面板

**按 `doc/gpui-app.md` 和 `doc/network.md` 定义**

**任务**：
- [ ] 对话 UI 组件
- [ ] 连接 Gateway（通过 ClientProtocol）
- [ ] 发送消息和接收流式响应
- [ ] 工具调用可视化
- [ ] vim 键绑定（j/k 滚动、q 关闭）

**文件**：
- `crates/ominiforge-ui/src/panels/chat.rs`：对话面板
- `crates/ominiforge-net/src/client.rs`：ClientProtocol 实现

### 4.3 状态栏

**按 `doc/gpui-app.md` 定义**

**任务**：
- [ ] 状态栏 UI 组件
- [ ] 显示 vim 模式（来自 nvim RPC）
- [ ] 显示 session 状态
- [ ] 显示连接状态

**文件**：
- `crates/ominiforge-ui/src/panels/status_bar.rs`：状态栏

### 4.4 全局 vim 键绑定

**按 `doc/gpui-app.md` 定义**

**任务**：
- [ ] 面板切换（Ctrl+W hjkl）
- [ ] 列表导航（j/k、gg/G、/）
- [ ] 模式显示和切换

**文件**：
- `crates/ominiforge-app/src/keymap.rs`：全局键绑定

### 4.5 验证

**任务**：
- [ ] 能浏览文件并打开编辑
- [ ] 能与 Agent 对话
- [ ] 能在面板间切换
- [ ] 全局键绑定工作

---

## Phase 5: GPUI 完整功能（3-4 周）

**目标**：GPUI 客户端功能与 Web 前端对等

### 5.1 Session 管理

**任务**：
- [ ] Session 列表面板
- [ ] 创建 Session
- [ ] Fork Session
- [ ] 删除 Session
- [ ] Session 切换

**文件**：
- `crates/ominiforge-ui/src/panels/session_list.rs`：Session 列表面板

### 5.2 监控 Dashboard

**任务**：
- [ ] Usage 统计面板
- [ ] Cost 统计面板
- [ ] Trace 查看面板

**文件**：
- `crates/ominiforge-ui/src/panels/monitor.rs`：监控面板

### 5.3 配置管理

**按 `doc/config-lua.md` 定义**

**任务**：
- [ ] Lua 配置解析（`mlua` crate）
- [ ] 配置图形界面
- [ ] 配置验证和错误提示
- [ ] LSP 支持（`ominiforge.d.lua` 类型定义）

**文件**：
- `crates/ominiforge-config/src/lua.rs`：Lua 配置解析
- `crates/ominiforge-ui/src/panels/settings.rs`：配置界面

### 5.4 多机连接

**按 `doc/network.md` 定义**

**任务**：
- [ ] `ConnectionManager` 实现
- [ ] Direct 传输
- [ ] Tunnel 传输（Cloudflare Tunnel）
- [ ] P2P 传输（`iroh` crate）
- [ ] 设备发现（mDNS）
- [ ] 权限管理

**文件**：
- `crates/ominiforge-net/src/connection_manager.rs`：连接管理
- `crates/ominiforge-net/src/transports/`：各种传输实现

### 5.5 配置同步

**按 `doc/config-lua.md` 定义**

**任务**：
- [ ] Last-Write-Wins + 字段级合并
- [ ] Version vector
- [ ] 自动同步（连接建立时）

**文件**：
- `crates/ominiforge-config/src/sync.rs`：配置同步

### 5.6 验证

**任务**：
- [ ] 所有功能与 Web 前端对等
- [ ] 多机连接工作
- [ ] 配置同步工作

---

## Phase 6: Web 前端退出（1 周）

**目标**：Web 前端停止维护，最终移除

### 6.1 标记 deprecated

**任务**：
- [ ] 在 `frontend/README.md` 中标记为 deprecated
- [ ] 停止新功能开发

### 6.2 决定最终命运

**任务**：
- [ ] 选项 1：完全移除 `frontend/`
- [ ] 选项 2：保留为只读/轻量入口

### 6.3 清理

**任务**：
- [ ] 删除 `frontend/`（如果选择选项 1）
- [ ] 删除 Gateway 的静态文件服务（如果 Web 前端移除）
- [ ] 更新文档

---

## Phase 7: 高级功能（后续）

**目标**：实现高级功能

### 7.1 Sandbox（feature request）

**任务**：
- [ ] 设计 Sandbox 架构
- [ ] 实现 `SandboxService` trait
- [ ] 实现多种 Sandbox Runtime（boxlite、Nix、Docker）

### 7.2 Eval 系统

**任务**：
- [ ] 启用 `eval` feature
- [ ] 完善 Eval 系统

### 7.3 手机 App

**任务**：
- [ ] 设计手机 App 架构
- [ ] 实现手机 App（原生或跨平台）

---

## 时间线估算

| Phase | 时间 | 产出 | 状态 |
|-------|------|------|------|
| Phase 0: 文档更新 | 1-2 天 | 文档定义完成 | ✅ 已完成 |
| Phase 1: 代码清理 | 1-2 天 | 干净的代码库 | ✅ 已完成 |
| Phase 2: Core 重构 | 3-5 天 | Service traits 定义完成 | ⏳ 待开始 |
| Phase 3: GPUI 技术验证 | 1-2 周 | GPUI + Neovim 原型 | ⏳ 待开始 |
| Phase 4: GPUI 核心功能 | 2-3 周 | 基本可用 | ⏳ 待开始 |
| Phase 5: GPUI 完整功能 | 3-4 周 | 功能完整 | ⏳ 待开始 |
| Phase 6: Web 前端退出 | 1 周 | 完成切换 | ⏳ 待开始 |
| **总计** | **10-15 周** | **新架构完成** | **Phase 0-1/7 完成** |

**已完成**：Phase 0-1（2/7）
**当前**：Phase 2（Core 重构）
**剩余**：Phase 2-7（6 个 Phase）

---

## 风险和缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| GPUI 学习曲线陡峭 | Phase 3 延期 | 先做最小原型验证可行性 |
| Neovim 嵌入复杂 | Phase 3 延期 | 参考 Neovide 架构，分步实现 |
| IME 问题 | 中文输入不可用 | 提前测试，参考 GPUI 文档 |
| P2P 复杂 | Phase 5 延期 | 先用 Tunnel，P2P 作为后续优化 |
| Lua 配置复杂 | Phase 5 延期 | 先用 TOML，Lua 作为后续优化 |

---

## 执行原则

1. **文档先行**：先更新文档定义清楚，再按文档执行
2. **大刀阔斧**：不考虑过渡兼容，该删的直接删
3. **模块化 + 超低耦合 + 组合优先**：每个功能域是独立 crate，通过 trait 通信
4. **不重复维护多套**：所有共享功能在 core，Editor 和 Agent 都是客户端
5. **验证驱动**：每个 Phase 结束都有明确的验证标准

---

**本文档是实施的单一事实来源。所有实施工作都应按本文档定义执行。**

---

## 文档生命周期说明

**本文档是临时文档**，用于指导实施过程。实施完成后，本文档将被删除。

**临时文档 vs 持久化文档**：

| 类型 | 文档 | 生命周期 | 目的 |
|------|------|---------|------|
| **临时文档** | `migration-plan.md`（本文档） | 实施完成后删除 | 指导执行、记录任务清单 |
| **临时文档** | `architecture-decisions.md` | 实施完成后删除 | 记录决策理由、替代方案 |
| **持久化文档** | `architecture.md` | 长期保留 | 描述系统架构、设计规范 |
| **持久化文档** | `editor.md`、`gpui-app.md` 等 | 长期保留 | 描述子系统设计、接口定义 |

**关键原则**：
- 持久化文档只描述"是什么"和"怎么做"，不描述"为什么这么做"
- "为什么这么做"在临时文档、代码注释、commit message 中记录
- 实施完成后，临时文档删除，持久化文档保留并随系统演化更新
