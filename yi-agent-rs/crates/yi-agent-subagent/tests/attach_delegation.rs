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
    yi_agent_subagent::register_attached_root_tools(
        &mut registry,
        attached.socket_path.clone(),
        &attached.attached_root,
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
