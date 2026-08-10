<!-- status: current -->
<!-- owner: @OminiForge -->

# Editor 系统

> **状态：后置的高级功能**（`migration-plan.md` Phase 7）。本文描述的 Neovim `nvim --embed`
> 子进程方案已否决（非自包含、与产品定位冲突），内容仅作历史参考。启动条件与选型以
> `doc/research/editor_embed_report.agent.final.md` 为准重新评估。详见 `architecture.md` §22。

Editor 系统是 GPUI 客户端的核心组件，提供完整的 vim 编辑体验。

## 1. 设计目标

- **完整 vim 体验**：normal/visual/insert 模式、寄存器、宏、text objects
- **高性能渲染**：GPU 加速，平台原生文本光栅化
- **全局 vim 键绑定**：编辑器内外统一的 vim 操作
- **IME 支持**：中文/日文/韩文输入法
- **模块化**：通过 trait 抽象，可替换实现

## 2. 核心抽象

### 2.1 EditorBackend trait

Editor 系统通过 `EditorBackend` trait 抽象编辑器后端。所有编辑器实现必须实现此 trait。

**职责**：

- 渲染编辑器内容
- 处理用户输入
- 管理文件状态（打开、保存、关闭）
- 提供模式信息（vim 的 normal/insert/visual 等）
- 提供光标位置

**实现**：

- `NeovimBackend`：通过 `nvim --embed` 嵌入 Neovim（当前唯一实现）

### 2.2 编辑器面板

编辑器面板是 GPUI 客户端的一个面板组件，包含：

- 文件标签栏（打开的文件列表）
- 编辑器区域（Neovim grid 渲染）
- 状态栏（vim 模式、光标位置、文件状态）

## 3. NeovimBackend

### 3.1 Neovim 嵌入

NeovimBackend 通过 `nvim --embed` 启动 headless Neovim 进程，通过 msgpack-rpc 通信。

**通信协议**：

- Neovim UI 协议（grid-based）
- msgpack-rpc over stdio

**生命周期**：

- 启动：`nvim --embed` 启动进程
- 连接：建立 msgpack-rpc 连接
- 附加：`nvim_ui_attach` 订阅 UI 事件
- 断开：`nvim_ui_detach` + 终止进程

### 3.2 Grid 渲染

Neovim 的 UI 协议是基于字符网格（grid）的。NeovimBackend 接收 grid 事件，用 GPUI 渲染。

**Grid 事件**：

- `grid_resize`：grid 大小变化
- `grid_line`：一行字符更新
- `grid_clear`：清空 grid
- `grid_cursor_goto`：光标位置变化
- `hl_attr_define`：高亮属性定义

**渲染策略**：

- 每个 grid cell 包含字符和高亮 ID
- 高亮 ID 映射到颜色/样式（fg、bg、bold、italic 等）
- GPUI 的文本渲染管线绘制字符
- 支持字体连字（ligature）、平滑滚动、光标动画

### 3.3 IME 桥接

IME（输入法编辑器）是中文/日文/韩文输入的关键。NeovimBackend 在 GPUI 层桥接 IME 事件。

**IME 事件**：

- `ImeCompositionStart`：开始组合
- `ImeCompositionUpdate`：组合更新（未提交的文本）
- `ImeCompositionEnd`：组合结束（提交最终文本）

**桥接策略**：

- 组合期间：在 GPUI 层显示组合文本，不发送给 Neovim
- 组合结束：将最终文本发送给 Neovim
- 光标位置：考虑组合文本的偏移

### 3.4 键路由

键盘输入的路由策略：

- **编辑器面板有焦点**：按键转发给 Neovim（nvim 处理 vim 模式）
- **其他面板有焦点**：按键由应用处理（全局 vim 键绑定）

**Neovim 输入**：

- 普通按键：通过 `nvim_input` 发送
- 特殊按键：映射到 Neovim 的键码（如 `<C-w>`、`<Esc>`）
- 修饰键：Ctrl、Alt、Shift 的组合

## 4. 全局 vim 键绑定

全局 vim 键绑定在 GPUI 层实现，不影响编辑器面板内的 Neovim。

**实现方式**：

- GPUI 的 Keymap/KeyContext 系统
- 应用自己的 modal 引擎（normal/insert/visual 模式）
- 面板特定的键绑定（文件树、对话面板等）

**键绑定示例**：

- 文件树：j/k 导航、/ 搜索、gg/G 跳转
- 对话面板：j/k 滚动、q 关闭
- 面板切换：Ctrl+W hjkl

**状态栏显示**：

- 编辑器面板内：显示 Neovim 的 vim 模式（来自 nvim RPC）
- 编辑器面板外：显示应用的 modal 模式

## 5. LSP 集成

Editor 系统通过 `LspService`（在 ominiforge-core 中）访问 LSP 功能。

**共享 LSP 连接**：

- Editor 和 Agent 共享同一个 LSP 服务器连接
- rust-analyzer 等语言服务器只启动一次
- 诊断、符号表等状态在 Editor 和 Agent 之间共享

**Neovim LSP 桥接**：

- Neovim 的 LSP 请求通过 ominiforge-core 的 `LspService` 转发
- 避免 Neovim 和 Agent 各自启动 LSP 服务器

## 6. 语法高亮

Editor 系统通过 `SyntaxService`（在 ominiforge-core 中）访问语法高亮。

**Tree-sitter 集成**：

- Tree-sitter 解析在 ominiforge-core 中
- Editor 通过 `SyntaxService` 获取语法树和高亮
- Agent 通过 `SyntaxService` 获取代码结构分析
