---
version: alpha
name: ominiforge-console
description: 'Developer-facing agent console with near-black surfaces, a scarce acid-lime accent, dense information hierarchy, Chinese-first content rendering, and precise tool-state feedback.'
colors:
  primary: '#c6f135'
  on-primary: '#0e0e10'
  primary-hover: '#d4ff3d'
  canvas-base: '#0e0e10'
  canvas-raised: '#141416'
  canvas-overlay: '#1a1a1e'
  canvas-float: '#222228'
  border-subtle: 'rgba(255, 255, 255, 0.055)'
  border-default: 'rgba(255, 255, 255, 0.09)'
  border-strong: 'rgba(255, 255, 255, 0.16)'
  text-primary: '#f0f0f2'
  text-secondary: '#8a8a9a'
  text-tertiary: '#5c5c6e'
  text-disabled: '#3a3a48'
  state-done: '#3d9b5c'
  state-done-bg: 'rgba(61, 155, 92, 0.1)'
  state-done-text: '#6dbe8c'
  state-running: '#e8a838'
  state-running-bg: 'rgba(232, 168, 56, 0.1)'
  state-running-text: '#f0bc5e'
  state-error: '#e05252'
  state-error-bg: 'rgba(224, 82, 82, 0.1)'
  state-error-text: '#f07070'
  reasoning-border: 'rgba(120, 120, 220, 0.18)'
  reasoning-bg: 'rgba(100, 100, 200, 0.05)'
  reasoning-text: '#7878b0'
  user-bg: 'rgba(198, 241, 53, 0.06)'
  user-border: 'rgba(198, 241, 53, 0.18)'
  syntax-key: '#7b9bd8'
  syntax-str: '#a8d68a'
  syntax-num: '#e8a838'
typography:
  product-title:
    fontFamily: Inter
    fontSize: 22px
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: -0.01em
  section-title:
    fontFamily: Inter
    fontSize: 18px
    fontWeight: 600
    lineHeight: 1.3
    letterSpacing: -0.01em
  body:
    fontFamily: Inter
    fontSize: 15px
    fontWeight: 400
    lineHeight: 1.6
    letterSpacing: 0
  chinese-body:
    fontFamily: LXGW WenKai
    fontSize: 15px
    fontWeight: 400
    lineHeight: 1.75
    letterSpacing: 0
  label:
    fontFamily: Inter
    fontSize: 12px
    fontWeight: 510
    lineHeight: 1.3
    letterSpacing: 0.04em
  mono-label:
    fontFamily: JetBrains Mono
    fontSize: 11px
    fontWeight: 510
    lineHeight: 1.35
    letterSpacing: 0.08em
  mono-value:
    fontFamily: JetBrains Mono
    fontSize: 13px
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: 0
  button:
    fontFamily: Inter
    fontSize: 14px
    fontWeight: 590
    lineHeight: 1.2
    letterSpacing: 0
rounded:
  sm: 4px
  md: 6px
  lg: 8px
  xl: 12px
spacing:
  xs: 4px
  sm: 8px
  md: 12px
  lg: 16px
  xl: 24px
  xxl: 32px
  page-x: 40px
  page-y: 32px
components:
  button-primary:
    backgroundColor: '{colors.primary}'
    textColor: '{colors.on-primary}'
    typography: '{typography.button}'
    rounded: '{rounded.md}'
    padding: '8px 14px'
  button-primary-hover:
    backgroundColor: '{colors.primary-hover}'
    textColor: '{colors.on-primary}'
    typography: '{typography.button}'
    rounded: '{rounded.md}'
    padding: '8px 14px'
  button-secondary:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.text-primary}'
    typography: '{typography.button}'
    rounded: '{rounded.md}'
    padding: '8px 14px'
  sidebar:
    backgroundColor: '{colors.canvas-raised}'
    textColor: '{colors.text-secondary}'
    typography: '{typography.label}'
    width: 220px
  dashboard-card:
    backgroundColor: '{colors.canvas-raised}'
    textColor: '{colors.text-primary}'
    typography: '{typography.body}'
    rounded: '{rounded.md}'
    padding: 16px
  popover:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.text-primary}'
    typography: '{typography.body}'
    rounded: '{rounded.lg}'
    padding: 16px
  text-input:
    backgroundColor: '{colors.canvas-base}'
    textColor: '{colors.text-primary}'
    typography: '{typography.mono-value}'
    rounded: '{rounded.sm}'
    padding: '8px 12px'
  user-message:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.text-primary}'
    typography: '{typography.chinese-body}'
    rounded: '{rounded.xl}'
    padding: '12px 16px'
  reasoning-block:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.text-primary}'
    typography: '{typography.chinese-body}'
    rounded: '{rounded.md}'
    padding: '12px 16px'
  tool-status-done:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.state-done-text}'
    typography: '{typography.mono-label}'
    rounded: '{rounded.sm}'
    padding: '2px 8px'
  tool-status-running:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.state-running-text}'
    typography: '{typography.mono-label}'
    rounded: '{rounded.sm}'
    padding: '2px 8px'
  tool-status-error:
    backgroundColor: '{colors.canvas-overlay}'
    textColor: '{colors.state-error-text}'
    typography: '{typography.mono-label}'
    rounded: '{rounded.sm}'
    padding: '2px 8px'
---

# ominiforge 前端设计宪法（DESIGN.md）

> **单一事实源。** 改任何 UI 前读它，改完对照它自检。设计风格不漂、质量不退，全靠这份文档 + `tokens.css` 两个锚。
> 来源：v2「Linear 参照」方向（huashu-design 三套逻辑产出，已迁入生产代码）。

---

## 0. 一句话定位

ominiforge Web 控制台是**开发者每天盯 8 小时的 agent 生产工具**。气质：克制、专业、信息密集但层级清晰、有工程师工具的扎实感。**不是**消费级聊天 app 的圆润可爱。

---

## 1. 设计哲学（优先级从高到低）

1. **从 token 长出，不凭空发明** —— 所有视觉走 `tokens.css` 的 CSS 变量。见 §2 铁律。
2. **单一 acid-lime accent，按需配给** —— 每个屏幕 acid-lime 只给**一个**主操作（如 Send 按钮 / 当前 nav）。满屏高饱和 = slop。
3. **状态一眼可辨** —— tool 的 done/running/error、turn incomplete，靠颜色+形状+动效冗余表达，不靠读文字。
4. **一处 120%，其余 80%** —— 招牌细节是 **tool 块的三态设计**（pip + badge + 脉冲边框 + spinner + 涟漪）。别处不跟它抢。
5. **暗色层级靠 surface ladder，不靠装饰** —— 画布/卡片/浮层/hover 只在 `--canvas-*` 四层内移动；深色界面的留白由暗面本身承担。
6. **反 AI slop** —— 见 §5。这是保品牌识别度，不是审美洁癖。

### 1.1 从 Linear 参考里吸收什么 / 不吸收什么

`DESIGN-linear.app.md` 只能作为参考资料，不是本项目事实源。可吸收的是抽象方法：

- **四级 surface ladder**：canvas → raised → overlay → float，层级递进不要跳级。
- **hairline depth**：深色界面优先用 1px 边框、细分割线和 surface lift 表达层次；少用大阴影。
- **单一 chromatic accent**：保持 acid-lime 稀缺，只用于主操作、focus、链接/当前状态等少数位置。
- **紧凑控件**：按钮约 8px × 14px，输入约 8px × 12px；默认圆角 6–8px，不做大 pill。
- **精确 typography**：标题轻微负字距，label/eyebrow 轻微正字距；正文保持可读行高。
- **产品 UI 是主角**：dashboard、conversation、tool 状态本身承担视觉重点，外壳只做安静框架。

不吸收：Linear 紫蓝色、Linear 字体/Logo/文案、官网 pricing/testimonial/CTA 组件、任何让 ominiforge 看起来像 Linear clone 的具体表达。

---

## 2. 配色 token 语义（铁律 + 速查）

🔴 **铁律：组件里禁止 hardcode 颜色值（`#xxx` / `rgb()`）。只能用 `var(--token)`。需要新颜色 → 先进 `tokens.css`，再用。**
原因：颜色集中在 tokens.css 才能换肤、调对比度、保持一致；一旦组件里散落魔法色值，几轮迭代必回退成 slop。

双主题：dark 默认 + light（`:root[data-theme='light']`）。两套都必须有值。light 下 acid-lime 用压暗的 `--accent`（避免亮底发飘），文字/链接用 `--accent-ink`。

| 用途      | token                                                           | 何时用                                                                                                                            |
| --------- | --------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| 画布 4 层 | `--canvas-base/raised/overlay/float`                            | base=主背景；raised=侧栏/顶栏/输入区；overlay=卡片头/输入框；float=code 块/最浮层。新增层级时先问能否复用这四层，避免凭空造第五层 |
| 边框 3 级 | `--border-subtle/default/strong`                                | subtle=分隔线/默认卡片；default=可交互边框；strong=hover/focus。深色层次优先用 hairline border + surface lift，不用厚重阴影       |
| 文字 4 级 | `--text-primary/secondary/tertiary/disabled`                    | primary=正文；secondary=次要；tertiary=label/时间戳；disabled=placeholder/极弱                                                    |
| 强调      | `--accent` / `--accent-hover` / `--accent-dim` / `--accent-ink` | accent=主操作填充；ink=亮色主题下的 lime 文字/链接                                                                                |
| 状态      | `--state-{done,running,error}` + `-bg` + `-text`                | base=pip/边框；bg=徽章底；text=徽章字                                                                                             |
| reasoning | `--reasoning-border/bg/text`                                    | think 块专用，靛蓝调，刻意「次一级」                                                                                              |
| user 气泡 | `--user-bg/border`                                              | 用户消息，acid-lime 淡色调                                                                                                        |
| 代码高亮  | `--syntax-key/str/num`                                          | tool JSON 参数着色                                                                                                                |

> 旧变量名（`--bg-primary`/`--surface`/`--border`/`--accent-fg` 等）保留为别名，向后兼容。新代码优先用上表语义名。

---

## 3. 字体规则

- `--font-sans`（Inter）：拉丁文 UI 文案、按钮、label。
- `--font-chinese`（LXGW WenKai 霞鹜文楷）：**所有中文内容**（对话、标题、placeholder）。中文必须用它，不能落到 Inter 的中文回退。
- `--font-mono`（Berkeley/JetBrains Mono）：**主角字体之一**。tool 名、JSON 参数、session id、RUNTIME label/value、kbd。等宽承载"工具感"。
- 数字对齐：表格/统计数字加 `font-variant-numeric: tabular-nums`。
- 标题层级：18–28px 的产品标题可用 `letter-spacing: -0.01em` 到 `-0.03em`；不要引入 Linear 官网式 56/80px hero 标题，除非未来真的做 marketing page。
- label/eyebrow：mono uppercase label 可用 `letter-spacing: 0.07em` 到 `0.09em`，作为分类信息，不要当装饰。

---

## 4. 组件规范

### 4.1 对话流 5 种 item（`sessions/[id]`）

逻辑由 `lib/conversation.ts` 的 `Item` 类型驱动——**改视觉别动状态机**。

- **user**：右对齐 `.user-bubble`，`--user-bg/border`，中文字体。
- **text**：markdown 渲染，行高 1.75。行内 code 用 `--syntax-str` 绿。链接 `--accent-ink`。流式时尾部 `.streaming` 加 acid-lime 闪烁竖条。
- **reasoning**：默认折叠。折叠=`.reasoning-toggle`（靛蓝边+预览首行+箭头）；展开=`.reasoning-body`（靛蓝左竖条）。安静，不抢主回复。
- **tool（120% 招牌）**：折叠头 = pip + name + status-badge + preview + chevron。三态：
  - `done` 绿 pip + 绿徽章 + 绿边框
  - `running` 琥珀 pip(涟漪) + spinner + 琥珀徽章 + **2s 脉冲边框**
  - `error` 红 pip + 红徽章 + 红边框
  - 展开 = params(JSON 语法高亮) + result。
- **流式光标**：2px `--accent` 竖条，`cursor-blink` 1.1s。

### 4.2 输入区

- `.input-box`（focus-within 时 acid-lime 淡发光）+ textarea + 底部 actions。
- 操作只有 **Cancel + Send**（英文）。Compact/其它走未来 `/` 命令。
- 控件密度参考 Linear 的紧凑原则：按钮约 `8px 14px`，输入约 `8px 12px`；触屏断点下可放大点击面积，但不要默认做巨大 CTA。
- 下方 `Type / for commands` 提示（`--text-disabled`，mono）。
- 状态行：turn incomplete 时显示 `Turn incomplete`（`--state-running-text`）。

### 4.3 侧栏 + RUNTIME（`+layout.svelte`）

- brand mark（acid-lime 方块）+ 分组 label（`Nav`/`Runtime`，mono uppercase）+ nav-item（active=lime 点+高亮）。
- **RUNTIME**：仅当在 session 页（`currentSession` store 非 null）显示。竖排 label/value，顺序固定 **workspace → env → model → profile**，**每项仅有数据才渲染**（"检测到才显示"）。
  - 当前：workspace/profile 接 `SessionMeta`（真）；model/env 待后端（Phase B1/B2），暂不渲染。
- 离开 session 页清空 store，避免上下文泄漏到列表/monitor/evolution。

### 4.4 内容页外壳（list/monitor/evolution）

layout 的 `main` **不带 padding**（对话页要全屏）。每个内容页自己包 `.page { height:100%; overflow-y:auto; padding: var(--space-8) var(--space-10); }`。

内容页层级按 surface ladder 决策：页面背景用 `--canvas-base`，一级卡片用 `--canvas-raised`，弹层/卡片内输入用 `--canvas-overlay`，代码/最浮层用 `--canvas-float`。hover 通常只提升一级或增强到 `--border-default`，不要同时加亮背景、粗边框、大阴影。

---

## 5. 反 AI slop 禁令（硬清单）

| 禁                                  | 为什么                                       | 例外                                    |
| ----------------------------------- | -------------------------------------------- | --------------------------------------- |
| 紫色大渐变铺底                      | "科技感"万能公式，无品牌信息                 | 无                                      |
| emoji 当功能图标                    | "不够专业用 emoji 凑"的病                    | 无（用纯 CSS/SVG 图标）                 |
| 圆角卡片 + 左彩色 border accent     | 2020-2024 烂大街组合                         | 无                                      |
| 均匀深蓝底 `#0D1117` + 通用青紫霓虹 | GitHub-dark 偷懒解                           | 无（我们的炭黑是 `--canvas-*`，有性格） |
| hardcode 颜色值                     | 见 §2 铁律                                   | 无                                      |
| 满屏 acid-lime                      | 强调色泛滥即失效                             | 无（一屏一主操作）                      |
| 大阴影制造层级                      | 深色工具界面会变脏，且和 surface ladder 冲突 | 弹层可用 token 化的小阴影               |
| pill 圆角泛滥                       | 会把工程工具做成消费级 app                   | 状态 badge / avatar 可用 full radius    |

正向：`text-wrap: pretty`、tabular-nums、合理留白节奏、状态冗余表达、hairline border、surface lift。

---

## 6. 改 UI 的标准流程

### 小改（调间距 / 修 bug / 换文案）

1. 直接改，**只用 token，不 hardcode 颜色**。
2. 改完对照本文档 §2-§5 自检。
3. `npm run check` 过 + 起 dev server 本机浏览器肉眼验。

### 大改（新页面 / 新组件 / 重排版）

1. 先确认是否真需要——能否复用现有组件/token。
2. 先决定 surface 层级：该区域在 `base/raised/overlay/float` 哪一层？hover/focus 是否只提升一级？
3. 不确定方向 → 可选先出 HTML 稿对齐视觉（huashu-design skill），再迁 Svelte。
4. 写进 Svelte，token 化，对照 §2-§5。
5. `check` + `build` + `test` 全过 + 浏览器验关键路径（含暗/亮主题）。
6. 大改后建议跑一次设计自评（huashu `references/critique-guide.md` 5 维度）。

### 验证手段

- 类型：`npm run check`（必须 0/0）
- 构建：`npm run build`
- 回归：`npm run test`（conversation 状态机 17 测试不能挂）
- 视觉：起 `npm run dev` + 本机浏览器（无显示器时用 playwright headless 截图，nix 环境用 `nixpkgs#playwright-driver.browsers` + `PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=1`）

---

## 7. 已知陷阱（踩过的坑，别再犯）

- **markdown 容器必须自补列表缩进**：`global.css` 的 `* { padding: 0 }` 清掉了 `ol/ul` 默认缩进。任何用 `{@html renderMarkdown(...)}` 的容器，都要在其 `:global(ol/ul)` 上加 `padding-left`，否则序号/`+`/`-` 列表贴左边。已修：`.item-text`、`.reasoning-body`。新增 markdown 容器照做。
- **不动状态机**：`conversation.ts` 的竞态/折叠/截断逻辑很微妙（修过多个重复渲染 bug）。改对话流只换 class/CSS/markup，别碰 apply/commitBlock。
- **中文字体回退**：中文内容忘了加 `font-family: var(--font-chinese)` 会落到 Inter 的丑回退。对话相关元素都要显式声明。

---

## 8. 路线图（详见 `~/.claude/plans/robust-toasting-moon.md`）

- **B1** 后端 gateway 暴露 resolved provider/model → RUNTIME 显示真 model
- **B2** 后端探测 workspace env（flake.nix/Cargo.toml…）→ RUNTIME 显示 env
- **B3** 前端 RUNTIME 接 B1/B2 真数据（组件已写成「有值才渲染」，后端就绪即自动出现）
- **B4** 运行层校验：从事件流 `ModelEvent::RequestStarted` 提取实际 model，与配置层比对，不一致 fail loud（不替换显示源）
- **C** monitor + evolution 铺满 v2 设计语言（含 monitor 的 `.error` 样式统一为全框红边）
