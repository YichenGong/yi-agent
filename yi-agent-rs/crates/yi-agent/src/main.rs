//! yi-agent CLI 入口。

mod config;
mod control_commands;
mod llm_prefix;
mod schedule_intent;
mod subagent_runtime;
mod tracing_init;
mod tui;

#[cfg(test)]
mod control_commands_tests;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use yi_agent_core::Provider;
// Headless 模式的工具 + system prompt 构建结果。类型来自共享 crate
// (`ToolSetup` 的字段都是 `pub`),`build_headless_root_tools` 直接用
// `HeadlessSetup { tools, catalog, system_prompt, mcp }` 字面量构造仍然成立。
use yi_agent_runtime::bootstrap::ToolSetup as HeadlessSetup;

use crate::config::{AgentAction, Cli, Command, DaemonAction, ScheduleAction};

fn format_ipc_error(code: yi_agent_store::ipc::IpcErrorCode, message: Option<String>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _trace_guard = tracing_init::init(cli.debug);

    match cli.command {
        Some(Command::Web { ref host, ref port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env_path = config::resolve_env_path(&cli);
                let global_env_path = if config::is_workdir_explicit(&cli) {
                    None
                } else {
                    config::resolve_global_env_path()
                };
                yi_agent_web::serve(host, *port, env_path, global_env_path).await
            })
        }
        Some(Command::Run {
            ref prompt,
            json,
            stdin,
            naked,
            subagents,
        }) => {
            let prompt = prompt.clone();
            run_headless(cli, prompt, json, stdin, naked, subagents)
        }
        Some(Command::Daemon { action }) => control_daemon(&cli, action),
        Some(Command::Agents { ref project, all }) => control_agents(&cli, project.clone(), all),
        Some(Command::Agent { ref action }) => control_agent(&cli, action.clone()),
        Some(Command::Schedule { ref action }) => control_schedule(&cli, action),
        Some(Command::AppServer { ref listen }) => {
            let listen = listen.clone();
            run_app_server(cli, &listen)
        }
        None => run_agent(cli),
    }
}

/// Validate the app-server listen transport. Only the stdio transport is
/// implemented; anything else is rejected before the runtime is assembled.
fn ensure_stdio_listen(listen: &str) -> Result<()> {
    if listen != "stdio://" {
        anyhow::bail!("unsupported app-server transport `{listen}`: only `stdio://` is supported");
    }
    Ok(())
}

/// Run the JSON-RPC 2.0 app-server over stdio for the desktop GUI sidecar.
///
/// stdout is the protocol channel and must stay free of log lines; tracing
/// writes to a file (and to stderr only when `YI_LOG` is set).
fn run_app_server(cli: Cli, listen: &str) -> Result<()> {
    ensure_stdio_listen(listen)?;
    let config = config::load(&cli)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(yi_agent_app_server::run(
        tokio::io::stdin(),
        tokio::io::stdout(),
        config,
    ))
}

fn control_agents(cli: &Cli, project: Option<std::path::PathBuf>, all: bool) -> Result<()> {
    if project.is_some() {
        anyhow::bail!("project filtering is not available from this daemon version")
    }
    let workdir = config::resolve_workdir(cli)?;
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&workdir))?;
    let mut subscription = yi_agent_store::ipc::subscribe(&socket, 0).map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })?;
    let yi_agent_store::ipc::IpcResponse::Subscription(snapshot) = subscription.next_response()?
    else {
        anyhow::bail!("runtime daemon returned an invalid task snapshot")
    };
    for task in snapshot
        .tasks
        .into_iter()
        .filter(|task| all || !matches!(task.state.as_str(), "completed" | "cancelled" | "failed"))
    {
        println!("{} {}", task.task_id, task.state);
    }
    Ok(())
}

fn control_agent(cli: &Cli, action: AgentAction) -> Result<()> {
    let workdir = config::resolve_workdir(cli)?;
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&workdir))?;
    let request = match action {
        AgentAction::Show { task_id } => yi_agent_store::ipc::IpcRequest::InspectTask { task_id },
        AgentAction::Events { task_id, follow } => {
            if follow {
                return follow_task_events(&socket, task_id);
            }
            yi_agent_store::ipc::IpcRequest::ReadTaskEvents {
                task_id,
                after_event_id: None,
            }
        }
        AgentAction::Mailbox { task_id } => {
            yi_agent_store::ipc::IpcRequest::ReadTaskMailbox { task_id }
        }
        AgentAction::Diff { task_id } => yi_agent_store::ipc::IpcRequest::ReadTaskDiff { task_id },
        AgentAction::Message { task_id, text, .. } => {
            yi_agent_store::ipc::IpcRequest::SendUserMessage {
                task_id,
                message: text,
            }
        }
        AgentAction::Cancel {
            task_id,
            recursive,
            yes,
            confirmation,
        } => {
            if !yes {
                return show_cancel_preview(&socket, task_id, recursive);
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "cancel confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                task_id,
                recursive,
                confirmation_token,
            }
        }
        AgentAction::Pause { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::PauseTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Resume { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::ResumeTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Retry { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::RetryTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Accept {
            task_id,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Accept {};
            if !yes {
                return show_review_preview(&socket, task_id, decision, "accept");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        AgentAction::Rework {
            task_id,
            feedback,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Rework { feedback };
            if !yes {
                return show_review_preview(&socket, task_id, decision, "rework");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        AgentAction::Reject {
            task_id,
            reason,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Reject { reason };
            if !yes {
                return show_review_preview(&socket, task_id, decision, "reject");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        other => {
            anyhow::bail!("agent control `{other:?}` is not yet supported by this daemon version")
        }
    };
    match yi_agent_store::ipc::send_request(&socket, request).map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => {
            println!("{} {}", detail.task_id, detail.state);
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } => {
            for event in events {
                println!("{}", task_event_line(&event));
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } => {
            for message in messages {
                println!(
                    "{} {} {} {}",
                    message.message_id, message.kind, message.priority, message.payload_json
                );
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskDiff {
            task_id,
            delivery_json,
            diff,
        } => {
            println!("{task_id} {delivery_json}");
            if let Some(diff) = diff {
                println!("{diff}");
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskCancelled
        | yi_agent_store::ipc::IpcResponse::TaskPaused
        | yi_agent_store::ipc::IpcResponse::TaskResumed
        | yi_agent_store::ipc::IpcResponse::TaskRetried
        | yi_agent_store::ipc::IpcResponse::MessageQueued
        | yi_agent_store::ipc::IpcResponse::ReviewApproved
        | yi_agent_store::ipc::IpcResponse::ReviewReworkRequested
        | yi_agent_store::ipc::IpcResponse::ReviewRejected => Ok(()),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected request: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
}

fn show_review_preview(
    socket: &std::path::Path,
    task_id: String,
    decision: yi_agent_store::ipc::IpcReviewDecision,
    action: &str,
) -> Result<()> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewReview { task_id, decision },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::ReviewPreview {
            task_id,
            delivery_id,
            confirmation_token,
            expires_in_secs,
            ..
        } => {
            println!("task: {task_id}");
            println!("delivery: {delivery_id}");
            println!(
                "rerun agent {action} with --yes --confirmation {confirmation_token} within {expires_in_secs}s"
            );
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected review preview: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime review preview response: {other:?}"),
    }
}

fn show_cancel_preview(socket: &std::path::Path, task_id: String, recursive: bool) -> Result<()> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewCancel { task_id, recursive },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token,
            task_ids,
            expires_in_secs,
            ..
        } => {
            println!("affected tasks: {}", task_ids.join(", "));
            println!(
                "rerun with --yes --confirmation {confirmation_token} within {expires_in_secs}s"
            );
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected cancel preview: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime cancel preview response: {other:?}"),
    }
}

fn follow_task_events(socket: &std::path::Path, task_id: String) -> Result<()> {
    let mut subscription = yi_agent_store::ipc::subscribe_with_filters(
        socket,
        0,
        yi_agent_store::ipc::SubscriptionFilters {
            task_ids: vec![task_id],
            kinds: Vec::new(),
        },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })?;

    loop {
        match subscription.next_response()? {
            yi_agent_store::ipc::IpcResponse::Subscription(snapshot) => {
                for event in snapshot.events {
                    println!("{}", task_event_line(&event));
                }
            }
            yi_agent_store::ipc::IpcResponse::Event(event) => {
                println!("{}", task_event_line(&event));
            }
            yi_agent_store::ipc::IpcResponse::ResyncRequired => {
                anyhow::bail!(
                    "daemon event stream requires resync; rerun `yi-agent agent events --follow`"
                )
            }
            yi_agent_store::ipc::IpcResponse::Error { code, message } => {
                anyhow::bail!(
                    "runtime daemon rejected event subscription: {}",
                    format_ipc_error(code, message)
                )
            }
            other => anyhow::bail!("unexpected runtime event subscription response: {other:?}"),
        }
    }
}

fn task_event_line(event: &yi_agent_store::ipc::IpcEvent) -> String {
    format!("{} {} {}", event.event_id, event.kind, event.payload_json)
}

fn inspect_session(socket: &std::path::Path, task_id: &str) -> Result<String> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.into(),
        },
    )? {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => Ok(detail.session_id),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected request: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
}

fn control_schedule(cli: &Cli, action: &ScheduleAction) -> Result<()> {
    let ScheduleAction::Add { request, confirm } = action;
    let config = config::load(cli)?;
    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let preview = runtime.block_on(
        schedule_intent::ScheduleIntentParser::new(provider, config.model).preview(request),
    )?;
    if !*confirm {
        println!(
            "Schedule preview (not saved): {} -> {}\nRun again with --confirm to create it.",
            preview.definition.cron, preview.definition.objective
        );
        return Ok(());
    }
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&config.workdir))?;
    match yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::CreateSchedule {
            cron: preview.definition.cron,
            objective: preview.definition.objective,
        },
    )? {
        yi_agent_store::ipc::IpcResponse::ScheduleCreated { schedule_id } => {
            println!("schedule created: {schedule_id}")
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected schedule: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
    Ok(())
}

fn control_daemon(cli: &Cli, action: DaemonAction) -> Result<()> {
    let workdir = config::resolve_workdir(cli)?;
    let runtime_dir = runtime_directory_for(&workdir);
    let runtime = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    let database = runtime_database_path(&runtime_dir);
    match action {
        DaemonAction::Start => {
            if yi_agent_store::ipc::send_request(&runtime, yi_agent_store::ipc::IpcRequest::Status)
                .is_ok()
            {
                anyhow::bail!("runtime daemon is already running")
            }
            // A manually started daemon creates the same project-local state
            // root the embedded runtimes do, so it must self-ignore it too.
            ignore_project_local_runtime_state(&workdir);
            std::process::Command::new(std::env::current_exe()?)
                .args(["daemon", "serve"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(&workdir)
                .spawn()?;
            for _ in 0..50 {
                if yi_agent_store::ipc::send_request(
                    &runtime,
                    yi_agent_store::ipc::IpcRequest::Status,
                )
                .is_ok()
                {
                    println!("daemon started");
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            anyhow::bail!("daemon process did not become ready")
        }
        DaemonAction::Serve => {
            // `daemon start` spawns this subcommand detached; a directly
            // invoked `serve` must record the same self-ignore so the project
            // state it creates never dirties the checkout.
            ignore_project_local_runtime_state(&workdir);
            yi_agent_store::ipc::Daemon::start_with_factory(
                &runtime_dir,
                &database,
                build_daemon_worker_factory(
                    cli,
                    yi_agent_store::ipc::socket_path_for(&runtime_dir)?,
                )?,
            )
            .map_err(|error| anyhow::anyhow!("could not start runtime daemon: {error}"))?
            .wait()
            .map_err(|error| anyhow::anyhow!("runtime daemon failed: {error}"))
        }
        DaemonAction::Status | DaemonAction::Stop => control_daemon_client(action, &runtime),
    }
}

fn build_daemon_worker_factory(
    cli: &Cli,
    runtime_socket: std::path::PathBuf,
) -> Result<Arc<dyn yi_agent_core::subagent::worker::AgentWorkerFactory>> {
    let config = config::load(cli)?;
    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;

    // Skills-only registry: the worker's deliberate contract is to NOT register
    // builtin/process tools here (recovery path adds its own workspace-rooted set).
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(&config)?;
    let catalog = prompt.catalog;
    let mut registry = yi_agent_core::ToolRegistry::new();
    if let Some(skills) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
    }
    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, prompt.system_prompt);
    Ok(Arc::new(
        subagent_runtime::DaemonAgentWorkerFactory::new(
            provider,
            Arc::new(registry),
            agent_config,
            runtime_socket,
        )
        .with_catalog(catalog)
        // Recovery must inspect the same worktree ordinary builtin tools use.
        .with_sandbox(config.sandbox, config.sandbox_writable_roots)
        .with_workspace(config.workdir),
    ))
}

/// The runtime socket for a project, resolved identically everywhere.
///
/// The TUI's slash commands and the CLI must reach the same daemon, so both
/// resolve through here rather than each inventing their own location.
pub(crate) fn runtime_socket_for(
    workdir: &std::path::Path,
) -> Result<std::path::PathBuf, yi_agent_store::ipc::IpcError> {
    yi_agent_store::ipc::socket_path_for(&runtime_directory_for(workdir))
}

pub(crate) fn runtime_directory_for(workdir: &std::path::Path) -> std::path::PathBuf {
    runtime_directory_from(
        std::env::var_os("YI_AGENT_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from),
        workdir,
    )
}

fn runtime_directory_from(
    override_path: Option<std::path::PathBuf>,
    workdir: &std::path::Path,
) -> std::path::PathBuf {
    override_path.unwrap_or_else(|| workdir.join(".yi-agent/runtime"))
}

/// Single source of truth for the runtime database filename. The daemon and the
/// embedded TUI/headless runtimes must share one store, so they all resolve the
/// path here instead of hardcoding a name that can drift apart.
fn runtime_database_path(runtime_dir: &std::path::Path) -> std::path::PathBuf {
    runtime_dir.join("runtime.sqlite")
}

/// Keeps the daemon's own project-local state out of the checkout's git status.
///
/// The runtime state root is `<workdir>/.yi-agent/` (the daemon's `runtime/`
/// store plus the TUI's `threads/` logs). Creating it dirties a checkout that
/// does not already ignore it, and git worktree provisioning then refuses that
/// dirty parent — so the very first delegation would fail on a clean project.
/// This records the state root in the shared, untracked `.git/info/exclude`, so
/// the tool never defeats its own precondition. It is a no-op for a non-git
/// workdir or a workdir outside any repository. The entry is keyed on
/// `<workdir>/.yi-agent` even when `YI_AGENT_RUNTIME_DIR` relocates the store:
/// the TUI's `threads/` logs, the permission cache and the project `.env` still
/// live under `.yi-agent/`, so ignoring it is what keeps the checkout clean
/// regardless of the runtime directory. Recording the entry is idempotent and
/// harmless when the project already ignores the path elsewhere: git tolerates
/// redundant ignore sources.
fn ignore_project_local_runtime_state(workdir: &std::path::Path) {
    let state_root = workdir.join(".yi-agent");
    let service = yi_agent_tools::worktree::WorktreeService::new();
    match service.ignore_project_path(&state_root) {
        Ok(()) => {}
        Err(error) => {
            // Degrade to the previous behavior: a non-git or unreadable
            // workdir must not turn delegation setup into a hard failure.
            tracing::debug!(
                error = %error,
                workdir = %workdir.display(),
                "could not record the project-local runtime state in git exclude"
            );
        }
    }
}

fn control_daemon_client(action: DaemonAction, runtime: &std::path::Path) -> Result<()> {
    let request = match action {
        DaemonAction::Status => yi_agent_store::ipc::IpcRequest::Status,
        DaemonAction::Stop => yi_agent_store::ipc::IpcRequest::Stop,
        DaemonAction::Start | DaemonAction::Serve => {
            unreachable!("handled by its own arm")
        }
    };
    let response = yi_agent_store::ipc::send_request(runtime, request)
        .map_err(|error| anyhow::anyhow!("runtime daemon is unavailable: {error}"))?;
    match response {
        yi_agent_store::ipc::IpcResponse::Status {
            high_water_event_id,
        } => println!("daemon running (event high-water: {high_water_event_id})"),
        yi_agent_store::ipc::IpcResponse::Stopping => println!("daemon stopping"),
        yi_agent_store::ipc::IpcResponse::UnsupportedProtocol { .. } => {
            anyhow::bail!("runtime daemon protocol is incompatible")
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected request: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
    Ok(())
}

/// Lists reclaimable worktrees, then reclaims the automatic scope.
///
fn build_headless_root_tools(
    config: &config::Config,
    runtime_socket: std::path::PathBuf,
    attached_root: &crate::tui::subagents::AttachedRoot,
) -> Result<HeadlessSetup> {
    let setup =
        build_headless_setup_for_workspace(config, false, attached_root.workspace.path.clone())?;
    let mut registry = (*setup.tools).clone();
    crate::tui::subagents::register_attached_root_tools(
        &mut registry,
        runtime_socket,
        attached_root,
    );
    Ok(HeadlessSetup {
        tools: Arc::new(registry),
        catalog: setup.catalog,
        system_prompt: setup.system_prompt,
        mcp: setup.mcp,
    })
}

struct HeadlessRuntimeSession {
    socket_path: std::path::PathBuf,
    attached_root: crate::tui::subagents::AttachedRoot,
    embedded_daemon: Option<yi_agent_store::ipc::Daemon>,
}

fn attach_application_root_request(
    idempotency_key: String,
    workspace: std::path::PathBuf,
) -> yi_agent_store::ipc::IpcRequest {
    yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
        idempotency_key,
        workspace,
    }
}

fn attach_headless_runtime(cli: &Cli, config: &config::Config) -> Result<HeadlessRuntimeSession> {
    let runtime_dir = runtime_directory_for(&config.workdir);
    let database = runtime_database_path(&runtime_dir);
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    // Record the project-local state before the daemon creates it, otherwise
    // root worktree provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&config.workdir);
    let embedded_daemon = match yi_agent_store::ipc::Daemon::start_with_factory(
        &runtime_dir,
        &database,
        build_daemon_worker_factory(cli, socket_path.clone())?,
    ) {
        Ok(daemon) => Some(daemon),
        Err(yi_agent_store::ipc::IpcError::AlreadyRunning { .. }) => None,
        Err(error) => anyhow::bail!("could not start subagent runtime: {error}"),
    };
    let response = yi_agent_store::ipc::send_request(
        &socket_path,
        attach_application_root_request(
            format!(
                "headless:{}:{}:{}",
                std::process::id(),
                config.workdir.display(),
                uuid::Uuid::new_v4()
            ),
            config.workdir.clone(),
        ),
    )
    .map_err(|error| anyhow::anyhow!("could not attach headless subagent runtime: {error}"))?;
    let yi_agent_store::ipc::IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        anyhow::bail!("{}", runtime_attached_root_rejection(&response));
    };
    Ok(HeadlessRuntimeSession {
        socket_path,
        attached_root: crate::tui::subagents::AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        },
        embedded_daemon,
    })
}

fn activate_headless_runtime_root(runtime: &HeadlessRuntimeSession, objective: &str) -> Result<()> {
    activate_tui_runtime_root(&runtime.socket_path, &runtime.attached_root, objective)
        .map_err(|error| anyhow::anyhow!("could not activate headless subagent runtime: {error}"))
}

fn detach_headless_runtime_root(runtime: &HeadlessRuntimeSession) {
    detach_tui_runtime_root(&runtime.socket_path, &runtime.attached_root);
}

fn build_tui_root_tools(
    base_registry: &yi_agent_core::ToolRegistry,
    config: &config::Config,
    runtime_socket: std::path::PathBuf,
    attached_root: &crate::tui::subagents::AttachedRoot,
) -> yi_agent_core::ToolRegistry {
    let mut registry = base_registry.clone();
    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        attached_root.workspace.path.clone(),
        config.sandbox,
        config.sandbox_writable_roots.clone(),
    );
    crate::tui::subagents::register_attached_root_tools(
        &mut registry,
        runtime_socket,
        attached_root,
    );
    registry
}

/// The outcome of trying to attach the TUI to a runtime.
///
/// `Unavailable` is not an error: delegation is optional, so a runtime that
/// cannot start only disables delegation. It still carries the reason, so the
/// degradation is visible instead of appearing as a mysteriously absent tool.
enum TuiRuntimeSession {
    Attached(Box<AttachedTuiRuntime>),
    Unavailable { reason: String },
}

struct AttachedTuiRuntime {
    socket_path: std::path::PathBuf,
    attached_root: crate::tui::subagents::AttachedRoot,
    embedded_daemon: Option<yi_agent_store::ipc::Daemon>,
}

impl TuiRuntimeSession {
    fn unavailable(reason: String) -> Self {
        Self::Unavailable { reason }
    }
}

/// Explains, in one actionable line, why subagent delegation is unavailable.
///
/// Delegation is optional: a runtime that cannot start degrades the session to
/// no-delegation rather than aborting it. That degradation must still be
/// visible, because the symptom the user sees is merely a missing tool, which
/// is impossible to diagnose from the outside.
fn runtime_unavailable_reason(error: &yi_agent_store::ipc::IpcError) -> String {
    format!("subagent delegation unavailable: {error}")
}

/// Turns a rejected attach response into an actionable, human-readable reason.
///
/// A raw `{response:?}` dump buries the real cause (for example a dirty
/// checkout) behind `Error { code: InvalidState, message: Some(...) }`, which
/// reads like an internal admission fault rather than a local git state
/// problem the user can fix.
fn runtime_attached_root_rejection(response: &yi_agent_store::ipc::IpcResponse) -> String {
    match response {
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: Some(message),
        } => format!(
            "daemon rejected the runtime attachment: {code}: {message}; \
             subagent delegation is disabled for this session"
        ),
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: None,
        } => format!(
            "daemon rejected the runtime attachment: {code}; \
             subagent delegation is disabled for this session"
        ),
        other => format!("daemon rejected the runtime attachment: {other:?}"),
    }
}

/// Builds the notice emitted when subagent-runtime bring-up fails after the
/// user asked for it (`y`, or a remembered `always`).
///
/// Returns the event rather than sending it so the classification is testable
/// without a live driver: the original bug was that this path produced
/// `AgentEvent::Error(ProviderTurnAdmission)`, rendering the raw cause as
/// `Error: provider turn admission failed: ...` with no hint that a restart
/// restores delegation. `stage` names the failing bring-up step and `cause`
/// carries the diagnostic; both go to the trace, never to the user's line.
fn runtime_unavailable_event(stage: &str, cause: impl Into<String>) -> yi_agent_core::AgentEvent {
    yi_agent_core::AgentEvent::SubagentRuntimeUnavailable {
        stage: stage.to_owned(),
        cause: cause.into(),
    }
}

/// Maps the persisted runtime preference to what the TUI should do on launch.
///
/// The preference file is the single source of truth: a missing or malformed
/// file reads as `Ask`, so existing users keep today's behaviour.
fn startup_intent_for(
    workdir: &std::path::Path,
) -> Option<crate::tui::subagents::RuntimeStartupIntent> {
    use crate::tui::runtime_prefs::{self, RuntimePreference};
    use crate::tui::subagents::RuntimeStartupIntent;

    Some(match runtime_prefs::load(workdir) {
        RuntimePreference::Ask => RuntimeStartupIntent::Prompt,
        RuntimePreference::Always => RuntimeStartupIntent::AutoStart,
        RuntimePreference::Never => RuntimeStartupIntent::DisabledNotice {
            reason: format!(
                "已禁用子 Agent 委派（{}: never）；用 /runtime 开启",
                runtime_prefs::preferences_path(workdir).display()
            ),
        },
    })
}

fn attach_tui_runtime(cli: &Cli, config: &config::Config) -> Result<Option<TuiRuntimeSession>> {
    let runtime_dir = runtime_directory_for(&config.workdir);
    let database = runtime_database_path(&runtime_dir);
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    // Record the project-local state before the daemon creates it, otherwise
    // root worktree provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&config.workdir);
    let embedded_daemon = match yi_agent_store::ipc::Daemon::start_with_factory(
        &runtime_dir,
        &database,
        build_daemon_worker_factory(cli, socket_path.clone())?,
    ) {
        Ok(daemon) => Some(daemon),
        Err(yi_agent_store::ipc::IpcError::AlreadyRunning { .. }) => None,
        Err(error) => {
            let reason = runtime_unavailable_reason(&error);
            tracing::warn!(error = %error, "{reason}");
            return Ok(Some(TuiRuntimeSession::unavailable(reason)));
        }
    };
    let idempotency_key = format!(
        "tui:{}:{}:{}",
        std::process::id(),
        config.workdir.display(),
        uuid::Uuid::new_v4()
    );
    let response = match yi_agent_store::ipc::send_request(
        &socket_path,
        attach_application_root_request(idempotency_key, config.workdir.clone()),
    ) {
        Ok(response) => response,
        Err(error) => {
            let reason = format!("could not reach the runtime socket: {error}");
            tracing::warn!(error = %error, "{reason}");
            return Ok(Some(TuiRuntimeSession::Unavailable { reason }));
        }
    };
    let yi_agent_store::ipc::IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        let reason = runtime_attached_root_rejection(&response);
        tracing::warn!(response = ?response, "{reason}");
        return Ok(Some(TuiRuntimeSession::Unavailable { reason }));
    };
    Ok(Some(TuiRuntimeSession::Attached(Box::new(
        AttachedTuiRuntime {
            socket_path,
            attached_root: crate::tui::subagents::AttachedRoot {
                session_id,
                task_id: root_task_id,
                capability: message_capability,
                workspace,
            },
            embedded_daemon,
        },
    ))))
}

fn activate_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
    objective: &str,
) -> Result<()> {
    match yi_agent_store::ipc::send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::ActivateApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
            objective: objective.to_owned(),
        },
    )? {
        yi_agent_store::ipc::IpcResponse::ApplicationRootActivated => Ok(()),
        other => anyhow::bail!("daemon rejected TUI runtime activation: {other:?}"),
    }
}

fn detach_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
) {
    let response = yi_agent_store::ipc::send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::DetachApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
        },
    );
    if let Err(error) = response {
        tracing::warn!(error = %error, "could not detach TUI runtime root");
    }
}

fn load_permission_checker_for_workdir(
    workdir: std::path::PathBuf,
    config: &config::Config,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    yi_agent_runtime::bootstrap::load_permission_checker(&workdir, config.yolo)
}

async fn load_permission_checker_for_workdir_async(
    workdir: std::path::PathBuf,
    config: &config::Config,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    let yolo = config.yolo;
    tokio::task::spawn_blocking(move || {
        yi_agent_runtime::bootstrap::load_permission_checker(&workdir, yolo)
    })
    .await
    .map_err(|e| anyhow::anyhow!("permission loader task failed: {e}"))?
}

fn run_agent(cli: Cli) -> Result<()> {
    let config = config::load(&cli)?;

    let agent_workdir = config.workdir.clone();

    // Load permissions and construct checker for the initial project workspace.
    let checker = load_permission_checker_for_workdir(agent_workdir.clone(), &config)?;
    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);

    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;

    let mut registry = yi_agent_core::ToolRegistry::new();

    // --- Skills system setup ---
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(&config)?;

    // Register Skill tool
    if let Some(svc) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    // `base_registry` MUST stay skills-only: `build_tui_root_tools` clones it
    // and adds workspace-rooted builtin tools + subagent tools on top.
    let base_registry = registry.clone();
    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        config.workdir.clone(),
        config.sandbox,
        config.sandbox_writable_roots.clone(),
    );
    let process_manager = yi_agent_tools::ProcessManager::with_sandbox(
        config.workdir.clone(),
        yi_agent_tools::SandboxPolicy::new(
            config.sandbox,
            &config.workdir,
            config.sandbox_writable_roots.clone(),
        ),
    );
    yi_agent_tools::register_process_tools(&mut registry, process_manager.clone());

    // Register MCP tools and keep the manager so the TUI can toggle servers and
    // the driver can refresh the registry at runtime. A bad MCP config must never
    // block the agent: warn and continue with an empty manager.
    let mcp = match yi_agent_mcp::register_mcp_tools(&mut registry, &config.workdir) {
        Ok(Some(manager)) => manager,
        Ok(None) => yi_agent_mcp::McpManager::empty(),
        Err(err) => {
            tracing::warn!("MCP disabled: {err:#}");
            yi_agent_mcp::McpManager::empty()
        }
    };

    let tools = Arc::new(registry);

    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, prompt.system_prompt);

    run_tui_agent(
        provider,
        tools,
        agent_config,
        agent_workdir,
        checker,
        decision_tx,
        decision_rx,
        cli,
        config,
        base_registry,
        process_manager,
        prompt.catalog,
        mcp,
    )
}

/// Drain an `AgentEvent` stream to the provided writers in human-readable
/// (non-JSON) form. Returns the process exit code.
///
/// `AssistantText` deltas are written inline without forcing a newline
/// after every chunk, so streaming text renders as one continuous line
/// (the LLM's own newlines are preserved). A trailing newline is emitted
/// at the end of the stream if the last assistant text did not end with
/// one, so the shell prompt (or any following output) starts on a fresh
/// line.
///
/// ToolCall / ToolResult / Done / Cancelled / Error are routed to `err`.
async fn drain_stream_human<W: std::io::Write, E: std::io::Write>(
    stream: futures::stream::BoxStream<'static, yi_agent_core::AgentEvent>,
    out: &mut W,
    err: &mut E,
) -> i32 {
    use futures::StreamExt;

    let mut stream = Box::pin(stream);
    let mut exit_code = 0;
    // True when the last bytes written to `out` did NOT end with '\n'.
    // Used to ensure we terminate assistant text before returning so the
    // shell prompt starts on a fresh line.
    let mut mid_line = false;

    while let Some(event) = stream.next().await {
        match &event {
            yi_agent_core::AgentEvent::AssistantText(t) => {
                let _ = out.write_all(t.as_bytes());
                mid_line = !t.ends_with('\n');
            }
            yi_agent_core::AgentEvent::ToolCall { name, input, .. } => {
                let _ = writeln!(err, "[tool:{name}] {input}");
            }
            yi_agent_core::AgentEvent::ToolResult { id, result } => {
                let _ = writeln!(
                    err,
                    "[result:{id}] error={} content={:?}",
                    result.is_error, result.content
                );
            }
            yi_agent_core::AgentEvent::ToolRetry { id } => {
                let _ = writeln!(err, "[tool-retry:{id}]");
            }
            yi_agent_core::AgentEvent::ProviderRetry {
                attempt,
                max,
                cause,
                ..
            } => {
                // The stall line keeps its historical shape (no suffix) so
                // existing log scrapers stay valid; a timeout adds one.
                match cause {
                    yi_agent_core::RetryCause::IdleStall => {
                        let _ = writeln!(err, "[provider-retry:{attempt}/{max}]");
                    }
                    yi_agent_core::RetryCause::RequestTimeout => {
                        let _ = writeln!(err, "[provider-retry:{attempt}/{max} timeout]");
                    }
                }
            }
            yi_agent_core::AgentEvent::Done { reason } => match reason {
                // Normal completion is already signaled by exit code 0; the
                // [done:EndTurn] line is noise on stderr and is suppressed
                // to match the TUI, which renders EndTurn as a silent
                // separator. Only abnormal non-error terminations emit a
                // diagnostic line.
                yi_agent_core::DoneReason::EndTurn => {}
                yi_agent_core::DoneReason::MaxTurns => {
                    let _ = writeln!(err, "[done:{reason:?}]");
                }
                yi_agent_core::DoneReason::Interrupted { reason } => {
                    let _ = writeln!(err, "[interrupted:{reason}]");
                    exit_code = 1;
                }
            },
            yi_agent_core::AgentEvent::Cancelled => {
                let _ = writeln!(err, "[cancelled]");
                exit_code = 130;
            }
            yi_agent_core::AgentEvent::Error(e) => {
                let _ = writeln!(err, "[error:{e}]");
                exit_code = 1;
            }
            _ => {}
        }
        if matches!(
            event,
            yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Cancelled
                | yi_agent_core::AgentEvent::Error(_)
        ) {
            break;
        }
    }

    if mid_line {
        let _ = out.write_all(b"\n");
    }

    exit_code
}

/// Drain an `AgentEvent` stream to the provided writer as JSONL (one JSON
/// object per line). Returns the process exit code.
async fn drain_stream_json<W: std::io::Write>(
    stream: futures::stream::BoxStream<'static, yi_agent_core::AgentEvent>,
    out: &mut W,
) -> i32 {
    use futures::StreamExt;

    let mut stream = Box::pin(stream);
    let exit_code = 0;
    while let Some(event) = stream.next().await {
        let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
        let _ = writeln!(out, "{line}");
        if matches!(
            event,
            yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Cancelled
                | yi_agent_core::AgentEvent::Error(_)
        ) {
            break;
        }
    }
    exit_code
}

/// 根据 `naked` flag 构建 headless 模式用的工具集和 system prompt。
///
/// `naked = true`:不注册任何工具,不加载 skills,`system_prompt = None`(裸模型)。
/// `naked = false`:与 TUI `run_agent` 对齐 — 注册内置工具、加载 skills、
/// 注册 SkillTool、解析默认 prompt + 当前日期 + skills catalog。
fn build_headless_setup(config: &config::Config, naked: bool) -> Result<HeadlessSetup> {
    build_headless_setup_for_workspace(config, naked, config.workdir.clone())
}

fn build_headless_setup_for_workspace(
    config: &config::Config,
    naked: bool,
    workspace: std::path::PathBuf,
) -> Result<HeadlessSetup> {
    yi_agent_runtime::bootstrap::build_tool_setup_in(config, naked, &workspace)
}

/// Run agent non-interactively: drain AgentEvent stream to stdout/stderr.
/// Used for headless CLI usage and end-to-end real-LLM testing.
fn run_headless(
    cli: Cli,
    prompt: Option<String>,
    json: bool,
    from_stdin: bool,
    naked: bool,
    subagents: bool,
) -> Result<()> {
    let config = config::load(&cli)?;

    // Resolve prompt: explicit stdin flag > no prompt arg > prompt arg
    let prompt_text = match (from_stdin, prompt) {
        (true, _) | (false, None) => {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim_end_matches('\n').to_string()
        }
        (false, Some(p)) => p,
    };
    if prompt_text.is_empty() {
        anyhow::bail!("empty prompt");
    }

    if subagents && naked {
        anyhow::bail!("--subagents cannot be combined with --naked");
    }

    let headless_runtime = if subagents {
        let runtime = attach_headless_runtime(&cli, &config)?;
        activate_headless_runtime_root(&runtime, &prompt_text)?;
        Some(runtime)
    } else {
        None
    };
    let workdir = headless_runtime
        .as_ref()
        .map(|runtime| runtime.attached_root.workspace.path.clone())
        .unwrap_or_else(|| config.workdir.clone());
    // Headless mode: auto-allow non-blacklisted tools (yolo behavior)
    let checker = yi_agent_runtime::bootstrap::load_permission_checker(&workdir, true)?;
    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
    // Headless mode has no confirmation UI. Closing the sender makes a
    // blacklisted command resolve as Deny instead of waiting forever.
    drop(decision_tx);

    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;

    let setup = match &headless_runtime {
        Some(runtime) => {
            build_headless_root_tools(&config, runtime.socket_path.clone(), &runtime.attached_root)?
        }
        None => build_headless_setup(&config, naked)?,
    };
    let tools = setup.tools;

    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, setup.system_prompt);
    let mcp = setup.mcp;

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async move {
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        let mut agent = yi_agent_core::Agent::new(provider, tools, agent_config)
            .with_permission(checker, decision_rx);

        let stream = match agent.run(prompt_text).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };

        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        if json {
            drain_stream_json(stream, &mut out).await
        } else {
            drain_stream_human(stream, &mut out, &mut err).await
        }
    });

    // Close MCP server connections on a running reactor so the child processes
    // are reaped; `std::process::exit` below would skip destructors. Done after
    // `block_on` so it also covers the early-error exit path.
    if let Some(manager) = &mcp {
        rt.block_on(manager.shutdown());
    }

    if let Some(runtime) = &headless_runtime {
        detach_headless_runtime_root(runtime);
        let _embedded_daemon = runtime.embedded_daemon.as_ref();
    }
    drop(headless_runtime);
    std::process::exit(exit_code);
}

/// Run the ratatui TUI. Sets up channels, spawns agent driver task, calls run_tui.
#[allow(clippy::too_many_arguments)]
fn run_tui_agent(
    provider: Arc<dyn Provider>,
    tools: Arc<yi_agent_core::ToolRegistry>,
    agent_config: yi_agent_core::AgentConfig,
    workdir: std::path::PathBuf,
    checker: Arc<yi_agent_core::permission::PermissionChecker>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    decision_rx: tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
    cli: Cli,
    config: config::Config,
    base_registry: yi_agent_core::ToolRegistry,
    process_manager: Arc<yi_agent_tools::ProcessManager>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
) -> Result<()> {
    use futures::StreamExt;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;

    let rt = tokio::runtime::Runtime::new()?;
    let tui_result = rt.block_on(async move {
        // Channels between agent driver and TUI
        let (agent_tx, agent_rx) = mpsc::channel::<yi_agent_core::AgentEvent>(256);
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<ControlCommand>(8);
        let (runtime_choice_tx, mut runtime_choice_rx) =
            mpsc::channel::<crate::tui::subagents::RuntimeStartupChoice>(1);
        let runtime_detach = Arc::new(std::sync::Mutex::new(None::<(
            std::path::PathBuf,
            crate::tui::subagents::AttachedRoot,
        )>));
        let is_running = Arc::new(AtomicBool::new(false));

        // `always` reuses the existing attach path by pre-seeding the choice the
        // driver would otherwise wait for. Capacity is 1 and nothing has been
        // sent yet, so `try_send` cannot fail here.
        let runtime_intent = startup_intent_for(&workdir);
        if matches!(
            runtime_intent,
            Some(crate::tui::subagents::RuntimeStartupIntent::AutoStart)
        ) {
            let _ = runtime_choice_tx
                .try_send(crate::tui::subagents::RuntimeStartupChoice::Start);
        }

        enum DriverInput {
            Prompt(Option<String>),
            Control(Option<ControlCommand>),
            RuntimeChoice(Option<crate::tui::subagents::RuntimeStartupChoice>),
        }

        // Spawn agent driver task (stays on the async runtime)
        let provider_clone = Arc::clone(&provider);
        let mut current_tools = Arc::clone(&tools);
        let mut current_checker = Arc::clone(&checker);
        let config_clone = agent_config.clone();
        let is_running_clone = Arc::clone(&is_running);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        let rebuild_provider = Arc::clone(&provider);
        let rebuild_config = agent_config.clone();
        let rebuild_decision_rx = Arc::clone(&decision_rx);
        let runtime_detach_for_driver = Arc::clone(&runtime_detach);
        let mcp_for_driver = Arc::clone(&mcp);
        let mcp_for_teardown = Arc::clone(&mcp);
        let driver = tokio::spawn(async move {
            let mut root_activated = false;
            let mut current_runtime: Option<TuiRuntimeSession> = None;
            let catalog = catalog;
            let mut agent = yi_agent_core::Agent::new(
                Arc::clone(&provider_clone),
                Arc::clone(&current_tools),
                config_clone,
            )
            .with_permission(Arc::clone(&current_checker), Arc::clone(&decision_rx));
            let _ = workdir; // workdir already passed to tools registration

            loop {
                // Wait for user input, runtime startup choice, or a control command.
                let input = tokio::select! {
                    biased;
                    cmd = control_rx.recv() => DriverInput::Control(cmd),
                    choice = runtime_choice_rx.recv() => DriverInput::RuntimeChoice(choice),
                    text = input_rx.recv() => DriverInput::Prompt(text),
                };

                // Handle control commands first (rebuild agent, no prompt run).
                if let DriverInput::Control(Some(cmd)) = input {
                    match cmd {
                        ControlCommand::Clear => {
                            // Rebuild agent with empty session.
                            agent = yi_agent_core::Agent::new(
                                Arc::clone(&rebuild_provider),
                                Arc::clone(&current_tools),
                                rebuild_config.clone(),
                            )
                            .with_session(yi_agent_core::Session::new())
                            .with_permission(
                                Arc::clone(&current_checker),
                                Arc::clone(&rebuild_decision_rx),
                            );
                            tracing::info!("agent session cleared via /clear");
                        }
                        ControlCommand::Compact => {
                            let session = agent.session();
                            match yi_agent_core::compact_session(
                                &rebuild_provider,
                                &rebuild_config,
                                &session,
                            )
                            .await
                            {
                                Ok(Some(new_session)) => {
                                    agent = yi_agent_core::Agent::new(
                                        Arc::clone(&rebuild_provider),
                                        Arc::clone(&current_tools),
                                        rebuild_config.clone(),
                                    )
                                    .with_session(new_session)
                                    .with_permission(
                                        Arc::clone(&current_checker),
                                        Arc::clone(&rebuild_decision_rx),
                                    );
                                    tracing::info!("agent session compacted via /compact");
                                }
                                Ok(None) => {
                                    tracing::info!("no compactable session history");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "compact failed");
                                    let _ =
                                        agent_tx.send(yi_agent_core::AgentEvent::Error(e)).await;
                                }
                            }
                        }
                        ControlCommand::McpRefresh => {
                            let mut refreshed = (*current_tools).clone();
                            mcp_for_driver.refresh_registry(&mut refreshed);
                            current_tools = Arc::new(refreshed);
                            agent = yi_agent_core::Agent::new(
                                Arc::clone(&rebuild_provider),
                                Arc::clone(&current_tools),
                                rebuild_config.clone(),
                            )
                            .with_session(agent.session())
                            .with_permission(
                                Arc::clone(&current_checker),
                                Arc::clone(&rebuild_decision_rx),
                            );
                            tracing::info!("MCP tool registry refreshed");
                        }
                    }
                    continue;
                }

                if let DriverInput::RuntimeChoice(choice) = input {
                    match choice {
                        Some(crate::tui::subagents::RuntimeStartupChoice::Start) => {
                            match attach_tui_runtime(&cli, &config) {
                                Ok(Some(TuiRuntimeSession::Unavailable { reason })) => {
                                    tracing::warn!(
                                        cause = %reason,
                                        "subagent runtime unavailable; continuing without delegation"
                                    );
                                    let _ = agent_tx
                                        .send(runtime_unavailable_event("runtime attach", reason))
                                        .await;
                                }
                                Ok(Some(TuiRuntimeSession::Attached(attached))) => {
                                    let AttachedTuiRuntime {
                                        socket_path,
                                        attached_root,
                                        embedded_daemon,
                                    } = *attached;
                                    if embedded_daemon.is_some() {
                                        tracing::info!("embedded subagent runtime started for TUI");
                                    }
                                    *runtime_detach_for_driver
                                        .lock()
                                        .expect("runtime detach mutex poisoned") = Some((
                                        socket_path.clone(),
                                        attached_root.clone(),
                                    ));
                                    crate::tui::subagents::set_current_attached_root(
                                        attached_root.clone(),
                                    );
                                    let runtime_workdir = attached_root.workspace.path.clone();
                                    let mut next_registry = build_tui_root_tools(
                                        &base_registry,
                                        &config,
                                        socket_path.clone(),
                                        &attached_root,
                                    );
                                    mcp_for_driver.refresh_registry(&mut next_registry);
                                    let next_tools = Arc::new(next_registry);
                                    match load_permission_checker_for_workdir_async(runtime_workdir, &config).await {
                                        Ok(next_checker) => {
                                            let session = agent.session();
                                            current_tools = next_tools;
                                            current_checker = next_checker;
                                            agent = yi_agent_core::Agent::new(
                                                Arc::clone(&rebuild_provider),
                                                Arc::clone(&current_tools),
                                                rebuild_config.clone(),
                                            )
                                            .with_session(session)
                                            .with_permission(
                                                Arc::clone(&current_checker),
                                                Arc::clone(&rebuild_decision_rx),
                                            );
                                            current_runtime =
                                                Some(TuiRuntimeSession::Attached(Box::new(
                                                    AttachedTuiRuntime {
                                                        socket_path,
                                                        attached_root,
                                                        embedded_daemon,
                                                    },
                                                )));
                                            root_activated = false;
                                        }
                                        Err(error) => {
                                            if let Some((socket, root)) = runtime_detach_for_driver
                                                .lock()
                                                .expect("runtime detach mutex poisoned")
                                                .take()
                                            {
                                                detach_tui_runtime_root(&socket, &root);
                                            }
                                            let cause = error.to_string();
                                            tracing::warn!(
                                                %cause,
                                                "subagent runtime attach failed; continuing without delegation"
                                            );
                                            let _ = agent_tx
                                                .send(runtime_unavailable_event(
                                                    "runtime attach",
                                                    cause,
                                                ))
                                                .await;
                                        }
                                    }
                                }
                                Ok(None) => {
                                    tracing::warn!("subagent runtime unavailable; continuing without delegation");
                                }
                                Err(error) => {
                                    let _ = agent_tx
                                        .send(yi_agent_core::AgentEvent::Error(
                                            yi_agent_core::AgentError::ProviderTurnAdmission(
                                                error.to_string(),
                                            ),
                                        ))
                                        .await;
                                }
                            }
                        }
                        Some(crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation) => {
                            tracing::info!("TUI subagent runtime disabled by user choice");
                        }
                        None => break,
                    }
                    continue;
                }

                // Otherwise, a prompt arrived (or all channels closed).
                let DriverInput::Prompt(Some(text)) = input else {
                    break;
                };

                // Clear any stale interrupt signal
                let _ = interrupt_rx.try_recv();

                if !root_activated {
                    if let Some(TuiRuntimeSession::Attached(attached)) =
                        current_runtime.as_ref()
                    {
                        if let Err(error) = activate_tui_runtime_root(
                            &attached.socket_path,
                            &attached.attached_root,
                            &text,
                        ) {
                            let cause = error.to_string();
                            tracing::warn!(
                                %cause,
                                "could not activate TUI runtime root; continuing without delegation"
                            );
                            let _ = agent_tx
                                .send(runtime_unavailable_event("runtime activation", cause))
                                .await;
                            continue;
                        }
                    }
                    root_activated = true;
                }

                // Run agent
                is_running_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(handle) = &catalog {
                    if let Some(prompt) = handle.current_system_prompt() {
                        agent.set_system_prompt(Some(prompt));
                    }
                }
                match agent.run(text).await {
                    Ok(stream) => {
                        let mut stream = Box::pin(stream);
                        loop {
                            // Concurrently forward events and listen for interrupt
                            tokio::select! {
                                event = stream.next() => {
                                    match event {
                                        Some(ev) => {
                                            if agent_tx.send(ev).await.is_err() {
                                                break;
                                            }
                                        }
                                        None => break, // stream ended
                                    }
                                }
                                _ = interrupt_rx.recv() => {
                                    // User pressed Ctrl+C/Esc: cancel agent
                                    agent.cancel();
                                    // Drain remaining events until Cancelled/Done
                                    while let Some(ev) = stream.next().await {
                                        if agent_tx.send(ev).await.is_err() { break; }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = agent_tx.send(yi_agent_core::AgentEvent::Error(e)).await;
                    }
                }
                is_running_clone.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            if current_runtime.is_some() {
                if let Some((socket, root)) = runtime_detach_for_driver
                    .lock()
                    .expect("runtime detach mutex poisoned")
                    .take()
                {
                    detach_tui_runtime_root(&socket, &root);
                }
            }
        });

        // Run TUI on a dedicated blocking thread (it uses sync crossterm polling)
        let tui_handle = tokio::task::spawn_blocking(move || {
            crate::tui::app::run_tui(
                agent_rx,
                input_tx,
                interrupt_tx,
                control_tx,
                decision_tx,
                is_running,
                agent_config.model.clone(),
                runtime_intent,
                // The driver's `RuntimeChoice(None) => break` means this sender
                // must outlive the session: dropping it would end the run.
                Some(runtime_choice_tx),
                process_manager,
                workdir.clone(),
                mcp,
            )
        });

        let result = match tui_handle.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(e) => Err(anyhow::Error::from(e)),
        };

        if let Some((socket, root)) = runtime_detach
            .lock()
            .expect("runtime detach mutex poisoned")
            .take()
        {
            detach_tui_runtime_root(&socket, &root);
        }

        // TUI exited; abort the driver task to clean up
        // (driver may still be blocked on input_rx.recv() if agent was idle)
        driver.abort();
        // Await the aborted handle so the driver future — and any `Arc<Client>`
        // clone it held mid-call — is dropped before shutdown. Otherwise
        // `shutdown`'s `Arc::try_unwrap` fails and the child is not reaped
        // deterministically. `await` on an aborted handle resolves promptly.
        let _ = driver.await;

        // Close MCP server connections while the reactor is still running so the
        // child processes are reaped before the runtime is dropped.
        mcp_for_teardown.shutdown().await;

        result
    });

    tui_result?;

    Ok(())
}

/// Control commands sent from the TUI to the agent driver task.
/// Allows the TUI to trigger agent session rebuilds (e.g. /clear, /compact)
/// without reconstructing the whole agent inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlCommand {
    /// Clear the agent session (rebuild with empty session).
    Clear,
    /// Compact the agent session (summarize old messages, keep recent turns).
    Compact,
    /// Rebuild the agent so its tool registry matches the MCP switches the TUI
    /// already applied directly to the shared `McpManager`.
    McpRefresh,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_stdio_listen_accepts_stdio() {
        assert!(ensure_stdio_listen("stdio://").is_ok());
    }

    #[test]
    fn ensure_stdio_listen_rejects_other_transports() {
        assert!(ensure_stdio_listen("tcp://127.0.0.1:9000").is_err());
    }

    #[test]
    fn default_system_prompt_requires_a_workdir_for_isolation() {
        let prompt = yi_agent_core::AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("git worktree add"),
            "default prompt must tell a parent to prepare an isolated workdir"
        );
        assert!(
            prompt.contains("workdir"),
            "default prompt must name the spawn_agent workdir argument"
        );
        assert!(
            prompt.contains("git merge --no-ff"),
            "default prompt must still tell parents to integrate the delivered commit"
        );
    }

    // --- drain_stream_human tests ---

    use futures::stream::{self, BoxStream, StreamExt};
    use yi_agent_core::{AgentEvent, DoneReason};

    fn scripted_stream(events: Vec<AgentEvent>) -> BoxStream<'static, AgentEvent> {
        stream::iter(events).boxed()
    }

    // --- build_headless_setup tests ---

    use crate::config::Config;
    use std::path::PathBuf;

    fn test_config() -> Config {
        Config {
            provider: "anthropic".into(),
            api_url: "https://api.anthropic.com".into(),
            api_key: "test-key".into(),
            model: "claude-sonnet-4-5".into(),
            max_turns: 50,
            workdir: PathBuf::from("/tmp"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 8192,
            compact_tool_budget_tokens: 4096,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    fn attached_root_for_main_tests() -> crate::tui::subagents::AttachedRoot {
        crate::tui::subagents::AttachedRoot {
            session_id: "session-1".into(),
            task_id: "task-1".into(),
            capability: "capability-1".into(),
            workspace: yi_agent_core::subagent::worker::WorkerWorkspace {
                lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
                repository_root: "/tmp/repo".into(),
                path: "/tmp/repo/.worktrees/root".into(),
                branch: "feat/root".into(),
                parent_branch: "main".into(),
                base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        }
    }

    #[test]
    fn headless_root_tools_include_delegation_only_when_attached() {
        let config = test_config();
        let ordinary = build_headless_setup(&config, false)
            .expect("ordinary setup")
            .tools;
        assert!(ordinary.get("spawn_agent").is_none());

        let root = attached_root_for_main_tests();
        let registry = build_headless_root_tools(&config, "/tmp/runtime.sock".into(), &root)
            .expect("attached headless setup");
        let names = registry
            .tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"send_message".to_string()));
        assert!(names.contains(&"wait_agent".to_string()));
        assert!(!names.contains(&"accept_review".to_string()));
    }

    /// A runtime that cannot start must say so. Silently continuing left the
    /// user with no visible reason why `spawn_agent` was missing from the tool
    /// list, which is exactly how the long-socket-path failure hid for days.
    #[test]
    fn a_failed_runtime_attach_reports_an_actionable_reason() {
        let reason =
            runtime_unavailable_reason(&yi_agent_store::ipc::IpcError::SocketPathTooLong {
                path: "/very/long/path/runtime.sock".into(),
                limit: yi_agent_store::ipc::MAX_SOCKET_PATH_BYTES,
            });

        assert!(
            reason.contains("socket"),
            "the reason must name the failing mechanism, got: {reason}"
        );
        assert!(
            reason.contains("YI_AGENT_RUNTIME_DIR"),
            "the reason must offer an actionable remedy, got: {reason}"
        );
    }

    #[test]
    fn build_tui_root_tools_registers_subagent_tools_for_attached_runtime() {
        let config = test_config();
        let root = attached_root_for_main_tests();
        let registry = build_tui_root_tools(
            &yi_agent_core::ToolRegistry::new(),
            &config,
            "/tmp/runtime.sock".into(),
            &root,
        );
        let names = registry
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();

        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"send_message".to_string()));
        assert!(names.contains(&"wait_agent".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[test]
    fn build_headless_setup_naked_has_no_tools_and_no_system_prompt() {
        let config = test_config();
        let setup = build_headless_setup(&config, true).expect("setup should succeed");
        assert!(
            setup.tools.schemas().is_empty(),
            "naked mode should register zero tools, got: {:?}",
            setup
                .tools
                .schemas()
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            setup.system_prompt.is_none(),
            "naked mode should pass None as system_prompt, got: {:?}",
            setup.system_prompt
        );
    }

    #[test]
    fn build_headless_setup_default_registers_builtin_tools() {
        let config = test_config();
        let setup = build_headless_setup(&config, false).expect("setup should succeed");
        assert!(
            !setup.tools.schemas().is_empty(),
            "default mode should register builtin tools, got empty set"
        );
        let names: Vec<String> = setup
            .tools
            .schemas()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        // 至少应该有 read/write/bash 这几个核心工具
        assert!(
            names.iter().any(|n| n == "read"),
            "default mode should register 'read' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "write"),
            "default mode should register 'write' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "bash"),
            "default mode should register 'bash' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "process_start"),
            "non-naked headless setup must register process tools, got: {names:?}"
        );
    }

    #[test]
    fn build_headless_setup_default_includes_current_date_in_system_prompt() {
        let config = test_config();
        let setup = build_headless_setup(&config, false).expect("setup should succeed");
        let sp = setup
            .system_prompt
            .as_ref()
            .expect("default mode should produce a system prompt");
        assert!(
            sp.contains("Current date:"),
            "default system_prompt should contain current date marker, got: {sp}"
        );
    }

    // Sync wrapper around `drain_stream_human` so tests can drive the async
    // stream without spinning up a multi-thread runtime.
    fn drain_stream_human_sync<W: std::io::Write, E: std::io::Write>(
        stream: BoxStream<'static, AgentEvent>,
        out: &mut W,
        err: &mut E,
    ) -> i32 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(drain_stream_human(stream, out, err))
    }

    #[test]
    fn drain_stream_human_concatenates_text_deltas_without_extra_newlines() {
        // Bug regression: each AssistantText delta should NOT be on its own
        // line. Three chunks "chunk1" "chunk2" "chunk3" must produce
        // "chunk1chunk2chunk3\n" (one trailing newline), not
        // "chunk1\nchunk2\nchunk3\n".
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("chunk1".into()),
            AgentEvent::AssistantText("chunk2".into()),
            AgentEvent::AssistantText("chunk3".into()),
            AgentEvent::Done {
                reason: DoneReason::MaxTurns,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0, "exit code should be 0 for Done::MaxTurns");
        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "chunk1chunk2chunk3\n",
            "AssistantText deltas should concatenate without per-chunk newlines"
        );
        assert!(
            String::from_utf8(err).unwrap().contains("[done:MaxTurns]"),
            "MaxTurns is an abnormal non-error termination and should be reported on stderr"
        );
    }

    #[test]
    fn drain_stream_human_suppresses_done_endturn_on_stderr() {
        // Bug regression: normal completion (EndTurn) is already signaled by
        // exit code 0. The [done:EndTurn] line is noise on stderr and should
        // be suppressed, matching the TUI which renders EndTurn as a silent
        // separator. Only abnormal terminations (MaxTurns/Cancelled/Error)
        // emit diagnostic lines to stderr.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0, "exit code should be 0 for Done::EndTurn");
        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(stdout, "hello\n");
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            !stderr.contains("[done:"),
            "EndTurn must not emit [done:EndTurn] on stderr; got: {stderr:?}"
        );
    }

    #[test]
    fn drain_stream_human_reports_provider_retry_on_stderr() {
        let stream = futures::stream::iter(vec![
            AgentEvent::AssistantText("partial".into()),
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 60,
                cause: yi_agent_core::RetryCause::IdleStall,
            },
            AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        ])
        .boxed();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0);
        let err_text = String::from_utf8(err).unwrap();
        assert!(
            err_text.contains("[provider-retry:1/3]"),
            "the retry must be announced on stderr, got: {err_text}"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "partial\n",
            "stdout must carry only assistant text"
        );
        // A stall line keeps its historical shape (no suffix) for scrapers.
        assert!(
            !err_text.contains("timeout]"),
            "an idle stall must not be labelled a timeout, got: {err_text}"
        );
    }

    #[test]
    fn drain_stream_human_marks_a_timeout_retry_on_stderr() {
        let stream = futures::stream::iter(vec![
            AgentEvent::AssistantText("partial".into()),
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 0,
                cause: yi_agent_core::RetryCause::RequestTimeout,
            },
            AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        ])
        .boxed();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0);
        let err_text = String::from_utf8(err).unwrap();
        // The suffix distinguishes a deadline from a stall without changing the
        // stall line's existing format.
        assert!(
            err_text.contains("[provider-retry:1/3 timeout]"),
            "a timeout retry must be labelled on stderr, got: {err_text}"
        );
    }

    #[test]
    fn drain_stream_human_preserves_embedded_newlines_in_text() {
        // The LLM's own newlines inside AssistantText must be preserved.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("line one\n".into()),
            AgentEvent::AssistantText("line two\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "line one\nline two\n",
            "embedded newlines preserved, no extra trailing newline added"
        );
    }

    #[test]
    fn drain_stream_human_adds_trailing_newline_only_when_missing() {
        // If the final AssistantText does NOT end with '\n', drain_stream
        // should add exactly one so the shell prompt starts on a fresh line.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(stdout, "hello\n", "exactly one trailing newline added");
    }

    #[test]
    fn drain_stream_human_no_trailing_newline_when_text_already_ends_with_newline() {
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "hello\n",
            "no duplicate trailing newline when text already ends with \\n"
        );
    }

    #[test]
    fn drain_stream_human_routes_tool_events_to_err_not_out() {
        // ToolCall and ToolResult must go to stderr, not stdout, so they
        // don't pollute the assistant text stream.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("let me run ".into()),
            AgentEvent::AssistantText("a command\n".into()),
            AgentEvent::ToolCall {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "echo hi"}),
            },
            AgentEvent::ToolResult {
                id: "tool_1".into(),
                result: yi_agent_core::ToolResult::text("hi\n"),
            },
            AgentEvent::AssistantText("done\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert_eq!(
            stdout, "let me run a command\ndone\n",
            "stdout should contain only assistant text, concatenated"
        );
        assert!(
            stderr.contains("[tool:bash]"),
            "stderr should contain tool call: {stderr}"
        );
        assert!(
            stderr.contains("[result:tool_1]"),
            "stderr should contain tool result: {stderr}"
        );
    }

    #[test]
    fn drain_stream_human_cancelled_returns_exit_130() {
        let stream = scripted_stream(vec![AgentEvent::Cancelled]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 130, "Cancelled should produce exit code 130");
    }

    #[test]
    fn drain_stream_human_error_returns_exit_1() {
        let stream = scripted_stream(vec![AgentEvent::Error(
            yi_agent_core::AgentError::Provider(yi_agent_core::ProviderError::Network(
                "boom".into(),
            )),
        )]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 1, "Error should produce exit code 1");
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("[error:"),
            "stderr should contain error: {stderr}"
        );
    }

    #[test]
    fn attach_application_root_request_uses_the_configured_workdir() {
        let yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace,
        } = attach_application_root_request(
            "test-key".into(),
            std::path::PathBuf::from("/projects/b"),
        )
        else {
            panic!("expected an application-root attach request");
        };
        assert_eq!(idempotency_key, "test-key");
        assert_eq!(workspace, std::path::PathBuf::from("/projects/b"));
    }

    #[test]
    fn runtime_directory_uses_workdir_local_default() {
        assert_eq!(
            runtime_directory_from(None, std::path::Path::new("/tmp/project-a")),
            std::path::PathBuf::from("/tmp/project-a/.yi-agent/runtime"),
        );
    }

    #[test]
    fn runtime_directory_prefers_a_nonempty_explicit_override() {
        assert_eq!(
            runtime_directory_from(
                Some(std::path::PathBuf::from("/tmp/shared-runtime")),
                std::path::Path::new("/tmp/project-a"),
            ),
            std::path::PathBuf::from("/tmp/shared-runtime"),
        );
    }

    #[test]
    fn runtime_directory_isolated_between_workdirs() {
        let first = runtime_directory_from(None, std::path::Path::new("/tmp/project-a"));
        let second = runtime_directory_from(None, std::path::Path::new("/tmp/project-b"));
        assert_ne!(first, second);
    }

    #[test]
    fn runtime_directory_for_workdir_uses_the_same_project_path_for_daemon_and_attachment() {
        let workdir = std::path::PathBuf::from("/tmp/isolated-project");
        let daemon_runtime = runtime_directory_from(None, &workdir);
        let attachment_runtime = runtime_directory_from(None, &workdir);

        assert_eq!(daemon_runtime, attachment_runtime);
        assert_eq!(
            daemon_runtime,
            std::path::PathBuf::from("/tmp/isolated-project/.yi-agent/runtime"),
        );
    }

    #[test]
    fn runtime_database_path_is_shared_by_daemon_and_attachment() {
        let workdir = std::path::PathBuf::from("/tmp/isolated-project");
        let daemon_database = runtime_database_path(&runtime_directory_from(None, &workdir));
        let attachment_database = runtime_database_path(&runtime_directory_from(None, &workdir));

        assert_eq!(daemon_database, attachment_database);
        assert_eq!(
            daemon_database,
            std::path::PathBuf::from("/tmp/isolated-project/.yi-agent/runtime/runtime.sqlite"),
        );
    }

    #[test]
    fn task_event_line_uses_the_stable_event_wire_fields() {
        let event = yi_agent_store::ipc::IpcEvent {
            event_id: 42,
            task_id: "task-1".into(),
            kind: "task_progress".into(),
            payload_json: r#"{"done":true}"#.into(),
        };

        assert_eq!(task_event_line(&event), "42 task_progress {\"done\":true}");
    }

    #[test]
    fn attach_rejection_reason_surfaces_the_daemon_message_not_a_debug_dump() {
        let response = yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::InvalidState,
            message: Some("worker startup failed: Git workspace error: parent worktree is dirty: /tmp/p — a coding root needs a clean checkout; commit or stash the changes first".into()),
        };

        let reason = runtime_attached_root_rejection(&response);

        assert!(
            reason.contains("parent worktree is dirty"),
            "the user must see the real cause, got: {reason}"
        );
        assert!(
            reason.contains("commit or stash"),
            "the reason must offer an actionable remedy, got: {reason}"
        );
        assert!(
            !reason.contains("Some("),
            "the reason must not leak a Debug dump, got: {reason}"
        );
    }

    #[test]
    fn attach_rejection_reason_degrades_gracefully_without_a_message() {
        let response = yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::Internal,
            message: None,
        };

        let reason = runtime_attached_root_rejection(&response);

        assert!(reason.contains("internal"), "got: {reason}");
        assert!(reason.contains("delegation is disabled"), "got: {reason}");
    }

    #[test]
    fn ignoring_project_local_runtime_state_keeps_a_clean_checkout_usable() {
        use std::process::Command;

        let directory = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(directory.path())
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "tests@example.com"]);
        run(&["config", "user.name", "Tests"]);
        std::fs::write(directory.path().join("README.md"), "base\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "base"]);
        let porcelain = || {
            let output = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(directory.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(porcelain(), "");

        // The daemon hook runs before the state dir exists.
        ignore_project_local_runtime_state(directory.path());

        assert_eq!(
            porcelain(),
            "",
            "the hook alone must not dirty the checkout"
        );

        // Once the daemon creates its store, the checkout must stay clean.
        std::fs::create_dir_all(directory.path().join(".yi-agent/runtime")).unwrap();
        std::fs::write(
            directory.path().join(".yi-agent/runtime/runtime.sqlite"),
            "state",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".yi-agent/threads")).unwrap();
        std::fs::write(
            directory.path().join(".yi-agent/threads/thread-1.jsonl"),
            "log",
        )
        .unwrap();

        assert_eq!(
            porcelain(),
            "",
            "the daemon's own state must never dirty the checkout"
        );
    }

    #[test]
    fn ignoring_project_local_runtime_state_is_a_no_op_outside_git() {
        let directory = tempfile::TempDir::new().unwrap();

        // Must not panic or create stray files in a non-git workdir.
        ignore_project_local_runtime_state(directory.path());

        assert!(!directory.path().join(".git").exists());
    }

    /// The user pressed `y` and the runtime did not come up. That outcome must
    /// reach the session as a notice, never as `Error`: an error reads as a
    /// failed turn, and the code this replaced sent `ProviderTurnAdmission`,
    /// which rendered the raw `IpcError` text as `Error: provider turn
    /// admission failed: ...` and never mentioned the restart that fixes it.
    ///
    /// This pins the classification. Reverting the emit site to the error path
    /// used to leave every test green, so the regression could only be caught
    /// by hand-driving a TUI.
    #[test]
    fn a_runtime_bring_up_failure_becomes_a_notice_not_an_error() {
        let event = runtime_unavailable_event("runtime attach", "file is not a database");
        match event {
            yi_agent_core::AgentEvent::SubagentRuntimeUnavailable { stage, cause } => {
                assert_eq!(stage, "runtime attach");
                assert_eq!(cause, "file is not a database");
            }
            other => panic!("bring-up failure must be a notice, not a turn error: {other:?}"),
        }
    }

    /// The activation step reports the same remedy, so it takes the same
    /// notice path with its own stage label.
    #[test]
    fn a_runtime_activation_failure_takes_the_same_notice_path() {
        let event = runtime_unavailable_event("runtime activation", "daemon rejected: internal");
        match event {
            yi_agent_core::AgentEvent::SubagentRuntimeUnavailable { stage, .. } => {
                assert_eq!(stage, "runtime activation");
            }
            other => panic!("activation failure must be a notice: {other:?}"),
        }
    }

    #[test]
    fn preference_maps_to_startup_intent() {
        use crate::tui::runtime_prefs::{RuntimePreference, save};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();

        save(dir.path(), RuntimePreference::Ask).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));

        save(dir.path(), RuntimePreference::Always).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::AutoStart)
        ));

        save(dir.path(), RuntimePreference::Never).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::DisabledNotice { .. })
        ));
    }

    #[test]
    fn never_notice_is_chinese_and_points_at_the_command() {
        use crate::tui::runtime_prefs::{RuntimePreference, save};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        let Some(RuntimeStartupIntent::DisabledNotice { reason }) = startup_intent_for(dir.path())
        else {
            panic!("never must produce a disabled notice");
        };
        assert!(reason.contains("/runtime"), "reason: {reason}");
        assert!(reason.contains("已禁用"), "reason: {reason}");
    }

    #[test]
    fn reading_the_intent_does_not_create_the_preference_directory() {
        use crate::tui::runtime_prefs::preferences_path;
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        // A project that never opted in has no `.yi-agent/`. Deriving the
        // startup intent must stay read-only: creating the directory would
        // dirty every project the user merely launched the TUI in.
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));
        assert!(
            !preferences_path(dir.path()).exists(),
            "deriving the startup intent must not create .yi-agent/"
        );
    }
}
