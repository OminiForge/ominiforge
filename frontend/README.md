# Ominiforge Web 前端（过渡期方案）

> **⚠️ 这是过渡期方案，最终会被 GPUI 客户端替代。**
>
> 本文档说明 Web 前端的定位、现状和最终命运。

## 定位

Web 前端（SvelteKit）是 **过渡期方案**，在 GPUI 客户端完成前提供可用的用户界面。

**最终形态**：GPUI 客户端是唯一 UI（见 `doc/architecture.md` §3.2），Web 前端将被移除或保留为只读/轻量入口。

## 现状

- ✅ HTTP/SSE 通信（通过 Gateway）
- ✅ Session 管理（创建、fork、删除）
- ✅ Agent 对话（流式响应、工具调用可视化）
- ✅ 监控面板（usage、cost、trace）
- ❌ 多机连接（P2P）
- ❌ Lua 配置
- ❌ 全局 vim 键绑定

## 最终命运

按 `doc/migration-plan.md` Phase 6 的定义：

1. **GPUI 客户端功能完备前**：Web 前端继续维护
2. **GPUI 客户端功能完备后**：
   - Web 前端停止新功能开发
   - 标记为 deprecated
   - 最终决定：完全移除 或 保留为只读/轻量入口

## 开发

```bash
cd frontend
pnpm install
pnpm dev
```

详见 `package.json` 和 SvelteKit 文档。

## 相关文档

- [`doc/migration-plan.md`](../doc/migration-plan.md)：完整迁移计划
- [`doc/architecture.md`](../doc/architecture.md)：系统架构（§3.3 Web 前端过渡期策略）
- [`doc/gateway.md`](../doc/gateway.md)：Gateway HTTP/SSE API
