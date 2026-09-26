//! Agent 装配:provider 构造、工具注册、权限通道。

use std::sync::Arc;

use anyhow::Result;

use crate::config::RuntimeConfig;

/// 根据配置构造 LLM provider。
pub fn build_provider(cfg: &RuntimeConfig) -> Result<Arc<dyn yi_agent_core::Provider>> {
    match cfg.provider.as_str() {
        "anthropic" => Ok(Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(cfg.api_url.clone()),
                api_key: Some(cfg.api_key.clone()),
                ..Default::default()
            },
        )?)),
        "openai" => Ok(Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(cfg.api_url.clone()),
                api_key: Some(cfg.api_key.clone()),
                ..Default::default()
            },
        )?)),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sample_config;

    #[test]
    fn build_provider_rejects_unknown_name() {
        let mut cfg = sample_config();
        cfg.provider = "gemini".into();
        // `Arc<dyn Provider>` 不是 Debug,不能用 unwrap_err();改用 match 取错误。
        let err = match build_provider(&cfg) {
            Ok(_) => panic!("unknown provider should be rejected"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("unknown provider"));
    }

    #[test]
    fn build_provider_accepts_anthropic_and_openai() {
        for name in ["anthropic", "openai"] {
            let mut cfg = sample_config();
            cfg.provider = name.into();
            assert!(build_provider(&cfg).is_ok(), "provider {name} should build");
        }
    }
}
