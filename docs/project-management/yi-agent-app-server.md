# yi-agent-app-server（GUI app-server）

## 模块说明

`yi-agent-app-server` 是桌面 GUI 的后端进程：一个长驻进程，通过 **stdio 上的
JSON-RPC 2.0**（每行一个 JSON 对象 = JSONL 分帧）与前端通信，采用 codex 风格的
app-server 架构。本 crate 依赖 `yi-agent-core` / `yi-agent-runtime`，把 Agent 运行
事件翻译成稳定的线协议，避免前端直接耦合内部类型。

## 范围边界

**做什么：**
- JSON-RPC 2.0 信封与 yi-agent 线协议类型（请求/响应/通知/错误码）
- stdio JSONL 分帧传输（读一行、写一行，帧大小上限保护）
- 后续：thread/turn 状态机、`AgentEvent` → 通知翻译、权限审批反向请求、CLI 子命令

**不做什么：**
- 不做 TUI（由 `yi-agent-tui` 负责）
- 不做配置加载与 Agent 装配（委托 `yi-agent-runtime`）
- 不在本 crate 实现前端 UI（Tauri/前端另仓）

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-app-server/`；`yi-agent-rs/Cargo.toml` members 含 `crates/yi-agent-app-server`
- [x] JSON-RPC 2.0 信封：`RequestEnvelope` / `ResponseEnvelope` / `RequestId`（num/str 兼容）/ `RpcError`（标准错误码 + `-32010`~`-32012` 业务错误码）— `src/protocol.rs:11` / `src/protocol.rs:17` / `src/protocol.rs:27` / `src/protocol.rs:38`
- [x] 服务端 → 客户端通知 `Notification`（`thread/started`、`turn/started`、`item/*`、`turn/completed`、`thread/tokenUsage/updated`、`error`）与 `TurnStatus` — `src/protocol.rs:107` / `src/protocol.rs:147`
- [x] 会话条目模型 `Item`（`userMessage` / `agentMessage` / `toolCall`）与 `ToolStatus`；`None` 字段序列化时省略 — `src/protocol.rs:155` / `src/protocol.rs:177`
- [x] stdio JSONL 传输：`MessageReader::next_line`（CRLF 归一、EOF 返回 `None`、超 `MAX_FRAME_BYTES` 报错）、`MessageWriter::write_value`（序列化失败只记日志不 panic）— `src/transport.rs:27` / `src/transport.rs:60`
- [ ] thread/turn 状态机 + `AgentEvent` → 通知翻译层
- [ ] 权限审批反向请求闭环（服务端 → 客户端请求 / 客户端响应）
- [ ] CLI `app-server` 子命令（装配 runtime 并驱动 stdio 循环）

**验证命令：** `cargo test -p yi-agent-app-server`（14 个测试）
