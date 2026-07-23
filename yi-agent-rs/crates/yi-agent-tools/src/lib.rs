//! yi-agent-tools: built-in tool implementations.
//!
//! 包含文件系统操作(Read/Write/Edit/Glob/Grep)、Shell 命令执行。
//! 通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

mod context;
mod error;
mod fs;
mod shell;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use yi_agent_core::ToolRegistry;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use shell::BashTool;
pub use web::{
    BochaSearchProvider, SearchResult, WebFetchTool, WebSearchProvider, WebSearchTool,
};

/// Register all builtin tools into the given registry.
///
/// `root` constrains FS tool operations to the given directory.
/// Shell tools use it as initial cwd but do not restrict `sh -c` operations
/// to within root (system-level isolation requires sandbox, which is future work).
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    let ctx = Arc::new(ToolsContext::new(root));
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    registry.register(Arc::new(WriteTool::new(ctx.clone())));
    registry.register(Arc::new(EditTool::new(ctx.clone())));
    registry.register(Arc::new(GlobTool::new(ctx.clone())));
    registry.register(Arc::new(GrepTool::new(ctx.clone())));
    registry.register(Arc::new(BashTool::new(ctx)));
}
