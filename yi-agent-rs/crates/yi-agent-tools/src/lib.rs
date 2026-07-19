//! yi-agent-tools: built-in tool implementations.
//!
//! 包含文件系统操作(Read/Write/Edit/Glob/Grep)、Shell 命令执行。
//! 通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

mod context;
mod error;
mod fs;

pub use context::ToolsContext;
pub use error::ToolsError;
