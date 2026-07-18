//! yi-agent-mcp: MCP (Model Context Protocol) client.
//!
//! 独立成 crate 主要是因为 MCP SDK 依赖较重,且需要支持连接多个
//! MCP server。通过实现 `yi-agent-core` 的 `Tool` trait 把远端 MCP
//! 工具接入 agent。
