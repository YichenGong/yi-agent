//! 配置加载：环境变量 + CLI 参数 > 默认值。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 运行时配置，由 CLI 参数和环境变量合并而来。
#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub compact_threshold: u32, // computed: context_length * ratio / 100
    pub compact_keep_turns: u32,
    pub yolo: bool,
    pub skills_catalog_budget: usize,
    /// True if user explicitly set the budget via CLI flag or env var (skips interactive prompt).
    pub skills_catalog_budget_explicit: bool,
}

/// clap CLI 参数定义。
#[derive(clap::Parser, Debug)]
#[command(name = "yi-agent", version, about = "Interactive AI agent CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// LLM provider: "anthropic" or "openai" (overrides YI_AGENT_PROVIDER)
    #[arg(long)]
    pub provider: Option<String>,

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

    /// Model max context length in tokens (fallback: 200000)
    #[arg(long)]
    pub model_context_length: Option<u32>,

    /// Percentage of context length triggering auto-compact (default: 80)
    #[arg(long)]
    pub compact_ratio: Option<u32>,

    /// Number of recent turns to keep during compact
    #[arg(long)]
    pub compact_keep_turns: Option<u32>,

    /// Skip permission prompts (except blacklisted commands)
    #[arg(long)]
    pub yolo: bool,

    /// Alias for --yolo
    #[arg(long = "dangerously-skip-permissions")]
    pub skip_permissions: bool,

    /// Maximum bytes for the skills catalog in the system prompt (default: 8192)
    #[arg(long)]
    pub skills_catalog_budget: Option<usize>,

    /// Enable debug-level tracing for conversation content (LLM messages and responses)
    #[arg(long)]
    pub debug: bool,
}

/// 子命令
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run a prompt non-interactively and exit (headless mode).
    Run {
        /// Prompt text. If omitted, reads from stdin.
        prompt: Option<String>,

        /// Output events as JSONL (one AgentEvent per line).
        #[arg(long)]
        json: bool,

        /// Read prompt from stdin even if prompt arg is given.
        #[arg(long)]
        stdin: bool,

        /// 裸模型模式:不注册任何工具,不加载 skills,不补 system prompt。
        /// 等同于直接对话裸 LLM,无任何附加能力。
        #[arg(long)]
        naked: bool,
    },
    /// Start web config UI
    Web {
        /// Host to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to bind
        #[arg(long, default_value = "7292")]
        port: u16,
    },
}

/// 解析 .env 文件路径：优先 workdir CLI 参数，否则 YI_AGENT_WORKDIR 环境变量，否则当前目录。
/// 所有路径均使用 `.yi-agent/.env` 子目录结构，避免与项目自身的 .env 冲突。
pub fn resolve_env_path(cli: &Cli) -> std::path::PathBuf {
    cli.workdir
        .as_ref()
        .map(|w| w.join(".yi-agent").join(".env"))
        .or_else(|| {
            std::env::var("YI_AGENT_WORKDIR")
                .ok()
                .map(PathBuf::from)
                .map(|p| p.join(".yi-agent").join(".env"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".yi-agent")
                .join(".env")
        })
}

/// 确保目录存在,不存在则递归创建(类似 mkdir -p)。
fn ensure_dir_exists(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory: {}", path.display()))
}

/// 加载 .env 文件到进程环境变量(不覆盖已存在的)。
///
/// - `local_path`: 本地 .env 路径(必填,不存在则静默跳过)
/// - `global_path`: 全局 .env 路径(可选,None 表示跳过全局)
///
/// 加载顺序:先 local 后 global。dotenvy 默认不覆盖已存在的环境变量,
/// 因此真实环境变量 > local > global。
pub fn load_env_files(local_path: &Path, global_path: Option<&Path>) {
    load_one_env(local_path);
    if let Some(global) = global_path {
        load_one_env(global);
    }
}

/// 加载单个 .env 文件,不存在则静默跳过,其他错误打印警告。
fn load_one_env(path: &Path) {
    if let Err(e) = dotenvy::from_path(path) {
        if !e.not_found() {
            eprintln!(
                "warning: failed to load .env from {}: {}",
                path.display(),
                e
            );
        }
    }
}

/// 解析全局 .env 路径:~/.yi-agent/.env
pub fn resolve_global_env_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .map(|h| h.join(".yi-agent").join(".env"))
}

/// 判断是否为显式指定 workdir(CLI 参数或环境变量)
pub fn is_workdir_explicit(cli: &Cli) -> bool {
    cli.workdir.is_some()
        || std::env::var("YI_AGENT_WORKDIR")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
}

/// 从 CLI 参数 + 环境变量加载配置。
///
/// 优先级：CLI 参数 > 环境变量 > 默认值。
/// .env 加载:显式指定 workdir 时只加载指定目录,fallback 模式合并全局兜底。
/// fallback 模式下自动创建 .yi-agent/ 目录(本地和全局)。
pub fn load(cli: &Cli) -> Result<Config> {
    let local_env_path = resolve_env_path(cli);
    let global_env_path = if is_workdir_explicit(cli) {
        None
    } else {
        // fallback 模式:自动创建本地和全局 .yi-agent/ 目录
        if let Some(parent) = local_env_path.parent() {
            ensure_dir_exists(parent)?;
        }
        let global = resolve_global_env_path();
        if let Some(ref g) = global {
            if let Some(parent) = g.parent() {
                ensure_dir_exists(parent)?;
            }
        }
        global
    };
    load_env_files(&local_env_path, global_env_path.as_deref());

    let provider = cli
        .provider
        .clone()
        .or_else(|| std::env::var("YI_AGENT_PROVIDER").ok())
        .unwrap_or_else(|| "anthropic".to_string());

    let api_key = cli
        .api_key
        .clone()
        .or_else(|| std::env::var("MODEL_API_KEY").ok())
        .context("API key required: set MODEL_API_KEY or use --api-key")?;
    if api_key.is_empty() {
        bail!("API key is empty: set MODEL_API_KEY or use --api-key");
    }

    let default_api_url = match provider.as_str() {
        "openai" => "https://api.openai.com",
        _ => "https://api.anthropic.com",
    };
    let default_model = match provider.as_str() {
        "openai" => "gpt-4o",
        _ => "claude-sonnet-4-20250514",
    };

    let api_url = cli
        .api_url
        .clone()
        .or_else(|| std::env::var("MODEL_API_URL").ok())
        .unwrap_or_else(|| default_api_url.to_string());

    let model = cli
        .model
        .clone()
        .or_else(|| std::env::var("YI_AGENT_MODEL").ok())
        .unwrap_or_else(|| default_model.to_string());

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
                .filter(|s| !s.is_empty())
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

    let model_context_length = cli.model_context_length.or_else(|| {
        std::env::var("YI_AGENT_MODEL_CONTEXT_LENGTH")
            .ok()
            .and_then(|s| s.parse().ok())
    });

    let compact_ratio = cli
        .compact_ratio
        .or_else(|| {
            std::env::var("YI_AGENT_COMPACT_RATIO")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(80);

    let effective_context_length = model_context_length.unwrap_or(200_000);
    let compact_threshold = effective_context_length * compact_ratio / 100;

    let compact_keep_turns = cli
        .compact_keep_turns
        .or_else(|| {
            std::env::var("YI_AGENT_COMPACT_KEEP_TURNS")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(4);

    let yolo = cli.yolo
        || cli.skip_permissions
        || std::env::var("YI_AGENT_YOLO")
            .map(|v| v == "true")
            .unwrap_or(false);

    let skills_catalog_budget_explicit = cli.skills_catalog_budget.is_some()
        || std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();
    let skills_catalog_budget = cli
        .skills_catalog_budget
        .or_else(|| {
            std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(8192);

    Ok(Config {
        provider,
        api_url,
        api_key,
        model,
        max_turns,
        workdir,
        system_prompt,
        compact_threshold,
        compact_keep_turns,
        yolo,
        skills_catalog_budget,
        skills_catalog_budget_explicit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    /// 测试用互斥锁:涉及环境变量的测试必须串行执行,避免并行干扰。
    static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        original: BTreeMap<&'static str, Option<OsString>>,
    }

    impl EnvVarGuard {
        fn new(names: impl IntoIterator<Item = &'static str>) -> Self {
            let original = names
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
            Self { original }
        }

        fn set(&mut self, name: &'static str, value: impl AsRef<std::ffi::OsStr>) {
            self.original
                .entry(name)
                .or_insert_with(|| std::env::var_os(name));
            unsafe {
                std::env::set_var(name, value);
            }
        }

        fn remove(&mut self, name: &'static str) {
            self.original
                .entry(name)
                .or_insert_with(|| std::env::var_os(name));
            unsafe {
                std::env::remove_var(name);
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

    struct CurrentDirGuard(std::path::PathBuf);

    impl CurrentDirGuard {
        fn change_to(path: &Path) -> Self {
            let original = std::env::current_dir().expect("read current directory");
            std::env::set_current_dir(path).expect("change current directory");
            Self(original)
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    #[test]
    fn env_var_guard_restores_host_value() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            std::env::set_var("YI_AGENT_TEST_GUARD", "host-value");
        }

        {
            let mut env = EnvVarGuard::new(["YI_AGENT_TEST_GUARD"]);
            env.remove("YI_AGENT_TEST_GUARD");
            assert!(std::env::var("YI_AGENT_TEST_GUARD").is_err());
        }

        assert_eq!(
            std::env::var("YI_AGENT_TEST_GUARD").as_deref(),
            Ok("host-value")
        );
        unsafe {
            std::env::remove_var("YI_AGENT_TEST_GUARD");
        }
    }

    #[test]
    fn load_requires_api_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY", "MODEL_API_URL"]);
        env.remove("MODEL_API_KEY");
        env.remove("MODEL_API_URL");
        let temp = tempfile::TempDir::new().expect("tempdir");
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            // An explicit empty workdir prevents fallback loading of a local
            // or global .yi-agent/.env file.
            workdir: Some(temp.path().to_path_buf()),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
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
            command: None,
            provider: Some("openai".into()),
            api_url: Some("https://example.com".into()),
            api_key: Some("test-key".into()),
            model: Some("test-model".into()),
            max_turns: Some(5),
            workdir: Some(PathBuf::from(".")),
            system_prompt: Some("custom prompt".into()),
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
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
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.api_url, "https://api.anthropic.com");
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.max_turns, 20);
        assert!(config.system_prompt.is_none());
    }

    #[test]
    fn load_includes_compact_defaults() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
        assert_eq!(config.compact_keep_turns, 4);
    }

    #[test]
    fn load_computes_threshold_from_context_and_ratio() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: Some(100_000),
            compact_ratio: Some(50),
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_threshold, 50_000); // 100000 * 50 / 100
    }

    #[test]
    fn load_falls_back_to_default_context_length() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: Some(80),
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
    }

    #[test]
    fn load_rejects_nonexistent_workdir() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from("/nonexistent/path/that/should/not/exist")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let result = load(&cli);
        assert!(result.is_err());
    }

    #[test]
    fn load_defaults_provider_to_anthropic() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.provider, "anthropic");
    }

    #[test]
    fn load_defaults_openai_provider() {
        let cli = Cli {
            command: None,
            provider: Some("openai".into()),
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_url, "https://api.openai.com");
        assert_eq!(config.model, "gpt-4o");
    }

    #[test]
    fn load_reads_dotenv_file() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        env.remove("MODEL_API_KEY");
        // 创建临时目录和 .yi-agent/.env 文件
        let temp_dir = std::env::temp_dir().join(".env_test_dotenv_dir");
        let yi_agent_dir = temp_dir.join(".yi-agent");
        std::fs::create_dir_all(&yi_agent_dir).unwrap();
        let env_path = yi_agent_dir.join(".env");
        std::fs::write(&env_path, "MODEL_API_KEY=from-dotenv-file\n").unwrap();

        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: Some(temp_dir.clone()),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.api_key, "from-dotenv-file");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn resolve_env_path_uses_yi_agent_subdir_for_workdir() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from("/tmp/my-project")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let path = resolve_env_path(&cli);
        assert_eq!(path, PathBuf::from("/tmp/my-project/.yi-agent/.env"));
    }

    #[test]
    fn resolve_env_path_uses_yi_agent_subdir_for_env_var() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR"]);
        env.set("YI_AGENT_WORKDIR", "/tmp/my-env-dir");
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let path = resolve_env_path(&cli);
        assert_eq!(path, PathBuf::from("/tmp/my-env-dir/.yi-agent/.env"));
    }

    #[test]
    fn load_env_files_loads_global_when_no_local() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        // local 不存在,global 存在 → 应该加载 global
        let temp = std::env::temp_dir().join(".env_test_global_only");
        let local_path = temp.join("local/.yi-agent/.env");
        let global_path = temp.join("global/.yi-agent/.env");
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();

        env.remove("MODEL_API_KEY");
        load_env_files(&local_path, Some(&global_path));

        assert_eq!(std::env::var("MODEL_API_KEY").unwrap(), "from-global");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_env_files_local_overrides_global() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        // local 和 global 都存在 → local 覆盖 global
        let temp = std::env::temp_dir().join(".env_test_local_overrides");
        let local_path = temp.join("local/.yi-agent/.env");
        let global_path = temp.join("global/.yi-agent/.env");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, "MODEL_API_KEY=from-local\n").unwrap();
        std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();

        env.remove("MODEL_API_KEY");
        load_env_files(&local_path, Some(&global_path));

        assert_eq!(std::env::var("MODEL_API_KEY").unwrap(), "from-local");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_env_files_skips_global_when_none() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        // global_path = None → 不加载 global(显式指定 --workdir 的场景)
        let temp = std::env::temp_dir().join(".env_test_no_global");
        let local_path = temp.join("local/.yi-agent/.env");
        let global_path = temp.join("global/.yi-agent/.env");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, "MODEL_API_KEY=from-local\n").unwrap();
        std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();

        env.remove("MODEL_API_KEY");
        load_env_files(&local_path, None);

        assert_eq!(std::env::var("MODEL_API_KEY").unwrap(), "from-local");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_env_files_real_env_overrides_all() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        // 真实环境变量 > local > global
        let temp = std::env::temp_dir().join(".env_test_real_env");
        let local_path = temp.join("local/.yi-agent/.env");
        let global_path = temp.join("global/.yi-agent/.env");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, "MODEL_API_KEY=from-local\n").unwrap();
        std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();

        env.set("MODEL_API_KEY", "from-real-env");
        load_env_files(&local_path, Some(&global_path));

        assert_eq!(std::env::var("MODEL_API_KEY").unwrap(), "from-real-env");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn ensure_dir_exists_creates_missing_directory() {
        let temp = std::env::temp_dir().join(".env_test_ensure_dir");
        let target = temp.join("a/b/c");
        assert!(!target.exists());

        ensure_dir_exists(&target).unwrap();
        assert!(target.is_dir());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn ensure_dir_exists_noop_when_already_exists() {
        let temp = std::env::temp_dir().join(".env_test_ensure_dir_exists");
        std::fs::create_dir_all(&temp).unwrap();

        ensure_dir_exists(&temp).unwrap();
        assert!(temp.is_dir());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_creates_local_yi_agent_dir_in_fallback_mode() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // fallback 模式下,当前目录的 .yi-agent/ 不存在时应自动创建
        let temp = std::env::temp_dir().join(".env_test_auto_create_local");
        std::fs::create_dir_all(&temp).unwrap();
        let yi_agent_dir = temp.join(".yi-agent");
        assert!(!yi_agent_dir.exists());

        // 临时切换 current_dir 到 temp
        let _cwd = CurrentDirGuard::change_to(&temp);

        // 清除可能干扰的环境变量
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR", "MODEL_API_KEY"]);
        env.remove("YI_AGENT_WORKDIR");
        env.remove("MODEL_API_KEY");

        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let result = load(&cli);
        assert!(result.is_ok(), "load should succeed: {:?}", result.err());

        // 验证 .yi-agent/ 目录已创建
        assert!(yi_agent_dir.is_dir(), ".yi-agent/ should be auto-created");
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_falls_back_to_current_dir_when_workdir_env_empty() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR"]);
        // 设置空字符串环境变量,应该 fallback 到 current_dir 而非变成空路径
        env.set("YI_AGENT_WORKDIR", "");
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert!(
            config.workdir.is_absolute(),
            "workdir should be a valid absolute path (current_dir fallback), got: {}",
            config.workdir.display()
        );
    }

    #[test]
    fn cli_parses_web_subcommand() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "web", "--host", "0.0.0.0", "--port", "9999"]);
        match cli.command {
            Some(Command::Web { host, port }) => {
                assert_eq!(host, "0.0.0.0");
                assert_eq!(port, 9999);
            }
            other => panic!("expected Web command, got {:?}", other),
        }
    }

    #[test]
    fn cli_parses_web_subcommand_defaults() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "web"]);
        match cli.command {
            Some(Command::Web { host, port }) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 7292);
            }
            other => panic!("expected Web command, got {:?}", other),
        }
    }

    #[test]
    fn cli_parses_run_naked_flag() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "run", "--naked", "hi"]);
        match cli.command {
            Some(Command::Run {
                prompt,
                json: _,
                stdin: _,
                naked,
            }) => {
                assert_eq!(prompt.as_deref(), Some("hi"));
                assert!(naked, "naked flag should be true");
            }
            other => panic!("expected Run command, got {:?}", other),
        }
    }

    #[test]
    fn cli_parses_run_default_naked_false() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "run", "hi"]);
        match cli.command {
            Some(Command::Run {
                prompt,
                json: _,
                stdin: _,
                naked,
            }) => {
                assert_eq!(prompt.as_deref(), Some("hi"));
                assert!(!naked, "naked flag should default to false");
            }
            other => panic!("expected Run command, got {:?}", other),
        }
    }

    #[test]
    fn cli_no_subcommand_has_none_command() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "--api-key", "test"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn yolo_env_var_enables_yolo() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_YOLO"]);
        env.set("YI_AGENT_YOLO", "true");
        let yolo = std::env::var("YI_AGENT_YOLO")
            .map(|v| v == "true")
            .unwrap_or(false);
        assert!(yolo);
    }

    #[test]
    fn yolo_env_var_false_by_default() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_YOLO"]);
        env.remove("YI_AGENT_YOLO");
        let yolo = std::env::var("YI_AGENT_YOLO")
            .map(|v| v == "true")
            .unwrap_or(false);
        assert!(!yolo);
    }

    #[test]
    fn cli_parses_yolo_flag() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "--yolo", "--api-key", "test"]);
        assert!(cli.yolo);
        assert!(!cli.skip_permissions);
    }

    #[test]
    fn cli_parses_dangerously_skip_permissions_flag() {
        use clap::Parser;
        let cli = Cli::parse_from([
            "yi-agent",
            "--dangerously-skip-permissions",
            "--api-key",
            "test",
        ]);
        assert!(!cli.yolo);
        assert!(cli.skip_permissions);
    }

    #[test]
    fn load_yolo_from_cli_flag() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: true,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert!(config.yolo);
    }

    #[test]
    fn load_yolo_from_skip_permissions_flag() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: true,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert!(config.yolo);
    }

    #[test]
    fn load_yolo_defaults_false() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        let config = load(&cli).unwrap();
        assert!(!config.yolo);
    }

    #[test]
    fn cli_parses_debug_flag() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "--debug", "--api-key", "test"]);
        assert!(cli.debug);
    }

    #[test]
    fn cli_debug_defaults_false() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "--api-key", "test"]);
        assert!(!cli.debug);
    }
}
