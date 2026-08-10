<!-- status: current -->
<!-- owner: @OminiForge -->

# 发布流程

本项目**参考 Zed 的版本管理理念**（常规节奏 + 语义化版本 + 每次发布附 changelog），
但落地为**全自动**：合并即触发，无需人手改版本号、写 CHANGELOG。

> Zed 用 pinned stable toolchain、每周一个 minor + 若干 patch，并每次发布都附 changelog。
> 参考的是其公开的工程做法与理念，不复制其代码或文案，无版权问题（详见 README「版本管理」）。

## 自动化机制：release-please

`.github/workflows/release.yml` 监听 `master` 推送，由
[release-please](https://github.com/googleapis/release-please) 驱动：

1. **解析 Conventional Commits**：`feat` → minor，`fix`/`perf` → patch，`!` / `BREAKING CHANGE` → major。
   因为使用 squash-merge 且 PR 标题经 CI 校验（`.github/workflows/pr-title.yml`），
   master 历史始终可被解析。
2. **自动维护一个 Release PR**：持续累加未发布的变更，预生成 `CHANGELOG.md`、预 bump `Cargo.toml` 版本。
   该 PR 随新提交自动更新，**无需人手编辑**。
3. **合并 Release PR** → 自动打 git tag + 创建 GitHub Release。
4. **构建产物**：release 创建后自动构建 Linux 二进制并上传到该 Release。

维护者唯一动作：**审阅并合并 Release PR**。这就是「重自动化、轻人力」的发布。

## 版本规则（0.x 阶段）

当前为 `0.x`，`release-please-config.json` 设了 `bump-minor-pre-major`：

| 变更类型 | commit 前缀 | 版本影响 |
| -------- | ----------- | -------- |
| 新功能   | `feat:`     | minor（0.1.0 → 0.2.0） |
| 修复     | `fix:`      | patch（0.1.0 → 0.1.1） |
| 破坏性   | `feat!:` / footer | minor（0.x 下 major 降为 minor） |

`1.0.0` 之前，破坏性变更只 bump minor（语义化版本对 0.x 的约定）。

## CHANGELOG

`CHANGELOG.md` 由 release-please 自动生成与维护，**不要手改**（手改会与其解析冲突）。
要写「面向用户的发布说明」，在 GitHub Release 页面补充。

## 手动干预（很少需要）

如需跳过某次发布：不合并 Release PR 即可，它会一直挂着并持续累加。
如需强制指定版本号：在合并提交 footer 写 `Release-As: 0.3.0`。
