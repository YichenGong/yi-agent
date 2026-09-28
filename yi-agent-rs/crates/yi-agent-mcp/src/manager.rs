//! `McpManager`: owns configured servers, lazily connects, and dispatches calls.
//!
//! Connections are established on first `call_tool` and cached per server, so a
//! configured-but-unused server never spawns a child process. The master switch
//! and per-server switches are plain atomics, letting the UI toggle them from a
//! different task without locking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::info;
use yi_agent_core::{Tool, ToolResult};

use crate::cache::CachedTool;
use crate::config::ServerConfig;
use crate::tool::McpTool;

/// Upper bound on a single remote tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A live client connection to one MCP server.
type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// One configured server: its config, switch state, advertised tools, and the
/// lazily-established connection (guarded so only one connect races through).
struct ServerEntry {
    config: ServerConfig,
    enabled: AtomicBool,
    tools: Vec<CachedTool>,
    client: Mutex<Option<Client>>,
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

    /// Dispatch a tool call, connecting to the server on first use.
    ///
    /// Never returns `Err`: transport and protocol failures are reported as an
    /// error `ToolResult` so the model sees the failure and can react.
    pub async fn call_tool(&self, server: &str, remote_name: &str, args: Value) -> ToolResult {
        let Some(entry) = self.servers.get(server) else {
            return ToolResult::error(format!("unknown MCP server: {server}"));
        };
        if !self.is_effective(server) {
            return ToolResult::error(format!("MCP server '{server}' is disabled"));
        }

        let mut guard = entry.client.lock().await;
        if guard.is_none() {
            match connect(&entry.config).await {
                Ok(c) => {
                    info!(server, "connected to MCP server");
                    *guard = Some(c);
                }
                Err(e) => return ToolResult::error(format!("connect to '{server}' failed: {e}")),
            }
        }
        let client = guard.as_ref().expect("just connected");

        let params = rmcp::model::CallToolRequestParams::new(remote_name.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());

        match tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await {
            Ok(Ok(result)) => map_result(result),
            Ok(Err(e)) => ToolResult::error(format!("MCP call failed: {e}")),
            Err(_) => ToolResult::error(format!("MCP call timed out after {CALL_TIMEOUT:?}")),
        }
    }

    /// Drop all live connections. Must be awaited inside a tokio runtime:
    /// dropping a child transport only kills the process on a running reactor.
    pub async fn shutdown(&self) {
        for (name, entry) in &self.servers {
            let mut guard = entry.client.lock().await;
            if let Some(client) = guard.take() {
                let _ = client.cancel().await;
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

    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig {
            command: cmd.into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
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
}
