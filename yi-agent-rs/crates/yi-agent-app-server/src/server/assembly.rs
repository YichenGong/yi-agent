//! agent 的装配：runtime tooling、按主题重建、委派包装，以及四个会话上下文字段。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 5）。三对 `thread_local!` 槽位
//! （model_ref / reasoning_effort / extra_roots）与各自的读写函数同文件，
//! 避免跨模块访问 `LocalKey`。
//!
//! 注意 `production_factory` **不在**本文件——它在 `runtime_binding.rs`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::binding::RuntimeBinding;
use yi_agent_subagent::thread_root::ThreadRoot;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    BuiltAgent, ProjectRuntimes, apply_session, current_session_preserve_thinking, project_key,
};

/// 一个 cwd 的工具集与其权限检查器。
pub(crate) struct RuntimeTooling {
    pub(crate) registry: Arc<yi_agent_core::ToolRegistry>,
    pub(crate) permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 这一份注册表对应的 manager(与 registry 同源)。
    pub(crate) process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// The live caller transcript shared with this registry's delegation tools.
    /// `build_runtime_tooling` hands a clone to the tools and keeps this one, so
    /// binding it here (see [`wrap_for_delegation`]) is visible to those tools.
    pub(crate) caller: yi_agent_subagent::CallerContext,
    /// MCP 管理器;与 `registry` 里的 `mcp_reload` 是同一份。
    pub(crate) mcp: Option<Arc<yi_agent_mcp::McpManager>>,
}
/// Turn one bring-up choice into a binding, or the failure that explains why not.
///
/// Split out of [`attach_cwd_runtime`] so the dispatch itself is testable. The
/// alternative -- asserting through the real attach -- would need a live daemon and
/// a process-wide `OnceLock` for every case, and the interesting property here is
/// only which branch runs.
fn bring_up_with(
    bringup: &yi_agent_subagent::binding::BringUpChoice,
    cfg: &RuntimeConfig,
    runtime_dir: &Path,
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, yi_agent_subagent::attach::AttachFailure> {
    use yi_agent_subagent::attach::{
        AttachFailure, BringUpPolicy, ensure_owned_runtime, ensure_owned_runtime_with_policy,
    };

    match bringup.policy {
        // Embedded needs no scope; the four-argument form keeps its exact
        // historical behaviour, including which policy a later repair uses.
        BringUpPolicy::Embedded => {
            ensure_owned_runtime(cfg, runtime_dir.to_path_buf(), models_path).map(|attached| {
                RuntimeBinding::managed(
                    cfg,
                    runtime_dir.to_path_buf(),
                    models_path.to_path_buf(),
                    Arc::new(attached),
                )
            })
        }
        BringUpPolicy::Managed => {
            let Some(instance_home) = bringup.instance_home.as_deref() else {
                // A managed start with no scope to resolve has nothing to join and
                // must not silently degrade to an embedded one: that would leave a
                // client-owned daemon behind a policy that promises it is shared.
                return Err(AttachFailure::new(
                    "runtime bring-up",
                    "the managed policy needs the instance home this process never installed",
                ));
            };
            ensure_owned_runtime_with_policy(
                cfg,
                runtime_dir.to_path_buf(),
                models_path,
                instance_home,
                bringup.policy,
                bringup.expected.as_ref(),
            )
            .map(|attached| {
                RuntimeBinding::managed_with(
                    cfg,
                    runtime_dir.to_path_buf(),
                    models_path.to_path_buf(),
                    Arc::new(attached),
                    bringup.clone(),
                )
            })
        }
    }
}

/// Test seam: the same dispatch, exposed to the server's own tests.
#[cfg(test)]
pub(crate) fn bring_up_with_for_test(
    bringup: &yi_agent_subagent::binding::BringUpChoice,
    cfg: &RuntimeConfig,
    runtime_dir: &Path,
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, yi_agent_subagent::attach::AttachFailure> {
    bring_up_with(bringup, cfg, runtime_dir, models_path)
}

/// Test seam: an identity that is *not* the running build.
///
/// The real id comes from `yi-agent`'s build script, which this library cannot
/// read. A synthesized one is enough here because the tests only assert which
/// branch runs, never that a daemon accepted the identity.
#[cfg(test)]
pub(crate) fn test_identity(dir: &Path) -> yi_agent_lifecycle::types::ArtifactIdentity {
    yi_agent_lifecycle::types::ArtifactIdentity {
        build_id: "0".repeat(32),
        display_version: "test".to_string(),
        executable: dir.join("yi-agent"),
        sha256: String::new(),
        schema_min: yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MIN,
        schema_max: yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MAX,
        handover_version: 1,
        business_protocol_min: yi_agent_store::ipc::PROTOCOL_VERSION,
        business_protocol_max: yi_agent_store::ipc::PROTOCOL_VERSION,
        capabilities: Vec::new(),
    }
}

/// 取出(或建立)该 cwd 的项目 runtime。
pub(crate) fn attach_cwd_runtime(
    runtimes: &ProjectRuntimes,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, String> {
    let key = project_key(&cfg.workdir);
    // A project directory that no longer exists has nothing to bring up, and the
    // bring-up path creates the runtime directory -- which would resurrect
    // `.yi-agent/runtime` inside a worktree that was already reclaimed. Refuse
    // before consulting the cache, and *drop* any cached entry: the cache stores
    // failures on purpose so a doomed bring-up is not re-run on every thread, but
    // a missing directory is recoverable -- the project can come back -- and a
    // cached `Err` would lock that recovery out forever.
    if !cfg.workdir.exists() {
        runtimes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        return Err(format!(
            "the project directory {} no longer exists",
            cfg.workdir.display()
        ));
    }
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        match existing {
            Ok(binding) => return Ok(Arc::clone(binding)),
            // A failure that another attempt could fix must not be replayed: it
            // is only in the cache because the bring-up happened to run during
            // the daemon's startup window. Re-run it and let the outcome replace
            // this entry.
            Err(failure) if failure.retryable => {}
            Err(failure) => return Err(failure.to_string()),
        }
    }
    // Own a *usable* runtime: adopt a healthy daemon, replace a dead or wedged
    // one. The binding it returns heals itself on later calls, so a daemon that
    // dies after this point (e.g. its terminal closes) no longer strands the
    // thread's delegation tools.
    //
    // This is the seam: the process's bring-up choice decides whether we own an
    // embedded daemon or join a shared guardian-managed one. A production path
    // that never consults the policy cannot honour `YI_AGENT_BRINGUP=managed` at
    // all -- the policy, and everything behind it, would only ever run in tests.
    let bringup = yi_agent_subagent::attach::process_bringup_choice();
    let outcome = bring_up_with(&bringup, cfg, runtime_dir, models_path);
    let mut cache = runtimes.lock().unwrap_or_else(|p| p.into_inner());
    match &outcome {
        // Only durable conclusions are worth remembering. A transient failure is
        // not one, and storing it is exactly the bug that lost `spawn_agent`
        // process-wide; leave the key absent so the next thread simply retries.
        Err(failure) if failure.retryable => {
            cache.remove(&key);
        }
        _ => {
            cache.insert(key, outcome.clone());
        }
    }
    drop(cache);
    outcome.map_err(|failure| failure.to_string())
}
/// 断开已 attach、但已无 thread 使用的项目 root。
///
/// 判据是「该项目目录下还有没有活着的 thread」,而不是「被删的 thread 属于哪个项目」:
/// `thread/delete` 靠 `store_lookup` 按 thread_id 定位,分支里拿不到 cwd。这样删掉某个
/// 项目里的**一个** thread 不会连带废掉同项目其它 thread 的委派(daemon 对重复 detach
/// 幂等,但被断开后那个 driver 不会再激活第二次,故不能多断)。
pub(crate) fn detach_unused_runtimes(runtimes: &ProjectRuntimes, live_cwds: &[String]) {
    for binding in attached_runtimes(runtimes) {
        if live_cwds
            .iter()
            .any(|cwd| project_key(Path::new(cwd)) == project_key(&binding.project_root()))
        {
            continue;
        }
        binding.detach();
    }
}
/// 本进程已 attach 的 runtime(只看成功项)。
pub(crate) fn attached_runtimes(runtimes: &ProjectRuntimes) -> Vec<Arc<RuntimeBinding>> {
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter_map(|entry| entry.clone().ok())
        .collect()
}
/// 委派就绪的工具集。
///
/// 只有内置工具与权限根搬到 checkout;skills 与 MCP 的项目根仍取 `cfg.workdir`
/// (用户选的项目)。`yolo` 必须是该 thread 自己的开关,否则运行期切 YOLO 时沙箱层
/// 与权限层会脱钩。
// 这些参数彼此独立(装配期各自注入),拆成结构体只会把同一批值再包一层。
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
    // 本次会话的关联根（主根之外）。逐调用传入：它同时决定 FS 根集与 shell 沙箱的
    // 额外可写根，且不得跨会话共享。
    extra_roots: Vec<PathBuf>,
    // 本会话所属项目的键(见 `ThreadStore::project_key`)与该进程的组长信箱。
    // 两者绑在装配期,与下面的 thread marker 同一理由:模型看不到自己的
    // thread id,也不知道自己在哪个项目,只能由装配期注入。
    project_key: String,
    lead_inbox: crate::lead::LeadInbox,
    // 回信登记表与超时:两者一起决定 `blocking: true` 是否真的有界往返
    // (只有登记表而没有超时来源,或反之,都会让契约半途而废)。
    lead_replies: crate::lead::LeadReplies,
    // 组长写通道与结果登记表:组长的两个写工具经它们把「取消 / 打回」送到主循环。
    // 与 `lead_inbox`/`lead_replies` 同一注入理由(装配期绑,否则工具投进没人听的
    // 通道),只是方向相反——那是 thread→lead,这是 lead→thread。
    lead_control: crate::lead_control::LeadControlInbox,
    lead_control_replies: crate::lead_control::LeadControlReplies,
    // 本会话的起点 sha：`show_git_diff` 默认按它算 diff，并在回执里如实报出。
    // 与 project_key 同理——模型看不到会话元数据，装配期注入是唯一来源。
    anchor_sha: Option<String>,
    // 组长开关。`None` = 装配方漏传，按关处理（fail closed）。这里不读
    // `yi_agent_flags::is_enabled`：那是进程级量，见 `lead_gate` 的模块文档。
    lead_agent_enabled: Option<bool>,
    // 调用方额外注入给本会话 shell 的环境变量（目前只有被驱动的会话用：插件
    // 在 `launch` 动作里给它自己起的那张卡一个句柄）。宿主不认识其中任何一个
    // 键名——它只是把插件给的东西转交给 bash。
    extra_session_env: Vec<(String, String)>,
) -> Result<RuntimeTooling, String> {
    // The tools get the root handle itself, not its snapshot: they re-resolve
    // the live socket and this conversation's own root on every call.
    let workspace_root = root.handle()?.workspace_root;
    // FS 根集 = 主根（工作区）+ 关联根。shell 沙箱的额外可写根由
    // `build_tool_setup_with_controller` 内部按同一口径推导（`roots[1..]` +
    // `cfg.sandbox_writable_roots`），故此处不再单独拼一遍。
    let mut roots = vec![workspace_root.clone()];
    roots.extend(extra_roots.iter().cloned());
    // 本会话的 thread store。根取 `cfg.workdir`,它在**此处就是 thread cwd**:
    // `attach_delegation` 在调用本函数之前先做了
    // `thread_cfg.workdir = PathBuf::from(cwd)`(冷启动、resume 两条生产路径同形)。
    //
    // 刻意**不**用上面的 `workspace_root`:那是 daemon 回传的 git **toplevel**
    // (`rev-parse --show-toplevel`),而组长的 meta 落在 `<cwd>/.yi-agent/threads`。
    // 会话 cwd 是仓库子目录时两者分叉,按 toplevel 去找子目录里的组长只会 `load` 回
    // `Ok(None)`,真组长静默丢掉两个工具。用 cwd 也让工具读的 store 与
    // `thread/lead`、冷启动写 meta 的那一份**同源**。装配期用它读落盘的 `role`,
    // 判定「本 thread 是不是组长」——这是唯一权威。
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    // One controller, one truth: it backs the root's builtin tools AND the
    // subagent spawn tools, and it reads the thread's live YOLO switch.
    let controller =
        yi_agent_tools::SandboxController::new(yolo.clone(), cfg.sandbox, cfg.sandbox_promotable);
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &roots,
        controller.clone(),
        std::iter::once(("YI_AGENT_THREAD_ID".to_string(), thread_id.to_string()))
            .chain(extra_session_env)
            .collect(),
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    let process_manager = Arc::clone(&setup.process_manager);
    // One caller handle, shared with the delegation tools: the host binds it to
    // the thread's live session right after building the Agent, and the tools
    // re-read that session on every call.
    let caller = yi_agent_subagent::CallerContext::unbound();
    // The conversation marker is bound here, not inferred later: the model that
    // calls `spawn_agent` never sees a thread id, so each thread's tools carry
    // their own so every child they spawn is tagged with this conversation.
    yi_agent_subagent::register_attached_root_tools_in_thread(
        &mut registry,
        Arc::clone(root),
        controller,
        Some(thread_id.to_string()),
        caller.clone(),
    );
    let permission = yi_agent_runtime::bootstrap::load_permission_checker_with_roots(&roots, yolo)
        .map_err(|error| error.to_string())?;
    register_theme_tool(&mut registry, theme);
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
        // 仓库取 `cfg.workdir`,与上面的 `store` 同源:此处它就是 thread cwd,
        // 而不是 daemon 回传的 toplevel(见上文的注释)。
        cfg.workdir.clone(),
        anchor_sha,
    )));
    // 普通会话用它给组长留言。`from_thread_id` 绑在装配期,与
    // `register_attached_root_tools_in_thread` 的 thread marker 同理由:
    // 模型看不到自己的 thread id,只能由装配期注入。
    //
    // 两类会话**不注册**:
    // - 卡片会话(项目键落在 `.worktrees/kanban/` 下):它所在的那个 worktree 不是
    //   项目、没有组长,`ask_lead` 只会把消息投进一个空邮箱,并在主循环里经
    //   `resolve_lead_target` 给 worktree 造一个假组长(真机已有 9 例)。与
    //   `find_lead`/`is_lead_meta` 同一判定来源(`project_key` 就是
    //   `<cwd>/.yi-agent` 的 canonical 串,去掉末段即 cwd)。
    // - 组长自己:它就是那个收件人(`resolve_lead_target(project_key)` 反查回来的
    //   正是它),注册了等于自问自答——单向时每轮往自己信箱塞一条「来自会话<自己>」,
    //   阻塞时登记一个只有自己能回填的 request_id,无人应答、白等满 120s 超时。
    //   判定复用下面的 `is_lead_thread`(落盘 `ThreadMeta.role`,唯一权威),与
    //   lead_memory 的注册同源;组长要影响自己直接写即可,不需要这个工具。
    let is_lead = crate::lead_memory::is_lead_thread(&store, thread_id);
    // 组长开关只做「与」运算：它能让组长能力消失，**不能**把一个普通会话变成
    // 组长（那条链路的唯一权威仍是落盘 `ThreadMeta.role`）。
    let lead_enabled = crate::lead_gate::is_lead_agent_enabled(lead_agent_enabled);
    let state_dir = Path::new(&project_key);
    let session_cwd = state_dir.parent().unwrap_or(state_dir);
    if lead_enabled && !is_lead && !crate::plugin_worktree::is_plugin_worktree(session_cwd) {
        registry.register(Arc::new(
            // `project_key` 克隆而非 move:下面的 `if is_lead` 分支也要用它,而借用
            // 检查器不做「两分支互斥」的路径分析,任何一侧 move 都会让另一侧报
            // use-after-move。普通会话是常态,故把那次克隆放在这里(组长分支另算)。
            crate::lead::AskLeadTool::new(lead_inbox, project_key.clone(), thread_id.to_string())
                // 回信能力**必须**在这里接上:漏掉它没有编译错误,`blocking: true`
                // 只会静默退化成 P1 的显式拒绝。超时用共享常量,不写裸字面量——
                // 两个 2 分钟各自漂移时,「等待方超时」与「组长还有多久回来」会脱钩。
                .with_replies(lead_replies, crate::lead::ASK_LEAD_TIMEOUT),
        ));
    }
    // lead_memory 只给组长:普通会话与子 worker 一律看不到(spec §7.5)。判定用
    // 落盘的 `ThreadMeta.role`(唯一权威),不靠标题/名字/cwd 猜——判定函数见
    // `lead_memory::is_lead_thread`(结论已在上面的 `is_lead` 里取过一次,此处复用)。
    //
    // `project_root` 取 `cfg.workdir`(= thread cwd),理由与上面的 store 同根:
    // ① meta 落在 `<cwd>/.yi-agent/threads`,故账本也必须落在 `<cwd>/.yi-agent`
    //    下,两者才是同一个「项目」;
    // ② 本功能其余部分对「组长的项目」的定义就是 cwd —— Task 6 的后台 tick 用
    //    `session.store.project_key()`(= `<cwd>/.yi-agent`)枚举组长,
    //    `project_root_of(project_key)` 即其 parent = cwd;
    // ③ 若留成 git toplevel,`<repo>` 与 `<repo>/sub` 两个不同 project_key(两个
    //    不同组长)会共用同一份 `<repo>/.yi-agent/lead_memory/memory.md`,跨项目
    //    账本串味。
    if lead_enabled && is_lead {
        let project_root = cfg.workdir.clone();
        registry.register(Arc::new(crate::lead_memory::LeadRememberTool::new(
            project_root.clone(),
        )));
        // 只读视野工具（P3-A）：同一份 `project_root`（= thread cwd），与账本
        // 工具同源。插在 `LeadRecallTool` 之前是因为后者 move 了 `project_root`。
        registry.register(Arc::new(crate::lead_index::LeadListThreadsTool::new(
            project_root.clone(),
        )));
        registry.register(Arc::new(crate::lead_index::LeadReadThreadTool::new(
            project_root.clone(),
        )));
        registry.register(Arc::new(crate::lead_memory::LeadRecallTool::new(
            project_root,
        )));
        // 两个写工具:组长对「项目里别的 thread」的写能力(spec §3)。
        // 与账本工具同门控——`is_lead` 的唯一权威是落盘 `ThreadMeta.role`,普通会话
        // 与子 worker 一律看不到。
        //
        // `project_key` 在**上面的** `if !is_lead` 分支里已被 `AskLeadTool::new`
        // move 过一次(那条分支传的是克隆;见彼处注释),故这里拿到的是同一个
        // owned 串:第一个工具克隆一份,第二个 move 出去。
        registry.register(Arc::new(crate::lead_control::LeadInterruptTool::new(
            lead_control.clone(),
            lead_control_replies.clone(),
            project_key.clone(),
        )));
        registry.register(Arc::new(crate::lead_control::LeadRejectTool::new(
            lead_control,
            lead_control_replies,
            project_key,
        )));
    }
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
        caller,
        // `build_tool_setup_with_controller` 已经把 `mcp_reload` 注册进 registry，
        // 这里把同一份 manager 带出去，换根之后仍指向同一份状态。
        mcp: setup.mcp,
    })
}
/// 把主题工具注册进一个 thread 的工具集。
///
/// 委派可用时走 [`build_runtime_tooling`] 的 registry,不可用时走工厂里用
/// bootstrap `tools` 克隆出来的 registry——两条路径都必须注册,否则非 git
/// 目录下自然语言切主题会静默失效。
pub(crate) fn register_theme_tool(
    registry: &mut yi_agent_core::ToolRegistry,
    theme: crate::theme_tool::ThemeHandle,
) {
    registry.register(Arc::new(crate::theme_tool::SetThemeTool::new(theme)));
}
/// 克隆一份 bootstrap 工具集并补上主题工具。
///
/// 非委派路径(`Agent::new` + bootstrap 克隆)与委派路径共用同一段注册逻辑,
/// 避免两条路径漂移出「一边有 tool、一边没有」的缺口。
pub(crate) fn registry_with_theme_tool(
    tools: &Arc<yi_agent_core::ToolRegistry>,
    theme: crate::theme_tool::ThemeHandle,
) -> yi_agent_core::ToolRegistry {
    let mut registry = (**tools).clone();
    register_theme_tool(&mut registry, theme);
    registry
}
/// 用带主题工具的工具集重建 thread agent,并保留原 agent 的审批路径。
///
/// `Agent::new` 从零开始、不含权限检查器与决定接收端,所以重建后必须用
/// [`Agent::permission_checker`] / [`Agent::decision_rx`] 取回并重新装上——
/// 否则非 git 目录下的 thread 会退化成不弹审批。两个取回器都为 `None`
/// (AutoAllow)时保持不装,与原 agent 一致。
///
/// provider 沿用 bootstrap 的同一个实例:再建一个会重读凭据、多一个 client。
pub(crate) fn rebuild_thread_agent_with_theme(
    built: yi_agent_runtime::bootstrap::AgentBootstrap,
    session: Option<yi_agent_core::Session>,
    theme: crate::theme_tool::ThemeHandle,
) -> BuiltAgent {
    let yi_agent_runtime::bootstrap::AgentBootstrap {
        agent,
        provider,
        tools,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
        mcp,
        ..
    } = built;
    let mut config = agent.config().clone();
    // 让重建后的 agent 沿用本次会话的 `preserve_thinking`：`build_agent_config`
    // 只认 `RuntimeConfig`（无该字段），故值经 thread-local 由本函数落到
    // `AgentConfig` 上（镜像 effort 的做法）。未设定时按默认 `true`。
    config.preserve_thinking = current_session_preserve_thinking();
    let permission = agent.permission_checker();
    let decision = agent.decision_rx();
    // Reuse the original session `Arc`, not a deep clone: a `CallerContext`
    // bound to this thread's live conversation must keep seeing the same
    // session across the rebuild.
    let session_handle = agent.session_handle();
    let registry = registry_with_theme_tool(&tools, theme);
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
            .with_session_arc(session_handle);
    if let (Some(checker), Some(rx)) = (permission, decision) {
        rebuilt = rebuilt.with_permission(checker, rx);
    }
    apply_session(&mut rebuilt, session);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
        mcp,
    }
}
/// 把 thread 的工具集与权限根换成 attached runtime 的,其余部件沿用。
pub(crate) fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 委派用的是新工具集自带的 manager，旧的 `mcp` / `process_manager` 随之丢弃。
        ..
    } = built;
    // Reuse the live session `Arc` so the delegation tools' `CallerContext`,
    // bound just below, survives the rebuild and keeps reading the thread's
    // current transcript rather than a snapshot taken here.
    let session_handle = agent.session_handle();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session_arc(session_handle.clone());
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    // Bind the caller to the rebuilt agent's *live* session handle. `tooling`'s
    // `caller` and the registry's tool instances share one inner slot, so this
    // bind is what makes `fork: true` read this conversation.
    tooling.caller.bind(session_handle);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 注册表换了,manager 必须跟着换:否则面板查的是被丢弃的那一份。
        process_manager: tooling.process_manager,
        // MCP 管理器跟着 registry 走:换根目录后 `mcp_reload` 要改的是**新根**
        // 下的 `.yi-agent/mcp.json`。
        mcp: tooling.mcp,
    }
}
thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `model_ref`(清单显示名)。工厂闭包是
    /// `Fn` 且被 `'static` 持有,签名里带不进这个逐会话参数,故用 thread-local
    /// 传入。`build_agent` 同步执行、期间无 `await`,故槽位不会被跨任务污染。
    pub(crate) static SESSION_MODEL_REF: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}
/// 在调用工厂(`build_agent`)前设定本次会话的模型覆盖(清单显示名),供工厂解析。
///
/// 调用返回后恢复调用前的值,故嵌套调用(如 `thread/start` 内再建其它 agent)不会
/// 泄漏覆盖。`thread/start` 取请求参数、`thread/resume` 取 `loaded.meta.model_ref`。
pub(crate) fn with_session_model_ref<T>(model_ref: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_MODEL_REF.with(|slot| {
        let prev = slot.replace(model_ref);
        let out = f();
        slot.replace(prev);
        out
    })
}
/// 工厂内部读取当前会话的 `model_ref`;不在 [`with_session_model_ref`] 内时为空。
pub(crate) fn current_session_model_ref() -> Option<String> {
    SESSION_MODEL_REF.with(|slot| slot.borrow().clone())
}
thread_local! {
    /// 本次 `build_agent` 调用所属会话的 effort 覆盖（七档之一）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进这个逐会话参数。
    /// 槽位为 `Some` 时**覆盖** `resolve_effective` 从条目带出的默认值；为 `None`
    /// 时跟随条目默认。解析链「会话覆盖 → 条目默认 → 未设置」由此收敛：工厂只认
    /// 「有没有覆盖」，**没有**「强制不发」这一态——清除覆盖等同于未设置(都跟随
    /// 条目默认),这样 resume 重建与活线程经 provider 自补的路径手感一致。
    pub(crate) static SESSION_REASONING_EFFORT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}
/// 在调用工厂(`build_agent`)前设定本次会话的 effort 覆盖，供工厂解析。
///
/// `Some(tier)` 覆盖条目默认；`None` 表示本次会话没有覆盖，工厂沿用
/// `resolve_effective` 带出的条目默认。调用返回后恢复调用前的值，故嵌套调用不会
/// 泄漏覆盖。`thread/start` 传 `None`（新会话无覆盖），`thread/resume` 传
/// `meta.reasoning_effort`（清过的即为 `None` ⇒ 同样回落条目默认）。
pub(crate) fn with_session_reasoning_effort<T>(effort: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_REASONING_EFFORT.with(|slot| {
        let prev = slot.replace(effort);
        let out = f();
        slot.replace(prev);
        out
    })
}
/// 工厂内部读取当前会话的 effort 覆盖；不在
/// [`with_session_reasoning_effort`] 内时为 `None`（无覆盖，跟随条目默认）。
pub(crate) fn current_session_reasoning_effort() -> Option<String> {
    SESSION_REASONING_EFFORT.with(|slot| slot.borrow().clone())
}
thread_local! {
    /// 本次 `build_agent` 调用所属会话的**关联根**（主根之外的附属根）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进这个逐会话参数。
    ///
    /// **必须逐调用传**：daemon 的 worker factory 按项目目录构建、被同目录多个会话
    /// 共享，把根集写进共享 `cfg` 会让 A 会话的关联根泄漏给同目录的其它会话（那些
    /// 会话会凭空拿到不该有的写权限）。故这里走 thread-local，工厂每次克隆 `cfg`
    /// 时再逐调用写入。
    pub(crate) static SESSION_EXTRA_ROOTS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
/// 在调用工厂（`build_agent`）前设定本次会话的关联根，供工厂写入 `thread_cfg`。
///
/// 调用返回后恢复调用前的值，故嵌套调用（如 `thread/start` 内再建其它 agent）不会
/// 泄漏关联根。`thread/start` 取本会话的关联根、`thread/resume` 取
/// `meta.extra_roots`。
///
/// 生产调用点（`thread/start`、`thread/resume` 两个分支）各自捕获本会话的根集
/// 后传入槽位；`start_thread_core` 与 resume 侧的 `attach_delegation` 收到的都是
/// 同一个局部量，故槽位与委派路径不可能漂移。
pub(crate) fn with_session_extra_roots<T>(roots: Vec<PathBuf>, f: impl FnOnce() -> T) -> T {
    SESSION_EXTRA_ROOTS.with(|slot| {
        let prev = slot.replace(roots);
        let out = f();
        slot.replace(prev);
        out
    })
}
/// 工厂内部读取当前会话的关联根；不在 [`with_session_extra_roots`] 内时为空
/// （空 = 单根会话，与改动前逐字节一致）。
///
/// 可见性与 [`current_session_model_ref`] 一致（同为私有 `fn`）：`mod tests` 是
/// `server` 的子模块，直接可达父模块私有项，故测试无需放宽可见性。
///
/// 读点有两个：生产代码 `production_factory`，以及测试里的记录型工厂
/// （断言槽位里真正可见的根集）。
pub(crate) fn current_session_extra_roots() -> Vec<PathBuf> {
    SESSION_EXTRA_ROOTS.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
mod foreground_root_lifecycle_tests {
    /// The bring-up plan a repair reuses must be internally consistent.
    ///
    /// `RuntimeBinding` records the choice it was built with so a later `repair()`
    /// cannot switch owners mid-session: handing it `managed()`'s default would make
    /// the first heal replace a guardian-owned daemon with an embedded one, taking
    /// the authority channel with it while the conversation kept running.
    ///
    /// A half-filled managed plan is the failure worth pinning: it fails every
    /// repair with "needs the artifact identity" long after the launch that looked
    /// fine, so the assertion is on coherence, not on which policy is chosen.
    #[test]
    fn foreground_root_lifecycle_bring_up_choice_is_coherent() {
        let choice = yi_agent_subagent::attach::process_bringup_choice();
        match choice.policy {
            yi_agent_subagent::attach::BringUpPolicy::Managed => {
                assert!(
                    choice.expected.is_some(),
                    "a managed plan with no expected artifact would be refused at every repair"
                );
                assert!(
                    choice.instance_home.is_some(),
                    "a managed plan with no instance home cannot resolve its scope"
                );
            }
            yi_agent_subagent::attach::BringUpPolicy::Embedded => {
                // Embedded ignores both fields; carrying them would imply a managed
                // intent the policy does not act on.
            }
        }
    }
}
