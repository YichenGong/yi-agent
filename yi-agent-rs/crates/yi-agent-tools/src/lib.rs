//! yi-agent-tools: built-in tool implementations.
//!
//! 包含文件系统操作(Read/Write/Edit/Glob/Grep)、Shell 命令执行。
//! 通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

mod context;
mod error;
pub(crate) mod fs;
mod process;
mod process_group;
mod sandbox;
mod shell;
mod skill_tool;
mod web;

pub mod image_prep;
pub mod side_read;
pub mod worktree;

use std::path::{Path, PathBuf};
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
pub use side_read::{
    SIDE_READ_SUPPORTED, SideBlockingPool, SideFileAlias, SideReadAccess, SideRoots,
    StaticSideRoots, register_side_file_tools,
};
pub use skill_tool::SkillTool;
pub use web::config::{
    EngineConfig, Freshness, WebSearchConfig, load_websearch_config, load_websearch_config_from,
    websearch_path,
};
pub use web::{
    BochaSearchProvider, SearchResult, UnconfiguredProvider, WebFetchTool, WebSearchProvider,
    WebSearchTool,
};

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
/// - WebSearchTool 按 `~/.yi-agent/websearch.json` 决定，详见
///   [`register_web_search_from`]。
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
    register_builtin_tools_with_controller_and_env(
        registry,
        roots,
        controller,
        sandbox_writable_roots,
        Vec::new(),
    );
}

/// 同 [`register_builtin_tools_with_controller`]，但把 `session_env` 交给
/// `ToolsContext`，由 `BashTool` 注入 shell 子进程（如当前会话的 thread id）。
pub fn register_builtin_tools_with_controller_and_env(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    controller: SandboxController,
    sandbox_writable_roots: Vec<PathBuf>,
    session_env: Vec<(String, String)>,
) {
    // FS 根集 = 整集（关联根也进工具层）；shell 沙箱的主根仍取 `ctx.root()`（主根），
    // 额外可写根只认调用方给的沙箱汇总项。
    let ctx = Arc::new(ToolsContext::with_roots_and_env(roots, session_env));
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
    register_web_search_from(registry, &websearch_path());
}

/// 按配置注册网页搜索工具。
///
/// 三种情形，都刻意与「静默消失」不同：
/// - `enabled == false`：用户明确关掉了，不注册。
/// - `enabled == true` 且无 key：注册占位实现，调用时给出「未配置」原因。
/// - `enabled == true` 且有 key：注册真实的 Bocha provider。
pub fn register_web_search_from(registry: &mut ToolRegistry, path: &Path) {
    let cfg = web::config::load_websearch_config_from(path);
    if !cfg.enabled {
        return;
    }
    let engine = cfg.active().cloned().unwrap_or_default();
    let default_count = engine.clamped_count();
    let provider: Arc<dyn WebSearchProvider> = match engine.resolved_api_key() {
        Some(key) => Arc::new(BochaSearchProvider::with_options(
            key,
            engine.resolved_base_url(),
            engine.freshness,
        )),
        None => Arc::new(UnconfiguredProvider),
    };
    registry.register(Arc::new(WebSearchTool::with_default_count(
        provider,
        default_count,
    )));
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

    #[tokio::test]
    async fn the_env_carrying_registration_reaches_the_bash_tool() {
        let tmp = tempfile::TempDir::new().unwrap();
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let controller = SandboxController::new(switch, SandboxMode::WorkspaceWrite, false);
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_builtin_tools_with_controller_and_env(
            &mut registry,
            vec![tmp.path().to_path_buf()],
            controller,
            Vec::new(),
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        );
        // 真实调用一次，证明 env 确实随注册入口一路到达 shell 子进程——
        // 只断言「bash 被注册」证明不了本函数承诺的那件事。
        let bash = registry.get("bash").expect("bash is registered");
        let result = bash
            .call(serde_json::json!({ "command": "echo $YI_AGENT_THREAD_ID" }))
            .await;
        assert!(!result.is_error, "bash failed: {:?}", result.content);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("thread-abc"), "env must reach the child: {s}");
        } else {
            panic!("expected text block");
        }
    }

    /// 关掉开关就不该注册任何搜索工具。
    #[test]
    fn a_disabled_config_registers_no_search_tool() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = dir.path().join("websearch.json");
        std::fs::write(&cfg, r#"{"enabled":false}"#).unwrap();
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_web_search_from(&mut registry, &cfg);
        assert!(registry.get("web_search").is_none());
    }

    /// 开着但没有 key：仍注册，调用时回明确原因（而不是工具消失）。
    #[tokio::test]
    async fn enabled_without_a_key_registers_a_tool_that_explains_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = dir.path().join("websearch.json");
        std::fs::write(
            &cfg,
            r#"{"enabled":true,"engines":{"bocha":{"api_key":""}}}"#,
        )
        .unwrap();
        // 保证测试不受本机真实环境变量影响。
        unsafe { std::env::remove_var("BOCHA_API_KEY") };

        let mut registry = yi_agent_core::ToolRegistry::new();
        register_web_search_from(&mut registry, &cfg);
        let tool = registry
            .get("web_search")
            .expect("must still be registered");
        let out = tool.call(serde_json::json!({"query": "x"})).await;
        assert!(out.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &out.content[0] {
            assert!(s.contains("not configured"), "got: {s}");
        } else {
            panic!("expected text block");
        }
    }

    /// 配了 key 就注册真实 provider（用不可达端点，断言错误来自 HTTP 而非
    /// 「未配置」——证明走的是 Bocha 通道）。
    #[tokio::test]
    async fn a_configured_key_registers_the_bocha_provider() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = dir.path().join("websearch.json");
        std::fs::write(
            &cfg,
            r#"{"enabled":true,"engines":{"bocha":{"api_key":"k","base_url":"http://127.0.0.1:1","count":9}}}"#,
        )
        .unwrap();
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_web_search_from(&mut registry, &cfg);
        let tool = registry.get("web_search").expect("registered");
        let out = tool.call(serde_json::json!({"query": "x"})).await;
        assert!(out.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &out.content[0] {
            assert!(!s.contains("not configured"), "should have hit HTTP: {s}");
        } else {
            panic!("expected text block");
        }
    }
}
