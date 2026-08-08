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

- [ ] MCP client 与远端 Tool 适配 — 当前仅有 `crates/yi-agent-mcp/src/lib.rs` crate 骨架，尚无 transport、工具发现或调用实现
