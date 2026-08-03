# Profile 系统

## 1. 设计原则

- Profile 定义 agent 身份和能力组合，不涉及连接/计费细节。
- Provider 定义连接信息和 model 元数据。
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

[[providers.models]]
id = "gpt-4o-mini"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0


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
| thinking | ✗ | 推理行为：`none`（默认）/`optional`/`always` |
| think_efforts | ✗ | 可选推理强度档位，**原始 provider 字符串**（如 `["low","high","max"]` 或 `["low","medium","high"]`）；空 = 无可选档位，不显示强度选择器 |
| modalities | ✗ | 文本以外的输入模态（如 `["image","video"]`） |

> **推理强度档位为何存原始字符串**：各家对「推理强度」的命名与可选集合不一致
> （Kimi K3 用 `low/high/max`，MiMo 用 `low/medium/high`），前端选择器直接展示模型
> 声明的档位名，请求时按 OpenAI 兼容的 `reasoning_effort` 字段原样透传。Profile 的
> `model.think_effort` 与会话的每轮 effort 覆盖都只是**引用**这里声明的档位；
> 引用了一个模型未声明的档位时会被丢弃（不会把一个模型的档位发给另一个模型）。
> **只声明官方文档确认或实测接受的档位**——不接受 `reasoning_effort` 的模型
> （如 Kimi K2.x，用 `thinking` 参数开关思考）留空。

### 2.4 内置 Provider（catalog）

除用户 `providers.toml` 外，二进制内置一份 provider 目录（`src/config/catalog.toml`，
编译期内嵌），加载时**追加**在用户条目之后。设计要点：

- **只读**：内置条目不写入用户文件；设置页对它们渲染为「连接卡片」（名称 +
  model 列表 + API key 输入），不提供字段编辑；`PUT /providers` 携带内置名称时
  返回 400，而不是静默写入。
- **认证**：卡片里粘贴的 key 存入 secret store（`PUT /secrets/{provider}`），resolve
  时优先于 `api_key_env` 环境变量。连接成功后该 provider 的全部 model 立即可在
  profile / `--model` 中以 `provider/model_id` 引用。
- **覆盖**：用户在 `providers.toml` 中定义同名 provider 会 shadow 内置条目（同名
  合并去重，用户条目排序在前，`find_model` 取第一个匹配）。这是逃生通道，不是
  推荐路径。
- **扩展方式**：新增内置 provider 必须只是数据变更（改 `catalog.toml`）；需要新
  协议时先实现 adapter（`src/provider/`）。model 清单按官方文档刷新，随发版更新。

当前内置：`kimi-code`、`kimi-platform`（Kimi / Moonshot）、`mimo-token`、`mimo-api`（Xiaomi MiMo）。

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
default = "openai-main/gpt-4o"      # provider_name/model_id（设置页从可用模型下选）
fallback = "openai-main/gpt-4o-mini" # 降级模型
think_effort = "high"               # 默认推理强度档位（须为所选模型声明的档位）
# temperature / max_output_tokens 不再由 profile 覆盖：它们是模型自身的元数据
# （providers.toml 的 ModelConfig），profile 只引用模型。

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

[network]
policy = "allowlist"                 # isolated | allowlist | open；缺省继承 gateway 兜底
allow = ["crates.io", "pypi.org"]    # 仅 allowlist 生效的可达主机

[[permission.deny]]                  # 工具调用门控（deny 最高优先级）
tool = "shell"
contains = ["rm -rf", "sudo"]

[[permission.ask]]                   # 命中则需人工审批；无 contains = 对该工具任意调用
tool = "write"
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
| network.policy | ✗ | 沙箱网络策略 `isolated`/`allowlist`/`open`；缺省继承 gateway `default_network`（兜底 = `open`）。见 `doc/sandbox.md` §6.2 |
| network.allow | ✗ | `allowlist` 下的可达主机；其他策略忽略 |
| permission.deny | ✗ | 门控 deny 规则表（`{tool, contains}`）；命中即阻断。见 `doc/permission.md` |
| permission.ask | ✗ | 门控 ask 规则表；命中需人工审批。缺省 = 空策略 = 全 allow |

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
- 运行中切换 profile → 创建新 session（origin.kind = "reconfiguration"），自动带 context_snapshot。**惰性执行**：选取只是记下意图，下一次发送消息时才 reconfigure + 发送——误操作选取不产生任何会话。
- 运行中切换 **model / 推理强度** → **不**创建新 session：它们是每轮可改的运行参数，随下一次 `POST /sessions/{id}/message`（`model` / `think_effort`）生效，只影响那一轮。
- 用户命令：`/profile coding` → 切换并创建新 session。
- 同一 session 内 profile 不可变（历史不可变原则）；model/effort 是每轮参数，不属于这条。

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
| default_temperature | Provider (model) | model 推荐默认值 |
| think_efforts（可选档位） | Provider (model) | model 支持的推理强度集合 |
| think_effort 默认档 | Profile | agent 行为偏好（引用模型的档位），会话内可每轮覆盖 |
| temperature / max_output_tokens | Provider (model) | model 固有元数据，profile 不再覆盖 |
| compaction_threshold | Profile | 用户偏好 |
| injection_max_tokens | Profile | 用户偏好 |
| budget limit | Profile | 不同 agent 角色预算不同 |
| system prompt | Profile | agent 身份 |
| tool set | Profile | agent 能力 |
| skill set | Profile | agent 能力 |
| memory scope | Profile | agent 知识范围 |
| network policy | Profile（workspace 覆盖 / gateway 兜底） | agent 能力（能否联网、可达哪些主机）；解析链 `workspace.toml > profile > gateway default_network > Open`，见 `doc/workspace-config.md` |

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
