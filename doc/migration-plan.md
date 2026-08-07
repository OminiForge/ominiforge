# Ominiforge 架构转型实施规划

本文档定义从当前架构（Web 前端 + Gateway）到新架构（GPUI 客户端 + 多机分布式）的完整实施计划。

**当前进度**：Phase 0-3.3 ✅ | Phase 3.4（Agent 对话面板）⏳ 待开始

**核心原则**：
- 文档先行：先更新文档定义清楚，再按文档执行
- 大刀阔斧：不考虑过渡兼容，该删的直接删
- 模块化 + 超低耦合 + 组合优先
- 不重复维护多套
- **核心功能优先**：agent 对话、session、监控先行；editor 嵌入后置

---

## 已完成 Phase

### Phase 0: 文档更新 ✅

### Phase 1: 代码清理 ✅

### Phase 2: Core 重构 ✅

### Phase 3.1-3.2: Workspace + GPUI 基础 ✅

已完成：
- workspace 拆分（`crates/ominiforge-core` / `-ui` / `-app`），`[workspace.package]` + `[workspace.lints]` 统一
- gpui 0.2 依赖接入，`test-support` 无头测试链打通
- 组件测试模式确立（`simulate_keystrokes` 行为断言 + `debug_bounds` 布局断言，无像素 diff）

**注**：曾有的 `StatusBar` 组件是 editor 残留（围绕 vim 模式），随 editor 后置一并移除。

### Phase 3.3: ClientProtocol 本地模式 ✅

已完成：
- 新建 `ominiforge-net` crate；`ClientProtocol` trait 定义全部客户端↔Core 操作面（session 生命周期 / 消息 / 事件订阅 / 监控 / 配置 / 连接状态）
- `LocalProtocol` 复用 `SessionRegistry` + `ActorHandle` + `StatusHub`（与 gateway 同一份，零网络零序列化）；事件订阅复刻 SSE 的 subscribe-first → replay → `ReplayEnd` → live 语义
- core 侧补 `gateway::actor` 的 re-export（`GatewayEvent`/`Command`/`Delta`）
- `cargo clippy -p ominiforge-net` 零警告，workspace 全量编译通过

**文件**：`crates/ominiforge-net/src/{lib,local}.rs`；core 侧 `gateway/mod.rs` 增 re-export。

---

## 当前 Phase

## Phase 3.3+: GPUI Agent 核心功能

**目标**：GPUI 客户端能完成完整的 agent 对话闭环（连接 core、发消息、收流式响应、工具调用可视化），不依赖任何 editor。

### 3.4 Agent 对话面板

**按 `doc/gpui-app.md` §3.3 定义**

**任务**：
- [ ] 对话 UI 组件（消息列表、输入框）
- [ ] 通过 `ClientProtocol` 发送消息
- [ ] 渲染流式响应（text delta、tool call、tool result）
- [ ] 工具调用可视化
- [ ] 键位绑定（j/k 滚动、q 关闭、Enter 发送）

**文件**：
- `crates/ominiforge-ui/src/panels/chat.rs`

### 3.5 Session 管理面板

**任务**：
- [ ] Session 列表 UI
- [ ] 创建 / 切换 / 删除 session
- [ ] Fork session

**文件**：
- `crates/ominiforge-ui/src/panels/session_list.rs`

### 3.6 监控面板

**任务**：
- [ ] Usage / Cost 统计展示
- [ ] Trace 查看

**文件**：
- `crates/ominiforge-ui/src/panels/monitor.rs`

### 3.7 文件树面板（只读浏览）

**任务**：
- [ ] 文件树 UI
- [ ] 浏览、选择、预览（只读，不编辑）
- [ ] 键位绑定（j/k 导航、/ 搜索）

**文件**：
- `crates/ominiforge-ui/src/panels/file_tree.rs`

### 3.8 验证

**任务**：
- [ ] 本地模式连上 core，发起一轮完整 agent 对话
- [ ] 流式响应实时渲染
- [ ] 工具调用可见
- [ ] 能管理 session（创建/切换/fork）
- [ ] 能浏览文件树
- [ ] 所有面板键位在无头测试下断言（`simulate_keystrokes` + `debug_bounds`）

---

## 后续 Phase

### Phase 4: 远程模式 + 多机连接

**目标**：GPUI 客户端能连接远程 Gateway。

### 4.1 WebSocket 协议

- [ ] `WebSocketProtocol` 实现 `ClientProtocol`
- [ ] Gateway 添加 WebSocket endpoint（与 HTTP/SSE 并存）

### 4.2 ConnectionManager

- [ ] Direct / Tunnel / P2P 传输抽象
- [ ] 自动状态机与降级
- [ ] 设备发现（mDNS）

**文件**：`crates/ominiforge-net/src/`

### 4.3 权限与认证

- [ ] token 认证
- [ ] 连接 ≠ 授权

---

### Phase 5: 配置系统

**目标**：配置管理（图形界面为主入口）。

- [ ] 配置图形界面（Settings 面板）
- [ ] 配置验证与错误提示
- [ ] 配置同步（Last-Write-Wins + 字段级合并）

**文件**：`crates/ominiforge-config/`（此时建 crate）

**说明**：Lua 配置（`config-lua.md`）作为**高级可选项**延后，非必须。初期用图形界面 + 简单格式即可。

---

### Phase 6: Web 前端退出

- [ ] GPUI 客户端功能与 Web 前端对等的部分全部覆盖后，标记 `frontend/` deprecated
- [ ] 停止新功能开发
- [ ] 决定最终移除或保留只读

---

### Phase 7: Editor 嵌入（后置，高级功能）

**状态**：**明确后置**。Editor 嵌入（NeovimBackend / 自研模态引擎）是一个独立的、工程量巨大的高级功能，与 agent 核心解耦。在 agent 对话、session、监控、远程连接全部稳定之前，不投入。

**启动条件**（届时才评估，现在不展开）：
- Agent 核心功能完整且稳定
- 重新评估 vim 完备度目标（键位手感 vs 完整 vim）
- 基于 `doc/research/editor_embed_report.agent.final.md` 的结论选型

**已否决的路线**（调研结论，见该报告）：
- libnvim 静态库嵌入（官方不支持，唯一生产用户已放弃）
- zed editor crate（技术耦合 + GPL 双否决）
- 依赖系统 nvim 的 `nvim --embed` 子进程（非自包含，与产品定位冲突）

---

## 时间线估算

| Phase | 产出 | 状态 |
|-------|------|------|
| Phase 0-2: 文档/清理/Core 重构 | 干净的代码库 + Service traits | ✅ 已完成 |
| Phase 3.1-3.2: Workspace + GPUI 基础 | 组件测试模式 + 最小窗口 | ✅ 已完成 |
| Phase 3.3: ClientProtocol 本地模式 | `ominiforge-net` + `LocalProtocol` | ✅ 已完成 |
| Phase 3.4+: Agent 对话核心 | 可用的 agent 对话客户端 | ⏳ 当前 |
| Phase 4: 远程模式 + 多机 | 分布式连接 | ⏳ 待开始 |
| Phase 5: 配置系统 | 图形化配置 | ⏳ 待开始 |
| Phase 6: Web 前端退出 | 完成切换 | ⏳ 待开始 |
| Phase 7: Editor 嵌入 | （后置，启动条件满足才排期） | 🔒 锁定 |

**关键变化**：editor 从「Phase 3 核心」降级为「Phase 7 后置高级功能」，agent 对话核心提前为当前最高优先级。

---

## 风险和缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| GPUI 学习曲线 | 面板开发慢 | 先用最小组件验证，复用 Zed 测试模式 |
| ClientProtocol 抽象不当 | 本地/远程模式分叉 | 先只实现 LocalProtocol，远程模式验证后再抽象 |
| Editor 后置导致返工 | 面板布局需预留 editor 位置 | 面板系统设计为可插拔（dock），editor 面板后续作为一个新 panel 加入 |

---

## 执行原则

1. **文档先行**：先更新文档定义清楚，再按文档执行
2. **大刀阔斧**：不考虑过渡兼容，该删的直接删
3. **模块化 + 超低耦合 + 组合优先**：每个功能域是独立 crate，通过 trait 通信
4. **不重复维护多套**：所有共享功能在 core，各客户端统一接口
5. **核心功能优先**：agent 对话、session、监控先行；editor 嵌入后置
6. **验证驱动**：每个 Phase 结束都有明确的验证标准
7. **完成即清理**：完成一个 Phase 后，将详细任务清单从正文删除，换成简短总结移入「已完成 Phase」区域

---

**本文档是实施的单一事实来源。所有实施工作都应按本文档定义执行。**

---

## 文档生命周期说明

**本文档是临时文档**，实施完成后删除。持久化文档（`architecture.md`、`gpui-app.md` 等）长期保留并随系统演化更新。
