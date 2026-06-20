# Ominiforge Phase 2 实施计划（"可用"阶段）

本文记录 Phase 2 的拆分与实施顺序。Phase 2 的目标是让 agent 从"能跑通单轮"
变成"可用"：多轮对话、上下文超限自动压缩、可观测、可通过 MCP 扩展、有简洁 TUI。

拆分原则：**每一步都能独立编译、运行、验证**，不依赖尚未实现的后续步骤。

## 两个前置依赖（原 todo 清单未单列，但绕不开）

1. **多轮交互循环 + session resume**。当前 CLI 是退化的单轮（每次 `run` 新建
   `SessionRuntime` 后丢弃）。compaction（"创建新 session 并切换继续"）和 TUI
   都要求一个跨轮循环；resume 要求从 `events.jsonl` 重建 context view
   （`context-management.md §6`：冷启动从 events 重建）。是 Step 2/3/6 的地基。

2. **EventBus（tokio broadcast）**。`monitor.md §9` 规定 monitor 是 EventBus 的
   subscriber，纯 observe。当前 `SessionWriter` 直接写 jsonl，没有广播层。Step 4
   先补这一层（写 jsonl 的同时 publish），TUI 的实时事件流也来自它。

## 实施步骤

每步给出 范围 / 依赖 / 验证方式 / 产出模块。

### Step 1 — 多轮交互循环 + session resume
- 范围：非 TUI 的多轮 REPL（`ominiforge chat`，纯 stdio，复用现有 `CliSink`）；
  `--resume <id>` / `--continue` 从 `events.jsonl` 重建 `SessionRuntime`
  （context 的 `Vec<Message>` + 重放 plan ops）。`run` 保持单轮（脚本/管道用）。
- 依赖：无（建在 Phase 1 之上）。
- 验证：`chat` 中第一轮"记住数字 42"，第二轮"什么数字？"能答出（跨轮上下文）；
  退出后 `chat --resume <id>` 再问仍能答出（从磁盘重建）。
- 产出模块：`context`（context view 重建）、`cli`（chat 子命令）。

### Step 2 — Token 计数与上下文用量追踪
- 范围：权威运行计数 = provider 返回的 `usage.input_tokens`（见决策 A），
  缺失时回退本地启发式（chars/4），新增内容用本地增量补；计算 effective_limit
  并在跨越 threshold 时给出预警（**本步不做压缩，只追踪 + 预警**）。
  openai provider 开启 `stream_options.include_usage` 以拿到真实 usage。
- 依赖：Step 1。
- 验证：长对话中每轮打印已用 token / 上限；跨阈值时打印预警；对不返回 usage 的
  provider 自动走启发式（构造一个 mock 验证回退）。
- 产出模块：`context`（token 账本）、`provider/openai`（include_usage）。

### Step 3 — Compaction
- 范围：超限时调用模型生成摘要 → `SessionStore::create_compaction`（写
  `origin.kind=compaction` + `context_snapshot.json`）→ 自动切换到新 session 继续；
  `/compact` 手动命令（全量 + `--keep-last N`）。压缩模型见决策 B。
- 依赖：Step 1、Step 2。
- 验证：调低 threshold 触发自动压缩，确认新 session 目录生成、snapshot 内容正确、
  对话能在新 session 续上且记得压缩前的关键事实；原 session 完整保留。
- 产出模块：`context`（compaction）、`session`（create_compaction + snapshot 读写）。

### Step 4 — EventBus + Monitor
- 范围：在写 jsonl 的同时 publish 到 tokio broadcast；Monitor 作为 subscriber
  聚合 token/成本/缓存命中率/工具指标（`monitor.md`）。成本从 usage + pricing
  实时派生，不写回 event。提供离线 `inspect <session>`（读 jsonl 重算）+ 在线订阅
  两种消费路径，指标派生逻辑共用。
- 依赖：无（与 Step 5 可并行；建议在 Step 3 后做，以便观测压缩）。
- 验证：跑一轮后 `inspect` 打印 SessionSummary（turns/tokens/cost/cache_hit/tools），
  数字与手算 jsonl 一致。
- 产出模块：`monitor`、`session`（EventBus 接入）、`config`（pricing.toml）。

### Step 5 — MCP client
- 范围：MCP server 生命周期（spawn stdio 子进程）、JSON-RPC、把 MCP tool 适配到
  统一 `Tool` trait（`ToolSource::Mcp`）；`mcp.toml` 配置。
- 依赖：无（最独立）。
- 验证：用一个 mock stdio MCP server（或参考实现）配置后，`tools` 列表出现 MCP 工具，
  agent 能调用并拿到结果，ToolEvent 标记来源为 Mcp。
- 产出模块：`mcp`、`tool`（注册 MCP 工具）、`config`（mcp.toml）。

### Step 6 — TUI
- 范围：`ominiforge` 裸命令进入全屏 TUI（`ratatui`），订阅 EventBus 实时渲染
  对话/思考/工具/token 用量；多轮输入。**移植 Step 1 的 resume 能力**
  （`rebuild_runtime` + `SessionStore::open`/`list`，均 UI 无关库函数），含交互式
  session 选择器（CLI 只能打印列表，选择器本就归 TUI）。完成后移除临时的 `chat` 子命令。
- 依赖：Step 1、Step 4（事件流）。
- 验证：启动 TUI 完成一次多轮对话，实时看到流式输出、工具调用、token 用量；
  从列表选一个旧 session 恢复续聊；`Ctrl-C` 干净退出。
- 产出模块：`tui`、`cli`（裸命令路由；移除 `chat`）。

## 关键决策

### A. Token 计数策略
- **权威来源**：provider 在 `RequestCompleted` 返回的 `usage.input_tokens`，
  代表"本轮实际发出的 prefix 的真实 token 数"。
- **threshold 判断**：`effective_limit = auto_compact_threshold × context_window
  − max_output_tokens`（`context-management.md §4.2`，默认 0.8）。运行计数 ≥
  effective_limit 时触发压缩。检查放在**轮/回合边界**（发下一个请求前）；单轮内
  极端膨胀由 max_rounds + provider 报错兜底（已知限制，后续处理）。
- **provider 不返回 usage 是真实存在的**：OpenAI streaming 默认不带 usage，需
  `stream_options.include_usage=true`；OpenAI 兼容端（Xiaomi MiMo / 本地
  llama.cpp 等）不保证支持，可能仍为空（现有 wire 的 `null_default` 已在处理 null）。
  因此**不能假设一定有**。处理：始终维护本地启发式估算（chars/4，provider 无关、
  零依赖）作为基线；真实 usage 到达时用它校准基线（覆盖"已发送 prefix"那段），
  新追加内容用本地增量顶上，直到下一次 RequestCompleted 再校准。

### B. Compaction 使用的模型
默认用 session 当前模型（最简，无需额外配置）；profile 可在 `[context]` 指定
`compaction_model`（model 引用）覆盖，便于用便宜模型做摘要。

### C. 实施顺序
按上面 Step 1→6。主线 1→3 让 agent "可用"，4/5 增强（可并行），6 收口成界面。

### D. 配置格式与可发现性（TOML + JSON Schema）—— 待确认是否纳入 Phase 2
- **保留 TOML**：人工编辑友好（注释、可读），与 Rust 生态一致；优于 JSON（无注释）/
  YAML（空白敏感易错）。
- **可发现性**：TOML 无独立 schema 标准，但 **Taplo**（事实标准 TOML LSP）支持用
  **JSON Schema 校验 TOML**——在 TOML 顶部加 `#:schema <url/path>` 指令，编辑器即可
  自动补全 + 校验 + 悬停文档（Cargo.toml 的补全即如此）。
- **方案**：用 `schemars` 从 Rust 配置类型自动生成 JSON Schema（与代码同步），随仓库
  发布；`init` 模板顶部写入 `#:schema`；提供 `ominiforge config schema` 导出、
  `config validate` 校验。与功能解耦，建议作为可选小任务顺手做，不单独占用主线。

## 状态

- [x] Step 1 — 多轮循环 + resume ✅（2026-06-19）
- [x] Step 2 — token 计数与用量追踪 ✅（2026-06-20）
- [x] Step 3 — compaction ✅（2026-06-20）
- [ ] Step 4 — EventBus + monitor
- [ ] Step 5 — MCP client
- [ ] Step 6 — TUI

### Step 1 完成记录（2026-06-19）

实现（3 个子步）：
- **1a** `src/agent/resume.rs` — 纯函数 `rebuild_runtime(events, system) -> SessionRuntime`：
  把 events 逆向还原成 `Vec<Message>` + 重放 plan ops。映射规则与 agent loop 写入完全对称
  （TurnEvent::Started{input}→User；同 request_id 的 ContentBlock 聚合成 Assistant，
  Reasoning 跳过；ToolEvent→Tool，tool_call_id 经 seq→call_id 反查；Injection→User）；
  plan 重放对坏 op 静默跳过（与 live dispatch 容错一致）。7 单测。
- **1b** `SessionStore::open`（重开已存在 session，next_seq 定位末尾续写，确认存在+加锁）
  + `latest`（按 ULID 序取最新 session，给 --continue）。4 单测。
- **1c** `ominiforge chat` 子命令（stdio REPL，复用 CliSink）+ `--resume <id>` / `--continue`；
  抽出 `prepare()` 共享 run/chat 的 config→agent 装配，`report_turn()` 共享回合页脚。

验证：91 测试通过（80→+11），clippy pedantic+nursery 干净。Live（mimo/mimo-v2.5-pro）：
单进程内 2 轮记住 42；**独立新进程** `--resume` 从 13 个磁盘 events 重建并答出 42；
`--continue` 取最新 session 续上；events.jsonl seq 连续 0..18 无空洞/重复。

#### Step 1 完善（2026-06-19，按用户反馈）

去掉 `--continue`（"猜最新"策略脆：空 session 会盖住真正想续的）。`--resume` 改三态：
- `chat`（无 flag）→ 新建 session；
- `chat --resume <id>` → 恢复指定 session；
- `chat --resume`（无 id）→ `list_sessions` 打印本 workspace 全部 session（最新在前，
  含 created_at + turn 数）后退出。交互式选择器归 TUI，CLI 只打印 id 让用户 `--resume <id>`。

store 层 `latest()` 换成 `list()`（返回全部，ULID 序最新在前）。clap `--resume` 用
`num_args=0..=1` + `default_missing_value=""`（避开 clippy `option_option`）。
91 测试通过，clippy 干净，三态 live 验过。

**注意 chat 是临时形态**：Step 6 TUI 完成后，`chat` 子命令移除，resume 能力
（`rebuild_runtime` + `SessionStore::open`/`list`）移植给 TUI。这些是 UI 无关的库函数，
迁移只是换调用方。

### Step 2 完成记录（2026-06-20）

实现：`src/context.rs` 落地 `ContextLedger`（measured + pending_bytes 两段模型）。
- **measured** = 上一次请求返回的权威 `usage.input_tokens`（发出 prefix 的精确 token）；
  **pending_bytes** = 自那次请求后追加内容的原始字节，用 `bytes/4` 启发式折算。
  `running() = measured + pending_bytes/4`。
- `calibrate(input_tokens)`：非 0 时 measured 换成它并清零 tail（响应/工具结果还没追加，
  正好对齐"已发 prefix"）；为 0（provider 不返回 usage）时保持 measured、tail 继续涨——
  纯启发式兜底（决策 A）。
- `effective_limit = threshold × context_window − max_output_tokens`，window 为 0（未知）
  返回 `None`，调用方跳过阈值逻辑（不把一切当超限）；预留空间不足时 floor 到 0 不回绕。
- `estimate_tokens` 收编 `inject_runtime` 原先内联的 `len()/4`，注入记账与账本同源。

接线：
- `SessionRuntime` 加 `ledger` 字段 + 私有 `push_message`（追加 context 必经此，保账本同步）；
  `new` 用 `ContextLedger::seeded` 从初始 context 预热；`run`/`drive`/`inject_runtime` 四处
  `context.push` 全改道 `push_message`。
- agent loop 在 `run_model_round` 内：发请求前 `running()` 填进 `RequestStarted.
  input_tokens_estimate`（原先硬编码 0）；`collect_round` 拿到 usage 后、追加响应前
  `calibrate(usage.input_tokens)`。
- `AgentConfig` 加 `context_window` + `compaction_threshold`（default 0.8）；`context_limit()`
  代理到 `effective_limit`。`TurnOutcome` 加 `context_tokens` + `context_limit`。
- CLI `prepare` 从 `resolved.context_window` + `profile.context.compaction_threshold` 注入；
  `report_turn` 打印 `[context: ~used / limit tokens (pct%)]`，越过阈值再打一行 warning
  （Step 2 只预警，压缩留给 Step 3）。
- `rebuild_runtime` 走 `SessionRuntime::new` 给账本播种；resume 不带权威计数，首个请求重新校准。
- wire.rs `stream_options.include_usage` 早已为 `true`（Step 1 既存），无需改动。

验证：97 测试通过（91→+6：context.rs 4 单测 + agent 2 集成）。集成测试覆盖两条路：
provider 返回 5000 → 账本贴齐权威值、outcome 报 6000 上限且判定未超；provider 不返回 usage
（`Usage::default()`）→ 全程纯启发式，`running == 总字节/4`。clippy pedantic+nursery 干净。
Live（mimo/mimo-v2.5-pro）：单轮 `1068in` 真实 usage，context `~1071 / 672000`
（0.8×1M−128k），校准正确；低阈值 throwaway profile（threshold=0.05 → limit floor 到 0）
触发 warning 行验过，事后清理 profile + 两个 live session。

### Step 3 完成记录（2026-06-20）

实现 compaction：超限时调模型生成摘要 → 写 `origin.kind=compaction` 新 session + snapshot
→ 自动切换续聊；手动 `/compact`（含 `--keep-last N`）。

`src/session/`：
- `meta.rs` 加 `Origin::compaction(parent_id)`（`kind=compaction` + `parent_id`，无 `fork_at_seq`）。
- `mod.rs` 加 `create_compaction`（mint id → 写 meta → `serde_json` 落 `context_snapshot.json`
  → 开 log → 首条 `Session::Created`，与 `create_new` 同结构，多写 snapshot）+ `read_snapshot`
  （读回 messages 数组，给将来 resume compaction session 用）。`SNAPSHOT_FILE` 常量。

`src/agent/mod.rs`：
- `Agent::compact(runtime, keep_last) -> Option<Vec<Message>>`：`split_for_compaction` 把
  context 切三段（leading System / 中间待摘要 / 末尾保留 `keep_last` 个 user 轮）；中间为空返
  `None`（无可压缩）；否则把 system+待摘要+摘要指令发模型，收集 `TextDelta` 成 summary，
  拼回 `system + <conversation_summary> + tail` 作 snapshot。压缩模型 = session 当前模型（决策 B），
  temperature 0.3、max_tokens 1000、不带 tools。
- `split_for_compaction` 纯函数：System 前缀稳定；tail 从倒数第 `keep_last` 个 User 起；
  `keep_last=None/0` 全压；user 轮不足 keep 数则全保留。

`src/cli.rs`：
- `do_compact`（生成 snapshot → `create_compaction` → 新 `SessionRuntime::new(snapshot)`）+
  `swap_to_compaction`（原地换 writer/runtime，失败非致命只打日志）。
- chat loop：`/compact` 命令手动触发；每轮 `report_turn` 后 `context_tokens >= limit` 自动触发。
- 重构：抽 `open_or_create_session`（+ `ChatSession` enum）压缩 `chat` 行数；`report_turn`
  越限 warning 文案改中性（不再说"later step"，因 chat 已自动压缩，run 单轮无 loop）。

验证：103 测试通过（97→+6：session 1 + agent 5 覆盖 split 三段/keep-last/空摘要/compact
端到端/snapshot 格式）。clippy pedantic+nursery 干净。Live（mimo/mimo-v2.5-pro，低阈值
throwaway profile）：记住 42 → 自动压缩切到 compaction session → 新进程般续聊仍答出 42；
on-disk 验证 compaction session `kind=compaction`+`parent_id`、snapshot = system+summary、
首条 `Session::Created`、原 session 完整保留（`kind=new`/25 events）；事后清理 profile + sessions。
