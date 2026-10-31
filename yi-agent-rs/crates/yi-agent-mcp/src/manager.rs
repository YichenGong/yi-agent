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

use std::collections::{HashMap, HashSet};
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
use crate::config::{McpConfig, ServerConfig, Transport};
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

/// 一次 reload 里单个 server 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOutcome {
    pub name: String,
    /// 本次 reload 前不存在、现在存在。
    pub added: bool,
    /// 之前存在、现在被删除。
    pub removed: bool,
    /// 本次是否真的连上去探测过。
    pub probed: bool,
    pub tool_count: usize,
    pub read_only_count: usize,
    /// 探测失败原因原文;`None` 表示成功(或未探测)。
    pub error: Option<String>,
}

/// 一次 reload 的汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadReport {
    pub servers: Vec<ServerOutcome>,
}

impl ReloadReport {
    /// 全部 server 都没有探测错误。写盘成功与探测成功是两件事,由调用方分别呈现。
    pub fn all_ok(&self) -> bool {
        self.servers.iter().all(|s| s.error.is_none())
    }
}

/// Owns configured MCP servers and their connections.
pub struct McpManager {
    /// 配置快照。读路径 clone 出 `Arc` 后立即释放锁,**绝不跨 `.await` 持锁**
    /// (否则 future 不是 `Send`,`call_tool` 编不过)。
    servers: std::sync::RwLock<Arc<HashMap<String, Arc<ServerEntry>>>>,
    master_enabled: AtomicBool,
    /// 上一次 `refresh_registry` 注册过的工具名。用于在 server 被删除后把孤儿
    /// 工具摘掉——新快照里已经查不到这些名字,不能反过来推导。
    last_registered: std::sync::Mutex<HashSet<String>>,
    /// 上一次 reload / 启动探测时读到的配置指纹;用于每轮开跑前的廉价变更检测。
    last_seen: std::sync::Mutex<Option<String>>,
}

impl McpManager {
    /// A manager with no servers and the master switch off.
    pub fn empty() -> Arc<Self> {
        Self::from_parts(HashMap::new(), false)
    }

    fn from_parts(servers: HashMap<String, Arc<ServerEntry>>, master: bool) -> Arc<Self> {
        Arc::new(Self {
            servers: std::sync::RwLock::new(Arc::new(servers)),
            master_enabled: AtomicBool::new(master),
            last_registered: std::sync::Mutex::new(HashSet::new()),
            last_seen: std::sync::Mutex::new(None),
        })
    }

    /// 当前配置快照。clone 出 `Arc` 后立刻放开锁,调用方可以安全地 `.await`。
    fn snapshot(&self) -> Arc<HashMap<String, Arc<ServerEntry>>> {
        Arc::clone(&self.servers.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// 单个 server 的条目(先从快照 clone 出 `Arc`,不持锁)。
    fn entry(&self, server: &str) -> Option<Arc<ServerEntry>> {
        self.snapshot().get(server).cloned()
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
                Arc::new(ServerEntry {
                    config: sc.clone(),
                    enabled: AtomicBool::new(sc.enabled),
                    tools,
                    client: Mutex::new(None),
                }),
            );
        }

        if cache_dirty {
            if let Err(e) = cache.save(workdir) {
                warn!(error = %e, "failed to persist MCP cache");
            }
        }

        let manager = Self::from_parts(servers, cfg.enabled);
        // 记下启动时的指纹:否则第一轮热重载检测会把它误判成「配置变了」,
        // 白跑一次 reload(还可能是 55s 的探测)。
        if let Some(fp) = manager.config_fingerprint(workdir) {
            *manager.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        }
        Ok(manager)
    }

    /// A server is effective only when both the master switch and its own
    /// switch are on.
    pub fn is_effective(&self, server: &str) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
            && self
                .entry(server)
                .map(|e| e.enabled.load(Ordering::Relaxed))
                .unwrap_or(false)
    }

    /// 重读 `.yi-agent/mcp.json` 并同步内存快照。
    ///
    /// **只探测新增或配置变化过的 server**:配置没变的沿用旧工具清单,不重探
    /// (探测会 spawn 子进程,首次 `npx` 拉包最坏要 55s)。
    ///
    /// 单个 server 的探测失败**不会**被吞掉:原因原文写进 `ServerOutcome::error`,
    /// 由调用方回传给模型。写盘(cache)失败只 warn,不影响本次内存快照。
    pub async fn reload(&self, workdir: &Path) -> ReloadReport {
        let cfg = match McpConfig::load(workdir) {
            Ok(Some(c)) => c,
            Ok(None) => McpConfig::default(),
            Err(e) => {
                return ReloadReport {
                    servers: vec![ServerOutcome {
                        name: "<config>".into(),
                        added: false,
                        removed: false,
                        probed: false,
                        tool_count: 0,
                        read_only_count: 0,
                        error: Some(format!("{e:#}")),
                    }],
                };
            }
        };

        let old = self.snapshot();
        let mut cache = McpCache::load(workdir);
        let mut next: HashMap<String, Arc<ServerEntry>> = HashMap::new();
        let mut out = Vec::new();

        for (name, sc) in &cfg.mcp_servers {
            let previous = old.get(name);
            // 旧快照里有同名条目且配置完全一致 ⇒ 沿用工具清单,不重探。
            let reusable = previous.filter(|e| e.config == *sc);
            let (tools, probed, error) = match reusable {
                Some(e) => (e.tools.clone(), false, None),
                None => match cache.get_for(name, sc) {
                    Some(cached) => (cached.to_vec(), false, None),
                    None => match probe_tools(sc).await {
                        Ok(probed_tools) => {
                            cache.put(name, sc, probed_tools.clone());
                            (probed_tools, true, None)
                        }
                        Err(e) => (Vec::new(), true, Some(e.to_string())),
                    },
                },
            };
            let enabled = previous
                .map(|e| e.enabled.load(Ordering::Relaxed))
                .unwrap_or(sc.enabled);
            let read_only_count = tools.iter().filter(|t| t.read_only).count();
            out.push(ServerOutcome {
                name: name.clone(),
                added: previous.is_none(),
                removed: false,
                probed,
                tool_count: tools.len(),
                read_only_count,
                error,
            });
            next.insert(
                name.clone(),
                Arc::new(ServerEntry {
                    config: sc.clone(),
                    enabled: AtomicBool::new(enabled),
                    tools,
                    client: Mutex::new(None),
                }),
            );
        }

        for name in old.keys() {
            if !cfg.mcp_servers.contains_key(name) {
                out.push(ServerOutcome {
                    name: name.clone(),
                    added: false,
                    removed: true,
                    probed: false,
                    tool_count: 0,
                    read_only_count: 0,
                    error: None,
                });
            }
        }

        if let Err(e) = cache.save(workdir) {
            warn!(error = %e, "failed to persist MCP cache after reload");
        }
        self.master_enabled.store(cfg.enabled, Ordering::Relaxed);
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        if let Some(fp) = self.config_fingerprint(workdir) {
            *self.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        ReloadReport { servers: out }
    }

    /// 当前磁盘配置的指纹;文件缺失或坏 JSON 时返回 `None`(调用方据此决定是否 reload)。
    pub fn config_fingerprint(&self, workdir: &Path) -> Option<String> {
        let text = std::fs::read_to_string(crate::config::config_path(workdir)).ok()?;
        let cfg: McpConfig = serde_json::from_str(&text).ok()?;
        Some(crate::cache::fingerprint_for_manager(&cfg))
    }

    /// 上一次 reload / 启动探测时读到的配置指纹。
    pub fn last_seen_fingerprint(&self) -> Option<String> {
        self.last_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 磁盘上的配置是否与上次见到的不一样。
    ///
    /// 用在"每轮开始前"的检查:用户可能在别的窗口改了 `mcp.json`,也可能
    /// 刚刚通过 `mcp_reload` 写过盘——后者会在 `reload` 里更新 `last_seen`,
    /// 所以不会误判成"外部改动"而白跑一次。
    ///
    /// 文件缺失且从未见过配置时返回 `false`(没什么可重载的);文件从有到无
    /// 返回 `true`(server 全删了,得把它清出 registry)。
    pub fn config_changed_on_disk(&self, workdir: &Path) -> bool {
        let current = self.config_fingerprint(workdir);
        let last = self.last_seen_fingerprint();
        match (current, last) {
            (None, None) => false,
            (a, b) => a != b,
        }
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
            .entry(server)
            .ok_or_else(|| anyhow!("unknown MCP server: {server}"))?;
        entry.enabled.store(on, Ordering::Relaxed);
        Ok(())
    }

    /// Configured server names, sorted for stable display.
    pub fn server_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.snapshot().keys().cloned().collect();
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
        let snapshot = self.snapshot();
        snapshot
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
        let snapshot = self.snapshot();
        for (name, entry) in snapshot.iter() {
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
        // 按**上次注册过的名字**清理,而不是按当前快照推导:server 被删掉后
        // 它的名字已不在快照里,反向推导会把这些工具永久留在 registry。
        let mut registered = self
            .last_registered
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for name in registered.drain() {
            registry.remove(&name);
        }
        for tool in self.enabled_tools() {
            registered.insert(tool.name().to_string());
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
        let Some(entry) = self.entry(server) else {
            return ToolResult::error(format!("unknown MCP server: {server}"));
        };
        if !self.is_effective(server) {
            return ToolResult::error(format!("MCP server '{server}' is disabled"));
        }

        let client = match acquire_client(server, &entry).await {
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
                drop_client_if_same(&entry, &client).await;
                let retry = match acquire_client(server, &entry).await {
                    Ok(client) => client,
                    Err(e) => return ToolResult::error(e.to_string()),
                };
                match call_once(&retry, params).await {
                    CallOutcome::Result(result) => map_result(result),
                    CallOutcome::Timeout => {
                        ToolResult::error(format!("MCP call timed out after {CALL_TIMEOUT:?}"))
                    }
                    CallOutcome::Transport(e) => {
                        drop_client_if_same(&entry, &retry).await;
                        ToolResult::error(format!("MCP call failed: {e} (after reconnect)"))
                    }
                }
            }
        }
    }

    /// Drop all live connections. Must be awaited inside a tokio runtime:
    /// dropping a child transport only kills the process on a running reactor.
    /// Each connection close is bounded by `REAP_TIMEOUT`; a wedged server falls
    /// back to the `RunningService` drop path.
    pub async fn shutdown(&self) {
        let snapshot = self.snapshot();
        for (name, entry) in snapshot.iter() {
            // Take the handle and release the lock before awaiting.
            let client = entry.client.lock().await.take();
            if let Some(client) = client {
                // With no in-flight call holding a clone, close the service
                // loop and reap the child explicitly. If a clone is still out,
                // dropping our last-but-one handle lets `RunningService::drop`
                // close the loop once that call finishes.
                if let Ok(client) = Arc::try_unwrap(client) {
                    // Bound the close so a wedged server cannot stall teardown;
                    // on elapse, fall back to `RunningService`'s drop path, which
                    // cancels the connection when the service loop is dropped.
                    if tokio::time::timeout(REAP_TIMEOUT, client.cancel())
                        .await
                        .is_err()
                    {
                        tracing::debug!(
                            server = name,
                            "MCP shutdown timed out; falling back to drop"
                        );
                    }
                }
                info!(server = name, "MCP server connection closed");
            }
        }
    }

    // ---- test helpers ----

    #[cfg(test)]
    pub(crate) fn new_for_test(master: bool) -> Self {
        Self {
            servers: std::sync::RwLock::new(Arc::new(HashMap::new())),
            master_enabled: AtomicBool::new(master),
            last_registered: std::sync::Mutex::new(HashSet::new()),
            last_seen: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Arc<Self> {
        Arc::new(Self::new_for_test(false))
    }

    #[cfg(test)]
    pub(crate) fn insert_server_for_test(&mut self, name: &str, cfg: ServerConfig, on: bool) {
        let mut map = (**self.servers.get_mut().unwrap_or_else(|e| e.into_inner())).clone();
        map.entry(name.to_string()).or_insert_with(|| {
            Arc::new(ServerEntry {
                config: cfg,
                enabled: AtomicBool::new(on),
                tools: vec![],
                client: Mutex::new(None),
            })
        });
        *self.servers.get_mut().unwrap_or_else(|e| e.into_inner()) = Arc::new(map);
    }

    #[cfg(test)]
    pub(crate) fn insert_server_with_tools_for_test(
        &mut self,
        name: &str,
        cfg: ServerConfig,
        on: bool,
        tools: Vec<CachedTool>,
    ) {
        let mut map = (**self.servers.get_mut().unwrap_or_else(|e| e.into_inner())).clone();
        map.insert(
            name.to_string(),
            Arc::new(ServerEntry {
                config: cfg,
                enabled: AtomicBool::new(on),
                tools,
                client: Mutex::new(None),
            }),
        );
        *self.servers.get_mut().unwrap_or_else(|e| e.into_inner()) = Arc::new(map);
    }

    /// 模拟一次 reload 之后的新快照(测试用)。只暴露"清空"这一种动作,
    /// 免得把私有的 `ServerEntry` 泄进公开签名。
    #[cfg(test)]
    pub(crate) fn clear_servers_for_test(&self) {
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(HashMap::new());
    }
}

/// Test seam: when set, `connect` serves this future instead of spawning a
/// stdio child, letting an in-process test drive the real protocol over a
/// duplex transport. Production builds compile it out.
#[cfg(test)]
pub(crate) type TestConnector = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Client>> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
pub(crate) static TEST_CONNECTOR: std::sync::Mutex<Option<TestConnector>> =
    std::sync::Mutex::new(None);

/// Clone the installed test connector, dropping the lock guard before the
/// caller awaits (a `std::sync::MutexGuard` is not `Send`).
#[cfg(test)]
fn test_connector() -> Option<TestConnector> {
    TEST_CONNECTOR.lock().unwrap().clone()
}

/// 按 `cfg.transport()` 分派到 stdio 子进程或 Streamable HTTP,完成 MCP 握手。
///
/// 两条路径都产出同一个 `Client`,因此懒连接/重连/超时/shutdown 逻辑无需分支。
async fn connect(cfg: &ServerConfig) -> Result<Client> {
    #[cfg(test)]
    {
        if let Some(factory) = test_connector() {
            return factory().await;
        }
    }

    use rmcp::ServiceExt;

    match cfg.transport()? {
        Transport::Stdio { command, args, env } => {
            use rmcp::transport::TokioChildProcess;

            let mut cmd = tokio::process::Command::new(&command);
            cmd.args(&args);
            for (k, v) in &env {
                cmd.env(k, v);
            }
            let transport = TokioChildProcess::new(cmd)?;
            Ok(().serve(transport).await?)
        }
        Transport::Http { url, headers } => {
            use rmcp::transport::StreamableHttpClientTransport;
            use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

            let expanded = crate::config::expand_headers(&headers);
            let mut http_cfg = StreamableHttpClientTransportConfig::with_uri(url);
            if !expanded.is_empty() {
                let mut map = std::collections::HashMap::new();
                for (k, v) in &expanded {
                    let name = http::HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| anyhow!("invalid http header name {k:?}: {e}"))?;
                    let value = http::HeaderValue::from_str(v)
                        .map_err(|e| anyhow!("invalid http header value for {k:?}: {e}"))?;
                    map.insert(name, value);
                }
                http_cfg = http_cfg.custom_headers(map);
            }
            let transport = StreamableHttpClientTransport::from_config(http_cfg);
            Ok(().serve(transport).await?)
        }
    }
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
                path: None,
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
    use crate::config::ServerKind;
    use yi_agent_core::ToolRegistry;

    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(cmd.into()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
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

    /// 配置没变时 `reload` 不得重新探测——探测会 spawn 子进程并可能耗时 55s。
    #[tokio::test]
    async fn reload_does_not_reprobe_an_unchanged_server() {
        let _serial = seam_guard();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#,
        )
        .unwrap();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        *TEST_CONNECTOR.lock().unwrap() = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(anyhow!("counting connector: no real server")) })
        }));
        let _seam = SeamGuard;

        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        let after_load = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_load, 1, "startup must probe once (cache is cold)");

        let report = mgr.reload(dir.path()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            after_load,
            "unchanged config must not re-probe"
        );
        assert!(
            report.servers.iter().all(|s| !s.probed),
            "report must say nothing was probed: {report:?}"
        );
    }

    /// 改了配置就必须重新探测,并把失败原文带上(不静默)。
    #[tokio::test]
    async fn reload_reprobes_a_changed_server_and_reports_the_error() {
        let _serial = seam_guard();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        *TEST_CONNECTOR.lock().unwrap() = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(anyhow!("boom")) })
        }));
        let _seam = SeamGuard;

        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 改 args ⇒ 指纹变化
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["--new"]}}}"#,
        )
        .unwrap();
        let report = mgr.reload(dir.path()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must re-probe"
        );
        let fs = report.servers.iter().find(|s| s.name == "fs").unwrap();
        assert!(fs.probed);
        assert_eq!(fs.error.as_deref(), Some("boom"), "error must be surfaced");
    }

    /// 删除一个 server 后,它的工具名必须从 registry 消失。
    #[tokio::test]
    async fn reload_removes_a_deleted_server() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let mut cache = McpCache::default();
        let sc = ServerConfig {
            kind: crate::config::ServerKind::Stdio,
            command: Some("npx".into()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            enabled: true,
        };
        cache.put("fs", &sc, vec![]);
        cache.save(dir.path()).unwrap();

        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        let mut registry = ToolRegistry::new();
        mgr.refresh_registry(&mut registry);

        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{}}"#,
        )
        .unwrap();
        let report = mgr.reload(dir.path()).await;
        assert!(report.servers.iter().any(|s| s.name == "fs" && s.removed));
        mgr.refresh_registry(&mut registry);
        assert!(
            registry.names().iter().all(|n| !n.starts_with("mcp__fs__")),
            "deleted server's tools must be gone; got {:?}",
            registry.names()
        );
    }

    /// 每轮检查:磁盘没变就不该说"变了",否则每轮都会白跑一次 reload。
    #[tokio::test]
    async fn config_changed_on_disk_is_false_when_nothing_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();

        assert!(
            !mgr.config_changed_on_disk(dir.path()),
            "load_and_probe must record what it saw"
        );
    }

    #[tokio::test]
    async fn config_changed_on_disk_detects_an_external_edit() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert!(!mgr.config_changed_on_disk(dir.path()));

        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"uvx"}}}"#,
        )
        .unwrap();
        assert!(
            mgr.config_changed_on_disk(dir.path()),
            "edit must be noticed"
        );

        // reload 之后同一次改动不该反复触发。
        mgr.reload(dir.path()).await;
        assert!(!mgr.config_changed_on_disk(dir.path()));
    }

    /// 配置从有到无:server 全被删掉,必须触发一次收尾 reload。
    #[tokio::test]
    async fn config_changed_on_disk_is_true_when_the_file_disappears() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let path = dir.path().join(".yi-agent/mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert!(!mgr.config_changed_on_disk(dir.path()));

        std::fs::remove_file(&path).unwrap();
        assert!(mgr.config_changed_on_disk(dir.path()));
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
    fn refresh_registry_removes_tools_of_servers_dropped_from_the_snapshot() {
        let m = manager_with_fs_read();
        let mut registry = ToolRegistry::new();
        m.refresh_registry(&mut registry);
        assert!(registry.get("mcp__fs__read").is_some());

        // 模拟 reload 把 fs 整条删掉:新快照里没有它。
        m.clear_servers_for_test();
        m.refresh_registry(&mut registry);

        assert!(
            registry.get("mcp__fs__read").is_none(),
            "tool of a removed server must be unregistered; got {:?}",
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

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct EchoServer {
        calls: Arc<AtomicUsize>,
    }

    impl rmcp::ServerHandler for EchoServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                rmcp::model::ServerCapabilities::builder()
                    .enable_tools()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
            Ok(rmcp::model::ListToolsResult::with_all_items(vec![
                rmcp::model::Tool::new("echo", "Echoes its arguments", serde_json::Map::new()),
            ]))
        }

        async fn call_tool(
            &self,
            request: rmcp::model::CallToolRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    format!("echo:{:?}", request.arguments),
                )])
                .into(),
            )
        }
    }

    /// Install a connector that serves `EchoServer` over a fresh duplex stream
    /// per call. `calls` deliberately counts `tools/call` invocations across
    /// BOTH the probe connection and the later call connection, so a silent
    /// reconnect (a third `tools/call`) would be caught by `assert_eq!(calls, 1)`.
    fn install_echo_connector(calls: Arc<AtomicUsize>) {
        use rmcp::ServiceExt;
        let factory: TestConnector = Arc::new(move || {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    if let Ok(running) = (EchoServer { calls }).serve(server_io).await {
                        let _ = running.waiting().await;
                    }
                });
                let client: Client = ().serve(client_io).await?;
                Ok(client)
            })
        });
        *TEST_CONNECTOR.lock().unwrap() = Some(factory);
    }

    /// `TEST_CONNECTOR` 是进程级全局量,并行测试会互相清空/覆盖。
    /// 所有用到这个缝的测试都先拿这把锁,保证同一时刻只有一个在跑。
    static SEAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn seam_guard() -> std::sync::MutexGuard<'static, ()> {
        SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Clears the process-global `TEST_CONNECTOR` on drop, including on panic,
    /// so a failed assertion cannot leak the seam into other tests.
    struct SeamGuard;
    impl Drop for SeamGuard {
        fn drop(&mut self) {
            *TEST_CONNECTOR.lock().unwrap() = None;
        }
    }

    #[tokio::test]
    async fn probe_then_call_over_duplex() {
        let _serial = seam_guard();
        let _seam = SeamGuard;
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));

        let dir = tempfile::tempdir().unwrap();
        let sc = ServerConfig {
            kind: ServerKind::Stdio,
            command: Some("echo".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        };
        let mut servers = BTreeMap::new();
        servers.insert("s".to_string(), sc.clone());
        let cfg = McpConfig {
            enabled: true,
            mcp_servers: servers,
        };

        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();

        // Probe listed the tool and persisted the schema cache, but issued no tools/call.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "probe must not call any tool"
        );
        let cached = McpCache::load(dir.path());
        let hit = cached.get_for("s", &sc).expect("cache hit after probe");
        assert_eq!(hit.len(), 1, "probe should list exactly one tool");
        assert_eq!(hit[0].name, "echo");
        assert_eq!(mgr.enabled_tools().len(), 1);

        // First call connects lazily and round-trips the arguments.
        let res = mgr
            .call_tool("s", "echo", serde_json::json!({"x": 1}))
            .await;
        assert!(!res.is_error, "echo should succeed: {res:?}");
        match &res.content[0] {
            yi_agent_core::message::ContentBlock::Text(t) => {
                assert!(t.starts_with("echo:"), "unexpected text: {t}")
            }
            other => panic!("expected a text block, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one tools/call");

        // Toggling the server off removes its tools.
        mgr.set_server("s", false).unwrap();
        assert!(mgr.enabled_tools().is_empty());

        mgr.shutdown().await;
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
                path: None,
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
