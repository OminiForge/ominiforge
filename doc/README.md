# Ominiforge 文档

本目录是项目的**设计契约与运维手册**，可导出为静态文档站（mdbook，见 `book.toml`）。

## 组织原则

文档按「**稳定性与读者**」分四类目录。CLAUDE.md 规则 13 要求文档只记录
框架性、设计性内容，**不记录会随代码变化的具体实现**（接口/类/函数签名属于代码与注释）：

| 目录 | 内容 | 变化频率 | 例子 |
| ---- | ---- | -------- | ---- |
| `design/`     | 系统**目标结构**与长期契约 | 慢，须审慎 | 架构、协议、权限模型 |
| `operation/`  | **怎么操作**：发布、评测、工具链政策 | 中 | release、eval、MSRV |
| `decisions/`  | **为什么这么定**，追加不删改 | 只增 | 架构决策记录 (ADR) |
| `research/`   | 临时探索与调研，**可丢弃** | 快 | 一次性调研笔记 |

判断一篇文档该放哪：**「三个月后它还成立吗？」**
成立 → `design/`；会过时但要执行 → `operation/`；解释历史决策 → `decisions/`；
一次性 → `research/`。

## 规则

1. **单一事实源**（CLAUDE.md 规则 12）：一个主题只在一篇文档讲，其它地方引用（`见 ./xxx.md`），不复述。
2. **新文档必须登记**到 `SUMMARY.md`，否则 mdbook 导航和 CI 的孤儿检查会失败。
3. **front matter**：每篇文档顶部用 HTML 注释标注元信息：
   ```html
   <!-- status: current | draft | deprecated -->
   <!-- owner: @OminiForge -->
   ```
   `deprecated` 的文档保留但不再维护，内容须指向替代文档。
4. 代码级细节（接口、函数签名、具体实现取舍）**不写进这里**，写在代码与注释中。

## 本地预览 / 构建

```sh
just doc        # mdbook serve，浏览器实时预览（单版本，当前 master）
just doc-build  # mdbook build，产出静态站到 doc/book/
just doc-site   # 构建全版本站点到 doc/site/（每个 release tag + dev）
```
## 多版本文档（按 release tag）
mdbook 本身不带版本概念；本项目用 rustdoc/docs.rs 的模式实现多版本：
`build-all-versions.sh` 对每个 `vX.Y.Z` tag 各构建一份到 `site/<版本>/`，master 作为 `dev` 版本，
并生成 `versions.json` + 根重定向。页面顶部的版本下拉框（`version-switcher.js`）
读取 `versions.json`，让读者切换到同一页面的其它版本。release 时 CI 自动重建并部署到
GitHub Pages（见 `.github/workflows/release.yml` 的 `docs` job）。
读者访问 `https://ominiforge.github.io/ominiforge/` 会自动跳到最新稳定版，也可手动切到 `dev` 看未发布文档。
