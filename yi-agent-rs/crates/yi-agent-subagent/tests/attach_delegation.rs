//! End-to-end shape of the shared attach path: a clean git project attaches,
//! an objective activates the root, and the daemon accepts a delegated child.
//!
//! Both applications (the TUI and the app-server) drive this same module, so
//! the contract pinned here is what the desktop app relies on: attach, activate
//! once, delegate, detach.

use std::path::Path;

fn init_git_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }
}

fn config_for(repo: &Path) -> yi_agent_runtime::config::RuntimeConfig {
    yi_agent_runtime::config::RuntimeConfig {
        provider: "anthropic".into(),
        api_url: "https://api.anthropic.com".into(),
        api_key: String::new(),
        model: "test-model".into(),
        max_turns: 4,
        workdir: repo.to_path_buf(),
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

/// The whole point of the shared crate: a project attaches, its root activates
/// from the first real prompt, a delegated child is admitted, and detaching
/// always succeeds.
#[test]
fn a_clean_git_project_attaches_activates_delegates_and_detaches() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());

    let attached =
        yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime_dir.path().to_path_buf())
            .expect("a clean git project must attach");
    assert!(
        attached
            .workspace_root
            .starts_with(repo.path().canonicalize().unwrap()),
        "the root runs inside the project checkout, got {}",
        attached.workspace_root.display()
    );

    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("activation must succeed");
    // Activation is idempotent, so a second thread activating the same root is safe.
    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("a second activation must be a no-op, not an error");

    // The tools the applications register over this runtime are exactly the ones
    // the model calls, so exercise the spawn they share.
    let mut registry = yi_agent_core::ToolRegistry::new();
    let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
        yi_agent_subagent::binding::RuntimeHandle {
            socket_path: attached.socket_path.clone(),
            workspace_root: attached.workspace_root.clone(),
            session_id: attached.attached_root.session_id.clone(),
            task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
        },
    );
    yi_agent_subagent::register_attached_root_tools(
        &mut registry,
        binding,
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
    );
    assert!(
        registry.names().contains(&"spawn_agent".to_string()),
        "an attached root exposes the delegation tools, got {:?}",
        registry.names()
    );
    let response = yi_agent_store::ipc::send_request(
        &attached.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: attached.attached_root.session_id.clone(),
            parent_task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
            objective: "a delegated read-only investigation".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,

            sandbox: None,
        },
    )
    .expect("the runtime socket must accept a delegated child");
    let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = response else {
        panic!("a delegated child must be admitted, got {response:?}");
    };
    assert!(!task_id.is_empty(), "the child must get an id to wait on");

    yi_agent_subagent::attach::detach_root(&attached.socket_path, &attached.attached_root);
}

/// With nothing listening, the runtime must be created rather than reported as
/// a permanent failure: this is what lets the desktop recover at all.
#[test]
fn ensure_owned_starts_a_daemon_when_none_is_listening() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());

    let owned = yi_agent_subagent::attach::ensure_owned_runtime(&cfg, runtime.path().to_path_buf())
        .expect("a dead runtime must be replaced with a fresh one");

    assert!(
        owned.embedded_daemon.is_some(),
        "this process must own the new daemon"
    );
    let socket = yi_agent_store::ipc::socket_path_for(runtime.path()).unwrap();
    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    );
}

/// Strategy B: a healthy daemon already serving this project is adopted, never
/// stolen -- taking it over would break whichever process started it.
#[test]
fn ensure_owned_reuses_a_healthy_daemon() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let database = runtime.path().join("runtime.sqlite");
    let socket = yi_agent_store::ipc::socket_path_for(runtime.path()).unwrap();
    // The pre-existing daemon must be a real one: `Daemon::start`'s default
    // factory has no workspace service, so the attach that follows would be
    // rejected for unrelated reasons and this test could not tell "reused"
    // from "broken".
    let factory = yi_agent_subagent::attach::worker_factory(&cfg, socket).expect("factory");
    let _existing =
        yi_agent_store::ipc::Daemon::start_with_factory(runtime.path(), &database, factory)
            .expect("daemon starts");

    let joined =
        yi_agent_subagent::attach::ensure_owned_runtime(&cfg, runtime.path().to_path_buf())
            .expect("a healthy runtime must be adopted, not replaced");

    assert!(
        joined.embedded_daemon.is_none(),
        "must not steal a healthy daemon"
    );
}
