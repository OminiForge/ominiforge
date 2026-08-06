# Eval 系统设计

代码入口：[`src/eval/`](../crates/ominiforge-core/src/eval/)。本文讲设计意图与各层契约，实现细节以代码及其注释为准。

## 1. 设计原则

- **集成在二进制里，不额外启动进程**。`ominiforge eval` 子命令直接调 `app::assemble` + `Agent::run_turn`。
- **判定 ≠ 描述**。Monitor 问"发生了什么"（描述性，无期望），Eval 问"做对了吗"（规范性，需要 ground truth）。二者使用同一条 event stream，但数据模型正交，不合并。
- **Deterministic 优先**。能用代码断言的不上 LLM judge。judge 有真实校准成本，是后期选项，不是默认路径。
- **Score 是一等数据**。每次 eval run 的 per-case 分必须持久化，因为分析层（run 间 diff、回归检测）必须跨 run 查询；`events.jsonl` 是 per-session 的，兜不住这一层。
- **run 是新的聚合层级**。当前架构：`event → session`；eval 新增：`case(1 session) → run(N case) → run 序列`。
- **先攒 case，后建基础设施**。20–30 个来自真实失败的 case 是启动最小有意义 eval suite 的前提。

## 2. 与现有层的边界

```
events.jsonl (source of truth)
│
├── Monitor     fold → SessionSummary   描述性：token/tool 调用数/失败率
│                                        单 session，无期望，永远不说"对不对"
│
└── Eval Scorer fold + case + 可选 workspace → Score
                                         规范性：需要 EvalCase 作为对照
                                         可复用 Monitor 的聚合数值
                                         但绝大多数 scorer 要原始事件里的细节
                                         （tool input JSON、model 文本、world state）
```

Monitor 的 fold 模式是 scorer 的形状原型，但 scorer 不是 Monitor 的扩展。

## 3. 核心抽象

### 3.1 EvalCase

一个 case 是四元组：**input + target + checker + metadata**。

```toml
# .omini/eval/suites/coding/fix-off-by-one.toml
id = "fix-off-by-one-001"
source = "manual"          # manual | ingested | bootstrap —— case 来源
status = "approved"        # approved | proposed —— 自动提取的初始为 proposed，人审后转 approved
origin_session = ""        # ingested 时填来源 session_id，可追溯到原始运行
input = "The function returns n+1 instead of n. Fix it."  # 发给 agent 的 prompt
target = ""                                                # 可选；字符串 ground truth
tags = ["regression", "coding", "off-by-one"]
difficulty = "easy"

[[files]]                      # 放进 scratch workspace 的文件
path = "src/counter.rs"
content = """
pub fn count(n: u32) -> u32 { n + 1 }
"""

[checker]
kind = "tests"                 # 四种：exact | fuzzy | tests | state
command = "cargo test"
pass_patterns = ["test_count_returns_n"]
```

Case 的四种 checker，对应判分的四个桶：

| kind | 判分方式 | 适用场景 |
|------|---------|---------|
| `exact` / `fuzzy` | 精确/归一化字符串匹配 | Q&A、单一事实问题（GAIA 风格） |
| `tests` | 跑测试命令，检查 PASS/FAIL | Coding agent（SWE-bench 风格） |
| `state` | Diff scratch workspace 文件/目录 | 文件操作、配置修改 |
| `judge` | LLM-as-judge（后期） | 主观质量、无法程序化断言的语义 |

**不引入 docker/VM**。Tests checker 直接用已有的 `shell` tool 原语在 scratch workspace 里执行，不需要容器舰队。

### 3.2 Scorer trait

`Scorer` 对一次 agent 运行打一个维度的分：`score(EvalContext) -> Score`。
`EvalContext` 向 scorer 提供一次完整运行的只读视图——事件流（`events`，从
events.jsonl 读回）、重建的对话（`messages`）、跑后的 scratch workspace
（`workspace`）、以及 case 本身。`Score` 携带结论（`Pass | Fail | Partial |
Skip`）、可复盘的解释、以及 scorer 自定义的附加数据。

**设计要点**：deterministic scorer 是纯函数，不依赖 LLM；LLM judge scorer 会调
Provider（这也是 `score` 为 async 的原因）。runner 未接入前 `messages` /
`workspace` 为 `Option`（`None`），需要它们的 scorer 返回 `Skip`。

精确签名与字段可空性以代码及其注释为准：[`src/eval/scorer.rs`](../crates/ominiforge-core/src/eval/scorer.rs)
（`Scorer`）、[`src/eval/score.rs`](../crates/ominiforge-core/src/eval/score.rs)（`EvalContext` / `Score`）。

scorer 与 metric 分离：scorer 出**每样本**的分，metric 做**跨样本**聚合（pass_rate / pass_at_k / pass_hat_k）。一个 case 可挂多个 scorer，各自独立出分（对标 HELM 多维度，不压成单一数字）。

### 3.3 内置 Deterministic Scorer

优先实现，不依赖 LLM，便宜稳定，适合 CI：

| Scorer | 输入来源 | 断言内容 |
|--------|---------|---------|
| `ExactMatch` | `EvalCase.target` + model 文本 | 归一化字符串相等 |
| `FuzzyMatch` | 同上 | 包含/embedding 相似度 |
| `ToolCallCheck` | `ToolEvent::Started.input` | 某工具被调用、参数满足条件 |
| `NoToolError` | `ToolEvent::Failed` + `ToolOutput.is_error` | 无 tool 失败（业务+协议两种） |
| `TurnCompleted` | `TurnEvent` | Turn 以 Completed 结束（非 Failed/Stalled） |
| `TestsPass` | shell 执行 checker.command | 测试命令退出码 + pass_patterns |
| `WorkspaceDiff` | workspace before/after | 预期文件存在/内容匹配/无副作用 |

**Tool 失败的两种形态**（scorer 必须同时检查）：
- `ToolOutput.is_error = true`：业务失败（协议成功，但 tool 报错）
- `ToolEvent::Failed`：协议失败（spawn 失败、超时等）

**Turn 结局的三态**（scorer 判"任务完成"时必须区分）：
- `TurnEvent::Completed`：干净完成
- `TurnEvent::Failed { reason: Some(_) }`：优雅停止，副作用仍成立
- `TurnEvent::Failed { reason: None }` + 配对 `ErrorEvent`：硬错误

### 3.4 LLM Judge Scorer（后期）

`LlmJudgeScorer` 携带 few-shot 评分准则（rubric）、判分模型（`ResolvedModel`）、
以及判分格式（Binary / Graded(1..5) / ChainOfThought）。

**校准前提**：judge scorer 上线前必须：
1. 准备 ≥ 100 条人工标注样本（PASS/FAIL ground truth）
2. 在 held-out set 上报 TPR / TNR
3. 对齐结果达标后才可用于 CI gate

未校准的 judge 只能作为参考，不能作为通过/拦截依据。

## 4. Runner 与隔离

### 4.1 一次 case 的执行流

```
EvalCase
  │
  ├─ 建 scratch workspace（从 case.files fixture 拷贝）
  ├─ SessionStore::create_new(scratch_workspace)
  ├─ app::assemble(profile, scratch_workspace)
  ├─ Agent::run_turn(case.input)
  ├─ SessionStore::read_events(session_id)   ← 取回完整事件流
  ├─ rebuild_runtime(events)                 ← 重建 messages 给 scorer
  │
  └─ 对每个 Scorer::score(EvalContext) → Score
       └─ 写入 run manifest
```

每个 case 独立的 session + workspace，case 间完全隔离，互不影响。

### 4.2 pass@k / pass^k

同一个 case 重复跑 N 次（`--epochs N`）= 建 N 个独立 session。reducer 决定聚合方式：

- `pass@k`：k 次中至少 1 次 Pass —— 衡量能力上限
- `pass^k`：k 次全部 Pass —— 衡量可靠性（对 agent 产品更重要）
- `mean`：N 次分的均值（连续 scorer）

### 4.3 并发

Runner fan-out 多个 case 并发执行（tokio tasks），上限由 profile 或命令行参数控制，防止同时发太多 LLM 请求触发 rate limit。

### 4.4 只跑 approved case

Runner 默认只执行 `status = "approved"` 的 case。`status = "proposed"`（自动提取、尚未人审）的 case 不进入回归门，避免未经确认的 case 污染 pass rate。`--include-proposed` 可显式纳入（用于人审时预览候选 case 的判分）。

## 5. 存储层

### 5.1 目录结构

```
.omini/
├── sessions/                      # 现有：每个 agent session
│   └── <session_id>/
│       └── events.jsonl
│
└── eval/                          # 新增：eval 专属
    ├── suites/                    # case 定义（进 git）
    │   ├── coding/
    │   │   ├── fix-off-by-one.toml
    │   │   └── ...
    │   └── research/
    │       └── ...
    └── runs/                      # eval run 结果
        └── <run_id>/
            ├── manifest.json      # run 元数据
            └── scores.jsonl       # per-case 分（一行一条）
```

### 5.2 Run Manifest

```json
{
  "run_id": "01J...",
  "created_at": "2026-07-05T12:00:00Z",
  "git_commit": "abc123",
  "profile": "coding",
  "model": "openai-main/gpt-4o",
  "suite": "suites/coding/",
  "epochs": 3,
  "total_cases": 30,
  "pass_rate": 0.80,
  "pass_hat_k": 0.67
}
```

### 5.3 Score 行（scores.jsonl）

```json
{
  "case_id": "fix-off-by-one-001",
  "session_id": "01J...",
  "epoch": 1,
  "scorer": "TestsPass",
  "value": "Pass",
  "explanation": "cargo test: 3 passed, 0 failed",
  "duration_ms": 4200
}
```

**score 是一等持久数据**：run 间 diff 必须跨 run 查 per-case 分，`events.jsonl` 是 per-session 的，兜不住这一层。

## 6. 分析层

以 run 为单位，建立在 `scores.jsonl` 之上的关系型能力。按价值与依赖排序：

| # | 能力 | 输入 | 输出 |
|---|------|------|------|
| A1 | **单 run 聚合** | 一组 Score 行 | pass rate、按 scorer/tag 分组均值 |
| A2 | **run 间 diff** | 两次 run 的 per-case score | pass→fail / fail→pass 清单 + delta |
| A3 | **回归检测** | diff + baseline | "变差了吗"布尔判定（CI gate 真正的决策依据） |
| A4 | **维度切片** | Score + case metadata | 按 model/tag/difficulty 分组看分 |
| A5 | **失败聚类** | 失败 case 的 events + 输出 | 失败模式分类（LLM 辅助 + 人工确认） |
| A6 | **judge 对齐** | judge 分 vs 人工标注 | TPR/TNR/agreement（judge 上线前置） |
| A7 | **趋势/drift** | run 序列 | 时序曲线、分布漂移 |

A1–A3 是 CI gating 的最小有意义组合：**A2（diff）是核心，没有 diff 只看绝对阈值，价值大打折扣**。A4 依赖 case metadata 字段（设计 case schema 时预留）。A5–A7 是后期。

## 7. CLI 子命令

```
# 跑完整 suite，输出 per-case PASS/FAIL 表
ominiforge eval suites/coding/

# 指定 profile + 重复跑 3 次（pass^k）
ominiforge eval suites/coding/ --profile coding --epochs 3

# 只跑指定 tag 的 case
ominiforge eval suites/ --tag regression

# 对比两次 run（diff / 回归检测）
ominiforge eval diff <run_id_a> <run_id_b>

# 查看某次 run 的报告
ominiforge eval report <run_id>
```

exit code：全部 pass → 0；任何 case fail（或对比 baseline 有回归）→ 非零。CI gate 依赖 exit code。

## 8. Case 来源

### 8.1 手工积累（主线）

从**真实失败**积累，不从想象的 rubric 开始。每次修 bug：

1. 找到触发该 bug 的输入
2. 写成 case TOML，加入 `suites/regression/`
3. 确认 case 在修复后 pass、修复前 fail

15–30 个精选 case 是可信起步套件。**这是躲不掉的人力**，每个扎实 coding case 约 0.5–2 小时（要钉仓库 commit，确认测试不 flaky）。

### 8.2 Bootstrap 数据集

可近乎零成本加载覆盖通用能力，降低冷启动成本，但**不能替代针对 Ominiforge 自身行为的 case**：

| 数据集 | 覆盖 | Checker | 所需额外工作 |
|--------|------|---------|------------|
| **GAIA（~450 题）** | CLI research（文件读取、工具调用、多步推理） | 归一化精确匹配 | 实现 1 个 scorer + 文件服务 |
| OpenAI Evals 基础集 | 通用 Q&A | string exact/includes | 加载 JSONL |
| 自选小仓库（SWE-bench 风格） | Coding agent | 测试执行 | 每仓库 0.5–2h 选材 |

SWE-bench 官方 Docker 舰队不需要搬——只采纳其**格式**（钉 commit + 记 FAIL_TO_PASS/PASS_TO_PASS 测试名），在 scratch checkout 里跑项目自带测试命令，得到相同语义。

### 8.3 手工 Smoke Test 迁移（最快的起步）

把现有"verified live against mimo: codeword ... persisted..."这类手工验证步骤冻成 golden `events.jsonl` fixture，写成普通 `#[test]` 做断言（L0）。这几乎不需要新架构，立刻让 agent loop 重构有安全网。

**数据来源约束（重要）**：进 git 的 golden fixture 必须是**合成的**——虚构 session_id、通用占位 prompt、`/workspace` 占位路径、干净整数 token，不含任何真实用户数据（真实 prompt、绝对路径、命令输出）。真实运行派生的 fixture 属于用户专属数据，应放 `.omini/`（用户私有、不进公共仓库），且后续自动并入（§8.4）时需考虑脱敏。这与自动并入的 `origin_session` 追溯是两条独立路径：committed 回归 fixture 走合成，用户私有 case 走 `.omini/` + 脱敏。

### 8.4 自动并入（ingested case，与 Evolution 结合）

从运行历史自动提取 case，减少纯手工积累的负担。核心难题：**input 与 trace 可自动提取，但 ground truth（"什么算做对了"）不能**——一次运行本身不携带正确答案（EvalGen 的 criteria drift 问题）。因此自动化程度取决于 checker 从哪来，分三档：

| 档 | ground truth 来源 | 人工介入 | 典型场景 |
|----|------------------|---------|---------|
| **全自动** | 运行里已有确定性信号 | 无 | coding 任务结束时测试已绿 → case = `input + repo@commit + "同样测试要过"`，未来可确定性重放判分 |
| **半自动** | LLM judge 建议一条 rubric | 人确认 | 主观任务，judge 提议 checker，人审批 |
| **纯候选** | 无 | 人补 checker | 只存 input + trace，checker 留空 |

**只有确定性场景能真正全自动**（正是 SWE-bench 风格的主场）。主观场景的自动并入只能产出候选，checker 仍需人确认。

**提取来源要按信号筛，不是全量**：优先失败 / 被用户点踩或重试 / 异常 cost 的 session（理由见下方防污染门）。

**产物形态**：自动提取的 case 初始 `source = "ingested"`、`status = "proposed"`、`origin_session` 指向来源运行。人审批后转 `approved` 才进入回归门（§4.4）。

**必须的防污染门**（否则 eval 会失真）：

- **避免自我确认**：把"成功运行"都变 case，只会积累 agent 已经会做的事，测不出能力缺口。优先提取**失败 / 边界** case（regression 价值最高）。
- **去重 / 去饱和**：新 case 是否已被现有 suite 覆盖、该能力是否已稳定通过。
- **reward hacking 警惕**：case 若反向用于 evolution / RL reward，agent 自产自判易钻空子——审批门是必需的，不是可选的。

**前置依赖：workspace 快照**。要把一次运行变成可复现 case，需重建其**初始 workspace 状态**。当前 session 存 events 但不快照初始文件内容。两种情况：

- git 仓库：`repo + base_commit` 即可重建（SWE-bench 格式已含），成本低。
- 任意（非 git）workspace：需要新的初始快照能力（存入 artifact store）。

自动并入落地前必须先补此缺口，否则 ingested case 不可复现。

## 9. 与 Evolution 的双向闭环

`architecture.md §19` 的进化生命周期：

```
observed → proposed → approved → applied → evaluated
```

Eval 与 Evolution 通过同一个 worker、同一套生命周期形成双向闭环：

**方向一 —— Eval 验收 Evolution 提案（`evaluated` 步骤）**
- Evolution worker 生成提案（skill 草案、profile 修改、patch）。
- 应用提案后跑同一套 eval suite。
- 对比 pass rate delta（§6 A2/A3），判断提案是否真的改好了。
- eval scorer 就是 verifiable reward，同一套 artifact 同时用于评估与指导进化方向。

**方向二 —— Evolution 反向喂 case（§8.4 自动并入）**
- "提取 case" 是与 skill 草案 / profile 变更 / patch 并列的一种新 **proposal kind**，复用完全相同的生命周期：
  ```
  observed（一批 session）→ proposed（候选 case）→ approved（人审）→ applied（并入 suite）→ evaluated
  ```
- 从失败 session 提取候选 case，人审批后并入 suite，扩大回归覆盖。

因此 eval 先于 evolution 实现：不是 architecture 上 evolution 依赖 eval，而是**没有 eval，evolution 的"有没有变好"就无从量化**；反过来，evolution 又是 eval suite 规模化增长的自动来源。两者互为对方的基础设施。

## 10. 待后续完善

- Case schema 版本控制（case 修改后如何与历史 run 对比）。
- 并发执行与 rate limit 的配置（`eval.toml`）。
- Web dashboard 对接（run 列表、diff 视图、趋势图，Phase 6 实施时设计）。
- LLM judge 校准基础设施（人工标注管理、TPR/TNR 报告）。
- Workspace 初始快照能力（自动并入 §8.4 的前置依赖）。
- 自动提取的信号筛选策略与去重/去饱和算法（§8.4 防污染门）。
- MCP server mock 机制（case setup 需要特定 tool 状态时）。
- 跨 session 的 error analysis 流水线（A5，依赖 LLM 辅助聚类）。
