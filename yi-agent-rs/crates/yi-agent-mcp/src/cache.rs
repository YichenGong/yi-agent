//! 磁盘上的工具 schema 缓存:`.yi-agent/mcp-cache.json`。
//!
//! 用 `ServerConfig`(kind/命令/参数/环境变量/URL/header)的 SHA-256 指纹做失效
//! 判断:配置变了就丢弃旧缓存,重新拉取远端工具列表。指纹用摘要而非原文,避免
//! `env`/`headers` 里的 token/密钥明文落盘。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{McpConfig, ServerConfig};

/// 一个被缓存的远端工具。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub read_only: bool,
}

/// 单个 server 的缓存条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedServer {
    /// `ServerConfig`(kind+command+args+env+url+headers)的 SHA-256 十六进制指纹。
    fingerprint: String,
    /// 上次**成功**探测远端工具清单的时间(Unix 秒)。旧缓存缺该字段 ⇒ `None`。
    #[serde(default)]
    probed_at: Option<i64>,
    /// 上次探测**失败**的时间(Unix 秒),用于失败退避。
    #[serde(default)]
    failed_at: Option<i64>,
    tools: Vec<CachedTool>,
}

/// 整个缓存文件。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct McpCache {
    #[serde(default)]
    servers: BTreeMap<String, CachedServer>,
}

/// 给定工作目录下的缓存文件路径。
fn cache_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp-cache.json")
}

/// 工具清单的保鲜期:24 小时。
pub const TTL_SECS: i64 = 24 * 3600;

/// 探测失败后的退避窗口:5 分钟。
pub const RETRY_AFTER_SECS: i64 = 5 * 60;

/// 一次缓存查找的结果。
///
/// 「旧值是否保留」是 `reload` 的策略,不属于查找语义,故不放进枚举。
///
/// **只派生 `Debug, Clone, Copy`,不要加 `PartialEq`/`Eq`。** `Fresh` 的载荷是
/// `&[CachedTool]`,而 `CachedTool` 含 `serde_json::Value`(`input_schema`),
/// `Value` 未实现 `Eq`——派生会在 derive 展开处编译失败。测试里用 `matches!` 匹配。
#[derive(Debug, Clone, Copy)]
pub enum CacheLookup<'a> {
    /// 指纹匹配且未过期:直接用 `Fresh` 带的清单。
    Fresh(&'a [CachedTool]),
    /// 已过期但仍在失败退避窗口内:本轮不探测。
    BackedOff,
    /// 指纹不匹配、从未探测,或已过期且允许重试:需要重新探测。
    Stale,
}

impl CacheLookup<'_> {
    /// 「本轮是否不需要探测」:`Fresh` 与 `BackedOff` 都为真。
    ///
    /// **只用于跳过探测的判断**(`needs_reload` 的触发条件、`reload` 的短路与否),
    /// **不能用来选数据源**——`Fresh` 要磁盘清单、`BackedOff` 要内存快照,两者
    /// 相反(见 spec §4.2)。`reload` 里需要清单时必须逐变体匹配。
    pub fn is_settled(&self) -> bool {
        !matches!(self, CacheLookup::Stale)
    }
}

/// 工具清单是否还新鲜。时间戳在未来(时钟回拨)或缺失 ⇒ 不新鲜。
fn is_fresh(probed_at: Option<i64>, now: i64, ttl: i64) -> bool {
    probed_at.is_some_and(|t| t <= now && now - t < ttl)
}

/// 现在是否处于失败退避窗口内。时间戳在未来 ⇒ **不**退避(宁可多试一次)。
fn is_backed_off(failed_at: Option<i64>, now: i64, retry_after: i64) -> bool {
    failed_at.is_some_and(|t| t <= now && now - t < retry_after)
}

/// 计算 `ServerConfig` 的确定性指纹(SHA-256 十六进制摘要)。
///
/// 覆盖传输相关的全部字段(kind+command+args+env+url+headers);`enabled` 是
/// 运行时开关,不影响 schema,故意排除以免切换开关时无谓地让缓存失效。摘要
/// 而非原文,避免把 `env`/`headers` 里的 token/密钥明文落到 `.yi-agent/mcp-cache.json`。
/// header 值里的 `${ENV_VAR}` 以**未展开原文**参与指纹,轮换 token 不失效缓存。
fn fingerprint(cfg: &ServerConfig) -> String {
    let key = serde_json::json!({
        "kind": cfg.kind,
        "command": cfg.command,
        "args": cfg.args,
        "env": cfg.env,
        "url": cfg.url,
        "headers": cfg.headers,
    });
    // 序列化 `serde_json::Value` 不会失败;显式 panic 好过静默回退成空串
    // (空串会让所有配置都判定为命中,从而一直提供过期 schema)。
    let json = serde_json::to_string(&key).expect("serializing a serde_json::Value is infallible");
    let digest = Sha256::digest(json.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 整份配置的指纹(含顶层 `enabled`,按 server 名排序后再序列化)。
///
/// 与 [`fingerprint`] 的区别:那个只看单个 `ServerConfig`、用于缓存失效;
/// 这个看**整个文件**,用于"磁盘上的配置是否变了"的廉价检测——顶层开关
/// 从 on 改成 off 也必须被它感知到。
pub(crate) fn fingerprint_for_manager(cfg: &McpConfig) -> String {
    let mut parts = Vec::new();
    for (name, sc) in &cfg.mcp_servers {
        parts.push(serde_json::json!({
            "name": name,
            "kind": sc.kind,
            "command": sc.command,
            "args": sc.args,
            "env": sc.env,
            "url": sc.url,
            "headers": sc.headers,
            "enabled": sc.enabled,
        }));
    }
    let key = serde_json::json!({ "enabled": cfg.enabled, "servers": parts });
    let json = serde_json::to_string(&key).expect("serializing a serde_json::Value is infallible");
    let digest = Sha256::digest(json.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl McpCache {
    /// 加载缓存;文件缺失或损坏时返回空缓存。
    ///
    /// 失效只依据配置指纹,不感知远端工具列表的变化:像 `npx -y pkg` /
    /// `@latest` 这类未锁定版本的 spec,即使工具集变了指纹也不变,缓存会被
    /// 一直沿用。需要强制刷新时,删除 `.yi-agent/mcp-cache.json` 即可。
    pub fn load(workdir: &Path) -> Self {
        std::fs::read_to_string(cache_path(workdir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 当 `cfg` 的指纹匹配时,返回 `name` 的缓存工具,**不看新鲜度**。
    ///
    /// 给「磁盘上有什么」这类查询用(设置页面板、启动路径)。每轮的自愈判据
    /// 必须用 [`lookup`](Self::lookup),不要用这个。
    pub fn cached_tools_ignoring_freshness(
        &self,
        name: &str,
        cfg: &ServerConfig,
    ) -> Option<&[CachedTool]> {
        let entry = self.servers.get(name)?;
        (entry.fingerprint == fingerprint(cfg)).then_some(entry.tools.as_slice())
    }

    /// 写入/覆盖 `name` 的缓存条目(记录一次**成功**探测)。
    ///
    /// 成功后清掉 `failed_at`:退避的理由已经消失。
    pub fn put(&mut self, name: &str, cfg: &ServerConfig, tools: Vec<CachedTool>, now: i64) {
        self.servers.insert(
            name.to_string(),
            CachedServer {
                fingerprint: fingerprint(cfg),
                probed_at: Some(now),
                failed_at: None,
                tools,
            },
        );
    }

    /// 判定 `name` 当前该怎么处置。**判定顺序本身是语义,不能重排**:
    /// 1. 指纹不匹配 或 无条目 ⇒ `Stale`(配置变了,退避对它无效)
    /// 2. `probed_at` 在 TTL 内 ⇒ `Fresh`
    /// 3. `failed_at` 在退避窗口内 ⇒ `BackedOff`
    /// 4. 其余 ⇒ `Stale`
    pub fn lookup<'a>(
        &'a self,
        name: &str,
        cfg: &ServerConfig,
        now: i64,
        ttl: i64,
        retry_after: i64,
    ) -> CacheLookup<'a> {
        let Some(entry) = self.servers.get(name) else {
            return CacheLookup::Stale;
        };
        // 顺序不能换:先比指纹,退避只对「同一份配置」有意义。
        if entry.fingerprint != fingerprint(cfg) {
            return CacheLookup::Stale;
        }
        if is_fresh(entry.probed_at, now, ttl) {
            return CacheLookup::Fresh(entry.tools.as_slice());
        }
        if is_backed_off(entry.failed_at, now, retry_after) {
            return CacheLookup::BackedOff;
        }
        CacheLookup::Stale
    }

    /// 记录一次**失败**的探测尝试:只更新 `failed_at`,不动工具清单与 `probed_at`。
    ///
    /// 条目不存在时也建一条:否则「从未成功过」的坏 server 每轮都会重试。
    pub fn note_failure(&mut self, name: &str, cfg: &ServerConfig, now: i64) {
        let fp = fingerprint(cfg);
        let entry = self
            .servers
            .entry(name.to_string())
            .or_insert_with(|| CachedServer {
                fingerprint: fp.clone(),
                probed_at: None,
                failed_at: None,
                tools: Vec::new(),
            });
        entry.failed_at = Some(now);
        // 指纹可能已变(配置改过),跟着更新以免它被当成旧配置的失败记录。
        entry.fingerprint = fp;
    }

    /// 把 `probed_at == None` 的存量条目就地填成 `now`(一次性迁移)。
    ///
    /// **只填 `probed_at`,绝不改 `fingerprint` 或 `tools`。** 返回是否发生了改动。
    /// 不接受 `cfg`:迁移与配置无关,多一个用不上的参数会诱使实现者「顺手」做
    /// 指纹匹配——那等于把旧清单冒充成新配置的清单。
    pub fn backfill_probed_at(&mut self, name: &str, now: i64) -> bool {
        match self.servers.get_mut(name) {
            Some(e) if e.probed_at.is_none() => {
                e.probed_at = Some(now);
                true
            }
            _ => false,
        }
    }

    /// 测试辅助:按 `cfg` 计算指纹,供构造「旧格式缓存」的测试复刻真实指纹。
    #[cfg(test)]
    pub(crate) fn fingerprint_for_test(cfg: &ServerConfig) -> String {
        fingerprint(cfg)
    }

    /// 持久化缓存,必要时创建父目录;失败时返回错误交由调用方记录。
    pub fn save(&self, workdir: &Path) -> Result<()> {
        let path = cache_path(workdir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // 先写同目录临时文件再 rename:rename 在同一文件系统上是原子的,读方要么
        // 看到旧文件、要么看到完整新文件,不会读到半截 JSON。缓存坏了会被当成
        // 空缓存,下一轮全体重探,代价很高。
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            // 失败时清掉临时文件,别在 `.yi-agent/` 里留垃圾。
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("rename to {}", path.display()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerKind;
    use std::collections::BTreeMap;

    fn server(cmd: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(cmd.into()),
            args: vec!["--x".into()],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put(
            "fs",
            &server("npx"),
            vec![CachedTool {
                name: "read".into(),
                description: Some("reads".into()),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
            NOW,
        );
        cache.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        let hit = loaded
            .cached_tools_ignoring_freshness("fs", &server("npx"))
            .expect("cache hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "read");
        assert_eq!(hit[0].description.as_deref(), Some("reads"));
        assert_eq!(hit[0].input_schema, serde_json::json!({"type":"object"}));
        assert!(hit[0].read_only);
    }

    #[test]
    fn fingerprint_mismatch_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", &server("npx"), vec![], NOW);
        cache.save(dir.path()).unwrap();
        let loaded = McpCache::load(dir.path());
        assert!(
            loaded
                .cached_tools_ignoring_freshness("fs", &server("uvx"))
                .is_none()
        );
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let c = McpCache::load(dir.path());
        assert!(
            c.cached_tools_ignoring_freshness("fs", &server("npx"))
                .is_none()
        );
    }

    /// 损坏的缓存文件(非法 JSON)不应导致 panic,而是退化为空缓存。
    #[test]
    fn corrupt_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("mcp-cache.json"), "{ not json").unwrap();

        let c = McpCache::load(dir.path());
        assert!(
            c.cached_tools_ignoring_freshness("fs", &server("npx"))
                .is_none()
        );
    }

    /// `enabled` 是运行时开关,切换它不应让 schema 缓存失效。
    #[test]
    fn toggling_enabled_does_not_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", &server("npx"), vec![], NOW);
        cache.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        let mut disabled = server("npx");
        disabled.enabled = false;
        assert!(
            loaded
                .cached_tools_ignoring_freshness("fs", &disabled)
                .is_some()
        );
    }

    #[test]
    fn fingerprint_changes_when_url_changes() {
        let a = http_server("https://x/one");
        let b = http_server("https://x/two");
        let mut c = McpCache::default();
        c.put("ws", &a, vec![], NOW);
        assert!(
            c.cached_tools_ignoring_freshness("ws", &a).is_some(),
            "same config must hit"
        );
        assert!(
            c.cached_tools_ignoring_freshness("ws", &b).is_none(),
            "changed url must miss"
        );
    }

    /// 指纹必须覆盖 http header 的**值**:URL 相同、仅 header 值不同时,缓存
    /// 也必须失效并重新探测(spec §4 / §7.1)。用两个仅 header 值不同、其余
    /// 完全一致的 http 配置,同时锁定指纹摘要本身与 `get_for` 的命中/失效。
    #[test]
    fn fingerprint_changes_when_header_value_changes() {
        let a = http_server_with_auth("Bearer token-a");
        let b = http_server_with_auth("Bearer token-b");

        assert_ne!(
            fingerprint(&a),
            fingerprint(&b),
            "same url with a different header value must change the fingerprint"
        );

        let mut c = McpCache::default();
        c.put("ws", &a, vec![], NOW);
        assert!(
            c.cached_tools_ignoring_freshness("ws", &a).is_some(),
            "same config must hit"
        );
        assert!(
            c.cached_tools_ignoring_freshness("ws", &b).is_none(),
            "changed header value must miss"
        );
    }

    fn http_server(url: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some(url.to_string()),
            headers: BTreeMap::new(),
            enabled: true,
        }
    }

    /// 一个 http server:URL 固定,`Authorization` header 值可变(其余字段一致),
    /// 用于证明仅 header 值不同就会改变指纹。
    fn http_server_with_auth(auth: &str) -> ServerConfig {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), auth.to_string());
        ServerConfig {
            kind: ServerKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://x/mcp".to_string()),
            headers,
            enabled: true,
        }
    }

    // ---- TTL / 退避判定 ----

    /// 固定 `now`,避免测试依赖真实时钟。
    const NOW: i64 = 1_700_000_000;

    fn entry_with(probed_at: Option<i64>, failed_at: Option<i64>) -> CachedServer {
        CachedServer {
            fingerprint: fingerprint(&server("npx")),
            probed_at,
            failed_at,
            tools: vec![CachedTool {
                name: "read".into(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
        }
    }

    fn cache_with_entry(probed_at: Option<i64>, failed_at: Option<i64>) -> McpCache {
        let mut c = McpCache::default();
        c.servers
            .insert("fs".into(), entry_with(probed_at, failed_at));
        c
    }

    #[test]
    fn an_entry_older_than_the_ttl_is_not_fresh() {
        let c = cache_with_entry(Some(NOW - 25 * 3600), None);
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn an_entry_within_the_ttl_is_fresh() {
        let c = cache_with_entry(Some(NOW - 23 * 3600), None);
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Fresh(t) if t.len() == 1
        ));
    }

    #[test]
    fn a_legacy_entry_without_probed_at_is_stale() {
        let c = cache_with_entry(None, None);
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn a_future_probed_at_is_treated_as_stale() {
        let c = cache_with_entry(Some(NOW + 3600), None);
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn retry_is_skipped_within_the_backoff_window() {
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 60));
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::BackedOff
        ));
    }

    #[test]
    fn retry_happens_after_the_backoff_window() {
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 6 * 60));
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    /// 指纹不匹配必须优先于退避判定,否则用户改了 mcp.json 要等 5 分钟才生效。
    #[test]
    fn a_fingerprint_change_ignores_the_backoff_window() {
        let mut c = McpCache::default();
        c.servers.insert(
            "fs".into(),
            entry_with(Some(NOW - 25 * 3600), Some(NOW - 60)),
        );
        assert!(matches!(
            c.lookup("fs", &server("uvx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn a_future_failed_at_does_not_back_off() {
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW + 3600));
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn a_successful_put_records_probed_at_and_clears_failed_at() {
        let mut c = McpCache::default();
        c.note_failure("fs", &server("npx"), NOW - 60);
        c.put("fs", &server("npx"), vec![], NOW);
        let entry = c.servers.get("fs").expect("entry");
        assert_eq!(entry.probed_at, Some(NOW));
        assert_eq!(entry.failed_at, None, "成功后必须清掉 failed_at");
    }

    #[test]
    fn note_failure_only_updates_failed_at() {
        let mut c = cache_with_entry(Some(NOW - 25 * 3600), None);
        c.note_failure("fs", &server("npx"), NOW);
        let entry = c.servers.get("fs").expect("entry");
        assert_eq!(entry.failed_at, Some(NOW));
        assert_eq!(
            entry.probed_at,
            Some(NOW - 25 * 3600),
            "失败不得刷新 probed_at"
        );
        assert_eq!(entry.tools.len(), 1, "失败不得清空工具清单");
    }

    #[test]
    fn a_failure_persists_the_backoff_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = McpCache::default();
        c.note_failure("fs", &server("npx"), NOW);
        c.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        assert!(matches!(
            loaded.lookup("fs", &server("npx"), NOW + 60, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::BackedOff
        ));
    }

    #[test]
    fn a_legacy_entry_without_probed_at_is_migrated_to_now() {
        let mut c = cache_with_entry(None, None);
        assert!(c.backfill_probed_at("fs", NOW), "应报告发生了改动");
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Fresh(_)
        ));
        assert!(!c.backfill_probed_at("fs", NOW), "第二次应无改动");
    }

    /// backfill 绝不能顺手修正指纹——那会把旧清单冒充成新配置的清单。
    #[test]
    fn backfill_does_not_touch_the_fingerprint() {
        let mut c = McpCache::default();
        c.servers.insert("fs".into(), entry_with(None, None));
        let before = c.servers.get("fs").unwrap().fingerprint.clone();
        c.backfill_probed_at("fs", NOW);
        let after = c.servers.get("fs").unwrap();
        assert_eq!(after.fingerprint, before);
        assert!(matches!(
            c.lookup("fs", &server("uvx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn is_settled_is_true_for_fresh_and_backed_off_only() {
        let c = cache_with_entry(Some(NOW - 23 * 3600), None);
        assert!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS)
                .is_settled()
        );
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 60));
        assert!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS)
                .is_settled()
        );
        let c = cache_with_entry(None, None);
        assert!(
            !c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS)
                .is_settled()
        );
    }

    // ---- save 原子写 ----

    #[test]
    fn save_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = McpCache::default();
        c.put("fs", &server("npx"), vec![], NOW);
        c.save(dir.path()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    /// 覆盖写:旧内容必须被完整替换,且文件仍可解析。
    #[test]
    fn save_overwrites_atomically_and_stays_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = McpCache::default();
        c.put("fs", &server("npx"), vec![], NOW);
        c.save(dir.path()).unwrap();

        let mut c2 = McpCache::default();
        c2.put("ws", &server("uvx"), vec![], NOW);
        c2.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        assert!(
            loaded
                .lookup("ws", &server("uvx"), NOW, TTL_SECS, RETRY_AFTER_SECS)
                .is_settled()
        );
        assert!(
            matches!(
                loaded.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
                CacheLookup::Stale
            ),
            "旧条目应被整体替换掉"
        );
    }

    #[test]
    fn save_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        let mut c = McpCache::default();
        c.put("fs", &server("npx"), vec![], NOW);
        c.save(&nested).unwrap();
        assert!(nested.join(".yi-agent").join("mcp-cache.json").exists());
    }
}
