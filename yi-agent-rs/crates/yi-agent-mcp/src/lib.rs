//! yi-agent-mcp: MCP (Model Context Protocol) client.
//!
//! 连接外部 MCP server,发现远端工具,通过实现 `yi-agent-core` 的 `Tool`
//! trait 把远端 MCP 工具接入 agent。

mod cache;
mod config;
mod manager;
mod naming;
mod tool;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tracing::warn;
use yi_agent_core::ToolRegistry;

pub use cache::CachedTool;
pub use config::{McpConfig, ServerConfig};
pub use manager::McpManager;
pub use tool::McpTool;

/// Load MCP config, build the manager, probe missing caches, and register tools.
///
/// Returns `Ok(None)` when there is no `.yi-agent/mcp.json`.
/// Never fails the caller on a single bad server: it warns and continues.
///
/// Safe to call both outside a tokio runtime (headless/TUI startup) and inside
/// one (the app-server's async main loop); see [`run_blocking`].
pub fn register_mcp_tools(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Option<Arc<McpManager>>> {
    let Some(cfg) = McpConfig::load(workdir)? else {
        return Ok(None);
    };

    let manager = run_blocking(move || McpManager::load_and_probe(cfg, workdir))?;

    for tool in manager.enabled_tools() {
        registry.register(tool);
    }
    for (name, ok) in manager.status() {
        if !ok {
            warn!(server = %name, "MCP server not active");
        }
    }
    Ok(Some(manager))
}

/// Run an async operation to completion from a synchronous caller.
///
/// Must never call `block_on` on an ambient runtime: that panics when invoked
/// from within an async execution context. When we are already inside a tokio
/// runtime (the app-server path) the work runs on a dedicated thread with its
/// own runtime; otherwise (headless/TUI path) a fresh runtime on this thread is
/// enough.
///
/// `std::thread::scope` lets the dedicated thread borrow `workdir`/`cfg` from
/// the caller instead of forcing owned copies into a `'static` `spawn`.
fn run_blocking<F, Fut, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<T>> + Send,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(f())
                })
                .join()
                .expect("MCP probe thread panicked")
        }),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(f())
        }
    }
}
