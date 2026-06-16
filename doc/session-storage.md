# Session Storage 设计

本文档定义 session 的文件结构、事件持久化格式、fork/compaction/reconfiguration 机制和目录组织方式。

## 1. 目录结构

```text
.omini/sessions/
  {session_id}/
    session.toml
    events.jsonl
    context_snapshot.json   # 仅 fork/compaction/reconfiguration 时存在
    artifacts/
  {session_id}/
    ...
  index/
    sessions.sqlite         # 查询索引，可从文件重建
    search/                 # 全文检索索引，可从文件重建
```

- 目录名即 session_id，扁平存放，不按时间分片。
- session_id 采用 ULID 格式（时间排序 + 随机，26 字符），`ls` 时自然按创建时间排列。
- 索引数据库可从 session 文件重建，不承担唯一真相角色。

## 2. session.toml

纯元数据，不含运行时状态，不含 system prompt。

```toml
id = "01J5M3HKEA7V2X3P1YKRN9C4WG"
profile_id = "coding-agent"
created_at = 2026-06-11T10:00:00Z
workspace = "/home/user/project/foo"  # 可选，无则 filesystem tools 受限

[origin]
kind = "new"  # "new" | "fork" | "compaction" | "reconfiguration"
parent_id = "01J5M2..."   # 非 new 时存在
fork_at_seq = 42           # 仅 fork 时存在
```

设计决策：

- **无 status 字段**。Session 不需要显式生命周期状态。Session 存在即可用，任何 session 随时可被 fork。UI 需要区分"当前在用"与"历史"时，从 `last_event_at`（索引数据库缓存）或是否有子 session 派生判断。
- **无 reason 字段**。Kind 已足够表达来源语义。
- **parent_id 统一**。Fork、compaction、reconfiguration 都用同一个 parent_id 字段，kind 区分语义。
- **workspace 可选**。CLI 默认填 CWD，Web/Gateway 由用户显式选择，研究/聊天类 session 可不设置。workspace = None 时 filesystem tools 不可用或受限。

## 3. events.jsonl

每行一个事件。省略 session_id（从目录名获取）。

```json
{"schema_version":"ominiforge.event.v1","seq":0,"timestamp":"2026-06-11T10:00:00Z","source":{"kind":"Runtime","id":"ominiforge"},"parent_event_id":null,"turn_id":null,"payload":{"Session":{"Created":{"profile_id":"coding-agent","tools":["shell","read_file"]}}}}
```

设计决策：

- **不含 session_id**。避免同一 session 内每行重复，节省存储。Session_id 从目录名获取。
- **首条事件为 SessionEvent::Created**。记录初始 config 快照（profile_id、tool list 等），使 replay 自包含。
- **不生成 transcript.md**。人类可读展示由前端（TUI/Web/App）从 events.jsonl 解析渲染。

## 4. context_snapshot.json

仅在 origin.kind 非 "new" 时存在。内容为完整 messages 数组，agent loop 启动时直接加载作为初始上下文。

```json
[
  {"role": "system", "content": "你是一个 coding agent..."},
  {"role": "user", "content": "帮我写个函数"},
  {"role": "assistant", "content": "好的，这是实现..."}
]
```

设计决策：

- **格式统一为 messages 数组**。无论 fork、compaction 还是 reconfiguration，context_snapshot 都是同一格式。Agent loop 加载时不关心 origin kind。
- **System prompt 就是 messages 数组中的 system role message**。无独立存储机制。
- **自包含**。子 session 不依赖父 session 即可运行。父 session 可被删除而不影响子 session 功能。

## 5. Session 诞生方式

| origin.kind | 触发场景 | context_snapshot | 与父 session 关系 |
|-------------|---------|-----------------|------------------|
| `new` | 用户开始新对话 | 无 | 无父 session |
| `fork` | 用户从某点分叉探索 | 父 session 在 fork 点的 context view | 独立运行，可选回查父 session |
| `compaction` | 上下文超限自动/手动压缩 | LLM 生成的摘要（messages 数组格式） | 独立运行，可选回查父 session 细节 |
| `reconfiguration` | system prompt / tool set 变更 | 当前 context view + 新 system prompt | 独立运行 |

所有非 "new" session 共享相同加载机制：读 context_snapshot.json → 作为初始上下文 → 追加新事件。

## 6. Fork 与 Compaction 语义区分

- **Fork**：精确上下文复制。Context snapshot = 父 session 在 fork 点发给模型的完整 messages。目的是从同一状态探索不同方向。
- **Compaction**：有损压缩。Context snapshot = LLM 对父 session 历史的摘要。目的是在上下文超限时延续对话。
- **Reconfiguration**：配置变更。Context snapshot = 当前 context view 替换 system prompt 后的 messages。目的是保持历史不可变的前提下更新 agent 能力。

三者区别在语义，不在机制。运行时行为一致。

## 7. 父子 Session 依赖关系

- 子 session 完全自包含。Context snapshot 存储了启动所需的全部上下文。
- 父 session 可被用户删除。删除后子 session 仍可正常运行。
- Compaction 的回溯引用（"之前的细节在父 session 里"）是 **optional**。用于审计和调试，不影响运行。
- Session 之间无硬依赖。不需要维护依赖图来判断"哪些旧 session 不能删"。

## 8. 并发控制

### Session 级互斥

一个 session 同一时刻只允许一个 writer（agent loop）。

```text
.omini/sessions/{session_id}/events.jsonl   # flock(EXCLUSIVE) on this file
```

- Agent loop 启动时对 events.jsonl 执行 flock(EXCLUSIVE)。
- 拿不到锁 → 报错 "session in use by another process"。
- 进程退出或 crash → 内核自动释放 flock，无 stale lock 问题。
- 读取（如 Web UI tail events.jsonl）不需要排他锁，flock(SHARED) 或直接读均可，append-only 文件对 reader 安全。

适用场景：CLI 和 Gateway 同时运行时，防止两者对同一 session 写入冲突。

### SQLite 索引并发

sessions.sqlite 使用 WAL mode + busy_timeout（5s）。

- 多 reader 并行，单 writer 排队。
- 短暂写冲突自动重试。
- 索引可从 session 文件重建，非关键路径。

### Gateway 内部并发

Gateway 是 tokio async 运行时，多请求并发到达：

- 每个 session 一个 agent loop 实例。
- 同一 session 的请求串行化（per-session Mutex 或 actor model）。
- 不同 session 完全并行，无竞争。

## 9. 待后续讨论

- 索引数据库字段设计（sessions.sqlite schema）。
- Artifact 与 event log 的引用关系细节。
- Session 清理策略（自动过期、容量限制）。
