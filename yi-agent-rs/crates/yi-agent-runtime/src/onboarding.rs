//! 首次安装引导的共享逻辑：就绪判定、全局 `.env` 写入、连接测试、结束标记。
//!
//! CLI（TUI 首启问答）与 app-server（桌面向导）共用本模块，避免两处各写一份
//! 判定与写入。模型必需字段的落点统一是全局 `~/.yi-agent/.env`。

use crate::models::{ModelCatalog, effective_entry};

/// 从 `.env` 读出的模型相关字段（空串表示未设置）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvModelFields {
    pub provider: String,
    pub model: String,
    pub api_url: String,
    pub api_key: String,
}

/// 导致「未就绪」的缺失项，用于向导首屏说清到底缺什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Provider,
    Model,
    ApiUrl,
    ApiKey,
}

/// 就绪时，配置来自权威层（清单条目）还是兜底层（`.env`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadySource {
    Catalog,
    Env,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Ready { source: ReadySource },
    Needed { reasons: Vec<Missing> },
}

/// `api_url` 必须是以 `http://` 或 `https://` 开头的绝对地址；空串由调用方
/// 当作「用 provider 默认地址」处理，不走本函数。
pub fn is_valid_api_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 判定当前配置是否足以跑起一次对话。
///
/// 权威层优先：清单（若给了）能解析出默认条目、且该条目 key/model 非空 →
/// `Ready{Catalog}`。否则看兜底层 `.env`：provider 合法、model 非空、api_key
/// 非空、api_url（若给定）格式合法 → `Ready{Env}`；任一不满足 → `Needed` 并
/// 逐个列出原因。CLI 传 `catalog: None`，与「CLI 不读清单」一致。
pub fn assess(env: &EnvModelFields, catalog: Option<&ModelCatalog>) -> Assessment {
    if let Some(cat) = catalog {
        if let Some(entry) = effective_entry(cat, None) {
            if !entry.api_key.trim().is_empty() && !entry.model.trim().is_empty() {
                return Assessment::Ready {
                    source: ReadySource::Catalog,
                };
            }
        }
    }

    let mut reasons = Vec::new();
    if crate::models::ModelProvider::parse(&env.provider).is_none() {
        reasons.push(Missing::Provider);
    }
    if env.model.trim().is_empty() {
        reasons.push(Missing::Model);
    }
    if env.api_key.trim().is_empty() {
        reasons.push(Missing::ApiKey);
    }
    if !env.api_url.trim().is_empty() && !is_valid_api_url(env.api_url.trim()) {
        reasons.push(Missing::ApiUrl);
    }

    if reasons.is_empty() {
        Assessment::Ready {
            source: ReadySource::Env,
        }
    } else {
        Assessment::Needed { reasons }
    }
}

use std::path::Path;

/// 全局 `.env` 里模型相关的四个键。
pub const PROVIDER_KEY: &str = "YI_AGENT_PROVIDER";
pub const MODEL_KEY: &str = "YI_AGENT_MODEL";
pub const API_URL_KEY: &str = "MODEL_API_URL";
pub const API_KEY_KEY: &str = "MODEL_API_KEY";

/// 分组注释：缺失键追加时补上，与 `.env.example` / `yi-agent-web` 的格式一致。
const GROUP_COMMENT: &str = "# === Model Provider ===";

/// 引导收敛出的模型配置（写入 `.env` 的四个键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub api_url: String,
    pub api_key: String,
}

/// 写入前的校验失败。`EmptyField` 携带字段名（`"provider"` / `"model"` / `"api_key"`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    InvalidProvider(String),
    EmptyField(&'static str),
    InvalidApiUrl(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::InvalidProvider(p) => {
                write!(f, "unknown provider: {p}; expected anthropic or openai")
            }
            SettingsError::EmptyField(name) => write!(f, "{name} must not be empty"),
            SettingsError::InvalidApiUrl(u) => {
                write!(f, "api_url must be an absolute http(s) URL: {u}")
            }
        }
    }
}

/// 校验四项是否可写入。`api_url` 允许为空（= 用 provider 默认地址）。
pub fn validate_settings(settings: &ModelSettings) -> Result<(), SettingsError> {
    if crate::models::ModelProvider::parse(&settings.provider).is_none() {
        return Err(SettingsError::InvalidProvider(settings.provider.clone()));
    }
    if settings.model.trim().is_empty() {
        return Err(SettingsError::EmptyField("model"));
    }
    if settings.api_key.trim().is_empty() {
        return Err(SettingsError::EmptyField("api_key"));
    }
    if !settings.api_url.trim().is_empty() && !is_valid_api_url(settings.api_url.trim()) {
        return Err(SettingsError::InvalidApiUrl(settings.api_url.clone()));
    }
    Ok(())
}

/// 从 `.env` 读四个模型键；文件不存在 → 全空（与 `read` 的既有约定一致）。
pub fn read_env_fields(path: &Path) -> std::io::Result<EnvModelFields> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EnvModelFields::default());
        }
        Err(error) => return Err(error),
    };
    let mut fields = EnvModelFields::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let value = strip_quotes(line[eq + 1..].trim());
        match key {
            PROVIDER_KEY => fields.provider = value,
            MODEL_KEY => fields.model = value,
            API_URL_KEY => fields.api_url = value,
            API_KEY_KEY => fields.api_key = value,
            _ => {}
        }
    }
    Ok(fields)
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 行级保留式写入：只就地替换/追加这四个键，其余行（含用户自定义键与注释）
/// 逐字节保留。落盘用「临时文件 + rename」保证原子性，与 `models.json` /
/// `preferences.json` 同一约定。文件含密钥，新建时权限收紧为 `0600`。
pub fn write_model_settings(path: &Path, settings: &ModelSettings) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let mut replaced = [false; 4];
    let targets = [
        (PROVIDER_KEY, settings.provider.as_str()),
        (MODEL_KEY, settings.model.as_str()),
        (API_URL_KEY, settings.api_url.as_str()),
        (API_KEY_KEY, settings.api_key.as_str()),
    ];

    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..eq].trim().to_string();
        let mut replacement: Option<usize> = None;
        for (i, (name, _)) in targets.iter().enumerate() {
            if key == *name {
                replacement = Some(i);
            }
        }
        if let Some(i) = replacement {
            let (name, value) = targets[i];
            *line = format!("{name}={value}");
            replaced[i] = true;
        }
    }

    let missing: Vec<&(&str, &str)> = targets
        .iter()
        .enumerate()
        .filter(|(i, _)| !replaced[*i])
        .map(|(_, t)| t)
        .collect();
    if !missing.is_empty() {
        lines.push(GROUP_COMMENT.to_string());
        for (name, value) in missing {
            lines.push(format!("{name}={value}"));
        }
    }

    let mut output = lines.join("\n");
    output.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-onboarding-{}", std::process::id()));
    std::fs::write(&tmp, output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 新文件收紧到 0600；若目标已存在，rename 前把临时文件的权限对齐它，
        // 避免把用户放宽过的权限悄悄改回 0600。
        let mode = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `preferences.json` 里「引导已结束」的键：完成或用户选「稍后设置」都置位。
pub const DISMISSED_KEY: &str = "onboarding_dismissed";

/// 读结束标记。缺文件 / 不可读 / 损坏一律回 `false`（照常可能弹引导），
/// 坏偏好绝不阻断启动——与 `settings_store` 的既有约定一致。
pub fn load_dismissed(preferences_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(preferences_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .get(DISMISSED_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写结束标记：读-改-写并保留其余键，落盘用「临时文件 + rename」保证原子性。
pub fn save_dismissed(preferences_path: &Path, value: bool) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(preferences_path).unwrap_or_default();
    let mut object = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    object.insert(DISMISSED_KEY.to_string(), serde_json::Value::Bool(value));

    if let Some(parent) = preferences_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = preferences_path.with_extension(format!("tmp-onboarding-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&object)?)?;
    std::fs::rename(&tmp, preferences_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelEntry, ModelProvider};

    fn env(provider: &str, model: &str, api_url: &str, api_key: &str) -> EnvModelFields {
        EnvModelFields {
            provider: provider.into(),
            model: model.into(),
            api_url: api_url.into(),
            api_key: api_key.into(),
        }
    }

    fn catalog_with_default(name: &str, api_key: &str) -> ModelCatalog {
        ModelCatalog {
            models: vec![ModelEntry {
                name: name.into(),
                provider: ModelProvider::Anthropic,
                api_url: "https://a".into(),
                model: "m".into(),
                api_key: api_key.into(),
            }],
            default_model: Some(name.into()),
            subagent_model: None,
        }
    }

    #[test]
    fn env_complete_is_ready_from_env() {
        let a = assess(
            &env("openai", "gpt-4o", "https://api.openai.com", "sk-x"),
            None,
        );
        assert_eq!(
            a,
            Assessment::Ready {
                source: ReadySource::Env
            }
        );
    }

    #[test]
    fn env_without_key_is_needed_listing_the_key() {
        let a = assess(&env("openai", "gpt-4o", "https://api.openai.com", ""), None);
        assert_eq!(
            a,
            Assessment::Needed {
                reasons: vec![Missing::ApiKey]
            }
        );
    }

    #[test]
    fn env_with_empty_defaults_lists_every_missing_field() {
        let a = assess(&env("", "", "", ""), None);
        match a {
            Assessment::Needed { reasons } => {
                assert!(reasons.contains(&Missing::Provider));
                assert!(reasons.contains(&Missing::Model));
                assert!(reasons.contains(&Missing::ApiKey));
            }
            other => panic!("expected Needed, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_api_url_is_reported() {
        let a = assess(&env("openai", "gpt-4o", "not-a-url", "sk-x"), None);
        assert_eq!(
            a,
            Assessment::Needed {
                reasons: vec![Missing::ApiUrl]
            }
        );
    }

    #[test]
    fn an_empty_api_url_is_allowed() {
        let a = assess(&env("openai", "gpt-4o", "", "sk-x"), None);
        assert_eq!(
            a,
            Assessment::Ready {
                source: ReadySource::Env
            }
        );
    }

    #[test]
    fn a_catalog_entry_with_a_key_wins_over_env() {
        let cat = catalog_with_default("A", "sk-cat");
        let a = assess(&env("", "", "", ""), Some(&cat));
        assert_eq!(
            a,
            Assessment::Ready {
                source: ReadySource::Catalog
            }
        );
    }

    #[test]
    fn a_catalog_entry_without_a_key_falls_through_to_env() {
        let cat = catalog_with_default("A", "");
        let a = assess(&env("openai", "gpt-4o", "", "sk-x"), Some(&cat));
        assert_eq!(
            a,
            Assessment::Ready {
                source: ReadySource::Env
            }
        );
    }

    #[test]
    fn catalog_none_is_the_cli_path_and_ignores_the_catalog() {
        // CLI 永远传 None，只按 .env 判定。
        let a = assess(&env("anthropic", "claude-x", "", "sk-x"), None);
        assert_eq!(
            a,
            Assessment::Ready {
                source: ReadySource::Env
            }
        );
    }

    #[test]
    fn api_url_must_be_http_or_https() {
        assert!(is_valid_api_url("https://api.anthropic.com"));
        assert!(is_valid_api_url("http://localhost:8080"));
        assert!(!is_valid_api_url("ftp://x"));
        assert!(!is_valid_api_url("api.anthropic.com"));
    }

    fn settings(provider: &str, model: &str, api_url: &str, api_key: &str) -> ModelSettings {
        ModelSettings {
            provider: provider.into(),
            model: model.into(),
            api_url: api_url.into(),
            api_key: api_key.into(),
        }
    }

    #[test]
    fn read_env_fields_picks_the_four_model_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_URL=https://api.openai.com\nMODEL_API_KEY=sk-x\nBOCHA_API_KEY=keep\n",
        )
        .unwrap();
        let f = read_env_fields(&path).unwrap();
        assert_eq!(f.provider, "openai");
        assert_eq!(f.model, "gpt-4o");
        assert_eq!(f.api_url, "https://api.openai.com");
        assert_eq!(f.api_key, "sk-x");
    }

    #[test]
    fn read_env_fields_on_a_missing_file_is_all_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = read_env_fields(&dir.path().join("nope.env")).unwrap();
        assert_eq!(f, EnvModelFields::default());
    }

    #[test]
    fn write_creates_the_file_with_the_four_keys_and_a_group_comment() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        write_model_settings(&path, &settings("openai", "gpt-4o", "https://u", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(text.contains("YI_AGENT_MODEL=gpt-4o"));
        assert!(text.contains("MODEL_API_URL=https://u"));
        assert!(text.contains("MODEL_API_KEY=sk-k"));
        assert!(text.contains("# === Model Provider ==="));
        let f = read_env_fields(&path).unwrap();
        assert_eq!(f.api_key, "sk-k");
    }

    #[test]
    fn write_updates_existing_keys_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# my notes\nYI_AGENT_PROVIDER=anthropic\nYI_AGENT_MAX_TURNS=500\n",
        )
        .unwrap();
        write_model_settings(&path, &settings("openai", "gpt-4o", "https://u", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(
            text.contains("YI_AGENT_MAX_TURNS=500"),
            "unrelated key must survive"
        );
        assert!(text.contains("# my notes"), "comments must survive");
    }

    #[test]
    fn write_appends_missing_keys_and_preserves_user_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "MY_CUSTOM_KEY=abc\n").unwrap();
        write_model_settings(&path, &settings("anthropic", "claude-x", "", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("MY_CUSTOM_KEY=abc"));
        assert!(text.contains("YI_AGENT_PROVIDER=anthropic"));
        assert!(text.contains("YI_AGENT_MODEL=claude-x"));
        assert!(text.contains("MODEL_API_KEY=sk-k"));
    }

    #[test]
    fn write_does_not_duplicate_an_existing_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "YI_AGENT_MODEL=old\n").unwrap();
        write_model_settings(&path, &settings("openai", "new", "", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("YI_AGENT_MODEL=").count(), 1);
        assert!(text.contains("YI_AGENT_MODEL=new"));
    }

    #[cfg(unix)]
    #[test]
    fn write_uses_0600_for_a_new_file_and_keeps_an_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();

        // 新建：文件含密钥，权限必须收紧到 0600。
        let fresh = dir.path().join("fresh.env");
        write_model_settings(&fresh, &settings("openai", "gpt-4o", "https://u", "sk-k")).unwrap();
        let fresh_mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(fresh_mode, 0o600, "a newly written .env must be 0600");

        // 已存在：用户放宽过的权限必须原样保留，不能被悄悄改回 0600。
        let existing = dir.path().join("existing.env");
        std::fs::write(&existing, "MY_CUSTOM_KEY=abc\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_model_settings(
            &existing,
            &settings("openai", "gpt-4o", "https://u", "sk-k"),
        )
        .unwrap();
        let existing_mode = std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777;
        assert_eq!(existing_mode, 0o644, "an existing .env must keep its mode");
    }

    #[test]
    fn validate_rejects_a_bad_provider_and_a_bad_url() {
        assert!(matches!(
            validate_settings(&settings("gemini", "m", "", "k")),
            Err(SettingsError::InvalidProvider(_))
        ));
        assert!(matches!(
            validate_settings(&settings("openai", "m", "ftp://x", "k")),
            Err(SettingsError::InvalidApiUrl(_))
        ));
        assert!(matches!(
            validate_settings(&settings("openai", "  ", "", "k")),
            Err(SettingsError::EmptyField("model"))
        ));
        assert!(validate_settings(&settings("openai", "m", "", "k")).is_ok());
    }

    #[test]
    fn missing_preferences_means_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!load_dismissed(&dir.path().join("preferences.json")));
    }

    #[test]
    fn a_corrupt_preferences_file_reads_as_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!load_dismissed(&path));
    }

    #[test]
    fn save_then_load_round_trips_and_preserves_other_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"theme":"light"}"#).unwrap();
        save_dismissed(&path, true).unwrap();
        assert!(load_dismissed(&path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"theme\""),
            "other keys must survive: {text}"
        );
        save_dismissed(&path, false).unwrap();
        assert!(!load_dismissed(&path));
    }
}
