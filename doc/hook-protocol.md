# Ominiforge Hook Protocol

本文档定义 hook 的触发、执行和响应协议。Hook 用于在特定 pipeline 位置拦截或观察事件。

## 1. 设计原则

- Hook point 为固定预定义集合，不允许订阅任意 event。
- Before hook 同步执行，可 pass/modify/block。After hook 异步执行，仅 observe。
- Hook 实现为 host 侧 Rust trait（内置 hook）或 shell command（用户 hook）。
- 纯事件观察需求（全量 event 订阅）由 host 侧 EventBus 满足（内置 monitor/replay 等），不属于 hook 系统。
- 所有 hook 执行写入 event log，支持监控和审计。

## 2. Hook Point 列表

初始预定义 hook point：

```text
session:start
session:end
turn:start
turn:end
model:request:before
model:request:after
tool:invoke:before
tool:invoke:after
artifact:create:before
artifact:create:after
```

新增 hook point 需要 ominiforge 发版。Hook point 只在有"pipeline 可暂停"语义的位置设置。

## 3. Hook Timing

| Timing | 执行方式 | 能力 | 超时 |
|--------|----------|------|------|
| before | 同步，pipeline 等待 | pass / modify / block | 严格，默认 5s |
| after | 异步，不阻塞 pipeline | observe only | 宽松，默认 30s |

Before hook point：名称含 `:before` 或位于 pipeline 起始位置（`session:start`、`turn:start`）。
After hook point：名称含 `:after` 或位于 pipeline 终止位置（`session:end`、`turn:end`）。

## 4. Hook 分类

```text
Hook
├── Built-in hook（Rust trait impl，编译进 ominiforge）
│   ├── permission-guard    # 权限检查
│   ├── cost-limiter        # 成本控制
│   └── ...
└── User hook（shell command / 可执行文件）
    ├── 用户自定义脚本
    └── 社区共享 hook 脚本
```

## 5. Built-in Hook

```rust
#[async_trait]
pub trait BeforeHook: Send + Sync {
    fn name(&self) -> &str;
    fn hook_point(&self) -> HookPoint;
    fn priority(&self) -> u32 { 100 }
    fn failure_mode(&self) -> FailureMode { FailureMode::Open }

    async fn intercept(&self, req: &HookRequest) -> HookAction;
}

#[async_trait]
pub trait AfterHook: Send + Sync {
    fn name(&self) -> &str;
    fn hook_point(&self) -> HookPoint;
    fn priority(&self) -> u32 { 100 }

    async fn notify(&self, req: &HookRequest);
}

pub enum HookAction {
    Pass,
    Modify(serde_json::Value),
    Block { reason: String },
}
```

Built-in hook 直接注册到 HookRegistry，零 IPC 开销。

## 6. User Hook（Shell Hook）

### 6.1 配置

```toml
# .omini/config/hooks.toml

[[hooks]]
name = "lint-before-write"
hook_point = "tool:invoke:before"
timing = "before"
match_tool = "write"           # 可选：只对特定 tool 生效
command = "python3 ~/.omini/hooks/lint-check.py"
priority = 50
failure_mode = "open"
timeout_ms = 5000

[[hooks]]
name = "notify-on-complete"
hook_point = "turn:end"
timing = "after"
command = "~/.omini/hooks/notify.sh"
priority = 100
timeout_ms = 10000
```

### 6.2 执行协议

Shell hook 通过 stdin/stdout 通信：

**Before hook：**
```text
Host → stdin (JSON):
{
  "hook_point": "tool:invoke:before",
  "payload": { "tool_name": "write", "input": {...} },
  "config": { ... }
}

Hook → stdout (JSON):
{ "action": "pass" }
{ "action": "modify", "payload": {...} }
{ "action": "block", "reason": "..." }
```

**After hook：**
```text
Host → stdin (JSON):
{
  "hook_point": "tool:invoke:after",
  "payload": { "tool_name": "write", "result": {...} }
}

Hook → (无需 stdout 输出)
```

### 6.3 错误处理

- Hook 进程非零退出 → 按 failure_mode 处理
- Hook 超时 → kill 进程，按 failure_mode 处理
- Hook stdout 非法 JSON → 按 failure_mode 处理

## 7. 执行顺序

同一 hook point 有多个 hook 时：

1. 按 priority 升序排列（数字小的先执行）
2. 同 priority 按注册顺序执行
3. Before hook 链式执行：前一个 hook 的 modify 结果传给下一个
4. 任一 before hook 返回 block，后续 hook 不执行

Built-in hook 和 user hook 混合排序，统一按 priority。

## 8. Block 事件记录

Hook block 时，runtime 生成对应的 Failed event。以 tool:invoke:before 为例：

```rust
ToolEvent::Failed {
    tool_call_event_id: EventId,
    error: ErrorDetail {
        code: "blocked_by_hook",
        message: "Dangerous command pattern detected: rm -rf",
        severity: Error,
        retryable: false,
    },
}
```

传给 model 的 tool result：

```text
Blocked by hook [security-guard]: Dangerous command pattern detected: rm -rf
```

Model 可据此调整行为。

## 9. Failure Mode

- `open`：hook 失败时 pipeline 继续执行。适用于日志、metrics 类 hook。
- `closed`：hook 失败时 pipeline 阻断，生成 error event。适用于安全、权限类 hook。

用户可在配置中覆盖 failure_mode。

## 10. 与 Host EventBus 的区别

| | Hook | Host EventBus |
|--|------|---------------|
| 触发源 | 固定 hook point | 任意 EventPayload variant |
| 能力 | before 可 modify/block | 只能 observe |
| 执行时机 | pipeline 内 | pipeline 外，异步 |
| 运行形态 | Rust trait / shell command | host 侧 Rust trait impl |
| 使用者 | 内置逻辑 + 用户扩展 | 内置 monitor、replay、evolution |
| 适用场景 | 安全、权限、输入改写、通知 | 全量事件流消费（成本追踪、trace、回放） |

Host EventBus 是 runtime 内部机制（tokio broadcast channel），用于 monitor/replay/evolution 等内置系统。

## 11. Hook 文件系统布局

```text
.omini/
├── config/
│   └── hooks.toml        # user hook 配置
└── hooks/                # user hook 脚本目录
    ├── lint-check.py
    ├── notify.sh
    └── security-scan.js
```

## 12. 与旧方案对比

| | 旧方案（已废弃） | 新方案 |
|--|---|---|
| 运行时 | WASM Component + wasmtime | Rust trait + shell command |
| 通信 | WIT 类型直传 | 内存直调 / stdin JSON |
| 开发门槛 | Rust + wasm32-wasip3 | Rust（内置） / 任意语言（shell） |
| 沙箱 | WASM 隔离 | 无（信任用户 hook） |
| 能力 | 受 WASI 限制 | 完整 OS 能力 |

## 13. 待后续完善

- 各 hook point 的 payload 详细 schema。
- Hook 热更新：是否支持运行中替换 hook。
- Hook 与 profile 的关系：是否可以绑定到特定 profile。
- Hook 执行的 monitor metrics。
- match 条件扩展（按 tool name、input pattern 等过滤触发）。
