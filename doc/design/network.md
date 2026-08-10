<!-- status: current -->
<!-- owner: @OminiForge -->

# 网络通信

网络通信系统负责 GPUI 客户端与 Core 之间的通信，以及多机之间的连接。

## 1. 设计目标

- **统一接口**：ClientProtocol trait，编译期类型安全
- **可插拔**：底层传输可替换，不影响上层
- **高性能**：本地模式零网络开销，远程模式低延迟
- **自动切换**：Direct → Tunnel → P2P，自动降级
- **设备发现**：mDNS（局域网）+ Relay（广域网）

## 2. ClientProtocol trait

ClientProtocol 是 GPUI 客户端与 Core 之间的统一通信接口。

### 2.1 接口定义

ClientProtocol 定义了客户端与 Core 交互的所有操作：

**Session 管理**：
- 列出 session
- 创建 session
- 获取 session 详情
- Fork session
- 删除 session

**消息交互**：
- 发送消息
- 取消 turn
- 压缩 context

**事件订阅**：
- 订阅 session 事件流
- 订阅状态事件流

**监控**：
- 获取 session metrics
- 列出所有 metrics

**配置**：
- 获取配置
- 更新配置

**连接状态**：
- 查询当前连接状态

### 2.2 实现

ClientProtocol 有多个实现：

**LocalProtocol**：
- 直接链接 ominiforge-core 作为库
- 零网络开销，零序列化
- 编译期类型安全
- 最高性能

**WebSocketProtocol**：
- 通过 WebSocket 连接远程 Gateway
- 双向通信，单一连接
- JSON 消息（可读性好，调试方便）
- 低延迟（无需每次建立连接）

**QuicProtocol**（未来）：
- 通过 QUIC 连接远程 Gateway
- 二进制序列化（更高效）
- QUIC 传输（比 TCP 快，内置加密）
- 多路复用（多个 stream 共享一个连接）

## 3. 本地模式

本地模式是 GPUI 客户端的主要使用场景（台式机本地运行）。

### 3.1 架构

本地模式下，GPUI 客户端直接链接 ominiforge-core 作为库。

**优势**：
- 零网络开销
- 零序列化/反序列化
- 编译期类型安全
- 最高性能

**实现**：
- LocalProtocol 直接调用 Core 的 Service
- 无 IPC，无网络
- 直接内存访问

### 3.2 使用场景

- 台式机本地运行
- 单机使用
- 开发和测试

## 4. 远程模式

远程模式是 GPUI 客户端连接远程 Gateway 的场景。

### 4.1 WebSocket 协议

远程模式的第一阶段使用 WebSocket。

**消息格式**：
- JSON 消息（可读性好，调试方便）
- 请求-响应模式（同步操作）
- 流式模式（事件订阅）

**连接管理**：
- 单一 WebSocket 连接
- 心跳保活
- 断线重连

### 4.2 QUIC 协议（未来）

远程模式的未来优化使用 QUIC。

**优势**：
- 二进制序列化（更高效）
- QUIC 传输（比 TCP 快，内置加密）
- 多路复用（多个 stream 共享一个连接）
- 0-RTT 连接建立

**实现**：
- 基于 `iroh` crate
- 与 P2P 传输统一

## 5. 多机连接

### 5.1 ConnectionManager

ConnectionManager 管理多机连接，支持多种传输，自动切换。

**传输类型**：
- Direct：局域网直连（最低延迟）
- Tunnel：Cloudflare Tunnel（兼容性好）
- P2P：iroh（QUIC，高性能）

**连接状态机**：
- Disconnected：未连接
- Connecting：连接中
- Connected：已连接（Direct/Tunnel/P2P）

**自动切换**：
- 优先 Direct（局域网直连）
- 降级 Tunnel（Cloudflare Tunnel）
- 升级 P2P（iroh，QUIC）

### 5.2 设备发现

**局域网发现**：
- mDNS/bonjour 自动发现
- 显示可用设备列表
- 自动连接（可选）

**广域网发现**：
- Relay server 注册/发现
- 手动添加 peer 地址
- 通过 Cloudflare Tunnel 连接

### 5.3 权限管理

权限模型见 §7.3（连接 ≠ 授权、细粒度权限分级）。

## 6. Gateway 集成

### 6.1 Gateway 角色

Gateway 是 GPUI 客户端远程模式的后端。

**职责**：
- 接收 GPUI 客户端的连接
- 转发请求到 Core
- 推送事件到 GPUI 客户端
- 管理多客户端连接

### 6.2 WebSocket endpoint

Gateway 添加 WebSocket endpoint（与 HTTP/SSE 并存）。

**endpoint 设计**：
- `/ws`：WebSocket 连接
- `/api/*`：HTTP API（Web 前端过渡期保留）
- `/healthz`：健康检查

**消息协议**：
- JSON 消息（与 HTTP API 一致）
- 请求-响应模式
- 流式模式（事件订阅）

### 6.3 QUIC endpoint（未来）

Gateway 未来可以添加 QUIC endpoint。

**优势**：
- 更高性能
- 更低延迟
- 内置加密

**实现**：
- 基于 `iroh` crate
- 与 P2P 传输统一

## 7. 安全考虑

### 7.1 认证

**本地模式**：
- 无需认证（本地信任）

**远程模式**：
- Bearer token 认证
- Token 存储在 secret store
- Token 可以通过环境变量或配置文件提供

### 7.2 加密

**WebSocket**：
- 可以通过 wss://（WebSocket over TLS）
- TLS 由反向代理终止（caddy/nginx）

**QUIC**：
- 内置加密（TLS 1.3）
- 无需额外配置

### 7.3 权限

**连接 ≠ 授权**：连接成功不代表可以操作，远程操作需 token 认证。

**细粒度权限**：
- 只读权限：查看 session、监控
- 操作权限：发送消息、创建 session
- 管理权限：修改配置、删除 session

**权限配置**：
- per-peer 配置（`allow_remote_control`）
- per-session 配置
- 默认权限策略

