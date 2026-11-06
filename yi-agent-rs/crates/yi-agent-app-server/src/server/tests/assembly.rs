use super::*;

/// 单轮逻辑是共享的 `watch::once`(Task 3 已在其自身 crate 内测过);这里只
/// 断言 app-server 用的是同名同款:读登记 → 把登记项目交给 ensurer。
#[test]
fn the_app_side_loop_ensures_every_registered_project() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    yi_agent_store::resident::require(dir.path(), &a, "example-plugin").unwrap();
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    let count = yi_agent_supervisors::watch::once(dir.path(), &mut |projects| {
        seen.extend_from_slice(projects)
    });
    assert_eq!(count, 1);
    assert_eq!(seen, vec![a]);
}

/// A prepared turn over `root`, for tests that must drive the fenced spawn path.
///
/// A production daemon refuses a write that names no turn, so a test that sends
/// `SpawnApplicationChild` straight from the attach would be asserting a shape the
/// contract exists to forbid. This is the same sequence every host runs: attach the
/// container with its own resume digest, prepare, open the turn, register it.
fn prepared_turn(
    root: &Arc<ThreadRoot>,
    identity_directory: &std::path::Path,
    turn_id: &str,
) -> (
    Arc<yi_agent_subagent::session_delegation::SessionTurnClient>,
    yi_agent_lifecycle::foreground::ExecutionIdentity,
    yi_agent_subagent::session_delegation::SessionHost,
) {
    use yi_agent_lifecycle::session_delegation::{SessionIdentity, session_resume_digest};
    use yi_agent_subagent::session_delegation::SessionHost;
    use yi_agent_subagent::session_identity::SessionIdentityStore;

    let secret = SessionIdentityStore::new(identity_directory.to_path_buf())
        .load_or_create(root.thread_id())
        .expect("the conversation mints a resume secret");
    let attachment = root
        .attach_session_container(
            &SessionIdentity::Embedded(secret.clone()),
            &format!("op-attach-{turn_id}"),
            Some(session_resume_digest(&secret)),
        )
        .expect("the container attaches");
    let mut host = SessionHost::new(
        Arc::clone(root),
        SessionIdentity::Embedded(secret),
        None,
        attachment,
    );
    host.prepare(&format!("op-prepare-{turn_id}"))
        .expect("the embedded identity prepares its binding");
    let turn = host.begin_turn(turn_id).expect("the turn opens");
    let execution = host
        .register_execution(turn_id)
        .expect("the turn registers its execution before it writes");
    (Arc::new(turn), execution, host)
}

/// The fenced spawn a turn issues.
fn spawn_under_turn(
    turn: &Arc<yi_agent_subagent::session_delegation::SessionTurnClient>,
    objective: &str,
    mode: Option<&str>,
) -> yi_agent_store::ipc::IpcResponse {
    turn.send(
        |handle: &yi_agent_subagent::binding::RuntimeHandle,
         context: &yi_agent_lifecycle::session_delegation::SessionTurn| {
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: handle.session_id.clone(),
                parent_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: objective.to_string(),
                mode: mode.map(str::to_string),
                model: None,
                workdir: None,
                thread_id: None,
                sandbox: None,
                fork_token: None,
                session_turn: Some(context.clone()),
            }
        },
    )
    .expect("the runtime socket answers")
}

/// The five tools a root agent needs to delegate.
const DELEGATION_TOOLS: [&str; 5] = [
    "spawn_agent",
    "send_message",
    "wait_agent",
    "inspect_agent",
    "cancel_agent",
];

/// A git project thread reaches the delegation tools.
///
/// This is the desktop half of the TUI's
/// `build_tui_root_tools_registers_subagent_tools_for_attached_runtime`: the
/// app-server never had the wiring, so the model had no `spawn_agent`.
#[test]
fn a_git_project_gets_the_delegation_tools() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let tooling = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling");
    let names = tooling.registry.names();

    for expected in DELEGATION_TOOLS {
        assert!(
            names.contains(&expected.to_string()),
            "an attached root must expose {expected}, got {names:?}"
        );
    }

    // Existing is not enough: the five tools have to work here. Drive the
    // same request `spawn_agent` sends, so the test fails the day a git
    // project stops admitting a delegated child.
    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("activation must succeed");
    let (turn, _execution, _host) = prepared_turn(
        &root,
        &runtime.path().join(".session-identities"),
        "turn-assembly-git",
    );
    let spawned = spawn_under_turn(
        &turn,
        "a delegated read-only investigation",
        Some("read_only"),
    );
    let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = spawned else {
        panic!("a git project must admit a delegated child, got {spawned:?}");
    };
    assert!(!task_id.is_empty(), "the child must get an id to wait on");
}

/// 会话的工具集里必须有 `show_git_diff`：模型没有它就无法把 diff 推到用户眼前。
///
/// 与 `a_git_project_gets_the_delegation_tools` 走同一条 setup，故两者一起失败时
/// 说明是共享接线断了，而不是单点回归。
#[test]
fn a_git_project_gets_the_show_git_diff_tool() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling")
    .registry
    .names();

    assert!(
        names.contains(&"show_git_diff".to_string()),
        "an attached root must expose show_git_diff, got {names:?}"
    );
}

/// One live controller backs both the root's builtin tools and the
/// subagent spawn tools: flipping the thread's YOLO switch must move the
/// root's `bash` sandbox, proving there is no second, static copy.
#[test]
fn delegation_tooling_uses_the_threads_live_yolo_switch() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();
    // The default test sandbox is workspace-write and promotable.
    assert!(cfg.sandbox_promotable);

    let attached = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &isolated_models_path(runtime.path()),
    )
    .expect("a clean git repo must attach");

    let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
        yi_agent_subagent::binding::RuntimeHandle {
            socket_path: attached.socket_path.clone(),
            workspace_root: attached.workspace_root.clone(),
            session_id: attached.attached_root.session_id.clone(),
            task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
        },
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
    let tooling = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        switch.clone(),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling");
    let bash = tooling.registry.get("bash").expect("bash is registered");
    assert_eq!(
        bash.sandbox_mode(),
        Some("workspace-write"),
        "with YOLO off the root bash runs under workspace-write"
    );

    // Flipping the thread's live switch must escalate the already-built
    // registry: the controller is shared, not snapshotted.
    switch.set(true);
    assert_eq!(
        bash.sandbox_mode(),
        Some("danger-full-access"),
        "flipping the thread YOLO switch must escalate the root bash sandbox"
    );
}

/// 驱动会话的 shell 必须看得见插件给的环境变量。
///
/// 这是 `state --self` 的**唯一**信息来源：模型看不到 thread id、更不知道自己在
/// 哪张卡上，只能靠装配期注入。若这里断了，`--self` 会报「当前不在卡片会话里」，
/// 而看板**不会**替它补状态——卡片就停在原处。
///
/// 断言打在**真跑起来的 bash** 上，而不是「参数传进去了」：从
/// `build_runtime_tooling` 到 `ToolsContext` 再到子进程有三道手，只有跑一次
/// 才发现哪一道把值丢了。
#[tokio::test]
async fn the_session_env_reaches_the_bash_child() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let tooling = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        Some(false),
        vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
    )
    .expect("tooling");

    let bash = tooling.registry.get("bash").expect("bash is registered");
    let result = bash
        .call(serde_json::json!({
            "command": "echo \"[$SUPERPOWERS_KANBAN_CARD]\"",
            "timeout": 5,
            "expected_timeout_sec": 3,
        }))
        .await;
    assert!(!result.is_error, "the child must run: {result:?}");
    let yi_agent_core::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected a text block");
    };
    assert!(
        text.contains("[card-1]"),
        "the plugin's handle must reach the child: {text}"
    );
}

/// 生产装配必须把**同一个** `LeadReplies` 接进 `AskLeadTool`。
///
/// 这是本任务里唯一一处「漏了也没有编译错误」的接点:字段漏传、或忘了
/// `.with_replies(..)`,`blocking: true` 只会静默退化成 P1 的显式拒绝
/// (「not implemented」)。所以断言必须打在**装配出来的工具**上——它是
/// 唯一能把「登记表真的接上了」与「登记表只是建出来放在别处」分开的观测点。
#[test]
fn the_assembly_hands_the_reply_registry_to_ask_lead() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    let inbox = crate::lead::LeadInbox::new();
    let replies = crate::lead::LeadReplies::new();

    let tooling = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        inbox.clone(),
        replies.clone(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling");

    // 扮演主循环:订阅信箱、取出投递携带的 `request_id`、按它回填。
    // 这条路径只用到公开 API,故它同时钉住「装配接上了」与「登记表是同一个」。
    let mut lead_rx = inbox.subscribe();
    let block_on = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let text = block_on.block_on(async move {
        let ask_lead = tooling
            .registry
            .get("ask_lead")
            .expect("ask_lead must be registered");
        let call = ask_lead.call(serde_json::json!({
            "message": "需要你裁决 X",
            "blocking": true,
        }));
        tokio::pin!(call);
        let result = tokio::select! {
            // 装配漏接时 `call` 立刻返回(P1 的显式拒绝),这一分支先就绪,
            // 下面的断言据此报出真相,而不是干等 2 分钟超时。
            result = &mut call => result,
            envelope = lead_rx.recv() => {
                let envelope = envelope.expect("the tool must deliver to the lead inbox");
                let request_id = envelope
                    .request_id
                    .clone()
                    .expect("blocking delivery must carry a request_id");
                assert!(envelope.request_id.is_some(), "blocking delivery keeps its id");
                assert!(
                    replies.resolve(&request_id, "先别动".to_string()),
                    "the registry the tool registered into must be the injected one"
                );
                // 回填后在同一个运行时里等工具收口(有界:回信已投递)。
                tokio::time::timeout(Duration::from_secs(5), call)
                    .await
                    .expect("the tool must return once its request_id is resolved")
            }
        };
        result
            .content
            .iter()
            .filter_map(|block| match block {
                yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<String>()
    });

    assert!(
        !text.contains("not implemented"),
        "the assembly must wire the reply registry instead of falling back to \
         the P1 refusal: {text}"
    );
    assert!(
        text.contains("先别动"),
        "the waiting thread must receive the reply resolved for its request_id: {text}"
    );
    let _ = attached;
}

/// A plain directory has no checkout to move into, so the root runs in
/// place. That is the shared bring-up's decision (the TUI attaches a plain
/// directory the same way), not an app-server rule, and the tools it
/// registers are exactly as usable as the project is.
#[test]
fn a_non_git_cwd_attaches_in_place_with_the_delegation_tools() {
    let plain = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = plain.path().to_path_buf();

    let outcome = yi_agent_subagent::attach::attach_project_runtime(
        &cfg,
        runtime.path().to_path_buf(),
        &isolated_models_path(runtime.path()),
    );

    let attached = Arc::new(outcome.expect("a plain directory attaches as an in-place root"));
    assert_eq!(
        attached.workspace_root, attached.project_root,
        "a non-git project has no checkout to move the root into"
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling")
    .registry
    .names();
    for expected in DELEGATION_TOOLS {
        assert!(
            names.contains(&expected.to_string()),
            "attach in place still yields a root, so {expected} must be present, got {names:?}"
        );
    }

    assert_eq!(
        attached.workspace_root, attached.project_root,
        "an in-place root is the project directory itself"
    );
    // Activation must still work: this is what the thread's first turn does,
    // and a plain directory is not itself a reason for that to fail.
    yi_agent_subagent::attach::activate_root(
        &attached.socket_path,
        &attached.attached_root,
        "investigate the build",
    )
    .expect("an in-place root must activate");

    // Read-only delegation works in a plain directory: it writes nothing, so
    // there is no worktree to recover into and no delivery to validate. The
    // desktop app's default cwd is $HOME, so this is the ordinary case, not an
    // edge one.
    let (turn, _execution, _host) = prepared_turn(
        &root,
        &runtime.path().join(".session-identities"),
        "turn-assembly-in-place",
    );
    let read_only = spawn_under_turn(&turn, "a read-only investigation", Some("read_only"));
    assert!(
        matches!(
            read_only,
            yi_agent_store::ipc::IpcResponse::TaskSpawned { .. }
        ),
        "a plain directory must still admit a read-only child, got {read_only:?}"
    );

    // Coding is where it stops: a commit is the recovery boundary a coding
    // child cannot work without. The refusal must be recognizable rather than a
    // bare internal error, so the model can explain it and offer to initialize.
    let coding = spawn_under_turn(&turn, "change the plain directory", Some("coding"));
    let yi_agent_store::ipc::IpcResponse::Error { code, message } = coding else {
        panic!("a plain directory must refuse a coding child, got {coding:?}");
    };
    assert_eq!(code, yi_agent_store::ipc::IpcErrorCode::Validation);
    assert!(
        message
            .as_deref()
            .is_some_and(|text| text.contains("coding_requires_git")),
        "the refusal must be actionable, got {message:?}"
    );
}

/// One directory must never key two ways depending on whether it exists yet.
///
/// Attach runs while the project exists, but a lookup can be asked to key a
/// path that is momentarily absent; if the two disagree the map misses and
/// delegation goes silently dark. `/tmp` vs `/private/tmp` on macOS is the
/// everyday case: canonicalizing resolved it only once the directory was
/// there, so an absent path used to key as its literal spelling.
#[test]
fn a_project_directory_keys_the_same_before_and_after_it_exists() {
    // Reach the directory through a symlinked parent so its literal spelling
    // and its canonical path genuinely differ -- this is the shape of the
    // macOS `/tmp` -> `/private/tmp` divergence.
    let real = tempfile::TempDir::new().unwrap();
    let link_holder = tempfile::TempDir::new().unwrap();
    let link = link_holder.path().join("linked");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    let child = link.join("project");

    let absent_key = project_key(&child);
    std::fs::create_dir_all(&child).unwrap();
    let present_key = project_key(&child);

    assert_eq!(
        absent_key, present_key,
        "keying an absent directory must match keying it once it exists"
    );
    assert_eq!(
        present_key,
        child.canonicalize().unwrap(),
        "an existing directory must key as its canonical path"
    );
}

/// A path whose ancestor is a symlink keys identically whether reached
/// through the link or the target, so two spellings of one project share a
/// runtime instead of attaching two.
#[test]
fn a_symlinked_project_keys_by_its_target() {
    let target = tempfile::TempDir::new().unwrap();
    let link_parent = tempfile::TempDir::new().unwrap();
    let link = link_parent.path().join("link");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();

    assert_eq!(project_key(&link), project_key(target.path()));
}

/// Two threads in one cwd share the daemon but never the root: sharing the
/// root is what made every conversation in a directory split one
/// `MAX_DIRECT_CHILDREN` budget.
///
/// It drives the real wiring (`attach_delegation`), not two hand-built
/// `ThreadRoot`s: a test that builds its own roots would stay green even
/// while the wiring still shared one.
#[test]
fn two_threads_in_one_cwd_share_the_daemon_but_not_the_root() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    let cwd = cfg.workdir.to_string_lossy().to_string();

    let first = attach_delegation(
        &runtimes,
        &thread_roots,
        runtime.path(),
        &isolated_models_path(runtime.path()),
        &cfg,
        &cwd,
        "thread-a",
        &test_theme(),
        &crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        &crate::lead::LeadInbox::new(),
        &crate::lead::LeadReplies::new(),
        &crate::lead_control::LeadControlInbox::new(),
        &crate::lead_control::LeadControlReplies::new(),
        // 这些测试直接调 attach_delegation，不起会话，故无起点。
        None,
        // 组长开关：这些用例断言的是 root 复用，不涉组长工具，关着即可
        // （也顺带钉住「关掉时装配不报错」）。
        None,
        // 这些用例不涉注入：空集与 RPC 路径一致。
        Vec::new(),
        build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
    );
    let second = attach_delegation(
        &runtimes,
        &thread_roots,
        runtime.path(),
        &isolated_models_path(runtime.path()),
        &cfg,
        &cwd,
        "thread-b",
        &test_theme(),
        &crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        &crate::lead::LeadInbox::new(),
        &crate::lead::LeadReplies::new(),
        &crate::lead_control::LeadControlInbox::new(),
        &crate::lead_control::LeadControlReplies::new(),
        // 这些测试直接调 attach_delegation，不起会话，故无起点。
        None,
        // 组长开关：这些用例断言的是 root 复用，不涉组长工具，关着即可
        // （也顺带钉住「关掉时装配不报错」）。
        None,
        // 这些用例不涉注入：空集与 RPC 路径一致。
        Vec::new(),
        build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
    );

    assert_eq!(runtimes.lock().unwrap().len(), 1, "one daemon per project");
    let a = first
        .runtime
        .expect("thread a attached")
        .root_task_id()
        .expect("thread a root is attached");
    let b = second
        .runtime
        .expect("thread b attached")
        .root_task_id()
        .expect("thread b root is attached");
    assert_ne!(a, b, "two conversations must not share one root");
}

/// A runtime whose daemon died (its owning terminal was closed, say) must
/// come back on the next call instead of being cached as a failure forever.
///
/// The first half is the exact desktop failure: a daemon started by another
/// process exits, and the socket it left behind answers nothing.
#[test]
fn a_dead_runtime_is_repaired_on_the_next_tool_call() {
    let repo = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let binding = attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    )
    .expect("a git project must attach");
    let socket = binding.current().unwrap().socket_path.clone();

    // Simulate the owning process exiting: drop every handle to the runtime
    // (this binding and the cached one) so its embedded daemon is dropped too.
    drop(binding);
    runtimes.lock().unwrap().clear();
    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Dead,
        "the daemon must be gone before we test recovery"
    );

    // Asking again must replace the dead runtime rather than cache the miss.
    let repaired = attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    )
    .expect("a dead runtime must be replaced, not cached as a failure");
    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&repaired.current().unwrap().socket_path),
        yi_agent_subagent::attach::RuntimeProbe::Healthy,
        "the repaired runtime must be reachable"
    );
}

/// A project directory that no longer exists must not be brought up.
#[test]
fn a_vanished_project_directory_is_refused_and_not_recreated() {
    let repo = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();
    let workdir = cfg.workdir.clone();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));

    drop(repo);

    // `RuntimeBinding` 不实现 `Debug`，故用 match 而不是 `expect_err`。
    let error = match attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    ) {
        Ok(_) => panic!("a vanished project directory must not attach"),
        Err(error) => error,
    };
    assert!(
        error.contains(&workdir.display().to_string()),
        "the error must name the missing directory, got: {error}"
    );
    assert!(
        !workdir.exists(),
        "refusing to attach must not recreate the project directory"
    );

    std::fs::create_dir_all(&workdir).unwrap();
    init_git_repo(&workdir);
    attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    )
    .expect("a restored project directory must attach on the next call");
}

/// A transient attach failure must not be cached as a permanent one.
///
/// The incident (2026-11-01 desktop cold start): the first `thread/start` raced
/// the `daemon serve` that was binding the socket, `attach` failed with ENOENT,
/// and because the cache stores `Err` values the whole app-server process kept
/// answering "delegation unavailable" -- every later thread silently lost
/// `spawn_agent` until the desktop was restarted.
///
/// Inserting the failure by hand is the point: `attach_cwd_runtime` stores
/// whatever `ensure_owned_runtime` returns, so a test that can only fail it
/// through a real race would be flaky rather than a regression guard.
#[test]
fn a_transient_attach_failure_is_not_cached_as_a_permanent_one() {
    let repo = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let key = project_key(&cfg.workdir);
    runtimes.lock().unwrap().insert(
        key,
        Err(yi_agent_subagent::attach::AttachFailure::transient(
            "attach",
            "No such file or directory (os error 2)",
        )),
    );

    let binding = attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    )
    .expect("a transient attach failure must be retried, not replayed from the cache");

    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&binding.current().unwrap().socket_path),
        yi_agent_subagent::attach::RuntimeProbe::Healthy,
        "the retry must land on a live runtime"
    );
}

/// A *non*-transient failure stays cached: retrying it on every thread would
/// re-run a doomed bring-up forever, which is what the cache exists to prevent.
#[test]
fn a_non_transient_failure_is_still_cached() {
    let repo = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let key = project_key(&cfg.workdir);
    let stored = yi_agent_subagent::attach::AttachFailure::new(
        "worker factory",
        "the catalog cannot be read",
    );
    runtimes
        .lock()
        .unwrap()
        .insert(key.clone(), Err(stored.clone()));

    let error = match attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    ) {
        Ok(_) => panic!("a cached non-transient failure must be replayed"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        stored.to_string(),
        "the cached cause must be replayed verbatim"
    );
}

/// The incident, end to end.
#[test]
fn a_reclaimed_worktree_converges_instead_of_retrying_forever() {
    let directory = tempfile::TempDir::new().unwrap();
    let deep_slug = "2026-10-17-backend-module-split-design-2026-10-17-backend-module-split";
    let workdir = directory.path().join(deep_slug);
    std::fs::create_dir_all(&workdir).unwrap();
    init_git_repo(&workdir);
    let mut cfg = test_config();
    cfg.workdir = workdir.clone();
    let runtime = workdir.join(".yi-agent/runtime");

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let binding = attach_cwd_runtime(&runtimes, &runtime, &cfg, &isolated_models_path(&runtime))
        .expect("a git project must attach");
    let socket = binding.current().unwrap().socket_path.clone();
    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy,
        "the runtime must be live before we reclaim its directory"
    );
    assert!(
        !socket.starts_with(&runtime),
        "precondition: the socket must live outside the runtime directory"
    );

    drop(binding);
    std::fs::remove_dir_all(&workdir).unwrap();

    let error = match attach_cwd_runtime(&runtimes, &runtime, &cfg, &isolated_models_path(&runtime))
    {
        Ok(_) => panic!("a reclaimed worktree must not attach"),
        Err(error) => error,
    };
    assert!(
        error.contains(&workdir.display().to_string()),
        "the error must name the missing directory, got: {error}"
    );
    assert!(
        !workdir.exists(),
        "the refusal must not recreate the reclaimed project directory"
    );

    let mut socket_died = false;
    for _ in 0..240 {
        if !socket.exists() {
            socket_died = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&socket);
    assert!(
        socket_died,
        "the daemon must stop once its runtime directory is gone"
    );

    for _ in 0..5 {
        assert!(
            attach_cwd_runtime(&runtimes, &runtime, &cfg, &isolated_models_path(&runtime)).is_err(),
            "a reclaimed worktree stays refused"
        );
    }
    assert!(
        !workdir.exists(),
        "repeated refusals must not recreate the reclaimed project directory"
    );
}

/// 开关关掉时，**任何**会话都没有组长工具。
///
/// 这是「开关只能关不能开」的硬证据：普通会话拿不到 `ask_lead`，组长会话也
/// 拿不到那 6 个工具。门禁加在装配期，所以这条断言直接覆盖
/// `build_runtime_tooling` 的两个注册分支。
#[test]
fn no_lead_tools_exist_when_the_flag_is_off() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let project_key = repo.path().join(".yi-agent").display().to_string();

    let plain = tooling_names_for(
        repo.path(),
        runtime.path(),
        "thread-plain",
        project_key.clone(),
        Some(false),
    );
    assert!(
        !plain.contains(&"ask_lead".to_string()),
        "a plain session must have no ask_lead when the flag is off: {plain:?}"
    );

    // 组长会话：落一份真的 `role:"lead"` meta，让 `is_lead_thread` 为真。
    let store = crate::thread_store::ThreadStore::new(repo.path());
    let (lead_meta, _) = crate::lead::ensure_lead_meta(&store, repo.path()).expect("lead meta");
    let lead = tooling_names_for(
        repo.path(),
        runtime.path(),
        &lead_meta.thread_id,
        project_key,
        Some(false),
    );
    assert!(
        !lead.iter().any(|n| n.contains("lead")),
        "a lead must have no lead tools when the flag is off: {lead:?}"
    );
}

/// 开关打开时，两类会话各拿各的工具，且互不越界。
#[test]
fn lead_tools_follow_the_flag() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let project_key = repo.path().join(".yi-agent").display().to_string();

    let plain = tooling_names_for(
        repo.path(),
        runtime.path(),
        "thread-plain",
        project_key.clone(),
        Some(true),
    );
    assert!(
        plain.contains(&"ask_lead".to_string()),
        "a plain session gets ask_lead when the flag is on: {plain:?}"
    );
    assert!(
        !plain.contains(&"lead_remember".to_string()),
        "the flag must not turn a plain session into a lead: {plain:?}"
    );

    let store = crate::thread_store::ThreadStore::new(repo.path());
    let (lead_meta, _) = crate::lead::ensure_lead_meta(&store, repo.path()).expect("lead meta");
    let lead = tooling_names_for(
        repo.path(),
        runtime.path(),
        &lead_meta.thread_id,
        project_key,
        Some(true),
    );
    for name in [
        "lead_remember",
        "lead_recall",
        "lead_list_threads",
        "lead_read_thread",
        "lead_interrupt_thread",
        "lead_reject_thread",
    ] {
        assert!(
            lead.contains(&name.to_string()),
            "{name} must be registered for a lead when the flag is on: {lead:?}"
        );
    }
    assert!(
        !lead.contains(&"ask_lead".to_string()),
        "a lead must not ask itself: {lead:?}"
    );
}

/// `device/revoke` 对**在线** ws 设备的映射查找必须真的接上:撤销某设备时,
/// 主循环经 `ws_client_for_device` 找到它的 ws 连接并摘除。这里直接验证这条
/// 查找路径——ws 的 E2E 里 B 是"另一台设备",它靠这条映射被踢下线。
#[tokio::test(flavor = "multi_thread")]
async fn ws_client_for_device_resolves_through_the_installed_registry() {
    let registry: WsDeviceRegistry = Arc::new(StdMutex::new(HashMap::new()));
    // `install_device_registry` 幂等:进程里若已有 ws server 先设过全局句柄,
    // 它返回的是那**第一张**表,而不是本次传入的。故必须往返回值里插,才能
    // 保证 `ws_client_for_device` 读到;往局部 `registry` 里插会在并发测试下
    // 抖动失败。
    let global = install_device_registry(registry);
    let cid = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    global
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert("dev-abc".to_string(), cid.clone());

    assert!(
        ws_client_for_device("dev-abc").is_some(),
        "an online ws device must resolve to its ClientId"
    );
    // 未知设备(或连接已摘除)返回 None,`device/revoke` 于是只改设备表。
    assert!(ws_client_for_device("dev-missing").is_none());
}

/// 每个 thread 的工具集都必须带主题工具——这是「对话切主题」的落点。
#[test]
fn the_theme_tool_is_registered_into_a_thread_registry() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = crate::theme_tool::ThemeHandle::new(dir.path().to_path_buf());
    let mut registry = yi_agent_core::ToolRegistry::new();
    register_theme_tool(&mut registry, handle);
    assert!(
        registry.get("set_theme").is_some(),
        "a thread registry must carry the theme tool"
    );
}

/// 委派可用时,`build_runtime_tooling` 造出的 registry 也必须带主题工具
/// (否则每个 git 项目里的 thread 都说不了「切主题」)。
#[test]
fn delegation_tooling_carries_the_theme_tool() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.path().to_path_buf(),
            &isolated_models_path(runtime.path()),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let tooling = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
    )
    .expect("tooling");
    assert!(
        tooling.registry.get("set_theme").is_some(),
        "the delegation registry must carry the theme tool, got {:?}",
        tooling.registry.names()
    );
}

/// 非委派路径(非 git 目录)用的是 bootstrap 工具集的克隆。那一步必须既
/// 补上主题工具、又保留原有工具——用真实的 bootstrap 工具集断言,而不是
/// 自搭一个空注册表。
#[test]
fn the_non_delegation_registry_carries_the_theme_tool() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let built = yi_agent_runtime::bootstrap::bootstrap_agent(
        &cfg,
        yi_agent_runtime::bootstrap::PermissionMode::Interactive,
    )
    .expect("a default config bootstraps without network");
    assert!(
        built.tools.get("bash").is_some(),
        "precondition: the bootstrap carries real tools"
    );

    let registry = registry_with_theme_tool(&built.tools, test_theme());
    assert!(
        registry.get("set_theme").is_some(),
        "the non-delegation registry must carry the theme tool, got {:?}",
        registry.names()
    );
    assert!(
        registry.get("bash").is_some(),
        "cloning the bootstrap registry must keep its original tools"
    );
}

/// 重建 agent 会经过 `Agent::new`,它从零开始、**没有**权限检查器。
/// 非委派路径既然要重建,就必须把审批路径装回去,否则非 git 目录静默失去审批。
#[test]
fn rebuilding_the_thread_agent_keeps_the_approval_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let built = yi_agent_runtime::bootstrap::bootstrap_agent(
        &cfg,
        yi_agent_runtime::bootstrap::PermissionMode::Interactive,
    )
    .expect("a default config bootstraps without network");
    assert!(
        built.agent.permission_checker().is_some(),
        "precondition: the bootstrap attaches the checker"
    );

    let rebuilt = rebuild_thread_agent_with_theme(built, None, test_theme());
    assert!(
        rebuilt.agent.permission_checker().is_some(),
        "the rebuilt agent must keep the permission checker"
    );
    assert!(
        rebuilt.agent.decision_rx().is_some(),
        "the rebuilt agent must keep the decision receiver"
    );
}

/// A managed bring-up that has no instance home must fail loudly.
///
/// The failure mode this guards is the tempting one: fall back to an embedded
/// daemon so *something* starts. That would leave a client-owned process behind a
/// policy whose entire promise is that the daemon is shared, and nobody would ever
/// observe the difference until the client exited and took the runtime with it.
///
/// No daemon is started here, which is the point: the refusal happens before any
/// process work, so the test needs no socket and no project.
#[test]
fn a_managed_choice_without_an_instance_home_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();

    let choice = yi_agent_subagent::binding::BringUpChoice {
        policy: yi_agent_subagent::attach::BringUpPolicy::Managed,
        expected: Some(crate::server::assembly::test_identity(dir.path())),
        // Deliberately absent: the process never installed a context.
        instance_home: None,
    };

    let error = match crate::server::assembly::bring_up_with_for_test(
        &choice,
        &cfg,
        &dir.path().join("runtime"),
        &isolated_models_path(dir.path()),
    ) {
        Ok(_) => panic!("a managed choice with no scope must not produce a binding"),
        Err(error) => error,
    };
    assert!(
        error.cause.contains("instance home"),
        "the failure must name the missing scope, got: {}",
        error.cause
    );
    assert!(
        !dir.path().join("runtime").exists(),
        "refusing must not create the runtime directory"
    );
}

/// An embedded choice keeps using the historical path and needs no context.
///
/// This is the pair to the test above: the two policies must differ in *whether*
/// a scope is required, not merely in which error they raise.
#[test]
fn an_embedded_choice_needs_no_instance_home() {
    let dir = tempfile::tempdir().unwrap();
    let choice = yi_agent_subagent::binding::BringUpChoice {
        policy: yi_agent_subagent::attach::BringUpPolicy::Embedded,
        expected: None,
        instance_home: None,
    };
    // Reaching the actual attach is enough: there is no rejection up front. A
    // failure here would be about the (non-git) directory, not about the policy.
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let outcome = crate::server::assembly::bring_up_with_for_test(
        &choice,
        &cfg,
        &dir.path().join("runtime"),
        &isolated_models_path(dir.path()),
    );
    if let Err(error) = outcome {
        assert!(
            !error.cause.contains("instance home"),
            "embedded must never be rejected for lacking a scope, got: {}",
            error.cause
        );
    }
}
