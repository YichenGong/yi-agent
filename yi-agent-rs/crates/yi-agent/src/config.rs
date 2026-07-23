//! 配置加载：环境变量 + CLI 参数 > 默认值。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 运行时配置，由 CLI 参数和环境变量合并而来。
#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
}

/// clap CLI 参数定义。
#[derive(clap::Parser, Debug)]
#[command(name = "yi-agent", version, about = "Interactive AI agent CLI")]
pub struct Cli {
    /// API endpoint URL (overrides MODEL_API_URL)
    #[arg(long)]
    pub api_url: Option<String>,

    /// API key (overrides MODEL_API_KEY)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Model to use
    #[arg(long)]
    pub model: Option<String>,

    /// Max agent turns per conversation
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// Working directory for file system tools
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Custom system prompt
    #[arg(long)]
    pub system_prompt: Option<String>,
}

/// 从 CLI 参数 + 环境变量加载配置。
///
/// 优先级：CLI 参数 > 环境变量 > 默认值。
pub fn load(cli: &Cli) -> Result<Config> {
    let api_key = cli
        .api_key
        .clone()
        .or_else(|| std::env::var("MODEL_API_KEY").ok())
        .context("API key required: set MODEL_API_KEY or use --api-key")?;
    if api_key.is_empty() {
        bail!("API key is empty: set MODEL_API_KEY or use --api-key");
    }

    let api_url = cli
        .api_url
        .clone()
        .or_else(|| std::env::var("MODEL_API_URL").ok())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());

    let model = cli
        .model
        .clone()
        .or_else(|| std::env::var("YI_AGENT_MODEL").ok())
        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());

    let max_turns = cli
        .max_turns
        .or_else(|| {
            std::env::var("YI_AGENT_MAX_TURNS")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(20);

    let workdir = cli
        .workdir
        .clone()
        .or_else(|| {
            std::env::var("YI_AGENT_WORKDIR")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // 验证工作目录存在
    if !Path::new(&workdir).is_dir() {
        bail!("working directory does not exist: {}", workdir.display());
    }

    let system_prompt = cli
        .system_prompt
        .clone()
        .or_else(|| std::env::var("YI_AGENT_SYSTEM_PROMPT").ok())
        .filter(|s| !s.is_empty());

    Ok(Config {
        api_url,
        api_key,
        model,
        max_turns,
        workdir,
        system_prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_requires_api_key() {
        // 清除环境变量确保测试隔离（Rust 2024: remove_var is unsafe）
        unsafe {
            std::env::remove_var("MODEL_API_KEY");
            std::env::remove_var("MODEL_API_URL");
        }
        let cli = Cli {
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
        };
        let result = load(&cli);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("API key"),
            "error should mention API key, got: {msg}"
        );
    }

    #[test]
    fn load_loads_from_cli_args() {
        let cli = Cli {
            api_url: Some("https://example.com".into()),
            api_key: Some("test-key".into()),
            model: Some("test-model".into()),
            max_turns: Some(5),
            workdir: Some(PathBuf::from(".")),
            system_prompt: Some("custom prompt".into()),
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.api_url, "https://example.com");
        assert_eq!(config.api_key, "test-key");
        assert_eq!(config.model, "test-model");
        assert_eq!(config.max_turns, 5);
        assert_eq!(config.system_prompt.as_deref(), Some("custom prompt"));
    }

    #[test]
    fn load_defaults_api_url_and_model() {
        let cli = Cli {
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.api_url, "https://api.anthropic.com");
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.max_turns, 20);
        assert!(config.system_prompt.is_none());
    }

    #[test]
    fn load_rejects_nonexistent_workdir() {
        let cli = Cli {
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from("/nonexistent/path/that/should/not/exist")),
            system_prompt: None,
        };
        let result = load(&cli);
        assert!(result.is_err());
    }
}
