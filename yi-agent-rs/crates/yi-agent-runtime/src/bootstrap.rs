//! Agent 装配:provider 构造、工具注册、权限通道。

use std::path::Path;
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

/// skills 服务 + 解析后的 system prompt 的装配结果。
pub struct PromptSetup {
    pub skills: Option<Arc<yi_agent_skills::SkillsService>>,
    pub system_prompt: Option<String>,
}

/// 装配 skills 服务并解析 system prompt(默认 prompt + 当前日期 + skills catalog)。
pub fn build_prompt_setup(cfg: &RuntimeConfig) -> Result<PromptSetup> {
    let skills = build_skills_service(cfg)?;
    let system_prompt = resolve_system_prompt_with_skills(
        cfg.system_prompt.clone(),
        &skills,
        cfg.skills_catalog_budget,
        cfg.skills_catalog_budget_explicit,
    );
    Ok(PromptSetup {
        skills,
        system_prompt,
    })
}

/// 工具集 + system prompt 的装配结果。
pub struct ToolSetup {
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    pub system_prompt: Option<String>,
}

/// 注册内置工具(含 sandbox 配置)。
///
/// 等价于 [`build_tool_setup`] 的非 naked 路径,只取工具集。
pub fn build_tools(cfg: &RuntimeConfig) -> Result<Arc<yi_agent_core::ToolRegistry>> {
    Ok(build_tool_setup(cfg, false)?.tools)
}

/// 解析 system prompt(默认 prompt + 当前日期 + skills catalog)。
///
/// 非 naked 路径下 [`build_prompt_setup`] 恒返回 `Some`;若装配意外失败则返回空串。
pub fn build_system_prompt(cfg: &RuntimeConfig) -> String {
    build_prompt_setup(cfg)
        .map(|setup| setup.system_prompt.unwrap_or_default())
        .unwrap_or_default()
}

/// 装配工具集与 system prompt。
///
/// `naked = true` 返回空 registry + `None` prompt(供只读/无工具场景使用)。
///
/// 注意:`naked = false` 时**总是**注册 [`yi_agent_tools::ProcessManager`] 支撑的
/// 进程工具(`process_start` / `process_kill` 等),与 TUI 路径对齐——app-server
/// 需要这些工具。Phase B 之后 CLI 的无头路径若改走本函数,也会获得进程工具,
/// 这是计划明确认可的。
///
/// 另注意:非 naked 路径在 skills catalog 预算未显式指定、且 stdin 是 TTY 时,
/// **可能读取 stdin** 以询问是否纳入完整 catalog。stdin 非 TTY 的调用方(如
/// app-server sidecar,其 stdin 是管道)永远不会触发该询问。
pub fn build_tool_setup(cfg: &RuntimeConfig, naked: bool) -> Result<ToolSetup> {
    build_tool_setup_in(cfg, naked, &cfg.workdir)
}

/// 同 [`build_tool_setup`],但内置工具以 `workspace` 为根(子 agent 的 worktree 场景)。
///
/// 注意 `workspace` 只影响内置工具与进程工具的根;skills 的项目根仍取
/// `cfg.workdir`——这一不对称是有意保留的,与无头路径的历史行为一致。
pub fn build_tool_setup_in(
    cfg: &RuntimeConfig,
    naked: bool,
    workspace: &Path,
) -> Result<ToolSetup> {
    if naked {
        return Ok(ToolSetup {
            tools: Arc::new(yi_agent_core::ToolRegistry::new()),
            system_prompt: None,
        });
    }

    let mut registry = yi_agent_core::ToolRegistry::new();

    let prompt = build_prompt_setup(cfg)?;

    if let Some(svc) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        workspace.to_path_buf(),
        cfg.sandbox,
        cfg.sandbox_writable_roots.clone(),
    );

    let process_manager = yi_agent_tools::ProcessManager::with_sandbox(
        workspace.to_path_buf(),
        yi_agent_tools::SandboxPolicy::new(
            cfg.sandbox,
            workspace,
            cfg.sandbox_writable_roots.clone(),
        ),
    );
    yi_agent_tools::register_process_tools(&mut registry, process_manager);

    Ok(ToolSetup {
        tools: Arc::new(registry),
        system_prompt: prompt.system_prompt,
    })
}

/// 权限审批模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// 由外部(CLI / app-server)持有 receiver 并回传决定。
    Interactive,
    /// 自动放行(yolo),不暴露 receiver。
    AutoAllow,
}

/// app-server 持有的权限决定接收端(与 `Agent::with_permission` 期望的类型一致)。
pub type DecisionReceiver = Arc<
    tokio::sync::Mutex<tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>>,
>;

/// 一次 agent 装配的结果,含权限检查器与决定通道。
pub struct AgentBootstrap {
    pub agent: yi_agent_core::Agent,
    pub permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 交互模式下由调用方(CLI / app-server)用它回传权限决定;
    /// AutoAllow 模式下为 `None`(通道已关闭,黑名单命令解析为 Deny)。
    pub decision_tx: Option<tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>>,
    /// app-server 持有 rx 以接收权限决定;AutoAllow 时返回 None。
    pub decision_rx: Option<DecisionReceiver>,
}

/// 由运行时配置构造 [`yi_agent_core::AgentConfig`](集中一处,避免各调用点漂移)。
pub fn build_agent_config(
    cfg: &RuntimeConfig,
    system_prompt: Option<String>,
) -> yi_agent_core::AgentConfig {
    yi_agent_core::AgentConfig {
        model: cfg.model.clone(),
        system_prompt,
        max_turns: Some(cfg.max_turns),
        compact_threshold: Some(cfg.compact_threshold),
        compact_user_budget_tokens: cfg.compact_user_budget_tokens,
        compact_tool_budget_tokens: cfg.compact_tool_budget_tokens,
        ..Default::default()
    }
}

/// 组装 provider + 工具 + 权限通道,返回可运行的 agent。
pub fn bootstrap_agent(cfg: &RuntimeConfig, mode: PermissionMode) -> Result<AgentBootstrap> {
    let provider = build_provider(cfg)?;
    let setup = build_tool_setup(cfg, false)?;

    let yolo = match mode {
        PermissionMode::Interactive => cfg.yolo,
        PermissionMode::AutoAllow => true,
    };
    let checker = load_permission_checker(&cfg.workdir, yolo)?;

    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
    let rx_arc = Arc::new(tokio::sync::Mutex::new(decision_rx));

    let agent_config = build_agent_config(cfg, setup.system_prompt);

    match mode {
        PermissionMode::Interactive => {
            let agent = yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                .with_permission(checker.clone(), rx_arc.clone());
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                decision_tx: Some(decision_tx),
                decision_rx: Some(rx_arc),
            })
        }
        PermissionMode::AutoAllow => {
            // AutoAllow 下没有 UI 读取决定。丢弃 sender 让通道关闭:agent 侧
            // `recv()` 会立即返回 `None`,黑名单命令据此解析为 Deny,而不是
            // 永久阻塞等待一个不会到来的决定。
            drop(decision_tx);
            let agent = yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                .with_permission(checker.clone(), rx_arc);
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                decision_tx: None,
                decision_rx: None,
            })
        }
    }
}

/// 加载权限检查器。
///
/// `PermissionChecker::load` 是 async,但 [`bootstrap_agent`] 是同步函数。
/// 当调用方本身已处于 Tokio runtime 内时,直接 `Runtime::block_on` 会 panic,
/// 因此在专用线程(无 runtime 上下文)上跑加载——无论调用方是否在 runtime 内都成立。
pub fn load_permission_checker(
    workdir: &Path,
    yolo: bool,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    let permissions = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                rt.block_on(yi_agent_core::permission::PermissionChecker::load(workdir))
            })
            .join()
    })
    .map_err(|_| anyhow::anyhow!("permission loader thread panicked"))?
    .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?;

    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    Ok(Arc::new(yi_agent_core::permission::PermissionChecker::new(
        permissions,
        yolo,
        workdir.to_path_buf(),
        blocklist_fn,
    )))
}

/// Resolve the effective system prompt: fall back to the built-in default
/// when the user did not provide one. The current local date is appended to
/// the end so the model knows today's date; placed at the tail to avoid
/// disrupting the cached prefix of the prompt.
fn resolve_system_prompt(user: Option<String>) -> Option<String> {
    let mut base = yi_agent_core::AgentConfig::default_system_prompt();
    if let Some(user) = user {
        base.push_str("\n\nUser-provided instructions:\n");
        base.push_str(&user);
    }
    let today = chrono::Local::now().format("%Y-%m-%d");
    Some(format!("{base}\n\nCurrent date: {today}"))
}

/// Set up the skills service: install bundled system skills, build roots, snapshot.
/// Returns None on hard failure (and logs a warning); the agent runs without skills.
fn build_skills_service(
    cfg: &RuntimeConfig,
) -> Result<Option<Arc<yi_agent_skills::SkillsService>>> {
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("skills: could not determine home directory, skipping");
        return Ok(None);
    };
    let system_root = home.join(".yi-agent/skills/.system");

    // Install bundled skills; failure is non-fatal
    if let Err(e) = yi_agent_skills::install_system_skills(&system_root) {
        tracing::warn!("failed to install bundled skills: {e}");
    }

    let roots = vec![
        (
            cfg.workdir.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::Project,
        ),
        (
            home.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::User,
        ),
        (
            home.join(".yi-agent/skills/.system"),
            yi_agent_skills::SkillScope::System,
        ),
    ];

    let service = Arc::new(yi_agent_skills::SkillsService::new(roots));
    match service.snapshot() {
        Ok(skills) => {
            tracing::info!("skills: {} discovered", skills.len());
            Ok(Some(service))
        }
        Err(e) => {
            tracing::warn!("skills discovery failed: {e}");
            Ok(None)
        }
    }
}

/// Resolve the effective system prompt, appending the skills catalog if available.
fn resolve_system_prompt_with_skills(
    user: Option<String>,
    service: &Option<Arc<yi_agent_skills::SkillsService>>,
    budget: usize,
    budget_explicit: bool,
) -> Option<String> {
    let base = resolve_system_prompt(user);
    let Some(svc) = service else {
        return base;
    };

    let total = svc.full_catalog_size();
    let effective_budget = resolve_effective_budget(total, budget, budget_explicit);
    let catalog = svc.render_catalog(effective_budget);

    if catalog.is_empty() {
        return base;
    }

    match base {
        Some(p) => Some(format!("{p}\n\n{catalog}")),
        None => Some(catalog),
    }
}

fn resolve_effective_budget(total: usize, default: usize, explicit: bool) -> usize {
    if explicit || total <= default || !is_interactive() {
        return default;
    }
    prompt_catalog_budget(total, default).unwrap_or(default)
}

fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn prompt_catalog_budget(total: usize, default: usize) -> Option<usize> {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {total_kb} KB, exceeds default {default_kb} KB budget.\n\
         Include all skills? [Y/n]"
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    match input.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => Some(total),
        _ => Some(default),
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

    #[test]
    fn build_tools_registers_fs_and_shell() {
        let cfg = sample_config();
        let registry = build_tools(&cfg).expect("build tools");
        for name in ["bash", "read", "write"] {
            assert!(
                registry.get(name).is_some(),
                "tool `{name}` must be registered"
            );
        }
        // Locks in the TUI-aligned decision: `build_tool_setup` includes the
        // ProcessManager-backed process tools (the app-server needs them).
        assert!(
            registry.get("process_start").is_some(),
            "process tools must be registered"
        );
    }

    #[test]
    fn build_tool_setup_naked_is_empty() {
        let cfg = sample_config();
        let setup = build_tool_setup(&cfg, true).expect("build naked setup");
        assert!(setup.tools.is_empty());
        assert!(setup.system_prompt.is_none());
    }

    #[test]
    fn build_system_prompt_includes_current_date() {
        let cfg = sample_config();
        let prompt = build_system_prompt(&cfg);
        assert!(prompt.contains("Current date:"), "prompt was: {prompt}");
    }

    #[test]
    fn resolve_system_prompt_none_uses_default() {
        let resolved = resolve_system_prompt(None);
        let default = yi_agent_core::AgentConfig::default_system_prompt();
        // The resolved prompt should start with the default prompt and have
        // the current date appended at the end.
        assert!(
            resolved.as_deref().is_some_and(|r| r.starts_with(&default)),
            "resolved should start with default prompt"
        );
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            resolved.as_deref().is_some_and(|r| r.ends_with(&today)),
            "resolved should end with today's date: {resolved:?}"
        );
    }

    #[test]
    fn resolve_system_prompt_custom_keeps_base_instructions() {
        let resolved = resolve_system_prompt(Some("custom".into()));
        let default = yi_agent_core::AgentConfig::default_system_prompt();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            resolved.as_deref().is_some_and(|r| r.starts_with(&default)
                && r.contains("User-provided instructions:\ncustom")
                && r.ends_with(&today)),
            "resolved should retain base instructions, append custom prompt, and end with date: {resolved:?}"
        );
    }

    #[test]
    fn resolve_effective_budget_explicit_returns_default() {
        // When explicit=true, should return default regardless of total.
        assert_eq!(resolve_effective_budget(100_000, 8192, true), 8192);
        assert_eq!(resolve_effective_budget(0, 8192, true), 8192);
        assert_eq!(resolve_effective_budget(8192, 8192, true), 8192);
    }

    #[test]
    fn resolve_effective_budget_total_under_default_returns_default() {
        // When total <= default, should return default.
        assert_eq!(resolve_effective_budget(4096, 8192, false), 8192);
        assert_eq!(resolve_effective_budget(8192, 8192, false), 8192);
        assert_eq!(resolve_effective_budget(0, 8192, false), 8192);
    }

    #[test]
    fn resolve_effective_budget_non_interactive_returns_default() {
        // Tests run non-interactive (stdin is not a TTY), so even when
        // total > default and explicit=false, should return default without prompting.
        assert_eq!(resolve_effective_budget(100_000, 8192, false), 8192);
    }

    #[test]
    fn resolve_system_prompt_with_skills_no_service_returns_base() {
        // When service is None, should fall back to base via resolve_system_prompt
        // (which appends the current date).
        let resolved = resolve_system_prompt_with_skills(None, &None, 8192, false);
        let expected = resolve_system_prompt(None);
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_system_prompt_with_skills_empty_catalog_returns_base() {
        // When service is Some but catalog is empty (no skills discovered),
        // should return the base prompt unchanged (with current date appended).
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![]));
        let expected = resolve_system_prompt(None);
        let resolved = resolve_system_prompt_with_skills(None, &Some(svc), 8192, false);
        assert_eq!(resolved, expected);
    }

    #[test]
    fn build_agent_config_maps_all_fields() {
        let mut cfg = sample_config();
        cfg.model = "mapped-model".into();
        cfg.max_turns = 7;
        cfg.compact_threshold = 42_000;
        cfg.compact_user_budget_tokens = 1_234;
        cfg.compact_tool_budget_tokens = 5_678;

        let agent_config = build_agent_config(&cfg, Some("sys".into()));

        assert_eq!(agent_config.model, "mapped-model");
        assert_eq!(agent_config.system_prompt.as_deref(), Some("sys"));
        assert_eq!(agent_config.max_turns, Some(7));
        assert_eq!(agent_config.compact_threshold, Some(42_000));
        assert_eq!(agent_config.compact_user_budget_tokens, 1_234);
        assert_eq!(agent_config.compact_tool_budget_tokens, 5_678);
    }

    #[test]
    fn build_tool_setup_in_uses_workspace() {
        let cfg = sample_config();
        let workspace = std::path::Path::new("/tmp/other-workspace");

        let setup = build_tool_setup_in(&cfg, false, workspace).expect("build setup in workspace");
        assert!(
            !setup.tools.is_empty(),
            "non-naked setup registers builtin tools"
        );
        assert!(
            setup.system_prompt.is_some(),
            "non-naked setup resolves a system prompt"
        );

        let naked = build_tool_setup_in(&cfg, true, workspace).expect("build naked setup");
        assert!(naked.tools.is_empty());
        assert!(naked.system_prompt.is_none());
    }

    #[tokio::test]
    async fn bootstrap_interactive_keeps_decision_channel_open() {
        let cfg = sample_config();
        let b = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");
        assert!(b.decision_rx.is_some(), "interactive keeps the receiver");
        assert!(
            b.decision_tx
                .as_ref()
                .expect("interactive exposes the sender")
                .try_send((0, yi_agent_core::permission::Decision::Deny))
                .is_ok(),
            "interactive sender must stay live"
        );
    }

    #[tokio::test]
    async fn bootstrap_auto_allow_closes_decision_channel() {
        let cfg = sample_config();
        let b = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
        assert!(b.decision_rx.is_none(), "auto-allow exposes no receiver");
        // The sender is dropped inside `bootstrap_agent`, closing the channel so
        // blacklisted commands resolve as Deny instead of blocking forever.
        assert!(b.decision_tx.is_none(), "auto-allow exposes no sender");
    }
}
