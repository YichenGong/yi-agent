# 桌面设置页「网页搜索」Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在桌面 App 设置页里配置 Web 搜索（开关、API Key、Base URL、结果条数、时效范围），保存即生效，并能一键测试连接。

**Architecture:** 配置落在机器级 `~/.yi-agent/websearch.json`，由 `yi-agent-tools` 在每次注册/搜索时热读；`yi-agent-app-server` 新增 `websearch/*` RPC 域供桌面调用；桌面新增「网页搜索」Tab。多引擎数据结构（`engines` map + `active_engine`）先只实现 Bocha，为后续引擎留位。

**Tech Stack:** Rust（serde / tokio / async-trait / axum 无关）、TypeScript + React 18 + Vitest + Testing Library、JSON-RPC over stdio。

## Global Constraints

- **字段命名一律 snake_case**：配置 JSON 与 RPC 参数/响应都用 `api_key` / `base_url` / `active_engine` / `elapsed_ms`，与 `models.json`（`api_key` / `api_url` / `default_model`）和 `model/list`（`api_key_masked`）一致。禁止 camelCase。
- **密钥永不下发明文**：`websearch/get` 只回 `api_key_masked`（`••••1234` 形式，复用 `yi_agent_runtime::models::mask_key`）。
- **掩码值不回写**：`websearch/set` 收到等于掩码值的 `api_key` 时跳过该字段，绝不把掩码存成真实 key。
- **坏配置不阻断启动**：文件缺失 → 全默认值且不打日志；不可读/JSON 损坏 → 全默认值 + `tracing::warn!`。
- **`count` 钳制到 1–50**，默认 25（与现 `web/search.rs:10` 的 `DEFAULT_COUNT` 一致）。
- **`freshness` 枚举固定五项**：`noLimit` / `oneDay` / `oneWeek` / `oneMonth` / `oneYear`。
- **向后兼容**：`api_key` 为空时回退环境变量 `BOCHA_API_KEY`。
- **权限档位**：读用 `Observe`，写与测试连接用 `Control`（与 `model/*` 同档）。
- **原子落盘**：临时文件 + rename，临时名含 pid 与递增序号（参考 `settings_store.rs:287` 的 `temp_path_for`）。
- **注释与 UI 文案用中文**，与仓库其余部分一致；解释"为什么"而不是"做了什么"。
- **提交前跑 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- 本计划在 worktree `.worktrees/feat-websearch-settings` 内执行；所有路径相对仓库根。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-tools/src/web/config.rs` | **新增**。配置结构与容错读取、路径解析、默认值/钳制/回退逻辑。纯数据 + IO，不含网络。 |
| `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs` | 注册新模块并 re-export。 |
| `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs` | 注入 `base_url` / `freshness`，去掉两处硬编码。 |
| `yi-agent-rs/crates/yi-agent-tools/src/web/search.rs` | `WebSearchTool` 接受 `default_count`。 |
| `yi-agent-rs/crates/yi-agent-tools/src/web/unconfigured.rs` | **新增**。无 key 时的占位 provider，调用即回明确错误。 |
| `yi-agent-rs/crates/yi-agent-tools/src/lib.rs` | 注册逻辑改为读配置决定注册什么。 |
| `yi-agent-rs/crates/yi-agent-app-server/src/websearch_rpc.rs` | **新增**。`websearch/get|set|test` 三个 handler + 可测的 `_at` 变体。 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/websearch.rs` | **新增**。RPC 域分派 + 权限档位。 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` | 注册新域模块。 |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | `AppState` 增加 `websearch_path` 注入点。 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 构造 `AppState` 时填 `websearch_path`。 |
| `desktop/src/lib/websearch.ts` | **新增**。RPC 类型收敛（坏行丢弃、缺失字段补安全默认）。 |
| `desktop/src/components/SettingsWebSearchTab.tsx` | **新增**。面板本体。 |
| `desktop/src/components/SettingsWebSearchTab.test.tsx` | **新增**。5 条行为测试。 |
| `desktop/src/components/SettingsDialog.tsx` | 加 Tab 项 + prop 透传 + 面板渲染。 |
| `desktop/src/App.tsx` | 加 `websearchCall` 宿主接缝。 |
| `docs/project-management/yi-agent-tools.md` | 更新完成判据。 |

---

### Task 1: 配置结构与容错读取

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs:1-8`

**Interfaces:**
- Produces:
  - `pub struct WebSearchConfig { pub enabled: bool, pub active_engine: String, pub engines: BTreeMap<String, EngineConfig> }`
  - `pub struct EngineConfig { pub api_key: String, pub base_url: String, pub count: usize, pub freshness: Freshness }`
  - `pub enum Freshness { NoLimit, OneDay, OneWeek, OneMonth, OneYear }`
  - `impl Default for WebSearchConfig` / `impl Default for EngineConfig`
  - `pub fn websearch_path() -> std::path::PathBuf`
  - `pub fn load_websearch_config_from(path: &Path) -> WebSearchConfig`
  - `pub fn load_websearch_config() -> WebSearchConfig`
  - `impl WebSearchConfig { pub fn engine(&self, id: &str) -> Option<&EngineConfig>; pub fn active(&self) -> Option<&EngineConfig> }`
  - `impl EngineConfig { pub fn resolved_api_key(&self) -> Option<String>; pub fn clamped_count(&self) -> usize }`
  - `impl Freshness { pub fn as_bocha_str(&self) -> &'static str; pub fn parse(s: &str) -> Freshness }`

- [ ] **Step 1: 写失败的测试**

创建 `web/config.rs`，只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_text(json: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("websearch.json");
        std::fs::write(&path, json).unwrap();
        (dir, path)
    }

    #[test]
    fn defaults_are_bocha_with_25_results_and_no_limit() {
        let c = WebSearchConfig::default();
        assert!(c.enabled);
        assert_eq!(c.active_engine, "bocha");
        let e = c.engine("bocha").expect("bocha must exist by default");
        assert_eq!(e.count, 25);
        assert_eq!(e.freshness, Freshness::NoLimit);
        assert_eq!(e.base_url, "https://api.bocha.cn/v1");
        assert_eq!(e.api_key, "");
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let c = load_websearch_config_from(&dir.path().join("nope.json"));
        assert_eq!(c.active_engine, "bocha");
        assert!(c.enabled);
    }

    #[test]
    fn a_corrupt_file_yields_defaults_instead_of_failing() {
        let (_dir, path) = cfg_text("{ not json ");
        let c = load_websearch_config_from(&path);
        assert_eq!(c.engine("bocha").unwrap().count, 25);
    }

    #[test]
    fn a_partial_file_keeps_the_missing_fields_at_defaults() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"api_key":"k"}}}"#);
        let c = load_websearch_config_from(&path);
        let e = c.engine("bocha").unwrap();
        assert_eq!(e.api_key, "k");
        assert_eq!(e.count, 25, "an omitted count must fall back to 25");
        assert_eq!(e.freshness, Freshness::NoLimit);
    }

    #[test]
    fn count_is_clamped_to_the_supported_range() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"count":900}}}"#);
        assert_eq!(load_websearch_config_from(&path).engine("bocha").unwrap().clamped_count(), 50);
        let (_dir2, path2) = cfg_text(r#"{"engines":{"bocha":{"count":0}}}"#);
        assert_eq!(load_websearch_config_from(&path2).engine("bocha").unwrap().clamped_count(), 1);
    }

    #[test]
    fn an_unknown_freshness_falls_back_to_no_limit() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"freshness":"lastDecade"}}}"#);
        assert_eq!(
            load_websearch_config_from(&path).engine("bocha").unwrap().freshness,
            Freshness::NoLimit
        );
    }

    #[test]
    fn an_unknown_active_engine_falls_back_to_bocha() {
        let (_dir, path) = cfg_text(r#"{"active_engine":"tavily"}"#);
        let c = load_websearch_config_from(&path);
        assert_eq!(c.active_engine, "bocha");
    }

    #[test]
    fn the_env_var_is_only_a_fallback_for_an_empty_file_key() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"api_key":"from-file"}}}"#);
        let e = load_websearch_config_from(&path).engine("bocha").unwrap().clone();
        assert_eq!(e.resolved_api_key().as_deref(), Some("from-file"));
    }

    #[test]
    fn freshness_maps_onto_the_bocha_vocabulary() {
        assert_eq!(Freshness::NoLimit.as_bocha_str(), "noLimit");
        assert_eq!(Freshness::OneDay.as_bocha_str(), "oneDay");
        assert_eq!(Freshness::OneWeek.as_bocha_str(), "oneWeek");
        assert_eq!(Freshness::OneMonth.as_bocha_str(), "oneMonth");
        assert_eq!(Freshness::OneYear.as_bocha_str(), "oneYear");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::config`
Expected: 编译失败，`WebSearchConfig` 等类型未定义。

- [ ] **Step 3: 实现最小代码**

在测试模块**之前**加入：

```rust
//! 网页搜索的机器级配置：`~/.yi-agent/websearch.json`。
//!
//! 选择 JSON 文件而不是 `.env`：`.env` 只在进程启动时读一次
//! (`runtime/src/config.rs` 的 `load_env_files`)，改完必须重启；本文件每次搜索
//! 前热读，保存即生效。命名与 `models.json` 一致，故字段一律 snake_case。
//!
//! `engines` 刻意做成 map：加新引擎只是加一个键 + 一个 provider 实现，
//! `active_engine` 已经有位置放。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bocha 的默认端点。`base_url` 留空时用它。
pub const DEFAULT_BOCHA_BASE_URL: &str = "https://api.bocha.cn/v1";
/// 结果条数默认值，与 `web/search.rs` 的默认一致。
pub const DEFAULT_COUNT: usize = 25;
/// 结果条数的上下界：低于 1 没意义，高于 50 只会稀释上下文。
pub const MIN_COUNT: usize = 1;
pub const MAX_COUNT: usize = 50;

/// 搜索时效范围，映射到 Bocha 的 `freshness` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Freshness {
    #[default]
    #[serde(rename = "noLimit")]
    NoLimit,
    #[serde(rename = "oneDay")]
    OneDay,
    #[serde(rename = "oneWeek")]
    OneWeek,
    #[serde(rename = "oneMonth")]
    OneMonth,
    #[serde(rename = "oneYear")]
    OneYear,
}

impl Freshness {
    pub fn as_bocha_str(self) -> &'static str {
        match self {
            Freshness::NoLimit => "noLimit",
            Freshness::OneDay => "oneDay",
            Freshness::OneWeek => "oneWeek",
            Freshness::OneMonth => "oneMonth",
            Freshness::OneYear => "oneYear",
        }
    }

    /// 未知值一律回退 `NoLimit`——用户手改文件写错字不该让搜索整体不可用。
    pub fn parse(s: &str) -> Freshness {
        match s {
            "oneDay" => Freshness::OneDay,
            "oneWeek" => Freshness::OneWeek,
            "oneMonth" => Freshness::OneMonth,
            "oneYear" => Freshness::OneYear,
            _ => Freshness::NoLimit,
        }
    }
}

/// 单个引擎的配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_count")]
    pub count: usize,
    #[serde(default)]
    pub freshness: Freshness,
}

fn default_count() -> usize {
    DEFAULT_COUNT
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: DEFAULT_BOCHA_BASE_URL.to_string(),
            count: DEFAULT_COUNT,
            freshness: Freshness::NoLimit,
        }
    }
}

impl EngineConfig {
    /// 生效的 key：文件里的值优先；为空时回退 `BOCHA_API_KEY` 环境变量，
    /// 保证老用户不改配置也能继续用。
    pub fn resolved_api_key(&self) -> Option<String> {
        if !self.api_key.is_empty() {
            return Some(self.api_key.clone());
        }
        std::env::var("BOCHA_API_KEY").ok().filter(|v| !v.is_empty())
    }

    /// 钳制到 1–50。越界不报错：用户输错数字不该让整页保存失败。
    pub fn clamped_count(&self) -> usize {
        self.count.clamp(MIN_COUNT, MAX_COUNT)
    }

    /// 生效的端点：留空回退默认值。
    pub fn resolved_base_url(&self) -> String {
        if self.base_url.is_empty() {
            DEFAULT_BOCHA_BASE_URL.to_string()
        } else {
            self.base_url.clone()
        }
    }
}

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_engine_id")]
    pub active_engine: String,
    #[serde(default = "default_engines")]
    pub engines: BTreeMap<String, EngineConfig>,
}

fn default_true() -> bool {
    true
}

fn default_engine_id() -> String {
    "bocha".to_string()
}

fn default_engines() -> BTreeMap<String, EngineConfig> {
    let mut map = BTreeMap::new();
    map.insert("bocha".to_string(), EngineConfig::default());
    map
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            active_engine: default_engine_id(),
            engines: default_engines(),
        }
    }
}

impl WebSearchConfig {
    pub fn engine(&self, id: &str) -> Option<&EngineConfig> {
        self.engines.get(id)
    }

    /// 当前生效引擎的配置。未知 id 回退 bocha（见 `load_websearch_config_from`）。
    pub fn active(&self) -> Option<&EngineConfig> {
        self.engine(&self.active_engine)
    }
}

/// 实例状态根，与 `yi_agent_store::paths::instance_home` 同一契约：
/// `YI_AGENT_HOME` 优先（值不以 `.yi-agent` 结尾则自动补上），否则 `$HOME/.yi-agent`。
///
/// **为什么在这里重写而不用 `yi_agent_store`**：那个 crate 为了会话持久化会拉
/// rusqlite（bundled SQLite）。本 crate 只是要拼一个路径，为此把 SQLite 链进
/// 工具层不值得。契约只有十几行，改动时请与 `yi-agent-store/src/paths.rs`
/// 的 `instance_home_from` 对照。
fn instance_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            return Some(dir);
        }
        return Some(dir.join(".yi-agent"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".yi-agent"))
}

/// `<instance_home>/websearch.json`；实例目录不可解析时回退
/// `.yi-agent/websearch.json`，与 `models_path` 同口径。
pub fn websearch_path() -> PathBuf {
    instance_home()
        .map(|home| home.join("websearch.json"))
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("websearch.json"))
}

pub fn load_websearch_config() -> WebSearchConfig {
    load_websearch_config_from(&websearch_path())
}

/// 容错读：缺失是首次运行的正常情形（不打日志）；不可读或损坏则 warn 后回默认。
pub fn load_websearch_config_from(path: &Path) -> WebSearchConfig {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WebSearchConfig::default()
        }
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "could not read websearch.json");
            return WebSearchConfig::default();
        }
    };
    let mut cfg: WebSearchConfig = match serde_json::from_str(&text) {
        Ok(cfg) => cfg,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "invalid websearch.json; using defaults");
            return WebSearchConfig::default();
        }
    };
    // 手改文件可能写错的引擎 id：静默纠正成 bocha，而不是让搜索整体失效。
    if !cfg.engines.contains_key(&cfg.active_engine) {
        cfg.active_engine = default_engine_id();
    }
    // 部分文件的 engines 里可能只有别的引擎，bocha 缺失时补回默认条目，
    // 让「active_engine 指向 bocha」这条不变量永远成立。
    cfg.engines
        .entry(default_engine_id())
        .or_insert_with(EngineConfig::default);
    cfg
}
```

`web/mod.rs` 增加：

```rust
pub mod config;
```

并加 re-export：

```rust
pub use config::{EngineConfig, Freshness, WebSearchConfig};
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::config`
Expected: 9 个测试全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-tools/src/web/config.rs yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs \
  && git commit -m "feat(tools): 网页搜索配置结构与容错读取"
```

---

### Task 2: Bocha provider 注入 base_url 与 freshness

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs:9-60, 103-120`

**Interfaces:**
- Consumes: Task 1 的 `Freshness`
- Produces:
  - `BochaSearchProvider::with_options(api_key: String, base_url: String, freshness: Freshness) -> Self`
  - `BochaSearchProvider::new(api_key: String) -> Self`（签名不变，内部转调 `with_options`）

- [ ] **Step 1: 写失败的测试**

在 `bocha.rs` 的 `mod tests` 里加入：

```rust
    /// `freshness` 曾是硬编码 "noLimit"，用户改不了。现在必须随构造参数走，
    /// 否则设置页里的时效下拉框就是个摆设。
    #[tokio::test]
    async fn freshness_reaches_the_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .and(body_json(serde_json::json!({
                "query": "q",
                "summary": true,
                "count": 3,
                "freshness": "oneWeek",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 200,
                "data": { "webPages": { "value": [] } }
            })))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_options(
            "test-key".to_string(),
            server.uri(),
            Freshness::OneWeek,
        );
        provider.search("q", 3).await.unwrap();
        // body_json 匹配失败时 wiremock 会回 404，上面 unwrap 就会炸。
    }

    /// 默认构造保持 25 条的既有行为不变（`search` 的 count 由调用方给，
    /// 这里断言的是 `new` 仍然可用且不 panic）。
    #[tokio::test]
    async fn new_still_works() {
        let provider = BochaSearchProvider::new("k".to_string());
        assert_eq!(provider.name(), "bocha");
    }
```

同时在测试模块顶部确认已 `use wiremock::matchers::{body_json, header, method, path};`
（缺 `body_json` 就补上）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::bocha`
Expected: 编译失败，`with_options` 与 `Freshness` 未定义。

- [ ] **Step 3: 实现**

改结构体与构造器（`bocha.rs:9-60` 区域）：

```rust
use crate::web::config::{Freshness, DEFAULT_BOCHA_BASE_URL};

pub struct BochaSearchProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    freshness: Freshness,
}

impl BochaSearchProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_options(
            api_key,
            DEFAULT_BOCHA_BASE_URL.to_string(),
            Freshness::NoLimit,
        )
    }

    /// 带完整参数的构造：设置页可改端点与时效，这两个曾经是硬编码。
    pub fn with_options(api_key: String, base_url: String, freshness: Freshness) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url,
            freshness,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self::with_options(api_key, base_url, Freshness::NoLimit)
    }

    #[cfg(test)]
    fn with_base_url_for_test(api_key: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url,
            freshness: Freshness::NoLimit,
        }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("BOCHA_API_KEY").ok()?;
        Some(Self::new(api_key))
    }
}
```

`search()` 里把硬编码换掉：

```rust
        let req_body = BochaRequest {
            query: query.to_string(),
            summary: true,
            count,
            freshness: self.freshness.as_bocha_str().to_string(),
        };
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::bocha`
Expected: 全部 PASS（含既有的 5 个 wiremock 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs \
  && git commit -m "feat(tools): Bocha provider 支持注入 base_url 与时效"
```

---

### Task 3: WebSearchTool 接受默认条数

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/search.rs:10-27, 54-56`

**Interfaces:**
- Consumes: 无（仅本文件）
- Produces:
  - `WebSearchTool::with_default_count(engine: Arc<dyn WebSearchProvider>, default_count: usize) -> Self`
  - `WebSearchTool::new(engine) -> Self`（保持不变，等价于 `with_default_count(engine, 25)`）

- [ ] **Step 1: 写失败的测试**

在 `search.rs` 的 `mod tests` 里加入：

```rust
    #[tokio::test]
    async fn a_configured_default_count_replaces_the_hardcoded_25() {
        let provider = std::sync::Arc::new(MockProvider {
            results: vec![],
            last_count: std::sync::Mutex::new(None),
        });
        let tool = WebSearchTool::with_default_count(provider.clone(), 7);
        tool.call(serde_json::json!({"query": "test"})).await;
        assert_eq!(*provider.last_count.lock().unwrap(), Some(7));
    }

    #[tokio::test]
    async fn an_explicit_count_still_wins_over_the_default() {
        let provider = std::sync::Arc::new(MockProvider {
            results: vec![],
            last_count: std::sync::Mutex::new(None),
        });
        let tool = WebSearchTool::with_default_count(provider.clone(), 7);
        tool.call(serde_json::json!({"query": "test", "count": 3}))
            .await;
        assert_eq!(*provider.last_count.lock().unwrap(), Some(3));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::search`
Expected: 编译失败，`with_default_count` 未定义。

- [ ] **Step 3: 实现**

```rust
pub struct WebSearchTool {
    engine: Arc<dyn WebSearchProvider>,
    /// 调用方未显式给 `count` 时用它。来自 `websearch.json`，可被设置页改动。
    default_count: usize,
}

impl WebSearchTool {
    pub fn new(engine: Arc<dyn WebSearchProvider>) -> Self {
        Self::with_default_count(engine, DEFAULT_COUNT)
    }

    pub fn with_default_count(engine: Arc<dyn WebSearchProvider>, default_count: usize) -> Self {
        Self {
            engine,
            default_count: default_count.clamp(1, 50),
        }
    }
}
```

`call()` 里：

```rust
        let count = args.count.unwrap_or(self.default_count);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::search`
Expected: 5 个测试 PASS（3 个既有 + 2 个新增）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-tools/src/web/search.rs \
  && git commit -m "feat(tools): WebSearchTool 的默认条数可配置"
```

---

### Task 4: 未配置占位 provider

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/unconfigured.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`

**Interfaces:**
- Produces: `pub struct UnconfiguredProvider;` 实现 `WebSearchProvider`，`name()` 返回 `"unconfigured"`，`search()` 返回 `Err(ToolsError::SearchEngine("web search is not configured: set an API key in Settings > 网页搜索".into()))`

**背景**：现在没 key 时 `WebSearchTool` 根本不注册，模型看不到这个工具，用户也无从判断是"没配"还是"坏了"。改成注册一个占位实现后，模型能在调用时拿到明确原因并转述给用户。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn searching_without_a_key_says_so_instead_of_pretending() {
        let err = UnconfiguredProvider.search("q", 5).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not configured"), "got: {msg}");
        // 必须指出去哪儿配，否则用户只看到「不可用」。
        assert!(msg.contains("网页搜索"), "got: {msg}");
    }

    #[test]
    fn its_name_is_not_a_real_engine() {
        assert_eq!(UnconfiguredProvider.name(), "unconfigured");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::unconfigured`
Expected: 编译失败，模块不存在。

- [ ] **Step 3: 实现**

```rust
//! 无 key 时的占位 provider。
//!
//! 为什么不是「干脆不注册工具」：那样模型看不到 `web_search`，用户也无从判断
//! 是"没配置"还是"功能坏了"。注册一个占位实现后，调用会拿到一条明确原因，
//! 模型能原样讲给用户听。

use async_trait::async_trait;

use crate::error::ToolsError;
use crate::web::provider::{SearchResult, WebSearchProvider};

pub struct UnconfiguredProvider;

#[async_trait]
impl WebSearchProvider for UnconfiguredProvider {
    fn name(&self) -> &str {
        "unconfigured"
    }

    async fn search(&self, _query: &str, _count: usize) -> Result<Vec<SearchResult>, ToolsError> {
        Err(ToolsError::SearchEngine(
            "web search is not configured: set an API key in Settings > 网页搜索".to_string(),
        ))
    }
}
```

`web/mod.rs` 加：

```rust
pub mod unconfigured;

pub use unconfigured::UnconfiguredProvider;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib web::unconfigured`
Expected: 2 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-tools/src/web/unconfigured.rs yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs \
  && git commit -m "feat(tools): 未配置 key 时给出明确原因而不是让工具消失"
```

---

### Task 5: 注册逻辑改为读配置

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:44-56, 128-140`

**Interfaces:**
- Consumes: Task 1 的 `load_websearch_config_from`；Task 2 的 `with_options`；Task 3 的 `with_default_count`；Task 4 的 `UnconfiguredProvider`
- Produces: `pub fn register_web_search_from(registry: &mut ToolRegistry, path: &Path)`（供 app-server 用真实路径；同时 `register_builtin_tools_*` 内部调用它读默认路径）

- [ ] **Step 1: 写失败的测试**

在 `lib.rs` 的 `mod tests` 里加入：

```rust
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
        std::fs::write(&cfg, r#"{"enabled":true,"engines":{"bocha":{"api_key":""}}}"#).unwrap();
        // 保证测试不受本机真实环境变量影响。
        unsafe { std::env::remove_var("BOCHA_API_KEY") };

        let mut registry = yi_agent_core::ToolRegistry::new();
        register_web_search_from(&mut registry, &cfg);
        let tool = registry.get("web_search").expect("must still be registered");
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib register_web_search`
Expected: 编译失败，`register_web_search_from` 未定义。

- [ ] **Step 3: 实现**

在 `lib.rs` 顶部 use 区补：

```rust
use std::path::Path;

use crate::web::config::{load_websearch_config_from, EngineConfig, WebSearchConfig};
```

（按现有 use 风格放在 `use std::path::PathBuf;` 旁边，`Path` 合并进同一行。）

新增公共函数：

```rust
/// 按配置注册网页搜索工具。
///
/// 三种情形，都刻意与「静默消失」不同：
/// - `enabled == false`：用户明确关掉了，不注册。
/// - `enabled == true` 且无 key：注册占位实现，调用时给出「未配置」原因。
/// - `enabled == true` 且有 key：注册真实的 Bocha provider。
pub fn register_web_search_from(registry: &mut ToolRegistry, path: &Path) {
    let cfg = load_websearch_config_from(path);
    if !cfg.enabled {
        return;
    }
    let engine = cfg.active().cloned().unwrap_or_default();
    let engine: Arc<dyn WebSearchProvider> = match engine.resolved_api_key() {
        Some(key) => Arc::new(BochaSearchProvider::with_options(
            key,
            engine.resolved_base_url(),
            engine.freshness,
        )),
        None => Arc::new(UnconfiguredProvider),
    };
    registry.register(Arc::new(WebSearchTool::with_default_count(
        engine,
        engine_clamped(&cfg),
    )));
}

/// 默认条数取配置里钳制后的值；引擎缺失时回 `DEFAULT_COUNT`。
fn engine_clamped(cfg: &WebSearchConfig) -> usize {
    cfg.active()
        .map(EngineConfig::clamped_count)
        .unwrap_or(25)
}
```

把 `register_builtin_tools_with_controller_and_env` 末尾的两段替换成：

```rust
    // Web tools
    registry.register(Arc::new(WebFetchTool::new()));
    register_web_search_from(registry, &web::config::websearch_path());
```

同时更新该函数上方 doc 里关于 `BOCHA_API_KEY` 的两行说明（改成"按 `~/.yi-agent/websearch.json` 决定，详见 `register_web_search_from`"）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-tools/src/lib.rs \
  && git commit -m "feat(tools): 按 websearch.json 决定注册哪种搜索工具"
```

---

### Task 6: websearch RPC handler

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/websearch_rpc.rs`
- Test: 同文件内的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 的全部类型；`yi_agent_runtime::models::mask_key`
- Produces:
  - `pub fn handle_websearch_request(path: &Path, method: &str, params: &Value) -> Result<Value, RpcError>`
  - 方法：`websearch/get` / `websearch/set` / `websearch/test`
  - `websearch/get` 响应：`{ "enabled": bool, "active_engine": str, "engines": { "bocha": { "api_key_masked": str, "has_key": bool, "base_url": str, "count": usize, "freshness": str } }, "path": str }`
  - `websearch/set` 响应：`{ "ok": true }`
  - `websearch/test` 响应：`{ "ok": bool, "message": str, "elapsed_ms": u64 }`

- [ ] **Step 1: 写失败的测试**

先建文件，只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_cfg(json_text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("websearch.json");
        if !json_text.is_empty() {
            std::fs::write(&path, json_text).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn get_never_returns_the_plaintext_key() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":"sk-secret-1234"}}}"#);
        let out = handle_websearch_request(&p, "websearch/get", &json!({})).unwrap();
        let bocha = &out["engines"]["bocha"];
        assert_eq!(bocha["api_key_masked"], "••••1234");
        assert_eq!(bocha["has_key"], true);
        // 明文绝不出现在整个响应里。
        assert!(!out.to_string().contains("sk-secret-1234"));
    }

    #[test]
    fn get_defaults_when_no_file_exists() {
        let (_d, p) = tmp_cfg("");
        let out = handle_websearch_request(&p, "websearch/get", &json!({})).unwrap();
        assert_eq!(out["enabled"], true);
        assert_eq!(out["active_engine"], "bocha");
        assert_eq!(out["engines"]["bocha"]["count"], 25);
        assert_eq!(out["engines"]["bocha"]["freshness"], "noLimit");
        assert_eq!(out["engines"]["bocha"]["has_key"], false);
    }

    #[test]
    fn set_writes_the_fields_it_was_given() {
        let (_d, p) = tmp_cfg("");
        handle_websearch_request(
            &p,
            "websearch/set",
            &json!({"enabled": false, "engines": {"bocha": {"count": 9, "freshness": "oneDay"}}}),
        )
        .unwrap();
        let out = handle_websearch_request(&p, "websearch/get", &json!({})).unwrap();
        assert_eq!(out["enabled"], false);
        assert_eq!(out["engines"]["bocha"]["count"], 9);
        assert_eq!(out["engines"]["bocha"]["freshness"], "oneDay");
    }

    #[test]
    fn set_ignores_a_masked_key_instead_of_storing_it() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":"sk-real-9999"}}}"#);
        handle_websearch_request(
            &p,
            "websearch/set",
            &json!({"engines": {"bocha": {"api_key": "••••9999"}}}),
        )
        .unwrap();
        let out = handle_websearch_request(&p, "websearch/get", &json!({})).unwrap();
        assert_eq!(out["engines"]["bocha"]["api_key_masked"], "••••9999");
        // 磁盘上必须还是原来那把真 key，而不是被掩码覆盖。
        let saved = std::fs::read_to_string(&p).unwrap();
        assert!(saved.contains("sk-real-9999"), "masked value overwrote the key: {saved}");
    }

    #[test]
    fn set_preserves_keys_it_does_not_own() {
        let (_d, p) = tmp_cfg(r#"{"future_key":"keep-me","engines":{"bocha":{}}}"#);
        handle_websearch_request(&p, "websearch/set", &json!({"enabled": false})).unwrap();
        let saved = std::fs::read_to_string(&p).unwrap();
        assert!(saved.contains("keep-me"), "unrelated key dropped: {saved}");
    }

    /// 测试连接走真实网络，这里只验「无 key 时立刻拒绝，不发注定 401 的请求」
    /// （与 `model/test` 的 `resolve_test_settings` 同语义）。
    #[test]
    fn test_refuses_when_no_key_is_available() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":""}}}"#);
        unsafe { std::env::remove_var("BOCHA_API_KEY") };
        let err = handle_websearch_request(&p, "websearch/test", &json!({})).unwrap_err();
        assert!(err.message.contains("no api key"), "got: {}", err.message);
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        let (_d, p) = tmp_cfg("");
        let err = handle_websearch_request(&p, "websearch/nope", &json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server websearch_rpc`
Expected: 编译失败，`handle_websearch_request` 未定义。

- [ ] **Step 3: 实现**

```rust
//! `websearch/*` RPC：桌面设置页「网页搜索」Tab 的读写与连通性测试。
//!
//! 三类方法对应三个权限档位：`get` 只读盘且只回掩码（`Observe`）；
//! `set` 与 `test` 触及密钥且会落盘/发网络请求（`Control`），档位判在
//! `rpc/websearch.rs`。
//!
//! 与 `model/*` 同构：密钥从不下发明文，`set` 收到掩码值时跳过该字段。

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use yi_agent_runtime::models::mask_key;
use yi_agent_tools::web::config::{
    load_websearch_config_from, EngineConfig, Freshness, WebSearchConfig,
};
use yi_agent_tools::{BochaSearchProvider, WebSearchProvider};

use crate::protocol::RpcError;

pub fn handle_websearch_request(
    path: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "websearch/get" => get(path),
        "websearch/set" => set(path, params),
        "websearch/test" => test(path, params),
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn get(path: &Path) -> Result<Value, RpcError> {
    let cfg = load_websearch_config_from(path);
    let engines: serde_json::Map<String, Value> = cfg
        .engines
        .iter()
        .map(|(id, e)| (id.clone(), engine_view(e)))
        .collect();
    Ok(json!({
        "enabled": cfg.enabled,
        "active_engine": cfg.active_engine,
        "engines": Value::Object(engines),
        "path": path.to_string_lossy(),
    }))
}

fn engine_view(e: &EngineConfig) -> Value {
    json!({
        "api_key_masked": mask_key(&e.api_key),
        // `resolved_api_key` 才会看环境变量：面板据此决定要不要提示「未配置」。
        "has_key": e.resolved_api_key().is_some(),
        "base_url": e.resolved_base_url(),
        "count": e.clamped_count(),
        "freshness": e.freshness.as_bocha_str(),
    })
}

fn set(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let mut cfg = load_websearch_config_from(path);

    if let Some(enabled) = params.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(id) = params.get("active_engine").and_then(Value::as_str) {
        if cfg.engines.contains_key(id) {
            cfg.active_engine = id.to_string();
        }
    }
    if let Some(engines) = params.get("engines").and_then(Value::as_object) {
        for (id, patch) in engines {
            let entry = cfg.engines.entry(id.clone()).or_default();
            apply_engine_patch(entry, patch);
        }
    }

    save_config(path, &cfg).map_err(|e| RpcError::internal(format!("failed to write config: {e}")))?;
    Ok(json!({ "ok": true }))
}

fn apply_engine_patch(entry: &mut EngineConfig, patch: &Value) {
    if let Some(key) = patch.get("api_key").and_then(Value::as_str) {
        // 掩码值不得回写：它代表「用户没改」，不是新 key。
        let is_mask = key.contains('•');
        if !is_mask && !key.is_empty() {
            entry.api_key = key.to_string();
        }
    }
    if let Some(url) = patch.get("base_url").and_then(Value::as_str) {
        entry.base_url = url.trim().to_string();
    }
    if let Some(count) = patch.get("count").and_then(Value::as_u64) {
        entry.count = (count as usize).clamp(1, 50);
    }
    if let Some(f) = patch.get("freshness").and_then(Value::as_str) {
        entry.freshness = Freshness::parse(f);
    }
}

/// 原子落盘：临时文件 + rename，临时名含 pid 与递增序号，避免并发写者互相截断
/// （与 `settings_store::temp_path_for` 同一约定）。
fn save_config(path: &Path, cfg: &WebSearchConfig) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 读-改-写：以既有对象为底，只覆盖本模块拥有的键，其余顶层键原样保留。
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let ours = serde_json::to_value(cfg)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(map) = ours.as_object() {
        for (k, v) in map {
            object.insert(k.clone(), v.clone());
        }
    }

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), seq));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(object))?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn test(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let cfg = load_websearch_config_from(path);
    let engine = cfg.active().cloned().unwrap_or_default();

    // 参数里的明文 key 优先（用户刚填还没保存）；否则按磁盘上的值 + 环境变量兜底。
    let explicit = params
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty() && !k.contains('•'))
        .map(str::to_string);
    let api_key = match explicit.or_else(|| engine.resolved_api_key()) {
        Some(key) => key,
        None => {
            return Err(RpcError::invalid_params(
                "no api key: enter one before testing the connection",
            ))
        }
    };
    let base_url = params
        .get("base_url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| engine.resolved_base_url());
    let freshness = params
        .get("freshness")
        .and_then(Value::as_str)
        .map(Freshness::parse)
        .unwrap_or(engine.freshness);

    let provider = BochaSearchProvider::with_options(api_key, base_url, freshness);
    let started = Instant::now();

    // 同步 RPC handler 里跑异步搜索：本函数在 app-server 的 tokio 运行时内被调用，
    // 用一个短命 runtime 会 panic（cannot start a runtime from within a runtime），
    // 故改用 `block_in_place` 交给 blocking 线程池。
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            tokio::time::timeout(Duration::from_secs(30), provider.search("yi-agent", 1)).await
        })
    });

    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(match result {
        Ok(Ok(results)) => json!({
            "ok": true,
            "message": format!("连接正常，返回 {} 条结果", results.len()),
            "elapsed_ms": elapsed_ms,
        }),
        Ok(Err(e)) => json!({
            "ok": false,
            "message": describe_error(&e.to_string()),
            "elapsed_ms": elapsed_ms,
        }),
        Err(_) => json!({
            "ok": false,
            "message": "请求超时（30 秒）",
            "elapsed_ms": elapsed_ms,
        }),
    })
}

/// 把 provider 的英文错误翻成用户能动的提示。
fn describe_error(raw: &str) -> String {
    if raw.contains("invalid API key") {
        "API Key 无效，请检查后重试".to_string()
    } else if raw.contains("insufficient balance") {
        "账户余额不足".to_string()
    } else if raw.contains("rate limited") {
        "请求太频繁，已被限流".to_string()
    } else {
        format!("连接失败：{raw}")
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server websearch_rpc`
Expected: 7 个测试 PASS。

> 注：`test_refuses_when_no_key_is_available` 用 `unsafe { std::env::remove_var(..) }`
> 是因为 Rust 2024 edition 把 `set_var` / `remove_var` 标成了 unsafe。若本 crate
> 是 2021 edition，去掉 `unsafe` 块即可（编译器会给出提示）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-app-server/src/websearch_rpc.rs \
  && git commit -m "feat(app-server): websearch/get|set|test 三个 RPC handler"
```

---

### Task 7: RPC 域分派与权限

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/websearch.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs:18-32`（模块表）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（暴露 `websearch_rpc` 模块，紧邻 `mcp_rpc`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`AppState` 构造处填 `websearch_path`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` 的 `AppState` 结构体

**Interfaces:**
- Consumes: Task 6 的 `handle_websearch_request`
- Produces: `AppState` 新字段 `pub websearch_path: &'a Path`

- [ ] **Step 1: 写失败的测试**

在 `rpc_tests.rs` 末尾加入（与既有 `model/*` 权限测试同形）：

```rust
#[tokio::test]
async fn websearch_get_is_readable_by_an_observe_client_but_set_is_not() {
    let h = harness_with_scope(Scope::Observe).await; // 复用文件内既有 helper
    let ok = h.call("websearch/get", serde_json::json!({})).await;
    assert!(ok.is_ok(), "Observe may read the masked config: {ok:?}");

    let err = h
        .call("websearch/set", serde_json::json!({"enabled": false}))
        .await
        .expect_err("Observe must not write");
    assert_eq!(err.code, -32014, "expected insufficient_scope");
}
```

> 若文件内没有可直接复用的 `harness_with_scope` / `call` helper，则复用本文件
> 里 `model/list` 那条权限测试所用的同一套构造方式，只替换 method 名——先找到
> 该测试（搜 `insufficient_scope` 与 `model/upsert` 同现的位置）再照抄骨架。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server websearch_get_is_readable`
Expected: 失败——`websearch/get` 落到 `method_not_found`。

- [ ] **Step 3: 实现**

`rpc/websearch.rs`：

```rust
//! `websearch/*` 域分派。权限档位：`get` 只读盘且回掩码（`Observe`）；
//! `set` / `test` 触及密钥、会落盘或发网络请求（`Control`），与 `model/*` 同档。

use crate::protocol::{RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "websearch/get" => {}
        "websearch/set" | "websearch/test" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    match crate::websearch_rpc::handle_websearch_request(
        state.websearch_path,
        req.method.as_str(),
        &req.params,
    ) {
        Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
        Err(error) => write_response(state.hub, state.client, err_response(id, error)).await?,
    }
    Ok(Flow::Continue)
}
```

`rpc/mod.rs`：模块表加 `pub(crate) mod websearch;`；`AppState` 结构体加字段

```rust
    /// 网页搜索配置文件路径（`~/.yi-agent/websearch.json`）。可注入，让测试
    /// 不必改 `HOME` 这类进程全局量。
    pub websearch_path: &'a Path,
```

`lib.rs` 的模块声明区（`mod mcp_rpc;` 附近）加 `mod websearch_rpc;`，并按现有风格
（若这些 `*_rpc` 模块是 `pub mod` 则跟随）导出
`pub use websearch_rpc::handle_websearch_request;` 以外的内容不需要暴露。

`server.rs` 里构造 `AppState { .. }` 的那一处加：

```rust
        websearch_path: &websearch_path,
```

> 注意生命周期：`websearch_path()` 返回 `PathBuf`，`AppState` 借引用。若该构造点
> 无法让 `PathBuf` 活到 `AppState` 的作用域之外，先在构造 `AppState` 之前绑一个
> 局部量 `let websearch_path = yi_agent_tools::web::config::websearch_path();`（`server.rs` 在 app-server 内，用全路径是正常的），
> 再传 `&websearch_path`。多个构造点（如 `run` 与 `serve` 两个入口）都要加。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server websearch`
Expected: Task 6 的 7 个 + 本任务 1 个全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/websearch.rs \
      yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs \
      yi-agent-rs/crates/yi-agent-app-server/src/lib.rs \
      yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
      yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs \
  && git commit -m "feat(app-server): 接入 websearch RPC 域与权限档位"
```

---

### Task 8: 桌面 RPC 类型收敛

**Files:**
- Create: `desktop/src/lib/websearch.ts`
- Test: `desktop/src/lib/websearch.test.ts`

**Interfaces:**
- Produces:
  - `export type WebSearchCall = (method: string, params: unknown) => Promise<unknown>`
  - `export interface EngineView { apiKeyMasked: string; hasKey: boolean; baseUrl: string; count: number; freshness: Freshness }`
  - `export interface WebSearchView { enabled: boolean; activeEngine: string; engines: Record<string, EngineView>; path: string }`
  - `export type Freshness = "noLimit" | "oneDay" | "oneWeek" | "oneMonth" | "oneYear"`
  - `export function parseWebSearchView(raw: unknown): WebSearchView`
  - `export function parseTestResult(raw: unknown): { ok: boolean; message: string; elapsedMs: number }`

**为什么单独一层**：服务端回手写 `json!`，不走 ts-rs。面板宁可少列，也不能因为一条脏数据整页崩掉——与 `lib/mcp.ts` 同一策略。

- [ ] **Step 1: 写失败的测试**

```ts
import { describe, expect, it } from "vitest";
import { parseTestResult, parseWebSearchView } from "./websearch";

describe("parseWebSearchView", () => {
  it("maps the wire shape onto camelCase for the UI", () => {
    const v = parseWebSearchView({
      enabled: true,
      active_engine: "bocha",
      path: "/h/.yi-agent/websearch.json",
      engines: {
        bocha: {
          api_key_masked: "••••1234",
          has_key: true,
          base_url: "https://api.bocha.cn/v1",
          count: 25,
          freshness: "oneDay",
        },
      },
    });
    expect(v.engines.bocha.apiKeyMasked).toBe("••••1234");
    expect(v.engines.bocha.count).toBe(25);
    expect(v.engines.bocha.freshness).toBe("oneDay");
    expect(v.path).toBe("/h/.yi-agent/websearch.json");
  });

  it("falls back to safe defaults for a malformed payload", () => {
    const v = parseWebSearchView({ engines: "nonsense" });
    expect(v.enabled).toBe(true);
    expect(v.activeEngine).toBe("bocha");
    // 缺失的引擎补一份默认视图，面板才不会因为 key 不存在而崩。
    expect(v.engines.bocha.count).toBe(25);
    expect(v.engines.bocha.freshness).toBe("noLimit");
    expect(v.engines.bocha.hasKey).toBe(false);
  });

  it("clamps an out-of-range count and an unknown freshness", () => {
    const v = parseWebSearchView({
      engines: { bocha: { count: 900, freshness: "lastDecade" } },
    });
    expect(v.engines.bocha.count).toBe(50);
    expect(v.engines.bocha.freshness).toBe("noLimit");
  });
});

describe("parseTestResult", () => {
  it("reads a success payload", () => {
    expect(parseTestResult({ ok: true, message: "连接正常", elapsed_ms: 120 })).toEqual({
      ok: true,
      message: "连接正常",
      elapsedMs: 120,
    });
  });

  it("treats a malformed payload as a failure rather than throwing", () => {
    const r = parseTestResult(null);
    expect(r.ok).toBe(false);
    expect(r.message).toContain("失败");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/websearch.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现**

```ts
/**
 * 桌面与宿主之间的网页搜索通道。形状与 `mcp.ts` / `skills.ts` 一致：
 * 服务端回手写 `json!`（不走 ts-rs DTO），故本模块做一次形状收敛——
 * 坏行丢掉、缺失字段补安全默认，绝不把半成品往下传。
 */

export type WebSearchCall = (method: string, params: unknown) => Promise<unknown>;

export type Freshness = "noLimit" | "oneDay" | "oneWeek" | "oneMonth" | "oneYear";

const FRESHNESS_VALUES: Freshness[] = ["noLimit", "oneDay", "oneWeek", "oneMonth", "oneYear"];

export interface EngineView {
  apiKeyMasked: string;
  hasKey: boolean;
  baseUrl: string;
  count: number;
  freshness: Freshness;
}

export interface WebSearchView {
  enabled: boolean;
  activeEngine: string;
  engines: Record<string, EngineView>;
  path: string;
}

const DEFAULT_ENGINE: EngineView = {
  apiKeyMasked: "",
  hasKey: false,
  baseUrl: "https://api.bocha.cn/v1",
  count: 25,
  freshness: "noLimit",
};

function str(v: unknown, fallback: string): string {
  return typeof v === "string" ? v : fallback;
}

function bool(v: unknown, fallback: boolean): boolean {
  return typeof v === "boolean" ? v : fallback;
}

function clampCount(v: unknown): number {
  if (typeof v !== "number" || !Number.isFinite(v)) return DEFAULT_ENGINE.count;
  return Math.min(50, Math.max(1, Math.trunc(v)));
}

function parseFreshness(v: unknown): Freshness {
  return FRESHNESS_VALUES.includes(v as Freshness) ? (v as Freshness) : "noLimit";
}

function parseEngine(v: unknown): EngineView {
  const o = (v ?? {}) as Record<string, unknown>;
  return {
    apiKeyMasked: str(o.api_key_masked, ""),
    hasKey: bool(o.has_key, false),
    baseUrl: str(o.base_url, DEFAULT_ENGINE.baseUrl),
    count: clampCount(o.count),
    freshness: parseFreshness(o.freshness),
  };
}

export function parseWebSearchView(raw: unknown): WebSearchView {
  const o = (raw ?? {}) as Record<string, unknown>;
  const enginesRaw = o.engines;
  const engines: Record<string, EngineView> = {};
  if (enginesRaw && typeof enginesRaw === "object" && !Array.isArray(enginesRaw)) {
    for (const [id, value] of Object.entries(enginesRaw as Record<string, unknown>)) {
      engines[id] = parseEngine(value);
    }
  }
  // bocha 永远有一份视图：面板绑定它，缺了就会崩。
  if (!engines.bocha) engines.bocha = { ...DEFAULT_ENGINE };
  return {
    enabled: bool(o.enabled, true),
    activeEngine: str(o.active_engine, "bocha"),
    engines,
    path: str(o.path, ""),
  };
}

export interface TestResult {
  ok: boolean;
  message: string;
  elapsedMs: number;
}

export function parseTestResult(raw: unknown): TestResult {
  const o = (raw ?? {}) as Record<string, unknown>;
  return {
    ok: bool(o.ok, false),
    message: str(o.message, "测试失败：响应无法解析"),
    elapsedMs: typeof o.elapsed_ms === "number" ? o.elapsed_ms : 0,
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/websearch.test.ts`
Expected: 5 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/websearch.ts desktop/src/lib/websearch.test.ts \
  && git commit -m "feat(desktop): 网页搜索 RPC 响应的形状收敛"
```

---

### Task 9: 设置页「网页搜索」面板

**Files:**
- Create: `desktop/src/components/SettingsWebSearchTab.tsx`
- Test: `desktop/src/components/SettingsWebSearchTab.test.tsx`

**Interfaces:**
- Consumes: Task 8 的 `WebSearchCall` / `parseWebSearchView` / `parseTestResult` / `Freshness`
- Produces: `export function SettingsWebSearchTab({ call }: { call: WebSearchCall })`

- [ ] **Step 1: 写失败的测试**

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsWebSearchTab } from "./SettingsWebSearchTab";

afterEach(cleanup);

const baseView = {
  enabled: true,
  active_engine: "bocha",
  path: "/h/.yi-agent/websearch.json",
  engines: {
    bocha: {
      api_key_masked: "••••1234",
      has_key: true,
      base_url: "https://api.bocha.cn/v1",
      count: 25,
      freshness: "noLimit",
    },
  },
};

describe("SettingsWebSearchTab", () => {
  it("fills the form from websearch/get and never shows a plaintext key", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get" ? baseView : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);

    const key = (await screen.findByLabelText("API Key")) as HTMLInputElement;
    // 明文永不下发，输入框的占位符就是掩码。
    expect(key.value).toBe("");
    expect(key.placeholder).toBe("••••1234");
    expect((screen.getByLabelText("结果条数") as HTMLInputElement).value).toBe("25");
    expect((screen.getByLabelText("时效范围") as HTMLSelectElement).value).toBe("noLimit");
  });

  it("saves only the fields the user changed", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get" ? baseView : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    fireEvent.change(screen.getByLabelText("结果条数"), { target: { value: "10" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith("websearch/set", {
        engines: { bocha: { count: 10 } },
      }),
    );
  });

  it("never sends the mask back as a key", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get"
        ? { ...baseView, engines: { bocha: { ...baseView.engines.bocha, has_key: false } } }
        : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    // 只改开关，不碰 key 框。
    fireEvent.click(screen.getByLabelText("启用网页搜索"));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(call).toHaveBeenCalledWith("websearch/set", { enabled: false }));
  });

  it("shows the server's message when a connection test fails", async () => {
    const call = vi.fn(async (m: string) => {
      if (m === "websearch/get") return baseView;
      if (m === "websearch/test")
        return { ok: false, message: "API Key 无效，请检查后重试", elapsed_ms: 80 };
      return { ok: true };
    });
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    fireEvent.click(screen.getByRole("button", { name: "测试连接" }));
    expect(await screen.findByText("API Key 无效，请检查后重试")).toBeTruthy();
  });

  it("warns that search is unavailable when no key is configured", async () => {
    const call = vi.fn(async () => ({
      ...baseView,
      engines: { bocha: { ...baseView.engines.bocha, has_key: false } },
    }));
    render(<SettingsWebSearchTab call={call} />);
    expect(await screen.findByText(/未配置 API Key/)).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsWebSearchTab.test.tsx`
Expected: FAIL — 组件不存在。

- [ ] **Step 3: 实现**

```tsx
import { useCallback, useEffect, useState } from "react";
import {
  parseTestResult,
  parseWebSearchView,
  type Freshness,
  type WebSearchCall,
} from "../lib/websearch";

const FRESHNESS_CHOICES: Array<{ value: Freshness; label: string }> = [
  { value: "noLimit", label: "不限时间" },
  { value: "oneDay", label: "最近一天" },
  { value: "oneWeek", label: "最近一周" },
  { value: "oneMonth", label: "最近一月" },
  { value: "oneYear", label: "最近一年" },
];

/**
 * 「网页搜索」页。
 *
 * 只有 Bocha 一个引擎卡片——`engines` 在配置里是 map，多引擎时这里改成横向
 * 排列即可，不必动数据层。
 *
 * 保存与测试都走宿主接缝（`websearch/set` / `websearch/test`）。密钥框从不回填
 * 明文：服务端只给掩码，空值等于「不改」。
 */
export function SettingsWebSearchTab({ call }: { call: WebSearchCall }) {
  const [enabled, setEnabled] = useState(true);
  const [apiKey, setApiKey] = useState("");
  const [keyMask, setKeyMask] = useState("");
  const [hasKey, setHasKey] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [count, setCount] = useState(25);
  const [freshness, setFreshness] = useState<Freshness>("noLimit");
  /** 上次读/存时的值，用来算出「用户改了哪几项」。 */
  const [baseline, setBaseline] = useState({ enabled: true, baseUrl: "", count: 25, freshness: "noLimit" as Freshness });
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = parseWebSearchView(await call("websearch/get", {}));
      const bocha = view.engines.bocha;
      setEnabled(view.enabled);
      setApiKey("");
      setKeyMask(bocha.apiKeyMasked);
      setHasKey(bocha.hasKey);
      setBaseUrl(bocha.baseUrl);
      setCount(bocha.count);
      setFreshness(bocha.freshness);
      setBaseline({
        enabled: view.enabled,
        baseUrl: bocha.baseUrl,
        count: bocha.count,
        freshness: bocha.freshness,
      });
    } catch (e) {
      setNotice({ ok: false, text: `读取配置失败：${String(e)}` });
    } finally {
      setLoading(false);
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    // 只传真正变了的字段：既让服务端不必猜测意图，也让「掩码框为空表示不改」
    // 这条规则自然成立——没改就不会有 `api_key` 这一项。
    const enginePatch: Record<string, unknown> = {};
    if (apiKey.trim()) enginePatch.api_key = apiKey.trim();
    if (baseUrl !== baseline.baseUrl) enginePatch.base_url = baseUrl;
    if (count !== baseline.count) enginePatch.count = count;
    if (freshness !== baseline.freshness) enginePatch.freshness = freshness;

    const body: Record<string, unknown> = {};
    if (enabled !== baseline.enabled) body.enabled = enabled;
    if (Object.keys(enginePatch).length > 0) body.engines = { bocha: enginePatch };

    if (Object.keys(body).length === 0) {
      setNotice({ ok: true, text: "没有需要保存的改动" });
      return;
    }

    setSaving(true);
    setNotice(null);
    try {
      await call("websearch/set", body);
      setNotice({ ok: true, text: "已保存" });
      await load();
    } catch (e) {
      setNotice({ ok: false, text: `保存失败：${String(e)}` });
    } finally {
      setSaving(false);
    }
  };

  const runTest = async () => {
    setTesting(true);
    setNotice(null);
    try {
      const result = parseTestResult(
        await call("websearch/test", apiKey.trim() ? { api_key: apiKey.trim() } : {}),
      );
      setNotice({
        ok: result.ok,
        text: `${result.message}（${result.elapsedMs} ms）`,
      });
    } catch (e) {
      setNotice({ ok: false, text: `测试失败：${String(e)}` });
    } finally {
      setTesting(false);
    }
  };

  if (loading) return <section className="p-5 text-sm text-fg-muted">读取中…</section>;

  return (
    <section className="p-5">
      <label className="flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          aria-label="启用网页搜索"
          checked={enabled}
          onChange={(e) => setEnabled(e.target.checked)}
        />
        <span>启用网页搜索</span>
      </label>

      <h3 className="mt-6 text-sm font-medium text-fg">搜索引擎</h3>
      <div className="mt-3 rounded-md border border-line p-4">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-fg">Bocha</span>
          <span className="rounded bg-raised px-2 py-0.5 text-xs text-fg-muted">当前引擎</span>
        </div>

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-api-key">
          API Key
        </label>
        <input
          id="ws-api-key"
          type="password"
          aria-label="API Key"
          value={apiKey}
          placeholder={keyMask || "请输入 API Key"}
          onChange={(e) => setApiKey(e.target.value)}
          className="mt-1 w-full rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-base-url">
          Base URL
        </label>
        <input
          id="ws-base-url"
          aria-label="Base URL"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          className="mt-1 w-full rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-count">
          结果条数
        </label>
        <input
          id="ws-count"
          type="number"
          min={1}
          max={50}
          aria-label="结果条数"
          value={count}
          onChange={(e) => setCount(Number(e.target.value))}
          className="mt-1 w-32 rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-freshness">
          时效范围
        </label>
        <select
          id="ws-freshness"
          aria-label="时效范围"
          value={freshness}
          onChange={(e) => setFreshness(e.target.value as Freshness)}
          className="mt-1 w-48 rounded border border-line bg-base px-3 py-1.5 text-sm"
        >
          {FRESHNESS_CHOICES.map((c) => (
            <option key={c.value} value={c.value}>
              {c.label}
            </option>
          ))}
        </select>

        {!hasKey && !apiKey.trim() && (
          <p role="alert" className="mt-4 text-xs text-amber-500">
            未配置 API Key，网页搜索将不可用。
          </p>
        )}
      </div>

      <div className="mt-6 flex items-center gap-3">
        <button
          type="button"
          onClick={() => void save()}
          disabled={saving}
          className="rounded bg-accent px-4 py-1.5 text-sm text-white disabled:opacity-50"
        >
          {saving ? "保存中…" : "保存"}
        </button>
        <button
          type="button"
          onClick={() => void runTest()}
          disabled={testing}
          className="rounded border border-line px-4 py-1.5 text-sm disabled:opacity-50"
        >
          {testing ? "测试中…" : "测试连接"}
        </button>
      </div>

      {notice && (
        <p role={notice.ok ? "status" : "alert"} className={`mt-3 text-xs ${notice.ok ? "text-accent" : "text-amber-500"}`}>
          {notice.text}
        </p>
      )}
    </section>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsWebSearchTab.test.tsx`
Expected: 5 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsWebSearchTab.tsx desktop/src/components/SettingsWebSearchTab.test.tsx \
  && git commit -m "feat(desktop): 设置页网页搜索面板"
```

---

### Task 10: 接进 SettingsDialog 与 App

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx:12-27`（TABS）、`:100-124`（props）、`:346-359`（渲染分支）
- Modify: `desktop/src/App.tsx:434-445` 附近（新增 `websearchCall`）、`:2802` 附近（传 prop）
- Test: `desktop/src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Consumes: Task 9 的 `SettingsWebSearchTab`；Task 8 的 `WebSearchCall`
- Produces: `SettingsDialog` 新可选 prop `websearchCall?: WebSearchCall`

- [ ] **Step 1: 写失败的测试**

在 `SettingsDialog.test.tsx` 里加入：

```tsx
  it("renders the 网页搜索 tab and reads config through the injected seam", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get"
        ? {
            enabled: true,
            active_engine: "bocha",
            path: "/p/websearch.json",
            engines: {
              bocha: {
                api_key_masked: "••••1234",
                has_key: true,
                base_url: "https://api.bocha.cn/v1",
                count: 25,
                freshness: "noLimit",
              },
            },
          }
        : { ok: true },
    );
    render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} websearchCall={call} />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "网页搜索" }));
    expect(await screen.findByLabelText("API Key")).toBeTruthy();
    expect(call).toHaveBeenCalledWith("websearch/get", {});
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx`
Expected: FAIL — 找不到「网页搜索」Tab。

- [ ] **Step 3: 实现**

`SettingsDialog.tsx` 三处：

1. 导入：

```tsx
import { SettingsWebSearchTab } from "./SettingsWebSearchTab";
import type { WebSearchCall } from "../lib/websearch";
```

2. `TABS` 表在 `{ id: "mcp", label: "MCP" }` 之后插入：

```tsx
  { id: "websearch", label: "网页搜索" },
```

3. props 声明在 `flagsCall` 之前加：

```tsx
  /** 「网页搜索」Tab 的宿主 RPC 接缝（websearch/get|set|test）。 */
  websearchCall?: WebSearchCall;
```

4. 渲染分支在 `active === "mcp"` 那支之后插入：

```tsx
            ) : active === "websearch" && websearchCall ? (
              <SettingsWebSearchTab call={websearchCall} />
```

`App.tsx`：

1. 在 `mcpCall` 定义之后加（照抄同一模式，保持空依赖）：

```tsx
  /**
   * 「网页搜索」Tab 的宿主接缝。理由同 `modelCall`：只闭合 `clientRef`，空依赖
   * 即稳定；若每次渲染换新身份，面板的读取 effect 会跟着重跑，把用户没保存的
   * 编辑冲掉。
   */
  const websearchCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );
```

2. `<SettingsDialog ... />` 处加 `websearchCall={websearchCall}`（与 `mcpCall={mcpCall}` 同处）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsWebSearchTab.test.tsx`
Expected: 全部 PASS（含既有 SettingsDialog 用例）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/App.tsx \
  && git commit -m "feat(desktop): 设置页接入网页搜索 Tab"
```

---

### Task 11: 全量验证与文档

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md:36`
- Modify: `docs/project-management/README.md`（若计数变化）

- [ ] **Step 1: 跑 Rust 全量测试**

先确认没有残留 cargo 进程（项目规范）：

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "clean"
```

再按 crate 逐个跑（避免 workspace 全量 OOM）：

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-app-server
```

Expected: 全部 PASS。若出现 exit 137 或卡死，按 `CLAUDE.md` 的僵尸进程排查步骤处理。

- [ ] **Step 2: 跑桌面全量测试与类型检查**

```bash
cd desktop && npx tsc --noEmit && npx vitest run
```

Expected: 无类型错误，全部测试 PASS。

- [ ] **Step 3: 更新项目进度文档**

`docs/project-management/yi-agent-tools.md` 里 Web 工具那一行（现为第 36 行）改为：

```markdown
- [x] Web 工具：WebFetch + WebSearch — `crates/yi-agent-tools/src/web/` 目录；WebSearch 按 `~/.yi-agent/websearch.json` 决定注册形式（关闭则不注册；无 key 注册占位实现并在调用时说明原因；有 key 注册 Bocha provider，端点/条数/时效可配）；验证：`cargo test -p yi-agent-tools --lib` — [设计](../superpowers/specs/2026-10-25-websearch-settings-design.md)
```

- [ ] **Step 4: 提交文档**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add docs/project-management/yi-agent-tools.md docs/project-management/README.md \
  && git commit -m "docs: 登记网页搜索配置的完成判据"
```

---

## Self-Review

**Spec 覆盖检查：**

| Spec 要求 | 对应任务 |
|---|---|
| `~/.yi-agent/websearch.json` 落点与结构 | Task 1 |
| `count` 1–50 钳制、默认 25 | Task 1（`clamped_count`）、Task 3（注入）、Task 8（前端兜底） |
| `freshness` 五档、未知回退 | Task 1、Task 2（进请求体） |
| `base_url` 可配、留空回退默认 | Task 1、Task 2 |
| `BOCHA_API_KEY` 兜底 | Task 1（`resolved_api_key`） |
| 无 key 时不再静默消失 | Task 4、Task 5 |
| enabled=false 不注册 | Task 5 |
| `websearch/get|set|test` | Task 6 |
| 掩码回传、掩码不回写 | Task 6（`engine_view` / `apply_engine_patch`） |
| 保留无关键、原子落盘 | Task 6（`save_config`） |
| 权限 Observe / Control | Task 7 |
| 多引擎留位 | Task 1（map + `active_engine`）、Task 9（注释说明横向扩展） |
| 新 Tab + 未配置提示 + 测试连接 | Task 9 |
| 宿主接缝 | Task 10 |
| 五条前端测试 | Task 9 |
| 文档登记 | Task 11 |

**类型一致性抽查：**

- `Freshness` 在 Task 1 定义，Task 2/6/8/9 使用同一组字面量（`noLimit` / `oneDay` / `oneWeek` / `oneMonth` / `oneYear`），前后端一致。
- `mask_key` 输出形如 `••••1234`；Task 6 的 `apply_engine_patch` 用 `contains('•')` 判掩码，Task 8 不解析掩码语义，Task 9 只把掩码当 placeholder——三处一致。
- `websearch/test` 的字段是 `ok` / `message` / `elapsed_ms`；Task 8 的 `parseTestResult` 与 Task 9 的展示对得上。
- `WebSearchCall` 的签名在 Task 8 定义，Task 9 的 prop 与 Task 10 的注入一致。
- `register_web_search_from` 在 Task 5 定义并被内部注册函数调用，签名 `(&mut ToolRegistry, &Path)` 无歧义。

**已知实施风险（写在此处，执行时注意）：**

1. **Task 6 的 `block_in_place`**：需要在多线程 tokio runtime 上调用；若 app-server 用的是 `current_thread` runtime，`block_in_place` 会 panic，届时改用把 `test` 做成 async handler（`rpc/websearch.rs` 里 `Flow` 天然支持 async arm）。执行到此步若测试报 "can call blocking only when running on the multi-threaded runtime"，就按这条走。
2. **Task 7 的 `AppState` 生命周期**：新增 `&'a Path` 字段可能波及多处构造点。若编译报借用期不足，按 Step 3 的提示先绑局部量。
3. **Task 5 的 `unsafe { std::env::remove_var }`**：取决于 crate 的 edition（2024 才需要 unsafe）。编译器会直接告诉你，按提示加或去掉。
