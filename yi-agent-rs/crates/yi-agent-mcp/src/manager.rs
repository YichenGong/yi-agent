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

use crate::cache::{CacheLookup, CachedTool, McpCache, RETRY_AFTER_SECS, TTL_SECS};
use crate::config::{McpConfig, ServerConfig, Transport};
use crate::tool::McpTool;

/// Upper bound on a single remote tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Upper bound on spawning a server and completing the MCP `initialize`
/// handshake. Bounds a server that starts but never finishes handshaking.
/// 当前 Unix 秒。整个 crate 只有这一处取真实时钟,其余全部接受 `now` 参数。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
    /// 本轮因失败退避窗口而**没有**探测(区别于「配置未变,复用旧清单」)。
    pub skipped_backoff: bool,
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
    /// 本轮因失败退避窗口而跳过探测的 server 名。
    ///
    /// 手动 `reload` 时用来说明「为什么没刷新」:否则用户分不清「真没问题」
    /// 与「被退避挡住」。按名字排序,便于稳定展示。
    pub fn skipped_backoff_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .servers
            .iter()
            .filter(|s| s.skipped_backoff)
            .map(|s| s.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

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
            let now = now_unix();
            // 存量条目(无 probed_at)一次性迁移到 now:否则升级后第一条
            // needs_reload 会把它们全判成 Stale,同一轮集体重探(10 个 server
            // 串行就是 30~40 秒),看起来像「升级把软件搞坏了」。
            if cache.backfill_probed_at(name, now) {
                cache_dirty = true;
            }
            // 启动路径刻意**不**校验 TTL:过期条目照样直接用(决策 1)。
            // 但停用的 server 不该探——它根本不会注册成工具,探满 55s 纯浪费。
            let tools = match cache.cached_tools_ignoring_freshness(name, sc) {
                Some(t) => t.to_vec(),
                None if !cfg.enabled || !sc.enabled => Vec::new(),
                None => match probe_tools(sc).await {
                    Ok(probed) => {
                        cache.put(name, sc, probed.clone(), now);
                        cache_dirty = true;
                        probed
                    }
                    Err(e) => {
                        warn!(server = %name, error = %e, "MCP probe failed; server unavailable");
                        // 记下失败时刻:否则「只启动、不 reload」的 headless 场景下
                        // 退避永远不会开始计时,每轮都要重探一遍(最坏 55s)。
                        cache.note_failure(name, sc, now);
                        cache_dirty = true;
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
    pub async fn reload(&self, workdir: &Path, now: i64) -> ReloadReport {
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
                        skipped_backoff: false,
                        tool_count: 0,
                        read_only_count: 0,
                        error: Some(format!("{e:#}")),
                    }],
                };
            }
        };

        let old = self.snapshot();
        let mut cache = McpCache::load(workdir);
        let mut cache_dirty = false;
        let mut next: HashMap<String, Arc<ServerEntry>> = HashMap::new();
        let mut out = Vec::new();

        for (name, sc) in &cfg.mcp_servers {
            // 存量条目迁移:缺 probed_at 的填成 now(一次性,不探测)。
            if cache.backfill_probed_at(name, now) {
                cache_dirty = true;
            }

            let previous = old.get(name);
            let reusable = previous.filter(|e| e.config == *sc);
            let probe_allowed = cfg.enabled && sc.enabled;

            // 开关关闭:不探测、不写 failed_at,工具按「内存快照 → 磁盘缓存 → 空」回落。
            let (tools, probed, error, skipped_backoff) = if !probe_allowed {
                let t = match reusable {
                    Some(e) if !e.tools.is_empty() => e.tools.clone(),
                    _ => cache
                        .cached_tools_ignoring_freshness(name, sc)
                        .map(<[CachedTool]>::to_vec)
                        .unwrap_or_default(),
                };
                (t, false, None, false)
            } else {
                // 先把判定结果拷成 owned 值,再动 cache(否则 `Fresh` 借来的切片
                // 会与后面的 `cache.put` / `note_failure` 冲突)。
                let lookup = cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS);
                // `Fresh` 用**磁盘**清单(跨进程同步:进程 A 写的新清单,进程 B 必须
                // 读到)。这里不能要求 `reusable` 存在——内存里没有条目时同样该用磁盘。
                let fresh = match lookup {
                    CacheLookup::Fresh(disk) => Some(disk.to_vec()),
                    _ => None,
                };
                let backed_off = matches!(lookup, CacheLookup::BackedOff);

                if let Some(disk) = fresh {
                    (disk, false, None, false)
                } else if backed_off {
                    // `BackedOff`:刚失败过,不探测。数据源是**内存快照**(磁盘可能
                    // 因 save 失败而缺失或更旧),没有内存快照才退到磁盘旧清单。
                    let t = match &reusable {
                        Some(e) if !e.tools.is_empty() => e.tools.clone(),
                        _ => cache
                            .cached_tools_ignoring_freshness(name, sc)
                            .map(<[CachedTool]>::to_vec)
                            .unwrap_or_default(),
                    };
                    (t, false, None, true)
                } else {
                    match probe_tools(sc).await {
                        Ok(probed_tools) => {
                            cache.put(name, sc, probed_tools.clone(), now);
                            cache_dirty = true;
                            (probed_tools, true, None, false)
                        }
                        Err(e) => {
                            // 保留旧清单:内存快照优先(它才是 agent 当前在用的那批),
                            // 其次磁盘缓存。若反过来,磁盘缓存缺失时会把可用工具清成 0 个。
                            let fallback = match &reusable {
                                Some(entry) if !entry.tools.is_empty() => Some(entry.tools.clone()),
                                _ => cache
                                    .cached_tools_ignoring_freshness(name, sc)
                                    .map(<[CachedTool]>::to_vec),
                            };
                            // 记下失败时刻,让下一轮 reload 在退避窗口内跳过探测。
                            // 不写 probed_at:失败不能把「已过期」固化成「新鲜」。
                            cache.note_failure(name, sc, now);
                            cache_dirty = true;
                            (
                                fallback.unwrap_or_default(),
                                true,
                                Some(e.to_string()),
                                false,
                            )
                        }
                    }
                }
            };

            let read_only_count = tools.iter().filter(|t| t.read_only).count();
            out.push(ServerOutcome {
                name: name.clone(),
                added: previous.is_none(),
                removed: false,
                probed,
                skipped_backoff,
                tool_count: tools.len(),
                read_only_count,
                error,
            });

            let enabled = match previous {
                None => sc.enabled,
                Some(e) => {
                    let runtime = e.enabled.load(Ordering::Relaxed);
                    if e.config.enabled == sc.enabled {
                        runtime
                    } else {
                        sc.enabled
                    }
                }
            };
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

        // 被删掉的 server:只报 removed,不写缓存(它已经不在配置里)。
        for name in old.keys() {
            if !cfg.mcp_servers.contains_key(name) {
                out.push(ServerOutcome {
                    name: name.clone(),
                    added: false,
                    removed: true,
                    probed: false,
                    skipped_backoff: false,
                    tool_count: 0,
                    read_only_count: 0,
                    error: None,
                });
            }
        }

        if cache_dirty {
            if let Err(e) = cache.save(workdir) {
                warn!(error = %e, "failed to persist MCP cache after reload");
            }
        }
        self.master_enabled.store(cfg.enabled, Ordering::Relaxed);
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        if let Some(fp) = self.config_fingerprint(workdir) {
            *self.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        ReloadReport { servers: out }
    }

    /// 读一次 `mcp.json`,同时拿到指纹与解析后的配置。
    ///
    /// 拆出来是为了让 [`needs_reload`](Self::needs_reload) 不必读两遍盘:
    /// 指纹比较与逐 server 判定用的是同一份解析结果。
    pub fn fingerprint_and_config(&self, workdir: &Path) -> Option<(String, McpConfig)> {
        let text = std::fs::read_to_string(crate::config::config_path(workdir)).ok()?;
        let cfg: McpConfig = serde_json::from_str(&text).ok()?;
        let fp = crate::cache::fingerprint_for_manager(&cfg);
        Some((fp, cfg))
    }

    /// 当前磁盘配置的指纹;文件缺失或坏 JSON 时返回 `None`(调用方据此决定是否 reload)。
    pub fn config_fingerprint(&self, workdir: &Path) -> Option<String> {
        self.fingerprint_and_config(workdir).map(|(fp, _)| fp)
    }

    /// 上一次 reload / 启动探测时读到的配置指纹。
    pub fn last_seen_fingerprint(&self) -> Option<String> {
        self.last_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 磁盘上的配置是否与上次见到的不一样(只比指纹)。
    ///
    /// `needs_reload` 走的是「读一次、复用解析结果」的路径,不使用本方法;
    /// 它保留给只需要布尔判断的调用方(既有测试与将来的一次性检查)。
    ///
    /// 文件缺失且从未见过配置时返回 `false`(没什么可重载的);文件从有到无
    /// 返回 `true`(server 全删了,得把它清出 registry)。
    #[allow(dead_code)]
    fn fingerprint_changed(&self, workdir: &Path) -> bool {
        let current = self.config_fingerprint(workdir);
        let last = self.last_seen_fingerprint();
        match (current, last) {
            (None, None) => false,
            (a, b) => a != b,
        }
    }

    /// 每轮开跑前:是否需要重新 `reload`。
    ///
    /// 判据必须与 [`reload`](Self::reload) 的短路逐条对齐——`reload` 不会做任何事
    /// 的 server,就不能成为触发条件,否则每轮空转(还可能每轮重建一次 agent)。
    pub fn needs_reload(&self, workdir: &Path, now: i64) -> bool {
        // 整个函数只读一次 mcp.json:解析结果同时用于指纹比较与逐 server 判定。
        let current = self.fingerprint_and_config(workdir);

        // 1. 指纹变了 ⇒ 一定重载(含「文件从有到无 / 从无到有」)
        let last = self.last_seen_fingerprint();
        let changed = match (&current, &last) {
            (None, None) => false,
            (Some((fp, _)), Some(prev)) => fp != prev,
            _ => true,
        };
        if changed {
            return true;
        }

        // 2. 配置没变,但某个 server 的条目已过期且允许重试 ⇒ 也要重载
        let Some((_, cfg)) = current else {
            return false;
        };
        // 总开关关闭 ⇒ reload 整段不探测,不能成为触发条件
        if !cfg.enabled {
            return false;
        }
        let cache = McpCache::load(workdir);
        cfg.mcp_servers.iter().any(|(name, sc)| {
            // 停用的 server 由 reload 短路、永不探测 ⇒ 不能触发 reload
            sc.enabled
                && matches!(
                    cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS),
                    CacheLookup::Stale
                )
        })
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
    ///
    /// 顺带把返回的名字记进 `last_registered`:启动期注册走的是这条路径
    /// (而不是 `refresh_registry`),漏记的话之后停用/删除该 server 时
    /// `refresh_registry` 无名字可摘,工具会一直留在工具表里。
    pub fn enabled_tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let snapshot = self.snapshot();
        for (name, entry) in snapshot.iter() {
            if !self.is_effective(name) {
                continue;
            }
            for t in &entry.tools {
                let tool = McpTool::new(
                    name.clone(),
                    t.name.clone(),
                    t.description.clone().unwrap_or_default(),
                    t.input_schema.clone(),
                    t.read_only,
                    Arc::clone(self),
                );
                names.push(tool.name().to_string());
                out.push(Arc::new(tool));
            }
        }
        // 只增不删:`refresh_registry` 负责 drain,这里补记它没见过的那批。
        {
            let mut registered = self
                .last_registered
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for n in names {
                registered.insert(n);
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
        // 先取工具再拿锁:`enabled_tools` 自己也会写 `last_registered`,
        // 持锁调用会自死锁(std::sync::Mutex 不可重入)。
        let tools = self.enabled_tools();
        // 按**上次注册过的名字**清理,而不是按当前快照推导:server 被删掉后
        // 它的名字已不在快照里,反向推导会把这些工具永久留在 registry。
        let mut registered = self
            .last_registered
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for name in registered.drain() {
            registry.remove(&name);
        }
        for tool in tools {
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
    #[allow(clippy::await_holding_lock)]
    // `seam_guard()` 要持到 await 之后：这把锁保护的是进程级 `TEST_CONNECTOR`，
    // 中途放手别的 seam 测试会插进来换掉它。`#[tokio::test]` 是 current_thread
    // 单线程运行时（实测 `num_workers == 1`），没有第二个任务能竞争，故不会死锁。
    #[allow(clippy::await_holding_lock)]
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

        let report = mgr.reload(dir.path(), now_unix()).await;
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
    #[allow(clippy::await_holding_lock)]
    // `seam_guard()` 要持到 await 之后：这把锁保护的是进程级 `TEST_CONNECTOR`，
    // 中途放手别的 seam 测试会插进来换掉它。`#[tokio::test]` 是 current_thread
    // 单线程运行时（实测 `num_workers == 1`），没有第二个任务能竞争，故不会死锁。
    #[allow(clippy::await_holding_lock)]
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
        let report = mgr.reload(dir.path(), now_unix()).await;
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
    #[allow(clippy::await_holding_lock)]
    // 这些用例会走 `load_and_probe` → `connect`，而 `connect` 读进程级
    // `TEST_CONNECTOR`：不拿锁就会抢用别的 seam 测试装好的假 connector，把它的
    // 计数器加一（`after_load == 1` 断言失败），或反过来被清空后真去 spawn `npx`。
    // 锁要保住整个 await 区间；current_thread 单线程运行时不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn reload_removes_a_deleted_server() {
        let _serial = seam_guard();
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
        cache.put("fs", &sc, vec![], now_unix());
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
        let report = mgr.reload(dir.path(), now_unix()).await;
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
    #[allow(clippy::await_holding_lock)]
    // 这些用例会走 `load_and_probe` → `connect`，而 `connect` 读进程级
    // `TEST_CONNECTOR`：不拿锁就会抢用别的 seam 测试装好的假 connector，把它的
    // 计数器加一（`after_load == 1` 断言失败），或反过来被清空后真去 spawn `npx`。
    // 锁要保住整个 await 区间；current_thread 单线程运行时不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn config_changed_on_disk_is_false_when_nothing_changed() {
        let _serial = seam_guard();
        // 只测配置指纹，不关心工具探测：装一个立即失败的 connector，
        // 免得持锁期间真去 spawn `npx` 子进程。
        install_failing_connector();
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
            !mgr.needs_reload(dir.path(), now_unix()),
            "load_and_probe must record what it saw"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    // 这些用例会走 `load_and_probe` → `connect`，而 `connect` 读进程级
    // `TEST_CONNECTOR`：不拿锁就会抢用别的 seam 测试装好的假 connector，把它的
    // 计数器加一（`after_load == 1` 断言失败），或反过来被清空后真去 spawn `npx`。
    // 锁要保住整个 await 区间；current_thread 单线程运行时不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn config_changed_on_disk_detects_an_external_edit() {
        let _serial = seam_guard();
        // 只测配置指纹，不关心工具探测：装一个立即失败的 connector，
        // 免得持锁期间真去 spawn `npx` 子进程。
        install_failing_connector();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert!(!mgr.needs_reload(dir.path(), now_unix()));

        std::fs::write(
            dir.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"uvx"}}}"#,
        )
        .unwrap();
        assert!(
            mgr.needs_reload(dir.path(), now_unix()),
            "edit must be noticed"
        );

        // reload 之后同一次改动不该反复触发。
        mgr.reload(dir.path(), now_unix()).await;
        assert!(!mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 配置从有到无:server 全被删掉,必须触发一次收尾 reload。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    // 这些用例会走 `load_and_probe` → `connect`，而 `connect` 读进程级
    // `TEST_CONNECTOR`：不拿锁就会抢用别的 seam 测试装好的假 connector，把它的
    // 计数器加一（`after_load == 1` 断言失败），或反过来被清空后真去 spawn `npx`。
    // 锁要保住整个 await 区间；current_thread 单线程运行时不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn config_changed_on_disk_is_true_when_the_file_disappears() {
        let _serial = seam_guard();
        // 只测配置指纹，不关心工具探测：装一个立即失败的 connector，
        // 免得持锁期间真去 spawn `npx` 子进程。
        install_failing_connector();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let path = dir.path().join(".yi-agent/mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert!(!mgr.needs_reload(dir.path(), now_unix()));

        std::fs::remove_file(&path).unwrap();
        assert!(mgr.needs_reload(dir.path(), now_unix()));
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

    /// 反向约束:TUI 的 `/mcp disable` 只改内存,磁盘上的 `enabled` 没变;
    /// 之后因为**别的**原因触发一次 reload,那个运行时开关不能被冲掉。
    #[tokio::test]
    // `seam_guard()` 要持到 await 之后：这把锁保护的是进程级 `TEST_CONNECTOR`，
    // 中途放手别的 seam 测试会插进来换掉它。`#[tokio::test]` 是 current_thread
    // 单线程运行时（实测 `num_workers == 1`），没有第二个任务能竞争，故不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn reload_keeps_an_in_memory_switch_when_disk_did_not_change() {
        // 整进程共享的连接接缝:冷缓存探测会读它,必须与其它 seam 测试串行。
        let _serial = seam_guard();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let cfg_path = dir.path().join(".yi-agent/mcp.json");
        std::fs::write(&cfg_path, r#"{"mcpServers":{"fs":{"command":"npx"}}}"#).unwrap();

        let sc = cfg("npx");
        let mut cache = McpCache::default();
        cache.put("fs", &sc, vec![], now_unix());
        cache.save(dir.path()).unwrap();

        let loaded = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(loaded, dir.path())
            .await
            .unwrap();

        // 等价于 TUI 的 `/mcp disable`:只动内存。
        mgr.set_server("fs", false).unwrap();
        assert!(!mgr.is_effective("fs"));

        // 磁盘加一个无关的新 server,触发 reload(fs 自己的 enabled 没变)。
        // 新 server 也预置缓存,免得 reload 真的 spawn 一个 `uvx`。
        let other = ServerConfig {
            kind: crate::config::ServerKind::Stdio,
            command: Some("uvx".into()),
            ..cfg("uvx")
        };
        let mut cache = McpCache::load(dir.path());
        cache.put("other", &other, vec![], now_unix());
        cache.save(dir.path()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"mcpServers":{"fs":{"command":"npx"},"other":{"command":"uvx"}}}"#,
        )
        .unwrap();
        mgr.reload(dir.path(), now_unix()).await;

        assert!(
            !mgr.is_effective("fs"),
            "an unrelated disk edit must not re-enable a server disabled at runtime"
        );
    }

    /// 磁盘上停用某个 server 后 reload,它必须真的不再生效。
    ///
    /// 回归:`reload` 曾用内存里的旧 `enabled` 覆盖磁盘值,于是设置页的
    /// `mcp/set`(只写盘)与 `mcp_reload disable`(只写盘)的效果会在下一次
    /// reload 时被悄悄撤销——用户看到"已停用",工具却照旧可用。
    #[tokio::test]
    // `seam_guard()` 要持到 await 之后：这把锁保护的是进程级 `TEST_CONNECTOR`，
    // 中途放手别的 seam 测试会插进来换掉它。`#[tokio::test]` 是 current_thread
    // 单线程运行时（实测 `num_workers == 1`），没有第二个任务能竞争，故不会死锁。
    #[allow(clippy::await_holding_lock)]
    async fn reload_honors_a_server_disabled_on_disk() {
        // 整进程共享的连接接缝:冷缓存探测会读它,必须与其它 seam 测试串行。
        let _serial = seam_guard();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let cfg_path = dir.path().join(".yi-agent/mcp.json");
        std::fs::write(&cfg_path, r#"{"mcpServers":{"fs":{"command":"npx"}}}"#).unwrap();

        // 预置缓存,避免 reload 真的去 spawn `npx`。
        let sc = cfg("npx");
        let mut cache = McpCache::default();
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "read".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now_unix(),
        );
        cache.save(dir.path()).unwrap();

        let cfg = McpConfig::load(dir.path()).unwrap().unwrap();
        let mgr = McpManager::load_and_probe(cfg, dir.path()).await.unwrap();
        assert!(mgr.is_effective("fs"), "server starts out enabled");

        // 与 `mcp/set` / `mcp_reload disable` 一致:只改磁盘,不动内存。
        std::fs::write(
            &cfg_path,
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        )
        .unwrap();
        mgr.reload(dir.path(), now_unix()).await;

        assert!(
            !mgr.is_effective("fs"),
            "reload must honor enabled=false from disk, not the stale in-memory flag"
        );
        let mut registry = ToolRegistry::new();
        mgr.refresh_registry(&mut registry);
        assert!(
            registry.get("mcp__fs__read").is_none(),
            "a server disabled on disk must not keep its tools registered; got {:?}",
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

    /// 装一个「立即失败」的 connector：探测必然失败并落成空工具列表，
    /// 但不会 spawn 真实子进程，适合只关心配置行为的用例。
    fn install_failing_connector() {
        let factory: TestConnector = Arc::new(|| Box::pin(async { Err(anyhow!("no server")) }));
        *TEST_CONNECTOR.lock().unwrap() = Some(factory);
    }

    /// 装一个「计数型」失败 connector：每次**连接尝试**都 +1 再失败。
    ///
    /// 与 `install_echo_connector` 的区别:那个只在 `tools/call` 时计数,而
    /// **探测**走的是 `list_tools`。要断言「有没有去探测」必须数连接次数,
    /// 数 `call_tool` 永远是 0(踩过这个坑:变异测试打不掉)。
    fn install_counting_failing_connector(calls: Arc<AtomicUsize>) {
        let factory: TestConnector = Arc::new(move || {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow!("counting connector: no real server"))
            })
        });
        *TEST_CONNECTOR.lock().unwrap() = Some(factory);
    }

    /// `TEST_CONNECTOR` 是进程级全局量,并行测试会互相清空/覆盖。
    /// 所有用到这个缝的测试都先拿这把锁,保证同一时刻只有一个在跑。
    static SEAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 拿跨测试串行锁。
    ///
    /// 调用它的异步测试要带 `#[allow(clippy::await_holding_lock)]`——lint 在**调用点**
    /// 触发，加在这里不生效。那是有意为之：锁必须保住整个 await 区间。
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
    #[allow(clippy::await_holding_lock)]
    // `seam_guard()` 要持到 await 之后：这把锁保护的是进程级 `TEST_CONNECTOR`，
    // 中途放手别的 seam 测试会插进来换掉它。`#[tokio::test]` 是 current_thread
    // 单线程运行时（实测 `num_workers == 1`），没有第二个任务能竞争，故不会死锁。
    #[allow(clippy::await_holding_lock)]
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
        let hit = cached
            .cached_tools_ignoring_freshness("s", &sc)
            .expect("cache hit after probe");
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

    // ---- needs_reload ----

    fn write_mcp(dir: &std::path::Path, body: &str) {
        std::fs::create_dir_all(dir.join(".yi-agent")).unwrap();
        std::fs::write(dir.join(".yi-agent/mcp.json"), body).unwrap();
    }

    /// 建一个 manager,并把 `last_seen` 对齐到磁盘现状(模拟「刚 reload 过」)。
    async fn manager_in_sync(dir: &std::path::Path) -> Arc<McpManager> {
        let cfg = McpConfig::load(dir).unwrap().unwrap();
        McpManager::load_and_probe(cfg, dir).await.unwrap()
    }

    #[tokio::test]
    async fn needs_reload_is_false_when_nothing_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;
        assert!(!mgr.needs_reload(dir.path(), now_unix()));
    }

    #[tokio::test]
    async fn needs_reload_is_true_when_the_file_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"uvx"}}}"#);
        assert!(mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 配置没变、但缓存过期 ⇒ 触发重载(本 bug 的自愈入口)。
    #[tokio::test]
    async fn needs_reload_is_true_when_a_cache_entry_expired() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &cfg("npx"), vec![], now_unix() - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 配置没变、缓存新鲜 ⇒ 不重载(防每轮重探)。
    #[tokio::test]
    async fn needs_reload_is_false_when_the_cache_is_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &cfg("npx"), vec![], now_unix() - 23 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 过期 + 刚失败 ⇒ 不重载(否则每轮白跑一遍 reload + 重建 agent)。
    #[tokio::test]
    async fn needs_reload_is_false_while_a_stale_entry_is_backed_off() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        let sc = cfg("npx");
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.note_failure("fs", &sc, now - 60);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }

    /// 停用的 server 过期后不能触发 reload(否则永久空转)。
    #[tokio::test]
    async fn needs_reload_is_false_for_a_disabled_expired_server() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        let mut sc = cfg("npx");
        sc.enabled = false;
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }

    /// 总开关关闭时,任何过期条目都不能触发 reload。
    #[tokio::test]
    async fn master_off_suppresses_needs_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"enabled":false,"mcpServers":{"fs":{"command":"npx"}}}"#,
        );
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &cfg("npx"), vec![], now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }

    // ---- reload 分派 ----

    /// 过期 + 探测失败 ⇒ 工具沿用、error 非空、failed_at 被写。
    #[tokio::test]
    async fn a_failed_probe_falls_back_to_the_old_tool_list() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "old_tool".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 25 * 3600,
        );
        cache.save(dir.path()).unwrap();

        let report = mgr.reload(dir.path(), now).await;
        let s = &report.servers[0];
        assert_eq!(s.tool_count, 1, "旧工具必须保留");
        assert!(s.error.is_some());
        assert!(s.probed);

        let after = McpCache::load(dir.path());
        assert!(
            matches!(
                after.lookup("fs", &sc, now + 1, TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::BackedOff
            ),
            "失败必须写 failed_at"
        );
    }

    /// 探测失败**不能**刷新 probed_at(否则「已过期」被固化成「新鲜」,24h 内不再重探)。
    #[tokio::test]
    async fn a_failed_probe_does_not_make_the_entry_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.save(dir.path()).unwrap();
        let before = McpCache::load(dir.path())
            .lookup("fs", &sc, now, TTL_SECS, RETRY_AFTER_SECS)
            .is_settled();

        let _ = mgr.reload(dir.path(), now).await;

        let after = McpCache::load(dir.path());
        assert!(
            matches!(
                after.lookup("fs", &sc, now, TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::BackedOff
            ),
            "失败后必须处于退避状态(probed_at 未被刷新)"
        );
        assert!(!before, "前置条件:探测前必须不是 settled 状态");
    }

    /// 无缓存 + 探测失败 ⇒ 工具为空。
    ///
    /// 用空 manager:若先走 `manager_in_sync` 的启动探测,失败会被记成
    /// `failed_at`(启动路径也会写),紧接着的 reload 就命中退避而不探测了,
    /// 测不到「无缓存 + 探测失败」这条路径。
    #[tokio::test]
    async fn a_failed_probe_with_no_cache_yields_no_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = Arc::new(McpManager::new_for_test(true));

        let now = now_unix();
        let report = mgr.reload(dir.path(), now).await;
        assert_eq!(report.servers[0].tool_count, 0);
        assert!(report.servers[0].error.is_some());
    }

    /// 内存有工具、磁盘缓存无条目、探测失败 ⇒ 工具数不变。
    ///
    /// 钉住「回退源是内存快照」:若改成只回退磁盘缓存,会变成 0 个。
    #[tokio::test]
    async fn a_failed_probe_keeps_the_in_memory_tools_when_the_cache_file_is_gone() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_echo_connector(Arc::clone(&calls));
        let mgr = manager_in_sync(dir.path()).await;
        let before = mgr.enabled_tools().len();
        assert!(before > 0, "先要有内存工具");

        std::fs::remove_file(dir.path().join(".yi-agent/mcp-cache.json")).unwrap();
        install_failing_connector();

        let now = now_unix();
        let report = mgr.reload(dir.path(), now).await;
        assert_eq!(
            report.servers[0].tool_count, before,
            "内存快照必须保留 {before} 个工具,实际 {}",
            report.servers[0].tool_count
        );
    }

    /// 退避窗口内 reload 不探测。
    #[tokio::test]
    async fn reload_skips_a_probe_within_the_backoff_window() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.note_failure("fs", &sc, now - 60);
        cache.save(dir.path()).unwrap();

        let report = mgr.reload(dir.path(), now).await;
        assert!(!report.servers[0].probed, "退避窗口内不应探测");
        assert!(report.servers[0].skipped_backoff, "应标记为因退避跳过");
    }

    /// 新鲜缓存用**磁盘清单**,不是内存快照(跨进程同步)。
    #[tokio::test]
    async fn reload_uses_the_disk_list_not_the_memory_snapshot_when_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_failing_connector();
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "disk_tool".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 3600,
        );
        cache.save(dir.path()).unwrap();

        let report = mgr.reload(dir.path(), now).await;
        assert!(!report.servers[0].probed, "新鲜不应探测");
        assert_eq!(report.servers[0].tool_count, 1);
        let tools = mgr.enabled_tools();
        assert_eq!(tools[0].name(), "mcp__fs__disk_tool");
    }

    /// 本 bug 的直接回归测试:过期后探测拿到新工具名。
    #[tokio::test]
    async fn reload_refreshes_a_renamed_tool_after_the_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_echo_connector(Arc::clone(&calls));
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "old_name".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 25 * 3600,
        );
        cache.save(dir.path()).unwrap();

        let report = mgr.reload(dir.path(), now).await;
        assert!(report.servers[0].probed, "过期后必须重探");
        let names: Vec<String> = mgr
            .enabled_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with("old_name")),
            "旧工具名必须被替换, 实际: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("echo")),
            "必须换成新探测到的工具, 实际: {names:?}"
        );
    }

    /// 冷缓存探测成功后必须写入 probed_at,否则 TTL 永不开始计时。
    #[tokio::test]
    async fn load_and_probe_records_probed_at_on_success() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_echo_connector(Arc::clone(&calls));

        let _mgr = manager_in_sync(dir.path()).await;
        let cache = McpCache::load(dir.path());
        assert!(
            matches!(
                cache.lookup("fs", &cfg("npx"), now_unix(), TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::Fresh(_)
            ),
            "成功后 probed_at 必须被写入"
        );
    }

    // ---- 停用 server 的两条路径 ----

    /// 停用的 server 即使缓存过期也不探测,工具沿用旧清单。
    #[tokio::test]
    async fn a_disabled_server_is_never_probed_even_after_the_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut sc = cfg("npx");
        sc.enabled = false;
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "kept".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 25 * 3600,
        );
        cache.save(dir.path()).unwrap();

        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let report = mgr.reload(dir.path(), now).await;
        assert!(!report.servers[0].probed, "停用 server 不应探测");
        assert_eq!(report.servers[0].tool_count, 1, "旧清单必须保留");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), before);

        // failed_at 不该被写:没探测就没有失败
        let after = McpCache::load(dir.path());
        assert!(
            !matches!(
                after.lookup("fs", &sc, now, TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::BackedOff
            ),
            "未探测时不得写 failed_at"
        );
    }

    /// 启动路径刻意**不**校验 TTL:25h 前的缓存照样直接复用,不探测。
    #[tokio::test]
    async fn load_and_probe_does_not_probe_an_expired_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "old".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 25 * 3600,
        );
        cache.save(dir.path()).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));

        let mgr = manager_in_sync(dir.path()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "启动路径不应因 TTL 过期而探测"
        );
        assert_eq!(mgr.enabled_tools().len(), 1, "旧清单必须沿用");
    }

    /// 启动路径同样不探测停用的 server(与 reload 保持一致)。
    #[tokio::test]
    async fn load_and_probe_does_not_probe_a_disabled_server() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));

        let _mgr = manager_in_sync(dir.path()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "启动路径不应为停用 server 探测"
        );
    }

    /// 总开关关闭时,即使配置里有过期条目也不探测,且 failed_at 不被写。
    #[tokio::test]
    async fn master_off_probes_nothing_even_when_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"enabled":false,"mcpServers":{"fs":{"command":"npx"}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![CachedTool {
                name: "kept".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            now - 25 * 3600,
        );
        cache.save(dir.path()).unwrap();

        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let report = mgr.reload(dir.path(), now).await;
        assert!(!report.servers[0].probed);
        assert_eq!(report.servers[0].tool_count, 1, "旧清单必须保留");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), before);

        let after = McpCache::load(dir.path());
        assert!(
            !matches!(
                after.lookup("fs", &sc, now, TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::BackedOff
            ),
            "未探测时不得写 failed_at"
        );
    }

    /// 10 条存量条目迁移后,连续两轮 needs_reload 都为 false。
    ///
    /// 防「升级即卡 9 分钟」:若不迁移,10 条全部 Stale,第二轮仍会触发重探。
    #[tokio::test]
    async fn migration_of_many_servers_probes_none_of_them() {
        let dir = tempfile::TempDir::new().unwrap();

        // 手写一份**没有 probed_at / failed_at** 的旧格式缓存(10 个 server)
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let mut servers = serde_json::Map::new();
        for i in 0..10 {
            let sc = cfg("npx");
            servers.insert(
                format!("s{i}"),
                serde_json::json!({
                    "fingerprint": McpCache::fingerprint_for_test(&sc),
                    "tools": [{
                        "name": format!("tool{i}"),
                        "description": null,
                        "input_schema": {"type": "object"},
                        "read_only": true
                    }]
                }),
            );
        }
        std::fs::write(
            dir.path().join(".yi-agent/mcp-cache.json"),
            serde_json::to_string(&serde_json::json!({"servers": servers})).unwrap(),
        )
        .unwrap();

        // 配置里也有这 10 个 server
        let mut cfg_servers = serde_json::Map::new();
        for i in 0..10 {
            cfg_servers.insert(format!("s{i}"), serde_json::json!({"command": "npx"}));
        }
        write_mcp(
            dir.path(),
            &serde_json::to_string(&serde_json::json!({"mcpServers": cfg_servers})).unwrap(),
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let _serial = seam_guard();
        let _seam = SeamGuard;
        install_counting_failing_connector(Arc::clone(&calls));

        let mgr = manager_in_sync(dir.path()).await;

        // 关键断言一:只做过 `load_and_probe`(还没 reload)时,迁移就必须已经落盘
        // 且 `needs_reload` 为 false。否则升级后第一轮会因「10 条全 Stale」而集体
        // 重探——正是 load_and_probe 那处 backfill 的作用,reload 那处兜不住。
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "load_and_probe 不应探测存量条目"
        );
        assert!(
            !mgr.needs_reload(dir.path(), now_unix()),
            "load_and_probe 迁移后 needs_reload 必须为 false(否则升级即卡 30s+)"
        );

        // 关键断言二:再 reload 一轮也不探测(此时看的是 reload 那处 backfill)。
        let _ = mgr.reload(dir.path(), now_unix()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reload 一轮后仍不应有任何探测"
        );
        assert!(
            !mgr.needs_reload(dir.path(), now_unix()),
            "连续两轮后 needs_reload 必须为 false"
        );
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
