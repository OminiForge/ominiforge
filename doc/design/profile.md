<!-- status: current -->
<!-- owner: @OminiForge -->

# Profile 系统

> **新架构定位**：Profile 是运行时配置组合（agent 身份与能力组合），作为数据型拓展。组合运行时与拓展机制见 [`runtime-architecture.md`](./runtime-architecture.md)。

## 14. Profile 系统

Profile 用于定义不同 agent 身份和能力组合。例如 coding agent、research agent、daily assistant。Profile 应组合以下内容：

- system prompt
- model/provider preference
- tool set
- skill set
- permission policy
- sandbox policy
- memory scope
- context policy
- cost policy

Profile 不应复制核心逻辑。它是运行时配置组合。

