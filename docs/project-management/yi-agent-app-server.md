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
- 权限审批反向请求（服务端 → 客户端请求 / 客户端响应）
- CLI 子命令 `yi-agent app-server`（装配 runtime 并驱动 stdio 循环）
- 会话持久化与历史方法（`thread/list` / `thread/listAll` / `thread/resume` / `thread/rename` / `thread/delete`，落 `<每个 thread 的 cwd>/.yi-agent/threads/`）
- 全局「最近目录」索引（`~/.yi-agent/workspaces.json`）与 `workspace/list` / `workspace/add` / `workspace/remove`
- 每个 thread 独立工作目录：`thread/start` 接受 `cwd`，agent 与 `ThreadStore` 按该 cwd 构建

**不做什么：**
- 不做 TUI（由 `yi-agent-tui` 负责）
- 不做配置加载与 Agent 装配（委托 `yi-agent-runtime`）
- 不在本 crate 实现前端 UI（Tauri/前端另仓）

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-app-server/`；`yi-agent-rs/Cargo.toml` members 含 `crates/yi-agent-app-server`
- [x] JSON-RPC 2.0 信封：`RequestEnvelope` / `ResponseEnvelope`（均 `Serialize` + `Deserialize`）/ `RequestId`（num/str 兼容）/ `RpcError`（标准错误码 + `-32010`~`-32012` 业务错误码）— `src/protocol.rs:18` / `src/protocol.rs:28` / `src/protocol.rs:12` / `src/protocol.rs:62`
- [x] 服务端 → 客户端通知 `Notification`（`thread/started`、`turn/started`、`item/*`、`turn/completed`、`thread/tokenUsage/updated`、`error`）、`NotificationEnvelope`（补齐 `jsonrpc:"2.0"` 字段）与 `TurnStatus` — `src/protocol.rs:107` / `src/protocol.rs:150` / `src/protocol.rs:167`
- [x] 会话条目模型 `Item`（`userMessage` / `agentMessage` / `toolCall`）与 `ToolStatus`；`None` 字段序列化时省略 — `src/protocol.rs:175` / `src/protocol.rs:197`
- [x] stdio JSONL 传输：`MessageReader::next_line`（CRLF 归一、EOF 返回 `None`、内容超过 `MAX_FRAME_BYTES`（不含行终止符）报错）、`MessageWriter::write_value`（序列化失败返回错误，不 panic 不静默丢弃）— `src/transport.rs:29` / `src/transport.rs:67`
- [x] `AgentEvent` → 通知翻译层 `translate.rs`（`Translator::on_event`）— `src/translate.rs:174`
- [x] server 主循环 + thread/start（`initialize` / `thread/start` / `config/read` / 错误码 / EOF 退出）— `src/server.rs:50`
- [x] turn/start + turn/interrupt + 每 thread driver task（`turn-<uuid>` 编号、`turn/started`→响应→投递顺序、`-32011`/`-32012`/`-32602` 错误码、`Agent::run()` 之后取 cancel token 保证中断有效、中断信号携带目标 turn id 以丢弃残留、写失败也上报 `Finished` 防 `active_turn_id` 卡死）— `src/server.rs:687` / `src/server.rs:756` / `src/server.rs:1074`
- [x] 权限审批反向请求闭环（`AgentEvent::PermissionRequest` → 服务端反向请求 `item/toolCall/requestApproval`（id 取自进程级 `perm_seq` 计数器，跨 thread 全局唯一）→ 客户端响应 `ClientResponse` → `pending` 登记表路由 → `Decision` 回传 agent；审批等待内的中断按目标 turn id 过滤，超时/中断/畸形取值一律按 `Deny` fail-safe）— `src/protocol.rs:42` / `src/protocol.rs:54` / `src/server.rs:132` / `src/server.rs:843` / `src/server.rs:858` / `src/server.rs:1074`
- [x] CLI `app-server` 子命令（装配 runtime 并驱动 stdio 循环，仅支持 `stdio://`）— `yi-agent-rs/crates/yi-agent/src/config.rs:154`（`Command::AppServer` 变体）/ `yi-agent-rs/crates/yi-agent/src/main.rs:74`（`ensure_stdio_listen`）/ `yi-agent-rs/crates/yi-agent/src/main.rs:85`（`run_app_server`）
- [x] 会话持久化 + 历史方法（`thread_store.rs`：每 thread 一个只追加 `.jsonl` + 一个可变 `.meta.json`，落 `<workdir>/.yi-agent/threads/`；`thread/start` 分配 `thread-<uuid>` 并写 meta；driver 每 turn 落盘最终 Item + 完整 session `Message` 快照；新增 `thread/list` / `thread/resume`（回放 + `Agent::with_session` 恢复上下文）/ `thread/rename` / `thread/delete`；删除活跃 thread 时先中断并等 driver 落盘再删，避免 `create(true)` 复活文件）— `src/thread_store.rs:1` / `src/server.rs:286`（thread/list）/ `src/server.rs:361`（thread/start）/ `src/server.rs:459`（thread/resume）/ `src/server.rs:611`（thread/rename）/ `src/server.rs:654`（thread/delete）/ `src/server.rs:1109`（driver 落盘）
- [x] 工作目录选择：全局「最近目录」索引 + 每 thread 独立 cwd + 跨目录列对话 — 索引 `src/workspace_index.rs:17`（`WorkspaceIndex`）/ `src/workspace_index.rs:23`（`default_path` = `$HOME/.yi-agent/workspaces.json`，损坏按空 + stderr）；RPC `workspace/list` `src/server.rs:229` / `workspace/add` `src/server.rs:238`（canonicalize + 校验目录，非法 → `-32602`）/ `workspace/remove` `src/server.rs:269`；`thread/start` 接受 `cwd` `src/server.rs:361`（`resolve_thread_cwd` `src/server.rs:1157`，缺省回退 `cfg.workdir`）；agent 工厂按 cwd 克隆 `cfg` 覆盖 `workdir`（`run` 传给 `run_with` 的 agent 工厂闭包 `src/server.rs:63`）；`ThreadStore` 按 thread 的 cwd 构建、`ThreadSession` 持有该 `Arc`（`src/session.rs:27`）以共享 `meta_lock`，`store_lookup` `src/server.rs:1140` 供 rename/delete/resume 定位（缺省回退 `store_for` `src/server.rs:1129`）；`thread/listAll` 按索引目录分组 `src/server.rs:316`（失效目录跳过、不深扫）；验证 `cargo test -p yi-agent-app-server --lib workspace_`、`cargo test -p yi-agent-app-server --lib thread_list_all`、`cargo test -p yi-agent-app-server --lib thread_ops_target_thread_cwd`
- [x] 用量通知携带 prompt-cache token — `thread/tokenUsage/updated` 增加 `cache_creation_input_tokens` / `cache_read_input_tokens`（`src/protocol.rs:148`）；translate 层 `src/translate.rs:291` 经 `UsageSnapshot::merge`（`src/translate.rs:52`）合并拆分事件（Anthropic `message_start` 带 input/cache、`message_delta` 带 output；新调用整体替换、同一调用内按字段补齐），拼回本轮完整快照；`thread/resume` 回放 `src/server.rs:584` 带上；落盘 `TurnUsage` `src/thread_store.rs:29` 补两字段并 `#[serde(default)]` 兼容旧日志

**验证命令：** `cargo test -p yi-agent-app-server`（130 个测试）+ `cargo test -p yi-agent --bin yi-agent stdio`（CLI 子命令 4 个测试）
