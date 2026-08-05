# Workspace 配置

per-workspace 的沙箱策略覆盖层，位于 profile 与 gateway 默认之间。本文档定义它的位置、结构、解析优先级、以及生命周期/GC。

## 1. 解析链

沙箱策略沿四档派生，高档覆盖低档（`network` 覆盖语义；`permission` 见下方注）：

```text
workspace.toml  >  profile [network]  >  gateway default_network  >  Open（硬编码兜底）
```

- 任一档命中即用该值；`Open` 是一个新 boxlite session 不至于默认断网的兜底。
- 任一档策略名非法 → **fail loud**（Karpathy §12），建 session 失败，不静默回退到弱默认。
- 统一在 `app::resolve_network`（`src/app.rs`）解析，单元可测。
- `permission` 走平行的 `app::resolve_permission`,同为三层(workspace > profile > gateway),但 `deny` 是**并集**(安全底线,非覆盖)、`ask` 覆盖——见 `doc/permission.md` §3.1。

## 2. 位置：网关侧，不在项目目录

```text
<gateway_workspace>/.omini/workspaces/<workspace_id>.toml
```

- `workspace_id` = `WorkspaceId::from_path(canonical_path)`（FNV-1a 路径哈希，与 `workspaces.json` 同一套 id，版本稳定、可持久化）。
- 与 `workspaces.json` 同目录家族——per-workspace 的服务端状态集中在一处可信目录。

**为什么不放项目目录（如 `<project>/.omini/`）：** 项目目录是 **agent 可读写**的（`doc/sandbox.md` §3.3：「app 把 workspace 当普通目录」）。从 agent 可写的地方读安全策略 = agent 能给自己放开网络/权限 = 权限提升，撞 secret-store 威胁模型（[`architecture.md`](./architecture.md) §15）。网关目录由**部署者掌控、可信**。

## 3. 结构

workspace.toml 是一个 **workspace 命名空间**——不止网络策略,还承载共享挂载,以后 workspace memory 也放这。

```toml
# <gateway>/.omini/workspaces/<workspace_id>.toml
[network]
policy = "allowlist"                 # isolated | allowlist | open
allow  = ["crates.io", "pypi.org"]   # 仅 allowlist 生效

[[mounts]]
anchor = "workspace"                 # session | workspace | gateway
path   = "cache"                     # 锚点根内相对子路径(可空=根本身)
guest  = "/cache"                    # guest 内绝对挂载点
ro     = false                       # 只读挂载,默认 false(RW)

[[permission.deny]]                  # 本 workspace 追加的工具禁令(最高层)
tool     = "shell"
contains = ["git push"]
```

- `[network]` 缺省或无 `policy` 键 → 不构成覆盖，落到 profile/gateway 档。
- `[permission]` = 本 workspace 的工具门控,三层解析的**最高层**(`doc/permission.md` §3.1)。语义与 profile 一致:`deny` 与 profile+gateway **并集**(workspace 只能加禁令,不能放开下层禁令——因本文件在网关可信目录、非 agent 可写,故加 `deny` 安全),`ask` 覆盖下层。缺省=空=不贡献规则。
- `[[mounts]]`:命名锚点辅助挂载(`doc/sandbox.md` §3.7)。锚点命名**共享范围**,不是用途:

  | anchor | host 根 | 共享范围 |
  |---|---|---|
  | `session` | `<gateway>/.omini/sessions/<session_id>/work/` | session 私有 |
  | `workspace` | `<gateway>/.omini/workspaces/<workspace_id>/shared/` | 同 workspace 跨 session |
  | `gateway` | `<gateway>/.omini/shared/` | 全局 |

  - `path` 禁 `..`/绝对(逃逸 fail-loud);`guest` 必须绝对(否则 fail-loud);host 目录按需建。
  - 三根全在网关侧、可信;仅 boxlite 兑现,passthrough 遇 `[[mounts]]` fail-loud 拒绝(无命名空间)。
- 记录整体对**未来 workspace memory** 等仍开放(同文件加 section),当前定义 `[network]` + `[[mounts]]` + `[permission]`——其余需求未落地前不设字段(现在设计=猜)。
- 未知键忽略，向前兼容。

> **LSP/format 的项目覆盖不在本文件。** 安全策略（network/permission）必须在网关可信目录（§2）；但 lsp.toml/format.toml 是 **spawn 配置**而非安全策略，故项目覆盖直接读写 **`<workspace>/.omini/config/lsp.toml|format.toml`**（`doc/lsp.md` §8 / `doc/format.md` §8），由同一 `WorkspaceConfigDialog` 承载（与 network/permission 并列），运行时经 `app::assemble` 的 `lang_config_roots` 生效。install 探测用该 workspace 的 env-overlay PATH。

## 4. 生命周期与 GC

配置可比其项目活得久：项目被移走/删掉，但策略文件还在 `<gateway>/.omini/workspaces/`。

**原则：绝不自动物理删。** 路径消失可能是**瞬时的**（盘未挂载、项目 mid-move、worktree 临时删）；静默删一个用户手写的策略 = 不可回退的数据丢失。所以：

| 操作 | 语义 |
|------|------|
| `GET /workspaces/config/orphans` | **只读**列出「路径已不可解析」的配置（含它曾对应的 path，供人识别）。不删任何东西。 |
| `DELETE /workspaces/config/{workspace_id}` | **显式**删单个配置。幂等（不存在也返回 204）。GC 的唯一删除路径。 |

无自动 GC 触发器——对齐 session archive 的「显式、one-way」退休哲学。

## 5. 落盘布局

```text
<gateway_workspace>/.omini/
├── sessions/                    # session store
├── workspaces.json              # workspace_id → canonical path 反查表
└── workspaces/                  # 本文档
    ├── <id-a>.toml
    └── <id-b>.toml
```
