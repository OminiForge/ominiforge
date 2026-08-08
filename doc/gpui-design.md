# Ominiforge GPUI 设计语言

> **单一事实源。** 这是 ominiforge 唯一 UI（GPUI 客户端）的设计宪法。改任何 UI 前读它，改完对照它自检。
>
> **与代码的边界（`CLAUDE.md` 规则 12/13）**：本文档承载**设计原则与意图**（为什么、是什么结构）；具体 token **值**（颜色 hex、字号、间距数值）的事实源是代码——`crates/ominiforge-ui/src/theme.rs`。本文档**不列具体数值**，只定语义与规则；改风格时，值变只改 `theme.rs`，**原则变才改本文档**。两处不说同一件事。

---

## 0. 一句话定位

ominiforge 客户端是**开发者每天盯 8 小时的 agent 生产工具**。气质：克制、专业、信息密集但层级清晰、有工程师工具的扎实感。**不是**消费级聊天 app 的圆润可爱。

---

## 1. 设计哲学（优先级从高到低）

1. **从 token 长出，不凭空发明** —— 所有视觉走 `theme.rs` 的语义 token，组件里禁止 hardcode 颜色/字号/间距值（§2 铁律）。
2. **单一 accent，按需配给** —— 每个屏幕 accent 只给**一个**主操作（如 Send / 当前聚焦项）。满屏高饱和 = slop。
3. **状态一眼可辨** —— tool 的 done/running/error、turn incomplete，靠颜色 + 形状（+动效）冗余表达，不靠读文字。
4. **一处 120%，其余 80%** —— 招牌细节是 **tool 块的三态设计**。别处不跟它抢。
5. **暗色层级靠 surface ladder，不靠装饰** —— 画布/卡片/浮层/hover 只在有限的 surface 层级内移动；深色界面的留白由暗面本身承担。
6. **反 AI slop** —— 见 §5。这是保品牌识别度，不是审美洁癖。

---

## 2. Token 语义（铁律 + 速查）

🔴 **铁律：组件里禁止 hardcode 颜色值 / 字号 / 间距。只能用 `theme.rs` 的语义 token。需要新值 → 先进 `theme.rs`，再用。**
原因：值集中在 `theme.rs` 才能换肤、调对比度、保持一致；一旦组件里散落魔法值，几轮迭代必回退成 slop。

**这条由机器强制，不靠自觉**：CI 的 `design-lint`（见 justfile）扫 `crates/ominiforge-ui/src/**`，禁止 `theme.rs` 以外任何文件出现字面色值构造（`rgb(0x` / `rgba(0x` / `hsla(` 等）。「用到某个颜色却发现没有 token」时，lint 拦下字面值、逼你回 `theme.rs` 加语义字段——这就是「用到再补」不会漂的保证。

> **值在哪**：以下每个语义 token 的具体颜色/字号/圆角，以 `theme.rs` 的字段为准。这里只定**语义与用法**。

### 2.1 颜色

| 用途 | 语义 token | 何时用 |
|------|-----------|--------|
| 画布分层 | `canvas_base / raised / overlay / float` | base=主背景；raised=侧栏/顶栏/输入区；overlay=卡片/输入框；float=code 块/最浮层。新增层级时先问能否复用这四级，避免凭空造第五级 |
| 边框分级 | `border_subtle / default / strong` | subtle=分隔线/默认卡片；default=可交互边框；strong=hover/focus。深色层次优先用 hairline border + surface lift，不用厚重阴影 |
| 文字分级 | `text_primary / secondary / tertiary / disabled` | primary=正文；secondary=次要；tertiary=label/时间戳；disabled=placeholder/极弱 |
| 强调 | `accent` / `accent_dim` / `accent_ink` | accent=唯一主操作；ink=亮色主题下的 accent 文字/链接；dim=选中态底色（极少量） |
| 状态 | `state_{done,running,error}` + `_bg` + `_text` | base=pip/边框；bg=徽章底；text=徽章字 |
| reasoning | `reasoning_border / bg / text` | think 块专用，刻意「次一级」的冷调 |
| user 气泡 | `user_bg / border` | 用户消息，accent 的淡色调 |
| 代码高亮 | `syntax_{key,str,num,keyword,comment,fn,type}` | 语法高亮各色 |
| plan 卡片 | `plan_accent` | Plan 卡片专用，与 reasoning 同系 |

🔴 **双主题铁律**：dark 为默认，light 必须同步有值。新增任何颜色 token 时，dark 与 light 两套都必须给值，不允许只写一套（`theme.rs` 以结构保证：每 token 在两主题各有一值）。

### 2.2 字体

- **sans**（拉丁文 UI、按钮、label）。
- **chinese**：**所有中文内容**（对话、标题、placeholder）。中文必须用它，不能落到 sans 的中文回退。
- **mono**：**主角字体之一**。tool 名、JSON 参数、session id、runtime label/value、kbd。等宽承载"工具感"。
- 数字对齐：表格/统计数字用 tabular-nums。
- 标题层级：产品标题轻微负字距；不做 marketing 式超大 hero 标题。
- label/eyebrow：mono uppercase + 轻微正字距，作为分类信息，不当装饰。

### 2.3 间距与圆角

- 间距基于 **4px 网格**的命名梯度（`space_1…space_12`），值见 `theme.rs`。
- 圆角分级（`radius_sm…xl` + 状态 badge 可用 full）。**控件密度紧凑**（按钮约 8×14、输入约 8×12 的量级），不做巨大 CTA、不做大 pill。

### 2.4 动效

- 时长只用两档：**小元素 ~120ms、面板/列表项 ~200ms**；缓动统一 cubicOut 系。
- **reduced-motion** 优先：动效工厂对「减少动态」偏好自动归零，组件无需各自判断。
- 骨架屏复刻真实布局，替代纯文字「加载中…」，避免加载完成时跳动。
- 弹层/对话框必须有进入 + 退出过渡，消失和出现同等重要。

### 2.5 浮层定位（铁律）

🔴 **任何弹层/浮层禁止把定位方向硬编码进样式**。触发器位置由调用方决定，组件无法预判——硬编码方向必然在某个摆放位置溢出视口。
**规则：浮层打开时按触发器的视口矩形自动选向**（测量后空间不足就翻转：下→上、右对齐→左对齐）。新增浮层一律照此，不为它加「左/右/上/下」方向 prop 让调用方记着选。

---

## 3. 渲染与技术映射（GPUI 特有）

> 本节是设计原则在 GPUI 技术栈上的落点，区别于已退役的 Web 前端的 CSS 实现。

- **Token 落地**：web 的 CSS 变量 → GPUI 的 `theme.rs` `Theme` struct（语义字段）。语义名一一对应，值是唯一事实源。
- **GPU 加速**：文本光栅化（字形 → GPU atlas，平台原生文本栈）与 quad/path 图元走 GPU——div + 文本自动命中，无需额外工作。**设计表达上**：hairline border、surface lift、文本层级都是 GPU 免费项；**昂贵项**是全量列表重建与高频重排。
- **性能即设计**：
  - 长列表（对话流 / session 列表 / 文件树）用**虚拟滚动**（`uniform_list`），只渲染可见行。
  - 流式文本**增量更新**（标 dirty 而非全量重建）；同帧多次变更由 gpui 合并。
  - 渲染函数**借用迭代**，不在每帧做全量数据拷贝（clone）。需要专用数据结构（rope/gap buffer）时按性能需求引入，不预先抽象。
- **术语**：协议侧的 `SessionView`（后端折叠出的**数据快照**）与渲染侧的**元素树**是两个东西——前者是输入数据，后者是 `Render` 产出的 UI 树。代码与文档里不混用「view」一词指代两者。

---

## 4. 对话流（Chat 面板）

逻辑由 `chat.rs` 的 `Row` 渲染模型驱动——**改视觉别动状态机**。

- **user**：accent 淡色气泡/border，中文字体。
- **text**：正文，可读行高。流式时尾部加 accent 闪烁竖条（流式光标）。
- **reasoning**：降级为非卡片。流式中=一行内联状态（`思考中` + 脉冲点）+ 安静的流式正文（muted）；完成后=单行 muted 首行预览（点击就地展开）。视觉权重 ≤ 继承历史，不与 user/text/tool 抢块级节奏。
- **tool（120% 招牌）**：折叠头 = pip + name + status-badge + summary + chevron。三态**颜色 + 形状冗余**：
  - `done` 绿 pip + 绿徽章 + 绿边框
  - `running` 琥珀 pip(涟漪) + spinner + 琥珀徽章 + 脉冲边框
  - `error` 红 pip + 红徽章 + 红边框
  - 展开 = 主呈现（后端 `ToolView` 结构化 view，按 `kind` 分发到 Diff/Code/Terminal/Listing/Markdown/Plain——见 `doc/tool-view.md`，前端只渲染不构建）+ debug 折层（原始 args + model-facing result，全过程透明）。
  - **view 契约**：view 是 UI 的事实通道，可含 result 没有的信息（行号/上下文），但**永不进模型上下文**。
- **乐观 user 消息（pending）**：发送即渲染（即时反馈不可回退），打 pending 标（无 seq）；后端 `TurnEvent::Started` 提交后**按文本对账升级为已提交**（拿到 seq），失败则标红 + 重试。任何乐观行必须有「确认 / 失败」两条出路之一，**绝不允许无限悬挂的孤儿行**。

### 4.1 输入区

- 输入框（focus 时 accent 淡发光）+ 底部 actions。
- 操作只有 **Cancel + Send**。Compact/其它走未来 `/` 命令。
- 下方 `Type / for commands` 提示（`text_disabled`，mono）。
- 状态行：turn incomplete 时显示 `Turn incomplete`（`state_running_text`）。

### 4.2 轮操作层（turn actions）

挂在**用户消息**上的低频操作入口（用户消息 = 一轮对话的起点）。首个动作是 **Fork**（从该轮分支）。

- **克制第一**：默认态**极弱可见**（图标 `text_disabled`，容器低透明），仅当该轮被 hover 时提亮。
- **纯图标无可见文字**；hover 背景只提升一级（surface lift + hairline）。
- **不碰 accent**（accent 只留给主操作）。图标用原创描边字形，不借用现成图标集（§5）。
- **首轮例外**：第一条用户消息不渲染 fork（空上下文 = 等同 new session，无意义）。
- **Fork 惰性语义**：点 fork 不立即建会话——记住「父会话 + 分支点」，仅当下一次发送才真正 fork + 发送。随手点不产生空会话。

### 4.3 继承上下文（inherited context）

分支会话（fork/compaction/reconfiguration）继承了父会话的一段对话，存在 `context_snapshot.json`、不进事件流。在对话流**顶部**把这段历史以**变暗**样式渲染，避免「fork 后像凭空开始」。

- 数据源：session snapshot（新会话无 → 静默略过）。
- 渲染映射：System 丢弃；User→气泡；Assistant→正文；tool→紧凑单行 trace（**不用**三态 ToolBlock，历史不需要 live affordance）。
- **变暗**：整块低透明度（~0.62 量级），读作「之前发生的」，绝不与 live 对话抢注意力。
- **分隔线**：继承块末尾一条 hairline + 居中 mono label（`分支自此处 · inherited context above`），`border_subtle` + `text_tertiary`，无 accent。

### 4.4 卡内审批（in-card approval）

权限门控把一次 `ask` 工具调用挂起等人工决定（`doc/permission.md`）。**没有独立审批卡**——提示附着在被门控的那张 ToolBlock 上：批准了命令自然执行、tool 卡自然显示结果；拒绝了落入 error。审批只是 tool 卡的瞬态。

- **待审态**：卡片即 running 家族（琥珀脉冲 + pip 涟漪），badge 读「等待批准」，强制展开。
- **决定 × 作用域两正交控件**：批准（accent 主操作）/ 拒绝（ghost secondary）一击完成；旁边独立作用域选择器（默认「仅此次」），非默认会把决定 pin 成规则（session 内存 / profile / gateway 落盘），选中非默认时选择器染琥珀色。
- **决议回流**：`Permission::Decided` 提交后只清 pending 标记，终态由配对的 `Tool::Completed/Failed` 驱动（**不乐观更新**）。

### 4.5 待迁移的面板/组件规格

以下规格在 Web 前端已定稿，但对应 GPUI 面板**尚未实现**（见 `migration-plan.md` Phase 3.5+），故未纳入本文档正文。它们的原则与本文档一致，**在各面板实现时迁入并落进正文**；若 `frontend/` 先行删除，需在删除前把对应规格迁入本文档（见 `migration-plan.md` Phase 6）。

| 规格 | 对应面板/组件 | 状态 |
|------|--------------|------|
| Plan 卡片（折叠/展开、步骤 5 态标记、内联 vs Pinned） | 对话流 Plan 卡 | 待实现时迁入 |
| Detail Rail（会话详情栏，INFO/CONTEXT/STATS 三段、「有数据才渲染」） | 会话详情侧栏 | 待实现时迁入 |
| 门控编辑器（permission rules，三层增量规则列表） | 权限配置 UI | 待实现时迁入 |
| 语言工具清单编辑器（LSP/Format 注册表驱动清单） | 设置 → LSP/Format | 待实现时迁入 |
| ModelSelect / PickerSelect（主题化下拉：触发器=当前值、弹层自动选向） | 所有「选一个值」处 | 待实现时迁入 |
| 输入区 Config Pickers（profile/model/effort 三触发器 + 惰性切换） | 对话输入区 | 待实现时迁入 |
| 内容页外壳（页面 padding、surface ladder 决策） | list/monitor 等页 | 待实现时迁入 |

**迁移规则**：迁入时按本文档 §2 的 token 语义重写（不含 CSS 特有表达），并遵守 §2 双主题铁律。

---

## 5. 反 AI slop 禁令（硬清单）

| 禁 | 为什么 | 例外 |
|----|--------|------|
| 紫色大渐变铺底 | "科技感"万能公式，无品牌信息 | 无 |
| emoji 当功能图标 | "不够专业用 emoji 凑"的病 | 无（用纯矢量/描边图标） |
| 圆角卡片 + 左彩色 border accent | 2020-2024 烂大街组合 | 无 |
| 均匀深蓝底 + 通用青紫霓虹 | GitHub-dark 偷懒解（我们的炭黑有性格） | 无 |
| hardcode 颜色/字号/间距 | 见 §2 铁律 | 无 |
| 满屏 accent | 强调色泛滥即失效 | 无（一屏一主操作） |
| 大阴影制造层级 | 深色工具界面会变脏，与 surface ladder 冲突 | 弹层可用 token 化小阴影 |
| pill 圆角泛滥 | 会把工程工具做成消费级 app | 状态 badge / avatar 可用 full radius |

正向：合理留白节奏、tabular-nums、状态冗余表达、hairline border、surface lift。

---

## 6. 改 UI 的标准流程

### 小改（调间距 / 修 bug / 换文案）

1. 直接改，**只用 token，不 hardcode**。
2. 改完对照 §2-§5 自检。
3. `just check` / `just clippy` 过 + 本机起 app 肉眼验关键路径。

### 大改（新面板 / 新组件 / 重排版）

1. 先确认是否真需要——能否复用现有组件/token。
2. 先决定 surface 层级：该区域在 `base/raised/overlay/float` 哪一层？hover/focus 是否只提升一级？
3. 写进代码，token 化，对照 §2-§5。
4. `check` + `clippy` + `test` 全过 + 真机验关键路径（含暗/亮主题）。
5. 值变只改 `theme.rs`；**原则变才改本文档**。

### 验证手段

- 类型/静态：`just check`、`just clippy`（0 警告）。
- 回归：`just test`（组件行为 `simulate_keystrokes` + 布局 `debug_bounds` 断言）。
- 视觉：本机起 app 真机验（headless 截图链路按 Phase 3.2 的 test-support 模式）。

---

## 7. 已知陷阱（踩过的坑，别再犯）

- **不动状态机**：`chat.rs` 的事件折叠/对账/乐观更新逻辑微妙。改对话流只换视觉/布局，别碰 `apply`/对账。
- **中文字体回退**：中文内容忘了指定 chinese 字体会落到 sans 的丑回退。对话相关元素都要显式声明。
- **浮层方向**：见 §2.5 铁律，别硬编码方向。

---

**本文档是持久化文档**，随 GPUI 客户端演化长期维护。它取代了 Web 前端时代的设计文档（后者随 `frontend/` 一并退役，见 `migration-plan.md` Phase 6）。
