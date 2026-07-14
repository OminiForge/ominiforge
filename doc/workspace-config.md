# Workspace 配置

per-workspace 的沙箱策略覆盖层，位于 profile 与 gateway 默认之间。本文档定义它的位置、结构、解析优先级、以及生命周期/GC。

## 1. 解析链

沙箱策略（当前只有 network）沿四档派生，高档覆盖低档：

```text
workspace.toml  >  profile [network]  >  gateway default_network  >  Open（硬编码兜底）
```

- 任一档命中即用该值；`Open` 是一个新 boxlite session 不至于默认断网的兜底。
- 任一档策略名非法 → **fail loud**（Karpathy §12），建 session 失败，不静默回退到弱默认。
- 统一在 `app::resolve_network`（`src/app.rs`）解析，单元可测。

## 2. 位置：网关侧，不在项目目录

```text
<gateway_workspace>/.omini/workspaces/<workspace_id>.toml
```

- `workspace_id` = `WorkspaceId::from_path(canonical_path)`（FNV-1a 路径哈希，与 `workspaces.json` 同一套 id，版本稳定、可持久化）。
- 与 `workspaces.json` 同目录家族——per-workspace 的服务端状态集中在一处可信目录。

**为什么不放项目目录（如 `<project>/.omini/`）：** 项目目录是 **agent 可读写**的（`doc/sandbox.md` §3.3：「app 把 workspace 当普通目录」）。从 agent 可写的地方读安全策略 = agent 能给自己放开网络/权限 = 权限提升，撞 secret-store 威胁模型（[`architecture.md`](./architecture.md) §15）。网关目录由**部署者掌控、可信**。

## 3. 结构

```toml
# <gateway>/.omini/workspaces/<workspace_id>.toml
[network]
policy = "allowlist"                 # isolated | allowlist | open
allow  = ["crates.io", "pypi.org"]   # 仅 allowlist 生效
```

- `[network]` 缺省或无 `policy` 键 → 不构成覆盖，落到 profile/gateway 档。
- 记录整体对**未来权限门控**开放（同文件加 section），但当前只定义 `[network]`——需求未落地前不设具体权限字段（现在设计=猜）。
- 未知键忽略，向前兼容。

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
