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
- 后续：权限审批反向请求、CLI 子命令

**不做什么：**
- 不做 TUI（由 `yi-agent-tui` 负责）
- 不做配置加载与 Agent 装配（委托 `yi-agent-runtime`）
- 不在本 crate 实现前端 UI（Tauri/前端另仓）

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-app-server/`；`yi-agent-rs/Cargo.toml` members 含 `crates/yi-agent-app-server`
- [x] JSON-RPC 2.0 信封：`RequestEnvelope` / `ResponseEnvelope`（均 `Serialize` + `Deserialize`）/ `RequestId`（num/str 兼容）/ `RpcError`（标准错误码 + `-32010`~`-32012` 业务错误码）— `src/protocol.rs:18` / `src/protocol.rs:28` / `src/protocol.rs:12` / `src/protocol.rs:39`
- [x] 服务端 → 客户端通知 `Notification`（`thread/started`、`turn/started`、`item/*`、`turn/completed`、`thread/tokenUsage/updated`、`error`）、`NotificationEnvelope`（补齐 `jsonrpc:"2.0"` 字段）与 `TurnStatus` — `src/protocol.rs:84` / `src/protocol.rs:127` / `src/protocol.rs:144`
- [x] 会话条目模型 `Item`（`userMessage` / `agentMessage` / `toolCall`）与 `ToolStatus`；`None` 字段序列化时省略 — `src/protocol.rs:152` / `src/protocol.rs:174`
- [x] stdio JSONL 传输：`MessageReader::next_line`（CRLF 归一、EOF 返回 `None`、内容超过 `MAX_FRAME_BYTES`（不含行终止符）报错）、`MessageWriter::write_value`（序列化失败返回错误，不 panic 不静默丢弃）— `src/transport.rs:29` / `src/transport.rs:67`
- [x] `AgentEvent` → 通知翻译层 `translate.rs`（`Translator::on_event`）— `src/translate.rs:36`
- [x] server 主循环 + thread/start（`initialize` / `thread/start` / `config/read` / 错误码 / EOF 退出）— `src/server.rs:33`
- [x] turn/start + turn/interrupt + 每 thread driver task（`turn-{n}` 编号、`turn/started`→响应→投递顺序、`-32011`/`-32012`/`-32602` 错误码、`Agent::run()` 之后取 cancel token 保证中断有效、中断信号携带目标 turn id 以丢弃残留、写失败也上报 `Finished` 防 `active_turn_id` 卡死）— `src/server.rs:233` / `src/server.rs:303` / `src/server.rs:383`
- [ ] 权限审批反向请求闭环（服务端 → 客户端请求 / 客户端响应）
- [ ] CLI `app-server` 子命令（装配 runtime 并驱动 stdio 循环）

**验证命令：** `cargo test -p yi-agent-app-server`（63 个测试）
