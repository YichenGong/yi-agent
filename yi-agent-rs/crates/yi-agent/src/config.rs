//! CLI 配置适配层:把 clap 解析出的 [`Cli`] 转成共享 crate 的覆盖项。
//!
//! 实际的配置加载语义(覆盖项 + 环境变量 > 默认值)由
//! `yi_agent_runtime::config` 拥有,CLI 与未来的 GUI 共用同一实现。
//! 本模块只负责参数解析与类型转换。

use std::path::PathBuf;

use anyhow::Result;

/// CLI 侧仍以 `Config` 之名引用运行时配置;实际类型来自共享 crate。
pub type Config = yi_agent_runtime::config::RuntimeConfig;

/// clap CLI 参数定义。
#[derive(clap::Parser, Debug)]
#[command(name = "yi-agent", version = crate::build_info::CLAP_VERSION, about = "Interactive AI agent CLI")]
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

    /// Retained real user context budget during compact.
    #[arg(long)]
    pub compact_user_budget_tokens: Option<usize>,

    /// Retained complete tool context budget during compact; zero disables it.
    #[arg(long)]
    pub compact_tool_budget_tokens: Option<usize>,

    /// Deprecated: ignored in favor of token budgets.
    #[arg(long, hide = true)]
    pub compact_keep_turns: Option<u32>,

    /// Skip permission prompts (except blacklisted commands)
    #[arg(long)]
    pub yolo: bool,

    /// Process sandbox: read-only, workspace-write, or danger-full-access.
    #[arg(long, value_enum)]
    pub sandbox: Option<yi_agent_tools::SandboxMode>,

    /// Additional directory a workspace-write sandbox may modify (repeatable).
    #[arg(long = "sandbox-writable-root")]
    pub sandbox_writable_roots: Vec<PathBuf>,

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

        /// Enable local subagent delegation for this headless run.
        #[arg(long)]
        subagents: bool,

        /// 恢复一个既有会话再跑。跑完的内容会追加进同一份日志。
        ///
        /// 支持完整 id 或唯一前缀。这是 resume 链路在 TUI 之外唯一的
        /// 可自动化载体（TUI 是全屏应用，测不了）。
        #[arg(long)]
        resume: Option<String>,
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
    /// Inspect or stop the manually started local subagent runtime daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// List daemon-owned agent tasks.
    Agents {
        /// Limit to a project path when the daemon supports project filtering.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Include terminal tasks.
        #[arg(long)]
        all: bool,
    },
    /// Inspect or control one daemon-owned agent task.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Preview or confirm a natural-language Cron schedule.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Run the JSON-RPC app-server. Used by the desktop GUI sidecar and, over
    /// `ws://`, by network clients.
    AppServer {
        /// Listen transport: `stdio://` (default), `ws://host:port`, or
        /// `relay://wss://host/connect?session=<id>`.
        ///
        /// The ws transport authenticates every client with a paired device
        /// token (unauthenticated clients are closed with ws code 4401).
        #[arg(long, default_value = "stdio://")]
        listen: String,
        /// Compose stdio with a reverse WSS relay: one session, both clients.
        ///
        /// Takes the relay's computer-side endpoint, e.g.
        /// `wss://relay.example/connect?session=<id>`. The app-server keeps
        /// serving the stdio client (the desktop GUI) and *also* dials the relay
        /// outbound from a loopback ws server on 127.0.0.1:0, so the phone joins
        /// the very same session (same `serve()` and hub). For the pure-relay
        /// form, with no stdio client, use `--listen relay://<same url>` instead.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Generate a shell completion script on stdout (bash, zsh, fish, powershell).
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Pairing helpers: mint a one-time code, list paired devices, or revoke one.
    ///
    /// Reads/writes the same `~/.yi-agent/pairing.json` + `devices.json` as the
    /// app-server, so a code minted here is redeemable by a `--relay`/`ws://`
    /// app-server on the same machine.
    Pair {
        #[command(subcommand)]
        action: PairAction,
    },
    /// 功能开关（仅 dev 构建可开；stable 构建一律读作关）。
    Flags {
        #[command(subcommand)]
        action: FlagsAction,
    },
    /// Internal runtime roles. Hidden: these are spawned by the guardian and the
    /// desktop shell with an inherited private bootstrap FD, never typed by a
    /// user. Without that FD each role refuses to run.
    #[command(hide = true, name = "internal-runtime")]
    InternalRuntime {
        #[command(subcommand)]
        action: InternalRuntimeAction,
    },
    /// 常驻值守：保证登记在册的项目各自有一个活着的 daemon。
    ///
    /// 通用子命令：它只读通用常驻登记，不认识任何插件。
    ///
    /// **注意（一版风险）**：这里曾经有个隐藏别名 `boards`，是为「已经装过旧
    /// launchd plist 的机器」留的过渡期兼容。别名已随看板面删除——若某台机器的
    /// plist 尚未重装（仍跑 `boards watch`），它会在下一次登录时解析失败。
    /// 复原办法是重装值守（`ui/settings/write` 开关一次，或重装 app）。
    Resident {
        #[command(subcommand)]
        action: ResidentAction,
    },
    /// 恢复一个既有会话并继续对话。
    Resume {
        /// 会话 id（或唯一前缀）。`--last` 时省略；与 `--last` 同用时
        /// 这个位置留给提示词（dispatch 层换位）。
        thread_id: Option<String>,
        /// 恢复同目录最近一条会话。
        #[arg(long)]
        last: bool,
        /// 恢复后立即发送的提示词。
        prompt: Option<String>,
    },
    /// 复制一份会话历史，在新会话里继续（源会话不受影响）。
    Fork {
        /// 源会话 id（或唯一前缀）。
        thread_id: String,
        /// fork 后立即发送的提示词。
        prompt: Option<String>,
    },
    /// 列出当前目录的会话。
    Threads,
    /// 受管插件：执行与管理。
    ///
    /// 与 `flags` / `resident` 同一条边界：这里不读插件语义，只按签名清单
    /// 声明的 effect 与注册表里的身份做判断。
    Plugins {
        #[command(subcommand)]
        action: PluginsAction,
    },
}

/// `yi-agent plugins` 的子命令。
///
/// 两族命令共用一层：`exec` 起进程，其余作用于实例状态。管理命令**不带**
/// 任何页面/业务权限——它们能改的是"装了什么、授权给谁"，不是"替用户
/// 在某个项目里干活"。
#[derive(clap::Subcommand, Debug)]
pub enum PluginsAction {
    /// 以受管入口运行插件程序（前台，继承终端）。
    ///
    /// `--` 之后原样作为插件 argv 传递，不经过 shell。
    Exec {
        /// 插件 id。
        #[arg(long)]
        plugin: String,
        /// 宿主生成的项目句柄。
        #[arg(long = "project-handle")]
        project_handle: String,
        /// 宿主生成的会话绑定文件。不接受页面上传的文件作为授权。
        #[arg(long = "binding-file")]
        binding_file: PathBuf,
        /// 插件 argv，原样透传。
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<std::ffi::OsString>,
    },
    /// 查询插件此刻是否处于维护屏障之下。
    ///
    /// 结论只来自磁盘上的生命周期日志，**不**来自任何缓存或通知：
    /// 一条丢失的通知不能变成放行。`unknown` 表示权威记录读不出来，
    /// 调用方必须把它当成"不能启动"。
    Maintenance {
        /// 插件 id。
        #[arg(long)]
        plugin: String,
    },
    /// 官方发布目录（T16 之前恒为未实现）。
    Catalog,
    /// 列出已安装与已授权的状态。
    List,
    /// 安装一个发布：`--release`，或本地 `--bundle` + `--metadata`。
    Install {
        /// 官方目录里的发布身份。
        #[arg(long)]
        release: Option<String>,
        /// 本地官方签名包。
        #[arg(long)]
        bundle: Option<PathBuf>,
        /// 与包配对的签名元数据。
        #[arg(long)]
        metadata: Option<PathBuf>,
    },
    /// 首次为某个项目启用。
    EnableProject {
        #[arg(long)]
        plugin: String,
        /// 项目路径（会被规范化）。
        #[arg(long)]
        project: String,
        /// 能力，可重复：read|write|launch|quota-write|resident。
        #[arg(long = "capability")]
        capability: Vec<String>,
        /// 覆盖宿主身份（测试与多实例诊断用）。
        #[arg(long = "host-id")]
        host_id: Option<String>,
    },
    /// 变更已有授权（能力集合整体替换，代次递增）。
    ChangeGrant {
        #[arg(long)]
        plugin: String,
        #[arg(long)]
        project: String,
        #[arg(long = "capability")]
        capability: Vec<String>,
        #[arg(long = "host-id")]
        host_id: Option<String>,
    },
    /// 停用某个项目（不等于卸载）。
    DisableProject {
        #[arg(long)]
        plugin: String,
        #[arg(long)]
        project: String,
    },
    /// 激活一个已安装版本（建立事务；推进由 daemon 负责）。
    Activate {
        #[arg(long)]
        plugin: String,
        #[arg(long)]
        version: String,
    },
    /// 回滚当前打开的事务。
    Rollback {
        #[arg(long)]
        plugin: String,
    },
    /// 查询事务进度。
    Transaction {
        #[arg(long)]
        plugin: String,
    },
    /// 取消一个尚未提交的事务。
    CancelTransaction {
        #[arg(long)]
        plugin: String,
    },
    /// 卸载（默认保留数据）。
    Uninstall {
        #[arg(long)]
        plugin: String,
    },
    /// 显式清除数据（独立于卸载，需要 `--yes` 二次确认）。
    ClearData {
        #[arg(long)]
        plugin: String,
        /// 这些项目的数据将被删除。必须显式列出——"全部"不是一个安全默认值。
        #[arg(long = "project", required = true)]
        projects: Vec<String>,
        /// 二次确认。没有它就什么都不删。
        #[arg(long)]
        yes: bool,
    },
    /// 只清引用安全的受管产物。
    Gc,
    /// 探测旧安装。
    InspectLegacy,
    /// 接管旧安装。
    AdoptLegacy,
}

/// Hidden runtime roles plus the one statically-reportable command.
#[derive(clap::Subcommand, Debug)]
pub enum InternalRuntimeAction {
    /// Print this executable's static artifact identity and exit.
    ///
    /// Deliberately does NOT connect to a runtime, read the database, or start
    /// anything: the build pipeline and the bundle manifest generator call it on a
    /// machine where no runtime exists.
    Identity {
        /// Emit JSON instead of a human-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Run one client's worker executor.
    ///
    /// Started by the daemon with an inherited control descriptor; never typed by
    /// a user. Refuses to run without that descriptor.
    Executor {
        /// The client this executor serves, as the daemon established it.
        #[arg(long)]
        owner: String,
        /// The model catalogue to resolve providers from. Defaults to this
        /// machine's catalogue.
        #[arg(long)]
        models: Option<std::path::PathBuf>,
    },
    #[command(hide = true)]
    /// Serve one runtime as its guardian.
    ///
    /// Started by a bring-up with an inherited bootstrap descriptor and the scope
    /// key in the environment; never typed by a user. Refuses to run without both:
    /// a guardian that accepted its scope from a command line could adopt another
    /// project's process groups.
    Guardian,
    #[command(hide = true)]
    /// Serve one runtime's business daemon, under its guardian.
    ///
    /// Same authorization as `guardian`; this is the *only* daemon start that
    /// carries the exit guarantees, which is why it is a private role and not a
    /// flag on the public `daemon serve`.
    Daemon,
}

#[derive(clap::Subcommand, Debug)]
pub enum FlagsAction {
    /// 列出全部注册的开关及生效值。
    List,
    /// 打开或关闭一个开关。
    Set {
        name: String,
        /// `on` 或 `off`
        value: String,
    },
    /// 打印一个开关的最终生效值，以及它来自哪一层。
    Status { name: String },
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ResidentAction {
    /// 值守循环：按登记表确保各项目 daemon 存活（由 launchd 常驻托管）。
    Watch {
        #[arg(long, default_value_t = 30)]
        interval_secs: u64,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum PairAction {
    /// Mint a one-time pairing code and print it (default).
    Code {
        /// Relay URL to embed in the printed QR code (optional). When omitted,
        /// output is unchanged (text code only).
        #[arg(long)]
        relay: Option<String>,
    },
    /// List paired devices.
    List,
    /// Revoke a paired device by id (its token stops authenticating at once).
    Revoke {
        /// Device id, as shown by `pair list` (e.g. `dev-…`).
        device_id: String,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum ScheduleAction {
    /// Ask the configured model for a preview; persistence requires --confirm.
    Add {
        /// Natural-language recurrence and objective.
        request: String,
        /// Persist the validated preview through the local daemon.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Show {
        task_id: String,
    },
    Events {
        task_id: String,
        #[arg(long)]
        follow: bool,
    },
    Diff {
        task_id: String,
    },
    Mailbox {
        task_id: String,
    },
    Message {
        task_id: String,
        text: String,
        #[arg(long)]
        trigger: bool,
    },
    Pause {
        task_id: String,
    },
    Resume {
        task_id: String,
    },
    Cancel {
        task_id: String,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        yes: bool,
        /// Single-use token returned by the preceding cancel preview.
        #[arg(long)]
        confirmation: Option<String>,
    },
    Retry {
        task_id: String,
    },
    Priority {
        task_id: String,
        level: String,
    },
    Budget {
        task_id: String,
        #[arg(long)]
        turns: Option<u32>,
        #[arg(long)]
        tokens: Option<u64>,
        #[arg(long)]
        deadline: Option<u64>,
    },
}

#[derive(clap::Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    /// Start a detached local daemon process.
    Start,
    /// Report the daemon event high-water mark.
    Status,
    /// Request an orderly daemon stop.
    Stop,
    /// Internal long-running daemon worker.
    #[command(hide = true)]
    Serve,
}

/// 把 CLI 参数转换成共享 crate 的纯数据覆盖项。
///
/// 必须逐一映射所有字段:`ConfigOverrides` 里缺省(未提供)的项会回退到环境
/// 变量或默认值,漏映射会静默改变 CLI 行为。
impl From<&Cli> for yi_agent_runtime::config::ConfigOverrides {
    fn from(cli: &Cli) -> Self {
        Self {
            provider: cli.provider.clone(),
            api_url: cli.api_url.clone(),
            api_key: cli.api_key.clone(),
            model: cli.model.clone(),
            max_turns: cli.max_turns,
            workdir: cli.workdir.clone(),
            system_prompt: cli.system_prompt.clone(),
            model_context_length: cli.model_context_length,
            compact_ratio: cli.compact_ratio,
            compact_keep_turns: cli.compact_keep_turns,
            compact_user_budget_tokens: cli.compact_user_budget_tokens,
            compact_tool_budget_tokens: cli.compact_tool_budget_tokens,
            yolo: cli.yolo,
            skip_permissions: cli.skip_permissions,
            sandbox: cli.sandbox,
            sandbox_writable_roots: cli.sandbox_writable_roots.clone(),
            skills_catalog_budget: cli.skills_catalog_budget,
        }
    }
}

/// 解析全局 .env 路径:~/.yi-agent/.env
pub fn resolve_global_env_path() -> Option<PathBuf> {
    yi_agent_runtime::config::resolve_global_env_path()
}

/// Resolve the effective workdir without loading provider configuration.
///
/// Priority is CLI `--workdir`, non-empty `YI_AGENT_WORKDIR`, then the current directory.
pub fn resolve_workdir(cli: &Cli) -> Result<PathBuf> {
    yi_agent_runtime::config::resolve_workdir(&cli.into())
}

/// 从 CLI 参数 + 环境变量加载配置。
///
/// 优先级:CLI 参数 > 环境变量 > 默认值。实际语义由 runtime crate 实现。
///
/// 这是**唯一**的加载入口，且允许 API key 缺失（键为空串）。缺 key 不是加载
/// 错误：能否发起对话只由 `models::is_usable` 在 `resolve_effective` 之后回答。
/// 详见 `yi_agent_runtime::config::RuntimeConfig::load_lenient` 的文档。
///
/// 曾有一个严格版 `load`（缺 key 即 bail）。它和本函数对同一台机器给出相反
/// 结论，启动路径选错一个就把「模型只在 models.json」误报成启动失败。不要再
/// 引入第二个入口——那正是 2026-10-29 `provider turn admission failed: API key
/// required` 的成因。
pub fn load_lenient(cli: &Cli) -> Result<Config> {
    yi_agent_runtime::config::RuntimeConfig::load_lenient(&cli.into())
}

/// 与 [`load_lenient`] 同一份装配，但额外交出本进程的运行期设置绑定。
///
/// app-server 的三个生产入口都从这里取配置：把绑定一并交给 agent 工厂，
/// 保存的运行参数才会对**已存在**的会话生效。settings 路径必须在这里（cwd
/// 尚未漂移时）解析成稳定绝对路径，之后不再随工作目录变化。
pub fn load_lenient_with_binding(
    cli: &Cli,
) -> Result<(
    Config,
    yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding,
)> {
    yi_agent_runtime::config::RuntimeConfig::load_lenient_with_binding(
        &cli.into(),
        &yi_agent_runtime::settings::settings_path(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use yi_agent_runtime::config::ConfigOverrides;

    #[test]
    fn resume_last_with_positional_treats_it_as_prompt() {
        let cli = Cli::try_parse_from(["yi-agent", "resume", "--last", "修复这个 bug"]).unwrap();
        let Some(Command::Resume {
            thread_id,
            last,
            prompt,
        }) = cli.command
        else {
            panic!("应解析为 Resume");
        };
        assert!(last);
        // clap 会把第一个 positional 绑到 thread_id，但语义上它是提示词。
        // dispatch 层负责换位，这里钉住的是「clap 的绑定顺序」这个事实。
        assert_eq!(thread_id.as_deref(), Some("修复这个 bug"));
        assert_eq!(prompt, None);
    }

    #[test]
    fn resume_without_last_treats_positional_as_thread_id() {
        let cli = Cli::try_parse_from(["yi-agent", "resume", "thread-abc"]).unwrap();
        let Some(Command::Resume {
            thread_id,
            last,
            prompt,
        }) = cli.command
        else {
            panic!("应解析为 Resume");
        };
        assert!(!last);
        assert_eq!(thread_id.as_deref(), Some("thread-abc"));
        assert_eq!(prompt, None);
    }

    #[test]
    fn resume_accepts_id_then_prompt() {
        let cli = Cli::try_parse_from(["yi-agent", "resume", "thread-abc", "继续做"]).unwrap();
        let Some(Command::Resume {
            thread_id, prompt, ..
        }) = cli.command
        else {
            panic!("应解析为 Resume");
        };
        assert_eq!(thread_id.as_deref(), Some("thread-abc"));
        assert_eq!(prompt.as_deref(), Some("继续做"));
    }

    /// 测试用互斥锁:涉及环境变量的测试必须串行执行,避免并行干扰。
    ///
    /// `cargo nextest` 下每个测试独立进程，环境变量天然不共享，这把锁退化为
    /// 无害的空操作；`cargo test` 下它仍是必需的。两种运行器都必须保留。
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

    /// 所有 17 个覆盖项字段都必须从 clap 参数一一映射,漏映射会静默改变 CLI 行为。
    #[test]
    fn cli_adapter_maps_all_overrides() {
        let cli = Cli::parse_from([
            "yi-agent",
            "--provider",
            "openai",
            "--api-url",
            "https://example.com",
            "--api-key",
            "secret-key",
            "--model",
            "gpt-4o",
            "--max-turns",
            "7",
            "--workdir",
            "/tmp/adapter-workdir",
            "--system-prompt",
            "be terse",
            "--model-context-length",
            "123456",
            "--compact-ratio",
            "42",
            "--compact-keep-turns",
            "9",
            "--compact-user-budget-tokens",
            "3333",
            "--compact-tool-budget-tokens",
            "4444",
            "--yolo",
            "--sandbox",
            "read-only",
            "--sandbox-writable-root",
            "/tmp/one",
            "--sandbox-writable-root",
            "/tmp/two",
            "--dangerously-skip-permissions",
            "--skills-catalog-budget",
            "2048",
        ]);

        let overrides: ConfigOverrides = (&cli).into();

        assert_eq!(overrides.provider.as_deref(), Some("openai"));
        assert_eq!(overrides.api_url.as_deref(), Some("https://example.com"));
        assert_eq!(overrides.api_key.as_deref(), Some("secret-key"));
        assert_eq!(overrides.model.as_deref(), Some("gpt-4o"));
        assert_eq!(overrides.max_turns, Some(7));
        assert_eq!(
            overrides.workdir,
            Some(PathBuf::from("/tmp/adapter-workdir"))
        );
        assert_eq!(overrides.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(overrides.model_context_length, Some(123_456));
        assert_eq!(overrides.compact_ratio, Some(42));
        assert_eq!(overrides.compact_keep_turns, Some(9));
        assert_eq!(overrides.compact_user_budget_tokens, Some(3_333));
        assert_eq!(overrides.compact_tool_budget_tokens, Some(4_444));
        assert!(overrides.yolo, "--yolo must set yolo");
        assert!(
            overrides.skip_permissions,
            "--dangerously-skip-permissions must set skip_permissions"
        );
        assert_eq!(
            overrides.sandbox,
            Some(yi_agent_tools::SandboxMode::ReadOnly)
        );
        assert_eq!(
            overrides.sandbox_writable_roots,
            vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
        );
        assert_eq!(overrides.skills_catalog_budget, Some(2_048));
    }

    /// 未传任何 flag 时,所有覆盖项都必须为空,让环境变量 / 默认值生效。
    #[test]
    fn cli_adapter_defaults_have_no_overrides() {
        let cli = Cli::parse_from(["yi-agent"]);

        let overrides: ConfigOverrides = (&cli).into();

        assert_eq!(overrides.provider, None);
        assert_eq!(overrides.api_url, None);
        assert_eq!(overrides.api_key, None);
        assert_eq!(overrides.model, None);
        assert_eq!(overrides.max_turns, None);
        assert_eq!(overrides.workdir, None);
        assert_eq!(overrides.system_prompt, None);
        assert_eq!(overrides.model_context_length, None);
        assert_eq!(overrides.compact_ratio, None);
        assert_eq!(overrides.compact_keep_turns, None);
        assert_eq!(overrides.compact_user_budget_tokens, None);
        assert_eq!(overrides.compact_tool_budget_tokens, None);
        assert!(!overrides.yolo);
        assert!(!overrides.skip_permissions);
        assert_eq!(overrides.sandbox, None);
        assert!(overrides.sandbox_writable_roots.is_empty());
        assert_eq!(overrides.skills_catalog_budget, None);
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
    fn cli_parses_app_server_stdio() {
        let cli = Cli::parse_from(["yi-agent", "app-server", "--listen", "stdio://"]);
        assert!(matches!(cli.command, Some(Command::AppServer { .. })));
    }

    #[test]
    fn cli_app_server_defaults_to_stdio() {
        let cli = Cli::parse_from(["yi-agent", "app-server"]);
        match cli.command {
            Some(Command::AppServer { listen, relay }) => {
                assert_eq!(listen, "stdio://");
                assert!(relay.is_none(), "no relay unless asked for");
            }
            other => panic!("expected AppServer command, got {other:?}"),
        }
    }

    /// `--relay <url>` 是 `--listen relay://<url>` 的等价写法,二者都接受的字段。
    #[test]
    fn cli_parses_app_server_relay_flag() {
        let cli = Cli::parse_from([
            "yi-agent",
            "app-server",
            "--relay",
            "wss://relay.example/connect?session=abc",
        ]);
        match cli.command {
            Some(Command::AppServer { relay, .. }) => {
                assert_eq!(
                    relay.as_deref(),
                    Some("wss://relay.example/connect?session=abc")
                );
            }
            other => panic!("expected AppServer command, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_run_subagents_flag() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "run", "--subagents", "delegate"]);
        let Some(Command::Run { subagents, .. }) = cli.command else {
            panic!("expected run command");
        };
        assert!(subagents);
    }

    #[test]
    fn cli_defaults_run_subagents_to_false() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "run", "ordinary"]);
        let Some(Command::Run { subagents, .. }) = cli.command else {
            panic!("expected run command");
        };
        assert!(!subagents);
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
                subagents: _,
                resume: _,
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
                subagents: _,
                resume: _,
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
    fn cli_parses_daemon_status_and_stop() {
        use clap::Parser;
        for (argument, expected) in [
            ("start", DaemonAction::Start),
            ("status", DaemonAction::Status),
            ("stop", DaemonAction::Stop),
        ] {
            let cli = Cli::parse_from(["yi-agent", "daemon", argument]);
            assert!(matches!(cli.command, Some(Command::Daemon { action }) if action == expected));
        }
    }

    #[test]
    fn cli_parses_yolo_flag() {
        use clap::Parser;
        let cli = Cli::parse_from(["yi-agent", "--yolo", "--api-key", "test"]);
        assert!(cli.yolo);
        assert!(!cli.skip_permissions);
    }

    #[test]
    fn cli_parses_sandbox_mode_and_writable_roots() {
        use clap::Parser;
        let cli = Cli::parse_from([
            "yi-agent",
            "--sandbox",
            "read-only",
            "--sandbox-writable-root",
            "/tmp/one",
            "--sandbox-writable-root",
            "/tmp/two",
            "--api-key",
            "test",
        ]);
        assert_eq!(cli.sandbox, Some(yi_agent_tools::SandboxMode::ReadOnly));
        assert_eq!(
            cli.sandbox_writable_roots,
            vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
        );
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

    #[test]
    fn cli_parses_all_documented_task_controls_and_daemon_controls() {
        use clap::Parser;

        for argv in [
            vec!["yi-agent", "agents"],
            vec!["yi-agent", "agent", "show", "task"],
            vec!["yi-agent", "agent", "events", "task"],
            vec!["yi-agent", "agent", "mailbox", "task"],
            vec!["yi-agent", "agent", "diff", "task"],
            vec!["yi-agent", "agent", "cancel", "task"],
            vec!["yi-agent", "agent", "retry", "task"],
            vec!["yi-agent", "agent", "pause", "task"],
            vec!["yi-agent", "agent", "resume", "task"],
            vec!["yi-agent", "daemon", "status"],
            vec!["yi-agent", "daemon", "stop"],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok());
        }
    }

    #[test]
    fn cli_rejects_missing_required_task_control_arguments() {
        use clap::Parser;

        assert!(Cli::try_parse_from(["yi-agent", "agent", "cancel"]).is_err());
    }

    #[test]
    fn cli_parses_documented_agent_control_grammar() {
        use clap::Parser;

        let agents = Cli::parse_from(["yi-agent", "agents", "--all"]);
        assert!(matches!(
            agents.command,
            Some(Command::Agents { all: true, .. })
        ));

        let cancel = Cli::parse_from([
            "yi-agent",
            "agent",
            "cancel",
            "task-123",
            "--recursive",
            "--yes",
            "--confirmation",
            "preview-token",
        ]);
        assert!(matches!(
            cancel.command,
            Some(Command::Agent { action: AgentAction::Cancel { task_id, recursive: true, yes: true, confirmation: Some(confirmation) } })
                if task_id == "task-123" && confirmation == "preview-token"
        ));
    }
}
