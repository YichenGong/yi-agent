//! 配置加载:环境变量 + 调用方覆盖项 > 默认值。
//!
//! 本模块由 CLI 与 GUI(app-server)共用,因此不依赖 clap。CLI 负责把
//! `clap` 解析出的参数转换成 [`ConfigOverrides`],再交给
//! [`RuntimeConfig::load`]。

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use yi_agent_core::ApiStyle;

/// `.env` 不再持有这些键；它们只认「真实进程环境 + CLI 覆盖」这一条整包注入路径。
pub const MODEL_ENV_KEYS: &[&str] = &[
    "YI_AGENT_PROVIDER",
    "YI_AGENT_MODEL",
    "YI_AGENT_MODEL_CONTEXT_LENGTH",
    "MODEL_API_KEY",
    "MODEL_API_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];

/// 加载任何 `.env` **之前**对模型键做一次进程环境快照。
///
/// 必要性：`dotenvy::from_path` 会把 `.env` 的值写进进程环境变量，之后
/// `std::env::var("MODEL_API_KEY")` 无法分辨它是用户 shell 导出的、还是刚从
/// `.env` 读进来的。快照是唯一能区分二者的时刻。
pub struct ModelEnvSnapshot {
    values: std::collections::BTreeMap<&'static str, Option<String>>,
}

impl ModelEnvSnapshot {
    pub fn take() -> Self {
        Self {
            values: MODEL_ENV_KEYS
                .iter()
                .map(|key| {
                    let value = std::env::var(key).ok().filter(|v| !v.is_empty());
                    (*key, value)
                })
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned().flatten()
    }
}

/// 把「因 `.env` 而出现」的模型键从进程环境剔除，避免泄漏给子进程。
///
/// 判据：快照里没有、现在却有 → 它必然来自 `.env`。真实进程环境里的键不动。
fn prune_dotenv_model_keys(snapshot: &ModelEnvSnapshot) {
    for key in MODEL_ENV_KEYS {
        let from_real_env = snapshot.values.get(key).is_some_and(Option::is_some);
        if !from_real_env && std::env::var(key).ok().is_some_and(|v| !v.is_empty()) {
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}

/// Default resident subagent capacity, mirrored by
/// `AgentWorkerFactory::max_resident_subagents`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const RESIDENT_SUBAGENTS_DEFAULT: u16 = 64;

/// Default direct-child limit, mirrored by
/// `AgentWorkerFactory::max_direct_children`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const DIRECT_CHILDREN_DEFAULT: u16 = 8;

/// 运行时配置,由调用方覆盖项和环境变量合并而来。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    /// 端点风格：`auto`（默认）| `responses` | `chat`。条目未声明时为
    /// [`RuntimeConfig::default_api_style`]。
    pub api_style: ApiStyle,
    /// 推理努力档位（七档之一）；`None` = 请求体不带该字段（零回归默认）。
    pub reasoning_effort: Option<String>,
    pub max_turns: u32,
    /// Resident subagent capacity for the daemon this config starts. Roots do not
    /// consume it. Defaults to [`RESIDENT_SUBAGENTS_DEFAULT`].
    pub max_resident_subagents: u16,
    /// How many non-terminal direct children one agent may own. Defaults to
    /// [`DIRECT_CHILDREN_DEFAULT`].
    pub max_direct_children: u16,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub compact_threshold: u32, // computed: context_length * ratio / 100
    /// 触发自动压缩的上下文占比（百分数）。保留原值以便条目改写 context_length 时重算阈值。
    pub compact_ratio: u32,
    /// 显式声明的上下文窗口（tokens）；`None` = 未声明（阈值用兜底长度算出）。
    pub model_context_length: Option<u32>,
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
    /// 本次配置是否来自「整包注入」（CLI flag 或真实进程环境变量）。
    /// 为真时它压过清单的 `default_model`（会话覆盖仍在其上）。
    pub model_injected: bool,
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

/// 加载全局 `.env` 到进程环境变量(不覆盖已存在的)。文件不存在则静默跳过。
///
/// 「全局」是唯一的 `.env`：模型设置已迁到 `models.json`，其余键（轮次、压缩、
/// 沙箱等）只从 `~/.yi-agent/.env` 读，不再叠加项目级 `.env`。
pub(crate) fn load_env_files(path: &Path) {
    load_one_env(path);
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
    /// 条目未声明 `api_style` 时运行时采用的默认端点风格（`auto`）。
    ///
    /// 与 `resolve_effective` 的兜底同源，避免两处各写一份常量而漂移。
    pub fn default_api_style() -> ApiStyle {
        ApiStyle::Auto
    }

    /// 从覆盖项 + 环境变量加载配置。
    ///
    /// 优先级:覆盖项 > 真实进程环境变量 > 全局 `.env`（仅非模型键） > 默认值。
    /// `.env` 只有一处:`$HOME/.yi-agent/.env`,不主动创建目录。
    pub fn load(overrides: &ConfigOverrides) -> Result<Self> {
        Self::load_inner(overrides, false)
    }

    /// 与 [`load`] 相同，但**允许 API key 缺失**（回空串）。
    ///
    /// 桌面侧车在新机器上必须能起来才能提供引导 RPC；缺 key 时按空 key 加载，
    /// 由调用方先跑引导再真正发起对话。
    pub fn load_lenient(overrides: &ConfigOverrides) -> Result<Self> {
        Self::load_inner(overrides, true)
    }

    fn load_inner(overrides: &ConfigOverrides, allow_missing_key: bool) -> Result<Self> {
        // 快照必须早于任何 .env 加载（见 `ModelEnvSnapshot` 的文档）。
        let model_env = ModelEnvSnapshot::take();
        // 唯一的 `.env` 是全局 `~/.yi-agent/.env`；没有 HOME 时退回到 workdir 内的
        // 本地路径（与 `models_path` 同口径），不再有项目级 `.env` 概念。
        let env_path =
            resolve_global_env_path().unwrap_or_else(|| PathBuf::from(".yi-agent").join(".env"));
        load_env_files(&env_path);
        prune_dotenv_model_keys(&model_env);

        let injected_provider = overrides
            .provider
            .clone()
            .or_else(|| model_env.get("YI_AGENT_PROVIDER"));
        let injected_model = overrides
            .model
            .clone()
            .or_else(|| model_env.get("YI_AGENT_MODEL"));
        let injected_url = overrides
            .api_url
            .clone()
            .or_else(|| model_env.get("MODEL_API_URL"));
        let injected_key = overrides
            .api_key
            .clone()
            .or_else(|| model_env.get("MODEL_API_KEY"));

        // 「整包」判据：注入要生效，provider 与 api_key 必须给全。给一半即报错，
        // 绝不拼出 anthropic 地址配 openai 模型名这类畸形组合。
        let injection_present = injected_provider.is_some()
            || injected_key.is_some()
            || injected_url.is_some()
            || injected_model.is_some();
        let model_injected = injected_provider.is_some() && injected_key.is_some();
        if injection_present && !model_injected {
            bail!(
                "incomplete model override: provide both provider and api_key \
                 (got provider={}, api_key={}); use the settings UI to configure a model entry instead",
                injected_provider.is_some(),
                injected_key.is_some()
            );
        }

        let provider = injected_provider.unwrap_or_else(|| "anthropic".to_string());
        let default_api_url = match provider.as_str() {
            "openai" => "https://api.openai.com/v1",
            _ => "https://api.anthropic.com",
        };
        let api_url = injected_url.unwrap_or_else(|| default_api_url.to_string());
        // 模型名没有客观默认值:猜一个名字发出去只会得到 404,而且这个猜测会一路
        // 传播到子 agent(`subagent_model` -> `default_model` -> `cfg`)。空串是
        // "未配置"的唯一诚实表示,由 RPC 入口与 CLI 给出可操作的指引。
        let model = injected_model.unwrap_or_default();
        let api_key = match injected_key {
            Some(key) => key,
            _ if allow_missing_key => String::new(),
            _ => bail!("API key required: set MODEL_API_KEY or use --api-key"),
        };

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

        let max_direct_children = std::env::var("YI_AGENT_MAX_DIRECT_CHILDREN")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DIRECT_CHILDREN_DEFAULT);

        let workdir = resolve_workdir(overrides)?;

        let system_prompt = overrides
            .system_prompt
            .clone()
            .or_else(|| std::env::var("YI_AGENT_SYSTEM_PROMPT").ok())
            .filter(|s| !s.is_empty());

        let model_context_length = overrides.model_context_length.or_else(|| {
            model_env
                .get("YI_AGENT_MODEL_CONTEXT_LENGTH")
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
            api_style: RuntimeConfig::default_api_style(),
            reasoning_effort: None,
            max_turns,
            max_resident_subagents,
            max_direct_children,
            workdir,
            system_prompt,
            compact_threshold,
            compact_ratio,
            model_context_length,
            compact_user_budget_tokens,
            compact_tool_budget_tokens,
            yolo,
            sandbox_promotable,
            sandbox,
            sandbox_writable_roots,
            skills_catalog_budget,
            skills_catalog_budget_explicit,
            model_injected,
        })
    }

    /// 返回给 GUI 的安全配置视图:api_key 脱敏,其余字段原样。
    ///
    /// 这是**进程启动时**的配置快照。桌面端一个 app-server 服务多个会话,每个
    /// 会话各有 cwd / 模型覆盖 / 权限档,故按会话展示时必须走
    /// [`redacted_view_for_session`](Self::redacted_view_for_session),否则卡片
    /// 会摊出用户没选过的 provider / 模型 / 工作目录。
    pub fn redacted_view(&self) -> serde_json::Value {
        self.redacted_view_for_session(&self.workdir, &self.model, self.yolo)
    }

    /// 与 [`redacted_view`](Self::redacted_view) 同一形状,但三处按**会话**覆盖:
    /// `workdir` / `model` / `yolo`。
    ///
    /// `provider` / `api_url` / `api_key` / `compact_threshold` 仍取自 `self`:
    /// 调用方应先用 [`models::resolve_effective`](crate::models::resolve_effective)
    /// 把本会话的模型覆盖解析进来,再调用本方法。
    pub fn redacted_view_for_session(
        &self,
        workdir: &std::path::Path,
        model: &str,
        yolo: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "api_url": self.api_url,
            "api_key": if self.api_key.is_empty() { "" } else { "***" },
            "model": model,
            "max_turns": self.max_turns,
            "max_resident_subagents": self.max_resident_subagents,
            "max_direct_children": self.max_direct_children,
            "workdir": workdir.display().to_string(),
            "sandbox": format!("{:?}", self.sandbox),
            "sandbox_promotable": self.sandbox_promotable,
            "yolo": yolo,
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
        api_style: RuntimeConfig::default_api_style(),
        reasoning_effort: None,
        max_turns: 20,
        max_resident_subagents: RESIDENT_SUBAGENTS_DEFAULT,
        max_direct_children: DIRECT_CHILDREN_DEFAULT,
        workdir: PathBuf::from("/tmp/test-workdir"),
        system_prompt: None,
        compact_threshold: 160_000,
        compact_ratio: 80,
        model_context_length: None,
        compact_user_budget_tokens: 20_000,
        compact_tool_budget_tokens: 12_000,
        yolo: false,
        sandbox_promotable: true,
        sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
        sandbox_writable_roots: Vec::new(),
        skills_catalog_budget: 8192,
        skills_catalog_budget_explicit: false,
        model_injected: false,
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
            "HOME",
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
            "YI_AGENT_MAX_DIRECT_CHILDREN",
        ]);
        // 唯一 `.env` 是 `$HOME/.yi-agent/.env`。测试必须把 HOME 指到一个不存在
        // 的位置，否则会读到开发者机器上的真实 `.env`（其非模型键会污染默认值断言）。
        env.set(
            "HOME",
            std::env::temp_dir()
                .join(format!("yi-agent-config-test-home-{}", std::process::id()))
                .as_os_str(),
        );
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
            "YI_AGENT_MAX_DIRECT_CHILDREN",
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
            provider: Some("anthropic".into()),
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
    fn load_lenient_allows_a_missing_api_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load_lenient(&overrides).expect("lenient load must succeed");
        assert_eq!(config.api_key, "");
    }

    #[test]
    fn load_still_requires_a_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        assert!(RuntimeConfig::load(&overrides).is_err());
    }

    #[test]
    fn load_loads_from_cli_args() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    fn load_defaults_the_url_but_not_a_model() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
        // 地址有客观默认值,模型名没有:没有显式注入就必须是空串,由调用方
        // 自己判断"未配置"并报可识别的错误,而不是凭空猜一个名字发出去。
        assert_eq!(config.model, "");
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
            provider: Some("anthropic".into()),
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
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
            provider: Some("anthropic".into()),
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
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
            provider: Some("anthropic".into()),
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
        assert_eq!(config.api_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "", "no default model name may be invented");
    }

    /// `.env` 仍会被加载（非模型键需要它），但它**不再是模型 key 的来源**。
    ///
    /// 这是 Task 2 的语义翻转：同一个 `MODEL_API_KEY=...` 写进 `$HOME/.yi-agent/.env`，
    /// 但整包注入只认「真实进程环境 + CLI 覆盖」，故它既不进 config，也不残留在进程
    /// 环境里（`prune_dotenv_model_keys` 已剔除），更不会泄漏给子进程。
    #[test]
    fn load_reads_dotenv_file() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        // 唯一 `.env` 在 `$HOME/.yi-agent/.env`：把 HOME 指向本次测试的临时家。
        let home = tempfile::TempDir::new().unwrap();
        env.set("HOME", home.path());
        let yi_agent_dir = home.path().join(".yi-agent");
        std::fs::create_dir_all(&yi_agent_dir).unwrap();
        let env_path = yi_agent_dir.join(".env");
        std::fs::write(
            &env_path,
            "MODEL_API_KEY=from-dotenv-file\nYI_AGENT_MAX_TURNS=123\n",
        )
        .unwrap();

        let overrides = ConfigOverrides {
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: Some(home.path().to_path_buf()),
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
        assert_eq!(config.api_key, "", "dotenv must not supply the model key");
        assert!(!config.model_injected);
        assert_eq!(
            config.max_turns, 123,
            "non-model keys still come from the .env"
        );
        assert_eq!(
            std::env::var("MODEL_API_KEY").ok(),
            None,
            "the key dotenvy injected must be pruned from the process env"
        );
    }

    #[test]
    fn a_dotenv_model_key_never_reaches_the_config() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        // `.env` 必须落在唯一读取点：`$HOME/.yi-agent/.env`。
        let env_dir = home.path().join(".yi-agent");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::write(env_dir.join(".env"), "MODEL_API_KEY=from-dotenv\n").unwrap();

        let mut env = isolated_config_env();
        env.set("HOME", home.path());

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(home.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_ne!(cfg.api_key, "from-dotenv");
        assert_eq!(std::env::var("MODEL_API_KEY").ok(), None);
    }

    #[test]
    fn a_real_process_env_key_still_injects() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        env.set("MODEL_API_KEY", "from-shell");
        env.set("YI_AGENT_PROVIDER", "openai");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides::default()).unwrap();
        assert_eq!(cfg.api_key, "from-shell");
        assert!(cfg.model_injected);
    }

    /// 剔除只针对「因 `.env` 而出现」的键；真实 shell 导出的键必须原样留在进程环境里
    /// （它仍要能被 spawn 出去的子进程继承）。
    #[test]
    fn prune_leaves_a_real_process_env_key_alone() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        env.set("MODEL_API_KEY", "from-shell");
        env.set("YI_AGENT_PROVIDER", "openai");
        let temp = tempfile::TempDir::new().unwrap();

        RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(
            std::env::var("MODEL_API_KEY").ok().as_deref(),
            Some("from-shell")
        );
        assert_eq!(
            std::env::var("YI_AGENT_PROVIDER").ok().as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn half_an_injection_is_an_error() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        env.set("YI_AGENT_PROVIDER", "openai"); // no key anywhere
        let result = RuntimeConfig::load_lenient(&ConfigOverrides::default());
        assert!(result.is_err());
    }

    /// 整包注入的判据是 `is_some()`（不是「非空」），所以显式的空串覆盖会被当成
    /// 完整的整包：严格模式 `load` 不再报错，`api_key` 就是那个空串。Task 2 收敛掉了
    /// 原先 `Some(_) => bail!("API key is empty: ...")` 那一支，这里把新行为钉住。
    #[test]
    fn explicit_empty_api_key_is_accepted_as_a_full_package() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
            api_key: Some(String::new()),
            workdir: Some(PathBuf::from(".")),
            ..Default::default()
        };

        let cfg = RuntimeConfig::load(&overrides).expect("an explicit empty key is a full package");
        assert_eq!(cfg.api_key, "");
        assert!(cfg.model_injected);
    }

    #[test]
    fn load_env_files_loads_the_single_path() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        let temp = std::env::temp_dir().join(".env_test_single_path");
        let path = temp.join(".yi-agent/.env");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "MODEL_API_KEY=from-file\n").unwrap();

        env.remove("MODEL_API_KEY");
        load_env_files(&path);

        assert_eq!(std::env::var("MODEL_API_KEY").unwrap(), "from-file");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_env_files_missing_path_is_silent() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        env.remove("MODEL_API_KEY");
        load_env_files(std::path::Path::new("/nonexistent/yi-agent/.env"));
        assert_eq!(std::env::var("MODEL_API_KEY").ok(), None);
    }

    #[test]
    fn load_env_files_real_env_overrides_the_file() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = EnvVarGuard::new(["MODEL_API_KEY"]);
        let temp = std::env::temp_dir().join(".env_test_real_env");
        let path = temp.join(".yi-agent/.env");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "MODEL_API_KEY=from-file\n").unwrap();

        env.set("MODEL_API_KEY", "from-real-env");
        load_env_files(&path);

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
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR", "MODEL_API_KEY", "YI_AGENT_PROVIDER"]);
        env.remove("YI_AGENT_WORKDIR");
        env.remove("YI_AGENT_PROVIDER");
        env.remove("MODEL_API_KEY");

        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR", "YI_AGENT_PROVIDER", "MODEL_API_KEY"]);
        // 设置空字符串环境变量,应该 fallback 到 current_dir 而非变成空路径
        env.set("YI_AGENT_WORKDIR", "");
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
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
            provider: Some("anthropic".into()),
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
            provider: Some("anthropic".into()),
            api_key: Some("sk-test".into()),
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_resident_subagents, 8);
    }

    #[test]
    fn max_direct_children_defaults_to_eight_and_reads_the_env() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            provider: Some("anthropic".into()),
            api_key: Some("sk-test".into()),
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_direct_children, DIRECT_CHILDREN_DEFAULT);
        assert_eq!(DIRECT_CHILDREN_DEFAULT, 8);

        env.set("YI_AGENT_MAX_DIRECT_CHILDREN", "3");
        let config = RuntimeConfig::load(&overrides).expect("config loads");
        assert_eq!(config.max_direct_children, 3);
    }

    #[test]
    fn the_config_default_matches_the_core_direct_child_default() {
        assert_eq!(
            DIRECT_CHILDREN_DEFAULT,
            yi_agent_core::subagent::supervisor::MAX_DIRECT_CHILDREN as u16,
            "the runtime config default and the core supervisor default must not diverge"
        );
    }
}
