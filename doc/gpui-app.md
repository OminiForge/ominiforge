# GPUI 应用

GPUI 应用是 ominiforge 的唯一用户界面，提供完整的 Agent 工作台体验。

## 1. 设计目标

- **极致编辑体验**：Neovim 嵌入，完整 vim 功能
- **全局 vim 键绑定**：统一的 modal 操作
- **高性能**：GPU 加速渲染，平台原生文本光栅化
- **多机连接**：本地模式和远程模式，自动切换
- **模块化**：UI 组件库，可组合，可替换

## 2. 应用架构

### 2.1 面板布局

GPUI 应用采用面板布局，主要面板包括：

- **文件树面板**：浏览文件，打开文件到编辑器
- **编辑器面板**：Neovim 嵌入，编辑文件
- **对话面板**：Agent 对话，发送消息，查看响应
- **Session 列表面板**：管理 session（列表、创建、fork、删除）
- **监控面板**：查看 usage、cost、trace
- **设置面板**：配置管理（Lua + 图形界面）
- **状态栏**：显示 vim 模式、session 状态、连接状态

### 2.2 面板管理

面板管理通过 GPUI 的 Dock 系统实现：
- 面板可以拖拽、调整大小、关闭
- 面板布局可以保存和恢复
- 面板焦点管理（键盘导航）

### 2.3 全局 vim 键绑定

全局 vim 键绑定在 GPUI 的 Keymap/KeyContext 系统中实现（编辑器面板内转发给 Neovim、面板外由应用的 modal 引擎处理、状态栏显示当前模式）。机制定义见 [`editor.md`](./editor.md) §4。

## 3. UI 组件库

### 3.1 主题系统

主题系统定义颜色、字体、间距等设计 tokens。

**主题结构**：
- 颜色：语义化命名（surface_base、text_primary、accent 等）
- 字体：字体家族、大小、行高、字重
- 间距：标准化的间距值（space_xs、space_sm、space_md 等）
- 圆角：标准化的圆角值（radius_sm、radius_md 等）

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
- Editor：编辑器面板（NeovimBackend）
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

## 7. 性能优化

### 7.1 渲染优化

- 增量渲染：只重绘变化的区域
- 视口裁剪：只渲染可见区域
- GPU 加速：GPUI 的渲染管线

### 7.2 通信优化

- 批量更新：合并多个事件
- 异步处理：不阻塞 UI
- 增量同步：只同步变化的数据

### 7.3 内存优化

- 对象池：复用 UI 组件
- 缓存：缓存渲染结果
- 延迟加载：按需加载面板

## 8. 测试策略

### 8.1 单元测试

- UI 组件渲染
- 键绑定处理
- 配置解析

### 8.2 集成测试

- 面板切换
- 事件订阅
- 配置同步

### 8.3 端到端测试

- 完整工作流程
- 多机连接
- 全局键绑定

## 9. 可访问性

- 键盘导航：所有功能可通过键盘访问
- 焦点管理：清晰的焦点指示
- 屏幕阅读器：语义化标签（GPUI 的 a11y 支持）
- 高对比度：主题系统支持

## 10. 未来扩展

### 10.1 插件系统

- GPUI 应用插件（扩展 UI 功能）
- 面板插件（自定义面板）
- 主题插件（自定义主题）

### 10.2 协作功能

- 多人协作编辑（CRDT）
- 共享 session
- 实时通信

### 10.3 移动端

- 独立开发的手机 App
- 不通过 Web 前端
- 主要负责审批、监控、通知
