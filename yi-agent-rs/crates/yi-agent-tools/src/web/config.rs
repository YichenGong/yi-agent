//! 网页搜索的机器级配置：`~/.yi-agent/websearch.json`。
//!
//! 选择 JSON 文件而不是环境变量：本文件每次搜索前热读，保存即生效；环境变量
//! 只在进程启动时读一次，改完必须重启。命名与 `models.json` 一致，故字段一律
//! snake_case。
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
/// 结果条数的下界：0 条等于不出结果。
pub const MIN_COUNT: usize = 1;
/// 结果条数的上界：再多只会稀释上下文。
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
    /// 生效的 key：只认文件里的值。
    ///
    /// 过去这里还有一层 `std::env::var("BOCHA_API_KEY")` 回退，目的是「老用户
    /// 不改配置也能继续用」。`.env` 下线后那个 env 值在下次启动时必然为 `None`，
    /// 回退只会造出「当前进程能用、重启后静默失效」的难查故障。迁移已把旧 key
    /// 写进 `websearch.json`，故文件是唯一来源。
    pub fn resolved_api_key(&self) -> Option<String> {
        if self.api_key.is_empty() {
            None
        } else {
            Some(self.api_key.clone())
        }
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

    /// 当前生效引擎的配置。未知 id 已在加载期纠正回 bocha。
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
            return WebSearchConfig::default();
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
    // `or_default()` 与 `or_insert_with(EngineConfig::default)` 等价（`EngineConfig`
    // 有手写 `Default`），但后者会触发 clippy 的 `or_insert_with` 系列 lint。
    cfg.engines.entry(default_engine_id()).or_default();
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            load_websearch_config_from(&path)
                .engine("bocha")
                .unwrap()
                .clamped_count(),
            50
        );
        let (_dir2, path2) = cfg_text(r#"{"engines":{"bocha":{"count":0}}}"#);
        assert_eq!(
            load_websearch_config_from(&path2)
                .engine("bocha")
                .unwrap()
                .clamped_count(),
            1
        );
    }

    #[test]
    fn an_unknown_freshness_falls_back_to_no_limit() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"freshness":"lastDecade"}}}"#);
        assert_eq!(
            load_websearch_config_from(&path)
                .engine("bocha")
                .unwrap()
                .freshness,
            Freshness::NoLimit
        );
    }

    #[test]
    fn an_unknown_active_engine_falls_back_to_bocha() {
        let (_dir, path) = cfg_text(r#"{"active_engine":"tavily"}"#);
        let c = load_websearch_config_from(&path);
        assert_eq!(c.active_engine, "bocha");
    }

    /// 涉及 `BOCHA_API_KEY` 的测试必须串行：进程环境是全局量。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolved_api_key_reads_the_file_value() {
        let (_dir, path) = cfg_text(r#"{"engines":{"bocha":{"api_key":"from-file"}}}"#);
        let e = load_websearch_config_from(&path)
            .engine("bocha")
            .unwrap()
            .clone();
        assert_eq!(e.resolved_api_key().as_deref(), Some("from-file"));
    }

    /// `BOCHA_API_KEY` 不再是回退层：文件里没有 key 就是没配，哪怕进程环境里有。
    /// 这一条钉住「不再读 env」——`.env` 下线后那个 env 值在重启时必然消失，
    /// 留着回退会造出「当前能用、重启失效」的故障。
    #[test]
    fn resolved_api_key_does_not_fall_back_to_the_environment() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let engine = EngineConfig {
            api_key: String::new(),
            ..EngineConfig::default()
        };
        // env 里有值也不能被采用。
        unsafe { std::env::set_var("BOCHA_API_KEY", "from-env") };
        let resolved = engine.resolved_api_key();
        unsafe { std::env::remove_var("BOCHA_API_KEY") };
        assert_eq!(resolved, None, "the env fallback must be gone");
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
