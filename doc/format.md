# Ominiforge 自动格式化（format）

本文档描述 edit/write 后的自动格式化。它与 LSP **完全解耦**——虽然两者都按扩展名路由，但本质是两个系统，失败语义相反，不可混入 `src/lsp/`。

## 1. 为什么不属于 LSP

| 维度 | LSP 诊断 | 自动格式化 |
|---|---|---|
| 接口 | 有状态长连接 JSON-RPC 协议 | 无状态一次性进程调用 |
| 失败语义 | **fail-open**：拿不到诊断就不附带，绝不阻塞文件操作 | **fail-closed**：任何可疑状况→跳过格式化、用原始文本，绝不写入可疑结果 |
| 状态 | 服务器常驻，需 `didOpen`/`didChange` 同步 | 调用即弃，无状态 |

把 fail-closed 的同步改写塞进 fail-open 的异步诊断模块，会让 `doc/lsp.md` §4 的性能模型与失败哲学互相打架。

## 2. 定位与目标

`edit`/`write` 写盘后、生成 diff 与诊断**之前**，同步地用该文件的格式化器把内容格式化，**返回给模型的 diff 与诊断都基于格式化后的最终文本**。

- 模型的下一次编辑锚定的是真实（已格式化）的文件状态，不会因格式化漂移而 `not_found`。
- 诊断与 diff 天然一致，消掉「格式化器改文件 vs LSP 读旧文本」的时序竞态。

## 3. 统一接口：薄调用约定，非协议

格式化器没有 LSP 那样的协议，唯一公约数是 CLI 习惯。统一接口是一个**极薄的按扩展名路由 + 调用约定**，不是客户端层。

**首选 stdin→stdout 模式**：ominiforge 把源码喂给 formatter 的 stdin、读 stdout 的结果、**自己写盘、自己生成 diff**。绝不依赖 formatter 的原地改写（`rustfmt --emit files` / `clang-format -i` / `prettier --write`）——那样 ominiforge 就失去了「最终文本」，无法出 diff 和喂诊断。

常见 formatter 的 stdin→stdout 调用：

| formatter | 扩展名 | stdin→stdout 调用 |
|---|---|---|
| rustfmt | `rs` | `rustfmt --emit stdout` |
| clang-format | `c`,`cc`,`cpp`,`h`,`hpp` | `clang-format`（默认 stdin→stdout） |
| prettier | `js`,`ts`,`tsx`,`json`,`md`,… | `prettier --stdin-filepath <name>` |
| black | `py` | `black -` |
| ruff format | `py` | `ruff format -` |
| shfmt | `sh` | `shfmt`（默认 stdin→stdout） |
| gofmt | `go` | `gofmt`（默认 stdin→stdout） |

**配置文件发现交给 formatter 自己**（`.clang-format` / `rustfmt.toml` 向上查找）。不显式指定配置路径——那绑死用户的既有工作流，丧失灵活性；配置错误的检测靠 §4 的失败信号，而非显式指定。

**语言级配置零干预。** 我们只设 cwd（文件所在目录），formatter 自己向上找配置——`rustfmt.toml`（含 `edition`）、`.clang-format`、`.prettierrc` 都是 formatter 自己的事。推论：只在 `Cargo.toml` 里声明 edition 的项目，rustfmt 从 stdin 读时拿不到它（rustfmt 不读 `Cargo.toml`），会按 2015 默认解析、对 2018+ 语法报错——被 §4 防线拦成响亮跳过，即**该项目不格式化**。这是刻意的：修正在项目方（补一份 `rustfmt.toml`，一行 `edition = "..."`），不在我们这边再长一层语言发现。

## 4. fail-closed：静默回退的防线

**要防的坑**（clang-format 实例）：配置文件有错误时，clang-format 不显式报错，而是回退到内置默认配置把代码排成另一个格式，exit code 仍是 0。若把这种结果写进文件、把被默认配置重排的 diff 喂给模型，模型会以为那是它自己编辑的合理结果——比不格式化更糟。

防御（三层，前两道已覆盖 clang-format 场景）：

1. **stderr 非空即失败**：formatter 配置解析错误时通常在 stderr 打一行（即使 exit 0）。判定：`exit != 0` **或** `stderr 非空` ⇒ 失败。丢弃输出、用原始文本、stderr 内容 `tracing::warn!` 一次（fail-loud，但不阻塞 edit/write）。这把「静默回退」变成「响亮跳过」。
2. **一致性校验**：格式化必须幂等且不丢内容。输出为空而输入非空，或行数/非空白 token 数与原文偏差超阈值 ⇒ 判定异常，跳过。挡住「配置错误导致整个文件面目全非」（那种情况 token 结构通常剧烈变化）。
3. **有界且 best-effort**：formatter 不存在/超时/报错 ⇒ 跳过格式化，直接用原始文本出 diff。绝不能让一次 edit 因为 prettier 没装而失败。

**核心不变量**：宁可返回未格式化但真实的编辑结果，也绝不返回可疑的格式化结果。

## 5. 配置：mode + 与 LSP 同构的分层 + 注册表

复用 LSP 的「内置注册表 + 全局 + workspace 分层 + `enabled` 墓碑」机制（`doc/lsp.md` §3），把 §3 的调用表做成编译进的注册表，用户可覆盖 command、禁用某个内置 formatter、或新增自定义 formatter。**配置在 Web 界面编辑**（与 LSP 共用分层配置编辑器，`doc/lsp.md` §7，单独排期）。

配置文件是各 root 的 `config/format.toml`（与 `lsp.toml` 同层、同合并语义）：顶层 `mode` 键 + `[[formatters]]` 表。

```toml
mode = "file"   # "file"（默认）| "edit" | "off"

[[formatters]]
name = "rustfmt"                  # 唯一标识，用于日志与「formatted by <name>」标注
command = "rustfmt"
args = ["--emit", "stdout"]       # 可含 {file} 占位符，spawn 时替换为所触文件名
extensions = ["rs"]
# enabled = false                 # 高层写 false 以禁用（含禁用内置条目，墓碑语义）
# supports_line_range = true      # 能否局部格式化（mode="edit" 时按此决定）
# format_timeout_ms = 2000        # 单次格式化的硬上限（fail-closed，超时即跳过）
```

**路由是 first-match**（与 LSP 的多对多不同）：格式化是**改写**，跑两个 formatter 会互相覆盖，所以一个文件只路由到**第一个**声明其扩展名的启用 formatter。内置注册表里 `black` 排在 `ruff-format` 前——想用 ruff 的项目在自己的 `format.toml` 墓碑掉 `black` 即可。

### format file vs format edit（用户可选）

`mode = "file" | "edit" | "off"`，**默认 `file`**。两者语义不同，且对某些 formatter **产出结果不同**——clang-format 对「局部片段」（缺外围上下文）和「完整文件」的缩进/折行决策不一样，故这是真实差异，不是偏好：

- `file`：整文件格式化。结果最稳定、最符合「项目统一风格」，但可能顺带改了模型没动的部分。
- `edit`：只格式化本次 edit 触碰的行段。改动最小、归因最干净，但局部排版可能和整文件排版不一致。
- `off`：禁用。

**per-formatter 局部模式支持表**（`mode="edit"` 时按此决定）：

| formatter | 整文件 stdin→stdout | 局部模式 | `mode="edit"` 时 |
|---|---|---|---|
| clang-format | ✓ | `--lines=start:end` | 局部 |
| rustfmt | ✓ | `--file-lines` | 局部 |
| prettier | ✓ | ✗ | **跳过 + 日志** |
| gofmt | ✓ | ✗ | 跳过 + 日志 |
| shfmt | ✓ | ✗ | 跳过 + 日志 |
| black / ruff format | ✓ | ✗ | 跳过 + 日志 |

**`mode="edit"` 且 formatter 不支持局部 ⇒ 跳过 + 日志，绝不静默回退 `file`**（静默回退正是要避免的「结果与预期不一致」，见 §4）。注册表每条 formatter 需标注 `supports_line_range: bool`。

## 6. 执行顺序

```text
edit/write 产出模型的目标文本（内存中）
  → format（§3 stdin→stdout，fail-closed；mode 决定整文件或行段）
  → 盘上完整文件（fmt 后）
       ├─ diff：编辑前 → fmt 后【完整文本】，给模型看的「改动呈现」
       │        （合并 diff，块上方标注 "formatted by <name>"）
       └─ diagnostic：对 fmt 后【完整文本】做分析（不是消费 diff）
  → 返回模型：合并 diff（带 fmt 标注）+ 诊断
```

**diff 与 diagnostic 是同一完整文本的两个独立产物**：diff 是「编辑前 vs 完整文本」的呈现，diagnostic 是对完整文本的语义分析（LSP/tree-sitter 需要全文解析，给它 diff 无意义）。两者输入都是完整文件，互不依赖。

`edit` 的 diff view 当前从编辑前的 plan 渲染；fmt 插在「写盘」与「渲染 view」之间，view 渲染用的文本从「plan 的 new_content」换成「fmt 后的 new_content」——否则 view 与盘上真实状态不符。

## 7. 后续

- 与 LSP 注册表合并为一张「按扩展名的语言工具表」，消灭扩展名映射重复。

## 8. Web 配置编辑器

Format 的图形化配置与 LSP 同构（`doc/lsp.md` §8）：顶部 `mode` 选择（file/edit/off，复用 `PickerSelect`）+ formatter 固定清单（内置全列出、墓碑标灰、来源层 + 安装探测徽章、未安装不可改 command）。`mode=edit` 时列表标注各 formatter 的 `局部`（`supports_line_range`）支持——不支持的在 edit 模式下跳过（§5），徽章让用户一眼看出哪些会生效。

**两个编辑场所**（同 LSP）：全局默认在 Settings → **全局设置** tab 的「格式化」一节（不显示安装标注、不门控 command——见 `doc/lsp.md` §8 的理由）；项目覆盖在工作区 `WorkspaceConfigDialog`（写 `<workspace>/.omini/config/format.toml`，安装探测用该 workspace 的 env-overlay PATH，安装标注与 command 门控只在此层出现）。运行时 `app::assemble` 同样经 `lang_config_roots` 让项目 format.toml 生效。

**端点**（`gateway::langconfig`，读返回 `FormatConfigView{mode, formatters:[FormatterView{layer, builtin, installed, …}]}`）：

- 全局：`GET/PUT /config/format`（写回 primary root）。
- 项目：`GET/PUT /workspaces/{id}/config/format`（写回 workspace `.omini`；未知 id 404）。

**写语义**同 LSP：完整清单（只含用户字段；`supports_line_range`/`args` 由服务端重取）+ 整体重写目标层 + 重 `load` 验证。

**前端**：`lang-tools.ts` 的 `fmtToRows`/`fmtFromRows`（vitest round-trip）+ `FormatConfigEditor.svelte`（两个场所复用）。
