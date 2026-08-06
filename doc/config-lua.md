# 配置系统（Lua）

配置系统使用 Lua 作为统一配置语言，提供图形界面和 LSP 支持。

## 1. 设计目标

- **统一配置语言**：Neovim 配置和系统配置都使用 Lua
- **类型安全**：LSP 支持（补全、验证、文档）
- **图形界面**：GPUI 设置面板，图形化编辑
- **多机同步**：轻量自动同步
- **可扩展**：配置即代码，支持逻辑

## 2. Lua 配置格式

### 2.1 配置文件

配置文件为 `ominiforge.lua`，返回一个 Lua table。

**配置结构**：
- `agent`：Agent 配置（model、max_sessions 等）
- `editor`：Editor 配置（backend、neovim 选项等）
- `keybindings`：键绑定配置（mode、自定义键绑定等）
- `ui`：UI 配置（theme、font、layout 等）
- `network`：网络配置（peers、transports 等）
- `gateway`：Gateway 配置（bind、api_key_env 等）
- `session`：Session 配置（storage、retention 等）

### 2.2 配置层级

配置支持层级覆盖：

```
default config（内置默认值）
  → user config（~/.config/ominiforge/ominiforge.lua）
  → project config（.omini/ominiforge.lua）
  → profile config（.omini/profiles/{profile}.lua）
  → session override（session 特定配置）
```

**覆盖规则**：
- 后面的配置覆盖前面的配置
- 表（table）合并，不是替换
- 数组（array）替换，不是合并

### 2.3 配置验证

配置加载时进行验证：
- 类型检查（string、number、boolean、table）
- 枚举值检查（如 `editor.backend` 只能是 `"neovim"`）
- 必填字段检查
- 自定义验证函数

**验证失败处理**：
- 显示详细错误信息（字段路径、错误原因）
- 使用默认值（如果可能）
- 拒绝启动（如果关键配置错误）

## 3. LSP 支持

### 3.1 类型定义文件

提供 `ominiforge.d.lua` 类型定义文件，使用 EmmyLua 注解。

**注解类型**：
- `---@class`：定义配置表结构
- `---@field`：定义字段类型和描述
- `---@type`：指定变量类型
- `---@alias`：定义类型别名（枚举值）

### 3.2 LSP 功能

通过 `lua-language-server` 提供：

**自动补全**：
- 键名补全（输入 `agent.` 后提示 `model`、`max_sessions`）
- 枚举值补全（输入 `editor.backend = "` 后提示 `"neovim"`）
- 路径补全（文件路径字段）

**类型检查**：
- 类型错误（如 `model = 123` 应该是 string）
- 未知字段（如 `agent.unknown_field`）
- 必填字段缺失

**悬停文档**：
- 字段描述（鼠标悬停显示字段说明）
- 类型信息（字段类型、默认值）
- 示例（字段使用示例）

**跳转定义**：
- 跳转到类型定义
- 跳转到字段定义

### 3.3 LSP 配置

用户需要在编辑器中配置 `lua-language-server`：

**VSCode**：
- 安装 Lua 扩展
- 配置 `Lua.workspace.library` 包含 `ominiforge.d.lua`

**Neovim**：
- 安装 `lua-language-server`
- 配置 `workspace.library` 包含 `ominiforge.d.lua`

## 4. 图形界面

### 4.1 设置面板

GPUI 客户端提供 Settings 面板，图形化编辑配置。

**界面结构**：
- 左侧：配置分类（Agent、Editor、Keybindings、UI、Network 等）
- 右侧：配置表单（输入框、下拉框、复选框等）
- 底部：操作按钮（保存、取消、重置、编辑 Lua 源码）

**表单控件**：
- 文本输入框：string 类型
- 数字输入框：number 类型
- 复选框：boolean 类型
- 下拉框：枚举类型
- 文件选择器：路径类型
- 颜色选择器：颜色类型

### 4.2 实时验证

图形界面实时验证配置：
- 输入时验证（类型检查、枚举值检查）
- 错误提示（字段下方显示错误信息）
- 警告提示（不推荐但合法的值）

### 4.3 双向同步

图形界面和 Lua 代码双向同步：

**图形界面 → Lua**：
- 用户在图形界面修改配置
- 生成对应的 Lua 代码
- 写入 `ominiforge.lua`

**Lua → 图形界面**：
- 解析 `ominiforge.lua`
- 在图形界面显示当前值
- 用户可以切换回图形界面继续编辑

**同步冲突**：
- 如果 Lua 文件被外部修改，提示用户重新加载
- 如果图形界面有未保存修改，提示用户保存或放弃

## 5. 配置同步

### 5.1 同步策略

多机配置同步使用 Last-Write-Wins + 字段级合并。

**字段级合并**：
- 配置是扁平的键值对（如 `agent.model = "gpt-4"`）
- 每个字段记录最后修改时间和机器
- 合并时按字段取最后修改的值

**同步触发**：
- 连接建立时自动同步
- 配置修改时自动广播
- 定期心跳同步（可选）

### 5.2 同步元数据

同步元数据存储在 `ominiforge.sync.toml`：

**元数据内容**：
- `machine_id`：当前机器 ID
- `version_vector`：版本向量（每台机器的版本号）
- `last_modified`：每个字段的最后修改时间和机器

### 5.3 冲突处理

**同一字段并发修改**：
- Last-Write-Wins（最后修改的值胜出）
- 极少发生（配置变更频率低）
- 可以手动解决（查看历史版本）

**不同字段并发修改**：
- 自动合并（不同字段不冲突）
- 无需用户干预

## 6. 配置示例

### 6.1 基础配置

```lua
return {
    agent = {
        model = "gpt-4",
        max_sessions = 10,
    },
    editor = {
        backend = "neovim",
        neovim = {
            use_user_config = false,
        },
    },
    keybindings = {
        mode = "vim",
    },
    ui = {
        theme = "dark",
        font = {
            family = "JetBrains Mono",
            size = 14,
        },
    },
}
```

### 6.2 高级配置

```lua
-- 可以使用 Lua 逻辑
local function get_model()
    if os.getenv("OMINI_MODEL") then
        return os.getenv("OMINI_MODEL")
    else
        return "gpt-4"
    end
end

return {
    agent = {
        model = get_model(),
        max_sessions = 10,
    },
    -- ...
}
```

## 7. 配置迁移

### 7.1 从 TOML 迁移

旧版本使用 TOML 配置（`gateway.toml`、`mcp.toml` 等）。

**迁移策略**：
- 提供迁移工具（`ominiforge migrate-config`）
- 自动转换 TOML 到 Lua
- 保留原 TOML 文件（备份）

### 7.2 版本兼容

配置系统支持版本号：

```lua
return {
    version = 1,  -- 配置版本号
    agent = {
        -- ...
    },
}
```

**版本升级**：
- 配置加载时检查版本号
- 自动升级旧版本配置
- 保留备份

## 8. 安全考虑

### 8.1 敏感信息

敏感信息（API key、token）不应进入配置文件。

**存储方式**：
- 环境变量
- Secret store（SQLite）
- 系统 keychain

**配置引用**：
```lua
return {
    providers = {
        openai = {
            api_key_env = "OPENAI_API_KEY",  -- 引用环境变量
        },
    },
}
```

### 8.2 Lua 沙箱

Lua 配置在沙箱中执行，限制危险操作：

**禁止的操作**：
- 文件系统访问（`io.open`、`os.remove` 等）
- 网络访问（`socket.http` 等）
- 系统命令（`os.execute` 等）

**允许的操作**：
- 字符串操作（`string.*`）
- 表操作（`table.*`）
- 数学运算（`math.*`）
- 环境变量读取（`os.getenv`）

