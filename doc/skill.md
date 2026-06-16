# Skill 系统

## 1. 设计原则

- Skill 是可复用的任务模板，包含 prompt instructions + 动态内容。
- Skill 加载由 model 自主决定（渐进式披露），不靠关键词匹配。
- `load_skill` 是 built-in tool call，执行动态命令后返回完整内容。
- 动态命令全部执行、全部收集错误，不 fail-fast。
- Skill 人类可读可编辑（Markdown + frontmatter）。

## 2. 加载机制

### 2.1 渐进式披露

```text
System prompt
  → 包含 skill 索引（name + description 列表）
  → Model 根据当前任务判断是否需要加载某 skill
  → 调用 load_skill tool
  → 获得完整 instructions（动态内容已替换）
  → 按 instructions 执行
```

Model 自主决定何时需要 skill，不靠外部触发。

### 2.2 Skill 索引注入

System prompt 中 skill 部分示例：

```text
## Available Skills

- git-commit: Generate conventional commit message from staged changes
- code-review: Review code changes for bugs and style issues
- refactor: Refactor code with safety checks and tests

Use load_skill when your task matches a known skill.
```

索引只包含 name + description，不包含 instructions（节省 context）。

### 2.3 load_skill Tool

```rust
pub struct LoadSkillTool;

// input: { "name": "git-commit" }
// output: 完整 skill content（动态内容已替换）
// error: 模板执行失败时返回所有错误（不 fail-fast）
```

执行流程：
1. 根据 name 定位 `.omini/skills/{name}.md`
2. 解析 frontmatter + body
3. 扫描所有模板变量
4. **全部执行**，收集所有结果（成功和失败）
5. 替换成功的变量
6. 如有失败：返回替换后的内容 + 附带所有失败信息（不中断）
7. 记入 ToolEvent

## 3. Skill 文件格式

```markdown
---
name: "git-commit"
version: "0.1.0"
description: "Generate conventional commit message from staged changes"
tools_used: ["shell"]
created_by: "user"
created_at: "2026-06-15T10:00:00Z"
---

## Context

Current directory: {{exec "pwd"}}
Current branch: {{exec "git branch --show-current"}}
Staged files: {{exec "git diff --cached --name-only"}}
Current time: {{now}}

## Instructions

Based on the staged changes above:
1. Analyze what changed and why.
2. Generate a Conventional Commits message.
3. Subject ≤50 chars, body only when why isn't obvious.
4. Ask user to confirm before committing.

## Examples

User: "commit this"
Steps: read staged diff → generate message → confirm → git commit
```

## 4. 模板语法

| 语法 | 说明 | 示例 |
|------|------|------|
| `{{exec "cmd"}}` | 执行 shell 命令，替换为 stdout | `{{exec "git branch --show-current"}}` |
| `{{now}}` | 当前时间（ISO 8601） | `2026-06-15T10:30:00Z` |
| `{{workspace}}` | 当前 workspace 路径 | `/home/user/project` |
| `{{env "VAR"}}` | 环境变量值 | `{{env "USER"}}` → `duskgrow` |
| `{{profile}}` | 当前 profile name | `coding` |
| `{{session_id}}` | 当前 session ID | `01JXYZ...` |

### 4.1 exec 错误处理

所有模板变量全部执行，不 fail-fast：

```text
模板执行结果：
  {{exec "pwd"}}              → ✓ "/home/user/project"
  {{exec "git branch ..."}}   → ✓ "main"
  {{exec "invalid-cmd"}}      → ✗ exit_code=127, stderr="command not found"
  {{exec "timeout-cmd"}}      → ✗ timeout after 5s

返回给 model：
  - 替换后的 content（失败的变量保留原始 `{{exec ...}}` 或标记为 [FAILED]）
  - 附带错误摘要：
    "2 template executions failed:
     - `invalid-cmd`: command not found (exit 127)
     - `timeout-cmd`: timeout after 5000ms"
```

Model 收到错误信息后可以：
- 忽略非关键信息继续执行
- 告知用户某些上下文获取失败
- 尝试用其他方式获取信息

## 5. 监控

### 5.1 Skill Metrics

```rust
pub struct SkillMetrics {
    pub name: String,
    pub total_loads: u64,
    pub load_success: u64,          // 所有模板执行成功
    pub load_partial: u64,          // 部分模板执行失败
    pub load_failure: u64,          // skill 文件不存在或解析错误
    pub task_completed: u64,        // load 后 turn 正常完成
    pub task_failed: u64,           // load 后 turn 失败
    pub last_used: DateTime,
}
```

### 5.2 Load Failure 记录

```rust
pub struct LoadFailureRecord {
    pub timestamp: DateTime,
    pub session_id: String,
    pub command: String,
    pub error: String,
    pub exit_code: Option<i32>,
}
```

写入 ToolEvent，供 evolution worker 分析。

## 6. 生命周期

```text
created → active → (needs_review | stale | broken) → updated | disabled
```

### 6.1 状态判定

| 条件 | 状态 |
|------|------|
| `load_partial / total_loads > 0.3` | needs_review（模板命令不稳定） |
| `task_failed / task_completed > 0.3` | needs_review（instructions 效果差） |
| `last_used` > 30 天 | stale |
| 引用的 tool 被移除 | broken |

### 6.2 Evolution 处理

Evolution worker 定期扫描 metrics，生成提案：
- 修复失败的模板命令
- 改进 instructions
- 标记废弃 skill
- 基于 session 历史提出新 skill 草案

## 7. Skill 审批流程

### 7.1 来源

| 来源 | 审批 |
|------|------|
| 用户手动创建 | 不需要，直接可用 |
| Evolution 提议 | 需要 review |
| 社区共享（未来） | 用户自行决定安装 |

### 7.2 Evolution 提议流程

```text
Evolution 生成 skill 草案
  → /evolution review
  → 用户选择：
    - approve → 移入 .omini/skills/，状态 active
    - reject → 丢弃
    - revise "修改意见" → evolution 修改 → 再次 review → 循环
```

用户可以多轮 revise 直到满意或 reject。

## 8. 显式调用

除 model 自主加载外，用户也可显式调用：

```text
/skill git-commit      → 直接触发 load_skill，注入 context
/skill list            → 列出所有可用 skill
/skill edit git-commit → 打开编辑
/skill disable old-one → 移入 _disabled/
```

## 9. 文件系统布局

```text
.omini/
└── skills/
    ├── git-commit.md
    ├── code-review.md
    ├── refactor.md
    └── _disabled/
        └── old-deploy.md
```

## 10. 待后续完善

- Skill 间组合（一个 skill 引用另一个 skill）。
- Skill 参数化（调用时传参，如 `/skill deploy --env production`）。
- Skill 版本历史（git 管理或内置版本）。
- Skill 与 profile 绑定（某些 skill 只在特定 profile 可用）。
