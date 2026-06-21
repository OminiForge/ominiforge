# Phase 2 实施日志（归档）

本文归档 Phase 2 各步的逐步完成记录，作为实施叙事留痕。设计依据（实施步骤 +
关键决策）见 [`../phase2-plan.md`](../phase2-plan.md)；实现细节亦可查 git 历史。

## 状态

- [x] Step 1 — 多轮循环 + resume ✅（2026-06-19）
- [x] Step 2 — token 计数与用量追踪 ✅（2026-06-20）
- [x] Step 3 — compaction ✅（2026-06-20）
- [x] Step 4 — EventBus + monitor ✅（2026-06-20）
- [x] Step 5 — MCP client ✅（2026-06-20）
- [x] Step 6 — TUI ✅（2026-06-20）

## Step 1 完成记录（2026-06-19）

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

### Step 1 完善（2026-06-19，按用户反馈）

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

## Step 2 完成记录（2026-06-20）

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

## Step 3 完成记录（2026-06-20）

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

## Step 4 完成记录（2026-06-20）

实现 EventBus（tokio broadcast）+ Monitor（事件流派生指标），含离线 `inspect <session>`。

`src/session/bus.rs`：`EventBus`（`broadcast::Sender<CoreEvent>` 封装，clone 共享通道）。
`publish` best-effort（无 subscriber 不报错，丢弃即可）；`subscribe` 给 receiver；容量 1024，
落后 subscriber 收 `Lagged` 后从 log 重同步。事件先落 jsonl 再 publish（log 仍是真相，广播只为
liveness）。`SessionWriter` 加 `bus: Option<EventBus>` + `with_bus` builder，`append` 写完发布。

`src/monitor.rs`：`Monitor`（纯 fold）+ `SessionSummary` + `summarize(events, pricing)` 离线入口。
- 聚合 turns / model requests / tool calls(+failures) / input+output+cache_read tokens /
  cache_hit_rate（read/input）/ tools_used / errors（按 code 计数）。
- 成本：`RequestStarted` 记 `request_id→model`，`RequestCompleted` 按该 model 的 pricing 折算
  累加；无任何 priced model → `cost_usd=None`（不报误导性 $0.00）。`cost_of` 用 input/output/
  cache_read(缺省回退 input rate)/cache_write(缺省 0) 四项 per-million 求和。
- 决策符合 `monitor.md`：成本不写回 event，读时用最新 pricing 重算。

`src/config/mod.rs`：`load_pricing(&providers) -> HashMap<String,Pricing>`：先铺 `providers.toml`
内联 pricing 作基线，再用 `.omini/config/pricing.toml`（`[models."<id>"]` 表）覆盖；缺文件不报错。
`PricingFile` 私有结构 + `PRICING_FILE` 常量。

`src/cli.rs`：`inspect <session_id>` 子命令——读 jsonl → `summarize` → `print_summary`
（turns/requests/tools/tokens/cache/cost/tools_used/errors，tools 与 errors 按计数降序）。
pricing best-effort（加载失败则 unpriced）。

验证：112 测试通过（106→+6：bus 2 + monitor 4，另 config 2 pricing 合并/缺省）。clippy
pedantic+nursery 干净。Live（mimo）：单轮 `run` 后 `inspect` 报 1 turn / 1 request /
1075in/7out / cost $0.0033，与 turn footer 完全一致；手算 1075×$3/M + 7×$6/M = $0.003267
→ $0.0033 ✓（用 providers.toml 内联 pricing，无 pricing.toml 走回退层）；事后清理 session。

注：EventBus 在线订阅路径已接线 + 单测（`with_bus` + broadcast 投递），实际在线消费者
（TUI 实时渲染）在 Step 6 接入；本步可验证产出是离线 `inspect`。

## Step 5 完成记录（2026-06-20）

实现 MCP client：stdio 子进程生命周期 + JSON-RPC 2.0 framing + 适配到统一 `Tool` trait。

`src/mcp/config.rs`：`McpConfig`/`McpServerConfig`（`.omini/config/mcp.toml`）。`load(roots)`
按 root 优先级合并，同名 server 高优先 root 遮蔽低优先；缺文件不报错。`is_stdio`（有 `command`
即 stdio；`url` 字段解析但 SSE 暂不支持）。

`src/mcp/protocol.rs`：JSON-RPC wire 类型（`Request`/`Notification`/`Response`/`RpcError`）+ MCP
子集（`ToolDef`/`ToolsListResult`/`ToolCallResult`/`ContentBlock`）。protocol version `2025-11-25`
（最新 spec；server 在握手回包里回声其自身支持的版本，我们发最新并容忍更旧的回复——只用
`tools/list` + `tools/call` 这层跨版本稳定的接口）。
`Response.id` optional，使日志行/通知行能解码不报错（client 跳过非匹配行）。

`src/mcp/client.rs`：`McpClient` 持有子进程（`kill_on_drop`）+ mutex 串行化 stdio（单条有序
字节流，一次只允许一个 request/response 在途）。`connect` = spawn → `initialize` 握手 →
`notifications/initialized` → `tools/list`。`request` 用单调 id，读行直到 id 匹配（跳过通知/异 id）。
`McpTool` 适配器实现 `Tool`：`source()` 返回 `ToolSource::Mcp{server_name}`；`invoke` 用
`input.timeout` 包住 round-trip（挂起=协议 Timeout，server 关闭=ServerCrashed，均非业务错误）；
`isError:true` → 业务级 `ToolOutput{is_error,error_code="mcp_tool_error"}`（`tool-protocol.md` §7.1）。

`src/mcp/mod.rs`：`connect_all(config, &mut registry, on_warn)`——逐 server 连接并注册其 tool 到
统一 registry；单个 server 失败仅 warn + skip，不中断 agent（§12）。返回 live clients 供 caller
持有（drop client 即杀子进程）。

`src/cli.rs`：`prepare` 改 `async`（spawn 子进程需 await），加载 `mcp.toml` → `connect_all` 把
MCP tool 与 built-in 并排注册；`Prepared._mcp_clients` 持有 client 保活整个 session。
`src/agent/mod.rs`：ToolEvent 的 source 由 `registry.source_of(name)` 决定（不再硬编码 Builtin）。
`src/tool/mod.rs`：`Tool::source()` 默认方法（默认 Builtin，MCP 适配器覆盖）+ `ToolRegistry::source_of`。

验证：120 测试通过（112→+8：config 3 [doc 示例解析/同名遮蔽/缺省空] + client 3 [connect+list 标
MCP source / invoke 经 stdio 回环 / isError→业务错误] + mod 2 [connect_all 端到端注册 + 坏 server
跳过不致命]）。用 Python mock stdio server 跑真实 JSON-RPC 握手+调用回环，不依赖外部 binary。
clippy pedantic+nursery 干净（request 持锁跨写读为有意，带 `allow(significant_drop_tightening)`
+ 注释说明）。

注：在线 agent 调用 MCP 工具的实际 live 验证（mimo 模型实跑）可在 Step 6 TUI 或单独 e2e 中做；
本步用 mock server 覆盖了协议层全回环 + 统一 Tool trait 适配 + source 归属。

## Step 6 完成记录（2026-06-20）

实现 TUI（`ratatui` 0.29 + `crossterm` 0.28）：裸命令 `ominiforge` 进全屏交互界面，订阅
EventBus 实时渲染对话/思考/工具/token 用量；多轮输入；移植 Step 1 的 resume（交互式 session
选择器）；移除临时 `chat` 子命令。

`src/tui.rs`（全新实现，渲染循环永不阻塞模型）：
- **并发模型**：一个 turn 在 `tokio::spawn` 后台任务里跑（`Agent` 包 `Arc`，`writer`/`runtime`
  move 进任务、连同 `TurnOutcome` 经 `oneshot` move 回）；渲染循环每 50ms `draw` + 轮询键盘 +
  排空两条 channel，所以输出边产生边显示（满足"实时看到流式输出"）。turn 运行时锁键盘输入
  （仅 Ctrl-C 例外）。`run`/`run_app` 保持 `async`（`spawn` 需运行时上下文，带 `allow`）。
- **两路事件源**：①`ChannelSink`（实现 `StreamSink`，unbounded send 非阻塞）把 token 级 delta
  （text/reasoning/tool-args + block_start）推给 UI——这是真·流式来源，因 EventBus 只在 block
  收尾投递整块；②`EventBus`（Step 4 为在线消费建的，本步首个在线消费者）投递 ToolEvent
  Completed/Failed——sink 看不到工具*结果*。两路都 fold 进 `AppState.lines`，`Open` 枚举追踪
  当前 channel 决定 delta 追加到末行还是开新行（镜像 CLI sink 的 channel 跟踪）。
- **session 选择器**：`select_or_create_session` 全屏列出本 workspace sessions（每行 id + turn
  数）+ `[ New session ]` 行，↑↓ 移动、Enter 选中、q/Esc 取消；选已存在的走 `read_events` +
  `open` + `rebuild_runtime`（Step 1 的 UI 无关库函数，迁移只换调用方）。无 session 时直接建新。
- **auto-compaction**：turn 收尾若 `context_tokens >= context_limit`，调 `do_compact`（= 旧 CLI
  `do_compact` 同结构：`Agent::compact` → `create_compaction` → 新 writer 接 bus + 新 runtime）
  原地换 session，header 更新 id。失败非致命（打 note 保留原 session）。补回了移除 `chat` 丢掉
  的能力。
- **渲染**：上对话区（按行数算 scroll 让末尾可见）+ 下输入区（busy 时 title 显示 "working…"）。

`src/cli.rs`：`Command` 改 `Option`（裸命令 → `tui_main`）；删 `chat`/`ChatArgs`/`ChatSession`/
`open_or_create_session`/`list_sessions`/`swap_to_compaction`/`do_compact`（resume + compaction
能力迁入 TUI）。`run`/`inspect`/`init` 不变。`report_turn` 文案改为单轮 `run` 专用。
`Cargo.toml`：加 `ratatui = "0.29"` + `crossterm = "0.28"`。

验证：120 测试通过（无回归；TUI 是 IO/终端层，逻辑复用既测过的 agent/session/context）。clippy
pedantic+nursery 干净。Live（mimo/mimo-v2.5-pro，PTY harness 设 40×120 winsize 驱动真终端）：
①新建 session → 输入"run echo STEP6_OK"→ 实时看到 `[thinking]`/`[tool: shell]`/工具结果
`STEP6_OK`/流式答案/footer（rounds + `ctx ~`），Ctrl-C 干净退出；on-disk 验 session `kind=new`、
2 model round + shell tool + `Turn Completed`、seq 0..12 连续。②**独立新进程** resume 该 session
（选择器 Enter 选首行）问"echo 输出过什么"→ 答出 `STEP6_OK`（证 `rebuild_runtime` 从磁盘重建
上下文），新 turn 追加后 seq 0..18 连续无空洞。事后清理两个 throwaway session。

### Step 6 UX 修订（2026-06-20，按用户反馈）

首版三个交互问题，已修：
- **裸命令应直接进新 session，而非弹选择器**。`Cli` 加全局 `--resume` flag；裸 `ominiforge` →
  新 session 直接进对话，`ominiforge --resume` → 才弹选择器。`tui::run` 加 `resume: bool` 参数。
- **resume 后看不到历史**。原先只 `rebuild_runtime` 重建了 `runtime.context` 但从不渲染。新增
  `AppState::seed_history`：把重建的 `Vec<Message>` 铺进对话区（System 跳过=身份非对话；User→
  `> ...`；Assistant 文本/tool_calls；Tool 结果缩进 `↳`），末尾加 `── resumed; continue below ──`
  分隔线。选择器行也从裸 id 改为 `时间 · N turn(s) · 首条 prompt 预览`（`session_rows` +
  `first_line` 截断）。空 session 显示 `(no messages yet)`。
- **选择器按 q/Esc 崩溃**。原 `anyhow::bail!("cancelled")` 当错误冒泡（带 backtrace）。改：
  `select_session` 返回 `Result<Option<SessionId>>`，q/Esc/无 session → `Ok(None)` → 干净退出。

重构：`select_or_create_session` 拆为 `create_session` / `open_session`（后者额外返回重建的
history 供渲染）/ `select_session`（纯选择器）+ `Chosen` enum；`AppState::new` 改收 `&str`
provider/model（不再依赖 `ResolvedModel`，可单测）。

验证：123 测试通过（+3：`seed_history` 渲染历史/跳过 System、空历史 no-op、`first_line` 截断）。
clippy pedantic+nursery 干净。Live（mimo，PTY 40×120）：①裸命令直接进对话（无选择器）；
②`--resume` 选择器显示富信息行，Esc 退出码 0 无 panic/backtrace；③裸命令记住 codeword BANANA77
→ `--resume` 选该 session → 历史区渲染出 BANANA77 + 分隔线。事后清理 3 个 throwaway session。
</content>
</invoke>
