# MCP 缓存 TTL 自愈 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `.yi-agent/mcp-cache.json` 里的工具清单在 24 小时后自动重新探测，使上游改名的工具在一天内自愈，同时不引入每轮 55 秒的探测开销。

**Architecture:** 在缓存条目里加两个时间戳（`probed_at` 成功 / `failed_at` 失败），把「要不要探测」收敛成一个 `CacheLookup` 枚举（`Fresh` / `BackedOff` / `Stale`），由 `lookup()` 单点判定；`needs_reload` 与 `reload` 都只消费这一个枚举，保证不漂移。

**Tech Stack:** Rust 2021、`serde`（`#[serde(default)]` 做旧缓存兼容）、`anyhow`、`tempfile`、`tokio::test`、rmcp（测试用 `TEST_CONNECTOR` seam，不需要真实网络）。

## Global Constraints

- 所有时间一律用 **Unix 秒 `i64`**，不用 `Duration`；`now` **必须由调用方传入**，函数内不得调用 `SystemTime::now()`。
- 常量精确值：`TTL_SECS: i64 = 24 * 3600`；`RETRY_AFTER_SECS: i64 = 5 * 60`。
- `CachedServer` 新字段必须带 `#[serde(default)]`，**不得**加 `skip_serializing_if`（保留 `null` 以便排查退避是否写过）。
- 两个时间戳函数的判据必须含 `t <= now`，挡掉时钟回拨导致的负 delta。
- **`is_settled()` 只用于「要不要探」，不得用于选数据源**（`Fresh` 用磁盘清单，`BackedOff` 用内存快照）。
- `needs_reload` 与 `reload` 的短路条件必须逐条对齐：`reload` 不会做任何事的 server，`needs_reload` 就不能因它返回 `true`。
- 探测失败**只**写 `failed_at`，不改 `probed_at`；工具调用（`call_tool`）失败**不写**任何时间戳。
- 测试全部是常规单测，不用 `#[ignore]`，不联网，不 spawn 真实子进程。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 每完成一组改动同步更新 `docs/project-management/` 对应模块文件（见 Task 9）。

## 文件结构

| 文件 | 责任 | 改动 |
|---|---|---|
| `crates/yi-agent-mcp/src/cache.rs` | 缓存读写 + 时间戳判定 | `CachedServer` 加两字段；新增 `CacheLookup`、`lookup`、`note_failure`、`backfill_probed_at`；`put` 加 `now`；`get_for` 改名；`save` 改原子写 |
| `crates/yi-agent-mcp/src/manager.rs` | 判据接入 + 探测编排 | 新增 `fingerprint_and_config`；`config_changed_on_disk` → `needs_reload`；`reload` / `load_and_probe` 按枚举分派 |
| `crates/yi-agent-mcp/src/lib.rs` | 对外只读查询 | `cached_tools_for` 改调 `cached_tools_ignoring_freshness` |
| `crates/yi-agent-mcp/src/reload_tool.rs` | `mcp_reload` 工具 | `put` 补 `now`；渲染「因退避跳过」 |
| `crates/yi-agent-app-server/src/server.rs` | app-server 每轮检查 | 调 `needs_reload` 并传 `now` |
| `crates/yi-agent/src/main.rs` | TUI 每轮检查 + `/mcp reload` | 同上；手动 reload 输出退避说明 |
| `crates/yi-agent-app-server/src/mcp_rpc.rs` | 设置页 RPC | `put` 补 `now` |
| `docs/project-management/mcp.md` | 模块进度 | 新增一条已完成项 |

**任务顺序的理由**：Task 1-2 先把判定逻辑做成可单测的纯函数（无 I/O、无 async），风险最低且是后面所有任务的地基；Task 3-5 接入两条路径；Task 6-8 处理放大路径与用户可见行为；Task 9 收尾。每完成一个 Task 都是可编译、测试全绿的状态。

---

### Task 1: 缓存时间戳字段与 `CacheLookup` 判定

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs:26-32`（`CachedServer`）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs:93-134`（`impl McpCache`）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（`mod tests`，文件尾部）

**Interfaces:**
- Produces:
  - `pub enum CacheLookup<'a> { Fresh(&'a [CachedTool]), BackedOff, Stale }`
  - `impl CacheLookup<'_> { pub fn is_settled(&self) -> bool }`
  - `McpCache::lookup(&'a self, name: &str, cfg: &ServerConfig, now: i64, ttl: i64, retry_after: i64) -> CacheLookup<'a>`
  - `McpCache::put(&mut self, name: &str, cfg: &ServerConfig, tools: Vec<CachedTool>, now: i64)`
  - `McpCache::note_failure(&mut self, name: &str, cfg: &ServerConfig, now: i64)`
  - `McpCache::backfill_probed_at(&mut self, name: &str, now: i64) -> bool`
  - `pub const TTL_SECS: i64 = 24 * 3600;` / `pub const RETRY_AFTER_SECS: i64 = 5 * 60;`
- Consumes: 无（本任务是地基）

- [ ] **Step 1: 写失败的测试**

在 `cache.rs` 的 `mod tests` 末尾追加（`server()` 辅助函数已存在于 `cache.rs:142`，直接用）：

```rust

    // ---- TTL / 退避判定 ----

    /// 固定 `now`，避免测试依赖真实时钟。
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
        c.servers.insert("fs".into(), entry_with(probed_at, failed_at));
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
        // 过期 + 1 分钟前刚失败 ⇒ 退避
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 60));
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::BackedOff
        ));
    }

    #[test]
    fn retry_happens_after_the_backoff_window() {
        // 过期 + 6 分钟前失败 ⇒ 可以再试
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 6 * 60));
        assert!(matches!(
            c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    /// 指纹不匹配必须优先于退避判定，否则用户改了 mcp.json 要等 5 分钟才生效。
    #[test]
    fn a_fingerprint_change_ignores_the_backoff_window() {
        let mut c = McpCache::default();
        // 指纹按 "npx" 记，查询用 "uvx" ⇒ 不匹配；同时刚失败过
        c.servers.insert("fs".into(), entry_with(Some(NOW - 25 * 3600), Some(NOW - 60)));
        assert!(matches!(
            c.lookup("fs", &server("uvx"), NOW, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Stale
        ));
    }

    #[test]
    fn a_future_failed_at_does_not_back_off() {
        // 时钟回拨：failed_at 在未来 ⇒ 宁可多试一次
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
            "失败不得刷新 probed_at（否则过期状态被固化成新鲜）"
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
        assert!(c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS).is_settled());
        let c = cache_with_entry(Some(NOW - 25 * 3600), Some(NOW - 60));
        assert!(c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS).is_settled());
        let c = cache_with_entry(None, None);
        assert!(!c.lookup("fs", &server("npx"), NOW, TTL_SECS, RETRY_AFTER_SECS).is_settled());
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib cache::tests 2>&1 | tail -20
```

Expected: 编译失败，错误形如 `error[E0560]: struct 'CachedServer' has no field named 'probed_at'` 与 `error[E0599]: no method named 'lookup' found`。

- [ ] **Step 3: 改 `CachedServer` 与 `put`，实现判定逻辑**

把 `cache.rs:26-32` 替换为：

```rust
/// 单个 server 的缓存条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedServer {
    /// `ServerConfig`(kind+command+args+env+url+headers) 的 SHA-256 十六进制指纹。
    fingerprint: String,
    /// 上次**成功**探测远端工具清单的时间（Unix 秒）。旧缓存缺该字段 ⇒ `None`。
    #[serde(default)]
    probed_at: Option<i64>,
    /// 上次探测**失败**的时间（Unix 秒），用于失败退避。
    #[serde(default)]
    failed_at: Option<i64>,
    tools: Vec<CachedTool>,
}
```

在 `cache_path` 之后（`cache.rs:44` 附近）加常量：

```rust
/// 工具清单的保鲜期：24 小时。
pub const TTL_SECS: i64 = 24 * 3600;

/// 探测失败后的退避窗口：5 分钟。
pub const RETRY_AFTER_SECS: i64 = 5 * 60;
```

在 `impl McpCache` 之前加枚举与两个私有判据函数：

```rust
/// 一次缓存查找的结果。
///
/// 「旧值是否保留」是 `reload` 的策略，不属于查找语义，故不放进枚举。
/// **只派生 `Debug, Clone, Copy`，不要加 `PartialEq`/`Eq`。** `Fresh` 的载荷是
/// `&[CachedTool]`，而 `CachedTool` 含 `serde_json::Value`（`input_schema`），
/// `Value` 未实现 `Eq`——派生会在 derive 展开处报
/// `the trait bound 'CachedTool: Eq' is not satisfied`。测试里用 `matches!` 匹配，
/// 不需要 `PartialEq`。
#[derive(Debug, Clone, Copy)]
pub enum CacheLookup<'a> {
    /// 指纹匹配且未过期：直接用 `Fresh` 带的清单。
    Fresh(&'a [CachedTool]),
    /// 已过期但仍在失败退避窗口内：本轮不探测。
    BackedOff,
    /// 指纹不匹配、从未探测，或已过期且允许重试：需要重新探测。
    Stale,
}

impl CacheLookup<'_> {
    /// 「本轮是否不需要探测」：`Fresh` 与 `BackedOff` 都为真。
    ///
    /// **只用于跳过探测的判断**，**不能用来选数据源**——`Fresh` 要磁盘清单、
    /// `BackedOff` 要内存快照，两者相反（见 spec §4.2）。
    pub fn is_settled(&self) -> bool {
        !matches!(self, CacheLookup::Stale)
    }
}

/// 工具清单是否还新鲜。时间戳在未来（时钟回拨）或缺失 ⇒ 不新鲜。
fn is_fresh(probed_at: Option<i64>, now: i64, ttl: i64) -> bool {
    probed_at.is_some_and(|t| t <= now && now - t < ttl)
}

/// 现在是否处于失败退避窗口内。时间戳在未来 ⇒ **不**退避。
fn is_backed_off(failed_at: Option<i64>, now: i64, retry_after: i64) -> bool {
    failed_at.is_some_and(|t| t <= now && now - t < retry_after)
}
```

把 `put`（`cache.rs:112-121`）替换为：

```rust
    /// 写入/覆盖 `name` 的缓存条目（记录一次**成功**探测）。
    ///
    /// 成功后清掉 `failed_at`：退避的理由已经消失。
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
```

在 `put` 之后追加 `lookup` / `note_failure` / `backfill_probed_at`：

```rust
    /// 判定 `name` 当前该怎么处置。**判定顺序本身是语义，不能重排**：
    /// 1. 指纹不匹配 或 无条目 ⇒ `Stale`（配置变了，退避对它无效）
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
        // 顺序不能换：先比指纹，退避只对「同一份配置」有意义。
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

    /// 记录一次**失败**的探测尝试：只更新 `failed_at`，不动工具清单与 `probed_at`。
    ///
    /// 条目不存在时也建一条：否则「从未成功过」的坏 server 每轮都会重试。
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
        // 指纹可能已变（配置改过），跟着更新以免它被当成旧配置的失败记录。
        entry.fingerprint = fp;
    }

    /// 把 `probed_at == None` 的存量条目就地填成 `now`（一次性迁移）。
    ///
    /// **只填 `probed_at`，绝不改 `fingerprint` 或 `tools`。** 返回是否发生了改动。
    /// 不接受 `cfg`：迁移与配置无关，多一个用不上的参数会诱使实现者「顺手」做
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
```

把 `get_for`（`cache.rs:107-110`）改名为 `cached_tools_ignoring_freshness`，保留 `&self`：

```rust
    /// 当 `cfg` 的指纹匹配时，返回 `name` 的缓存工具，**不看新鲜度**。
    ///
    /// 给「磁盘上有什么」这类查询用（设置页面板、启动路径）。每轮的自愈判据
    /// 必须用 [`lookup`](Self::lookup)，不要用这个。
    pub fn cached_tools_ignoring_freshness(
        &self,
        name: &str,
        cfg: &ServerConfig,
    ) -> Option<&[CachedTool]> {
        let entry = self.servers.get(name)?;
        (entry.fingerprint == fingerprint(cfg)).then_some(entry.tools.as_slice())
    }
```

- [ ] **Step 4: 修编译错误（`get_for` 改名与 `put` 新签名）**

`cargo check` 会把所有调用点报出来，逐处改：

```bash
cd yi-agent-rs && cargo check -p yi-agent-mcp 2>&1 | grep -E "^error" | head -20
```

- `cache.rs` 测试里的 `loaded.get_for(` → `loaded.cached_tools_ignoring_freshness(`
- `cache.rs` 测试里的 `cache.put("fs", &server("npx"), vec![])` → 末尾补 `, NOW`
- `lib.rs:49` 的 `cache.get_for(...)` → `cache.cached_tools_ignoring_freshness(...)`
- `manager.rs:145`、`manager.rs:232`、`manager.rs:1472` 同理
- `manager.rs:149/236` 的 `cache.put(name, sc, tools)` → 末尾补 `, now`（`reload` 里用函数参数；`load_and_probe` 本任务暂时用 `now_unix()`，Task 4 会换成参数）

给 `manager.rs` 加一个临时辅助（Task 4 完成后仍保留，供其他模块用）：

```rust
/// 当前 Unix 秒。整个 crate 只有这一处取真实时钟，其余全部接受 `now` 参数。
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib cache::tests 2>&1 | tail -8
```

Expected: `test result: ok.` 且通过数包含新增的 15 个用例。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add yi-agent-rs/crates/yi-agent-mcp/src/cache.rs yi-agent-rs/crates/yi-agent-mcp/src/manager.rs yi-agent-rs/crates/yi-agent-mcp/src/lib.rs && git commit -m "feat(mcp): cache 增加 probed_at/failed_at 与 CacheLookup 判定"
```

---

### Task 2: `save` 改为原子写

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs:123-133`（`save`）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `McpCache::save`（签名不变）
- Produces: 无新接口。行为变更：写入走 `tmp + rename`，失败时清理临时文件。

- [ ] **Step 1: 写失败的测试**

```rust

    /// 写入必须先落到临时文件再 `rename`，且失败后不留残留。
    ///
    /// 判别方式：让 `rename` 必然失败（目标名被一个**目录**占住），然后断言
    /// (a) `save` 返回错误、(b) 该目录里没有 `.tmp` 残留、(c) 目标目录仍在。
    /// 直接 `std::fs::write` 的实现也会报错，但它**不会**先建 tmp，因此配套的
    /// `grep` 判据（Step 2）才是变红的依据；本测试是长期契约。
    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join(".yi-agent");
        std::fs::create_dir_all(agent_dir.join("mcp-cache.json")).unwrap();

        let mut c = McpCache::default();
        c.put("fs", &server("npx"), vec![], NOW);
        assert!(c.save(dir.path()).is_err(), "目标被目录占住时必须失败");

        let leftovers: Vec<_> = std::fs::read_dir(&agent_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件必须被清理, 实际: {leftovers:?}");

        std::fs::remove_dir_all(agent_dir.join("mcp-cache.json")).unwrap();
    }

    /// 正常路径：写完后目录里只有 `mcp-cache.json`，没有 `.tmp`。
    #[test]
    fn save_leaves_only_the_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = McpCache::default();
        c.put("fs", &server("npx"), vec![], NOW);
        c.save(dir.path()).unwrap();

        let names: Vec<_> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["mcp-cache.json".to_string()], "实际: {names:?}");
    }

    /// 覆盖写时整个替换（不是截断后留半截）。
    #[test]
    fn save_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = McpCache::default();
        c.put("a", &server("npx"), vec![], NOW);
        c.save(dir.path()).unwrap();

        let mut c2 = McpCache::default();
        c2.put("b", &server("uvx"), vec![], NOW);
        c2.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        assert!(loaded
            .cached_tools_ignoring_freshness("a", &server("npx"))
            .is_none());
        assert!(loaded
            .cached_tools_ignoring_freshness("b", &server("uvx"))
            .is_some());
    }
```

- [ ] **Step 2: 跑测试并确认「红」的判据**

```bash
cd yi-agent-rs && grep -c "std::fs::rename" crates/yi-agent-mcp/src/cache.rs
```

Expected: `0`（当前 `save` 用的是 `std::fs::write`，没有 rename）。这就是本步骤的变红
依据——Step 3 之后必须变成 `1`。

再跑一次确认三条测试能被编译与执行：

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib cache::tests::save_ 2>&1 | tail -12
```

- [ ] **Step 3: 改 `save` 为 `tmp + rename`**

把 `cache.rs:123-133` 的 `save` 替换为：

```rust
    /// 持久化缓存，必要时创建父目录。
    ///
    /// 用 `tmp + rename` 而非 `std::fs::write`：同一个项目可能被 TUI、desktop
    /// sidecar、app-server 同时打开，截断式写入会被别的进程读到半截 JSON。
    pub fn save(&self, workdir: &Path) -> Result<()> {
        let path = cache_path(workdir);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;

        let text = serde_json::to_string_pretty(self)?;
        let tmp = parent.join(format!(".mcp-cache.json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            // 别把临时文件留在用户项目里。
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("rename into {}", path.display()));
        }
        Ok(())
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6
```

Expected: `test result: ok.`

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add yi-agent-rs/crates/yi-agent-mcp/src/cache.rs && git commit -m "fix(mcp): cache.save 改为 tmp+rename 原子写"
```

---

### Task 3: `needs_reload` 单次读取配置并接入 TTL 触发

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:260-345`（`config_fingerprint` / `config_changed_on_disk`）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2489`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5500`
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`mod tests`）

**Interfaces:**
- Consumes: `CacheLookup`、`McpCache::lookup`、`TTL_SECS`、`RETRY_AFTER_SECS`（Task 1）
- Produces:
  - `McpManager::needs_reload(&self, workdir: &Path, now: i64) -> bool`（**替代** `config_changed_on_disk`）
  - `McpManager::fingerprint_and_config(&self, workdir: &Path) -> Option<(String, McpConfig)>`
  - `McpManager::fingerprint_changed(&self, workdir: &Path) -> bool`（私有，`#[allow(dead_code)]`；
    `needs_reload` 为省一次读盘而内联了同一判定，本方法留给只需布尔值的调用方）

- [ ] **Step 1: 写失败的测试**

在 `manager.rs` 的 `mod tests` 里追加。辅助函数复用文件里已有的 `write_config` 风格（若不存在，用下面的 `write_mcp`）：

```rust

    // ---- needs_reload ----

    fn write_mcp(dir: &std::path::Path, body: &str) {
        std::fs::create_dir_all(dir.join(".yi-agent")).unwrap();
        std::fs::write(dir.join(".yi-agent/mcp.json"), body).unwrap();
    }

    fn stdio_cfg(command: &str) -> ServerConfig {
        ServerConfig {
            kind: crate::config::ServerKind::Stdio,
            command: Some(command.into()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            enabled: true,
        }
    }

    /// 建一个 manager，并把 `last_seen` 对齐到磁盘现状（模拟「刚 reload 过」）。
    async fn manager_in_sync(dir: &std::path::Path) -> Arc<McpManager> {
        let cfg = McpConfig::load(dir).unwrap().unwrap();
        McpManager::load_and_probe(cfg, dir).await.unwrap()
    }

    #[tokio::test]
    async fn needs_reload_is_false_when_nothing_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;
        assert!(!mgr.needs_reload(dir.path(), now_unix()));
    }

    #[tokio::test]
    async fn needs_reload_is_true_when_the_file_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"uvx"}}}"#);
        assert!(mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 配置没变、但缓存过期 ⇒ 触发重载（本 bug 的自愈入口）。
    #[tokio::test]
    async fn needs_reload_is_true_when_a_cache_entry_expired() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        // 把缓存时间戳改到 25 小时前
        let mut cache = McpCache::load(dir.path());
        let sc = stdio_cfg("npx");
        cache.put("fs", &sc, vec![], now_unix() - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 配置没变、缓存新鲜 ⇒ 不重载（防每轮重探）。
    #[tokio::test]
    async fn needs_reload_is_false_when_the_cache_is_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let mut cache = McpCache::load(dir.path());
        let sc = stdio_cfg("npx");
        cache.put("fs", &sc, vec![], now_unix() - 23 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now_unix()));
    }

    /// 过期 + 刚失败 ⇒ 不重载（否则每轮白跑一遍 reload + 重建 agent）。
    #[tokio::test]
    async fn needs_reload_is_false_while_a_stale_entry_is_backed_off() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        let sc = stdio_cfg("npx");
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.note_failure("fs", &sc, now - 60);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }

    /// 停用的 server 过期后不能触发 reload（否则永久空转）。
    #[tokio::test]
    async fn needs_reload_is_false_for_a_disabled_expired_server() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        let mut sc = stdio_cfg("npx");
        sc.enabled = false;
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }

    /// 总开关关闭时，任何过期条目都不能触发 reload。
    #[tokio::test]
    async fn master_off_suppresses_needs_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"enabled":false,"mcpServers":{"fs":{"command":"npx"}}}"#,
        );
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &stdio_cfg("npx"), vec![], now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        assert!(!mgr.needs_reload(dir.path(), now));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests::needs_reload 2>&1 | tail -12
```

Expected: 编译失败 `error[E0599]: no method named 'needs_reload'`。

- [ ] **Step 3: 实现 `fingerprint_and_config` / `needs_reload`**

把 `manager.rs` 的 `config_fingerprint`（原 `manager.rs:310`）替换为下面三个方法，并删除 `config_changed_on_disk`：

```rust
    /// 读一次 `mcp.json`，同时拿到指纹与解析后的配置。
    ///
    /// 拆出来是为了让 [`needs_reload`](Self::needs_reload) 不必读两遍盘：
    /// 指纹比较与逐 server 判定用的是同一份解析结果。
    pub fn fingerprint_and_config(&self, workdir: &Path) -> Option<(String, McpConfig)> {
        let text = std::fs::read_to_string(crate::config::config_path(workdir)).ok()?;
        let cfg: McpConfig = serde_json::from_str(&text).ok()?;
        let fp = crate::cache::fingerprint_for_manager(&cfg);
        Some((fp, cfg))
    }

    /// 当前磁盘配置的指纹；文件缺失或坏 JSON 时返回 `None`。
    pub fn config_fingerprint(&self, workdir: &Path) -> Option<String> {
        self.fingerprint_and_config(workdir).map(|(fp, _)| fp)
    }

    /// 磁盘配置是否与上次见到的不同（只比指纹，不读配置内容给调用方用）。
    ///
    /// `needs_reload` 走的是「读一次、复用解析结果」的路径，不使用本方法；
    /// 它保留给只需要布尔判断的调用方（既有测试与将来的一次性检查）。
    #[allow(dead_code)]
    fn fingerprint_changed(&self, workdir: &Path) -> bool {
        let current = self.config_fingerprint(workdir);
        let last = self.last_seen_fingerprint();
        match (current, last) {
            (None, None) => false,
            (a, b) => a != b,
        }
    }

    /// 每轮开跑前：是否需要重新 `reload`。
    ///
    /// 判据必须与 [`reload`](Self::reload) 的短路逐条对齐——`reload` 不会做任何事
    /// 的 server，就不能成为触发条件，否则每轮空转。
    pub fn needs_reload(&self, workdir: &Path, now: i64) -> bool {
        // 整个函数只读一次 mcp.json：解析结果同时用于指纹比较与逐 server 判定。
        let current = self.fingerprint_and_config(workdir);

        // 1. 指纹变了 ⇒ 一定重载（含「文件从有到无 / 从无到有」）
        let last = self.last_seen_fingerprint();
        let changed = match (&current, &last) {
            (None, None) => false,
            (Some((fp, _)), Some(prev)) => fp != prev,
            _ => true,
        };
        if changed {
            return true;
        }

        // 2. 配置没变，但某个 server 的条目已过期且允许重试 ⇒ 也要重载
        let Some((_, cfg)) = current else {
            return false;
        };
        // 总开关关闭 ⇒ reload 整段不探测，不能成为触发条件
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
```

顶部 `use` 补上：`use crate::cache::{CacheLookup, McpCache, RETRY_AFTER_SECS, TTL_SECS};`

- [ ] **Step 4: 更新两个调用点与既有测试**

`crates/yi-agent/src/main.rs:2489`：

```rust
                if mcp_for_driver.needs_reload(&driver_workdir, yi_agent_mcp::now_unix()) {
```

`crates/yi-agent-app-server/src/server.rs:5500`：

```rust
            if mcp.needs_reload(&mcp_workdir, yi_agent_mcp::now_unix()) {
```

`now_unix` 目前是 `pub(crate)`，需改成 `pub` 才能跨 crate 调用：在 `manager.rs` 把
`pub(crate) fn now_unix()` 改为 `pub fn now_unix()`，并在 `lib.rs` 加一行重导出：

```rust
pub use manager::{now_unix, McpManager, ReloadReport, ServerOutcome};
```

（若 `lib.rs` 已有 `pub use manager::...`，则只在该行补 `now_unix`。）

把 `manager.rs` 里 9 处测试中的 `config_changed_on_disk(` 改为 `needs_reload(..., now_unix())`
形式，例如：

```rust
            !mgr.config_changed_on_disk(dir.path()),
```
改为
```rust
            !mgr.needs_reload(dir.path(), now_unix()),
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6
```

Expected: `test result: ok.`

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "feat(mcp): needs_reload 接入 TTL 触发并单次读取配置"
```

---

### Task 4: `reload` 按 `lookup` 分派 + 失败退避

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:226-242`（`reload` 循环体）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:144-158`（`load_and_probe` 循环体）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `lookup` / `note_failure` / `backfill_probed_at`
- Produces: `reload` 的四条路径行为；`load_and_probe` 成功时写 `probed_at`

- [ ] **Step 1: 写失败的测试**

```rust

    // ---- reload 分派 ----

    /// 过期 + 探测失败 ⇒ 工具沿用、error 非空、failed_at 被写。
    #[tokio::test]
    async fn a_failed_probe_falls_back_to_the_old_tool_list() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        // 预置一个过期的、带工具名的缓存
        let now = now_unix();
        let mut cache = McpCache::load(dir.path());
        let sc = stdio_cfg("npx");
        cache.put(
            "fs",
            &sc,
            vec![crate::cache::CachedTool {
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
        assert!(matches!(
            after.lookup("fs", &sc, now + 1, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::BackedOff
        ), "失败必须写 failed_at");
    }

    /// 无缓存 + 探测失败 ⇒ 工具为空。
    #[tokio::test]
    async fn a_failed_probe_with_no_cache_yields_no_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let report = mgr.reload(dir.path(), now).await;
        assert_eq!(report.servers[0].tool_count, 0);
        assert!(report.servers[0].error.is_some());
    }

    /// 内存有 3 个工具、磁盘缓存无条目、探测失败 ⇒ 工具仍为 3 个。
    ///
    /// 钉住「回退源是内存快照」：若改成回退到磁盘缓存，会变成 0 个。
    #[tokio::test]
    async fn a_failed_probe_keeps_the_in_memory_tools_when_the_cache_file_is_gone() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        // 先装一个能成功的 connector 建立内存快照
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;
        let before = mgr.enabled_tools().len();
        assert!(before > 0, "先要有内存工具");

        // 删掉磁盘缓存，再换成失败 connector
        std::fs::remove_file(dir.path().join(".yi-agent/mcp-cache.json")).unwrap();
        install_failing_connector();

        // 保持配置不变，把内存里的快照当作 reusable 的来源：
        // 磁盘缓存已删 ⇒ lookup 走不到 Fresh/BackedOff，理论上会探测；
        // 但 reusable 存在时 Stale 分支仍会先探一次，失败则回落到 e.tools。
        let now = now_unix();
        let report = mgr.reload(dir.path(), now).await;
        assert_eq!(
            report.servers[0].tool_count, before,
            "内存快照必须保留 {before} 个工具，实际 {}",
            report.servers[0].tool_count
        );
    }

    /// 退避窗口内 reload 不探测。
    #[tokio::test]
    async fn reload_skips_a_probe_within_the_backoff_window() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = stdio_cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.note_failure("fs", &sc, now - 60);
        cache.save(dir.path()).unwrap();

        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let report = mgr.reload(dir.path(), now).await;
        assert!(!report.servers[0].probed, "退避窗口内不应探测");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    /// 新鲜缓存用**磁盘清单**，不是内存快照（跨进程同步）。
    #[tokio::test]
    async fn reload_uses_the_disk_list_not_the_memory_snapshot_when_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = stdio_cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![crate::cache::CachedTool {
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

    /// 冷缓存探测成功后必须写入 probed_at，否则 TTL 永不开始计时。
    #[tokio::test]
    async fn load_and_probe_records_probed_at_on_success() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;

        let _mgr = manager_in_sync(dir.path()).await;
        let cache = McpCache::load(dir.path());
        assert!(matches!(
            cache.lookup("fs", &stdio_cfg("npx"), now_unix(), TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::Fresh(_)
        ), "成功后 probed_at 必须被写入");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests 2>&1 | tail -20
```

Expected: `a_failed_probe_falls_back_to_the_old_tool_list` 等失败（当前 `reload` 在失败时返回 `Vec::new()`）。

- [ ] **Step 3: 改 `reload` 循环体**

把 `manager.rs:226-242` 替换为：

`reload` 签名改为 `pub async fn reload(&self, workdir: &Path, now: i64) -> ReloadReport`，
函数开头保留原有 `McpConfig::load` 逻辑。把从 `let old = self.snapshot();` 到
`if let Err(e) = cache.save(workdir) { .. }` 这一段整体替换为：

```rust
        let old = self.snapshot();
        let mut cache = McpCache::load(workdir);
        let mut cache_dirty = false;
        let mut next: HashMap<String, Arc<ServerEntry>> = HashMap::new();
        let mut out = Vec::new();

        for (name, sc) in &cfg.mcp_servers {
            // 存量条目迁移：缺 probed_at 的填成 now（一次性，不探测）。
            if cache.backfill_probed_at(name, now) {
                cache_dirty = true;
            }

            let previous = old.get(name);
            let reusable = previous.filter(|e| e.config == *sc);
            let probe_allowed = cfg.enabled && sc.enabled;

            // 开关关闭：不探测、不写 failed_at，工具按「内存快照 → 磁盘缓存 → 空」回落。
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
                // 逐变体匹配：Fresh 用磁盘清单（跨进程同步），BackedOff 用内存快照。
                match (reusable, cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS)) {
                    (Some(_), CacheLookup::Fresh(disk)) => (disk.to_vec(), false, None, false),
                    (Some(e), CacheLookup::BackedOff) => (e.tools.clone(), false, None, true),
                    (other, _) => match probe_tools(sc).await {
                        Ok(probed_tools) => {
                            cache.put(name, sc, probed_tools.clone(), now);
                            cache_dirty = true;
                            (probed_tools, true, None, false)
                        }
                        Err(e) => {
                            // 保留旧清单：内存快照优先，其次磁盘缓存。
                            let fallback = match other {
                                Some(entry) if !entry.tools.is_empty() => {
                                    Some(entry.tools.clone())
                                }
                                _ => cache
                                    .cached_tools_ignoring_freshness(name, sc)
                                    .map(<[CachedTool]>::to_vec),
                            };
                            cache.note_failure(name, sc, now);
                            cache_dirty = true;
                            (fallback.unwrap_or_default(), true, Some(e.to_string()), false)
                        }
                    },
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

        // 被删掉的 server：只报 removed，不写缓存（它已经不在配置里）。
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
```

**本任务同时给 `ServerOutcome` 加一个字段**（否则上面的四元组与 `out.push` 都编不过）：

```rust
pub struct ServerOutcome {
    pub name: String,
    pub added: bool,
    pub removed: bool,
    pub probed: bool,
    /// 本轮因失败退避窗口而**没有**探测（区别于「配置未变，跳过复用」）。
    pub skipped_backoff: bool,
    pub tool_count: usize,
    pub read_only_count: usize,
    pub error: Option<String>,
}
```

`reload` 开头 `McpConfig::load` 失败时构造的那个 `ServerOutcome` 要补
`skipped_backoff: false`；「被删掉的 server」那个循环同样要补。Task 7 只负责把它
渲染成用户可见文案。

- [ ] **Step 4: 改 `load_and_probe`**

把 `manager.rs:144-158` 的 `for` 循环体替换为下面这一版（**这就是最终版本**，含停用
短路与 `probed_at` 写入，后续 Task 不再改动）：

```rust
        for (name, sc) in &cfg.mcp_servers {
            let now = now_unix();
            if cache.backfill_probed_at(name, now) {
                cache_dirty = true;
            }
            let tools = if !cfg.enabled || !sc.enabled {
                // 停用：只查缓存，不探测
                cache
                    .cached_tools_ignoring_freshness(name, sc)
                    .map(<[CachedTool]>::to_vec)
                    .unwrap_or_default()
            } else {
                match cache.cached_tools_ignoring_freshness(name, sc) {
                    // 启动路径刻意**不**校验 TTL（决策 1）
                    Some(t) => t.to_vec(),
                    None => match probe_tools(sc).await {
                        Ok(probed) => {
                            cache.put(name, sc, probed.clone(), now);
                            cache_dirty = true;
                            probed
                        }
                        Err(e) => {
                            warn!(server = %name, error = %e, "MCP probe failed; server unavailable");
                            cache.note_failure(name, sc, now);
                            cache_dirty = true;
                            Vec::new()
                        }
                    },
                }
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
```

所有 `reload(...)` 调用点补 `now`：`main.rs:2186`、`main.rs:2491`、`server.rs:5502`、`reload_tool.rs:105`，统一用 `yi_agent_mcp::now_unix()`。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6
```

Expected: `test result: ok.`

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "feat(mcp): reload 按 lookup 分派并记录失败退避"
```

---

### Task 5: 停用 server 的两条路径都不探测

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`reload` 已含 `probe_allowed`，本任务补 `load_and_probe`）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 4 的 `reload`（`cfg.enabled && sc.enabled` 已在 `probe_allowed` 里）
- Produces: 无新接口。行为：`load_and_probe` 对停用 server 不探测。

**背景**：`reload` 的开关短路在 Task 4 已加。`load_and_probe` 现状（原 `manager.rs:144`）不看 `enabled`，一个「停用且从未缓存过」的 server 每次冷启动都要探满 55s，而它根本不会注册成工具。这是既有缺陷，本任务统一。

- [ ] **Step 1: 写失败的测试**

```rust

    /// 停用的 server 即使缓存过期也不探测，工具沿用旧清单。
    #[tokio::test]
    async fn a_disabled_server_is_never_probed_even_after_the_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        // 预置过期缓存（带工具）
        let now = now_unix();
        let mut sc = stdio_cfg("npx");
        sc.enabled = false;
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![crate::cache::CachedTool {
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
    }

    /// 启动路径同样不探测停用的 server（与 reload 保持一致）。
    #[tokio::test]
    async fn load_and_probe_does_not_probe_a_disabled_server() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;

        let _mgr = manager_in_sync(dir.path()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "启动路径不应为停用 server 探测"
        );
    }

    /// 总开关关闭时，即使配置里有过期条目也不探测，且 failed_at 不被写。
    #[tokio::test]
    async fn master_off_probes_nothing_even_when_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(
            dir.path(),
            r#"{"enabled":false,"mcpServers":{"fs":{"command":"npx"}}}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        let now = now_unix();
        let sc = stdio_cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put(
            "fs",
            &sc,
            vec![crate::cache::CachedTool {
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

        // failed_at 不该被写：没探测就没有失败
        let after = McpCache::load(dir.path());
        let entry_written = matches!(
            after.lookup("fs", &sc, now, TTL_SECS, RETRY_AFTER_SECS),
            CacheLookup::BackedOff
        );
        assert!(!entry_written, "未探测时不得写 failed_at");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests::load_and_probe_does_not_probe_a_disabled_server 2>&1 | tail -12
```

Expected: FAIL，`calls` 为 1 而非 0（现状会探）。

- [ ] **Step 3: 确认实现已在 Task 4 就位**

`load_and_probe` 的停用短路在 **Task 4 Step 4** 已经写入（那一版就是最终版本）。本任务
只补测试来钉住它——若测试通过，说明 Task 4 做对了：

```bash
cd yi-agent-rs && grep -n "if !cfg.enabled || !sc.enabled" crates/yi-agent-mcp/src/manager.rs
```

若没有输出，说明 Task 4 Step 4 没做全：把 `load_and_probe` 的 `for` 循环体替换为
Task 4 Step 4 给出的完整版本（含 `!cfg.enabled || !sc.enabled` 分支）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests 2>&1 | tail -6
```

Expected: `test result: ok.`

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "fix(mcp): 停用 server 在启动路径也不探测"
```

---

### Task 6: 首次迁移填 `probed_at`，避免升级即卡 30 秒

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`reload` / `load_and_probe` 的 `backfill_probed_at` 调用已在 Task 4 加）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`mod tests`）

**Interfaces:**
- Consumes: `McpCache::backfill_probed_at`（Task 1）
- Produces: 无新接口。行为：升级后第一轮不探测任何存量条目。

**为什么需要**：存量缓存 JSON 里没有 `probed_at` 字段，`#[serde(default)]` 读成 `None` ⇒ 全部 `Stale` ⇒ 同一轮集体探测。10 个 server 串行、走成功路径就是 30~40 秒，用户会判定「升级把软件搞坏了」。

- [ ] **Step 1: 写失败的测试**

```rust

    /// 10 条存量条目迁移后，连续两轮 needs_reload 都为 false。
    ///
    /// 防「升级即卡 9 分钟」：若不迁移，10 条全部 Stale，第二轮仍会触发重探。
    #[tokio::test]
    async fn migration_of_many_servers_probes_none_of_them() {
        let dir = tempfile::TempDir::new().unwrap();

        // 手写一份**没有 probed_at / failed_at** 的旧格式缓存（10 个 server）
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        let mut servers = serde_json::Map::new();
        for i in 0..10 {
            let sc = stdio_cfg("npx");
            servers.insert(
                format!("s{i}"),
                serde_json::json!({
                    "fingerprint": crate::cache::fingerprint_for_test(&sc),
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
            cfg_servers.insert(
                format!("s{i}"),
                serde_json::json!({"command": "npx"}),
            );
        }
        write_mcp(
            dir.path(),
            &serde_json::to_string(&serde_json::json!({"mcpServers": cfg_servers})).unwrap(),
        );

        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;

        let mgr = manager_in_sync(dir.path()).await;
        // 迁移发生在 reload / load_and_probe 里；这里再显式 reload 一次确认第一轮不探
        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let _ = mgr.reload(dir.path(), now_unix()).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "迁移后不应有任何探测"
        );
        assert!(
            !mgr.needs_reload(dir.path(), now_unix()),
            "迁移后 needs_reload 必须为 false"
        );
    }
```

需要在 `cache.rs` 的 `mod tests` 之外加一个测试辅助（因为上面的测试要用它复刻指纹）：

```rust
/// 测试辅助：按 `cfg` 计算指纹，供构造「旧格式缓存」的测试复刻真实指纹。
#[cfg(test)]
pub(crate) fn fingerprint_for_test(cfg: &ServerConfig) -> String {
    fingerprint(cfg)
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests::migration_of_many_servers 2>&1 | tail -15
```

Expected: FAIL，`calls` 为 10（全部重探）。

- [ ] **Step 3: 确认 backfill 已就位**

```bash
cd yi-agent-rs && grep -n "backfill_probed_at" crates/yi-agent-mcp/src/manager.rs
```

Expected: 至少 2 处（`reload` 与 `load_and_probe` 各一）。若缺，在两者的 `for` 循环第一行加：

```rust
            if cache.backfill_probed_at(name, now) {
                cache_dirty = true;
            }
```

`needs_reload` **不加** backfill：它每轮都跑且只读不写。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6
```

Expected: `test result: ok.`

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "feat(mcp): 存量缓存首次迁移不触发集体重探"
```

---

### Task 7: 手动 reload 报告「因退避跳过」

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs:58-85`（`ServerOutcome` / `ReloadReport`）
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs`（`render_with_report`）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:2192-2203`（`McpReload` 分支的 label）
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs`（`mod tests`）

**Interfaces:**
- Consumes: `ServerOutcome.skipped_backoff`（Task 4 已加）
- Produces: `ReloadReport::skipped_backoff_names(&self) -> Vec<&str>`

**背景**：用户敲 `/mcp reload` 想强制刷新，但 2 分钟前自动检查刚失败过 ⇒ 什么都探。用户无法区分「真没问题」与「被退避挡住」。做法是**如实说明**，不给手动路径开 `force` 后门（重探风暴是真实风险）。

**本任务不新增字段**（Task 4 已加 `skipped_backoff`），只把它渲染出来。

- [ ] **Step 1: 写失败的测试**

在 `reload_tool.rs` 的 `mod tests` 追加：

```rust

    /// 因退避跳过时，报告必须写明原因与最早可重试时间。
    #[test]
    fn backoff_skip_is_explained_in_the_report() {
        let req = ReloadRequest {
            action: ReloadAction::Reload,
            name: "fs".into(),
            spec: None,
        };
        let report = crate::manager::ReloadReport {
            servers: vec![crate::manager::ServerOutcome {
                name: "fs".into(),
                added: false,
                removed: false,
                probed: false,
                skipped_backoff: true,
                tool_count: 2,
                read_only_count: 1,
                error: None,
            }],
        };
        let text = render_with_report(&req, &report);
        assert!(
            text.contains("跳过") || text.contains("退避"),
            "必须说明为什么没探测, 实际: {text}"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib reload_tool::tests::backoff_skip 2>&1 | tail -12
```

Expected: 编译失败 `struct 'ServerOutcome' has no field named 'skipped_backoff'`。

- [ ] **Step 3: 补 `ReloadReport` 的汇总方法**

`ServerOutcome.skipped_backoff` 已在 Task 4 加好，本步只加汇总方法：

```rust
impl ReloadReport {
    /// 本轮因退避而跳过探测的 server 名。
    pub fn skipped_backoff_names(&self) -> Vec<&str> {
        self.servers
            .iter()
            .filter(|s| s.skipped_backoff)
            .map(|s| s.name.as_str())
            .collect()
    }
}
```

Task 4 已把 `reload` 写成四元组、`(Some(e), CacheLookup::BackedOff)` 分支末位为
`true`。确认一下：

```bash
cd yi-agent-rs && grep -n "CacheLookup::BackedOff) => (e.tools.clone(), false, None, true)" crates/yi-agent-mcp/src/manager.rs
```

Expected: 有输出。没有则按 Task 4 Step 3 补齐。

- [ ] **Step 4: 渲染文案**

`render_with_report` 的 `for s in &report.servers` 循环里，在 `else if s.probed` 之前插入：

```rust
        } else if s.skipped_backoff {
            out.push_str(&format!(
                "- `{}` 跳过探测（上次失败后的退避窗口内），沿用 {} 个工具（{} 只读）\n",
                s.name, s.tool_count, s.read_only_count
            ));
```

`main.rs:2192` 的 label 计算里，`all_ok` 为真但存在退避跳过时补一句：

```rust
                            let skipped = report.skipped_backoff_names();
                            let label = if report.all_ok() {
                                let probed = report.servers.iter().filter(|s| s.probed).count();
                                let mut l = format!(
                                    "MCP 配置已重载（{} 个 server，重探 {probed} 个）",
                                    report.servers.len()
                                );
                                if !skipped.is_empty() {
                                    l.push_str(&format!(
                                        "，{} 个因退避窗口跳过（{}）",
                                        skipped.len(),
                                        skipped.join(", ")
                                    ));
                                }
                                l
                            } else { /* 原失败分支不变 */ };
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6 && cargo test -p yi-agent --lib 2>&1 | tail -6
```

Expected: 两处都 `test result: ok.`

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "feat(mcp): 手动 reload 报告因退避跳过的 server"
```

---

### Task 8: 不可用 server 每窗口只探一次（放大路径防线）

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`（`mod tests`）
- Modify: 仅在测试暴露缺陷时才改 `manager.rs`

**Interfaces:**
- Consumes: Task 3-5 的全部行为
- Produces: 无新接口。这是**回归防线**任务，不新增功能。

**为什么单列**：这是本设计唯一可能造成**严重性能回归**的地方（每轮 55s × k 个坏 server）。前几个任务分散实现了退避，本任务用一个端到端测试把「连续 10 轮只探 1~2 次」钉死。

- [ ] **Step 1: 写失败的测试**

```rust

    /// 连续 10 轮 needs_reload + reload，坏 server 的探测次数必须是 1~2 次，不是 10。
    #[tokio::test]
    async fn a_permanently_dead_server_is_probed_once_per_backoff_window() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);

        let calls = Arc::new(AtomicUsize::new(0));
        install_failing_connector();
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;

        // 用一个计数 connector 替换，统计探测次数
        let counted = Arc::clone(&calls);
        *TEST_CONNECTOR.lock().unwrap() = Some(Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(anyhow!("dead endpoint")) })
        }));

        // 清掉启动时可能写入的缓存，保证从「过期」状态开始
        let now = now_unix();
        let sc = stdio_cfg("npx");
        let mut cache = McpCache::load(dir.path());
        cache.put("fs", &sc, vec![], now - 25 * 3600);
        cache.note_failure("fs", &sc, now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        let mut probes = 0usize;
        for round in 0..10 {
            let t = now + round; // 10 秒内跑完 10 轮
            if mgr.needs_reload(dir.path(), t) {
                let report = mgr.reload(dir.path(), t).await;
                probes += report.servers.iter().filter(|s| s.probed).count();
            }
        }
        assert!(
            (1..=2).contains(&probes),
            "10 轮内应只探 1~2 次，实际 {probes} 次（退避失效会导致 10 次，每轮 55s）"
        );
    }

    /// 同一进程内先建立快照，把缓存时间戳改旧，再 reload ⇒ 必须重探（决策 3 的命门）。
    #[tokio::test]
    async fn reload_reprobes_when_the_in_memory_snapshot_has_expired() {
        let dir = tempfile::TempDir::new().unwrap();
        write_mcp(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let calls = Arc::new(AtomicUsize::new(0));
        install_echo_connector(Arc::clone(&calls));
        let _seam = SeamGuard;
        let mgr = manager_in_sync(dir.path()).await;
        let built = mgr.enabled_tools().len();
        assert!(built > 0);

        // 把磁盘缓存时间戳改到 25h 前（配置指纹不变）
        let now = now_unix();
        let sc = stdio_cfg("npx");
        let mut cache = McpCache::load(dir.path());
        let tools: Vec<_> = {
            let hit = cache
                .cached_tools_ignoring_freshness("fs", &sc)
                .expect("cache hit")
                .to_vec();
            hit
        };
        cache.put("fs", &sc, tools, now - 25 * 3600);
        cache.save(dir.path()).unwrap();

        let report = mgr.reload(dir.path(), now).await;
        assert!(
            report.servers[0].probed,
            "内存快照过期后必须重探（否则永远拿不到新工具名）"
        );
    }
```

- [ ] **Step 2: 跑测试确认结果**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests::a_permanently_dead_server 2>&1 | tail -12
```

Expected: PASS（Task 3-5 已实现退避）。若 FAIL 且 `probes == 10`，说明 `needs_reload` 没用 `lookup==Stale` 或 `reload` 没写 `failed_at`，回去检查 Task 3 Step 3 与 Task 4 Step 3。

- [ ] **Step 3: 跑全部并做变异验证**

先跑全绿：

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib 2>&1 | tail -6
```

然后逐条手工验证防线（每条改完跑一次、再改回）：

```bash
# 1) 删掉 needs_reload 里的 TTL 判据（把 matches!(.., Stale) 改成 false）
#    预期：needs_reload_is_true_when_a_cache_entry_expired 变红
# 2) 把 reload 失败分支的 note_failure 注释掉
#    预期：a_permanently_dead_server_is_probed_once_per_backoff_window 变红
# 3) 把 lookup 里的退避判定挪到指纹判定之前
#    预期：a_fingerprint_change_ignores_the_backoff_window 变红
# 4) 去掉 needs_reload 里的 sc.enabled &&
#    预期：needs_reload_is_false_for_a_disabled_expired_server 变红
# 5) 去掉 probe_allowed 里的 cfg.enabled
#    预期：master_off_probes_nothing_even_when_stale 变红
```

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A yi-agent-rs && git commit -m "test(mcp): 钉住坏 server 每退避窗口只探一次"
```

---

### Task 9: 文档、进度与本轮范围外确认

**Files:**
- Modify: `docs/project-management/mcp.md`
- Modify: `docs/superpowers/specs/2026-10-31-mcp-cache-ttl-selfheal-design.md`（仅在实现与 spec 有偏差时）
- Test: `just fmt-check`、全量相关 crate 测试

**Interfaces:**
- Consumes: Task 1-8 全部
- Produces: 无代码接口

- [ ] **Step 1: 跑全部验证判据**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp 2>&1 | tail -8
```

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib 2>&1 | tail -6
```

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib 2>&1 | tail -6
```

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib 2>&1 | tail -6
```

```bash
cd yi-agent-rs && cargo fmt --all -- --check
```

Expected: 全部通过，无 diff。若有失败，先修再继续（不要带着红灯提交）。

- [ ] **Step 2: 确认新增测试总数与 spec 一致**

```bash
cd yi-agent-rs && cargo test -p yi-agent-mcp --lib -- --list 2>&1 | grep -c "^cache::tests\|^manager::tests\|^reload_tool::tests"
```

与 spec §7 声称的「新增 29 例」核对。若数量不符，把实际数字更新到 spec §7 与 §6（两处必须一致）。

- [ ] **Step 3: 更新项目进度文档**

在 `docs/project-management/mcp.md` 的对应小节追加一条（保持文件既有的表格/列表格式）：

```markdown
- [x] 工具清单按 24h TTL 自动重探，失败按 5 分钟退避
      （`yi-agent-rs/crates/yi-agent-mcp/src/cache.rs` 的 `lookup`；
      验证：`cargo test -p yi-agent-mcp --lib` 全绿）
```

若该文件有「完成 / 总计」计数，同步 +1。

- [ ] **Step 4: 真机验收（三条必做，记录结果到实施分支的 PR 说明或 commit 正文）**

1. **自愈**：把某个 server 的 `probed_at` 手工改到 25 小时前，启动会话发一条消息，
   日志应出现**一次**探测，工具名更新为远端现值。
2. **不卡顿**：3 个 server 的 `probed_at` 同时改到 25 小时前，计时这一轮。成功路径约
   12s；明显超过（如 >20s）说明有 server 走了黑洞路径，结合日志确认。
3. **升级路径**：用不含 `probed_at` 的旧缓存启动，确认**零探测**且文件里被补上
   `probed_at`。
4. **总开关关闭**：顶层 `enabled: false` + 过期条目 + 上次探测失败，发几条消息，
   **不应出现任何探测**，`failed_at` 不被刷新；打开总开关后应立刻正常探测一次。

- [ ] **Step 5: 提交并合并**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "docs(mcp): 记录 TTL 自愈完成与验收结果"
```

实现完成后按 `superpowers:finishing-a-development-branch` 合并回 `main`。

---

## 附：与 spec 的对照（自检表）

| spec 章节 | 覆盖任务 |
|---|---|
| §4.1 `CachedServer` 加两字段、`#[serde(default)]`、不加 `skip_serializing_if` | Task 1 Step 3 |
| §4.1 `CacheLookup` 三变体 + `is_settled` 只做「要不要探」 | Task 1 Step 3 |
| §4.1 `lookup` 四步判定顺序（指纹优先于退避） | Task 1 Step 3 + 测试 `a_fingerprint_change_ignores_the_backoff_window` |
| §4.1 `is_fresh` / `is_backed_off` 含 `t <= now` | Task 1 Step 3 + 测试 `a_future_*` |
| §4.1 `put` 加 `now`、成功后清 `failed_at` | Task 1 Step 3 |
| §4.1 `note_failure` 只动 `failed_at` | Task 1 Step 3 + 测试 `note_failure_only_updates_failed_at` |
| §4.1 `backfill_probed_at` 不改指纹 | Task 1 Step 3 + 测试 `backfill_does_not_touch_the_fingerprint` |
| §4.1 `get_for` → `cached_tools_ignoring_freshness` | Task 1 Step 3-4 |
| §4.2 `needs_reload` 单次读取（`fingerprint_and_config`） | Task 3 Step 3 |
| §4.2 停用 server / 总开关不进 `needs_reload` | Task 3 Step 3 + 2 条测试 |
| §4.2 `reload` 四路径（Fresh 用磁盘清单、BackedOff 用内存快照） | Task 4 Step 3 |
| §4.2 失败回退「内存快照优先」 | Task 4 Step 3 + 测试 `a_failed_probe_keeps_the_in_memory_tools_when_the_cache_file_is_gone` |
| §4.3.1 失败退避、每窗口一次 | Task 4 + Task 8 |
| §4.3.1.1(a) 单 server 停用不探测（两条路径） | Task 5 |
| §4.3.1.1(b) 串行放大 | 不在本 plan 解决（spec 列为可选的错峰，实现时若真机验收超时再加） |
| §4.3.1.1(c) 关机跨 24h | 无需代码（预期行为） |
| §4.3.1.1(d) 总开关短路 | Task 3 + Task 5 |
| §4.3.1.1.1 首次迁移填 `now` | Task 6 |
| §4.3.1.2 手动 reload 说明退避 | Task 7 |
| §4.3.4 `save` 原子写 | Task 2 |
| §5 错误处理表 | Task 1（时钟回拨）、Task 4（失败回退）、Task 2（并发写） |
| §7 变异验证 | Task 8 Step 3 |

**已知未覆盖**：§4.3.1.1(b) 的错峰抖动与每轮限额在 spec 里是「可选压住手段」，本 plan
不实现。真机验收第 2 条若超出 20s 再补，届时新增一个 Task 10。
