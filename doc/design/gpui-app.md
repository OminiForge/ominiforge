<!-- status: current -->
<!-- owner: @OminiForge -->

# GPUI 应用

GPUI 应用是 ominiforge 的唯一用户界面，提供完整的 Agent 工作台体验。

## 1. 设计目标

- **Agent 对话为核心**：流式对话、工具调用可视化
- **全局键位绑定**：统一的键盘操作（GPUI Keymap 系统）
- **高性能**：GPU 加速渲染，平台原生文本光栅化
- **多机连接**：本地模式和远程模式，自动切换
- **模块化**：UI 组件库，可组合，可替换

## 2. 应用架构

### 2.1 面板布局

GPUI 应用采用面板布局，主要面板包括：

- **文件树面板**：浏览文件，打开文件到编辑器
- **编辑器面板**：（后置，见 `migration-plan.md` Phase 7）
- **对话面板**：Agent 对话，发送消息，查看响应
- **Session 列表面板**：管理 session（列表、创建、fork、删除）
- **监控面板**：查看 usage、cost、trace
- **设置面板**：配置管理（Lua + 图形界面）
- **状态栏**：显示 session 状态、连接状态（vim 模式显示属 Editor 后置功能）

### 2.2 面板管理

面板管理通过 GPUI 的 Dock 系统实现：

- 面板可以拖拽、调整大小、关闭
- 面板布局可以保存和恢复
- 面板焦点管理（键盘导航）

### 2.3 全局 vim 键绑定

全局键位绑定在 GPUI 的 Keymap/KeyContext 系统中实现，面板各自处理自己的导航键（文件树 j/k、对话面板滚动等），面板切换用统一快捷键。Editor 面板内的 vim 键位属后置功能（见 [`editor.md`](./editor.md) 与 `migration-plan.md` Phase 7）。

## 3. UI 组件库

### 3.1 主题系统

主题系统定义颜色、字体、间距等设计 tokens，是全部视觉**值**的单一事实源（`crates/ominiforge-ui/src/theme.rs` 的 `Theme`，进程级 gpui global）。设计**原则与语义**见 [`gpui-design.md`](./gpui-design.md)——本文档不重复其 token 表，只定结构。

**主题结构**：

- 颜色：语义化角色（canvas ladder、text ladder、accent、state 三态等）
- 字体：sans / chinese / mono 三族语义
- 间距：基于 4px 网格的命名梯度
- 圆角：标准化分级

**主题实现**：

- 暗色主题（默认）
- 亮色主题（可选）
- 用户自定义主题

### 3.2 通用组件

通用组件是可复用的 UI 元素：

- Button、Input、Select、Checkbox、Radio
- List、Tree、Table
- Modal、Tooltip、Popover
- Tabs、Accordion、Collapse

### 3.3 面板组件

面板组件是应用的核心 UI：

- FileTree：文件树面板
- Editor：编辑器面板（后置，见 `migration-plan.md` Phase 7）
- Chat：对话面板
- SessionList：Session 列表面板
- Monitor：监控面板
- Settings：设置面板
- StatusBar：状态栏

## 4. 通信

### 4.1 ClientProtocol

GPUI 应用通过 `ClientProtocol` trait 与 Core 通信——本地模式直接链接 `ominiforge-core`、远程模式走 WebSocket 连接 Gateway。模式定义与协议细节见 [`network.md`](./network.md) §2-§4。

### 4.2 事件订阅

GPUI 应用订阅 Core 的事件流：

- Session 事件（创建、更新、删除）
- Agent 事件（消息、工具调用、状态变更）
- 监控事件（usage、cost、trace）

**事件处理**：

- 事件驱动 UI 更新
- 增量更新（不重绘整个 UI）
- 事件过滤和路由

## 5. 配置管理

GPUI 客户端消费 Lua 配置（格式、图形界面设置面板、双向同步、多机同步），定义见 [`config-lua.md`](./config-lua.md)。设置面板（§3.3）是其前端载体。

## 6. 多机连接

GPUI 应用通过 `ConnectionManager` 管理多机连接（Direct/Tunnel/P2P 自动切换、设备发现、权限模型），定义见 [`network.md`](./network.md) §5。

## 7. 可访问性

- 键盘导航：所有功能可通过键盘访问
- 焦点管理：清晰的焦点指示
- 屏幕阅读器：语义化标签（GPUI 的 a11y 支持）
- 高对比度：主题系统支持
