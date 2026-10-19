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

/// 引导收敛出的模型配置（写入 `.env` 的四个键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub api_url: String,
    pub api_key: String,
}

impl ModelSettings {
    /// 把「空 `api_url`」解析成该 provider 的默认地址。
    ///
    /// 探测与落盘必须共用这一份解析：否则用户留空时，探测打的是空串、
    /// 落盘写的是默认地址，两条路径验证的根本不是同一个配置。
    pub fn with_default_url(&self) -> Self {
        let provider = crate::models::ModelProvider::parse(&self.provider);
        let default_url = match provider {
            Some(crate::models::ModelProvider::Openai) => "https://api.openai.com/v1",
            Some(crate::models::ModelProvider::Anthropic) => "https://api.anthropic.com",
            // provider 非法时留给 `validate_settings` 报错，这里不动。
            None => "",
        };
        let api_url = if self.api_url.trim().is_empty() {
            default_url.to_string()
        } else {
            self.api_url.trim().to_string()
        };
        Self {
            provider: self.provider.clone(),
            model: self.model.trim().to_string(),
            api_url,
            api_key: self.api_key.clone(),
        }
    }
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

/// `preferences.json` 里「引导已结束」的键：完成或用户选「稍后设置」都置位。
pub const DISMISSED_KEY: &str = "onboarding_dismissed";

/// 把一组模型设置落成清单里的一条并设为默认。重名自动加 `-2`/`-3` 后缀。
///
/// CLI 的终端引导与 app-server 的 `onboarding/apply` 共用它，保证两条写路径同源。
pub fn import_settings_into_catalog(
    models_path: &Path,
    settings: &ModelSettings,
) -> Result<String, String> {
    let Some(provider) = crate::models::ModelProvider::parse(&settings.provider) else {
        return Err(format!("unknown provider: {}", settings.provider));
    };
    // 空 `api_url` 视为「用 provider 默认地址」：清单条目要求 api_url 非空，
    // 空串会在读回时被 `parse_catalog` 丢弃（与 `migrate.rs` 同口径）。
    // 解析收敛在 `with_default_url`，保证探测与落盘同源。
    let api_url = settings.with_default_url().api_url;
    let mut catalog = crate::models::load_catalog_from(models_path);
    let base = settings.model.trim().to_string();
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(crate::models::ModelEntry {
        name: name.clone(),
        provider,
        api_url,
        model: settings.model.trim().to_string(),
        api_key: settings.api_key.clone(),
        // 引导固化的是四个字段，通道两项不在此声明 → 保持默认。
        api_style: None,
        reasoning_effort: None,
        reasoning_capabilities: None,
        context_length: None,
    });
    catalog.default_model = Some(name.clone());
    crate::models::save_catalog_to(models_path, &catalog).map_err(|e| e.to_string())?;
    Ok(name)
}

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

/// 一次连接测试的结论。`reason` 是给人看的中文；成功时为 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOutcome {
    pub ok: bool,
    pub reason: Option<String>,
}

/// 用一份临时 provider 发一次最小请求，验证 key/url/model 是否可用。
///
/// **只探测，不写任何文件**。明文 key 只在此处进请求头，绝不进日志或返回值。
pub async fn test_connection(settings: &ModelSettings) -> ConnectionOutcome {
    use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};

    // 与落盘同一份解析：用户留空 URL 时探测的必须是真正会写进清单的地址。
    let settings = settings.with_default_url();

    let provider: std::sync::Arc<dyn Provider> = match settings.provider.as_str() {
        "openai" => match yi_agent_llm::OpenaiProvider::new(yi_agent_llm::OpenaiProviderOpts {
            base_url: Some(settings.api_url.clone()),
            api_key: Some(settings.api_key.clone()),
            // 探测只等一小会儿（spec §5 建议 ~15s）：默认 300s 会让「地址写错」
            // 看起来像界面卡死。
            timeout: Some(std::time::Duration::from_secs(15)),
            // 探测不携带 api_style/reasoning_effort：连接验证只需 key/url/model。
            ..Default::default()
        }) {
            Ok(p) => std::sync::Arc::new(p),
            Err(error) => return failure(error),
        },
        _ => match yi_agent_llm::AnthropicProvider::new(yi_agent_llm::AnthropicProviderOpts {
            base_url: Some(settings.api_url.clone()),
            api_key: Some(settings.api_key.clone()),
            // 同 openai 分支：探测的等待上限同为 15s。
            timeout: Some(std::time::Duration::from_secs(15)),
            ..Default::default()
        }) {
            Ok(p) => std::sync::Arc::new(p),
            Err(error) => return failure(error),
        },
    };

    let request = ProviderRequest {
        model: settings.model.clone(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: Vec::new(),
        params: GenParams {
            max_tokens: Some(1),
            ..Default::default()
        },
    };

    match provider.call(request).await {
        Ok(_) => ConnectionOutcome {
            ok: true,
            reason: None,
        },
        Err(error) => failure(error),
    }
}

/// 把 provider 错误翻成一句可读中文。明文响应体只用于「其它」分支的粗粒度提示，
/// 且经过截断——绝不外泄给界面之外。
fn failure(error: yi_agent_core::ProviderError) -> ConnectionOutcome {
    use yi_agent_core::ProviderError;
    let reason = match error {
        ProviderError::Auth(_) => "API 密钥无效或无权限".to_string(),
        ProviderError::InvalidRequest(_) => "模型标识不被该地址接受".to_string(),
        ProviderError::RateLimited => "请求过于频繁，请稍后重试".to_string(),
        ProviderError::Network(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("timed out") || lower.contains("timeout") {
                "连接超时".to_string()
            } else {
                "无法连接 API 地址".to_string()
            }
        }
        ProviderError::Server(message) | ProviderError::Stream(message) => {
            format!("服务端错误：{}", truncate(&message, 120))
        }
    };
    ConnectionOutcome {
        ok: false,
        reason: Some(reason),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
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
                api_style: None,
                reasoning_effort: None,
                reasoning_capabilities: None,
                context_length: None,
            }],
            default_model: Some(name.into()),
            subagent_model: None,
        }
    }

    #[test]
    fn an_empty_url_resolves_to_the_provider_default() {
        let openai = ModelSettings {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_url: String::new(),
            api_key: "sk-x".into(),
        };
        assert_eq!(
            openai.with_default_url().api_url,
            "https://api.openai.com/v1"
        );

        let anthropic = ModelSettings {
            provider: "anthropic".into(),
            model: "m".into(),
            api_url: "  ".into(),
            api_key: "sk-x".into(),
        };
        assert_eq!(
            anthropic.with_default_url().api_url,
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn an_explicit_url_is_kept_and_trimmed() {
        let settings = ModelSettings {
            provider: "openai".into(),
            model: "m".into(),
            api_url: "  https://gw.example/v1  ".into(),
            api_key: "k".into(),
        };
        assert_eq!(settings.with_default_url().api_url, "https://gw.example/v1");
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
    fn importing_settings_suffixes_on_a_name_collision() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let settings = ModelSettings {
            provider: "openai".into(),
            model: "dup".into(),
            api_url: "https://gw/v1".into(),
            api_key: "sk-a".into(),
        };
        assert_eq!(
            import_settings_into_catalog(&models, &settings).unwrap(),
            "dup"
        );
        assert_eq!(
            import_settings_into_catalog(&models, &settings).unwrap(),
            "dup-2"
        );
        // 冲突条目必须真的落盘（两条重名条目 + 默认指向最新）。
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.default_model.as_deref(), Some("dup-2"));
    }

    /// 探测与落盘必须打出同一个地址：用户留空 URL 时，若两条路径各解析一次，
    /// 很容易一条用空串、一条用默认值——于是"测试通过"的配置落盘后却不可用。
    #[test]
    fn importing_writes_the_same_url_the_probe_would_use() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let settings = ModelSettings {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_url: String::new(),
            api_key: "sk-a".into(),
        };
        import_settings_into_catalog(&models, &settings).unwrap();
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(
            catalog.models[0].api_url,
            settings.with_default_url().api_url
        );
        assert_eq!(catalog.models[0].api_url, "https://api.openai.com/v1");
    }

    #[test]
    fn importing_rejects_an_unknown_provider() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = ModelSettings {
            provider: "bogus".into(),
            model: "m".into(),
            api_url: "https://x".into(),
            api_key: "k".into(),
        };
        assert!(import_settings_into_catalog(&dir.path().join("models.json"), &settings).is_err());
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

    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Anthropic 的成功响应走 SSE 流。一段只有 `message_start` / `message_stop`
    /// 的最小事件流足以让 `provider.call()` 正常收流；用 JSON body 会因缺少终止
    /// 事件而撞上空闲超时。
    const SSE_OK: &str = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn a_working_endpoint_reports_ok() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_OK),
            )
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "m", &server.uri(), "sk-x")).await;
        assert!(outcome.ok, "reason: {:?}", outcome.reason);
    }

    #[tokio::test]
    async fn a_401_reads_as_an_invalid_key() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "m", &server.uri(), "bad")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("API 密钥无效或无权限"));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_reads_as_cannot_connect() {
        // 端口 1 上必然连不上。
        let outcome =
            test_connection(&settings("anthropic", "m", "http://127.0.0.1:1", "sk-x")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("无法连接 API 地址"));
    }

    #[tokio::test]
    async fn a_400_reads_as_a_rejected_model() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad model"))
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "nope", &server.uri(), "sk-x")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("模型标识不被该地址接受"));
    }
}
