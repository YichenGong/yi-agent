# yi-agent-mcp

## 模块说明

yi-agent 的 Model Context Protocol（MCP）客户端 crate。目标是连接 MCP server、发现远端工具，并通过 `yi-agent-core::Tool` 将其暴露给 agent。

## 范围边界

**做什么：**
- MCP server 连接与配置
- 远端工具发现、调用和结果映射
- 多 server 生命周期管理

**不做什么：**
- 不实现 MCP server
- 不在本 crate 实现内置文件或 Shell 工具

## Features

- [x] MCP client 与远端 Tool 适配
  - 配置解析：`yi-agent-rs/crates/yi-agent-mcp/src/config.rs`（`.yi-agent/mcp.json`，兼容 Claude Desktop schema）
  - 工具命名：`yi-agent-rs/crates/yi-agent-mcp/src/naming.rs`（`mcp__{server}__{tool}`，字符清洗 + 64 字节截断）
  - schema 缓存：`yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（`.yi-agent/mcp-cache.json`，按 `ServerConfig` SHA-256 指纹失效）
  - 懒连接 / 调用 / 重连 / 关闭：`yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`
  - Tool 适配与安全注解映射：`yi-agent-rs/crates/yi-agent-mcp/src/tool.rs`
  - 注册入口：`yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` 的 `register_mcp_tools`
  - 运行时开关：`/mcp` slash 命令（`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`）+ agent 热刷新（`ControlCommand::McpRefresh`，`yi-agent-rs/crates/yi-agent/src/main.rs`）
  - 验证：`cargo test -p yi-agent-mcp`（含 duplex 进程内协议往返测试 `manager::tests::probe_then_call_over_duplex`）
- [x] Streamable HTTP transport（仅 POST + JSON）
  - 配置：`ServerConfig` 增加 `type`（`"stdio"` 默认 / `"http"`）、`url`、`headers`；`transport()` 集中校验
  - 展开：http header 值支持 `${ENV_VAR}`，未定义变量丢弃该 header 并 warn；缓存指纹以未展开原文参与
  - 接线：`manager.rs` 的 `connect()` 按 `Transport` 分派 `TokioChildProcess` / `StreamableHttpClientTransport`
  - 指纹：`cache.rs` 缓存指纹扩展为覆盖 `kind`/`url`/`headers`（排除 `enabled`）
  - 验证：`cargo test -p yi-agent-mcp --test http_transport`（wiremock 往返）+ `-- --ignored`（真实 horizon 端点）
- [x] 设置页只读接缝
  - 公开视图：`yi-agent-rs/crates/yi-agent-mcp/src/lib.rs:29` 的 `McpCacheEntry` 与 `:40` 的 `cached_tools_for`（只回名字/描述/read_only，不带 `input_schema`；指纹不匹配回 `None`）
  - RPC：`mcp/list`（`yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`），`Observe` 档，读 `.yi-agent/mcp.json` + `.yi-agent/mcp-cache.json`；面板见 [desktop](./desktop.md)
  - 验证：`cargo test -p yi-agent-app-server --lib mcp_rpc`、`cargo test -p yi-agent-app-server mcp_list_is_available_at_observe_scope`
- [x] 会话内自助配置（`mcp_reload`）
  - 工具：`yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs`，四个 action（`add`/`enable`/`disable`/`remove`），结构化参数，元数据 `requires_confirmation: true`
  - 纯逻辑：`yi-agent-rs/crates/yi-agent-mcp/src/reload.rs`（名字规则、`${VAR}` 必须已定义、header 名校验、add 重名需 `overwrite`；失败不改配置）
  - 热重载：`yi-agent-rs/crates/yi-agent-mcp/src/manager.rs` 的 `McpManager::reload`（只探测新增/指纹变化的 server；单个失败原文进 `ServerOutcome::error`，不吞）、`config_changed_on_disk`（每轮变更检测）
  - 原子写盘：`McpConfig::save`（临时文件 + rename，见 `config.rs`）
  - 孤儿清理：`refresh_registry` 按 `last_registered` drain，删除 server 后其工具不再残留
  - 注册：`register_mcp_reload_tool` **无条件注册**（无 `mcp.json` 时也有入口），接在 `yi-agent-runtime/src/bootstrap.rs` 与 `yi-agent/src/main.rs` 两处
  - 权限：`mcp_reload` 始终 `NeedConfirm`，**不受 yolo 豁免**（`yi-agent-rs/crates/yi-agent-core/src/permission.rs`）
  - 生效时机：下一轮生效（不改 `ToolRegistry` 为 `RwLock`）；返回文案固定写「新工具从下一条消息起可用」
  - 验证：`cargo test -p yi-agent-mcp`（72 个 lib 测试，含 `reload_does_not_reprobe_an_unchanged_server`、`reload_reprobes_a_changed_server_and_reports_the_error`、`reload_removes_a_deleted_server`、`config_changed_on_disk_*`、`reload_tool::tests::*`）
