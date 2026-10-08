//! 机器级模型清单：`~/.yi-agent/models.json`。
//!
//! 每条模型自带 provider 格式 / api_url / api key，密钥跟随 url 走。该文件是
//! **机器级全局**的（与 `~/.yi-agent/.env` 同目录），跨项目、跨重启共享。
//! 未配置时为空清单，调用方回退 `.env`/cfg，行为与今天一致。
//!
//! 落盘约定与 `yi-agent-app-server/src/settings_store.rs` 一致：读-改-写、
//! 临时名逐次唯一、`rename` 原子替换；文件含密钥，权限收紧为 `0600`。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::RuntimeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    Anthropic,
    Openai,
}

impl ModelProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelProvider::Anthropic => "anthropic",
            ModelProvider::Openai => "openai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(ModelProvider::Anthropic),
            "openai" => Some(ModelProvider::Openai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(deserialize_with = "de_provider")]
    pub provider: ModelProvider,
    pub api_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

/// 反序列化时把非法 provider 变成错误，让整条被跳过（见 `load_catalog_from` 的逐条过滤）。
fn de_provider<'de, D>(deserializer: D) -> Result<ModelProvider, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    ModelProvider::parse(&s).ok_or_else(|| serde::de::Error::custom("unknown provider"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub subagent_model: Option<String>,
}

/// `<HOME>/.yi-agent/models.json`；HOME 缺失回退当前目录，与 `resolve_global_env_path` 同口径。
pub fn models_path() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yi-agent")
        .join("models.json")
}

/// 读清单。缺失 / 不可读 / 不可解析 → 空清单（坏文件绝不阻断启动）。
/// 逐条过滤：字段非法（空显示名、未知 provider、空 url/model）的条目被跳过。
pub fn load_catalog() -> ModelCatalog {
    load_catalog_from(&models_path())
}

pub fn load_catalog_from(path: &Path) -> ModelCatalog {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelCatalog::default();
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not read models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "invalid models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    let mut catalog = ModelCatalog::default();
    if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
        for raw in models {
            match serde_json::from_value::<ModelEntry>(raw.clone()) {
                Ok(entry)
                    if !entry.name.trim().is_empty()
                        && !entry.api_url.trim().is_empty()
                        && !entry.model.trim().is_empty() =>
                {
                    catalog.models.push(entry);
                }
                Ok(_) => tracing::warn!(path = %path.display(), "skipping incomplete model entry"),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping malformed model entry")
                }
            }
        }
    }
    catalog.default_model = value
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog.subagent_model = value
        .get("subagent_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog
}

pub fn save_catalog(catalog: &ModelCatalog) -> std::io::Result<()> {
    save_catalog_to(&models_path(), catalog)
}

/// 写清单：读-改-写，保留 `models.json` 里无关的顶层键，最后原子替换。
pub fn save_catalog_to(path: &Path, catalog: &ModelCatalog) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 读-改-写：以既有对象为底，仅覆盖本模块拥有的三个键，其余顶层键原样保留。
    // 文件缺失 / 损坏 / 非对象时从空对象起步（与 `load_catalog_from` 的容错一致）。
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let catalog_value = serde_json::to_value(catalog).map_err(std::io::Error::other)?;
    if let serde_json::Value::Object(fields) = catalog_value {
        for (key, value) in fields {
            object.insert(key, value);
        }
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!("models.json.{}.{}.tmp", std::process::id(), seq));
    write_owner_only(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    // rename 保留 tmp 的权限位；显式再设一次以防既有文件权限更宽。
    set_owner_only(path)?;
    Ok(())
}

fn write_owner_only(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

fn set_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 密钥掩码：只留尾 4 位，其余用 `••••`；长度 ≤4 时全部掩掉。
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// 会话覆盖 → 全局默认 → 原 cfg 三级解析，返回改写了 provider/api_url/api_key/model
/// 四个字段的 cfg 克隆，其余字段保持原值。
pub fn resolve_effective(
    cfg: &RuntimeConfig,
    catalog: &ModelCatalog,
    session_override: Option<&str>,
) -> RuntimeConfig {
    match effective_entry(catalog, session_override) {
        Some(entry) => {
            let mut out = cfg.clone();
            out.provider = entry.provider.as_str().to_string();
            out.api_url = entry.api_url.clone();
            out.api_key = entry.api_key.clone();
            out.model = entry.model.clone();
            out
        }
        None => cfg.clone(),
    }
}

/// 会话覆盖命中则用它；否则全局默认；都命中不了返回 None（调用方回退 cfg）。
pub fn effective_entry<'a>(
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    session_override
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}

/// 子 agent 条目：`subagent_model` → 全局默认 → None（调用方回退 cfg）。
pub fn subagent_entry(catalog: &ModelCatalog) -> Option<&ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    catalog
        .subagent_model
        .as_deref()
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            provider: ModelProvider::Anthropic,
            api_url: "https://x".into(),
            model: "claude-x".into(),
            api_key: "sk-secret-1234".into(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let catalog = ModelCatalog {
            models: vec![entry("a")],
            default_model: Some("a".into()),
            subagent_model: None,
        };
        save_catalog_to(&path, &catalog).unwrap();
        assert_eq!(load_catalog_from(&path), catalog);
    }

    #[test]
    fn a_missing_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn a_broken_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn malformed_entries_are_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"ok","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"bad-provider","provider":"gemini","api_url":"u","model":"m","api_key":"k"}
            ],"default_model":"ok"}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].name, "ok");
        assert_eq!(catalog.default_model.as_deref(), Some("ok"));
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "models.json carries API keys and must be 0600");
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "models.json")
            .collect();
        assert!(
            stray.is_empty(),
            "no temp file may survive a save: {stray:?}"
        );
    }

    #[test]
    fn saving_preserves_unrelated_top_level_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"future_key":42}"#).unwrap();
        let catalog = ModelCatalog {
            models: vec![entry("a")],
            default_model: Some("a".into()),
            subagent_model: None,
        };
        save_catalog_to(&path, &catalog).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw.get("future_key"), Some(&serde_json::json!(42)));
        assert_eq!(load_catalog_from(&path), catalog);
    }

    #[test]
    fn masks_the_key_leaving_the_tail() {
        assert_eq!(mask_key("sk-secret-1234"), "••••1234");
        assert_eq!(mask_key("ab"), "••••");
        assert_eq!(mask_key(""), "••••");
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::config::RuntimeConfig;

    /// `config.rs` exposes no reusable test constructor, so build a complete
    /// literal here. The four fields the resolver rewrites are deliberately set
    /// to recognizable "base" values so the tests can tell them apart.
    fn cfg() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".into(),
            api_url: "https://base".into(),
            api_key: "base-key".into(),
            model: "base-model".into(),
            max_turns: 200,
            max_resident_subagents: 64,
            max_direct_children: crate::config::DIRECT_CHILDREN_DEFAULT,
            workdir: std::path::PathBuf::from("."),
            system_prompt: None,
            compact_threshold: 1000,
            compact_user_budget_tokens: 100,
            compact_tool_budget_tokens: 100,
            yolo: false,
            sandbox_promotable: false,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 0,
            skills_catalog_budget_explicit: false,
        }
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog {
            models: vec![
                ModelEntry {
                    name: "A".into(),
                    provider: ModelProvider::Anthropic,
                    api_url: "https://a".into(),
                    model: "model-a".into(),
                    api_key: "key-a".into(),
                },
                ModelEntry {
                    name: "B".into(),
                    provider: ModelProvider::Openai,
                    api_url: "https://b".into(),
                    model: "model-b".into(),
                    api_key: "key-b".into(),
                },
            ],
            default_model: Some("A".into()),
            subagent_model: None,
        }
    }

    #[test]
    fn a_session_override_wins() {
        let out = resolve_effective(&cfg(), &catalog(), Some("B"));
        assert_eq!(out.provider, "openai");
        assert_eq!(out.api_url, "https://b");
        assert_eq!(out.model, "model-b");
        assert_eq!(out.api_key, "key-b");
    }

    #[test]
    fn a_dangling_override_falls_back_to_the_default() {
        let out = resolve_effective(&cfg(), &catalog(), Some("gone"));
        assert_eq!(out.model, "model-a");
    }

    #[test]
    fn no_default_falls_back_to_cfg() {
        let base = cfg();
        let empty = ModelCatalog::default();
        let out = resolve_effective(&base, &empty, None);
        assert_eq!(out.model, base.model);
        assert_eq!(out.api_url, base.api_url);
    }

    #[test]
    fn only_the_four_fields_change() {
        let base = cfg();
        let out = resolve_effective(&base, &catalog(), Some("B"));
        assert_eq!(out.workdir, base.workdir);
        assert_eq!(out.sandbox, base.sandbox);
        assert_eq!(out.max_turns, base.max_turns);
    }

    #[test]
    fn subagent_falls_back_to_default_then_cfg() {
        let cat = catalog();
        assert_eq!(subagent_entry(&cat).map(|e| e.name.as_str()), Some("A"));
        let mut cat2 = cat.clone();
        cat2.subagent_model = Some("B".into());
        assert_eq!(subagent_entry(&cat2).map(|e| e.name.as_str()), Some("B"));
    }
}
