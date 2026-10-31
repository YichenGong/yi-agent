//! 首次安装引导的共享逻辑：就绪判定、连接测试、结束标记。
//!
//! CLI（TUI 首启问答）与 app-server（桌面向导）共用本模块，避免两处各写一份
//! 判定与写入。**模型配置的唯一真相源是 `models.json`**——过去还有一层「读
//! `.env` 兜底」，那条路径已随后者的下线一并删除：留着它只会让 `.env` 改名后
//! 判定永远为「未就绪」。

use crate::models::{ModelCatalog, effective_entry};

/// 导致「未就绪」的缺失项，用于向导首屏说清到底缺什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Provider,
    Model,
    ApiUrl,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Ready,
    Needed { reasons: Vec<Missing> },
}

/// `api_url` 必须是以 `http://` 或 `https://` 开头的绝对地址；空串由调用方
/// 当作「用 provider 默认地址」处理，不走本函数。
pub fn is_valid_api_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 判定当前配置是否足以跑起一次对话。
///
/// 唯一判据是清单的默认条目：能解析出条目、且 `api_key` 与 `model` 都非空 →
/// `Ready`；否则 `Needed` 并逐个列出该条目缺哪几项。
///
/// `catalog: None` 表示调用方不读清单（CLI 的某些路径），等价于「空清单」。
/// 原因列表与清单状态同源——过去 `.env` 兜底层会让「只缺 key」被报成「四项
/// 全缺」，因为兜底层一空就什么都不知道了。
pub fn assess(catalog: Option<&ModelCatalog>) -> Assessment {
    let entry = catalog.and_then(|catalog| effective_entry(catalog, None));
    let mut reasons = Vec::new();
    match entry {
        Some(entry) => {
            if crate::models::ModelProvider::parse(entry.provider.as_str()).is_none() {
                reasons.push(Missing::Provider);
            }
            if entry.model.trim().is_empty() {
                reasons.push(Missing::Model);
            }
            if entry.api_key.trim().is_empty() {
                reasons.push(Missing::ApiKey);
            }
            if !entry.api_url.trim().is_empty() && !is_valid_api_url(entry.api_url.trim()) {
                reasons.push(Missing::ApiUrl);
            }
        }
        None => {
            // 没有默认条目 = 什么都缺。
            reasons.extend([Missing::Provider, Missing::Model, Missing::ApiKey]);
        }
    }
    if reasons.is_empty() {
        Assessment::Ready
    } else {
        Assessment::Needed { reasons }
    }
}

use std::path::Path;

/// 引导收敛出的模型配置（写入 `models.json` 的一条条目）。
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
    use crate::models::{ModelCatalog, ModelEntry, ModelProvider};

    fn catalog_with(api_key: &str, model: &str) -> ModelCatalog {
        let mut catalog = ModelCatalog::default();
        catalog.models.push(ModelEntry {
            name: "entry".to_string(),
            provider: ModelProvider::Openai,
            api_url: "https://api.openai.com/v1".to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        });
        catalog.default_model = Some("entry".to_string());
        catalog
    }

    #[test]
    fn a_usable_default_entry_is_ready() {
        let catalog = catalog_with("sk-x", "gpt-4o");
        assert_eq!(assess(Some(&catalog)), Assessment::Ready);
    }

    #[test]
    fn a_default_entry_without_a_key_is_needed_with_only_the_key_missing() {
        let catalog = catalog_with("", "gpt-4o");
        assert_eq!(
            assess(Some(&catalog)),
            Assessment::Needed {
                reasons: vec![Missing::ApiKey]
            },
            "a catalog-only machine must report exactly what is missing, not all four"
        );
    }

    #[test]
    fn a_default_entry_without_a_model_is_needed_with_only_the_model_missing() {
        let catalog = catalog_with("sk-x", "");
        assert_eq!(
            assess(Some(&catalog)),
            Assessment::Needed {
                reasons: vec![Missing::Model]
            }
        );
    }

    #[test]
    fn an_empty_catalog_is_needed() {
        assert_eq!(
            assess(Some(&ModelCatalog::default())),
            Assessment::Needed {
                reasons: vec![Missing::Provider, Missing::Model, Missing::ApiKey]
            }
        );
    }

    #[test]
    fn no_catalog_is_needed() {
        // CLI 传 `None` 表示「不读清单」，此时无任何可用配置。
        assert_eq!(
            assess(None),
            Assessment::Needed {
                reasons: vec![Missing::Provider, Missing::Model, Missing::ApiKey]
            }
        );
    }

    /// 钉住「`.env` 兜底层已删」：把一份写满四个模型键的 `.env` 放在实例目录里，
    /// 判定依然只认清单。将来有人把 Env 分支加回来，这条会红。
    #[test]
    fn an_env_file_with_a_full_model_config_does_not_make_it_ready() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".env"),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\nMODEL_API_URL=https://api.openai.com/v1\n",
        )
        .unwrap();
        std::fs::write(home.join(".env.migrated"), "YI_AGENT_MODEL=gpt-4o\n").unwrap();

        // 清单为空 → 未就绪，与 .env 里写了什么无关。
        let catalog = ModelCatalog::default();
        assert!(matches!(assess(Some(&catalog)), Assessment::Needed { .. }));
    }

    #[test]
    fn is_valid_api_url_accepts_http_and_https() {
        assert!(is_valid_api_url("https://api.example.com"));
        assert!(is_valid_api_url("http://localhost:8080/v1"));
        assert!(!is_valid_api_url("ftp://example.com"));
        assert!(!is_valid_api_url("api.example.com"));
    }

    #[test]
    fn dismissed_flag_round_trips_and_preserves_other_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"theme":"light"}"#).unwrap();

        assert!(!load_dismissed(&path));
        save_dismissed(&path, true).unwrap();
        assert!(load_dismissed(&path));

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "light", "other keys must survive");
    }

    #[test]
    fn a_corrupt_preferences_file_is_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(!load_dismissed(&path));
    }
}
