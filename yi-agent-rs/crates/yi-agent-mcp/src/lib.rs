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

use anyhow::{Result, anyhow};
use yi_agent_core::ToolRegistry;

pub use cache::CachedTool;
pub use config::{McpConfig, ServerConfig, ServerKind, Transport, config_path};
pub use manager::McpManager;
pub use tool::McpTool;

/// 缓存里一个远端工具的**只读视图**，供设置页展示。
///
/// 刻意不带 `input_schema`：那份 schema 可能有上千字，面板只展示名字与一行描述，
/// 把它下发既无用又撑大响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCacheEntry {
    pub name: String,
    pub description: Option<String>,
    pub read_only: bool,
}

/// 读 `workdir` 下 `.yi-agent/mcp-cache.json` 里 `name` 的工具清单。
///
/// 指纹与 `cfg` 不匹配或缓存缺失时返回 `None`——调用方据此区分「尚未探测」与
/// 「探测过但没有工具」（后者是 `Some(vec![])`）。判定逻辑复用同源的
/// `cache::McpCache::get_for`，故面板看到的清单与真正注册进工具集的那份一致。
pub fn cached_tools_for(
    workdir: &Path,
    name: &str,
    cfg: &ServerConfig,
) -> Option<Vec<McpCacheEntry>> {
    let cache = cache::McpCache::load(workdir);
    cache.get_for(name, cfg).map(|tools| {
        tools
            .iter()
            .map(|t| McpCacheEntry {
                name: t.name.clone(),
                description: t.description.clone(),
                read_only: t.read_only,
            })
            .collect()
    })
}

/// 测试支撑：让下游 crate 的测试能写入真实的缓存文件，而不必复刻 SHA-256 指纹
/// 格式（那会让下游测试对私有实现脆弱）。
#[doc(hidden)]
pub mod test_support {
    use super::*;

    /// 一个挂在 `workdir` 上的缓存（尚未 `save`）。
    pub fn cache_with(workdir: &Path) -> cache::McpCache {
        cache::McpCache::load(workdir)
    }

    /// 造一条缓存工具记录。
    pub fn cached_tool(name: &str, read_only: bool) -> cache::CachedTool {
        cache::CachedTool {
            name: name.to_string(),
            description: Some(format!("{name} 描述")),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
        }
    }
}

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
                .map_err(|_| anyhow!("MCP probe thread panicked"))?
        }),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(f())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CachedTool, McpCache};
    use crate::config::ServerConfig;
    use std::collections::BTreeMap;

    /// A cache hit must register the cached tools without spawning a process.
    /// The configured command does not exist, so any probe attempt would fail
    /// and register nothing; a passing assertion therefore proves the cache was
    /// consulted. Regression guard for the cache-probe wiring.
    #[test]
    fn cache_hit_registers_tools_without_probing() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let command = "/nonexistent/definitely-not-here";
        let sc = ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(command.into()),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        };

        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "read_file".into(),
                description: Some("reads a file".into()),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
        );
        cache.save(dir.path()).unwrap();

        std::fs::write(
            agent_dir.join("mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"/nonexistent/definitely-not-here"}}}"#,
        )
        .unwrap();

        let mut reg = ToolRegistry::new();
        let mgr = register_mcp_tools(&mut reg, dir.path()).unwrap();
        assert!(mgr.is_some());
        assert!(
            reg.get("mcp__fs__read_file").is_some(),
            "cached tool should be registered without probing; got {:?}",
            reg.names()
        );
    }
}
