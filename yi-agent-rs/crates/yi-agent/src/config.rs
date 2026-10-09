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
    /// 常驻值守：保证登记在册的项目各自有一个活着的 daemon。
    Boards {
        #[command(subcommand)]
        action: BoardsAction,
    },
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum BoardsAction {
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
    Accept {
        task_id: String,
        #[arg(long)]
        yes: bool,
        /// Single-use token returned by the preceding review preview.
        #[arg(long)]
        confirmation: Option<String>,
    },
    Rework {
        task_id: String,
        feedback: String,
        #[arg(long)]
        yes: bool,
        /// Single-use token returned by the preceding review preview.
        #[arg(long)]
        confirmation: Option<String>,
    },
    Reject {
        task_id: String,
        reason: String,
        #[arg(long)]
        yes: bool,
        /// Single-use token returned by the preceding review preview.
        #[arg(long)]
        confirmation: Option<String>,
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

/// 解析 .env 文件路径:优先 workdir CLI 参数,否则 YI_AGENT_WORKDIR 环境变量,否则当前目录。
/// 所有路径均使用 `.yi-agent/.env` 子目录结构,避免与项目自身的 .env 冲突。
pub fn resolve_env_path(cli: &Cli) -> std::path::PathBuf {
    yi_agent_runtime::config::resolve_env_path(&cli.into())
}

/// 解析全局 .env 路径:~/.yi-agent/.env
pub fn resolve_global_env_path() -> Option<PathBuf> {
    yi_agent_runtime::config::resolve_global_env_path()
}

/// 判断是否为显式指定 workdir(CLI 参数或环境变量)
pub fn is_workdir_explicit(cli: &Cli) -> bool {
    yi_agent_runtime::config::is_workdir_explicit(&cli.into())
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
pub fn load(cli: &Cli) -> Result<Config> {
    yi_agent_runtime::config::RuntimeConfig::load(&cli.into())
}

/// 与 [`load`] 相同，但允许 API key 缺失（桌面侧车首启场景）。
pub fn load_lenient(cli: &Cli) -> Result<Config> {
    yi_agent_runtime::config::RuntimeConfig::load_lenient(&cli.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use yi_agent_runtime::config::ConfigOverrides;

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
            vec!["yi-agent", "agent", "accept", "task"],
            vec!["yi-agent", "agent", "rework", "task", "feedback"],
            vec!["yi-agent", "agent", "reject", "task", "reason"],
            vec!["yi-agent", "daemon", "status"],
            vec!["yi-agent", "daemon", "stop"],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok());
        }
    }

    #[test]
    fn cli_rejects_missing_required_task_control_arguments() {
        use clap::Parser;

        for argv in [
            vec!["yi-agent", "agent", "cancel"],
            vec!["yi-agent", "agent", "rework", "task"],
            vec!["yi-agent", "agent", "reject", "task"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err());
        }
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

        let rework = Cli::parse_from(["yi-agent", "agent", "rework", "task-123", "tighten tests"]);
        assert!(matches!(
            rework.command,
            Some(Command::Agent { action: AgentAction::Rework { task_id, feedback, .. } })
                if task_id == "task-123" && feedback == "tighten tests"
        ));
    }
}
