# Ominiforge workspace 开发环境（direnv 集成）

workspace 的开发环境由它自己的 `.envrc` 声明——nix flake、uv、或别的什么都无所谓，统一以 direnv 为公分母。ominiforge 在会话组装（`app::assemble`）时把 `direnv export json` 的结果作为**环境 overlay** 应用到该 session 派生的一切子进程：shell 沙箱、MCP 服务器、LSP 语言服务器。设计目标：**环境求值的成本永远不让用户感知**。

## 1. 链路

```
POST /workspaces（record_workspace）
   └─ 后台预准备：direnv export json（≤300s）→ 写快照（顺带预热 direnv 自己的 .direnv/）

assemble（每次会话冷启动 / resume；CLI 与 gateway 同一入口）
   ├─ 无 .envrc → 空 overlay，零开销
   ├─ direnv export json（≤2s 快通道）
   │    ├─ 成功 → 过滤 DIRENV_* → 写快照 → 使用
   │    └─ 超时/失败 → 读快照：
   │         ├─ 命中 → 使用快照（warn 标注快照年龄）+ 后台刷新
   │         └─ 未命中 → 空环境 + warn（提示 direnv allow / 在 shell 里验证 .envrc）+ 后台预准备
   └─ overlay → sandbox（shell 工具）/ MCP / LSP
```

实现：`src/env.rs`。`session_env` 是 assemble 的热路径；`refresh_cache` 是后台任务体；`record_workspace`（`src/gateway/registry.rs`）在登记 workspace 后 fire-and-forget 触发后者。

## 2. 两层缓存，不是重复造轮子

- **direnv 自己的缓存**（项目内 `.direnv/`）：`use flake` 等 stdlib 按 watch 文件（`flake.nix`/`flake.lock`）指纹缓存求值结果。我们每次都走 `direnv export json`，没有任何绕过；热缓存下亚秒返回，输入变了 direnv 会正确地失效重估。后台预准备/刷新的首要作用就是**预热这层缓存**——昂贵的 nix 求值在没有用户等待的地方付掉。
- **快照缓存**（`<config root>/workspaces-env/<workspace-id>.json`，id = 工作区路径的 FNV-1a，与 `WorkspaceId` 同源）：上次成功导出的副本（带 `prepared_at`），只在 direnv 当下慢/失败时兜底。「可能陈旧但完整」严格好于「空环境」。

**新鲜度语义**：`.envrc`/`flake.nix` 刚改 → 当次 assemble 拿到快照（旧环境），后台刷新完成后**下一个 session** 拿到新环境。staleness 上限 = 一次重估的时长，且对用户无感。快通道每次都问 direnv，所以只要 direnv 缓存是热的，拿到的就是新鲜值。

## 3. 信任与开关

- 信任模型完全交给 direnv：`.envrc` 必须 `direnv allow`，ominiforge 不绕过、不代答。
- direnv 未安装 / `.envrc` 未 allow / 求值失败：warn 后按「无 workspace 环境」运行，绝不让无关 workspace 的会话挂掉。
- `--no-dotenv` 是总开关（命名是历史原因，实际同时关闭 direnv 激活与 `.env` 加载）。
- 活跃会话持有启动时的环境快照；改 `.envrc` 不追溯已在运行的会话（resume/冷启动重新求值）。

## 4. GC

快照与 workspace 配置同生命周期：`DELETE /workspaces/config/{id}` 在删 `<id>.toml` 时一并删除快照；孤儿处理沿用 `doc/workspace-config.md` 的显式 GC 模型。

## 5. 已知局限

- **boxlite 沙箱后端不应用 env overlay**：overlay 值是宿主路径（如 `/nix/store/...`），guest 里没有挂载点了无意义——待 `doc/sandbox.md` §3.7 的 `/nix/store` 挂载设计落地后再接。passthrough（默认后端）正常。
- env 求值的是**宿主**环境；服务器类子进程（MCP/LSP）本就跑在宿主，与 sandbox 内的 shell 共享同一份 overlay。
