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
}
