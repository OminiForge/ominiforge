<!-- status: current -->
<!-- owner: @OminiForge -->

# Provider 系统

> **新架构定位**：Provider 经 `ofg-llm` 的 provider 注册表承载（各 provider 为模块/feature）。组合运行时与拓展机制见 [`runtime-architecture.md`](./runtime-architecture.md)。

## 9a. Provider 系统

系统需要支持多个模型 provider，包括 Xiaomi MiMo、OpenAI-compatible provider、主流云模型和自部署模型。

Provider 不应把私有 DTO 泄漏到 core agent。Provider adapter 负责把外部协议转换成内部稳定事件和消息类型。

```text
External provider response
→ provider adapter
→ core AgentEvent / ModelEvent
→ agent loop
```

这样可以避免 agent loop 依赖某个 provider 的 JSON shape，也便于新增 provider。

**Provider 来源与装配解耦**：session 装配（`app::assemble`）不绑定 provider 的*来源*。`ProviderSource` 把「provider 从哪来」显式化——`Configured`（正常路径：解析 `providers.toml` 并 `provider::build` 构建适配器）或 `Injected`（注入一个已构建的 `Arc<dyn Provider>`，跳过配置文件与凭证要求）。注入路径服务于集成测试与本地合成运行：经 `SessionRegistry::new_with_provider` / `LocalProtocol::new_with_provider` 注入 `llm::ScriptedProvider`（按脚本回放 `StreamEvent`，零网络），即可驱动一轮完整 agent 对话做端到端验证。装配本身（工具/沙箱/环境/LSP）对两种来源一视同仁。

