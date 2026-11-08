//! 配置加载：调用方覆盖项 + 真实进程环境变量 + `settings.json` > 默认值。
//!
//! 本模块由 CLI 与 GUI(app-server)共用，因此不依赖 clap。CLI 负责把
//! `clap` 解析出的参数转换成 [`ConfigOverrides`]，再交给
//! [`RuntimeConfig::load_lenient`]。
//!
//! **不再有 `.env`。** 过去 `load_inner` 会 `dotenvy::from_path` 把 `~/.yi-agent/.env`
//! 注入进程环境，再剔除模型键——`load_env_files` / `prune_dotenv_model_keys`
//! 已随之删除。`ModelEnvSnapshot` 保留：它快照的是**真实进程环境变量**，
//! 与 `.env` 无关，是第二层的实现。
//! 运行参数现在读 `settings.json`（见 [`crate::settings`]），且它是**第三层**：
//! 覆盖 > 真实环境变量 > settings.json > 默认值。
//!
//! 唯一还认得 `.env` 的是 [`crate::migrate`] 的存量迁移层，它计划在
//! **`2027-01-01`** 整体退役；删除清单见那个模块的文档。

use std::path::PathBuf;

use anyhow::{Result, bail};
use yi_agent_core::ApiStyle;

use crate::live_settings::sources::{EffectiveRuntimeSettings, FrozenRuntimeSources, ValueSource};

/// 模型键清单：只认「真实进程环境 + CLI 覆盖」这一条整包注入路径。
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

/// 在任何配置层被合入**之前**对模型键做一次进程环境快照。
///
/// `load_inner` 是这里唯一读取模型键的地方，快照保证读到的是用户 shell 真正
/// 导出的值，而不是某个中间层刚写进去的值。
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

/// Default resident subagent capacity, mirrored by
/// `AgentWorkerFactory::max_resident_subagents`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const RESIDENT_SUBAGENTS_DEFAULT: u16 = 64;

/// Default direct-child limit, mirrored by
/// `AgentWorkerFactory::max_direct_children`'s default so a factory that does
/// not override it and a config that does not set it agree.
pub const DIRECT_CHILDREN_DEFAULT: u16 = 8;

/// 一次对话里 agent 最多执行的轮数。
pub const MAX_TURNS_DEFAULT: u32 = 200;

/// 触发自动压缩的上下文占比（百分数）。
pub const COMPACT_RATIO_DEFAULT: u32 = 80;

/// 压缩时保留的真实用户输入 token 数。
pub const COMPACT_USER_BUDGET_DEFAULT: usize = 20_000;

/// 压缩时保留的完整工具交互 token 数。
pub const COMPACT_TOOL_BUDGET_DEFAULT: usize = 12_000;

/// 注入系统提示词的技能目录字节预算。
pub const SKILLS_CATALOG_BUDGET_DEFAULT: usize = 8_192;

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
    /// 是否发送终端通知（`tui.notifications`，默认 true）。
    pub notifications: bool,
    /// `auto` / `osc9` / `bel`（`tui.notification_method`，默认 `auto`）。
    pub notification_method: String,
    /// `unfocused` / `always`（`tui.notification_condition`，默认 `unfocused`）。
    pub notification_condition: String,
    /// 是否把项目名与运行状态写进终端标签页标题
    /// （`tui.terminal_title`，默认 true）。
    pub terminal_title: bool,
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

/// 从一次 `resolve` 里取一个数字字段。`resolve` 保证每个 FIELDS 键都存在；
/// 这里返回 0 只是不可能发生的形状错误，不做第二套默认。
fn resolved_u64(resolved: &EffectiveRuntimeSettings, key: &str) -> u64 {
    resolved
        .values
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// 解析 `YI_AGENT_SANDBOX` 的值。
///
/// 语义等价于 clap 的 `ValueEnum::from_str(value, true)`(kebab-case 变体名、
/// 大小写不敏感);runtime crate 不依赖 clap,故在此手工映射。
pub(crate) fn parse_sandbox_mode(value: &str) -> Option<yi_agent_tools::SandboxMode> {
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

/// 解析全局配置目录下的 `.env` 路径：`<instance_home>/.env`
///
/// 注意：产品已不再读取 `.env`。保留这个路径解析是因为启动迁移
/// （[`crate::migrate`]）仍要找到旧的 `.env` 把它改名作废——迁移必须先能
/// 定位它，才谈得上「不再被读取」。
///
/// **退役计划 `2027-01-01`**：只服务于上述迁移，随迁移层整体删除。
/// 完整删除清单见 [`crate::migrate`] 的模块文档。
pub fn resolve_global_env_path() -> Option<PathBuf> {
    yi_agent_store::paths::home_join(".env")
}

/// Resolve the effective workdir without loading provider configuration.
///
/// Priority is the explicit workdir override, then the current directory.
/// `YI_AGENT_WORKDIR` is intentionally gone: the working directory is a
/// session/project property, decided by `--workdir`, the launch directory, or
/// the project the desktop app selected. A global env var fighting the
/// "last opened project" was the exact conflict this removal fixes.
pub fn resolve_workdir(overrides: &ConfigOverrides) -> Result<PathBuf> {
    let workdir = overrides
        .workdir
        .clone()
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

    /// 从覆盖项 + 环境变量加载配置；**允许 API key 缺失**（回空串）。
    ///
    /// 优先级:覆盖项 > 真实进程环境变量 > `settings.json` > 默认值。
    /// `settings.json` 只有一处:`<instance_home>/settings.json`,不主动创建目录。
    ///
    /// 这是**唯一**的加载入口。曾经还有一个严格版 `load`（缺 key 即 bail），
    /// 它和本函数对「这台机器配好了没有」给出相反答案：模型只在 `models.json`
    /// 里的机器，严格版读不到 key 就报错，宽松版照常返回。两个入口并存时，
    /// 启动路径选错一个就把「没有 key」变成启动失败——2026-10-29 的
    /// `Error: provider turn admission failed: API key required` 正是这样来的。
    ///
    /// 因此「能不能发起对话」不再由加载器回答，只由 [`crate::models::is_usable`]
    /// 回答，且必须作用在 `resolve_effective` **之后**（否则清单里的 key 还没
    /// 合入配置，只会误判）。加载器只负责把能读到的值读全。
    ///
    /// 缺 key 时返回空串而不是报错，理由不变：桌面侧车在新机器上必须能起来才能
    /// 提供引导 RPC，CLI 要靠 `unconfigured_guidance` 给出中文指引。
    pub fn load_lenient(overrides: &ConfigOverrides) -> Result<Self> {
        Self::load_with_sources(overrides, crate::settings::settings_path())
            .map(|(config, _, _)| config)
    }

    /// 与 [`RuntimeConfig::load_lenient`] 同一份装配，但额外交出本进程的
    /// [`RuntimeSettingsBinding`]（共享设置源 + 初始装配冻结的来源表）。
    ///
    /// `settings_path` 必须是稳定的绝对路径：调用方一旦装配完就不再随 cwd/env
    /// 变化。测试可以指向不同的临时路径，因此不同实例绝对隔离。
    ///
    /// 顺序固定：先 capture 真实来源，再有界取设置快照，最后做模型 / 其他装配。
    /// 快照只走 `LiveSettings` 的有界读取；本入口与 `load_lenient` 都不再调用
    /// 无界的 `settings::load_from`。
    pub fn load_lenient_with_binding(
        overrides: &ConfigOverrides,
        settings_path: &std::path::Path,
    ) -> Result<(Self, crate::live_settings::binding::RuntimeSettingsBinding)> {
        let (config, sources, live) =
            Self::load_with_sources(overrides, settings_path.to_path_buf())?;
        let binding = crate::live_settings::binding::RuntimeSettingsBinding { live, sources };
        Ok((config, binding))
    }

    /// 唯一的初始装配入口：`load_lenient` 与 `load_lenient_with_binding` 共用。
    fn load_with_sources(
        overrides: &ConfigOverrides,
        settings_path: PathBuf,
    ) -> Result<(
        Self,
        std::sync::Arc<FrozenRuntimeSources>,
        std::sync::Arc<crate::live_settings::LiveSettings>,
    )> {
        // 快照必须早于任何环境注入（见 `ModelEnvSnapshot` 的文档）。
        let model_env = ModelEnvSnapshot::take();
        // 固定来源必须在合并任何 settings **之前**抽取：初次合并的结果不是来源。
        // `capture` 内部已 `validate`（拒绝模型/凭据键与伪固定来源）。
        let sources = std::sync::Arc::new(FrozenRuntimeSources::capture(overrides)?);
        // 第三层：有界读取 settings.json 快照；缺文件是健康默认，坏文件是明确
        // 的启动期回退，都不阻断启动。
        let live = crate::live_settings::LiveSettings::shared(settings_path);
        let snapshot = live.refresh();
        let settings = snapshot.settings.clone();
        // 与 `resolve` 共用同一套逐字段解析：这里不再各抄一份优先级。
        let resolved = sources.resolve(&settings);

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

        let provider = injected_provider.unwrap_or_else(|| "openai".to_string());
        let default_api_url = match provider.as_str() {
            "openai" => "https://api.openai.com/v1",
            _ => "https://api.anthropic.com",
        };
        let api_url = injected_url.unwrap_or_else(|| default_api_url.to_string());
        // 模型名没有客观默认值:猜一个名字发出去只会得到 404,而且这个猜测会一路
        // 传播到子 agent(`subagent_model` -> `default_model` -> `cfg`)。空串是
        // "未配置"的唯一诚实表示,由 RPC 入口与 CLI 给出可操作的指引。
        let model = injected_model.unwrap_or_default();
        // 缺 key 回空串，不在此处报错：「能不能发起对话」只由
        // `models::is_usable` 在 `resolve_effective` 之后回答（见
        // [`RuntimeConfig::load_lenient`] 的文档）。加载器报错的旧行为会让
        // 「模型只在 models.json」的机器在启动路径上直接失败。
        let api_key = injected_key.unwrap_or_default();

        let max_turns = resolved_u64(&resolved, "agent.max_turns") as u32;
        let max_resident_subagents = resolved_u64(&resolved, "agent.max_resident_subagents") as u16;
        let max_direct_children = resolved_u64(&resolved, "agent.max_direct_children") as u16;

        let workdir = resolve_workdir(overrides)?;

        let system_prompt = resolved
            .values
            .get("agent.system_prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let model_context_length = overrides.model_context_length.or_else(|| {
            model_env
                .get("YI_AGENT_MODEL_CONTEXT_LENGTH")
                .and_then(|s| s.parse().ok())
        });

        let compact_ratio = resolved_u64(&resolved, "compact.ratio") as u32;
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

        let compact_user_budget_tokens =
            resolved_u64(&resolved, "compact.user_budget_tokens") as usize;
        // 预算为零的既有报错保留：它与「来源是哪一层」无关。
        if compact_user_budget_tokens == 0 {
            bail!("compact user budget tokens must be greater than zero");
        }
        let compact_tool_budget_tokens =
            resolved_u64(&resolved, "compact.tool_budget_tokens") as usize;

        let yolo = overrides.yolo
            || overrides.skip_permissions
            || std::env::var("YI_AGENT_YOLO")
                .map(|v| v == "true")
                .unwrap_or(false);

        // 只有 override / 真实 env 算「显式指定启动姿态」，永不被运行期提权。
        // settings.json 的沙箱是长住偏好，仍可提权（`promotable`）。
        let sandbox_source = resolved
            .sources
            .get("startup.sandbox")
            .copied()
            .unwrap_or(ValueSource::Default);
        let explicit_sandbox = matches!(
            sandbox_source,
            ValueSource::Override | ValueSource::Environment
        );
        let resolved_sandbox = resolved
            .values
            .get("startup.sandbox")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_sandbox_mode)
            .unwrap_or_default();
        // `promotable` 只在「启动时非 yolo」时才有意义(桌面端运行期切换场景):
        // 若启动时 yolo 已打开(CLI `--yolo`、`--dangerously-skip-permissions` 或
        // `YI_AGENT_YOLO`),`sandbox` 已经反映了今天的行为,运行期不应再提权。
        let (sandbox, sandbox_promotable) = if explicit_sandbox {
            (resolved_sandbox, false)
        } else if overrides.yolo {
            // 无显式沙箱且 CLI 要求 yolo：与旧行为一致地直接给全权。
            (yi_agent_tools::SandboxMode::DangerFullAccess, !yolo)
        } else {
            (resolved_sandbox, !yolo)
        };

        let sandbox_writable_roots = overrides.sandbox_writable_roots.clone();

        // settings.json 里的预算不算「显式指定」：只有 CLI / 真实 env 才算。
        // 判据是「**提供了**这个来源」，与数字能否解析无关：旧实现里
        // `YI_AGENT_SKILLS_CATALOG_BUDGET=abc` 仍算显式（值回落默认，但不再静默
        // 截断）。技能目录超预算时仍应询问（交互终端下）。
        let skills_catalog_budget_explicit = overrides.skills_catalog_budget.is_some()
            || std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
                .ok()
                .is_some_and(|value| !value.is_empty());
        let skills_catalog_budget = resolved_u64(&resolved, "agent.skills_catalog_budget") as usize;

        // 存储侧是字符串（见 settings.rs 的 TuiSettings 说明），这里解析成
        // bool；空串与非法值已在归一化里清成 None，故只需处理默认。
        let notifications = !matches!(
            resolved
                .values
                .get("tui.notifications")
                .and_then(serde_json::Value::as_str),
            Some("false")
        );
        let notification_method = resolved
            .values
            .get("tui.notification_method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("auto")
            .to_string();
        let notification_condition = resolved
            .values
            .get("tui.notification_condition")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unfocused")
            .to_string();
        // 与 `notifications` 同理：存储侧是字符串，这里解析成 bool。
        // 用 `!matches!(.., Some("false"))` 而不是 `== Some("true")`：
        // 后者会让「键缺失」（最常见情况）得到 false，把默认的 true 变成
        // false。
        let terminal_title = !matches!(
            resolved
                .values
                .get("tui.terminal_title")
                .and_then(serde_json::Value::as_str),
            Some("false")
        );

        Ok((
            RuntimeConfig {
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
                notifications,
                notification_method,
                notification_condition,
                terminal_title,
            },
            sources,
            live,
        ))
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
/// A legal [`RuntimeConfig`] with arbitrary values, for tests outside this module.
///
/// Same purpose as [`sample_config`] but reachable from other modules' tests (for
/// example the side-conversation assembly tests), which need a base config without
/// asserting anything about loading semantics.
#[cfg(test)]
pub(crate) fn sample_config_for_side_test() -> RuntimeConfig {
    sample_config()
}

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
        notifications: true,
        notification_method: "auto".to_string(),
        notification_condition: "unfocused".to_string(),
        terminal_title: true,
    }
}

/// 测试支持：进程环境变量是全局状态，凡会 `set_var` 的测试都必须共用这一把锁。
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    /// 测试用互斥锁:涉及环境变量的测试必须串行执行,避免并行干扰。
    ///
    /// `cargo nextest` 下每个测试独立进程，环境变量天然不共享，这把锁退化为
    /// 无害的空操作；`cargo test` 下它仍是必需的。两种运行器都必须保留。
    ///
    /// 它与 `live_settings::sources` 的 env 测试共用同一把（定义见
    /// [`super::test_support`]），否则两个模块各锁各的等于没锁。
    use super::test_support::ENV_TEST_MUTEX;

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
            "YI_AGENT_HOME",
        ]);
        // 测试必须把 HOME 指到一个不存在的位置：否则会读到开发者机器上真实的
        // `~/.yi-agent/settings.json`，其中的非模型键会污染默认值断言。
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
            "YI_AGENT_HOME",
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

    /// `load_lenient_with_binding`：同一份装配、显式路径、冻结来源只含固定层。
    #[test]
    fn with_binding_captures_only_fixed_sources_and_shares_one_live_source() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":321,"max_direct_children":9}}"#);
        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_MAX_TURNS", "654");
        let settings_path = instance.dir.path().join(".yi-agent/settings.json");

        let overrides = ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            max_turns: Some(999),
            ..Default::default()
        };
        let (config, binding) =
            RuntimeConfig::load_lenient_with_binding(&overrides, &settings_path).unwrap();

        assert_eq!(config.max_turns, 999);
        assert_eq!(config.max_direct_children, 9);
        assert_eq!(
            binding.sources.frozen("agent.max_turns").map(|f| f.source),
            Some(ValueSource::Override),
            "an explicit CLI override is frozen"
        );
        assert!(
            binding
                .sources
                .frozen("agent.max_direct_children")
                .is_none(),
            "a value that only came from settings.json must never be frozen as an override"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                &binding.live,
                &crate::live_settings::LiveSettings::shared(settings_path)
            ),
            "the binding must hand out the process-wide shared source"
        );

        // `load_lenient` 与绑定入口使用同一份装配：同一环境、同一文件必须同结果。
        let plain = RuntimeConfig::load_lenient(&overrides).unwrap();
        assert_eq!(plain.max_turns, config.max_turns);
        assert_eq!(plain.max_direct_children, config.max_direct_children);
        assert_eq!(plain.sandbox, config.sandbox);
        assert_eq!(plain.sandbox_promotable, config.sandbox_promotable);
        assert_eq!(plain.notifications, config.notifications);
        assert_eq!(plain.terminal_title, config.terminal_title);
    }

    /// 「显式预算」判据是**来源存在**，不是数字能否解析。
    ///
    /// 旧实现里 `YI_AGENT_SKILLS_CATALOG_BUDGET=not-a-number` 仍把
    /// `skills_catalog_budget_explicit` 置真（值回落默认，但不会静默截断）；
    /// 若改判为「解析成功才算」，交互终端会从「询问」悄悄变成「静默截断」。
    #[test]
    fn an_unparsable_budget_env_still_counts_as_explicit() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 实例先在隔离环境里建好，再设 env：后建的 guard 会清掉这类键。
        let instance = IsolatedInstance::new();
        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_SKILLS_CATALOG_BUDGET", "not-a-number");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.skills_catalog_budget, SKILLS_CATALOG_BUDGET_DEFAULT);
        assert!(
            cfg.skills_catalog_budget_explicit,
            "an env var that was provided is explicit even if unparsable"
        );
    }

    /// 绑定入口走有界读取，绝不转调无界的 `load_from`。
    #[test]
    fn with_binding_never_calls_the_unbounded_loader() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":42}}"#);
        let settings_path = instance.dir.path().join(".yi-agent/settings.json");

        crate::settings::reset_load_from_calls();
        let (config, _binding) = RuntimeConfig::load_lenient_with_binding(
            &ConfigOverrides {
                workdir: Some(instance.dir.path().to_path_buf()),
                ..Default::default()
            },
            &settings_path,
        )
        .unwrap();

        assert_eq!(crate::settings::load_from_calls(), 0);
        assert_eq!(config.max_turns, 42);
    }

    /// 显式路径：同一台机器上两个不同实例文件互不影响。
    #[test]
    fn with_binding_is_isolated_by_the_explicit_absolute_path() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        std::fs::write(
            first.path().join("settings.json"),
            r#"{"compact":{"ratio":40}}"#,
        )
        .unwrap();
        std::fs::write(
            second.path().join("settings.json"),
            r#"{"compact":{"ratio":60}}"#,
        )
        .unwrap();

        let (a, _) = RuntimeConfig::load_lenient_with_binding(
            &ConfigOverrides {
                workdir: Some(first.path().to_path_buf()),
                ..Default::default()
            },
            &first.path().join("settings.json"),
        )
        .unwrap();
        let (b, _) = RuntimeConfig::load_lenient_with_binding(
            &ConfigOverrides {
                workdir: Some(second.path().to_path_buf()),
                ..Default::default()
            },
            &second.path().join("settings.json"),
        )
        .unwrap();

        assert_eq!(a.compact_ratio, 40);
        assert_eq!(b.compact_ratio, 60);
        assert_ne!(a.compact_threshold, b.compact_threshold);
    }

    /// 保存后的值在不重启的情况下被「下一次读取」看见（同一绑定）。
    ///
    /// 这是 Task 3 能提供的部分：绑定 + 共享源 + gate 内发布。真正把新值交给
    /// 运行中的会话是后续任务的事。
    #[test]
    fn a_commit_on_the_binding_is_visible_to_the_next_snapshot() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let instance = IsolatedInstance::new();
        let settings_path = instance.dir.path().join(".yi-agent/settings.json");

        let (_, binding) = RuntimeConfig::load_lenient_with_binding(
            &ConfigOverrides {
                workdir: Some(instance.dir.path().to_path_buf()),
                ..Default::default()
            },
            &settings_path,
        )
        .unwrap();
        assert_eq!(
            binding.live.refresh().settings.compact.ratio,
            None,
            "no file yet: healthy defaults"
        );

        binding
            .live
            .commit(
                &serde_json::json!({"compact.ratio": 55}),
                crate::settings::store::PatchMode::ReplaceProvided,
            )
            .unwrap();

        let snapshot = binding.live.refresh();
        assert_eq!(snapshot.settings.compact.ratio, Some(55));
        let effective = binding.sources.resolve(&snapshot.settings);
        assert_eq!(effective.values["compact.ratio"], serde_json::json!(55));
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
    fn defaults_are_constants_not_scattered_literals() {
        // 提常量之前，这几行会编译失败——这正是本测试的用途：它钉住
        // 「默认值只有一个名字」，防止有人再往函数体里写裸字面量。
        assert_eq!(MAX_TURNS_DEFAULT, 200);
        assert_eq!(COMPACT_RATIO_DEFAULT, 80);
        assert_eq!(COMPACT_USER_BUDGET_DEFAULT, 20_000);
        assert_eq!(COMPACT_TOOL_BUDGET_DEFAULT, 12_000);
        assert_eq!(SKILLS_CATALOG_BUDGET_DEFAULT, 8_192);
    }

    /// `settings.json` 的默认值文案必须与这里同源。两个常量组一旦漂移，这条测试先红。
    #[test]
    fn settings_metadata_defaults_track_these_constants() {
        use crate::settings::find_spec;
        use crate::settings::{
            POWER_IDLE_GRACE_MINUTES_DEFAULT, POWER_REMOTE_TOUCH_MINUTES_DEFAULT,
        };
        let cases: &[(&str, String)] = &[
            ("agent.max_turns", MAX_TURNS_DEFAULT.to_string()),
            (
                "agent.max_direct_children",
                DIRECT_CHILDREN_DEFAULT.to_string(),
            ),
            (
                "agent.max_resident_subagents",
                RESIDENT_SUBAGENTS_DEFAULT.to_string(),
            ),
            (
                "agent.skills_catalog_budget",
                SKILLS_CATALOG_BUDGET_DEFAULT.to_string(),
            ),
            ("compact.ratio", COMPACT_RATIO_DEFAULT.to_string()),
            (
                "compact.user_budget_tokens",
                COMPACT_USER_BUDGET_DEFAULT.to_string(),
            ),
            (
                "compact.tool_budget_tokens",
                COMPACT_TOOL_BUDGET_DEFAULT.to_string(),
            ),
            (
                "power.idle_grace_minutes",
                POWER_IDLE_GRACE_MINUTES_DEFAULT.to_string(),
            ),
            (
                "power.remote_touch_minutes",
                POWER_REMOTE_TOUCH_MINUTES_DEFAULT.to_string(),
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(
                find_spec(key).unwrap().default_display,
                *expected,
                "default_display for {key} drifted from the runtime constant"
            );
        }
    }

    /// 设置一个白名单环境变量，并让它指向隔离的实例目录。
    struct IsolatedInstance {
        _env: EnvVarGuard,
        dir: tempfile::TempDir,
    }

    impl IsolatedInstance {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let mut env = isolated_config_env();
            env.set("YI_AGENT_HOME", dir.path().join(".yi-agent"));
            Self { _env: env, dir }
        }

        fn write_settings(&self, json: &str) {
            let home = self.dir.path().join(".yi-agent");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("settings.json"), json).unwrap();
        }
    }

    #[test]
    fn tui_notification_settings_reach_runtime_config() {
        // 端到端钉子：写进 settings.json 的三个值必须真的出现在 RuntimeConfig
        // 上。缺了这条，字段名写错（比如 `tui.notification`）在编译期毫无反应，
        // 用户改了配置却永远拿默认值。
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(
            r#"{"tui":{"notifications":"false","notification_method":"bel","notification_condition":"always"}}"#,
        );

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(!cfg.notifications);
        assert_eq!(cfg.notification_method, "bel");
        assert_eq!(cfg.notification_condition, "always");
    }

    #[test]
    fn tui_notification_defaults_apply_when_absent() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(cfg.notifications, "通知默认开启");
        assert_eq!(cfg.notification_method, "auto");
        assert_eq!(cfg.notification_condition, "unfocused");
    }

    #[test]
    fn tui_terminal_title_reaches_runtime_config() {
        // 端到端钉子：写进 settings.json 的值必须真的出现在 RuntimeConfig
        // 上。缺了这条，字段名写错（比如 `tui.terminaltitle`）在编译期毫无
        // 反应，用户改了配置却永远拿默认值。
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"tui":{"terminal_title":"false"}}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(!cfg.terminal_title, "explicit false must reach the config");
    }

    #[test]
    fn tui_terminal_title_defaults_apply_when_absent() {
        // 钉住 `!matches!(.., Some("false"))` 那行：键缺失时必须是 true。
        // 若有人写成 `== Some("true")`，这条会红。
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert!(cfg.terminal_title, "标题默认开启");
    }

    #[test]
    fn settings_json_supplies_values_when_nothing_else_does() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(
            r#"{"agent":{"max_turns":321},"compact":{"ratio":55},"startup":{"sandbox":"read-only"}}"#,
        );

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 321);
        assert_eq!(cfg.compact_ratio, 55);
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::ReadOnly);
        // 配置出来的沙箱不是「显式指定」，故仍可运行期提权。
        assert!(
            cfg.sandbox_promotable,
            "a sandbox from settings.json must stay promotable"
        );
    }

    /// 真实进程环境变量压过 settings.json——这是 spec 定的第二/第三层。
    #[test]
    fn real_env_beats_settings_json() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":321}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_MAX_TURNS", "654");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 654);
    }

    /// CLI 覆盖压过两者。
    #[test]
    fn override_beats_env_and_settings() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":321}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_MAX_TURNS", "654");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            max_turns: Some(999),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 999);
    }

    #[test]
    fn missing_settings_json_falls_back_to_defaults() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, MAX_TURNS_DEFAULT);
        assert_eq!(cfg.compact_ratio, COMPACT_RATIO_DEFAULT);
        assert_eq!(cfg.skills_catalog_budget, SKILLS_CATALOG_BUDGET_DEFAULT);
    }

    #[test]
    fn corrupt_settings_json_does_not_block_startup() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings("{ not json");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .expect("a corrupt settings.json must not block startup");

        assert_eq!(cfg.max_turns, MAX_TURNS_DEFAULT);
    }

    /// 空白串等同于未设置，不能让 `system_prompt` 变成空串而不是 None。
    #[test]
    fn blank_settings_string_is_unset() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"system_prompt":"   "}}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.system_prompt, None);
    }

    /// `YI_AGENT_WORKDIR` 已下线：设了也不影响工作目录。
    #[test]
    fn workdir_env_var_no_longer_overrides() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let here = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();

        let mut env = isolated_config_env();
        env.set("YI_AGENT_WORKDIR", elsewhere.path());

        let resolved = resolve_workdir(&ConfigOverrides {
            workdir: Some(here.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved, here.path());
        assert_ne!(resolved, elsewhere.path());
    }

    /// settings.json 里的 budget 不算「显式指定」：技能目录超预算时仍应询问
    /// （交互终端下），而不是静默截断。
    #[test]
    fn settings_budget_is_not_explicit() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"skills_catalog_budget":4096}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.remove("YI_AGENT_SKILLS_CATALOG_BUDGET");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.skills_catalog_budget, 4096);
        assert!(
            !cfg.skills_catalog_budget_explicit,
            "settings.json is a long-lived preference, not an explicit per-launch choice"
        );
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
    fn resolve_workdir_ignores_the_environment_value() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let here = tempfile::TempDir::new().expect("tempdir");
        let elsewhere = tempfile::TempDir::new().expect("tempdir");
        let mut env = EnvVarGuard::new(["YI_AGENT_WORKDIR"]);
        env.set(
            "YI_AGENT_WORKDIR",
            elsewhere.path().to_str().expect("UTF-8 temporary path"),
        );
        let _cwd = CurrentDirGuard::change_to(here.path());

        // `YI_AGENT_WORKDIR` 已下线：没有 workdir 覆盖时只能落回启动目录。
        // macOS 的 `/var` 是 `/private/var` 的符号链接，两边都规范化后再比。
        assert_eq!(
            resolve_workdir(&ConfigOverrides::default())
                .expect("resolve workdir")
                .canonicalize()
                .unwrap(),
            here.path().canonicalize().unwrap()
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let result = RuntimeConfig::load_lenient(&overrides);
        assert!(result.is_err());
    }

    /// 真正覆盖「未注入 provider 时的回退」。
    ///
    /// 旧版本这个位置的测试名叫 `load_defaults_provider_to_anthropic`，却传了
    /// `provider: Some("anthropic")` —— 断言的是显式配置，回退路径其实从未被测。
    /// 这里全部覆盖项留空，走真正的默认分支。
    #[test]
    fn defaults_provider_to_openai_when_nothing_is_configured() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let overrides = ConfigOverrides {
            provider: None,
            api_url: None,
            api_key: None,
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "", "no default model name may be invented");
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "", "no default model name may be invented");
    }

    /// 非模型键来自 `settings.json`，模型键则只认真实进程环境变量。
    ///
    /// `max_turns` 取 settings.json 的值；`api_key` 在没有任何环境变量时保持空串，
    /// 不会被某个文件悄悄填上。
    #[test]
    fn settings_file_supplies_non_model_keys() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut env = isolated_config_env();
        let home = tempfile::TempDir::new().unwrap();
        env.set("HOME", home.path());
        let yi_agent_dir = home.path().join(".yi-agent");
        std::fs::create_dir_all(&yi_agent_dir).unwrap();
        std::fs::write(
            yi_agent_dir.join("settings.json"),
            r#"{"agent":{"max_turns":123}}"#,
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
        assert_eq!(
            config.api_key, "",
            "the model key must not come from a file"
        );
        assert!(!config.model_injected);
        assert_eq!(
            config.max_turns, 123,
            "non-model keys come from settings.json"
        );
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
    #[test]
    fn load_does_not_create_local_yi_agent_dir_in_fallback_mode() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 加载配置不应在启动目录里造出 `.yi-agent/`：项目级文件只在真正写入时才建。
        let temp = std::env::temp_dir().join("yi-agent-test-auto-create-local");
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
        let result = RuntimeConfig::load_lenient(&overrides);
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
            RuntimeConfig::load_lenient(&overrides).unwrap().sandbox,
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
            RuntimeConfig::load_lenient(&overrides).unwrap().sandbox,
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
        let cfg = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let cfg = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let cfg = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let cfg = RuntimeConfig::load_lenient(&overrides).unwrap();
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::DangerFullAccess);
        assert!(!cfg.sandbox_promotable);
    }

    #[test]
    fn sandbox_promotable_true_by_default() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let cfg = RuntimeConfig::load_lenient(&test_overrides()).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&test_overrides()).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).unwrap();
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
        let config = RuntimeConfig::load_lenient(&overrides).expect("config loads");
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
        let config = RuntimeConfig::load_lenient(&overrides).expect("config loads");
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
        let config = RuntimeConfig::load_lenient(&overrides).expect("config loads");
        assert_eq!(config.max_direct_children, DIRECT_CHILDREN_DEFAULT);
        assert_eq!(DIRECT_CHILDREN_DEFAULT, 8);

        env.set("YI_AGENT_MAX_DIRECT_CHILDREN", "3");
        let config = RuntimeConfig::load_lenient(&overrides).expect("config loads");
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
