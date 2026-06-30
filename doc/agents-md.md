# AGENTS.md / 项目指引文件

代码：发现与包装逻辑见 [`src/agents_md.rs`](../src/agents_md.rs)（`read_root`、
`discover_nearest`、`wrap`、`label_from_wrapped`、`Guidance`）；根目录注入见
[`src/app.rs`](../src/app.rs) `assemble`；嵌套懒加载见 [`src/agent/mod.rs`](../src/agent/mod.rs)
`TurnState::load_project_guidance` + `touched_path`；resume 去重集重建见
[`src/agent/resume.rs`](../src/agent/resume.rs) `rebuild_loaded_guidance`。

## 1. 是什么

`AGENTS.md` 是放在仓库里、专门写给 AI agent 的指引文件（构建/测试命令、代码规范、注意事项），
与 `README.md`（写给人）互补。规范见 <https://agents.md/>。内容是自由格式 Markdown，无强制字段。

本项目按目录解析文件名：**优先 `AGENTS.md`，缺失则回退 `CLAUDE.md`**，让既有 Claude 项目零改动可用。

## 2. 两层模型

指引文件可散落在 workspace 各级目录。注入分两层，避免「每读一个文件就注入一次」的开销：

1. **根目录（always-on）**：`<workspace>/AGENTS.md`（或 `CLAUDE.md`）在 `assemble` 时读取一次，
   追加到 system prompt 末尾（在 skill index 之后）。它始终在前缀缓存里，零 per-round 成本。

2. **子目录（懒加载，一次性）**：agent 通过 `read`/`write`/`edit` 触碰某个文件时，从该文件所在
   目录向上查找**最近**的指引文件（到 workspace 根之前为止——根目录那份已在 system prompt），
   命中且**本 session 尚未加载过**则作为一条 `InjectionEvent` 注入，去重后不再重复。

`shell`、MCP tool、`plan` 控制 tool 没有单一路径，不触发子目录加载。

## 3. 注入时机与去重

- **时机**：子目录指引在一个 round 的**所有 tool 结果落库之后**才注入（注入是一条 `User` 消息，
  必须排在 assistant 的 tool_calls 与对应 tool 结果之后，否则破坏 provider 要求的配对）。
- **去重键**：指引文件相对 workspace 的路径（如 `src/api/AGENTS.md`），存于
  `SessionRuntime.loaded_guidance: HashSet<String>`。在发现时**同步**检查并写入，因此：
  - 同一 round 内多个 tool call 命中同一目录 → 只注入一次；
  - 跨 round 再次触碰同一子树 → 不再注入。
- **路径选择器**：`read` 的 `:N-M` / `:N+C` / `:raw` 后缀在发现前剥除。
- **越界路径**：解析后逃出 workspace 的路径不注入（绝不读 workspace 外的文件）。

## 4. 注入格式

正文逐字透传，仅包一层定界符，`path` 属性既给 model 标注来源，又供 resume 还原去重键：

```
<project-guidance path="src/api/AGENTS.md">
<AGENTS.md 正文>
</project-guidance>
```

`InjectionSource::ProjectGuidance`（见 `doc/event-schema.md` §9）标识此类注入，便于 monitor 路由。

## 5. Resume

system prompt 不入事件日志，根目录指引每次 resume 由 `assemble` 重新读取——始终最新。

子目录指引的正文作为 `InjectionEvent` 已在日志里，`rebuild_runtime` 照常重放为 `User` 消息；
`rebuild_loaded_guidance` 额外扫描 `ProjectGuidance` 注入、用 `label_from_wrapped` 解析回 `path`
标签填充去重集，使 resume 后的 session 不会对同一子树重复注入。

## 6. 边界与已知限制

- 子目录发现从「被碰文件的父目录」起步：对一次 `read` 目录的调用，用的是该目录的父级，不含目录自身的
  指引文件。属边角情况，v1 不特殊处理。
- 「最近优先」是注入语义（只注入最近一份），不做父级覆盖合并；根目录那份恒在 system prompt，因此
  实际模型同时看到「根 + 最近子目录」两份，子目录在后（按新近度优先级更高）。
- `ominiforge init` **不** scaffold `AGENTS.md`——本特性纯读取，由用户自行编写。
