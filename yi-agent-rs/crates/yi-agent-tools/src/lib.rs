//! yi-agent-tools: built-in tool implementations.
//!
//! 包含文件系统操作(Read/Write/Edit/Glob/Grep)、Shell 命令执行。
//! 通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

mod context;
mod error;
mod fs;
mod process;
mod sandbox;
mod shell;
mod skill_tool;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use yi_agent_core::ToolRegistry;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use process::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessEvent, ProcessKillTool, ProcessListTool,
    ProcessManager, ProcessReadResult, ProcessReadTool, ProcessSelector, ProcessStartOptions,
    ProcessStartResult, ProcessStartTool, ProcessStatus,
};
pub use sandbox::{SandboxMode, SandboxPolicy};
pub use shell::BashTool;
pub use shell::blocklist;
pub use skill_tool::SkillTool;
pub use web::{BochaSearchProvider, SearchResult, WebFetchTool, WebSearchProvider, WebSearchTool};

/// Register managed background process tools with a shared process manager.
pub fn register_process_tools(registry: &mut ToolRegistry, manager: Arc<ProcessManager>) {
    registry.register(Arc::new(ProcessStartTool::new(manager.clone())));
    registry.register(Arc::new(ProcessListTool::new(manager.clone())));
    registry.register(Arc::new(ProcessReadTool::new(manager.clone())));
    registry.register(Arc::new(ProcessKillTool::new(manager)));
}

/// Register all builtin tools into the given registry.
///
/// `root` constrains FS tool operations and shell writes to the given directory.
///
/// Web tools:
/// - WebFetchTool is always registered.
/// - WebSearchTool is only registered if BOCHA_API_KEY env var is set.
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    register_builtin_tools_with_sandbox(registry, root, SandboxMode::WorkspaceWrite, Vec::new());
}

/// Register builtin tools with an explicit shell sandbox policy.
pub fn register_builtin_tools_with_sandbox(
    registry: &mut ToolRegistry,
    root: PathBuf,
    sandbox_mode: SandboxMode,
    extra_writable_roots: Vec<PathBuf>,
) {
    let ctx = Arc::new(ToolsContext::new(root));
    let sandbox = SandboxPolicy::new(sandbox_mode, ctx.root(), extra_writable_roots);
    // A read-only session has no write/edit tool surface, in addition to the
    // process-level file-write denial enforced for shell commands.
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    if sandbox.allows_writes() {
        registry.register(Arc::new(WriteTool::new(ctx.clone())));
        registry.register(Arc::new(EditTool::new(ctx.clone())));
    }
    registry.register(Arc::new(GlobTool::new(ctx.clone())));
    registry.register(Arc::new(GrepTool::new(ctx.clone())));
    registry.register(Arc::new(BashTool::with_sandbox(ctx, sandbox)));

    // Web tools
    registry.register(Arc::new(WebFetchTool::new()));
    if let Some(bocha) = BochaSearchProvider::from_env() {
        registry.register(Arc::new(WebSearchTool::new(Arc::new(bocha))));
    }
    // BOCHA_API_KEY not set: WebSearchTool not registered
}
