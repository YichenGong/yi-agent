//! yi-agent-mcp: MCP (Model Context Protocol) client.
//!
//! 连接外部 MCP server,发现远端工具,通过实现 `yi-agent-core` 的 `Tool`
//! trait 把远端 MCP 工具接入 agent。

mod cache;
mod config;
mod manager;
mod naming;
mod tool;

pub use config::{McpConfig, ServerConfig};
pub use manager::McpManager;
pub use tool::McpTool;
