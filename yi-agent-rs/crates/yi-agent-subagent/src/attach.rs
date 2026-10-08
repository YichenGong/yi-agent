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
    /// 插件监督句柄：本进程起了 daemon 才真的监督，借用别人的则空转。
    /// 生命周期交给 Drop，随 binding 一起结束。
    // Held only for its Drop side effect (it stops supervision when dropped);
    // the value itself is never read.
    #[allow(dead_code)]
    supervisor: yi_agent_runtime::supervise::SuperviseHandle,
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
///
/// The child's model and credentials come from the machine catalog's
/// `subagent_model` entry, falling back to the global default, and finally to
/// `cfg` itself when neither is configured (see [`build_worker_factory`]).
pub fn worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
) -> anyhow::Result<std::sync::Arc<dyn AgentWorkerFactory>> {
    Ok(std::sync::Arc::new(build_worker_factory(
        cfg,
        runtime_socket,
        &yi_agent_runtime::models::load_catalog(),
    )?))
}

/// Builds the factory from an already-loaded catalog.
///
/// The catalog is a parameter, never re-read from `HOME` here, so a test (or a
/// caller that already holds one) resolves the same way the process does without
/// mutating process-wide environment state.
fn build_worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
    catalog: &yi_agent_runtime::models::ModelCatalog,
) -> anyhow::Result<crate::DaemonAgentWorkerFactory> {
    use std::sync::Arc;

    // The child runs on the catalog's `subagent_model` entry (falling back to the
    // global default); with no entry at all the process cfg is used unchanged.
    let effective = yi_agent_runtime::models::resolve_effective(
        cfg,
        catalog,
        catalog.subagent_model.as_deref(),
    );
    let effective = &effective;
    let provider = yi_agent_runtime::bootstrap::build_provider(effective)?;
    // Skills-only registry: the worker's deliberate contract is to NOT register
    // builtin/process tools here (the recovery path adds its own workspace-rooted
    // set, and the worker start call adds its own).
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(effective)?;
    let catalog = prompt.catalog;
    let mut registry = yi_agent_core::ToolRegistry::new();
    if let Some(skills) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
    }
    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(effective, prompt.system_prompt);
    Ok(crate::DaemonAgentWorkerFactory::new(
        provider,
        Arc::new(registry),
        agent_config,
        runtime_socket,
    )
    .with_catalog(catalog)
    // Recovery must inspect the same worktree ordinary builtin tools use.
    .with_sandbox(effective.sandbox, effective.sandbox_writable_roots.clone())
    .with_workspace(effective.workdir.clone())
    // The configured capacity must reach the coordinator; without this the
    // factory reports the trait default and the env var is inert.
    .with_max_resident_subagents(effective.max_resident_subagents))
}

/// Test-only seam: load a catalog from an explicit path instead of `HOME`, so a
/// test can inject one without mutating process-wide environment state.
#[cfg(test)]
fn worker_factory_with_catalog(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
    catalog_path: &Path,
) -> anyhow::Result<crate::DaemonAgentWorkerFactory> {
    build_worker_factory(
        cfg,
        runtime_socket,
        &yi_agent_runtime::models::load_catalog_from(catalog_path),
    )
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
    // The startup sweep reclaims tasks whose owning root is gone, and whoever
    // owns the terminal announces the count: `main.rs` prints the historical
    // line for `daemon serve` and headless runs, and the TUI renders a
    // transcript notice. This shared path has no terminal contract -- it serves
    // the desktop's app-server -- so the count would otherwise vanish with no
    // way to tell "the sweep tidied up leftovers" from "it reclaimed nothing".
    // Record it instead of writing to a stream this process does not own.
    if let Some(daemon) = &embedded_daemon {
        let reclaimed = daemon.reclaimed_orphans();
        if reclaimed > 0 {
            tracing::info!(reclaimed, "reclaimed orphaned subagent tasks");
        }
    }
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
    // 借用了别人的 daemon 就不监督：监督者应是那个真正拥有 daemon 的进程。
    let owns_daemon = embedded_daemon.is_some();
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
        supervisor: yi_agent_runtime::supervise::for_daemon_ownership(&cfg.workdir, owns_daemon),
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

/// How reachable and healthy the project runtime is.
///
/// `Unknown` is deliberately separate: a permission error or an exhausted file
/// descriptor table is not "the daemon died", and must never trigger a takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProbe {
    /// Answered a read-only `Status` normally.
    Healthy,
    /// Listening, but answers `internal` -- alive yet unusable (see the TUI's
    /// `replace_wedged_daemon`), so it must be retired and replaced.
    Wedged,
    /// The socket is gone or nobody is accepting on it.
    Dead,
    /// Any other outcome. Never taken over; the caller degrades as before.
    Unknown,
}

/// Probes a runtime socket with the cheapest read-only request a daemon serves.
///
/// `Status` creates no session and writes no attachment, so probing is free of
/// side effects. A wedged daemon fails it exactly as it fails everything else.
pub fn probe_runtime(socket_path: &std::path::Path) -> RuntimeProbe {
    match send_request(socket_path, yi_agent_store::ipc::IpcRequest::Status) {
        Ok(yi_agent_store::ipc::IpcResponse::Status { .. }) => RuntimeProbe::Healthy,
        Ok(yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::Internal,
            ..
        }) => RuntimeProbe::Wedged,
        Ok(_) => RuntimeProbe::Unknown,
        Err(yi_agent_store::ipc::IpcError::Io(error)) if is_dead_io(&error) => RuntimeProbe::Dead,
        Err(_) => RuntimeProbe::Unknown,
    }
}

/// Whether a connect failure means "no live daemon", as opposed to "we were not
/// allowed to find out".
fn is_dead_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
    )
}

/// Starts, joins, or replaces the project daemon so *this* process ends up
/// owning a usable runtime.
///
/// Strategy B: a healthy daemon is adopted -- its owner keeps it, we never
/// steal it. Only a `Dead` socket (nobody listening) or a `Wedged` daemon
/// (alive but answering `internal`) is replaced, because those cannot serve us
/// and would only keep failing.
///
/// `Unknown` is not a takeover signal: it is returned as an `AttachFailure` so
/// the caller degrades exactly as it did before.
pub fn ensure_owned_runtime(
    cfg: &RuntimeConfig,
    runtime_dir: PathBuf,
) -> Result<AttachedProjectRuntime, AttachFailure> {
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)
        .map_err(|error| AttachFailure::new("runtime directory", error))?;
    match probe_runtime(&socket_path) {
        RuntimeProbe::Healthy => {}
        RuntimeProbe::Dead => {}
        RuntimeProbe::Wedged => {
            if !retire_if_wedged(&socket_path) {
                return Err(AttachFailure::new(
                    "daemon start",
                    "the local runtime answered `internal` and could not be retired",
                ));
            }
        }
        RuntimeProbe::Unknown => {
            return Err(AttachFailure::new(
                "daemon start",
                "the local runtime could not be probed",
            ));
        }
    }
    // `attach_project_runtime` already implements the three outcomes this
    // classification implies: a fresh start, a join on `AlreadyRunning`, or a
    // start over the socket node a dead daemon left behind.
    attach_project_runtime(cfg, runtime_dir)
}

/// Retires a *wedged* daemon (listening, but every request answers `internal`)
/// so a fresh `Daemon::start` can take over.
///
/// Returns `true` only when a `Stop` was accepted. Every other outcome --
/// including a healthy daemon or a `Stop` that could not be delivered -- returns
/// `false`, because such a daemon is not ours to replace. Shared by the TUI's
/// bring-up and the desktop's self-heal so both apply one rule.
pub fn retire_if_wedged(socket_path: &std::path::Path) -> bool {
    if probe_runtime(socket_path) != RuntimeProbe::Wedged {
        return false;
    }
    tracing::warn!(
        socket = %socket_path.display(),
        "local runtime answered `internal`; retiring it so this session can start a working one"
    );
    matches!(
        send_request(socket_path, yi_agent_store::ipc::IpcRequest::Stop),
        Ok(yi_agent_store::ipc::IpcResponse::Stopping)
    )
}

#[cfg(test)]
mod tests {
    use super::{RuntimeProbe, probe_runtime, project_runtime_directory, worker_factory};
    use yi_agent_store::ipc::Daemon;

    /// A fully-populated config for the factory tests. Every field the resolved
    /// catalog entry could clobber is set to a recognizable "base" value so a
    /// test can tell the entry's value from the process cfg's.
    fn base_config(workdir: &std::path::Path) -> yi_agent_runtime::config::RuntimeConfig {
        yi_agent_runtime::config::RuntimeConfig {
            provider: "anthropic".into(),
            api_url: "https://api.anthropic.com".into(),
            api_key: String::new(),
            model: "base-model".into(),
            max_turns: 4,
            max_resident_subagents: 8,
            workdir: workdir.to_path_buf(),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: true,
        }
    }

    /// Writes a models.json into a per-test temp dir and returns its path. The
    /// catalog is injected by path, so no test mutates the process `HOME`.
    fn catalog_file(dir: &std::path::Path, json: &str) -> std::path::PathBuf {
        let path = dir.join("models.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    /// The factory must land the child on the catalog's `subagent_model` entry,
    /// not the process cfg's model: a worker running the global default model
    /// would ignore the user's dedicated (often cheaper) subagent choice.
    #[test]
    fn the_worker_uses_the_subagent_model_entry() {
        let directory = tempfile::TempDir::new().unwrap();
        let catalog_path = catalog_file(
            directory.path(),
            r#"{"models":[
                {"name":"A","provider":"anthropic","api_url":"https://a","model":"model-a","api_key":"key-a"},
                {"name":"S","provider":"openai","api_url":"https://s","model":"model-s","api_key":"key-s"}
            ],"default_model":"A","subagent_model":"S"}"#,
        );
        let cfg = base_config(directory.path());

        let factory = super::worker_factory_with_catalog(
            &cfg,
            directory.path().join("runtime.sock"),
            &catalog_path,
        )
        .expect("factory");

        assert_eq!(
            factory.worker_config_model("ignored"),
            "model-s",
            "the worker must run on the subagent_model entry, not the global default"
        );
    }

    /// With no `subagent_model`, the global default is the truthful choice, so
    /// the worker falls back to it rather than the process cfg.
    #[test]
    fn the_worker_falls_back_to_the_global_default() {
        let directory = tempfile::TempDir::new().unwrap();
        let catalog_path = catalog_file(
            directory.path(),
            r#"{"models":[
                {"name":"A","provider":"anthropic","api_url":"https://a","model":"model-a","api_key":"key-a"}
            ],"default_model":"A"}"#,
        );
        let cfg = base_config(directory.path());

        let factory = super::worker_factory_with_catalog(
            &cfg,
            directory.path().join("runtime.sock"),
            &catalog_path,
        )
        .expect("factory");

        assert_eq!(
            factory.worker_config_model("ignored"),
            "model-a",
            "without a subagent_model the global default must be used"
        );
    }

    /// An empty catalog (no default, no subagent) leaves the process cfg intact,
    /// which is the pre-catalog behavior every existing deployment relies on.
    #[test]
    fn the_worker_falls_back_to_the_process_cfg_without_a_catalog() {
        let directory = tempfile::TempDir::new().unwrap();
        let catalog_path = directory.path().join("missing-models.json");
        let cfg = base_config(directory.path());

        let factory = super::worker_factory_with_catalog(
            &cfg,
            directory.path().join("runtime.sock"),
            &catalog_path,
        )
        .expect("factory");

        assert_eq!(
            factory.worker_config_model("ignored"),
            "base-model",
            "no catalog entry must leave the process cfg's model in place"
        );
    }

    /// The configured capacity must survive the trip through the factory: this
    /// is what makes `YI_AGENT_MAX_RESIDENT_SUBAGENTS` effective in the daemon
    /// instead of inert. A regression that drops the `with_max_resident_subagents`
    /// chain in `worker_factory` fails this with 64 against 8.
    #[test]
    fn the_configured_resident_capacity_reaches_the_worker_factory() {
        let directory = tempfile::TempDir::new().unwrap();
        let cfg = yi_agent_runtime::config::RuntimeConfig {
            provider: "anthropic".into(),
            api_url: "https://api.anthropic.com".into(),
            api_key: String::new(),
            model: "test-model".into(),
            max_turns: 4,
            max_resident_subagents: 8,
            workdir: directory.path().to_path_buf(),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: true,
        };
        let socket = directory.path().join("runtime.sock");

        let factory = worker_factory(&cfg, socket).expect("factory");

        assert_eq!(
            factory.max_resident_subagents(),
            8,
            "the configured capacity must reach the factory, not the trait default"
        );
    }

    /// The daemon, the TUI and the app-server must land on one location, so
    /// this is the single place the default is spelled out.
    #[test]
    fn a_project_runtime_directory_is_workdir_local() {
        assert_eq!(
            project_runtime_directory(std::path::Path::new("/tmp/project-a")),
            std::path::PathBuf::from("/tmp/project-a/.yi-agent/runtime"),
        );
    }

    /// Two projects must never share a runtime: the socket and the SQLite store
    /// both hang off this path.
    #[test]
    fn two_projects_get_distinct_runtime_directories() {
        assert_ne!(
            project_runtime_directory(std::path::Path::new("/tmp/project-a")),
            project_runtime_directory(std::path::Path::new("/tmp/project-b")),
        );
    }

    /// Nothing is listening: the socket path does not resolve to a live daemon,
    /// so the runtime must be reported dead rather than merely unhealthy.
    #[test]
    fn probe_reports_dead_without_a_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("runtime.sock");
        assert_eq!(probe_runtime(&socket), RuntimeProbe::Dead);
    }

    /// A live daemon answers the read-only `Status` probe. This is the only
    /// outcome that must lead to reuse rather than replacement.
    #[test]
    fn probe_reports_healthy_on_status() {
        let runtime = tempfile::TempDir::new().unwrap();
        let database = runtime.path().join("runtime.sqlite");
        let daemon = Daemon::start(runtime.path(), &database).expect("daemon starts");
        let socket = yi_agent_store::ipc::socket_path_for(runtime.path()).unwrap();
        assert_eq!(probe_runtime(&socket), RuntimeProbe::Healthy);
        drop(daemon);
    }
}
