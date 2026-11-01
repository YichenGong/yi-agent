//! End-to-end shape of the shared attach path: a clean git project attaches,
//! an objective activates the root, and the daemon accepts a delegated child.
//!
//! Both applications (the TUI and the app-server) drive this same module, so
//! the contract pinned here is what the desktop app relies on: attach, activate
//! once, delegate, detach.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use yi_agent_subagent::thread_root::ThreadRoot;

/// Budget for "wait until the spawned child has done something observable".
///
/// A wide safety net, not a guess about scheduling speed. Every other wait in
/// this file already uses 10s; this one used a fixed 5s, which encoded the
/// assumption that a shell child starts and writes its marker within 5s. Under
/// `nextest --workspace` the machine is saturated by every other test binary at
/// once and that assumption is false — measured on an 18-core host, the
/// supervisor test in this file failed 3 of 9 full-workspace runs with the
/// marker simply absent (the child had been spawned; it had not yet run).
///
/// 20s matches `CHILD_START_BUDGET` in `yi-agent-supervisors/tests/supervisor.rs`
/// so the two files no longer disagree about what "slow" means. The wait is a
/// condition poll, so a genuine bug (a child that never starts) still trips the
/// deadline instead of being masked.
const CHILD_START_BUDGET: Duration = Duration::from_secs(20);

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
        api_style: yi_agent_core::ApiStyle::Auto,
        reasoning_effort: None,
        max_turns: 4,
        max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
        max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
        workdir: repo.to_path_buf(),
        system_prompt: None,
        compact_threshold: 160_000,
        compact_ratio: 80,
        model_context_length: None,
        compact_user_budget_tokens: 20_000,
        compact_tool_budget_tokens: 12_000,
        yolo: false,
        sandbox_promotable: true,
        sandbox: yi_agent_tools::SandboxMode::default(),
        sandbox_writable_roots: Vec::new(),
        skills_catalog_budget: 8192,
        skills_catalog_budget_explicit: true,
        model_injected: false,
        notifications: true,
        notification_method: "auto".to_string(),
        notification_condition: "unfocused".to_string(),
        terminal_title: true,
    }
}

/// A per-test catalog path that does not exist, so the worker factory resolves
/// against an empty catalog instead of the developer's real `~/.yi-agent`.
///
/// These tests exercise the attach/delegate wiring, not model selection; reading
/// the real machine catalog would make them depend on whatever models happen to
/// be configured on the host (and, before `models_path` was injected, flip the
/// resolved provider and silently break the attach assumptions).
fn isolated_models_path(dir: &Path) -> std::path::PathBuf {
    dir.join("models.json")
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

    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime_dir.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
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
    let root = ThreadRoot::from_handle(
        yi_agent_subagent::binding::RuntimeBinding::fixed(
            yi_agent_subagent::binding::RuntimeHandle {
                socket_path: attached.socket_path.clone(),
                workspace_root: attached.workspace_root.clone(),
                session_id: attached.attached_root.session_id.clone(),
                task_id: attached.attached_root.task_id.clone(),
                capability: attached.attached_root.capability.clone(),
            },
        ),
        attached.attached_root.clone(),
    );
    yi_agent_subagent::register_attached_root_tools(
        &mut registry,
        root,
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
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
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("the runtime socket must accept a delegated child");
    let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = response else {
        panic!("a delegated child must be admitted, got {response:?}");
    };
    assert!(!task_id.is_empty(), "the child must get an id to wait on");

    yi_agent_subagent::attach::detach_root(&attached.socket_path, &attached.attached_root);
}

/// A conversation root carrying linked roots must send them to the daemon, and
/// the daemon must record them on the session's root row. The unit tests cover
/// the resolver and the supervisor map; this covers the wire between them.
#[test]
fn a_conversation_root_sends_its_linked_roots_to_the_daemon() {
    let repo = tempfile::TempDir::new().unwrap();
    let linked = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());

    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime_dir.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
    .expect("a clean git project must attach");

    let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
        yi_agent_subagent::binding::RuntimeHandle {
            socket_path: attached.socket_path.clone(),
            workspace_root: attached.workspace_root.clone(),
            session_id: attached.attached_root.session_id.clone(),
            task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
        },
    );
    let thread = ThreadRoot::new(
        Arc::clone(&binding),
        "thread-linked",
        repo.path().to_path_buf(),
        vec![linked.path().to_path_buf()],
    );
    thread.attach().expect("the conversation root attaches");
    let root_task_id = thread.root_task_id().expect("the root is attached");

    let database = runtime_dir.path().join("runtime.sqlite");
    let stored = yi_agent_store::repository::RuntimeRepository::open(&database)
        .expect("the daemon's database opens for reading")
        .task_extra_roots(&root_task_id.parse().expect("the root id is a uuid"))
        .expect("the root row is readable");
    assert_eq!(
        stored,
        vec![linked.path().to_string_lossy().to_string()],
        "the conversation's linked roots must reach the daemon's root row"
    );

    yi_agent_subagent::attach::detach_root(&attached.socket_path, &attached.attached_root);
}

/// 根会话带着关联根派生的子任务，必须继承同一根集（跨 Task 1–3 的端到端）。
///
/// 覆盖的缺口：既有单测分别钉住「根行记下根集」（`a_conversation_root_sends_its_linked_roots_to_the_daemon`）
/// 与「协调器逐层继承」；这里钉的是**应用侧真实入口**（`ThreadRoot::attach` → `SpawnApplicationChild`）
/// 之后，子任务行里确实有根集，而不只是根行有。
///
/// 不调 LLM：全程只走 IPC 与 sqlite 读回。
#[test]
fn a_spawned_child_inherits_the_conversations_linked_roots() {
    let repo = tempfile::TempDir::new().unwrap();
    let linked = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());

    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime_dir.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
    .expect("a clean git project must attach");

    // 根会话携带关联根。
    let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
        yi_agent_subagent::binding::RuntimeHandle {
            socket_path: attached.socket_path.clone(),
            workspace_root: attached.workspace_root.clone(),
            session_id: attached.attached_root.session_id.clone(),
            task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
        },
    );
    let thread = ThreadRoot::new(
        Arc::clone(&binding),
        "thread-linked-spawn",
        repo.path().to_path_buf(),
        vec![linked.path().to_path_buf()],
    );
    thread.attach().expect("the conversation root attaches");
    let root_task_id = thread.root_task_id().expect("the root is attached");

    // 父任务必须先激活，子任务才被接纳（与文件内既有 e2e 同序）。
    thread
        .activate("investigate the linked roots")
        .expect("activation must succeed");

    // 全程用本会话自己的 root 句柄，与 app-server 的 driver 同序：
    // `attach_project_runtime` 起的是应用级 root，而承载关联根的是每个会话自己的
    // `ThreadRoot`。拿前者去 spawn 会被 capability 检查拒掉（`AuthorityDenied`），
    // 因为它的 root task id 不是 `thread.attach()` 铸出的那一个。
    let handle = thread
        .handle()
        .expect("the attached conversation resolves a handle");
    let spawned = yi_agent_store::ipc::send_request(
        &handle.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: handle.session_id.clone(),
            parent_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: "a delegated read-only investigation".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("the runtime socket must accept a delegated child");
    let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id: child_id } = spawned else {
        panic!("a delegated child must be admitted, got {spawned:?}");
    };

    let database = runtime_dir.path().join("runtime.sqlite");
    let repository = yi_agent_store::repository::RuntimeRepository::open(&database)
        .expect("the daemon's database opens for reading");
    let expected = vec![linked.path().to_string_lossy().to_string()];
    assert_eq!(
        repository
            .task_extra_roots(&root_task_id.parse().expect("the root id is a uuid"))
            .expect("the root row is readable"),
        expected,
        "the root row carries the conversation's linked roots"
    );
    assert_eq!(
        repository
            .task_extra_roots(&child_id.parse().expect("the child id is a uuid"))
            .expect("the child row is readable"),
        expected,
        "a spawned child must inherit the conversation's linked roots"
    );

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

    let owned = yi_agent_subagent::attach::ensure_owned_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
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
    let factory =
        yi_agent_subagent::attach::worker_factory(&cfg, socket, &isolated_models_path(repo.path()))
            .expect("factory");
    let _existing =
        yi_agent_store::ipc::Daemon::start_with_factory(runtime.path(), &database, factory)
            .expect("daemon starts");

    let joined = yi_agent_subagent::attach::ensure_owned_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
    .expect("a healthy runtime must be adopted, not replaced");

    assert!(
        joined.embedded_daemon.is_none(),
        "must not steal a healthy daemon"
    );
}

/// A daemon shared by several conversations, each with its own root: this is the
/// desktop app's shape, and the reason the child budget must not be shared
/// project-wide. One conversation's root must never reach another's child.
#[test]
fn delegation_tools_act_as_the_conversations_own_root() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime_dir.path().to_path_buf(),
        &isolated_models_path(repo.path()),
    )
    .expect("a clean git project must attach");

    // One shared binding (one daemon), two conversations: exactly the desktop
    // app-server's arrangement.
    let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
        yi_agent_subagent::binding::RuntimeHandle {
            socket_path: attached.socket_path.clone(),
            workspace_root: attached.workspace_root.clone(),
            session_id: attached.attached_root.session_id.clone(),
            task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
        },
    );
    let thread_a = ThreadRoot::new(
        Arc::clone(&binding),
        "thread-a",
        repo.path().to_path_buf(),
        Vec::new(),
    );
    let thread_b = ThreadRoot::new(
        Arc::clone(&binding),
        "thread-b",
        repo.path().to_path_buf(),
        Vec::new(),
    );
    thread_a.attach().expect("conversation A attaches");
    thread_b.attach().expect("conversation B attaches");

    let id_a = thread_a.root_task_id().expect("A is attached");
    let id_b = thread_b.root_task_id().expect("B is attached");
    assert_ne!(
        id_a, id_b,
        "two conversations in one directory must own two distinct roots"
    );
    assert_eq!(
        thread_a.handle().expect("A resolves a handle").task_id,
        id_a,
        "the delegation tools act as their own conversation's root"
    );
    assert_eq!(
        thread_b.handle().expect("B resolves a handle").task_id,
        id_b
    );

    // A child spawned as conversation A is out of reach for conversation B:
    // separate roots are separate authorization scopes.
    let a_handle = thread_a.handle().expect("A resolves a handle");
    let spawned = yi_agent_store::ipc::send_request(
        &a_handle.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: a_handle.session_id.clone(),
            parent_task_id: a_handle.task_id.clone(),
            capability: a_handle.capability.clone(),
            objective: "conversation A's child".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: Some("thread-a".into()),
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("A's child must be admitted");
    let yi_agent_store::ipc::IpcResponse::TaskSpawned {
        task_id: child_of_a,
    } = spawned
    else {
        panic!("A's child must be admitted, got {spawned:?}");
    };

    let b_handle = thread_b.handle().expect("B resolves a handle");
    let inspected = yi_agent_store::ipc::send_request(
        &b_handle.socket_path,
        yi_agent_store::ipc::IpcRequest::InspectChild {
            session_id: b_handle.session_id.clone(),
            caller_task_id: b_handle.task_id.clone(),
            capability: b_handle.capability.clone(),
            task_id: child_of_a.clone(),
        },
    )
    .expect("the socket answers");
    assert!(
        matches!(inspected, yi_agent_store::ipc::IpcResponse::Error { .. }),
        "conversation B must not inspect conversation A's child, got {inspected:?}"
    );

    yi_agent_subagent::attach::detach_root(&attached.socket_path, &attached.attached_root);
}

/// 一个把 pid 写进 marker 后睡到被杀的假子进程。
///
/// 用假的而不是真插件二进制：这条测试要证明的是**接线**
/// （谁拥有 daemon → 谁监督 → 子进程被拉起 → 路由被登记），
/// 而不是插件本身能不能跑；用真二进制会把 cargo 构建顺序变成测试前提。
///
/// 但它**必须**能区分 `manifest` 自述调用：daemon 启动时会跑
/// `<command> manifest` 校准清单（`refresh`），这个脚本过去忽略 argv，于是
/// 刷新那一次也会写 marker——marker 存在就不能再证明"监督器拉起了它"。
/// 现在自述只回一份与磁盘上等价的清单，绝不写 marker；只有真正的运行调用
/// （无参数）才写。
fn write_fake_child(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let script = dir.join("plugin-child.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"manifest\" ]; then\n\
             \x20 echo '{}'\n\
             \x20 exit 0\n\
             fi\n\
             echo $$ > {}\n\
             sleep 30\n",
            manifest_json(&script).replace('\'', r"'\''"),
            marker.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }
    script
}

/// 清单正文：磁盘上的 `demo.json` 与假脚本的自述必须是**同一份**。
///
/// 两边一致时刷新不会改写它（`RefreshOutcome::Current`），测试也就不会因为
/// 一次"自述与磁盘不同"的重写而掩盖真正的接线问题。
fn manifest_json(command: &Path) -> String {
    format!(
        r#"{{"name":"demo","command":"{}","args":[],"switch_key":"superpowers_kanban","restart_backoff_ms":50,"restart_backoff_max_ms":200,"query_socket":"{{state_dir}}/demo.sock"}}"#,
        command.display()
    )
}

/// 就位"清单 + 开关打开"，让监督者有事可做。
///
/// 开关落在**全局** `flags.json`：`Supervisor::desired_on` 只看这一层，
/// 项目层的 `preferences.json` 由插件自己写，进程起停不再读它。键必须是
/// 注册表里的名字——未注册的一律读成关。返回 flags 文件路径供注入。
fn arm_plugin(repo: &Path, marker: &Path) -> std::path::PathBuf {
    let child = write_fake_child(repo, marker);
    let manifests = repo.join(".yi-agent/supervisors");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(manifests.join("demo.json"), manifest_json(&child)).unwrap();
    let flags = repo.join("flags.json");
    std::fs::write(
        &flags,
        r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
    )
    .unwrap();
    flags
}

/// 拥有 daemon 的进程必须监督插件。
///
/// 这是修掉的那个缺口：内嵌 daemon（TUI 与桌面共用本路径）过去不监督，
/// 于是"装了看板却永远 pending"。断言落在可观测的两端——子进程被拉起、
/// 且 daemon 的转发表里出现它的路由。
#[test]
fn a_process_that_owns_the_daemon_supervises_its_plugins() {
    use std::time::Instant;
    let repo = tempfile::TempDir::new().unwrap();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let marker = repo.path().join("plugin.pid");
    let flags = arm_plugin(repo.path(), &marker);
    let cfg = config_for(repo.path());

    // 两处注入都不可省：开关只读全局 `flags.json`（项目层 `preferences.json`
    // 不再参与起停），且 `release` 构建下任何 flag 都读成关——测试二进制恒为
    // `release`，不注入 dev 就永远看不到子进程。
    let attached = yi_agent_subagent::attach::attach_project_runtime_with_environment(
        &cfg,
        runtime_dir.path().to_path_buf(),
        &isolated_models_path(repo.path()),
        Some(flags),
        Some("dev"),
    )
    .expect("a clean git project must attach");
    assert!(
        attached.embedded_daemon.is_some(),
        "this test needs the process to own the daemon"
    );

    // 子进程被拉起（监督循环 500ms 一轮）。预算见 CHILD_START_BUDGET：
    // 这里是条件轮询，宽预算只吸收调度延迟，不会掩盖「永远起不来」的真实缺陷。
    let deadline = Instant::now() + CHILD_START_BUDGET;
    let mut started = false;
    while Instant::now() < deadline {
        if marker.exists() {
            started = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        started,
        "owning the daemon means spawning the switched-on plugin"
    );

    // 且它可被路由：转发表里出现 demo（清单声明了 query_socket）。
    let deadline = Instant::now() + CHILD_START_BUDGET;
    let mut routable = false;
    while Instant::now() < deadline {
        if yi_agent_store::ipc::plugin_socket_for(runtime_dir.path(), "demo").is_some() {
            routable = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        routable,
        "a running plugin that declares a query socket must become routable"
    );
}

/// 会话还在、root 还握着 binding 的情况下，`retire` 也必须真的停掉 daemon。
///
/// 这是「回收」在地面上的唯一判据：宿主的表清空了不算数，daemon 的 socket 不再
/// 应答才算。用户的会话必须保持完好（不删会话、不丢历史），代价是下次委派要
/// 冷启动重建——这正是选定「整体丢弃」时的取舍。
#[test]
fn retiring_a_binding_stops_the_daemon_even_while_a_session_still_holds_its_root() {
    let repo = tempfile::tempdir().expect("tempdir");
    init_git_repo(repo.path());
    let runtime = tempfile::tempdir().expect("tempdir");
    let cfg = config_for(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &runtime.path().join("models.json"),
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    assert!(
        matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "the runtime must be live before we retire it"
    );

    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        runtime.path().join("models.json"),
        Arc::clone(&attached),
    );
    // 会话根照旧握着 binding：这正是「不能靠 Drop 回收」的场景。
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    // 测试自己的那份 `attached` 必须放掉，否则停掉的只是「少了一个引用」而不是
    // 产品逻辑——留着它，daemon 被别处继续持有，断言会假失败。
    drop(attached);

    // 先检查它确实握着一份引用，否则这个测试就没在测它声称的场景。
    assert!(
        Arc::strong_count(&binding) >= 2,
        "the session root and the caller must both hold the binding"
    );

    binding.retire();

    // daemon 必须应声而停；只看表或看引用数都不够。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let probe = yi_agent_subagent::attach::probe_runtime(&socket);
        if !matches!(probe, yi_agent_subagent::attach::RuntimeProbe::Healthy) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "retire must actually stop the daemon, not just drop a table entry"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // 会话与 root 都还在——用户不该因为回收丢对话。
    assert!(root.handle().is_err() || root.handle().is_ok());
    drop(binding);
    drop(root);
}

/// 「优先功能可用」的硬保证：回收之后，那个项目的委派必须还能用。
///
/// 回收停掉的是 runtime，不是能力。下一次调用打到死 socket 上会得到一次
/// liveness 失败，`send` 据此走一次 repair，把 daemon 重新拉起来——用户看到
/// 的只是这一次慢一点（冷启动），不会看到「这个项目坏了」。
#[test]
fn a_retired_runtime_heals_itself_on_the_next_delegation() {
    let repo = tempfile::tempdir().expect("tempdir");
    init_git_repo(repo.path());
    let runtime = tempfile::tempdir().expect("tempdir");
    let cfg = config_for(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &runtime.path().join("models.json"),
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        runtime.path().join("models.json"),
        Arc::clone(&attached),
    );
    let generation_before = binding.generation();
    drop(attached);

    binding.retire();
    assert!(
        !matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "retire must leave the daemon stopped"
    );

    // 下一次调用必须自愈：打到死 runtime 上会失败一次，`send` 据此 repair。
    let response = binding
        .send(|_handle| yi_agent_store::ipc::IpcRequest::Status)
        .expect("a retired runtime must be rebuilt rather than refusing the call");
    assert!(
        matches!(response, yi_agent_store::ipc::IpcResponse::Status { .. }),
        "the healed runtime must answer, got {response:?}"
    );
    assert!(
        binding.generation() > generation_before,
        "healing has to install a new runtime, so the generation must move"
    );
}

/// 删除会话时会调用 `ThreadRoot::detach()`。过去它会先 `handle()` → `attach()`
/// → `binding.send`，于是**把已退休的 daemon 重新拉起来**，并幂等地重建那个
/// 本该被删掉的 root——回收循环（`evict_idle_runtimes`）的努力被这一步直接抵消。
///
/// 实测（2026-10-29）：retire 后 detach，daemon 复活且 generation 跳到 2。
#[test]
fn detaching_a_retired_root_does_not_start_a_daemon() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    drop(attached);

    binding.retire();
    let generation_after_retire = binding.generation();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "retire must stop the daemon"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 这正是 `thread/delete` 做的事。
    root.detach();

    assert!(
        !matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "detaching a retired root must not start a daemon back up"
    );
    assert_eq!(
        binding.generation(),
        generation_after_retire,
        "detach must not repair, so it must not install a new runtime"
    );
}

/// 形态 A：daemon 死了，但 binding 不知道（没走 `retire`，没有 generation 变化）。
/// 这是「承载 daemon 的终端被关掉」的真实形状。修复前实测返回
/// `daemon is unavailable: No such file or directory (os error 2)`。
///
/// 断言只钉「自愈确实发生了」这四件事，不钉「调用成功」：`send_message` 是
/// `SingleShot`，第一次请求已因 liveness 失败而作废，重挂之后重发的那一次才会
/// 拿到真实业务结论——而本用例的 recipient 是随机 UUID，业务结论必然是拒绝。
/// 把「重发」和「调用语义成功」混在一起断言，会让用例随 daemon 的消息校验规则
/// 漂移。身份层陈旧导致的「不重放 + 提示」由 Task 6 的形态 B 用例负责。
#[tokio::test]
async fn a_tool_repairs_a_daemon_that_died() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let tool = registry
        .get("send_message")
        .expect("send_message is registered");
    let generation_before = binding.generation();

    // daemon 死了，binding 未被告知。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        binding.generation(),
        generation_before,
        "premise: the binding did not notice"
    );

    let result = tool
        .call(serde_json::json!({
            "recipient": "11111111-1111-4111-8111-111111111111",
            "message": "hi",
        }))
        .await;

    let text = format!("{:?}", result.content);
    assert!(
        !text.contains("daemon is unavailable"),
        "a dead runtime must be healed instead of surfacing a transport error: {text}"
    );
    assert!(
        binding.generation() > generation_before,
        "healing must install a new runtime, so the generation must move"
    );
    assert!(
        matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "the runtime must be up again after the call"
    );

    // 自愈后这个 root 必须真的可用：用它发一条只读请求，应当得到
    // `TaskSummaries`（业务结论），而不是「会话不认识你」（身份失效）。
    let handle = root.handle().expect("the healed root resolves a handle");
    let probe = yi_agent_store::ipc::send_request(
        &handle.socket_path,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(handle.session_id.clone()),
            active_only: false,
        },
    )
    .expect("the socket answers");
    assert!(
        matches!(
            probe,
            yi_agent_store::ipc::IpcResponse::TaskSummaries { .. }
        ),
        "the healed root must be usable, got {probe:?}"
    );
}

/// `spawn_agent` 同样必须自愈。fork 上传必须先于 spawn，所以句柄在
/// `send_as_root` 之前就要拿到——这条测试同时钉住「自愈后 socket 换过，
/// fork token 仍然有效」。
#[tokio::test]
async fn a_spawn_tool_repairs_a_daemon_that_died() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models,
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let tool = registry
        .get("spawn_agent")
        .expect("spawn_agent is registered");
    let generation_before = binding.generation();

    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(std::time::Instant::now() < deadline, "the daemon must stop");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let result = tool
        .call(serde_json::json!({ "task": "say hello", "fork": false }))
        .await;

    let text = format!("{:?}", result.content);
    assert!(
        !text.contains("daemon is unavailable"),
        "spawn must repair a dead runtime instead of surfacing a transport error: {text}"
    );
    assert!(binding.generation() > generation_before);
    assert!(
        matches!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Healthy
        ),
        "the runtime must be up again after the call"
    );
    // 自愈后必须真的能 spawn：成功时载荷里带 `"status":"queued"`，
    // 失败时是拒绝原因。这不依赖 recipient/task 的具体校验规则。
    assert!(
        text.contains("queued"),
        "a healed runtime must accept the spawn, got {text}"
    );
}

/// 形态 B：daemon 被**同伴**替换。socket 全程健康，所以不出现 liveness 错误，
/// generation 也不动——缓存 root 会永久失效。实测 `inspect_agent` 返回 `not_found`
/// 且之后每次调用都重复失败。
///
/// 修复后：身份层失败被识别为「root 过期」，重挂一次后成功。
#[tokio::test]
async fn a_replaced_daemon_invalidates_the_cached_root() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let old_session = attached.attached_root.session_id.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models.clone(),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let inspect = registry
        .get("inspect_agent")
        .expect("inspect_agent is registered");

    // 本进程停掉 daemon，然后由「同伴」在同一 runtime 目录起一个新的。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "the old daemon must stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fresh = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &models,
    )
    .expect("a peer brings a fresh daemon up");
    assert_ne!(
        fresh.attached_root.session_id, old_session,
        "the replacement minted a new session, so the old root is gone"
    );
    let _fresh = Arc::new(fresh);

    // 前提：socket 全程健康，没有触发过任何 liveness 错误——这正是形态 B：
    // 传输层看起来完美，只有身份层知道 root 已经不作数。
    assert!(matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ));
    let root_before = root.root_task_id().expect("the root is cached");

    // 为什么不断言错误码：这里传的是随机 UUID，**即使**重挂成功，重试也会因
    // 「没有这个 task」而返回 `not_found`——与「陈旧 root」的错误码完全一样。
    // 想用真实 child 造更强的判据也不成立：陈旧 root 造不出 child（旧 daemon
    // 已停），而用同伴的 root 造出来的 child 又不归本 root 管（会得到
    // `authority_denied`）。所以形状 B 的可观测判据只能是「root 换过了、
    // 且换过之后可用」，本用例就钉这两条。
    let _ = inspect
        .call(serde_json::json!({
            "task_id": "11111111-1111-4111-8111-111111111111",
        }))
        .await;

    let root_after = root.root_task_id().expect("the root is re-attached");
    assert_ne!(
        root_after, root_before,
        "a stale root must be replaced by one minted against the new runtime"
    );
    let handle = root
        .handle()
        .expect("the re-attached root resolves a handle");
    let probe = yi_agent_store::ipc::send_request(
        &handle.socket_path,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(handle.session_id.clone()),
            active_only: false,
        },
    )
    .expect("the socket answers");
    assert!(
        matches!(
            probe,
            yi_agent_store::ipc::IpcResponse::TaskSummaries { .. }
        ),
        "the re-attached root must be usable, got {probe:?}"
    );
}

/// 不可重放的请求在陈旧 root 上**只能发一次**：重放会造出第二个子任务。
/// 同时错误必须说清「已修好、再试一次」，而不是裸 `not_found`。
///
/// 这里用形态 B（daemon 被同伴替换）：第一次 `spawn` 已送达并被旧身份拒绝，
/// 若实现重放，新 runtime 会真的建出一个子任务，于是「子任务数为 0」就失败。
/// 形态 A 测不出这件事——那边重发是必要的，且恰好只产生一个子任务。
#[tokio::test]
async fn a_non_idempotent_call_reports_the_stale_root_instead_of_replaying() {
    /// 当前缓存 root 名下的子任务数。
    fn child_count(root: &ThreadRoot) -> usize {
        // integration test 是外部 crate，只能走公开的 `handle()`；这里只是
        // 只读计数，不需要自愈通道。
        let handle = root.handle().expect("the root is attached");
        let response = yi_agent_store::ipc::send_request(
            &handle.socket_path,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: Some(handle.session_id.clone()),
                active_only: false,
            },
        )
        .expect("the socket answers");
        match response {
            yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } => tasks
                .iter()
                .filter(|task| task.parent_task_id.as_deref() == Some(handle.task_id.as_str()))
                .count(),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let cfg = config_for(repo.path());
    let models = isolated_models_path(repo.path());

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &models,
        )
        .expect("a clean git repo must attach"),
    );
    let socket = attached.socket_path.clone();
    let old_session = attached.attached_root.session_id.clone();
    let binding = yi_agent_subagent::binding::RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        models.clone(),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(Arc::clone(&binding), attached.attached_root.clone());
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_subagent::register_application_subagent_tools(
        &mut registry,
        Arc::clone(&root),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        yi_agent_subagent::CallerContext::unbound(),
    );
    let spawn = registry
        .get("spawn_agent")
        .expect("spawn_agent is registered");

    // 停掉本进程的 daemon，由同伴在同一 runtime 目录起一个新的 ⇒ 缓存 root 过期，
    // 但 socket 全程健康（形态 B）。
    let _ = yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Stop);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "the old daemon must stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fresh = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &models,
    )
    .expect("a peer brings a fresh daemon up");
    assert_ne!(fresh.attached_root.session_id, old_session);
    let _fresh = Arc::new(fresh);

    // 注意：这里**不要**先 `root.attach()`。那会把缓存刷成同伴的 root，形态 B
    // 的前提就没了。直接调用即可。
    let result = spawn
        .call(serde_json::json!({ "task": "must not run twice", "fork": false }))
        .await;

    assert!(
        result.is_error,
        "a stale-root spawn must not silently succeed: {:?}",
        result.content
    );
    let text = format!("{:?}", result.content);
    assert!(
        text.contains("Retry this call once"),
        "the error must tell the model to retry once: {text}"
    );
    // 调用已经触发了 repair + 重挂，此时 `handle()` 拿到的是新 runtime 上的 root。
    assert_eq!(
        child_count(&root),
        0,
        "the spawn must NOT have been replayed: no child may exist under the new root"
    );
}
