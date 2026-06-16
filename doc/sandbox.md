# Sandbox 与监控

本文档定义 tool 执行的监控策略和可恢复性设计。

## 1. 设计原则

- 所有 tool 执行（built-in + MCP）统一经过 event journal 记录。
- 监控是 Day 1 能力，不是附加功能。
- 可恢复性分阶段实现，初期聚焦监控，后续增加 workspace snapshot。
- 不使用 WASM 沙箱（已废弃），安全性靠用户信任 + 监控审计。

## 2. 监控：Tool 执行记录

> **ToolEvent 权威定义在 [`event-schema.md`](./event-schema.md) §6。** 本节聚焦执行/恢复策略，字段以 event-schema 为准。`file_changes` 是 Phase 2 追加字段。

### 2.1 ToolEvent 字段

每次 tool invoke 写入 events.jsonl，字段见 event-schema §6（`Started` 含 `source` / `working_dir`，`Completed` 含 `duration_ms` / `output_bytes` / `artifacts_created`）。

### 2.2 FileChange（Phase 2）

```rust
pub struct FileChange {
    pub path: PathBuf,
    pub op: FileOp,       // Created | Modified | Deleted
    pub diff: Option<String>,  // unified diff, 可选
}
```

初期不记录 file_changes（需要 pre/post diff，有性能开销）。Phase 2 为写文件类 tool 增加。

### 2.3 Source 标识

```rust
pub enum ToolSource {
    Builtin,
    Mcp { server_name: String },
}
```

Monitor 可按 source 聚合统计（哪个 MCP server 最慢、最贵、最常失败）。

## 3. Shell Tool 执行

Shell tool 作为 built-in tool（Rust 实现），分阶段增强：

| Phase | 方案 | 场景 | 成本 |
|-------|------|------|------|
| Phase 1 | 无沙箱，直接 `tokio::process::Command` | 本地 CLI/TUI，用户信任自身环境 | 近零 |
| Phase 2 | 可选容器沙箱（namespace/seccomp/OCI） | Server 部署、不可信任务、多租户 | 中 |
| Phase 3 | 可复现快照（容器 checkpoint/restore） | 调试回放、evolution 分析 | 高 |

### Phase 1 设计（Day 1）

- `tokio::process::Command` 执行用户命令。
- 工作目录 = session workspace（无 workspace 时拒绝执行）。
- 超时由 host 侧 tokio timeout 控制（默认 120s，可配置）。
- stdout/stderr 捕获，写入 ToolEvent result。
- 输出超 64KB 存 artifact store + 引用。

### Phase 2 设计（后续）

- Linux namespace（mount/pid/net）隔离。
- Seccomp 限制系统调用。
- 网络策略（允许/拒绝/代理）。
- 文件系统：overlay mount，workspace 为 lower layer，写入到 session 临时层。
- 资源限制：cgroup v2（CPU/memory/IO）。

### Phase 3 设计（远期）

- 容器 checkpoint（CRIU 或 OCI checkpoint）。
- 快照与 session event 关联，支持从任意点恢复。
- Evolution worker 可对比不同执行路径。

## 4. 可恢复性（Phase 2）

### 4.1 Git-based Workspace Snapshots

```text
Tool 执行流程（写操作）：
  1. pre-exec:  git stash / lightweight tag
  2. execute:   tool 执行
  3. post-exec: git diff → 记入 ToolEvent.file_changes
  4. 继续下一步

恢复：
  /undo        → git restore 到 pre-exec snapshot
  /restore N   → 恢复到第 N 次 tool 执行前的状态
  fork         → 从任意 snapshot 点 fork session
```

### 4.2 非 git workspace fallback

```text
策略：修改前备份到 .omini/sessions/{id}/backups/{seq}/
  - 只备份被修改的文件（不是全量）
  - 恢复时还原对应文件
```

### 4.3 快照粒度

待后续确定：
- 每次 tool 调用都 snapshot？（安全但有开销）
- 只对文件写操作 snapshot？（平衡方案）
- 用户手动 checkpoint？（最省但需用户主动）

## 5. MCP Server 监控

MCP server 作为子进程，额外监控：

| 指标 | 来源 |
|------|------|
| 启动时间 | spawn → initialize 完成 |
| 崩溃次数 | 进程非正常退出 |
| 重启次数 | 自动重启计数 |
| 调用延迟 | JSON-RPC request → response |
| 错误率 | failed / total calls |

这些指标由 monitor module 从 event stream 派生，不在 tool invoke 路径计算。

## 6. 资源限制（Phase 1 只做超时）

| 资源 | Phase 1 | Phase 2 |
|------|---------|---------|
| 执行时间 | tokio timeout（默认 120s） | + cgroup CPU quota |
| 内存 | 不限制 | cgroup memory limit |
| 输出大小 | 64KB inline 上限 | 同左 |
| 文件写入 | 不限制 | overlay fs quota |
| 网络 | 不限制 | namespace + iptables |

## 7. 废弃内容

以下概念已废弃：

- WASM 沙箱（wasmtime StoreLimits、preopens）
- 路径变量系统（$WORKSPACE 等用于 WASM preopen）
- Guest 可见文件系统映射（/workspace/、/data/ 等）
- WASI capability-based 权限

这些被替换为：全量 event 监控 + 可选 OS 级容器沙箱（Phase 2）。
