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
