<!-- status: current -->
<!-- owner: @duskgrow -->

# 发布流程

本项目**参考 Zed 的版本管理理念**（常规节奏 + 语义化版本 + 每次发布附 changelog），
落地为**全自动**：合并即触发，无需人手改版本号、写 CHANGELOG。

## 发布产物（三种形态，统一版本号）

| 产物 | 内容 | crates.io | GitHub Release |
| ---- | ---- | --------- | -------------- |
| 核心库 `ominiforge` | 纯 lib（core） | ✅ | — |
| 纯 CLI 版 | `ominiforge` 二进制（serve，未来 TUI），无图形依赖 | ✅（crate `ominiforge-cli`） | ✅ 多平台压缩包 |

（零 UI 转向后不再有 GUI 桌面产物； ominiforge 不自带界面，UI 由门面承接，见
[`decisions/architecture-direction.md`](../decisions/architecture-direction.md)。）

- **统一版本**：所有 crate 继承 `[workspace.package].version`，同号发布。
- **crates.io 边界**：发布面 = `ominiforge` + `ominiforge-net` + `ominiforge-cli`。
- **内部依赖**：一律 `path + version` 双写——本地开发用 path（即时联动），
  `cargo publish` 时用 version（path 被剥离）。同步正确性由三层机制保证，不靠人工：
  cargo publish 硬校验上游版本存在、release-plz 按拓扑序发布、`cargo package` 剥离
  path 后重编译验证。

## 自动化机制：release-plz

`.github/workflows/release.yml` 由 [release-plz](https://release-plz.ieni.dev/) 驱动
（替代 release-please：它原生支持 Rust workspace 继承版本，后者在此报错）。

1. **解析 Conventional Commits**：`feat` → minor，`fix`/`perf` → patch，`!` → major
   （0.x 阶段按 minor）。因 squash-merge 且 PR 标题经 CI 校验，master 历史始终可解析。
2. **自动维护一个 Release PR**：累加变更、生成 CHANGELOG、bump workspace 版本。
3. **合并 Release PR** → 打 git tag + 创建 GitHub Release + 按拓扑序 `cargo publish`
   可发布的 crate（core → net → cli）。
4. **构建产物**：tag 推送后构建多平台 CLI 二进制并上传 Release。

维护者唯一动作：**审阅并合并 Release PR**。

## 版本规则（0.x 阶段）

| 变更类型 | commit 前缀 | 版本影响 |
| -------- | ----------- | -------- |
| 新功能   | `feat:`     | minor（0.1.0 → 0.2.0） |
| 修复     | `fix:`      | patch（0.1.0 → 0.1.1） |
| 破坏性   | `feat!:` / footer | minor（0.x 下 major 降为 minor） |

多个 `feat` 一次发布只 bump 一次 minor（看「有没有」而非「有几个」），不会跳号。

## CHANGELOG

`CHANGELOG.md` 由 release-plz 自动生成维护，**不要手改**。面向用户的发布说明在
GitHub Release 页面补充。

## 手动干预（很少需要）

跳过某次发布：不合并 Release PR 即可，它持续累加。强制指定版本：合并提交 footer 写
`Release-As: 0.3.0`。
