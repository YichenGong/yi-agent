//! 配置加载:环境变量 + 调用方覆盖项 > 默认值。
//!
//! 本模块由 CLI 与 GUI(app-server)共用,因此不依赖 clap。CLI 负责把
//! `clap` 解析出的参数转换成 [`ConfigOverrides`],再交给
//! [`RuntimeConfig::load`]。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Default resident subagent capacity, mirrored by
/// `AgentWorkerFactory::max_resident_subagents`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const RESIDENT_SUBAGENTS_DEFAULT: u16 = 64;

/// 运行时配置,由调用方覆盖项和环境变量合并而来。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    /// Resident subagent capacity for the daemon this config starts. Roots do not
    /// consume it. Defaults to [`RESIDENT_SUBAGENTS_DEFAULT`].
    pub max_resident_subagents: u16,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub compact_threshold: u32, // computed: context_length * ratio / 100
    pub compact_user_budget_tokens: usize,
    pub compact_tool_budget_tokens: usize,
    pub yolo: bool,
    /// 仅当沙箱既未被显式 / env 指定、且进程启动时 yolo 未打开时为 true,表示运行期
    /// 切到 yolo 可把沙箱提权到 DangerFullAccess(桌面端运行期切换场景)。
    /// 由 `load()` 推导。
    pub sandbox_promotable: bool,
    pub sandbox: yi_agent_tools::SandboxMode,
    pub sandbox_writable_roots: Vec<PathBuf>,
    pub skills_catalog_budget: usize,
    /// True if user explicitly set the budget via CLI flag or env var (skips interactive prompt).
    pub skills_catalog_budget_explicit: bool,
}

/// 来自 CLI flag 或 GUI 请求的纯数据覆盖项。不依赖 clap。
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub provider: Option<String>,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub workdir: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub model_context_length: Option<u32>,
    pub compact_ratio: Option<u32>,
    pub compact_keep_turns: Option<u32>,
    pub compact_user_budget_tokens: Option<usize>,
    pub compact_tool_budget_tokens: Option<usize>,
    pub yolo: bool,
    pub skip_permissions: bool,
    pub sandbox: Option<yi_agent_tools::SandboxMode>,
    pub sandbox_writable_roots: Vec<PathBuf>,
    pub skills_catalog_budget: Option<usize>,
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

/// 解析 `YI_AGENT_SANDBOX` 的值。
///
/// 语义等价于 clap 的 `ValueEnum::from_str(value, true)`(kebab-case 变体名、
/// 大小写不敏感);runtime crate 不依赖 clap,故在此手工映射。
fn parse_sandbox_mode(value: &str) -> Option<yi_agent_tools::SandboxMode> {
    if value.eq_ignore_ascii_case("read-only") {
        Some(yi_agent_tools::SandboxMode::ReadOnly)
    } else if value.eq_ignore_ascii_case("workspace-write") {
        Some(yi_agent_tools::SandboxMode::WorkspaceWrite)
    } else if value.eq_ignore_ascii_case("danger-full-access") {
        Some(yi_agent_tools::SandboxMode::DangerFullAccess)
    } else {
        None
    }
}

/// 解析 .env 文件路径:优先 workdir 覆盖项,否则 YI_AGENT_WORKDIR 环境变量,否则当前目录。
/// 所有路径均使用 `.yi-agent/.env` 子目录结构,避免与项目自身的 .env 冲突。
pub fn resolve_env_path(overrides: &ConfigOverrides) -> std::path::PathBuf {
    overrides
        .workdir
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

/// 加载 .env 文件到进程环境变量(不覆盖已存在的)。
///
/// - `local_path`: 本地 .env 路径(必填,不存在则静默跳过)
/// - `global_path`: 全局 .env 路径(可选,None 表示跳过全局)
///
/// 加载顺序:先 local 后 global。dotenvy 默认不覆盖已存在的环境变量,
/// 因此真实环境变量 > local > global。
pub(crate) fn load_env_files(local_path: &Path, global_path: Option<&Path>) {
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

/// 判断是否为显式指定 workdir(覆盖项或环境变量)
pub fn is_workdir_explicit(overrides: &ConfigOverrides) -> bool {
    overrides.workdir.is_some()
        || std::env::var("YI_AGENT_WORKDIR")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
}

/// Resolve the effective workdir without loading provider configuration.
///
/// Priority is the explicit workdir override, non-empty `YI_AGENT_WORKDIR`,
/// then the current directory.
pub fn resolve_workdir(overrides: &ConfigOverrides) -> Result<PathBuf> {
    let workdir = overrides
        .workdir
        .clone()
        .or_else(|| {
            std::env::var("YI_AGENT_WORKDIR")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if !workdir.is_dir() {
        bail!("working directory does not exist: {}", workdir.display());
    }
    Ok(workdir)
}

impl RuntimeConfig {
    /// 从覆盖项 + 环境变量加载配置。
    ///
    /// 优先级:覆盖项 > 环境变量 > 默认值。
    /// .env 加载:显式指定 workdir 时只加载指定目录,fallback 模式合并全局兜底。
    /// fallback 模式下只读取已存在的 `.yi-agent/.env`,不主动创建目录。
    pub fn load(overrides: &ConfigOverrides) -> Result<Self> {
        let local_env_path = resolve_env_path(overrides);
        let global_env_path = if is_workdir_explicit(overrides) {
            None
        } else {
            resolve_global_env_path()
        };
        load_env_files(&local_env_path, global_env_path.as_deref());

        let provider = overrides
            .provider
            .clone()
            .or_else(|| std::env::var("YI_AGENT_PROVIDER").ok())
            .unwrap_or_else(|| "anthropic".to_string());

        let api_key = overrides
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

        let api_url = overrides
            .api_url
            .clone()
            .or_else(|| std::env::var("MODEL_API_URL").ok())
            .unwrap_or_else(|| default_api_url.to_string());

        let model = overrides
            .model
            .clone()
            .or_else(|| std::env::var("YI_AGENT_MODEL").ok())
            .unwrap_or_else(|| default_model.to_string());

        let max_turns = overrides
            .max_turns
            .or_else(|| {
                std::env::var("YI_AGENT_MAX_TURNS")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(200);

        let max_resident_subagents = std::env::var("YI_AGENT_MAX_RESIDENT_SUBAGENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(RESIDENT_SUBAGENTS_DEFAULT);

        let workdir = resolve_workdir(overrides)?;

        let system_prompt = overrides
            .system_prompt
            .clone()
            .or_else(|| std::env::var("YI_AGENT_SYSTEM_PROMPT").ok())
            .filter(|s| !s.is_empty());

        let model_context_length = overrides.model_context_length.or_else(|| {
            std::env::var("YI_AGENT_MODEL_CONTEXT_LENGTH")
                .ok()
                .and_then(|s| s.parse().ok())
        });

        let compact_ratio = overrides
            .compact_ratio
            .or_else(|| {
                std::env::var("YI_AGENT_COMPACT_RATIO")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(80);

        let effective_context_length = model_context_length.unwrap_or(200_000);
        let compact_threshold = effective_context_length * compact_ratio / 100;

        let deprecated_keep_turns = overrides.compact_keep_turns.is_some()
            || std::env::var("YI_AGENT_COMPACT_KEEP_TURNS")
                .ok()
                .is_some_and(|value| !value.is_empty());
        if deprecated_keep_turns {
            eprintln!(
                "warning: YI_AGENT_COMPACT_KEEP_TURNS/--compact-keep-turns is deprecated and ignored; use compact token budgets instead"
            );
        }

        let compact_user_budget_tokens = overrides
            .compact_user_budget_tokens
            .or_else(|| env_usize("YI_AGENT_COMPACT_USER_BUDGET_TOKENS"))
            .unwrap_or(20_000);
        if compact_user_budget_tokens == 0 {
            bail!("compact user budget tokens must be greater than zero");
        }
        let compact_tool_budget_tokens = overrides
            .compact_tool_budget_tokens
            .or_else(|| env_usize("YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS"))
            .unwrap_or(12_000);

        let yolo = overrides.yolo
            || overrides.skip_permissions
            || std::env::var("YI_AGENT_YOLO")
                .map(|v| v == "true")
                .unwrap_or(false);

        let env_sandbox = std::env::var("YI_AGENT_SANDBOX").ok();
        // 显式 / env 指定的沙箱永不被提权。同时 `promotable` 只在「启动时非 yolo」时
        // 才有意义(桌面端运行期切换场景):若启动时 yolo 已打开(CLI `--yolo`、
        // `--dangerously-skip-permissions` 或 `YI_AGENT_YOLO`),`sandbox` 已经反映了
        // 今天的行为,运行期不应再提权。`overrides.yolo` 蕴含 `yolo == true`,故首臂
        // 的 `!yolo` 恒为 false;两臂统一写 `!yolo` 以免日后漂移。
        let (sandbox, sandbox_promotable) = match overrides.sandbox {
            Some(mode) => (mode, false),
            None => match env_sandbox {
                Some(value) => (
                    parse_sandbox_mode(&value).ok_or_else(|| {
                        anyhow::anyhow!(
                            "invalid YI_AGENT_SANDBOX: expected read-only, workspace-write, or danger-full-access"
                        )
                    })?,
                    false,
                ),
                None if overrides.yolo => (yi_agent_tools::SandboxMode::DangerFullAccess, !yolo),
                None => (yi_agent_tools::SandboxMode::default(), !yolo),
            },
        };

        let sandbox_writable_roots = overrides.sandbox_writable_roots.clone();

        let skills_catalog_budget_explicit = overrides.skills_catalog_budget.is_some()
            || std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some();
        let skills_catalog_budget = overrides
            .skills_catalog_budget
            .or_else(|| {
                std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(8192);

        Ok(RuntimeConfig {
            provider,
            api_url,
            api_key,
            model,
            max_turns,
            max_resident_subagents,
            workdir,
            system_prompt,
            compact_threshold,
            compact_user_budget_tokens,
            compact_tool_budget_tokens,
            yolo,
            sandbox_promotable,
            sandbox,
            sandbox_writable_roots,
            skills_catalog_budget,
            skills_catalog_budget_explicit,
        })
    }

    /// 返回给 GUI 的安全配置视图:api_key 脱敏,其余字段原样。
    pub fn redacted_view(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "api_url": self.api_url,
            "api_key": if self.api_key.is_empty() { "" } else { "***" },
            "model": self.model,
            "max_turns": self.max_turns,
            "max_resident_subagents": self.max_resident_subagents,
            "workdir": self.workdir.display().to_string(),
            "sandbox": format!("{:?}", self.sandbox),
            "sandbox_promotable": self.sandbox_promotable,
            "yolo": self.yolo,
            "compact_threshold": self.compact_threshold,
        })
    }
}

/// 测试辅助:构造一个字段任意的合法 [`RuntimeConfig`]。
///
/// 值可以随便填,仅供只关心个别字段的测试使用;它**不等于** [`RuntimeConfig::load`]
/// 的默认值,不要用它断言加载语义。
#[cfg(test)]
pub(crate) fn sample_config() -> RuntimeConfig {
    RuntimeConfig {
        provider: "anthropic".to_string(),
        api_url: "https://api.anthropic.com".to_string(),
        api_key: "sk-secret".to_string(),
        model: "test-model".to_string(),
        max_turns: 20,
        max_resident_subagents: RESIDENT_SUBAGENTS_DEFAULT,
        workdir: PathBuf::from("/tmp/test-workdir"),
        system_prompt: None,
        compact_threshold: 160_000,
        compact_user_budget_tokens: 20_000,
        compact_tool_budget_tokens: 12_000,
        yolo: false,
        sandbox_promotable: true,
        sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
        sandbox_writable_roots: Vec::new(),
        skills_catalog_budget: 8192,
        skills_catalog_budget_explicit: false,
    }
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

    fn isolated_config_env() -> EnvVarGuard {
        let mut env = EnvVarGuard::new([
            "MODEL_API_KEY",
            "MODEL_API_URL",
            "YI_AGENT_PROVIDER",
            "YI_AGENT_MODEL",
            "YI_AGENT_WORKDIR",
            "YI_AGENT_YOLO",
            "YI_AGENT_SANDBOX",
            "YI_AGENT_MAX_TURNS",
            "YI_AGENT_SYSTEM_PROMPT",
            "YI_AGENT_MODEL_CONTEXT_LENGTH",
            "YI_AGENT_COMPACT_RATIO",
            "YI_AGENT_COMPACT_KEEP_TURNS",
            "YI_AGENT_COMPACT_USER_BUDGET_TOKENS",
            "YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS",
            "YI_AGENT_SKILLS_CATALOG_BUDGET",
            "YI_AGENT_MAX_RESIDENT_SUBAGENTS",
        ]);
        for key in [
            "MODEL_API_KEY",
            "MODEL_API_URL",
            "YI_AGENT_PROVIDER",
            "YI_AGENT_MODEL",
            "YI_AGENT_WORKDIR",
            "YI_AGENT_YOLO",
            "YI_AGENT_SANDBOX",
            "YI_AGENT_MAX_TURNS",
            "YI_AGENT_SYSTEM_PROMPT",
            "YI_AGENT_MODEL_CONTEXT_LENGTH",
            "YI_AGENT_COMPACT_RATIO",
            "YI_AGENT_COMPACT_KEEP_TURNS",
            "YI_AGENT_COMPACT_USER_BUDGET_TOKENS",
            "YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS",
            "YI_AGENT_SKILLS_CATALOG_BUDGET",
            "YI_AGENT_MAX_RESIDENT_SUBAGENTS",
        ] {
            env.remove(key);
        }
        env
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

    /// 测试用覆盖项:只设置 API key 和 workdir,其余走默认值。
    fn test_overrides() -> ConfigOverrides {
        ConfigOverrides {
            api_key: Some("test-key".into()),
            workdir: Some(PathBuf::from(".")),
            ..Default::default()
        }
    }

    #[test]
    fn parse_sandbox_mode_covers_all_variants() {
        use clap::ValueEnum;
        for variant in yi_agent_tools::SandboxMode::value_variants() {
            let name = variant.to_possible_value().expect("has possible value");
            let name = name.get_name();
            assert_eq!(
                parse_sandbox_mode(name),
                Some(*variant),
                "variant `{name}` must be parseable"
            );
            // 大小写不敏感
            assert_eq!(
                parse_sandbox_mode(&name.to_ascii_uppercase()),
                Some(*variant)
            );
        }
    }

    #[test]
    fn parse_sandbox_mode_rejects_invalid_input() {
        for value in ["", " ", "  \t", "bogus", "read_only", "workspace_write"] {
            assert_eq!(
                parse_sandbox_mode(value),
                None,
                "`{value}` must be rejected"
            );
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
    fn resolve_workdir_prefers_override_value() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..Default::default()
        };

        assert_eq!(
            resolve_workdir(&overrides).expect("resolve override workdir"),
            temp.path()
        );
    }

    #[test]
    fn resolve_workdir_uses_nonempty_environment_value() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR"]);
        env.set(
            "YI_AGENT_WORKDIR",
            temp.path().to_str().expect("UTF-8 temporary path"),
        );
        let overrides = ConfigOverrides::default();

        assert_eq!(
            resolve_workdir(&overrides).expect("resolve environment workdir"),
            temp.path()
        );
    }

    #[test]
    fn load_requires_api_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let result = RuntimeConfig::load(&overrides);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("API key"),
            "error should mention API key, got: {msg}"
        );
    }

    #[test]
    fn load_loads_from_cli_args() {
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.api_url, "https://example.com");
        assert_eq!(config.api_key, "test-key");
        assert_eq!(config.model, "test-model");
        assert_eq!(config.max_turns, 5);
        assert_eq!(config.system_prompt.as_deref(), Some("custom prompt"));
    }

    #[test]
    fn load_defaults_api_url_and_model() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.api_url, "https://api.anthropic.com");
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(
            config.max_turns, 200,
            "an interactive session must not be capped at a turn count a real \
             task exhausts; YI_AGENT_MAX_TURNS still overrides this"
        );
        assert!(config.system_prompt.is_none());
    }

    #[test]
    fn load_includes_compact_defaults() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
        assert_eq!(config.compact_user_budget_tokens, 20_000);
        assert_eq!(config.compact_tool_budget_tokens, 12_000);
    }

    #[test]
    fn load_computes_threshold_from_context_and_ratio() {
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.compact_threshold, 50_000); // 100000 * 50 / 100
    }

    #[test]
    fn load_falls_back_to_default_context_length() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
    }

    #[test]
    fn load_rejects_nonexistent_workdir() {
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let result = RuntimeConfig::load(&overrides);
        assert!(result.is_err());
    }

    #[test]
    fn load_defaults_provider_to_anthropic() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.provider, "anthropic");
    }

    #[test]
    fn load_defaults_openai_provider() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
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

        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(config.api_key, "from-dotenv-file");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn resolve_env_path_uses_yi_agent_subdir_for_workdir() {
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let path = resolve_env_path(&overrides);
        assert_eq!(path, PathBuf::from("/tmp/my-project/.yi-agent/.env"));
    }

    #[test]
    fn resolve_env_path_uses_yi_agent_subdir_for_env_var() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR"]);
        env.set("YI_AGENT_WORKDIR", "/tmp/my-env-dir");
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let path = resolve_env_path(&overrides);
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
    fn load_does_not_create_local_yi_agent_dir_in_fallback_mode() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // fallback 模式只读取本地 .yi-agent/.env,不应污染启动目录。
        let temp = std::env::temp_dir().join(".env_test_auto_create_local");
        std::fs::remove_dir_all(&temp).ok();
        std::fs::create_dir_all(&temp).unwrap();
        let yi_agent_dir = temp.join(".yi-agent");
        assert!(!yi_agent_dir.exists());

        // 临时切换 current_dir 到 temp
        let _cwd = CurrentDirGuard::change_to(&temp);

        // 清除可能干扰的环境变量
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR", "MODEL_API_KEY"]);
        env.remove("YI_AGENT_WORKDIR");
        env.remove("MODEL_API_KEY");

        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let result = RuntimeConfig::load(&overrides);
        assert!(result.is_ok(), "load should succeed: {:?}", result.err());

        assert!(
            !yi_agent_dir.exists(),
            ".yi-agent/ should not be created until yi-agent writes a project file"
        );
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
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert!(
            config.workdir.is_absolute(),
            "workdir should be a valid absolute path (current_dir fallback), got: {}",
            config.workdir.display()
        );
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
    fn load_yolo_from_cli_flag() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_SANDBOX"]);
        env.remove("YI_AGENT_SANDBOX");
        let mut overrides = test_overrides();
        overrides.yolo = true;
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert!(config.yolo);
        assert_eq!(
            config.sandbox,
            yi_agent_tools::SandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn explicit_cli_sandbox_overrides_yolo() {
        let mut overrides = test_overrides();
        overrides.yolo = true;
        overrides.sandbox = Some(yi_agent_tools::SandboxMode::ReadOnly);
        assert_eq!(
            RuntimeConfig::load(&overrides).unwrap().sandbox,
            yi_agent_tools::SandboxMode::ReadOnly
        );
    }

    #[test]
    fn environment_sandbox_overrides_yolo() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_SANDBOX"]);
        env.set("YI_AGENT_SANDBOX", "read-only");
        let mut overrides = test_overrides();
        overrides.yolo = true;
        assert_eq!(
            RuntimeConfig::load(&overrides).unwrap().sandbox,
            yi_agent_tools::SandboxMode::ReadOnly
        );
    }

    #[test]
    fn sandbox_promotable_false_when_sandbox_explicit() {
        let mut overrides = test_overrides();
        overrides.yolo = true;
        overrides.sandbox = Some(yi_agent_tools::SandboxMode::ReadOnly);
        let cfg = RuntimeConfig::load(&overrides).unwrap();
        assert!(!cfg.sandbox_promotable);
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::ReadOnly);
    }

    #[test]
    fn sandbox_promotable_false_when_sandbox_from_env() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_SANDBOX"]);
        env.set("YI_AGENT_SANDBOX", "read-only");
        let mut overrides = test_overrides();
        overrides.yolo = true;
        let cfg = RuntimeConfig::load(&overrides).unwrap();
        assert!(!cfg.sandbox_promotable);
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::ReadOnly);
    }

    #[test]
    fn sandbox_promotable_false_when_skip_permissions() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let mut overrides = test_overrides();
        overrides.skip_permissions = true;
        let cfg = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::WorkspaceWrite);
        assert!(!cfg.sandbox_promotable);
    }

    #[test]
    fn sandbox_promotable_false_when_yolo_flag() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let mut overrides = test_overrides();
        overrides.yolo = true;
        let cfg = RuntimeConfig::load(&overrides).unwrap();
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::DangerFullAccess);
        assert!(!cfg.sandbox_promotable);
    }

    #[test]
    fn sandbox_promotable_true_by_default() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let cfg = RuntimeConfig::load(&test_overrides()).unwrap();
        assert!(cfg.sandbox_promotable);
    }

    #[test]
    fn skip_permissions_keeps_default_sandbox() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_SANDBOX"]);
        env.remove("YI_AGENT_SANDBOX");
        let mut overrides = test_overrides();
        overrides.skip_permissions = true;
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert!(config.yolo);
        assert_eq!(config.sandbox, yi_agent_tools::SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn yolo_environment_variable_keeps_default_sandbox() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["YI_AGENT_YOLO", "YI_AGENT_SANDBOX"]);
        env.set("YI_AGENT_YOLO", "true");
        env.remove("YI_AGENT_SANDBOX");
        let config = RuntimeConfig::load(&test_overrides()).unwrap();
        assert!(config.yolo);
        assert_eq!(config.sandbox, yi_agent_tools::SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn load_yolo_defaults_false() {
        let overrides = ConfigOverrides {
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
            compact_user_budget_tokens: None,
            compact_tool_budget_tokens: None,
            yolo: false,
            sandbox: None,
            sandbox_writable_roots: Vec::new(),
            skip_permissions: false,
            skills_catalog_budget: None,
        };
        let config = RuntimeConfig::load(&overrides).unwrap();
        assert!(!config.yolo);
    }

    #[test]
    fn redacted_view_hides_api_key() {
        let cfg = sample_config();
        let view = cfg.redacted_view();
        assert_eq!(view["api_key"], "***");
        assert_eq!(view["model"], cfg.model);
        assert_eq!(view["workdir"], cfg.workdir.display().to_string());
    }

    #[test]
    fn redacted_view_empty_key_stays_empty() {
        let mut cfg = sample_config();
        cfg.api_key = String::new();
        assert_eq!(cfg.redacted_view()["api_key"], "");
    }

    #[test]
    fn max_resident_subagents_defaults_to_sixty_four() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            api_key: Some("sk-test".into()),
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_resident_subagents, 64);
    }

    #[test]
    fn the_environment_can_lower_the_resident_capacity() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        env.set("YI_AGENT_MAX_RESIDENT_SUBAGENTS", "8");
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            api_key: Some("sk-test".into()),
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_resident_subagents, 8);
    }
}
