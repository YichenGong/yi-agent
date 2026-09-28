//! `McpManager`: owns configured servers, lazily connects, and dispatches calls.
//!
//! Connections are established on first `call_tool` and cached per server, so a
//! configured-but-unused server never spawns a child process. The master switch
//! and per-server switches are plain atomics, letting the UI toggle them from a
//! different task without locking.
//!
//! The connect handshake is bounded by `CONNECT_TIMEOUT` and each call by
//! `CALL_TIMEOUT`. Calls do not hold the per-server lock, so concurrent calls to
//! one server are not serialized; if a call fails at the transport layer the
//! connection is dropped and the call is retried once on a fresh connection.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};
use yi_agent_core::{Tool, ToolRegistry, ToolResult};

use crate::cache::{CachedTool, McpCache};
use crate::config::{McpConfig, ServerConfig};
use crate::tool::McpTool;

/// Upper bound on a single remote tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Upper bound on spawning a server and completing the MCP `initialize`
/// handshake. Bounds a server that starts but never finishes handshaking.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on the `tools/list` half of a cold cache probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Upper bound on reaping a probed server's child process. A wedged `cancel()`
/// must not stall startup; if it fires we fall back to `RunningService`'s drop
/// path, which cancels the connection.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// A live client connection to one MCP server.
type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// One configured server: its config, switch state, advertised tools, and the
/// lazily-established connection. The `Arc` lets a call clone the client and
/// release the lock before awaiting the (long) network call; the lock is held
/// only to (re)connect and to swap the cached handle.
struct ServerEntry {
    config: ServerConfig,
    enabled: AtomicBool,
    tools: Vec<CachedTool>,
    client: Mutex<Option<Arc<Client>>>,
}

/// Owns configured MCP servers and their connections.
pub struct McpManager {
    servers: HashMap<String, ServerEntry>,
    master_enabled: AtomicBool,
}

impl McpManager {
    /// A manager with no servers and the master switch off.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            servers: HashMap::new(),
            master_enabled: AtomicBool::new(false),
        })
    }

    /// Build a manager from config, probing servers whose schema cache is
    /// missing or stale. Never fails on a single server: a probe failure is
    /// logged and that server's tool list is left empty.
    ///
    /// Successful probes are written back to the on-disk cache once, after the
    /// loop, so a partially-probed run does not leave the cache half-written.
    ///
    /// Servers are probed sequentially, so worst-case startup is
    /// `N * (CONNECT_TIMEOUT + PROBE_TIMEOUT + REAP_TIMEOUT)` (about `N * 55s`).
    /// Concurrent probing is a possible future improvement; it is intentionally
    /// not done here to keep startup simple and bounded.
    ///
    /// The returned `Result` only errors on runtime/setup failure, never on an
    /// individual server's probe.
    pub async fn load_and_probe(cfg: McpConfig, workdir: &Path) -> Result<Arc<Self>> {
        let mut cache = McpCache::load(workdir);
        let mut servers = HashMap::new();
        let mut cache_dirty = false;

        for (name, sc) in &cfg.mcp_servers {
            let tools = match cache.get_for(name, sc) {
                Some(t) => t.to_vec(),
                None => match probe_tools(sc).await {
                    Ok(probed) => {
                        cache.put(name, sc, probed.clone());
                        cache_dirty = true;
                        probed
                    }
                    Err(e) => {
                        warn!(server = %name, error = %e, "MCP probe failed; server unavailable");
                        Vec::new()
                    }
                },
            };
            servers.insert(
                name.clone(),
                ServerEntry {
                    config: sc.clone(),
                    enabled: AtomicBool::new(sc.enabled),
                    tools,
                    client: Mutex::new(None),
                },
            );
        }

        if cache_dirty {
            if let Err(e) = cache.save(workdir) {
                warn!(error = %e, "failed to persist MCP cache");
            }
        }

        Ok(Arc::new(Self {
            servers,
            master_enabled: AtomicBool::new(cfg.enabled),
        }))
    }

    /// A server is effective only when both the master switch and its own
    /// switch are on.
    pub fn is_effective(&self, server: &str) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
            && self
                .servers
                .get(server)
                .map(|e| e.enabled.load(Ordering::Relaxed))
                .unwrap_or(false)
    }

    pub fn set_master(&self, on: bool) {
        self.master_enabled.store(on, Ordering::Relaxed);
    }

    pub fn master(&self) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
    }

    /// Toggle a single server. Errors if the server is not configured.
    pub fn set_server(&self, server: &str, on: bool) -> Result<()> {
        let entry = self
            .servers
            .get(server)
            .ok_or_else(|| anyhow!("unknown MCP server: {server}"))?;
        entry.enabled.store(on, Ordering::Relaxed);
        Ok(())
    }

    /// Configured server names, sorted for stable display.
    pub fn server_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.servers.keys().cloned().collect();
        v.sort();
        v
    }

    /// `(server name, effective on/off)` for every configured server.
    pub fn status(&self) -> Vec<(String, bool)> {
        self.server_names()
            .into_iter()
            .map(|n| {
                let on = self.is_effective(&n);
                (n, on)
            })
            .collect()
    }

    /// All qualified tool names this manager could register, regardless of
    /// switch state. Used to unregister tools when a server is toggled.
    ///
    /// The server name is the config map key, matching `enabled_tools`, not the
    /// command (`npx`) which is only how the server is launched.
    pub fn all_tool_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flat_map(|(name, e)| {
                e.tools
                    .iter()
                    .map(move |t| crate::naming::qualified_name(name, &t.name))
            })
            .collect()
    }

    /// Adapters for every tool on currently-effective servers.
    pub fn enabled_tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for (name, entry) in &self.servers {
            if !self.is_effective(name) {
                continue;
            }
            for t in &entry.tools {
                out.push(Arc::new(McpTool::new(
                    name.clone(),
                    t.name.clone(),
                    t.description.clone().unwrap_or_default(),
                    t.input_schema.clone(),
                    t.read_only,
                    Arc::clone(self),
                )));
            }
        }
        out
    }

    /// Sync `registry` with the effective MCP tools: unregister every tool this
    /// manager could register, then register the currently-effective ones.
    ///
    /// Idempotent. Call after changing the master/per-server switches so the next
    /// `Agent` rebuild exposes the new tool set.
    pub fn refresh_registry(self: &Arc<Self>, registry: &mut ToolRegistry) {
        for name in self.all_tool_names() {
            registry.remove(&name);
        }
        for tool in self.enabled_tools() {
            registry.register(tool);
        }
    }

    /// Dispatch a tool call, connecting to the server on first use.
    ///
    /// Never returns `Err`: transport and protocol failures are reported as an
    /// error `ToolResult` so the model sees the failure and can react.
    ///
    /// A call does not hold the per-server lock, so calls to the same server
    /// run concurrently. If a call fails at the transport layer (the server
    /// died), the cached connection is dropped and the call is retried once on
    /// a fresh connection; a tool-level error or a call timeout is returned
    /// as-is without touching the connection.
    pub async fn call_tool(&self, server: &str, remote_name: &str, args: Value) -> ToolResult {
        let Some(entry) = self.servers.get(server) else {
            return ToolResult::error(format!("unknown MCP server: {server}"));
        };
        if !self.is_effective(server) {
            return ToolResult::error(format!("MCP server '{server}' is disabled"));
        }

        let client = match acquire_client(server, entry).await {
            Ok(client) => client,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let params = rmcp::model::CallToolRequestParams::new(remote_name.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());

        match call_once(&client, params.clone()).await {
            CallOutcome::Result(result) => map_result(result),
            CallOutcome::Timeout => {
                ToolResult::error(format!("MCP call timed out after {CALL_TIMEOUT:?}"))
            }
            CallOutcome::Transport(e) => {
                // The connection may be dead: drop the cached handle so the
                // next call reconnects, then retry exactly once inline.
                tracing::debug!(server, error = %e, "MCP call failed; reconnecting once");
                drop_client_if_same(entry, &client).await;
                let retry = match acquire_client(server, entry).await {
                    Ok(client) => client,
                    Err(e) => return ToolResult::error(e.to_string()),
                };
                match call_once(&retry, params).await {
                    CallOutcome::Result(result) => map_result(result),
                    CallOutcome::Timeout => {
                        ToolResult::error(format!("MCP call timed out after {CALL_TIMEOUT:?}"))
                    }
                    CallOutcome::Transport(e) => {
                        drop_client_if_same(entry, &retry).await;
                        ToolResult::error(format!("MCP call failed: {e} (after reconnect)"))
                    }
                }
            }
        }
    }

    /// Drop all live connections. Must be awaited inside a tokio runtime:
    /// dropping a child transport only kills the process on a running reactor.
    pub async fn shutdown(&self) {
        for (name, entry) in &self.servers {
            // Take the handle and release the lock before awaiting.
            let client = entry.client.lock().await.take();
            if let Some(client) = client {
                // With no in-flight call holding a clone, close the service
                // loop and reap the child explicitly. If a clone is still out,
                // dropping our last-but-one handle lets `RunningService::drop`
                // close the loop once that call finishes.
                if let Ok(client) = Arc::try_unwrap(client) {
                    let _ = client.cancel().await;
                }
                info!(server = name, "MCP server connection closed");
            }
        }
    }

    // ---- test helpers ----

    #[cfg(test)]
    pub(crate) fn new_for_test(master: bool) -> Self {
        Self {
            servers: HashMap::new(),
            master_enabled: AtomicBool::new(master),
        }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Arc<Self> {
        Arc::new(Self::new_for_test(false))
    }

    #[cfg(test)]
    pub(crate) fn insert_server_for_test(&mut self, name: &str, cfg: ServerConfig, on: bool) {
        self.servers
            .entry(name.to_string())
            .or_insert_with(|| ServerEntry {
                config: cfg,
                enabled: AtomicBool::new(on),
                tools: vec![],
                client: Mutex::new(None),
            });
    }

    #[cfg(test)]
    pub(crate) fn insert_server_with_tools_for_test(
        &mut self,
        name: &str,
        cfg: ServerConfig,
        on: bool,
        tools: Vec<CachedTool>,
    ) {
        self.servers.insert(
            name.to_string(),
            ServerEntry {
                config: cfg,
                enabled: AtomicBool::new(on),
                tools,
                client: Mutex::new(None),
            },
        );
    }
}

/// Spawn the MCP server as a child process and complete the MCP handshake.
async fn connect(cfg: &ServerConfig) -> Result<Client> {
    use rmcp::ServiceExt;
    use rmcp::transport::TokioChildProcess;

    let mut command = tokio::process::Command::new(&cfg.command);
    command.args(&cfg.args);
    for (k, v) in &cfg.env {
        command.env(k, v);
    }
    let transport = TokioChildProcess::new(command)?;
    let client = ().serve(transport).await?;
    Ok(client)
}

/// Connect once, list every tool, convert to cache entries, then disconnect.
///
/// The connect is bounded by `CONNECT_TIMEOUT` and the `tools/list` by
/// `PROBE_TIMEOUT`. A created client is always explicitly cancelled — on the
/// success path, the list-error path, and the list-timeout path — so the child
/// process is reaped deterministically instead of relying on `RunningService`'s
/// drop path (which spawns a kill task that can be cancelled when a temporary
/// probe runtime is torn down, orphaning the child).
async fn probe_tools(cfg: &ServerConfig) -> Result<Vec<CachedTool>> {
    let client = match tokio::time::timeout(CONNECT_TIMEOUT, connect(cfg)).await {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => return Err(e),
        // `connect` already spawned the child (`TokioChildProcess::new`) before
        // the handshake, so on a handshake timeout there is no `RunningService`
        // to `cancel()`; reaping falls back to rmcp's `Drop` (a spawned kill
        // task), which may be aborted when the probe runtime is torn down.
        Err(_) => return Err(anyhow!("MCP connect timed out after {CONNECT_TIMEOUT:?}")),
    };

    // Borrow the client for the listing, then release it so `cancel` can consume.
    let listed = tokio::time::timeout(PROBE_TIMEOUT, client.list_all_tools()).await;

    // Always attempt to reap the child, even if listing failed or timed out.
    let _ = tokio::time::timeout(REAP_TIMEOUT, client.cancel()).await;

    let tools = match listed {
        Ok(Ok(tools)) => tools,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(anyhow!("MCP list tools timed out after {PROBE_TIMEOUT:?}")),
    };

    let mut out = Vec::new();
    for t in tools {
        let read_only = t
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false);
        out.push(CachedTool {
            name: t.name.to_string(),
            description: t.description.as_ref().map(|d| d.to_string()),
            input_schema: t.schema_as_json_value(),
            read_only,
        });
    }
    Ok(out)
}

/// Return a live client for `server`, connecting (bounded by `CONNECT_TIMEOUT`)
/// if none is cached. The lock is held only across the connect and the clone,
/// never across a tool call.
async fn acquire_client(server: &str, entry: &ServerEntry) -> Result<Arc<Client>> {
    let mut guard = entry.client.lock().await;
    if let Some(client) = guard.as_ref() {
        return Ok(Arc::clone(client));
    }
    let client = match tokio::time::timeout(CONNECT_TIMEOUT, connect(&entry.config)).await {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => return Err(anyhow!("connect to '{server}' failed: {e}")),
        Err(_) => return Err(anyhow!("connect to '{server}' timed out")),
    };
    let client = Arc::new(client);
    *guard = Some(Arc::clone(&client));
    info!(server, "connected to MCP server");
    Ok(client)
}

/// Clear the cached client only if it is still the exact `Arc` that failed, so
/// concurrent calls that fail together do not clobber a fresh connection.
async fn drop_client_if_same(entry: &ServerEntry, failed: &Arc<Client>) {
    let mut guard = entry.client.lock().await;
    if guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, failed))
    {
        guard.take();
    }
}

/// Result of one `tools/call`: a tool result, a call timeout, or a transport
/// failure (which may mean the connection is dead).
enum CallOutcome {
    Result(rmcp::model::CallToolResult),
    Timeout,
    Transport(rmcp::ServiceError),
}

/// Issue one bounded `tools/call` on an already-resolved client.
async fn call_once(client: &Client, params: rmcp::model::CallToolRequestParams) -> CallOutcome {
    match tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await {
        Ok(Ok(result)) => CallOutcome::Result(result),
        Ok(Err(e)) => CallOutcome::Transport(e),
        Err(_) => CallOutcome::Timeout,
    }
}

/// Translate an MCP tool result into the core `ToolResult`, flattening
/// non-text/non-image blocks to their JSON form so nothing is silently lost.
fn map_result(result: rmcp::model::CallToolResult) -> ToolResult {
    use yi_agent_core::message::{ContentBlock, ImageSource};

    let is_error = result.is_error.unwrap_or(false);
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for block in &result.content {
        match block {
            rmcp::model::ContentBlock::Text(t) => blocks.push(ContentBlock::Text(t.text.clone())),
            rmcp::model::ContentBlock::Image(img) => blocks.push(ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: img.mime_type.clone(),
                    data: img.data.clone(),
                },
                detail: Default::default(),
            }),
            other => blocks.push(ContentBlock::Text(
                serde_json::to_string(other).unwrap_or_else(|_| "<unrepresentable>".into()),
            )),
        }
    }
    if blocks.is_empty() {
        if let Some(sc) = &result.structured_content {
            blocks.push(ContentBlock::Text(sc.to_string()));
        }
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text(String::new()));
    }
    ToolResult {
        content: blocks,
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CachedTool;
    use yi_agent_core::ToolRegistry;

    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig {
            command: cmd.into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        }
    }

    /// A manager with master on and one server `fs` carrying a cached `read` tool.
    fn manager_with_fs_read() -> Arc<McpManager> {
        let mut m = McpManager::new_for_test(true);
        m.insert_server_with_tools_for_test(
            "fs",
            cfg("npx"),
            true,
            vec![CachedTool {
                name: "read".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
        );
        Arc::new(m)
    }

    /// A minimal non-MCP tool, used to prove `refresh_registry` leaves tools it
    /// does not own alone. The name is deliberately outside the `mcp__` set.
    struct LocalTool;

    #[async_trait::async_trait]
    impl Tool for LocalTool {
        fn name(&self) -> &str {
            "local_tool"
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        fn description(&self) -> &str {
            "a local, non-MCP tool"
        }

        async fn call(&self, _args: Value) -> ToolResult {
            ToolResult::text("local")
        }
    }

    #[test]
    fn effective_state_combines_master_and_server() {
        let mut m = McpManager::new_for_test(true);
        m.insert_server_for_test("fs", cfg("npx"), true);
        assert!(m.is_effective("fs"));
        m.set_master(false);
        assert!(!m.is_effective("fs"));
    }

    #[test]
    fn enabled_tools_skip_disabled_servers() {
        let mut m = McpManager::new_for_test(true);
        m.insert_server_with_tools_for_test(
            "fs",
            cfg("npx"),
            true,
            vec![CachedTool {
                name: "read".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
        );
        let m = Arc::new(m);
        assert_eq!(m.enabled_tools().len(), 1);
        m.set_server("fs", false).unwrap();
        assert_eq!(m.enabled_tools().len(), 0);
    }

    #[test]
    fn call_on_disabled_server_returns_error_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut m = McpManager::new_for_test(true);
        m.insert_server_with_tools_for_test("fs", cfg("npx"), false, vec![]);
        let res = rt.block_on(m.call_tool("fs", "read", serde_json::json!({})));
        assert!(res.is_error);
    }

    #[test]
    fn call_unknown_server_returns_error_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let m = McpManager::new_for_test(true);
        let res = rt.block_on(m.call_tool("nope", "x", serde_json::json!({})));
        assert!(res.is_error);
    }

    #[test]
    fn refresh_registry_registers_effective_tools() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        assert!(
            registry.get("mcp__fs__read").is_some(),
            "effective tool must be registered; got {:?}",
            registry.names()
        );
    }

    #[test]
    fn refresh_registry_drops_tools_when_server_disabled() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        m.set_server("fs", false).unwrap();
        m.refresh_registry(&mut registry);
        assert!(
            registry.get("mcp__fs__read").is_none(),
            "tool must be dropped after the server is disabled; got {:?}",
            registry.names()
        );
    }

    #[test]
    fn refresh_registry_drops_tools_when_master_off() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        m.set_master(false);
        m.refresh_registry(&mut registry);
        assert!(
            registry.get("mcp__fs__read").is_none(),
            "tool must be dropped after the master switch is off; got {:?}",
            registry.names()
        );
    }

    #[test]
    fn refresh_registry_is_idempotent() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        m.refresh_registry(&mut registry);
        let occurrences = registry
            .names()
            .iter()
            .filter(|n| n.as_str() == "mcp__fs__read")
            .count();
        assert_eq!(
            occurrences,
            1,
            "double refresh must not duplicate the tool; got {:?}",
            registry.names()
        );
    }

    /// `refresh_registry` must only touch MCP tools: an unrelated tool already in
    /// the registry must survive a refresh (it is not in `all_tool_names`).
    #[test]
    fn refresh_registry_preserves_unrelated_tools() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(LocalTool));
        m.refresh_registry(&mut registry);
        assert!(
            registry.get("local_tool").is_some(),
            "unrelated tool must survive; got {:?}",
            registry.names()
        );
        assert!(
            registry.get("mcp__fs__read").is_some(),
            "effective MCP tool must be registered; got {:?}",
            registry.names()
        );
        // And a second refresh must not drop it either.
        m.refresh_registry(&mut registry);
        assert!(registry.get("local_tool").is_some());
    }
}

#[cfg(test)]
mod map_result_tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock as McpContent};
    use yi_agent_core::message::{ContentBlock, ImageDetail, ImageSource};

    #[test]
    fn text_block_maps_to_one_text_block() {
        let out = map_result(CallToolResult::success(vec![McpContent::text("hello")]));
        assert!(!out.is_error);
        assert_eq!(out.content, vec![ContentBlock::Text("hello".into())]);
    }

    #[test]
    fn image_block_maps_to_base64_image_with_high_detail() {
        let out = map_result(CallToolResult::success(vec![McpContent::image(
            "ZGF0YQ==",
            "image/png",
        )]));
        assert_eq!(
            out.content,
            vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "ZGF0YQ==".into(),
                },
                detail: ImageDetail::High,
            }]
        );
    }

    #[test]
    fn non_text_block_maps_to_json_text() {
        let out = map_result(CallToolResult::success(vec![McpContent::embedded_text(
            "file:///x",
            "contents",
        )]));
        assert_eq!(out.content.len(), 1);
        match &out.content[0] {
            ContentBlock::Text(t) => {
                assert!(t.contains("resource"), "expected resource JSON, got {t}");
                assert!(t.contains("file:///x"), "expected uri JSON, got {t}");
            }
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn tool_level_error_flag_is_preserved() {
        let out = map_result(CallToolResult::error(vec![McpContent::text("boom")]));
        assert!(out.is_error);
        assert_eq!(out.content, vec![ContentBlock::Text("boom".into())]);
    }

    #[test]
    fn empty_content_with_structured_content_yields_structured_text() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [],
            "structuredContent": {"answer": 42}
        }))
        .unwrap();
        let out = map_result(result);
        assert_eq!(
            out.content,
            vec![ContentBlock::Text("{\"answer\":42}".into())]
        );
    }

    #[test]
    fn empty_content_without_structured_content_yields_empty_text() {
        let result: CallToolResult =
            serde_json::from_value(serde_json::json!({"content": []})).unwrap();
        let out = map_result(result);
        assert_eq!(out.content, vec![ContentBlock::Text(String::new())]);
    }
}
