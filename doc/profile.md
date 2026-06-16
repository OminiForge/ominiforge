# Profile 系统

## 1. 设计原则

- Profile 定义 agent 身份和能力组合，不涉及连接/计费细节。
- Provider 定义连接信息、model 元数据和 pricing。
- Profile 引用 provider/model，可 override 参数。
- 单继承，字段级覆盖。
- Session 绑定 profile，运行中切换 = 创建新 session。

## 2. Provider 配置

文件：`.omini/config/providers.toml`

```toml
[[providers]]
name = "openai-main"
type = "openai-chat"              # openai-chat | openai-completion | anthropic | custom
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0
pricing = { input_per_million = 2.50, output_per_million = 10.00, cache_read_per_million = 1.25 }

[[providers.models]]
id = "gpt-4o-mini"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0
pricing = { input_per_million = 0.15, output_per_million = 0.60 }


[[providers]]
name = "xiaomi-local"
type = "openai-chat"
base_url = "http://localhost:8080/v1"
api_key_env = "XIAOMI_API_KEY"

[[providers.models]]
id = "mimo-7b"
context_window = 32000
max_output_tokens = 8192
default_temperature = 0.7
pricing = { input_per_million = 0.0, output_per_million = 0.0 }


[[providers]]
name = "anthropic"
type = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[[providers.models]]
id = "claude-sonnet-4-6"
context_window = 200000
max_output_tokens = 16000
default_temperature = 0.0
pricing = { input_per_million = 3.00, output_per_million = 15.00, cache_read_per_million = 0.30, cache_write_per_million = 3.75 }
```

### 2.1 Provider type

| Type | 协议 | 说明 |
|------|------|------|
| `openai-chat` | OpenAI Chat Completions API | 最常见，兼容大量第三方 |
| `openai-completion` | OpenAI Completions API (legacy) | 旧接口 |
| `anthropic` | Anthropic Messages API | Claude 系列 |
| `custom` | 自定义 adapter（后续） | 需实现 provider trait |

### 2.2 Provider 字段

| 字段 | 必填 | 说明 |
|------|------|------|
| name | ✓ | 唯一标识，profile 引用用 |
| type | ✓ | 协议类型 |
| base_url | ✓ | API endpoint |
| api_key_env | ✓ | 环境变量名（不直接存 key） |
| models | ✓ | 该 provider 可用的 model 列表 |

### 2.3 Model 字段

| 字段 | 必填 | 说明 |
|------|------|------|
| id | ✓ | model 标识（发给 API 的值） |
| context_window | ✓ | 最大 context tokens |
| max_output_tokens | ✓ | 最大输出 tokens |
| default_temperature | ✗ | 默认温度，默认 0.0 |
| pricing | ✗ | 计费信息（用于成本估算） |

## 3. Profile 配置

文件：`.omini/profiles/{name}.toml`

```toml
[profile]
name = "coding"
description = "Software development agent"
extends = "base"                     # 可选，单继承

[prompt]
system = """
You are a software engineering assistant. You write clean, tested code.
"""
# 或引用文件：
# system_file = "prompts/coding.md"

[model]
default = "openai-main/gpt-4o"      # provider_name/model_id
fallback = "openai-main/gpt-4o-mini" # 降级模型
# 可选 override（覆盖 provider 默认值）
temperature = 0.0
max_output_tokens = 16384

[context]
compaction_threshold = 0.8           # 何时触发压缩（% of context window）
injection_max_tokens = 4096          # 每轮动态注入上限

[tools]
builtin = ["read", "write", "shell", "search", "lsp"]
mcp_servers = ["github"]             # 引用 mcp.toml 中的 server name
disabled = []                        # 显式禁用

[skills]
enabled = ["git-commit", "code-review", "refactor"]

[memory]
scopes = ["user", "project"]         # 可访问的 memory scope
auto_write = true                    # agent 可否自动写入 memory

[budget]
session_max_usd = 10.00
daily_max_usd = 50.00
warn_at_percent = 80

[hooks]
before_tool = ["security-guard"]     # 额外绑定的 hook
```

### 3.1 Profile 字段说明

| 字段 | 必填 | 说明 |
|------|------|------|
| name | ✓ | 唯一标识 |
| description | ✗ | 人类可读说明 |
| extends | ✗ | 继承的父 profile |
| prompt.system | ✓ | system prompt（或 system_file） |
| model.default | ✓ | 默认 model（provider_name/model_id） |
| model.fallback | ✗ | 降级 model |
| model.temperature | ✗ | override provider 默认值 |
| model.max_output_tokens | ✗ | override provider 默认值 |
| context.* | ✗ | 有合理默认值 |
| tools.* | ✗ | 默认全部可用 |
| skills.* | ✗ | 默认全部可用 |
| memory.* | ✗ | 默认 scopes=["user","project"], auto_write=true |
| budget.* | ✗ | 默认无限制 |
| hooks.* | ✗ | 默认无额外 hook |

### 3.2 Model 引用格式

```text
"openai-main/gpt-4o"         # 完整引用：provider_name/model_id
"gpt-4o"                      # 短引用：从已配置 providers 搜索第一个匹配
```

推荐使用完整引用避免歧义（同一 model_id 可能在多个 provider 中存在）。

## 4. 继承规则

- `extends` 只支持单继承。
- 子 profile 中出现的字段完整覆盖父字段（不做 list merge）。
- 未出现的字段继承父值。
- 无 `extends` 时使用硬编码默认值。
- 继承链最大深度 = 5（防止循环）。

示例：

```toml
# base.toml
[model]
default = "openai-main/gpt-4o"

[tools]
builtin = ["read", "write", "shell"]

[budget]
session_max_usd = 5.00
```

```toml
# coding.toml — extends base
[tools]
builtin = ["read", "write", "shell", "search", "lsp"]  # 完整覆盖

[budget]
session_max_usd = 10.00  # 覆盖
# daily_max_usd 继承 base（如果 base 有的话）
```

## 5. Session 与 Profile 的关系

- Session 启动时绑定一个 profile，`session.toml` 记录 `profile_id`。
- 首条 event（SessionEvent::Created）记录 profile 配置快照。
- 运行中切换 profile → 创建新 session（origin.kind = "reconfiguration"），自动带 context_snapshot。
- 用户命令：`/profile coding` → 切换并创建新 session。
- 同一 session 内 profile 不可变（历史不可变原则）。

## 6. Profile 变更对 Cache 的影响

| 变化 | 影响 |
|------|------|
| 同 profile 运行中 | system prompt + tool schemas 稳定 → cache 持续命中 |
| 切换 profile | 新 session，新 prefix → 首次 miss，后续正常命中 |
| Profile 内 system prompt 修改 | 影响所有使用此 profile 的新 session |
| Profile 内 tool set 变化 | tool schemas block 变化 → cache miss 一次 |

建议：profile 的 system prompt 和 tool set 不要频繁修改。

## 7. 职责划分总结

| 属性 | 归属 | 理由 |
|------|------|------|
| API endpoint / protocol | Provider | 连接属性 |
| context_window | Provider (model) | model 固有属性 |
| pricing | Provider (model) | 计费属性 |
| default_temperature | Provider (model) | model 推荐默认值 |
| temperature override | Profile | agent 行为偏好 |
| max_output_tokens override | Profile | agent 行为偏好 |
| compaction_threshold | Profile | 用户偏好 |
| injection_max_tokens | Profile | 用户偏好 |
| budget limit | Profile | 不同 agent 角色预算不同 |
| system prompt | Profile | agent 身份 |
| tool set | Profile | agent 能力 |
| skill set | Profile | agent 能力 |
| memory scope | Profile | agent 知识范围 |

## 8. 文件系统布局

```text
.omini/
├── config/
│   ├── providers.toml       # provider + model 配置
│   ├── mcp.toml             # MCP server 配置
│   └── hooks.toml           # hook 配置
└── profiles/
    ├── base.toml
    ├── coding.toml
    ├── research.toml
    └── daily.toml
```

## 9. 待后续完善

- Profile 模板（ominiforge 预置几个常用 profile）。
- Profile 导入导出（分享 profile 配置）。
- Provider 健康检查和自动 fallback。
- 多 provider 负载均衡（同一 model 多个 endpoint）。
