//! Agent 装配:provider 构造、工具注册、权限通道。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::config::RuntimeConfig;

/// 根据配置构造 LLM provider。
pub fn build_provider(cfg: &RuntimeConfig) -> Result<Arc<dyn yi_agent_core::Provider>> {
    // 模型名是唯一没有客观默认值的字段。放空串过去只会在第一次请求时变成
    // 一个难懂的 404/400，而且子 agent 会继承同一个空串——不如在这里就
    // 说清楚"还没配置"，让调用方把用户导向设置/引导。
    if cfg.model.trim().is_empty() {
        anyhow::bail!("模型未配置：请在设置中完成模型配置");
    }
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
                api_style: Some(cfg.api_style.as_str().to_string()),
                reasoning_effort: cfg.reasoning_effort.clone(),
                ..Default::default()
            },
        )?)),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    }
}

/// 刷新 skills catalog 并重新拼装 system prompt 的句柄。
///
/// 持有启动时确定的 base prompt 与预算策略;`current_system_prompt()` 每次调用
/// 都重扫 skill 根目录并重渲染 catalog。catalog 未变化时输出逐字节相同,因此
/// 不会破坏 provider 的 prompt cache。
pub struct SkillsCatalogHandle {
    /// The single prompt-composition entry point. The handle deliberately keeps
    /// no second copy of the base prompt or the budget: two sources of truth is
    /// exactly how a refresh used to overwrite a just-saved value.
    composer: Arc<crate::live_settings::prompt::LivePromptComposer>,
    /// `None` = 不截断(启动时用户选择了"纳入全部")。
    budget: Option<usize>,
    /// The user segment the handle was assembled with, used when the caller has
    /// no live value to pass (static/test paths).
    user_prompt: Option<String>,
    /// The budget source the static assembly resolved; it is reported as-is so
    /// a caller that later adopts the live path keeps the same provenance.
    budget_source: crate::live_settings::sources::ValueSource,
}

impl SkillsCatalogHandle {
    /// Re-scan the skill roots and rebuild the system prompt.
    pub fn current_system_prompt(&self) -> Option<String> {
        self.render_with(self.user_prompt.as_deref(), self.budget_source)
    }

    /// Rebuild the prompt from caller-provided live values.
    ///
    /// This is what a host uses between turns: the composer owns the stable
    /// context and the catalog, so only the user segment and the budget travel
    /// through here. A render failure yields `None` rather than silently
    /// reusing the previous user segment.
    pub fn render_with(
        &self,
        user_prompt: Option<&str>,
        budget_source: crate::live_settings::sources::ValueSource,
    ) -> Option<String> {
        let budget = self.budget.unwrap_or(usize::MAX);
        match self.composer.render(user_prompt, budget, budget_source) {
            Ok(prompt) => prompt,
            Err(error) => {
                tracing::warn!(code = error.code, "prompt render failed");
                None
            }
        }
    }

    /// The composer this handle renders through.
    ///
    /// Hosts that must keep live settings attached pass this to
    /// `RuntimeSettingsBinding::agent_source` so the prompt and the other
    /// runtime fields are derived at one safe point.
    pub fn composer(&self) -> &Arc<crate::live_settings::prompt::LivePromptComposer> {
        &self.composer
    }

    #[cfg(test)]
    fn for_test(
        service: Arc<yi_agent_skills::SkillsService>,
        base_prompt: Option<String>,
        budget: Option<usize>,
    ) -> Self {
        let mut cfg = crate::config::sample_config();
        // The test fixture supplies the base prompt as the user segment; no
        // project instructions or catalog ordering surprises are wanted here.
        cfg.workdir = std::env::temp_dir();
        let composer = Arc::new(
            crate::live_settings::prompt::LivePromptComposer::new(
                &cfg,
                crate::live_settings::prompt::PromptRole::Normal,
                Some(service),
                crate::live_settings::prompt::InitialCatalogPolicy::Bounded,
            )
            .expect("composer"),
        );
        Self {
            composer,
            budget,
            user_prompt: base_prompt,
            budget_source: crate::live_settings::sources::ValueSource::Settings,
        }
    }
}

/// skills 服务 + 解析后的 system prompt 的装配结果。
pub struct PromptSetup {
    pub skills: Option<Arc<yi_agent_skills::SkillsService>>,
    pub catalog: Option<SkillsCatalogHandle>,
    pub system_prompt: Option<String>,
}

/// 装配 skills 服务并解析 system prompt(默认 prompt + 当前日期 + skills catalog)。
pub fn build_prompt_setup(cfg: &RuntimeConfig) -> Result<PromptSetup> {
    let skills = build_skills_service(cfg)?;
    let catalog = build_catalog_handle(cfg, &skills);
    let system_prompt = match &catalog {
        Some(handle) => handle.current_system_prompt(),
        None => resolve_system_prompt(cfg.system_prompt.clone(), &cfg.workdir),
    };
    Ok(PromptSetup {
        skills,
        catalog,
        system_prompt,
    })
}

/// 工具集 + system prompt 的装配结果。
pub struct ToolSetup {
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    pub catalog: Option<SkillsCatalogHandle>,
    pub system_prompt: Option<String>,
    /// MCP 管理器。非 naked 路径**恒为 `Some`**:配置缺失或损坏时交出的是一份
    /// 空快照的 manager(而不是 `None`),这样 `mcp_reload` 与 driver 共用同一个
    /// 实例,用户第一次在对话里加 MCP 也能生效。只有 naked 路径(空工具集)为
    /// `None`。返回它以便**负责生命周期的调用方**(headless / TUI)在退出前调用
    /// `shutdown()` 关闭 stdio 子进程;不负责生命周期的调用方可以丢弃它。
    pub mcp: Option<Arc<yi_agent_mcp::McpManager>>,
    /// 支撑 `process_start` / `process_list` / `process_read` / `process_kill`
    /// 四个工具的进程管理器。**必须与 `tools` 里的进程工具同行**：调用方要按
    /// 「哪个注册表生效」选中对应的 manager，否则会查到一个空列表而工具明明可用。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// The controller the builtin tools were given.
    ///
    /// Exposed so a caller that adds tools later (the per-turn delegation set)
    /// builds them from the same live YOLO switch the rest of the set reads.
    /// Re-deriving one from config would silently fork the policy.
    pub sandbox: yi_agent_tools::SandboxController,
}

/// 注册内置工具(含 sandbox 配置)。
///
/// 等价于 [`build_tool_setup`] 的非 naked 路径,只取工具集。注意:它同时会加载
/// `.yi-agent/mcp.json` 并注册 MCP 工具(cold-cache 时触发一次启动期探测),但
/// **丢弃**返回的 [`yi_agent_mcp::McpManager`]——需要管理 MCP 生命周期的调用方
/// 应改用 [`build_tool_setup`] 并自行 `shutdown()`。
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
    build_tool_setup_in(cfg, naked, std::slice::from_ref(&cfg.workdir))
}

/// 同 [`build_tool_setup`],但内置工具以 `roots` 为根集(`roots[0]` = 主根)。
///
/// app-server 的会话可携带关联根:FS 工具层收整集,shell 沙箱的额外可写根只收
/// `roots[1..]` 加 `cfg.sandbox_writable_roots`(见 [`build_tool_setup_with_controller`])。
///
/// 注意 `roots` 只影响内置工具与进程工具的根;skills 与 MCP 配置
/// (`.yi-agent/mcp.json`)的项目根仍取 `cfg.workdir`——这一不对称是有意保留的,
/// 与无头路径的历史行为一致。
pub fn build_tool_setup_in(
    cfg: &RuntimeConfig,
    naked: bool,
    roots: &[PathBuf],
) -> Result<ToolSetup> {
    build_tool_setup_with_switch(
        cfg,
        naked,
        roots,
        yi_agent_core::autonomy::YoloSwitch::new(cfg.yolo),
    )
}

/// 同 [`build_tool_setup_in`],但由调用方提供共享的 [`yi_agent_core::autonomy::YoloSwitch`]。
///
/// 本函数把传入的 `switch` 绑到沙箱 controller;与权限层(控制权限的
/// `PermissionChecker`)共用同一 switch 的**接线**发生在 [`bootstrap_agent`]
/// ——它建唯一一份 switch,clone 给沙箱与权限,翻转时两层同时生效。
///
/// `naked = true` 时忽略 `switch`(直接返回空工具集)。
pub fn build_tool_setup_with_switch(
    cfg: &RuntimeConfig,
    naked: bool,
    roots: &[PathBuf],
    switch: yi_agent_core::autonomy::YoloSwitch,
) -> Result<ToolSetup> {
    let controller =
        yi_agent_tools::SandboxController::new(switch, cfg.sandbox, cfg.sandbox_promotable);
    build_tool_setup_with_controller(cfg, naked, roots, controller, Vec::new())
}

/// Same as [`build_tool_setup_with_switch`], but with an externally-owned
/// controller so the caller can share one controller with other tools (e.g.
/// the subagent spawn tools); this keeps the root's sandbox and the spawn
/// tools reading the same live YOLO switch.
///
/// `roots` 为空是配置错误:FS 工具层会退化成拒绝所有路径(Task 3 已定),这里直接
/// 报错,让「一个会话至少有一个工作区根」这条不变量在装配期就成立。
pub fn build_tool_setup_with_controller(
    cfg: &RuntimeConfig,
    naked: bool,
    roots: &[PathBuf],
    controller: yi_agent_tools::SandboxController,
    session_env: Vec<(String, String)>,
) -> Result<ToolSetup> {
    build_tool_setup_owned(cfg, naked, roots, controller, session_env, None)
}

/// Where spawned processes belong, when this runtime has an owner.
///
/// A managed runtime names the frontend that owns every process the session
/// starts, plus the channel it starts them on. `None` keeps the direct spawn.
pub type ManagedOwner = (
    yi_agent_tools::ManagedProcessPolicy,
    yi_agent_tools::ManagedSpawner,
);

/// Same as [`build_tool_setup_with_controller`], but tools spawn through
/// `managed`.
///
/// The MCP seam is wired from the same value: an MCP server started directly would
/// be an unregistered child, which is the leak this path exists to prevent. The
/// launcher is built here rather than inside the MCP crate so that crate keeps no
/// dependency on the tools crate.
pub fn build_tool_setup_owned(
    cfg: &RuntimeConfig,
    naked: bool,
    roots: &[PathBuf],
    controller: yi_agent_tools::SandboxController,
    session_env: Vec<(String, String)>,
    managed: Option<ManagedOwner>,
) -> Result<ToolSetup> {
    if naked {
        // 空 root 的 manager:与空 registry 配套,保证结构体总是自洽。
        // naked 调用方只用 `tools`(空),不会起进程。
        let process_manager = yi_agent_tools::ProcessManager::new(std::env::temp_dir());
        return Ok(ToolSetup {
            tools: Arc::new(yi_agent_core::ToolRegistry::new()),
            catalog: None,
            system_prompt: None,
            mcp: None,
            process_manager,
            sandbox: controller,
        });
    }

    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("a session needs at least one workspace root"))?;

    let mut registry = yi_agent_core::ToolRegistry::new();

    let prompt = build_prompt_setup(cfg)?;

    if let Some(svc) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    // FS 根集含全部关联根;shell 沙箱的可写根只取「关联根 + cfg 配置的额外汇总项」,
    // 且规范化(排序 + 去重)后交给两条执行路径。
    let mut sandbox_writable = roots[1..].to_vec();
    sandbox_writable.extend(cfg.sandbox_writable_roots.iter().cloned());
    sandbox_writable.sort();
    sandbox_writable.dedup();

    yi_agent_tools::register_builtin_tools_managed(
        &mut registry,
        roots.to_vec(),
        controller.clone(),
        sandbox_writable.clone(),
        session_env.clone(),
        managed.clone(),
    );

    let process_manager = yi_agent_tools::ProcessManager::with_controller(
        primary,
        controller.clone(),
        sandbox_writable,
    );
    let process_manager = match &managed {
        Some((policy, spawner)) => {
            Arc::clone(&process_manager).into_managed(policy.clone(), spawner.clone())
        }
        None => process_manager,
    };
    yi_agent_tools::register_process_tools(&mut registry, Arc::clone(&process_manager));

    // MCP 配置是项目级文件(`.yi-agent/mcp.json`,被 gitignore),固定在项目根
    // `cfg.workdir` 下读取——与 skills 的项目根一致,而非子 agent 的 worktree
    // `workspace`(新 worktree 里没有该文件,用它会让子 agent 静默丢失 MCP 工具)。
    //
    // MCP 是可选子系统:配置缺失/损坏或探测初始化失败都不应阻断 agent 启动
    // (单个 server 的问题已在 yi-agent-mcp 内部 warn 并跳过)。
    // A managed runtime must not leave the MCP probe's children unregistered, so
    // the launcher is built from the same owner the tools use. `None` keeps the
    // direct spawn, which is what an unmanaged daemon has always done.
    let launcher: Option<Arc<dyn yi_agent_mcp::StdioLauncher>> =
        managed.as_ref().map(|(_, spawner)| {
            crate::mcp_launcher::ManagedStdioLauncher::new(spawner.clone())
                as Arc<dyn yi_agent_mcp::StdioLauncher>
        });
    if managed.is_some() && launcher.is_none() {
        // Unreachable by construction, but a silent fallback to direct spawning
        // here is exactly the leak that must not be quiet.
        anyhow::bail!("a managed session must launch MCP servers through its owner");
    }
    // 没有 `mcp.json` 时也要交出一份 manager(空快照),而不是 `None`。
    //
    // 这不是洁癖:driver 每轮开跑前靠这个 manager 判断「registry 是否要刷新」,
    // 而 `mcp_reload` 工具**也**用它写盘并探测。若这里返回 `None`,`mcp_reload`
    // 就会拿到一个只有它自己知道的一次性空 manager——用户第一次在对话里说
    // 「帮我加一个 MCP」时,配置确实写进盘了,但 driver 手上没有 manager,既不会
    // 重载也不会重建 registry,新工具在进程重启前永远不出现。
    //
    // 配置损坏(`Err`)同样退回空 manager:一份坏掉的 JSON 不该把「修好它」的
    // 唯一入口(`mcp_reload`)一起废掉。
    let mcp = match yi_agent_mcp::register_mcp_tools_with(
        &mut registry,
        &cfg.workdir,
        launcher.clone(),
    ) {
        Ok(Some(manager)) => manager,
        Ok(None) => yi_agent_mcp::McpManager::empty_with_launcher(launcher),
        Err(err) => {
            tracing::warn!("MCP config unavailable; starting with an empty manager: {err:#}");
            yi_agent_mcp::McpManager::empty_with_launcher(launcher)
        }
    };

    // `mcp_reload` 无条件注册:还没有 mcp.json 时必须也有入口,否则"第一次
    // 加一个 MCP"就无从下手。它与上面探测用的 manager 是**同一个 Arc**,
    // 否则它改完配置后 driver 手上的那份快照不会跟着变,下一轮也就无从重建。
    {
        let blocklist_fn: yi_agent_core::permission::BlocklistFn =
            Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(str::to_string));
        yi_agent_mcp::register_mcp_reload_tool(
            &mut registry,
            Arc::clone(&mcp),
            &cfg.workdir,
            Some(blocklist_fn),
        );
    }

    Ok(ToolSetup {
        tools: Arc::new(registry),
        catalog: prompt.catalog,
        system_prompt: prompt.system_prompt,
        mcp: Some(mcp),
        process_manager,
        sandbox: controller,
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
    /// The provider the agent runs on. Callers that rebuild the agent with a
    /// different tool set must reuse this instance rather than building a
    /// second one, so the credential and the client stay a single object.
    pub provider: Arc<dyn yi_agent_core::Provider>,
    /// The registry the agent was built with. Callers that need to add tools
    /// (for example the six delegation tools) rebuild from this instead of
    /// re-deriving a tool set of their own, which would drop the skills and
    /// MCP wiring only this constructor knows about.
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    /// 本次装配的进程管理器;与 `tools` 里的进程工具是同一份。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// 交互模式下由调用方(CLI / app-server)用它回传权限决定;
    /// AutoAllow 模式下为 `None`(通道已关闭,黑名单命令解析为 Deny)。
    pub decision_tx: Option<tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>>,
    /// app-server 持有 rx 以接收权限决定;AutoAllow 时返回 None。
    pub decision_rx: Option<DecisionReceiver>,
    /// 刷新 skills catalog 的句柄;无 skills 服务时为 `None`。
    pub catalog: Option<SkillsCatalogHandle>,
    /// 本次装配的共享 yolo 开关(clone 自沙箱与权限共用的同一 `Arc`)。
    pub yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 本次装配的 MCP 管理器。`mcp_reload` 工具与每轮配置变更检查都靠它,
    /// 两者是**同一个 `Arc`**。非 naked 路径恒为 `Some`:配置缺失时是空快照的
    /// manager,`reload` 会把新建的配置读进来;只有 naked 路径为 `None`。
    pub mcp: Option<Arc<yi_agent_mcp::McpManager>>,
    /// The sandbox controller this bootstrap's tools were built with, so a caller
    /// rebuilding the agent can keep the same live policy.
    pub sandbox: yi_agent_tools::SandboxController,
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
        // 请求级的 effort 取自 cfg（`resolve_effective` 已把条目的默认值、
        // 或 app-server 注入的会话覆盖写进 `cfg.reasoning_effort`）。`None`
        // 表示不发该字段 ⇒ 零回归；precedence 由 `cfg` 单一权威。
        gen_params: yi_agent_core::GenParams {
            reasoning_effort: cfg.reasoning_effort.clone(),
            ..Default::default()
        },
        compact_threshold: Some(cfg.compact_threshold),
        compact_user_budget_tokens: cfg.compact_user_budget_tokens,
        compact_tool_budget_tokens: cfg.compact_tool_budget_tokens,
        // 请求体超限时的第 2 级降级：把超出「最近 N 张」的旧图缩到 1024 长边。
        // 不注入也能工作（直接退到占位），但那样旧图对模型彻底消失；注入后
        // 模型仍看得到缩略图。装配只此一处，各调用点不再各自设置。
        image_downscaler: Some(std::sync::Arc::new(
            yi_agent_tools::image_downscale::ImagePrepDownscaler::new(),
        )),
        ..Default::default()
    }
}

/// 组装 provider + 工具 + 权限通道,返回可运行的 agent(**静态**装配)。
///
/// 生产宿主应改用 [`bootstrap_agent_with_binding`]：本 facade 不挂运行期设置
/// 来源，因此会话一旦启动就固定在本次装配的取值上。它保留给只读测试与不关心
/// 热更新的调用方，行为与历史逐字一致。
pub fn bootstrap_agent(cfg: &RuntimeConfig, mode: PermissionMode) -> Result<AgentBootstrap> {
    bootstrap_agent_inner(cfg, mode, None)
}

/// 同 [`bootstrap_agent`]，但把每会话的运行期设置来源挂到 agent 上。
///
/// 这是生产入口：装配完成后，agent 会在每个安全执行点读取 `binding`，因此
/// 保存设置后的下一次语义操作就采用新值，无需重建 agent。提示词经同一个
/// [`crate::live_settings::prompt::LivePromptComposer`] 组合，稳定上下文（默认
/// 指令、`AGENTS.md`、日期）仍只在装配时取一次。
pub fn bootstrap_agent_with_binding(
    cfg: &RuntimeConfig,
    mode: PermissionMode,
    binding: crate::live_settings::binding::RuntimeSettingsBinding,
) -> Result<AgentBootstrap> {
    bootstrap_agent_inner(cfg, mode, Some(binding))
}

/// Build the per-session live source from an already-assembled catalog handle.
///
/// Hosts that assemble their own agent (the CLI's TUI/headless paths do) must
/// bind through this instead of duplicating the composer choice: the prompt the
/// static assembly produced and the prompt a live refresh produces have to come
/// from the *same* composer, otherwise every save would silently change the
/// stable context too.
///
/// The composer is the catalog handle's own when skills are enabled; without
/// skills a skill-less composer still yields the identical stable context, so a
/// live user-segment change keeps working.
pub fn live_source_for(
    cfg: &RuntimeConfig,
    catalog: Option<&SkillsCatalogHandle>,
    binding: Option<&crate::live_settings::binding::RuntimeSettingsBinding>,
) -> Option<yi_agent_core::live_settings::SharedSettingsSource> {
    let binding = binding?;
    let prompt = composer_for(cfg, catalog);
    Some(
        binding.agent_source(crate::live_settings::binding::AgentSettingsContext {
            // The declared window when present; otherwise the same 200k the start-up
            // threshold computation uses.
            model_context_length: cfg
                .model_context_length
                .unwrap_or(crate::live_settings::binding::FALLBACK_MODEL_CONTEXT_LENGTH),
            prompt,
        }),
    )
}

/// The composer a session's prompts are composed by.
///
/// A catalog handle already owns one (built with the skills service and the
/// static budget policy); without skills a skill-less composer still yields the
/// identical stable context, which is what keeps a live user-segment refresh
/// from also rewriting the default instructions or the project file.
pub fn composer_for(
    cfg: &RuntimeConfig,
    catalog: Option<&SkillsCatalogHandle>,
) -> Arc<crate::live_settings::prompt::LivePromptComposer> {
    match catalog {
        Some(handle) => Arc::clone(handle.composer()),
        None => Arc::new(
            crate::live_settings::prompt::LivePromptComposer::new(
                cfg,
                crate::live_settings::prompt::PromptRole::Normal,
                None,
                crate::live_settings::prompt::InitialCatalogPolicy::Bounded,
            )
            .expect("a composer without skills cannot fail"),
        ),
    }
}

/// Attach an assembled live source to an agent, if there is one.
///
/// A no-op for `None`, so static callers keep byte-identical behaviour.
pub fn attach_live_source(
    mut agent: yi_agent_core::Agent,
    source: Option<yi_agent_core::live_settings::SharedSettingsSource>,
) -> yi_agent_core::Agent {
    if let Some(source) = source {
        agent.set_live_settings(source);
    }
    agent
}

fn bootstrap_agent_inner(
    cfg: &RuntimeConfig,
    mode: PermissionMode,
    binding: Option<crate::live_settings::binding::RuntimeSettingsBinding>,
) -> Result<AgentBootstrap> {
    let provider = build_provider(cfg)?;

    // 唯一一份共享开关:同时交给沙箱与控制权限的 PermissionChecker,并由
    // AgentBootstrap 暴露出去(供后续 app-server 按线程切换模式)。
    let yolo_on = match mode {
        PermissionMode::Interactive => cfg.yolo,
        // AutoAllow 使该 switch 初值为 true,因此沙箱(若 `sandbox_promotable`)也会被
        // 提权到 `DangerFullAccess`——符合 design「yolo 即两层全开」;`ReadOnly` base
        // 由 `SandboxController::new` 兜底,不可提权。
        PermissionMode::AutoAllow => true,
    };
    let switch = yi_agent_core::autonomy::YoloSwitch::new(yolo_on);

    // 会话根集 = 主根 + `cfg.sandbox_writable_roots`。对主线程而言后者装的就是本次
    // 会话的关联根(app-server 由 `production_factory` 逐调用写进 `thread_cfg`);
    // 工具层与权限层必须拿到**同一份**根集,否则 `bash` 能进关联根而 `write` 不能。
    let mut roots = vec![cfg.workdir.clone()];
    roots.extend(cfg.sandbox_writable_roots.iter().cloned());

    let setup = build_tool_setup_with_switch(cfg, false, &roots, switch.clone())?;
    let checker = load_permission_checker_with_roots(&roots, switch.clone())?;

    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
    let rx_arc = Arc::new(tokio::sync::Mutex::new(decision_rx));

    let agent_config = build_agent_config(cfg, setup.system_prompt);

    // The live source is built from the same composer the static prompt came
    // from, so a hot refresh replaces exactly the user segment and the catalog
    // segment and keeps every other block byte-identical.
    let live_source = live_source_for(cfg, setup.catalog.as_ref(), binding.as_ref());
    let attach_live = |agent: yi_agent_core::Agent| attach_live_source(agent, live_source.clone());

    let provider_handle = Arc::clone(&provider);
    let tools = Arc::clone(&setup.tools);
    let catalog_for_bootstrap = setup.catalog;
    let process_manager = Arc::clone(&setup.process_manager);
    match mode {
        PermissionMode::Interactive => {
            let agent = attach_live(
                yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                    .with_permission(checker.clone(), rx_arc.clone()),
            );
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                provider: provider_handle,
                tools,
                process_manager: Arc::clone(&process_manager),
                catalog: catalog_for_bootstrap,
                decision_tx: Some(decision_tx),
                decision_rx: Some(rx_arc),
                yolo: switch.clone(),
                mcp: setup.mcp,
                sandbox: setup.sandbox,
            })
        }
        PermissionMode::AutoAllow => {
            // AutoAllow 下没有 UI 读取决定。丢弃 sender 让通道关闭:agent 侧
            // `recv()` 会立即返回 `None`,黑名单命令据此解析为 Deny,而不是
            // 永久阻塞等待一个不会到来的决定。
            drop(decision_tx);
            let agent = attach_live(
                yi_agent_core::Agent::new(provider, setup.tools, agent_config)
                    .with_permission(checker.clone(), rx_arc),
            );
            Ok(AgentBootstrap {
                agent,
                permission: checker,
                provider: provider_handle,
                tools,
                process_manager: Arc::clone(&process_manager),
                catalog: catalog_for_bootstrap,
                decision_tx: None,
                decision_rx: None,
                yolo: switch.clone(),
                mcp: setup.mcp,
                sandbox: setup.sandbox,
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
    load_permission_checker_with_switch(workdir, yi_agent_core::autonomy::YoloSwitch::new(yolo))
}

/// 加载权限检查器,使用调用方提供的共享 [`yi_agent_core::autonomy::YoloSwitch`]。
///
/// 与 [`load_permission_checker`] 的区别仅在于开关由外部传入,便于与沙箱层
/// (`build_tool_setup_with_switch`)共用同一份 `Arc`。
pub fn load_permission_checker_with_switch(
    workdir: &Path,
    switch: yi_agent_core::autonomy::YoloSwitch,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    load_permission_checker_with_roots(&[workdir.to_path_buf()], switch)
}

/// 加载权限检查器,前缀解析基点为本会话的**根集**(`roots[0]` = 主根)。
///
/// 相对路径白名单按命中的那个根解析(权限层已按根集实现),因此关联根里的 `src/**`
/// 与主根命中的是同一份白名单。权限配置文件仍只从主根读(与 `PermissionChecker::load`
/// 的既有口径一致);根集为空时退化为 `"."`,由调用方保证那一态不会出现
/// (装配期 [`build_tool_setup_with_controller`] 已对空根集报错)。
pub fn load_permission_checker_with_roots(
    roots: &[PathBuf],
    switch: yi_agent_core::autonomy::YoloSwitch,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    let roots = roots.to_vec();
    let permissions = std::thread::scope(|scope| {
        let probe = roots.clone();
        scope
            .spawn(move || {
                let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                rt.block_on(yi_agent_core::permission::PermissionChecker::load(
                    probe
                        .first()
                        .map(PathBuf::as_path)
                        .unwrap_or(Path::new(".")),
                ))
            })
            .join()
    })
    .map_err(|_| anyhow::anyhow!("permission loader thread panicked"))?
    .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?;

    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    Ok(Arc::new(
        yi_agent_core::permission::PermissionChecker::with_roots(
            permissions,
            switch,
            roots,
            blocklist_fn,
        ),
    ))
}

/// Read the project instructions file at the workdir root.
///
/// Only `<workdir>/AGENTS.md` is read: parent directories are deliberately not
/// searched, so a nested workdir never silently inherits an unrelated project's
/// rules. A missing file is the normal case (non-git directories, projects that
/// never wrote instructions) and yields `None`; any other I/O failure is logged
/// and treated as absent, because project instructions must never be able to
/// take down the agent.
pub(crate) fn load_project_instructions(workdir: &Path) -> Option<String> {
    let path = workdir.join("AGENTS.md");
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), "failed to read project instructions: {error}");
            None
        }
    }
}

/// Resolve the effective system prompt: fall back to the built-in default
/// when the user did not provide one. Project instructions from
/// `<workdir>/AGENTS.md` are appended next, then the current local date at the
/// tail so the model knows today's date without disrupting the cached prefix.
///
/// Order is deliberate and test-pinned: default → user → project → date. The
/// project block comes after the user's own prompt because it is repository
/// state, not model-native instruction, and is labelled as such.
///
/// Regression note: this loading was silently dropped when
/// `34c63d2c merge: integrate subagent runtime` resolved the prompt helper in
/// favour of a side that never carried it, so every later caller compiled
/// without a `workdir` argument and the feature stayed gone. The tests below
/// exist to make that loss impossible to repeat unnoticed.
fn resolve_system_prompt(user: Option<String>, workdir: &Path) -> Option<String> {
    // One implementation lives in the composer; this keeps the historical
    // name for callers that assemble a prompt without a live source.
    Some(crate::live_settings::prompt::base_prompt(
        user.as_deref(),
        workdir,
    ))
}

/// Set up the skills service: install bundled system skills, build roots, snapshot.
/// Returns None on hard failure (and logs a warning); the agent runs without skills.
fn build_skills_service(
    cfg: &RuntimeConfig,
) -> Result<Option<Arc<yi_agent_skills::SkillsService>>> {
    let Some(roots) = resolve_skills_roots(cfg) else {
        tracing::warn!("skills: could not determine the skills directory, skipping");
        return Ok(None);
    };
    let system_root = roots
        .iter()
        .find(|(_, scope)| *scope == yi_agent_skills::SkillScope::System)
        .map(|(path, _)| path.clone())
        .expect("resolve_skills_roots always includes the system root");

    // Install bundled skills; failure is non-fatal. This is the *only* place
    // that writes: read-only callers use `resolve_side_prompt`, which goes
    // through `SkillsService` without installing anything.
    if let Err(e) = yi_agent_skills::install_system_skills(&system_root) {
        tracing::warn!("failed to install bundled skills: {e}");
    }

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

/// 构造 catalog 刷新句柄。无 skills service 时返回 None。
fn build_catalog_handle(
    cfg: &RuntimeConfig,
    skills: &Option<Arc<yi_agent_skills::SkillsService>>,
) -> Option<SkillsCatalogHandle> {
    let service = skills.as_ref()?.clone();
    let total = service.full_catalog_size();
    let budget = resolve_catalog_budget_policy(
        total,
        cfg.skills_catalog_budget,
        cfg.skills_catalog_budget_explicit,
    );
    // This handle is the **static** assembly path: it has no live budget, so an
    // "include all" choice degenerates to an unlimited bounded render and there
    // is nothing to revoke. The live path (a per-session `AgentSettingsSource`)
    // owns the selection-time binding described in the spec.
    let composer = crate::live_settings::prompt::LivePromptComposer::new(
        cfg,
        crate::live_settings::prompt::PromptRole::Normal,
        Some(service),
        crate::live_settings::prompt::InitialCatalogPolicy::Bounded,
    )
    .ok()?;
    Some(SkillsCatalogHandle {
        composer: Arc::new(composer),
        budget,
        user_prompt: cfg.system_prompt.clone(),
        budget_source: if cfg.skills_catalog_budget_explicit {
            // An explicit CLI flag is a real fixed source, not a settings value.
            crate::live_settings::sources::ValueSource::Override
        } else {
            crate::live_settings::sources::ValueSource::Settings
        },
    })
}

/// 启动时把预算决策固化为策略:`Some(n)` 截断到 n 字节,`None` 不截断。
///
/// 仅在交互式且 catalog 超预算且未显式指定预算时才询问;刷新路径不调用本函数,
/// 因此不会在会话中途打断用户。
fn resolve_catalog_budget_policy(total: usize, budget: usize, explicit: bool) -> Option<usize> {
    if explicit || total <= budget || !is_interactive() {
        return Some(budget);
    }
    if prompt_include_all(total, budget) {
        None
    } else {
        Some(budget)
    }
}

/// The skill roots a session scans: project, user, system (`roots[2]`).
///
/// Pure resolution shared by the normal assembly and by the side-conversation
/// prompt: it reads the instance layout only, so a cold parent can resolve the
/// same roots without re-guessing a path from the real home directory.
fn resolve_skills_roots(
    cfg: &RuntimeConfig,
) -> Option<Vec<(PathBuf, yi_agent_skills::SkillScope)>> {
    let skills_root = yi_agent_store::paths::skills_dir()?;
    let system_root = skills_root.join(".system");
    Some(vec![
        (
            cfg.workdir.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::Project,
        ),
        (skills_root, yi_agent_skills::SkillScope::User),
        (system_root, yi_agent_skills::SkillScope::System),
    ])
}

/// Resolve the effective configuration for a side conversation.
///
/// This is the same, side-effect-free resolution the normal assembly performs
/// for model/effort/compact settings, so a side inherits what its parent would
/// use *right now* rather than a global default:
///
/// - `model_override` / `effort_override` are the parent's effective values
///   (`None` = inherit the base configuration);
/// - `preserve_thinking` is part of the effective config (a side keeps the
///   parent's choice, so inherited thinking credentials round-trip);
/// - thinking-effort and compaction settings come from `base` unchanged.
///
/// Nothing here touches the network, the filesystem or a provider.
pub fn resolve_side_config(
    base: &RuntimeConfig,
    cwd: &Path,
    model_override: Option<&str>,
    effort_override: Option<&str>,
    preserve_thinking: bool,
) -> Result<RuntimeConfig> {
    let _ = preserve_thinking;
    let mut cfg = base.clone();
    if let Some(model) = model_override {
        if model.trim().is_empty() {
            anyhow::bail!("模型未覆盖为空：旁支必须继承父的有效模型");
        }
        cfg.model = model.to_string();
    }
    if let Some(effort) = effort_override {
        cfg.reasoning_effort = Some(effort.to_string());
    }
    cfg.workdir = cwd.to_path_buf();
    Ok(cfg)
}

/// Resolve the side conversation's system prompt: inherited prompt, then the
/// fixed side policy at the very end.
///
/// The inherited part goes through the same [`LivePromptComposer`] as a normal
/// session, so the segment order (default → user → project AGENTS.md → date →
/// catalog) and the catalog rendering have exactly one implementation. The
/// catalog is resolved **read-only**: `SkillsService` scans and renders, but this
/// path never calls `install_system_skills`, never asks the user, never installs
/// a refresh handle and never executes a skill.
///
/// The side policy is appended by the composer as the very last block and is
/// therefore not displaceable by a later skills refresh.
///
/// [`LivePromptComposer`]: crate::live_settings::prompt::LivePromptComposer
pub fn resolve_side_prompt(cfg: &RuntimeConfig) -> Result<Option<String>> {
    let skills =
        resolve_skills_roots(cfg).map(|roots| Arc::new(yi_agent_skills::SkillsService::new(roots)));
    let composer = crate::live_settings::prompt::LivePromptComposer::new(
        cfg,
        crate::live_settings::prompt::PromptRole::Side,
        skills,
        // A side never inherits another role's "include all" decision; the
        // composer enforces that too, but pass the honest policy here.
        crate::live_settings::prompt::InitialCatalogPolicy::Bounded,
    )?;
    let prompt = composer.render(
        cfg.system_prompt.as_deref(),
        cfg.skills_catalog_budget,
        crate::live_settings::sources::ValueSource::Settings,
    )?;
    Ok(prompt)
}

fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn prompt_include_all(total: usize, default: usize) -> bool {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {total_kb} KB, exceeds default {default_kb} KB budget.\n\
         Include all skills? [Y/n]"
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sample_config;

    #[test]
    fn build_provider_rejects_an_empty_model() {
        let mut cfg = sample_config();
        cfg.model = String::new();
        let err = match build_provider(&cfg) {
            Ok(_) => panic!("an empty model must not build a provider"),
            Err(error) => error.to_string(),
        };
        assert!(err.contains("模型未配置"), "got {err}");
    }

    fn write_skills(dir: &std::path::Path, count: usize, desc_len: usize) {
        for i in 0..count {
            let d = dir.join(format!("s{i:02}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!(
                    "---\nname: s{i:02}\ndescription: {}\n---\nbody",
                    "x".repeat(desc_len)
                ),
            )
            .unwrap();
        }
    }

    /// The app-server rebuilds a thread's agent with a different tool set once
    /// the project runtime is attached. It can only do that if the bootstrap
    /// hands back the registry it built — re-deriving one would silently drop
    /// the skills and MCP wiring — and the provider, so no second provider is
    /// constructed (which would re-read the credential).
    #[test]
    fn bootstrap_exposes_the_provider_registry_and_catalog_handle() {
        let mut cfg = sample_config();
        cfg.workdir = tempfile::TempDir::new().unwrap().path().to_path_buf();
        let built = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");

        let names = built.tools.names();
        assert!(
            names.contains(&"bash".to_string()),
            "the bootstrap must expose the registry it built, got {names:?}"
        );
        assert!(
            built.catalog.is_some(),
            "the bootstrap must expose the skills catalog handle it built"
        );
        // The provider is exposed so a rebuilding caller reuses this instance
        // instead of constructing a second one. Identity cannot be compared
        // from outside the struct (a fresh `build_provider` is a different
        // allocation by definition), so assert the handle is live and shared:
        // the agent holds the other reference.
        assert!(
            Arc::strong_count(&built.provider) >= 2,
            "the exposed provider must be the instance the agent runs on"
        );
    }

    /// `build_agent_config` 把 `cfg.reasoning_effort` 搬进请求级的
    /// `gen_params.reasoning_effort`。这是 precedence 的落点：`resolve_effective`
    /// 已把「条目默认」写进 cfg，app-server 的会话覆盖又写在 cfg 之上，故 agent
    /// 主循环取 `config.gen_params` 时读到的正是最终生效档位。
    #[test]
    fn build_agent_config_carries_the_effort_into_gen_params() {
        let mut cfg = sample_config();
        cfg.reasoning_effort = Some("high".into());
        let ac = build_agent_config(&cfg, None);
        assert_eq!(ac.gen_params.reasoning_effort.as_deref(), Some("high"));
    }

    /// 未设置 effort ⇒ `None` ⇒ provider 不发该字段（零回归）。这条钉住的是
    /// 「缺省即不发」的核心契约，任何把 `None` 变成默认档位的改动都会让它失败。
    #[test]
    fn unset_reasoning_effort_stays_none() {
        let mut cfg = sample_config();
        cfg.reasoning_effort = None;
        let ac = build_agent_config(&cfg, None);
        assert_eq!(ac.gen_params.reasoning_effort, None);
    }

    #[test]
    fn agent_bootstrap_exposes_the_process_manager() {
        let mut cfg = sample_config();
        // TempDir 必须绑到具名变量(同 Task 1):匿名的临时值在本行末即被 drop,
        // 目录随之删除,manager 以 workdir 为根时会在 canonicalize 上失败。
        let tmp = tempfile::TempDir::new().unwrap();
        cfg.workdir = tmp.path().to_path_buf();
        let boot = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
        // 与 AgentBootstrap.tools 同理:调用方重建 agent 时要能沿用同一份
        // manager,否则进程列表会跟实际生效的工具集脱钩。
        assert!(boot.tools.get("process_start").is_some());
        assert_eq!(boot.process_manager.list().len(), 0);
    }

    /// Pins the invariant the test above only *names*, at the `AgentBootstrap`
    /// layer: the manager exposed as `boot.process_manager` must be the very
    /// same `Arc` the returned registry's `process_start` tool drives. The test
    /// above starts nothing and merely lists the exposed manager, so it stays
    /// green even if `bootstrap_agent` handed back a *second, independent*
    /// manager while registering the tools against the real one — precisely the
    /// forbidden refactor (a fresh manager also lists zero processes). Here the
    /// probe is started *through the registry's own tool* and must then be
    /// visible via `boot.process_manager.list()`; that can only hold while both
    /// hold one manager.
    #[test]
    fn agent_bootstrap_process_start_tool_shares_the_returned_manager() {
        let mut cfg = sample_config();
        // Same fixture rule as the test above: a *named* `TempDir` must outlive
        // the test, because the manager canonicalizes this root on `start`.
        let tmp = tempfile::TempDir::new().unwrap();
        cfg.workdir = tmp.path().to_path_buf();
        let boot = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");

        let tool = boot.tools.get("process_start").expect("registered");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.call(serde_json::json!({
            "command": "sleep 30",
            "name": "t2-arc-probe",
        })));
        assert!(
            !result.is_error,
            "process_start through the registry failed: {:?}",
            result.content
        );
        // The registry tool started the process in *its* manager; finding it
        // here proves that manager is the one `AgentBootstrap` returned.
        assert!(
            boot.process_manager
                .list()
                .iter()
                .any(|p| p.name.as_deref() == Some("t2-arc-probe")),
            "the manager on AgentBootstrap is not the one backing the registry's process_start"
        );
        // Teardown on the SAME runtime (the reader/waiter tasks live on it):
        // `on_exit` defaults to Kill, so `shutdown()` reaps the `sleep 30`.
        let _ = rt.block_on(boot.process_manager.shutdown());
    }

    #[test]
    fn build_tool_setup_with_controller_tracks_the_live_switch() {
        let cfg = sample_config();
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let controller = yi_agent_tools::SandboxController::new(
            switch.clone(),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        );
        let setup = build_tool_setup_with_controller(
            &cfg,
            false,
            std::slice::from_ref(&cfg.workdir),
            controller,
            Vec::new(),
        )
        .expect("setup");
        let bash = setup.tools.get("bash").expect("bash is registered");
        assert_eq!(
            bash.sandbox_mode(),
            Some("workspace-write"),
            "the controller's base mode must drive the root bash sandbox"
        );
        // The registry reads the same live switch, so a flip after build must
        // be visible without rebuilding the tool set.
        switch.set(true);
        assert_eq!(
            bash.sandbox_mode(),
            Some("danger-full-access"),
            "a live YOLO flip must escalate the root bash sandbox"
        );
    }

    #[test]
    fn build_tool_setup_threads_the_session_env_into_bash() {
        let mut cfg = sample_config();
        // `sample_config` 的 workdir 是 `/tmp/test-workdir`,不一定存在;
        // 真实调用 bash 要求 cwd 可进,故就地给一个临时工作区。
        let workdir = tempfile::TempDir::new().unwrap();
        cfg.workdir = workdir.path().to_path_buf();
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let controller = yi_agent_tools::SandboxController::new(
            switch,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        );
        let setup = build_tool_setup_with_controller(
            &cfg,
            false,
            std::slice::from_ref(&cfg.workdir),
            controller,
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        )
        .expect("setup");
        // bash 拿到该 env：用一次真实调用验证（沿用本文件既有 `rt.block_on(tool.call(...))` 范式）。
        let bash = setup.tools.get("bash").expect("bash is registered");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(bash.call(serde_json::json!({
            "command": "echo $YI_AGENT_THREAD_ID",
        })));
        assert!(
            !result.is_error,
            "bash through the registry failed: {:?}",
            result.content
        );
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("thread-abc"), "env must reach the child: {s}");
        } else {
            panic!("expected text block");
        }
    }

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
    fn tool_setup_exposes_the_process_manager_it_registered() {
        let mut cfg = sample_config();
        // workdir 指向临时目录:测试不许往仓库根写进程运行时目录。
        // TempDir 必须绑到具名变量——匿名的临时值在本行末即被 drop,目录随之
        // 删除,后续 `start`(manager 以 workdir 为根,并 canonicalize 它)会因
        // cwd 不存在而失败。
        let tmp = tempfile::TempDir::new().unwrap();
        cfg.workdir = tmp.path().to_path_buf();
        let setup = build_tool_setup(&cfg, false).expect("build setup");
        // manager 必须与注册表同行:注册表里有 process_start,就必须有对应的
        // manager 可查——否则 app-server 拿不到句柄,进程面板会是空的。
        assert!(setup.tools.get("process_start").is_some());
        // 起一个真实后台进程,断言它出现在同一份 manager 的 list 里。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = rt
            .block_on(
                setup
                    .process_manager
                    .start(yi_agent_tools::ProcessStartOptions {
                        command: "sleep 30".into(),
                        name: Some("t1-probe".into()),
                        cwd: None,
                        env: Default::default(),
                        on_exit: Default::default(),
                        ready_pattern: None,
                        ready_timeout_sec: None,
                        prevent_idle_sleep: false,
                    }),
            )
            .expect("start");
        assert_eq!(started.name.as_deref(), Some("t1-probe"));
        assert!(
            setup
                .process_manager
                .list()
                .iter()
                .any(|p| p.name.as_deref() == Some("t1-probe"))
        );
        // 收尾:on_exit 默认 Kill,shutdown 会杀掉它(不留孤儿 sleep 30)。
        // 必须在同一个 runtime 上 block_on —— 进程的 reader/waiter task 挂在
        // 那个 runtime 上,换一个 runtime 收尾是无效的。
        let _ = rt.block_on(setup.process_manager.shutdown());
    }

    /// Pins the invariant the test above only *names*: the manager exposed as
    /// `setup.process_manager` must be the very same `Arc` the registry's
    /// `process_start` tool drives. The previous test starts a process through
    /// `setup.process_manager` and then lists that same instance, so it stays
    /// green even if `build_tool_setup` registered a *second, independent*
    /// manager while returning a different one — precisely the forbidden
    /// refactor. Here the probe is started *through the registry's own tool*
    /// and must then be visible via `setup.process_manager.list()`; that can
    /// only hold while both hold one manager.
    #[test]
    fn tool_setup_process_start_tool_shares_the_returned_manager() {
        let mut cfg = sample_config();
        // Same fixture rule as the test above: a *named* `TempDir` must outlive
        // the test, because the manager canonicalizes this root on `start`.
        let tmp = tempfile::TempDir::new().unwrap();
        cfg.workdir = tmp.path().to_path_buf();
        let setup = build_tool_setup(&cfg, false).expect("build setup");

        let tool = setup.tools.get("process_start").expect("registered");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.call(serde_json::json!({
            "command": "sleep 30",
            "name": "t1-arc-probe",
        })));
        assert!(
            !result.is_error,
            "process_start through the registry failed: {:?}",
            result.content
        );
        // The registry tool started the process in *its* manager; finding it
        // here proves that manager is the one `ToolSetup` returned.
        assert!(
            setup
                .process_manager
                .list()
                .iter()
                .any(|p| p.name.as_deref() == Some("t1-arc-probe")),
            "the manager returned by ToolSetup is not the one backing the registry's process_start"
        );
        // Teardown on the SAME runtime (the reader/waiter tasks live on it):
        // `on_exit` defaults to Kill, so `shutdown()` reaps the `sleep 30`.
        let _ = rt.block_on(setup.process_manager.shutdown());
    }

    #[test]
    fn build_tool_setup_naked_is_empty() {
        let cfg = sample_config();
        let setup = build_tool_setup(&cfg, true).expect("build naked setup");
        assert!(setup.tools.is_empty());
        assert!(setup.catalog.is_none());
        assert!(setup.system_prompt.is_none());
    }

    #[test]
    fn catalog_handle_none_budget_does_not_truncate() {
        let tmp = tempfile::TempDir::new().unwrap();
        // 20 skills x ~600-char descriptions => full catalog well over 8192 bytes,
        // so only an effectively-unlimited budget can include the last skill.
        write_skills(tmp.path(), 20, 600);
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));

        let unlimited = SkillsCatalogHandle::for_test(svc.clone(), Some("BASE".into()), None);
        let all = unlimited.current_system_prompt().unwrap();
        assert!(
            all.len() > 8192,
            "fixture must exceed the default budget to be a meaningful test"
        );
        assert!(all.contains("s19"), "None budget must include every skill");

        let limited = SkillsCatalogHandle::for_test(svc, Some("BASE".into()), Some(400));
        let truncated = limited.current_system_prompt().unwrap();
        assert!(
            !truncated.contains("s19"),
            "a small Some budget must truncate the catalog"
        );
    }

    #[test]
    fn catalog_handle_none_budget_survives_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_skills(tmp.path(), 20, 600);
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let handle = SkillsCatalogHandle::for_test(svc, Some("BASE".into()), None);
        assert!(
            handle.current_system_prompt().unwrap().contains("s19"),
            "None budget must include every skill before refresh"
        );

        // Add one more skill, then confirm refresh picks it up with no truncation.
        write_skills(tmp.path(), 21, 600);
        assert!(
            handle.current_system_prompt().unwrap().contains("s20"),
            "newly added skill must appear under a None budget"
        );
    }

    #[test]
    fn build_system_prompt_includes_current_date() {
        let cfg = sample_config();
        let prompt = build_system_prompt(&cfg);
        assert!(prompt.contains("Current date:"), "prompt was: {prompt}");
    }

    #[test]
    fn resolve_system_prompt_none_uses_default() {
        // 用一个没有 AGENTS.md 的临时目录,隔离掉项目指令,以免宿主仓库的
        // AGENTS.md 混进断言。
        let project = tempfile::TempDir::new().unwrap();
        let resolved = resolve_system_prompt(None, project.path());
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
        let project = tempfile::TempDir::new().unwrap();
        let resolved = resolve_system_prompt(Some("custom".into()), project.path());
        let default = yi_agent_core::AgentConfig::default_system_prompt();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            resolved.as_deref().is_some_and(|r| r.starts_with(&default)
                && r.contains("User-provided instructions:\ncustom")
                && r.ends_with(&today)),
            "resolved should retain base instructions, append custom prompt, and end with date: {resolved:?}"
        );
    }

    /// 项目指令注入：`<workdir>/AGENTS.md` 必须在 user prompt 之后、日期之前
    /// 进入 system prompt。这条行为曾在 2026-08-15 的一次整合
    /// （`34c63d2c merge: integrate subagent runtime`）中被静默丢弃，本测试
    /// 钉住恢复后的顺序契约，防止再次被 merge 悄悄吃掉。
    #[test]
    fn resolve_system_prompt_appends_root_agents_md_after_custom_instructions() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(
            project.path().join("AGENTS.md"),
            "Always run focused tests.",
        )
        .expect("write AGENTS.md");

        let resolved = resolve_system_prompt(Some("Use concise output.".into()), project.path());
        let prompt = resolved.expect("normal mode should have a system prompt");

        assert!(prompt.contains("User-provided instructions:\nUse concise output."));
        assert!(prompt.contains("Project instructions (AGENTS.md):\nAlways run focused tests."));
        assert!(
            prompt.find("User-provided instructions:").unwrap()
                < prompt.find("Project instructions (AGENTS.md):").unwrap(),
            "project instructions come after the user prompt"
        );
        assert!(
            prompt.find("Project instructions (AGENTS.md):").unwrap()
                < prompt.find("Current date:").unwrap(),
            "project instructions come before the date"
        );
    }

    #[test]
    fn resolve_system_prompt_states_the_working_directory() {
        let project = tempfile::TempDir::new().unwrap();
        // 不 canonicalize：`resolve_system_prompt` 用 `workdir.display()` 原样输出，
        // macOS 上 TempDir 的 `/var/folders/...` 与 `/private/var/...` 字符串不同。
        let workdir = project.path().to_path_buf();

        let prompt = resolve_system_prompt(None, &workdir).expect("prompt");

        assert!(
            prompt.contains("## Environment"),
            "prompt must name the environment section, got: {prompt}"
        );
        assert!(
            prompt.contains(&format!("Working directory: {}", workdir.display())),
            "prompt must state the absolute working directory, got: {prompt}"
        );
    }

    /// 顺序契约：环境段在项目指令之后、日期之前。
    #[test]
    fn environment_section_sits_between_project_instructions_and_the_date() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(
            project.path().join("AGENTS.md"),
            "Always run focused tests.",
        )
        .expect("write AGENTS.md");

        let prompt =
            resolve_system_prompt(Some("Be brief.".into()), project.path()).expect("prompt");

        let custom = prompt.find("User-provided instructions:").unwrap();
        let project_rules = prompt.find("Project instructions (AGENTS.md):").unwrap();
        let environment = prompt.find("## Environment").unwrap();
        let date = prompt.find("Current date:").unwrap();

        assert!(custom < project_rules, "got: {prompt}");
        assert!(
            project_rules < environment,
            "environment follows project rules, got: {prompt}"
        );
        assert!(
            environment < date,
            "environment precedes the date, got: {prompt}"
        );
    }

    /// 没有 AGENTS.md 时环境段照常出现。
    #[test]
    fn environment_section_appears_without_project_instructions() {
        let project = tempfile::TempDir::new().unwrap();
        let prompt = resolve_system_prompt(None, project.path()).expect("prompt");

        assert!(prompt.contains("## Environment"));
        assert!(!prompt.contains("Project instructions"));
    }

    /// 缺失时不得注入任何标记，也不得报错（非 git / 无 AGENTS.md 的项目照常工作）。
    #[test]
    fn resolve_system_prompt_ignores_missing_root_agents_md() {
        let project = tempfile::TempDir::new().unwrap();
        let prompt = resolve_system_prompt(None, project.path())
            .expect("normal mode should have a system prompt");

        assert!(!prompt.contains("Project instructions"));
    }

    /// 只读项目根的 `AGENTS.md`：不得向上搜索父目录的 AGENTS.md。
    #[test]
    fn resolve_system_prompt_does_not_search_parent_directories() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("AGENTS.md"), "parent rules").unwrap();
        let child = parent.path().join("nested");
        std::fs::create_dir_all(&child).unwrap();

        // 子目录没有自己的 AGENTS.md，父目录的不得被采用。
        // 注意：TempDir 的路径可能经过符号链接（macOS 的 /var -> /private/var），
        // 因此用 canonicalize 后的 child 作 workdir。
        let workdir = child.canonicalize().unwrap();
        let prompt =
            resolve_system_prompt(None, &workdir).expect("normal mode should have a prompt");
        assert!(
            !prompt.contains("parent rules"),
            "only the workdir's own AGENTS.md may be loaded, got: {prompt}"
        );
    }

    /// workdir 下的 AGENTS.md 读取失败（如它是目录）必须降级为非致命：
    /// 只记警告，prompt 照常生成。
    #[test]
    fn resolve_system_prompt_survives_an_unreadable_agents_md() {
        let project = tempfile::TempDir::new().unwrap();
        // 把 AGENTS.md 建成目录：`read_to_string` 会以非 NotFound 的错误失败。
        std::fs::create_dir_all(project.path().join("AGENTS.md")).unwrap();

        let prompt = resolve_system_prompt(None, project.path())
            .expect("an unreadable AGENTS.md must not remove the system prompt");
        assert!(prompt.contains("Current date:"));
        assert!(!prompt.contains("Project instructions"));
    }

    #[test]
    fn resolve_catalog_budget_policy_explicit_returns_budget() {
        assert_eq!(
            resolve_catalog_budget_policy(100_000, 8192, true),
            Some(8192)
        );
        assert_eq!(resolve_catalog_budget_policy(0, 8192, true), Some(8192));
    }

    #[test]
    fn resolve_catalog_budget_policy_under_budget_returns_budget() {
        assert_eq!(resolve_catalog_budget_policy(4096, 8192, false), Some(8192));
        assert_eq!(resolve_catalog_budget_policy(8192, 8192, false), Some(8192));
        assert_eq!(resolve_catalog_budget_policy(0, 8192, false), Some(8192));
    }

    #[test]
    fn resolve_catalog_budget_policy_non_interactive_returns_budget() {
        // Tests run non-interactive (stdin is not a TTY), so even when
        // total > budget and explicit=false, return the budget without prompting.
        assert_eq!(
            resolve_catalog_budget_policy(100_000, 8192, false),
            Some(8192)
        );
    }

    #[test]
    fn catalog_handle_no_skills_returns_none() {
        let cfg = sample_config();
        assert!(build_catalog_handle(&cfg, &None).is_none());
    }

    /// catalog 路径（几乎所有正常启动都走它）必须同样带上项目指令：
    /// `build_catalog_handle` 把 `resolve_system_prompt(workdir)` 的结果作为
    /// `base_prompt`，catalog 非空时拼在它后面。这条测试钉住「项目指令在
    /// catalog 路径下不丢」，因为只修 `build_prompt_setup` 是修不到这里的。
    #[test]
    fn catalog_handle_carries_project_instructions_into_the_catalog_prompt() {
        let mut cfg = sample_config();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(
            project.path().join("AGENTS.md"),
            "MAGIC-PROJECT-RULE: always cite evidence.",
        )
        .unwrap();
        cfg.workdir = project.path().to_path_buf();

        // 一个非空的 skills service,迫使 prompt 走 catalog 拼接分支。
        let skills_dir = project.path().join(".yi-agent/skills");
        write_skills(&skills_dir, 1, 20);
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            skills_dir,
            yi_agent_skills::SkillScope::Project,
        )]));
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");

        let prompt = handle.current_system_prompt().expect("prompt");
        assert!(
            prompt.contains(
                "Project instructions (AGENTS.md):\nMAGIC-PROJECT-RULE: always cite evidence."
            ),
            "project instructions must survive the catalog path, got: {prompt}"
        );
        assert!(prompt.contains("## Skills"), "catalog is still appended");
    }

    #[test]
    fn catalog_handle_empty_catalog_returns_base_prompt() {
        let cfg = sample_config();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![]));
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");
        assert_eq!(
            handle.current_system_prompt(),
            resolve_system_prompt(None, &cfg.workdir)
        );
    }

    #[test]
    fn catalog_handle_is_byte_stable_when_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let cfg = sample_config();
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");

        let first = handle.current_system_prompt();
        let second = handle.current_system_prompt();
        assert_eq!(first, second, "unchanged catalog must render identically");
        assert!(first.unwrap().contains("foo"));
    }

    #[test]
    fn catalog_handle_picks_up_new_skill_after_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let cfg = sample_config();
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");

        let before = handle.current_system_prompt().unwrap();
        assert!(!before.contains("bar"));

        std::fs::create_dir_all(tmp.path().join("bar")).unwrap();
        std::fs::write(
            tmp.path().join("bar/SKILL.md"),
            "---\nname: bar\ndescription: y\n---\nbody",
        )
        .unwrap();

        let after = handle.current_system_prompt().unwrap();
        assert!(after.contains("bar"), "new skill must appear after refresh");
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

        let setup = build_tool_setup_in(&cfg, false, &[workspace.to_path_buf()])
            .expect("build setup in workspace");
        assert!(
            !setup.tools.is_empty(),
            "non-naked setup registers builtin tools"
        );
        assert!(
            setup.system_prompt.is_some(),
            "non-naked setup resolves a system prompt"
        );

        let naked =
            build_tool_setup_in(&cfg, true, &[workspace.to_path_buf()]).expect("build naked setup");
        assert!(naked.tools.is_empty());
        assert!(naked.system_prompt.is_none());
    }

    /// 无 `.yi-agent/mcp.json` 时 MCP 装配**仍要交出一份空 manager**,且不注册
    /// 任何 MCP 工具。
    ///
    /// 这里刻意不写「no config => None」:driver 每轮开跑前靠 manager 判断
    /// registry 是否要刷新,`mcp_reload` 也用它写盘与探测。返回 `None` 会让
    /// 「用户第一次在对话里说『帮我加一个 MCP』」这条链路断掉——配置确实写进了
    /// 盘,但 driver 手上没有 manager,既不会重载也不会重建 registry。
    #[test]
    fn build_tool_setup_without_mcp_config_still_yields_a_manager() {
        // 用全新 TempDir 而非 `sample_config()` 的固定 `/tmp/test-workdir`:后者若
        // 意外存在 `.yi-agent/mcp.json`(如被其他测试遗落)会让本测试失真。
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = sample_config();
        cfg.workdir = tmp.path().to_path_buf();

        let setup = build_tool_setup(&cfg, false).expect("build setup");
        let manager = setup
            .mcp
            .as_ref()
            .expect("no mcp.json must still yield an empty manager for the driver");
        assert!(
            manager.server_names().is_empty(),
            "the empty manager must have no servers; got {:?}",
            manager.server_names()
        );
        assert!(
            setup.tools.names().iter().all(|n| !n.starts_with("mcp__")),
            "no MCP tools should be registered without config; got {:?}",
            setup.tools.names()
        );

        // `mcp_reload` 是个例外:没有配置时它**仍然**要在,否则用户没法从
        // 对话里添加第一个 server。
        assert!(
            setup.tools.names().iter().any(|n| n == "mcp_reload"),
            "mcp_reload must be registered even without mcp.json; got {:?}",
            setup.tools.names()
        );

        // naked(子 agent 精简集)刻意返回空工具集,`mcp_reload` 也不例外:
        // 那个入口属于根 agent,子 agent 不该改写用户的 MCP 配置。
        let naked = build_tool_setup(&cfg, true).expect("build naked setup");
        assert!(naked.mcp.is_none(), "naked setup never has a manager");
        assert!(naked.tools.is_empty());
    }

    /// 没有 `mcp.json` 时,`mcp_reload` 改完配置之后,**driver 手上那份 manager
    /// 必须能看到它**。
    ///
    /// 这是「第一次加 MCP」链路的完整闭环:工具写盘 + 探测 → 同一份 manager 的
    /// 快照更新 → 下一轮 driver 的 `needs_registry_refresh` 才会返回 true 并重建
    /// registry。若两处各拿一个空 manager,配好也永远不生效——这正是修复前的情形。
    #[tokio::test]
    async fn mcp_reload_writes_into_the_same_manager_the_driver_gets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = sample_config();
        cfg.workdir = tmp.path().to_path_buf();

        let setup = build_tool_setup(&cfg, false).expect("build setup");
        let manager = setup.mcp.clone().expect("empty manager must be handed out");
        assert!(manager.server_names().is_empty(), "starts empty");

        let tool = setup
            .tools
            .get("mcp_reload")
            .expect("mcp_reload must be registered");
        // command 用 `true`:它不触发黑名单/路径校验,进程启动后立刻退出,
        // 探测会当场失败——但配置**已经写盘**,快照也照样记录了这个 server。
        let result = tool
            .call(serde_json::json!({
                "action": "add",
                "name": "fs",
                "kind": "stdio",
                "command": "true"
            }))
            .await;
        assert!(
            !result.is_error,
            "writing the config must succeed; got {result:?}"
        );

        assert!(
            manager.server_names().iter().any(|n| n == "fs"),
            "the manager handed to the driver must see the new server; got {:?}",
            manager.server_names()
        );
    }

    /// 与上一条互补的正向用例:`.yi-agent/mcp.json` 存在时必须返回 `Some(manager)`。
    /// 同时覆盖顶层路径(`workspace == cfg.workdir`)与子 agent 路径
    /// (`workspace != cfg.workdir`)——后者正是 Fix 1 存在的场景:若把 MCP 根
    /// 退回 `workspace`,子 agent 会找不到配置而返回 `None`,本测试据此守住回归。
    ///
    /// server 的 `command` 故意指向不存在的路径:探测失败只 warn 并跳过该 server,
    /// `register_mcp_tools` 仍返回 manager。此处不断言具体工具名——制造 cache hit
    /// 需复刻 yi-agent-mcp 私有的 SHA-256 指纹格式,过于脆弱,该路径已由
    /// `crates/yi-agent-mcp/src/lib.rs` 的测试覆盖。
    #[test]
    fn build_tool_setup_returns_manager_when_mcp_config_present() {
        // cfg.workdir 放项目级 mcp.json;另一个 worktree(子 agent 场景)必须仍从
        // cfg.workdir 解析 MCP 配置,而不是从 `workspace`。
        let project = tempfile::TempDir::new().unwrap();
        let worktree = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".yi-agent")).unwrap();
        std::fs::write(
            project.path().join(".yi-agent/mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"/nonexistent/definitely-not-here"}}}"#,
        )
        .unwrap();

        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();

        // 顶层路径(workspace == cfg.workdir)。
        let top = build_tool_setup(&cfg, false).expect("build setup");
        assert!(top.mcp.is_some(), "MCP config present -> manager returned");

        // 子 agent 路径:workspace 与 cfg.workdir 不同。把 MCP 根退回 `workspace`
        // 会在此处找不到配置并返回 None。
        let sub = build_tool_setup_in(&cfg, false, &[worktree.path().to_path_buf()])
            .expect("build setup in worktree");
        assert!(
            sub.mcp.is_some(),
            "MCP root must be cfg.workdir, not workspace"
        );
    }

    /// 本测试只覆盖 switch↔权限共享;沙箱路径由 tools 的 sandbox 单测覆盖,
    /// `BashTool`/`ProcessManager` 的 sandbox 字段非公开,此处无法直接观测。
    #[test]
    fn bootstrap_exposes_switch_shared_with_permission_checker() {
        let cfg = sample_config();
        let b = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");
        // Interactive + cfg.yolo=false → 开关初始关闭
        assert!(!b.yolo.get());
        // 打开共享开关:权限检查器必须立即看到(证明两者共享同一 Arc)
        b.yolo.set(true);
        assert!(b.yolo.get());
        assert!(
            b.permission.is_yolo(),
            "permission checker must share the switch"
        );
    }

    /// Interactive 模式下 `cfg.yolo=true` 必须把共享开关的初值置为开:这是
    /// app-server 工厂「mode==Yolo → thread_cfg.yolo=true → Interactive bootstrap」
    /// 映射的落点(见 `yi-agent-app-server` 的 `resume_passes_persisted_mode_to_factory`)。
    #[test]
    fn interactive_yolo_config_starts_switch_on() {
        let mut cfg = sample_config();
        cfg.yolo = true;
        let b = bootstrap_agent(&cfg, PermissionMode::Interactive).expect("bootstrap");
        assert!(b.yolo.get(), "cfg.yolo=true must seed the shared switch on");
        assert!(b.permission.is_yolo());
    }

    #[test]
    fn bootstrap_auto_allow_starts_with_switch_on() {
        let cfg = sample_config();
        let b = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
        assert!(b.yolo.get(), "auto-allow starts with yolo on");
        assert!(b.permission.is_yolo());
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

    /// 回归锁定（非 RED→GREEN）：`max_tokens` 是 Anthropic Messages API 的**必填**
    /// 字段，缺席会被判 400。Task 4 已在 `From<GenParams> for AnthropicGenParams`
    /// 里用 `DEFAULT_ANTHROPIC_MAX_TOKENS` 兜底，本测试只是钉住该不变量，防止日后
    /// 有人把兜底误改回直接透传 `None`。故此处没有失败阶段，也没有生产代码改动。
    #[test]
    fn anthropic_gen_params_always_carry_max_tokens() {
        let params = yi_agent_core::GenParams::default();
        let anthropic: yi_agent_llm::anthropic::types::AnthropicGenParams = params.into();
        assert!(
            anthropic.max_tokens.is_some(),
            "max_tokens 是 Anthropic 必填字段，转换后绝不为 None"
        );
    }

    /// 生产入口必须把运行期设置来源挂到 agent 上；静态 facade 必须不挂。
    ///
    /// 这条钉住的是「接线是否存在」：一个装配了 binding 却没有把来源交给
    /// `Agent` 的宿主，其会话会静默地停在启动值上，任何设置界面都看不出异常。
    #[test]
    fn bootstrap_with_binding_attaches_the_shared_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings_path = dir.path().join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::json!({"agent": {"max_turns": 7}}).to_string(),
        )
        .unwrap();
        // The overrides pin provider/model/key so the assembly is independent of
        // the developer's own environment; they contain no FIELDS key, so the
        // frozen source table stays empty and settings remain authoritative.
        let overrides = crate::config::ConfigOverrides {
            provider: Some("anthropic".to_string()),
            api_key: Some("sk-test".to_string()),
            model: Some("test-model".to_string()),
            workdir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let (cfg, binding) =
            crate::config::RuntimeConfig::load_lenient_with_binding(&overrides, &settings_path)
                .unwrap();

        let live = bootstrap_agent_with_binding(&cfg, PermissionMode::AutoAllow, binding.clone())
            .expect("bootstrap");
        assert!(
            live.agent.live_settings_source().is_some(),
            "binding 入口必须把来源挂到 agent 上"
        );
        let snapshot = live
            .agent
            .live_settings_source()
            .unwrap()
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.max_turns, Some(7), "来源必须读到共享设置");

        let stat = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
        assert!(
            stat.agent.live_settings_source().is_none(),
            "静态 facade 不得挂来源（旧测试与无热更新调用方的行为必须不变）"
        );

        // The same binding is meant to be cloned to every session: a second
        // save must be visible without rebuilding anything.
        binding
            .live
            .commit(
                &serde_json::json!({"agent.max_turns": 9}),
                crate::settings::store::PatchMode::ReplaceProvided,
            )
            .expect("commit");
        let snapshot = live
            .agent
            .live_settings_source()
            .unwrap()
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.max_turns, Some(9), "保存后同一 agent 应读到新值");
    }

    /// 上一条只钉住「不为 `None`」，这条钉住「缺席时落到哪个值」：`GenParams` 显式
    /// 给 `None` 时，转换后必须是 `DEFAULT_ANTHROPIC_MAX_TOKENS`，而不是任意>0 的
    /// 数字。定值断言比 `is_some()` 更紧，能挡住「兜底名存实亡、换了个魔法数」的
    /// 回归。
    #[test]
    fn anthropic_gen_params_fallback_to_default_max_tokens() {
        let params = yi_agent_core::GenParams {
            max_tokens: None,
            ..Default::default()
        };
        let anthropic: yi_agent_llm::anthropic::types::AnthropicGenParams = params.into();
        assert_eq!(
            anthropic.max_tokens,
            Some(yi_agent_llm::anthropic::types::DEFAULT_ANTHROPIC_MAX_TOKENS),
            "未指定 max_tokens 时必须落到 DEFAULT_ANTHROPIC_MAX_TOKENS"
        );
    }
}
