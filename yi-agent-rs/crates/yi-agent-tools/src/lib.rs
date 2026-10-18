//! yi-agent-tools: built-in tool implementations.
//!
//! 包含文件系统操作(Read/Write/Edit/Glob/Grep)、Shell 命令执行。
//! 通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

mod context;
mod error;
mod fs;
mod process;
mod process_group;
mod sandbox;
mod shell;
mod skill_tool;
mod web;

pub mod image_prep;
pub mod worktree;

use std::path::PathBuf;
use std::sync::Arc;

use yi_agent_core::ToolRegistry;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{EditTool, GlobTool, GrepTool, ReadDocumentTool, ReadTool, ViewImageTool, WriteTool};
pub use process::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessEvent, ProcessKillTool, ProcessListTool,
    ProcessManager, ProcessReadResult, ProcessReadTool, ProcessSelector, ProcessStartOptions,
    ProcessStartResult, ProcessStartTool, ProcessStatus,
};
pub use sandbox::{SandboxController, SandboxMode, SandboxPolicy};
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
    register_builtin_tools_with_sandbox(
        registry,
        vec![root],
        SandboxMode::WorkspaceWrite,
        Vec::new(),
    );
}

/// Register builtin tools with an explicit shell sandbox policy.
///
/// `roots` 是本次会话的 **FS 根集**（`roots[0]` = 主根）：`read`/`write`/`edit`/
/// `glob`/`grep` 的逃逸检查与相对路径解析都以整集为准（关联根里的文件与主根同等
/// 可达）。
///
/// `sandbox_writable_roots` 是 **shell 沙箱**的额外可写根，与 `roots` 刻意分开：
/// worker 路径会把 git 内部目录放进去，那些目录必须能让 `bash` 写，却**不能**因此
/// 变成 FS 工具的写根。
pub fn register_builtin_tools_with_sandbox(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    sandbox_mode: SandboxMode,
    sandbox_writable_roots: Vec<PathBuf>,
) {
    let controller = SandboxController::new(
        yi_agent_core::autonomy::YoloSwitch::new(false),
        sandbox_mode,
        false,
    );
    register_builtin_tools_with_controller(registry, roots, controller, sandbox_writable_roots);
}

/// Register builtin tools sharing one externally-owned sandbox controller.
///
/// The same controller instance backs both the shell tool and any process
/// manager built alongside it, so a live YOLO switch flips every execution
/// path at once instead of drifting between independent policies.
pub fn register_builtin_tools_with_controller(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    controller: SandboxController,
    sandbox_writable_roots: Vec<PathBuf>,
) {
    // FS 根集 = 整集（关联根也进工具层）；shell 沙箱的主根仍取 `ctx.root()`（主根），
    // 额外可写根只认调用方给的沙箱汇总项。
    let ctx = Arc::new(ToolsContext::with_roots(roots));
    let sandbox = SandboxPolicy::with_controller(ctx.root(), sandbox_writable_roots, controller);
    // A read-only session has no write/edit tool surface, in addition to the
    // process-level file-write denial enforced for shell commands.
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    // Image viewing is read-only, so it stays available in read-only sessions.
    registry.register(Arc::new(ViewImageTool::new(ctx.clone())));
    // Document reading is read-only, so it stays available in read-only sessions too.
    registry.register(Arc::new(ReadDocumentTool::new(ctx.clone())));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册入口按**根集**收参：主根之外的关联根同样进 FS 工具层（Task 3 的
    /// `ToolsContext::with_roots`），而 shell 沙箱的额外可写根另走一个参数。
    #[test]
    fn builtin_registration_accepts_a_root_set() {
        let primary = tempfile::TempDir::new().unwrap();
        let secondary = tempfile::TempDir::new().unwrap();
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_builtin_tools_with_sandbox(
            &mut registry,
            vec![primary.path().to_path_buf(), secondary.path().to_path_buf()],
            SandboxMode::WorkspaceWrite,
            vec![secondary.path().to_path_buf()],
        );
        let names: Vec<String> = registry.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"write".to_string()));
    }
}
