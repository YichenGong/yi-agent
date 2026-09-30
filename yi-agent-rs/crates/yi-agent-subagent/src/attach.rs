//! Client half of the local subagent runtime: start (or join) the daemon for a
//! project, attach an application root, then activate and detach it.
//!
//! The TUI and the desktop app-server both drive the same daemon contract, so
//! both go through here rather than each inventing their own bring-up.

use std::path::{Path, PathBuf};

use yi_agent_core::subagent::worker::AgentWorkerFactory;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_store::ipc::{Daemon, IpcError, IpcResponse, send_request};

use crate::AttachedRoot;

/// Why a runtime could not be brought up. Both fields are diagnostics for the
/// trace: delegation is optional, so neither reaches the user as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachFailure {
    /// The bring-up step that failed: `runtime directory`, `worker factory`,
    /// `daemon start`, or `attach`.
    pub stage: &'static str,
    pub cause: String,
}

impl AttachFailure {
    pub fn new(stage: &'static str, cause: impl std::fmt::Display) -> Self {
        Self {
            stage,
            cause: cause.to_string(),
        }
    }
}

/// A project's attached runtime. Dropping it releases the embedded daemon (when
/// this process started one) but never detaches the root: callers detach
/// explicitly, because detaching is a durable state change.
pub struct AttachedProjectRuntime {
    pub socket_path: PathBuf,
    /// The Git checkout the delegation tools resolve their workdir against.
    pub workspace_root: PathBuf,
    /// The project directory the root was attached for.
    pub project_root: PathBuf,
    pub attached_root: AttachedRoot,
    /// Held only when this process started the daemon.
    pub embedded_daemon: Option<Daemon>,
}

/// The project-local runtime directory. The TUI's slash commands and the CLI
/// resolve the same location, so everything goes through here.
pub fn project_runtime_directory(workdir: &Path) -> PathBuf {
    std::env::var_os("YI_AGENT_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.join(".yi-agent/runtime"))
}

/// Records the project-local state root in the shared git exclude.
///
/// Creating `<workdir>/.yi-agent/` dirties a checkout that does not already
/// ignore it, and a dirty parent is exactly what makes the first delegation
/// fail. Must run *before* the daemon creates that directory. A non-git or
/// unreadable workdir is a no-op: the exclusion is a convenience, never a
/// precondition the caller has to satisfy.
pub fn ignore_project_local_runtime_state(workdir: &Path) {
    let state_root = workdir.join(".yi-agent");
    let service = yi_agent_tools::worktree::WorktreeService::new();
    if let Err(error) = service.ignore_project_path(&state_root) {
        tracing::debug!(
            error = %error,
            workdir = %workdir.display(),
            "could not record the project-local runtime state in git exclude"
        );
    }
}

/// Builds the daemon-side worker factory for `cfg` (provider, skills-only
/// registry, sandbox, project workspace).
pub fn worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
) -> anyhow::Result<std::sync::Arc<dyn AgentWorkerFactory>> {
    use std::sync::Arc;

    let provider = yi_agent_runtime::bootstrap::build_provider(cfg)?;
    // Skills-only registry: the worker's deliberate contract is to NOT register
    // builtin/process tools here (the recovery path adds its own workspace-rooted
    // set, and the worker start call adds its own).
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(cfg)?;
    let catalog = prompt.catalog;
    let mut registry = yi_agent_core::ToolRegistry::new();
    if let Some(skills) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
    }
    let agent_config = yi_agent_runtime::bootstrap::build_agent_config(cfg, prompt.system_prompt);
    Ok(Arc::new(
        crate::DaemonAgentWorkerFactory::new(
            provider,
            Arc::new(registry),
            agent_config,
            runtime_socket,
        )
        .with_catalog(catalog)
        // Recovery must inspect the same worktree ordinary builtin tools use.
        .with_sandbox(cfg.sandbox, cfg.sandbox_writable_roots.clone())
        .with_workspace(cfg.workdir.clone()),
    ))
}

/// Starts (or joins) the project daemon and attaches an application root.
///
/// `runtime_dir` is passed in rather than derived from the environment, so a
/// caller (and a test) can isolate one project's runtime without mutating
/// process-wide state. Pass `project_runtime_directory(&cfg.workdir)` for the
/// default location.
pub fn attach_project_runtime(
    cfg: &RuntimeConfig,
    runtime_dir: PathBuf,
) -> Result<AttachedProjectRuntime, AttachFailure> {
    let database = runtime_dir.join("runtime.sqlite");
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)
        .map_err(|error| AttachFailure::new("runtime directory", error))?;
    // Record the project-local state before the daemon creates it, otherwise the
    // root provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&cfg.workdir);
    let factory = worker_factory(cfg, socket_path.clone())
        .map_err(|error| AttachFailure::new("worker factory", error))?;
    let embedded_daemon = match Daemon::start_with_factory(&runtime_dir, &database, factory) {
        Ok(daemon) => Some(daemon),
        Err(IpcError::AlreadyRunning { .. }) => None,
        Err(error) => return Err(AttachFailure::new("daemon start", error)),
    };
    let idempotency_key = format!(
        "project:{}:{}:{}",
        std::process::id(),
        cfg.workdir.display(),
        uuid::Uuid::new_v4()
    );
    let response = send_request(
        &socket_path,
        yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace: cfg.workdir.clone(),
        },
    )
    .map_err(|error| AttachFailure::new("attach", error))?;
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        return Err(AttachFailure::new("attach", describe_rejection(&response)));
    };
    Ok(AttachedProjectRuntime {
        socket_path,
        workspace_root: workspace.path.clone(),
        project_root: cfg.workdir.clone(),
        attached_root: AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        },
        embedded_daemon,
    })
}

/// Turns a rejected attach response into one actionable line for the trace.
///
/// A raw `{response:?}` dump buries the real cause (for example a dirty
/// checkout) behind `Error { code: InvalidState, message: Some(...) }`, which
/// reads like an internal admission fault rather than the local git state the
/// user can fix.
fn describe_rejection(response: &IpcResponse) -> String {
    match response {
        IpcResponse::Error {
            code,
            message: Some(message),
        } => format!("daemon rejected the runtime attachment: {code}: {message}"),
        IpcResponse::Error {
            code,
            message: None,
        } => {
            format!("daemon rejected the runtime attachment: {code}")
        }
        other => format!("daemon rejected the runtime attachment: {other:?}"),
    }
}

/// Activates the root with the user's objective. Idempotent: an already running
/// root answers `Ok`.
pub fn activate_root(
    socket_path: &Path,
    root: &AttachedRoot,
    objective: &str,
) -> Result<(), String> {
    let response = send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::ActivateApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
            objective: objective.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        IpcResponse::ApplicationRootActivated => Ok(()),
        other => Err(format!("daemon rejected the runtime activation: {other:?}")),
    }
}

/// Detaches the root. Best effort: a failure is a trace line, never an error
/// that could mask the reason the process is winding down.
pub fn detach_root(socket_path: &Path, root: &AttachedRoot) {
    let response = send_request(
        socket_path,
        yi_agent_store::ipc::IpcRequest::DetachApplicationRoot {
            session_id: root.session_id.clone(),
            root_task_id: root.task_id.clone(),
            capability: root.capability.clone(),
        },
    );
    if let Err(error) = response {
        tracing::warn!(%error, "could not detach the application root");
    }
}
