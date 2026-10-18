//! TUI 首启的终端问答：把引导收敛成 `ModelSettings` 并写进 `models.json`。

use std::path::Path;

use anyhow::Result;

/// 是否应自动弹引导：未就绪且用户尚未结束过引导。
///
/// 就绪判定要看**清单**（模型设置的唯一真相源）：清单里已有可用默认条目就不再弹。
pub(crate) fn should_offer(env_path: &Path, preferences_path: &Path) -> bool {
    use yi_agent_runtime::onboarding::{Assessment, assess, load_dismissed, read_env_fields};
    if load_dismissed(preferences_path) {
        return false;
    }
    let env = read_env_fields(env_path).unwrap_or_default();
    let catalog =
        yi_agent_runtime::models::load_catalog_from(&yi_agent_runtime::models::models_path());
    matches!(assess(&env, Some(&catalog)), Assessment::Needed { .. })
}

/// provider 的默认 (api_url, model)，与 `config.rs` 的默认值同源。
pub(crate) fn provider_defaults(provider: &str) -> (&'static str, &'static str) {
    if provider == "openai" {
        ("https://api.openai.com/v1", "gpt-4o")
    } else {
        ("https://api.anthropic.com", "claude-sonnet-4-20250514")
    }
}

fn read_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn read_line_or(prompt: &str, default: &str) -> Result<String> {
    let value = read_line(prompt)?;
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value
    })
}

/// 跑一次终端问答并把模型写进清单。用户中断（EOF）即视为「稍后设置」。
pub(crate) fn run_onboarding_prompt(models_path: &Path, prefs_path: &Path) -> Result<()> {
    use yi_agent_runtime::onboarding::{self, ModelSettings, validate_settings};

    println!("欢迎使用 yi-agent。先配置一个模型就能开始对话。\n");

    let provider = loop {
        let raw = read_line_or(
            "API 格式 [anthropic/openai]（默认 anthropic）: ",
            "anthropic",
        )?;
        if yi_agent_runtime::models::ModelProvider::parse(&raw).is_some() {
            break raw.to_ascii_lowercase();
        }
        println!("请输入 anthropic 或 openai。");
    };

    let (default_url, default_model) = provider_defaults(&provider);
    let api_key = read_line("API 密钥: ")?;
    if api_key.is_empty() {
        println!("未填密钥，稍后可在设置里补。");
        onboarding::save_dismissed(prefs_path, true)?;
        return Ok(());
    }
    let model = read_line_or(
        &format!("模型标识（默认 {default_model}）: "),
        default_model,
    )?;
    let api_url = read_line_or(&format!("API 地址（默认 {default_url}）: "), default_url)?;

    let settings = ModelSettings {
        provider: provider.clone(),
        model: model.clone(),
        api_url: api_url.clone(),
        api_key: api_key.clone(),
    };
    if let Err(error) = validate_settings(&settings) {
        println!("配置无效：{error}");
        onboarding::save_dismissed(prefs_path, true)?;
        return Ok(());
    }

    let test = read_line_or("是否测试连接？[Y/n]（默认 Y）: ", "y")?;
    if !test.eq_ignore_ascii_case("n") {
        println!("正在测试连接…");
        let runtime = tokio::runtime::Runtime::new()?;
        match runtime.block_on(onboarding::test_connection(&settings)) {
            outcome if outcome.ok => println!("连接正常。"),
            outcome => {
                let reason = outcome.reason.unwrap_or_else(|| "未知原因".into());
                let save = read_line_or(
                    &format!("连接失败：{reason}。仍然保存？[Y/n]（默认 Y）: "),
                    "y",
                )?;
                if save.eq_ignore_ascii_case("n") {
                    onboarding::save_dismissed(prefs_path, true)?;
                    return Ok(());
                }
            }
        }
    }

    let name = onboarding::import_settings_into_catalog(models_path, &settings)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    onboarding::save_dismissed(prefs_path, true)?;
    println!("已保存模型「{name}」");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 涉及 `HOME` 的测试必须串行；`models::models_path()` 读的是 `$HOME/.yi-agent`。
    static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 把 `HOME` 指到临时家，Drop 时恢复。
    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &Path) -> Self {
            let original = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", path);
            }
            Self { original }
        }

        fn restore(&mut self) {
            unsafe {
                match self.original.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// 写一份含单条可用默认条目的清单到 `$HOME/.yi-agent/models.json`。
    fn write_catalog(home: &Path) {
        let dir = home.join(".yi-agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("models.json"),
            r#"{"default_model":"gpt-4o","models":[{"name":"gpt-4o","provider":"openai","api_url":"https://api.openai.com/v1","model":"gpt-4o","api_key":"sk-x","api_style":null,"reasoning_effort":null,"context_length":null}]}"#,
        )
        .unwrap();
    }

    #[test]
    fn offers_onboarding_on_a_blank_machine() {
        let _lock = HOME_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(home.path());
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        assert!(should_offer(&env, &prefs));
    }

    #[test]
    fn does_not_offer_when_the_catalog_is_complete() {
        let _lock = HOME_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(home.path());
        write_catalog(home.path());
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        assert!(!should_offer(&env, &prefs));
    }

    #[test]
    fn does_not_offer_after_dismissal() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        yi_agent_runtime::onboarding::save_dismissed(&prefs, true).unwrap();
        assert!(!should_offer(&env, &prefs));
    }

    #[test]
    fn a_completed_prompt_writes_the_catalog_not_the_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");

        // 直接断言共享导入器（`run_onboarding_prompt` 是它的终端封装）：
        let settings = yi_agent_runtime::onboarding::ModelSettings {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_url: "https://api.openai.com/v1".into(),
            api_key: "sk-x".into(),
        };
        let name =
            yi_agent_runtime::onboarding::import_settings_into_catalog(&models, &settings).unwrap();
        assert_eq!(name, "gpt-4o");

        let catalog = yi_agent_runtime::models::load_catalog_from(&models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert!(
            !dir.path().join(".env").exists(),
            "the prompt must not write .env"
        );
    }

    #[test]
    fn provider_defaults_cover_both_providers() {
        assert_eq!(
            provider_defaults("openai"),
            ("https://api.openai.com/v1", "gpt-4o")
        );
        assert_eq!(
            provider_defaults("anthropic"),
            ("https://api.anthropic.com", "claude-sonnet-4-20250514")
        );
    }
}
