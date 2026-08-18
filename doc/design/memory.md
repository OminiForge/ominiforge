<!-- status: current -->
<!-- owner: @OminiForge -->

# Memory 系统

> **新架构定位**：Memory 是跨 session 的长期知识，作为服务插件。组合运行时与拓展机制见 [`runtime-architecture.md`](./runtime-architecture.md)。

## 13. Memory 系统

Memory 系统需要支持 agent 跨 session 记忆。它应与 session 历史区分：session 是完整事实记录，memory 是经过提炼、可检索、可更新的长期知识。

Memory 应支持不同作用域：

- user memory
- project memory
- profile memory
- skill memory
- tool memory
- global memory

Memory 写入应可追溯来源 session，避免无法解释的记忆污染。

