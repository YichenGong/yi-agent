//! 共享 helper:供 e2e_real.rs 和 e2e_complex.rs 复用。
//!
//! 每个测试二进制独立编译本模块,未被该二进制使用的 helper 会触发 dead_code。
//! 加 module-level allow 避免每个函数单独标注。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::Duration;

/// 复杂测试超时上限(秒)。agent 挂起时强制 kill,避免测试无限阻塞。
const COMPLEX_TIMEOUT: Duration = Duration::from_secs(300);

pub struct RealLlmTestConfig {
    pub provider: String,
    pub api_url: String,
    pub model: String,
    api_key: String,
}

impl RealLlmTestConfig {
    pub fn apply_to_command(&self, command: &mut Command) {
        command
            .env("YI_AGENT_PROVIDER", &self.provider)
            .env("MODEL_API_URL", &self.api_url)
            .env("YI_AGENT_MODEL", &self.model)
            .env("MODEL_API_KEY", &self.api_key);
    }
}

pub fn resolve_real_llm_test_config() -> Result<Option<RealLlmTestConfig>, String> {
    let dedicated = [
        (
            "YI_AGENT_REAL_LLM_PROVIDER",
            std::env::var("YI_AGENT_REAL_LLM_PROVIDER").ok(),
        ),
        (
            "YI_AGENT_REAL_LLM_API_URL",
            std::env::var("YI_AGENT_REAL_LLM_API_URL").ok(),
        ),
        (
            "YI_AGENT_REAL_LLM_MODEL",
            std::env::var("YI_AGENT_REAL_LLM_MODEL").ok(),
        ),
        (
            "YI_AGENT_REAL_LLM_API_KEY",
            std::env::var("YI_AGENT_REAL_LLM_API_KEY").ok(),
        ),
    ];
    if let Some(provider) = dedicated[0]
        .1
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let missing = dedicated
            .iter()
            .filter(|(_, value)| value.as_deref().is_none_or(|value| value.trim().is_empty()))
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "incomplete explicit real LLM configuration: missing {}",
                missing.join(", ")
            ));
        }
        if !matches!(provider, "anthropic" | "openai") {
            return Err("YI_AGENT_REAL_LLM_PROVIDER must be anthropic or openai".into());
        }
        let api_url = dedicated[1].1.clone().unwrap();
        if !(api_url.starts_with("https://") || api_url.starts_with("http://")) {
            return Err("YI_AGENT_REAL_LLM_API_URL must be an absolute HTTP(S) URL".into());
        }
        return Ok(Some(RealLlmTestConfig {
            provider: provider.into(),
            api_url,
            model: dedicated[2].1.clone().unwrap(),
            api_key: dedicated[3].1.clone().unwrap(),
        }));
    }
    if let Some(api_key) = std::env::var("MODEL_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        let provider = std::env::var("YI_AGENT_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "anthropic".into());
        if !matches!(provider.as_str(), "anthropic" | "openai") {
            return Err("YI_AGENT_PROVIDER must be anthropic or openai".into());
        }
        let (default_api_url, default_model) = if provider == "anthropic" {
            ("https://api.anthropic.com", "claude-sonnet-4-20250514")
        } else {
            ("https://api.openai.com", "gpt-4o")
        };
        return Ok(Some(RealLlmTestConfig {
            provider,
            api_url: std::env::var("MODEL_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| default_api_url.into()),
            model: std::env::var("YI_AGENT_MODEL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| default_model.into()),
            api_key,
        }));
    }

    let (provider, api_key) = if let Some(key) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
    {
        ("anthropic", key)
    } else if let Some(key) = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
    {
        ("openai", key)
    } else {
        return Ok(None);
    };
    let (api_url, model) = if provider == "anthropic" {
        ("https://api.anthropic.com", "claude-sonnet-4-20250514")
    } else {
        ("https://api.openai.com", "gpt-4o")
    };
    Ok(Some(RealLlmTestConfig {
        provider: provider.into(),
        api_url: api_url.into(),
        model: model.into(),
        api_key,
    }))
}

/// Path to the compiled yi-agent binary.
pub fn yi_agent_bin() -> PathBuf {
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

/// 检查 headless CLI 所需的 API key 配置。
pub fn has_api_key() -> bool {
    std::env::var("MODEL_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 无 key 时打印 skip 并返回 false。
pub fn skip_if_no_key() -> bool {
    if !has_api_key() {
        eprintln!("SKIPPED: no headless configuration (MODEL_API_KEY)");
        false
    } else {
        true
    }
}

/// 返回可用于传给 yi-agent 的 --api-key 值。
pub fn resolve_api_key() -> Option<String> {
    std::env::var("MODEL_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// 从一行 JSONL 提取 AgentEvent 的 variant 名。
/// serde 对 unit variant(如 Start, Cancelled)序列化为裸字符串 `"Start"`,
/// 对其他 variant 序列化为 `{"VariantName": ...}` 对象。
pub fn event_variant(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(obj) = v.as_object() {
        return obj.keys().next().cloned();
    }
    None
}

/// 解析 JSONL 为 Vec<serde_json::Value>。
pub fn parse_events(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL line"))
        .collect()
}

/// 检查事件流中是否有 Done 事件。
pub fn has_done_event(events: &[serde_json::Value]) -> bool {
    events.iter().any(|v| {
        v.as_str() == Some("Done")
            || v.as_object()
                .map(|o| o.contains_key("Done"))
                .unwrap_or(false)
    })
}

pub fn has_normal_end_turn(events: &[serde_json::Value]) -> bool {
    events
        .iter()
        .any(|event| event.pointer("/Done/reason") == Some(&serde_json::json!("EndTurn")))
}

fn wait_for_child(mut child: Child, timeout: Duration) -> Result<Output, String> {
    let started = std::time::Instant::now();

    loop {
        if child
            .try_wait()
            .map_err(|err| format!("failed to poll yi-agent: {err}"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|err| format!("failed to collect yi-agent output: {err}"));
        }

        if started.elapsed() >= timeout {
            child
                .kill()
                .map_err(|err| format!("failed to terminate timed-out yi-agent: {err}"))?;
            let output = child
                .wait_with_output()
                .map_err(|err| format!("failed to collect timed-out yi-agent output: {err}"))?;
            return Err(format!(
                "yi-agent timed out after {}s: {}",
                timeout.as_secs(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<Output, String> {
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn yi-agent: {err}"))?;
    wait_for_child(child, timeout)
}

/// 用 `--workdir` + `--json` 启动 yi-agent,超时后仅终止自己持有的 Child。
pub fn run_agent_with_timeout(workdir: &Path, prompt: &str) -> Result<Output, String> {
    let mut command = Command::new(yi_agent_bin());
    command
        .arg("--workdir")
        .arg(workdir)
        .arg("run")
        .arg("--json")
        .arg(prompt);
    run_command_with_timeout(&mut command, COMPLEX_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        original: BTreeMap<&'static str, Option<OsString>>,
    }

    impl EnvVarGuard {
        fn new(names: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                original: names
                    .into_iter()
                    .map(|name| (name, std::env::var_os(name)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (name, value) in &self.original {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn real_config_prefers_standard_environment_over_provider_key_fallback() {
        let _env = EnvVarGuard::new([
            "YI_AGENT_REAL_LLM_PROVIDER",
            "YI_AGENT_REAL_LLM_API_URL",
            "YI_AGENT_REAL_LLM_MODEL",
            "YI_AGENT_REAL_LLM_API_KEY",
            "YI_AGENT_PROVIDER",
            "MODEL_API_URL",
            "YI_AGENT_MODEL",
            "MODEL_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ]);
        unsafe {
            for name in [
                "YI_AGENT_REAL_LLM_PROVIDER",
                "YI_AGENT_REAL_LLM_API_URL",
                "YI_AGENT_REAL_LLM_MODEL",
                "YI_AGENT_REAL_LLM_API_KEY",
                "ANTHROPIC_API_KEY",
            ] {
                std::env::remove_var(name);
            }
            std::env::set_var("YI_AGENT_PROVIDER", "openai");
            std::env::set_var("MODEL_API_URL", "https://gateway.example.test/v1");
            std::env::set_var("YI_AGENT_MODEL", "gateway-model");
            std::env::set_var("MODEL_API_KEY", "gateway-key");
            std::env::set_var("OPENAI_API_KEY", "fallback-key");
        }

        let config = resolve_real_llm_test_config()
            .expect("configuration resolves")
            .expect("standard configuration is present");

        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_url, "https://gateway.example.test/v1");
        assert_eq!(config.model, "gateway-model");
    }

    #[test]
    fn owned_child_timeout_reports_timeout() {
        let child = Command::new("sh")
            .args(["-c", "sleep 1"])
            .spawn()
            .expect("spawn sleeping child");

        let err = wait_for_child(child, Duration::from_millis(20)).expect_err("should time out");
        assert!(err.contains("timed out"), "unexpected error: {err}");
    }

    #[test]
    fn completion_helper_requires_normal_end() {
        let events = parse_events(r#"{"Done":{"reason":"EndTurn"}}"#);
        assert!(has_normal_end_turn(&events));
    }
}
