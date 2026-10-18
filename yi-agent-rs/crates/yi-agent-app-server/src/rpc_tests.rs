use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::rpc::{Domain, domain_for};
use crate::test_support::{EVENT_WAIT, isolated_models_path};

/// detach 的前提：这两个域的状态必须是 `Send + Sync + 'static`，否则
/// `tokio::spawn` 编译不过。这条断言就是「它们能脱离主循环执行」的编译期证明
/// ——主循环是单 task，内联 `await` 会串行堵住所有 thread 的 RPC。
#[test]
fn board_and_plugin_shared_state_is_owned_and_send() {
    fn assert_spawnable<T: Send + Sync + 'static>() {}
    assert_spawnable::<crate::rpc::BoardPluginState>();
}

/// Task 4 的致命通道是**唯一**回到主循环的信号，故单独钉一次:detached handler
/// 向已死的 stdio `local` 写响应失败时，`write_or_fatal` 必须把 `Err` 送上
/// fatal 通道，而不是像对 ws 客户端那样吞掉。搬移前这条 `Err` 由内联
/// `await ...?` 直接传播到主循环（`driver_reports_finished_when_writer_fails`
/// 钉死），故本测试就是 detach 后的等价性证明。
#[tokio::test]
async fn write_or_fatal_reports_a_local_write_failure_on_the_fatal_channel() {
    use std::path::{Path, PathBuf};

    let hub = crate::broadcast::Broadcaster::new();
    let local = crate::broadcast::ClientId::local();
    let _rx = hub.register(local.clone());
    // 出口泵已死:对 `local` 的后续写入必然失败(与
    // `write_response_to_a_gone_ws_client_is_not_fatal` 的反面同款)。
    hub.unregister(&local);

    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::channel::<anyhow::Error>(1);
    let state = crate::rpc::BoardPluginState {
        hub: std::sync::Arc::new(hub),
        client: local,
        board_dir: PathBuf::new(),
        resident_dir: PathBuf::new(),
        plugin_command: PathBuf::new(),
        theme_handle: crate::theme_tool::ThemeHandle::new(std::env::temp_dir()),
        watchman_home: PathBuf::new(),
        watchman_install: std::sync::Arc::new(|_exe: &Path, _home: &Path| -> Result<(), String> {
            Ok(())
        }),
        watchman_uninstall: std::sync::Arc::new(|_home: &Path| -> Result<(), String> { Ok(()) }),
        board_launcher: std::sync::Arc::new(|_project: &Path| -> Result<bool, String> {
            Ok(false)
        }),
        fatal_tx,
    };

    crate::rpc::write_or_fatal(
        &state,
        crate::protocol::RequestId::Num(1),
        crate::server::ok_response(crate::protocol::RequestId::Num(1), serde_json::json!({})),
    )
    .await;

    let error = tokio::time::timeout(EVENT_WAIT, fatal_rx.recv())
        .await
        .expect("a failed local write must report on the fatal channel, not be swallowed")
        .expect("the fatal channel must carry the error");
    assert!(
        error.to_string().contains("disconnected"),
        "the reported error must be write_response's local-disconnect error: {error}"
    );
}

/// 单轮逻辑是共享的 `watch::once`(Task 3 已在其自身 crate 内测过);这里只
/// 断言 app-server 用的是同名同款:读登记 → 把登记项目交给 ensurer。
#[test]
fn the_app_side_loop_ensures_every_registered_project() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    let count =
        yi_agent_boards::watch::once(dir.path(), &mut |projects| seen.extend_from_slice(projects));
    assert_eq!(count, 1);
    assert_eq!(seen, vec![a]);
}

/// A throwaway Git repository. Coding children need one; attaching does not.
fn init_git_repo(dir: &std::path::Path) {
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
    let spawned = yi_agent_store::ipc::send_request(
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
    .expect("the runtime socket must answer");
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

/// 组长私有的记忆工具只给组长:普通会话一个都看不到。
///
/// 负臂是本测试的重点。只断言「组长看得到」会漏掉最危险的接法——把判定写成
/// 「有 meta 就是组长」或干脆无条件注册;那样组长那条断言照样绿,而每个普通
/// 会话都悄悄拿到了写项目账本的权限。故这里同时钉:
/// ① 落过盘但 `role: None` 的会话看不到;② 从未落过盘的会话也看不到;
/// ③ 负臂会话的工具集非空(`ask_lead` 还在),证明「看不到」不是 registry 整体空掉。
///
/// 判定只看落盘的 `ThreadMeta.role`(唯一权威):测试里的正臂就走生产的
/// `ensure_lead_meta` 造 meta,不手搓一个「像组长」的形状。
#[test]
fn only_the_lead_sees_the_memory_tools() {
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
    // `build_runtime_tooling` 取的是 `root.handle()?.workspace_root`,而工具的
    // `project_root` 取 `cfg.workdir`(= thread cwd)。本测试里 thread cwd 就是
    // 仓库顶层,故二者重合(这也是把 store 根绑错也照样绿的原因——分歧情形由
    // `a_lead_in_a_repo_subdirectory_sees_the_memory_tools` 覆盖)。
    // 注意形态:workspace_root 是 cwd 的 **canonical** 形态(macOS 上
    // `/var` → `/private/var`),与项目根是同一个目录,不是另一个路径。
    let project_root = repo.path().canonicalize().expect("repo path canonicalizes");
    assert_eq!(
        attached.workspace_root, project_root,
        "the thread's workspace root must be the project root"
    );
    // 两种拼写必须指向同一份 store:生产里 thread 的 cwd 走的是
    // `resolve_thread_cwd` 的**非 canonical**(显式 cwd 才 canonicalize),
    // 而 workspace_root 是 canonical 的。若二者不是同一个目录,下面「用
    // cfg.workdir 建 store、也用 cfg.workdir 建 meta」就会各写各的,
    // 判定便永远读不到 role——本断言先把这个前提钉死。
    assert_eq!(
        std::fs::canonicalize(cfg.workdir.as_path()).expect("workdir canonicalizes"),
        attached.workspace_root,
        "cfg.workdir and the attached workspace root must be the same directory"
    );

    // 组装 store 时用生产那种拼写(`cfg.workdir`,即 `cwd`,非 canonical),
    // 装配里读 role 的 store 现在也取 `cfg.workdir`,两侧同源。而
    // `workspace_root` 是 canonical 形态,故上面那条断言仍把「两种拼写指向
    // 同一个物理目录」钉死:若二者不是同一目录,本测试会读不到自己写的
    // meta,「组长看到工具」的断言就永远不会绿。
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: "thread-plain".to_string(),
            cwd: cfg.workdir.to_string_lossy().to_string(),
            model: "test-model".to_string(),
            created_at: 1,
            updated_at: 1,
            title: Some("组长".to_string()), // 标题写着「组长」也不许算组长
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            role: None,
            tag: None,
            extra_roots: Vec::new(),
        })
        .expect("a plain meta must persist");
    // 正臂走生产的组长落盘路径,而不是手搓一个 role 字段。
    let (lead, _created) = crate::lead::ensure_lead_meta(&store, &project_root).expect("lead meta");
    assert_eq!(
        lead.role.as_deref(),
        Some(crate::thread_store::LEAD_ROLE),
        "the production lead path must persist the role"
    );

    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let build = |thread_id: &str| {
        build_runtime_tooling(
            &cfg,
            &root,
            thread_id,
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
            crate::git_diff_tool::GitDiffHandle::new(),
            Vec::new(),
            "proj-test".to_string(),
            crate::lead::LeadInbox::new(),
            crate::lead::LeadReplies::new(),
        )
        .expect("tooling")
    };

    let lead_names = build(&lead.thread_id).registry.names();
    assert!(
        lead_names.contains(&"lead_remember".to_string()),
        "the lead must see lead_remember, got {lead_names:?}"
    );
    assert!(
        lead_names.contains(&"lead_recall".to_string()),
        "the lead must see lead_recall, got {lead_names:?}"
    );

    // 负臂:落过盘的普通会话,以及从未落过盘的会话。
    for plain in ["thread-plain", "thread-never-persisted"] {
        let names = build(plain).registry.names();
        assert!(
            !names.contains(&"lead_remember".to_string()),
            "{plain} is not the lead and must not see lead_remember, got {names:?}"
        );
        assert!(
            !names.contains(&"lead_recall".to_string()),
            "{plain} is not the lead and must not see lead_recall, got {names:?}"
        );
        // 非空 witnesses:「看不到」必须是真的缺席,而不是整个 registry 空掉。
        assert!(
            names.contains(&"ask_lead".to_string()),
            "{plain} still gets the shared tools, got {names:?}"
        );
    }

    // `project_root` 绑定必须是真的项目根:跑一次工具,账本要落在
    // `<项目根>/.yi-agent/lead_memory/memory.md`。名字对了但路径绑错
    // (例如绑到 `.yi-agent/threads/..`)会在这里露出来。
    let lead_tooling = build(&lead.thread_id);
    let remember = lead_tooling
        .registry
        .get("lead_remember")
        .expect("the lead's registry must hold lead_remember");
    let block_on = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let out = block_on.block_on(remember.call(serde_json::json!({ "text": "项目根绑定检查" })));
    assert!(!out.is_error, "lead_remember must succeed: {out:?}");
    assert!(
        crate::lead_memory::memory_path(&project_root).exists(),
        "the ledger must land under the project root, not somewhere else"
    );
}

/// 组长的 cwd 落在 git 仓库的**子目录**时,两个记忆工具仍然必须可见。
///
/// 这是上一条测试抓不到的那一格:那里 thread cwd 就是仓库顶层,「读 role 的
/// store 根」与「meta 的落盘处」恰好重合,于是把 store 根绑错成 git toplevel
/// 也照样绿。生产里两者**会**分叉:`attach_project_runtime` 回传的
/// `workspace_root` 是 daemon 的 `git rev-parse --show-toplevel`(root 以
/// `ChildWriteMode::ReadOnly` 就地运行,位置取 `repository_root`),而组长 meta
/// 写在 `<cwd>/.yi-agent/threads`。按 toplevel 去找子目录里的组长,`load` 回
/// `Ok(None)`,真组长便静默丢掉两个工具。
#[test]
fn a_lead_in_a_repo_subdirectory_sees_the_memory_tools() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let subdir = repo.path().join("sub");
    std::fs::create_dir_all(&subdir).expect("the subdirectory must exist");
    // 进程 workdir 仍是仓库顶层(生产的 `cfg.workdir` 是进程级 workdir),
    // 而**本会话的 cwd** 是子目录——分叉就发生在这里。
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
    // 这两条断言是本测试有效性的前提:attach 回来的 `workspace_root` 是 git
    // **顶层**,与会话 cwd(子目录)不是同一个目录。若它们不成立,本测试只是又
    // 踩了一遍「两条路径重合」的情形,对「读错 store」毫无分辨力。
    let toplevel = repo.path().canonicalize().expect("repo path canonicalizes");
    assert_eq!(
        attached.workspace_root, toplevel,
        "the attached workspace root must be the git toplevel"
    );
    assert_ne!(
        std::fs::canonicalize(&subdir).expect("the subdirectory canonicalizes"),
        attached.workspace_root,
        "the thread cwd must diverge from the attached workspace root"
    );

    // 组长 meta 落在**子目录**的 store 里,走生产的落盘路径,而不是手搓字段。
    let cwd_store = crate::thread_store::ThreadStore::new(&subdir);
    let (lead, _created) = crate::lead::ensure_lead_meta(&cwd_store, &subdir).expect("lead meta");
    assert_eq!(
        lead.cwd,
        subdir.to_string_lossy().to_string(),
        "the lead's cwd must be the subdirectory, not the toplevel"
    );
    // 分歧路径的见证:按 toplevel 去找,**读不到**这个组长(修复前的读法)。
    // 这条一旦为真,下面的「工具可见」就不可能是靠 toplevel store 侥幸凑出来的。
    assert!(
        !crate::lead_memory::is_lead_thread(
            &crate::thread_store::ThreadStore::new(&toplevel),
            &lead.thread_id
        ),
        "the toplevel store must not hold this lead, otherwise this test would not \
         exercise the divergent path"
    );

    // 与 `attach_delegation` 同形:先把 thread cfg 的 workdir 设成 thread cwd,
    // 再装配。这正是生产里 `cfg.workdir` 与会话 cwd 同源的那一步。
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = subdir.clone();
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.path().to_path_buf(),
        isolated_models_path(runtime.path()),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    let tooling = build_runtime_tooling(
        &thread_cfg,
        &root,
        &lead.thread_id,
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
    )
    .expect("tooling");
    let names = tooling.registry.names();
    assert!(
        names.contains(&"lead_remember".to_string()),
        "a lead whose cwd is a repo subdirectory must still see lead_remember, got {names:?}"
    );
    assert!(
        names.contains(&"lead_recall".to_string()),
        "a lead whose cwd is a repo subdirectory must still see lead_recall, got {names:?}"
    );

    // 账本必须与组长的 meta 落在**同一个** `.yi-agent` 下(即子目录),而不是
    // toplevel:把 `project_root` 留在 toplevel,会让两个不同 project_key 的组长
    // (`<repo>` 与 `<repo>/sub`)共用一份 `lead_memory/memory.md` —— 跨项目串味。
    let remember = tooling
        .registry
        .get("lead_remember")
        .expect("the lead's registry must hold lead_remember");
    let block_on = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let out = block_on.block_on(remember.call(serde_json::json!({ "text": "子目录组长" })));
    assert!(!out.is_error, "lead_remember must succeed: {out:?}");
    assert!(
        crate::lead_memory::memory_path(&subdir).exists(),
        "the ledger must land in the subdirectory's .yi-agent, next to the lead's meta"
    );
    assert!(
        !crate::lead_memory::memory_path(&toplevel).exists(),
        "the ledger must not land at the toplevel: that key belongs to another project"
    );
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

    // Delegating is where a plain directory stops: the daemon refuses the
    // worker because it cannot name a Git worktree lease to recover into.
    // Pin it here so the boundary is a documented fact rather than a
    // surprise in a chat window -- the desktop app's default cwd is $HOME.
    let spawned = yi_agent_store::ipc::send_request(
        &attached.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: attached.attached_root.session_id.clone(),
            parent_task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
            objective: "a read-only investigation".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("the runtime socket answers even when it refuses");
    assert!(
        !matches!(
            spawned,
            yi_agent_store::ipc::IpcResponse::TaskSpawned { .. }
        ),
        "delegation needs a checkpoint to recover into, so a plain directory \
         must be refused rather than half-admitted; got {spawned:?}"
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

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

/// 极简 provider:只回一段文本后结束。
struct MockProvider;

#[async_trait]
impl yi_agent_core::Provider for MockProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 记录每次调用收到的 message 数,并回显 `n=<count>` 作为 agent 文本。
///
/// 标题生成（`system == TITLE_INSTRUCTIONS`）不是对话轮次，不算在内：否则
/// 「恢复上下文后第二轮应看到 3 条消息」之类的断言会把标题请求也算成一次调用。
struct RecordingProvider {
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait]
impl yi_agent_core::Provider for RecordingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        let n = req.messages.len();
        self.seen.lock().unwrap().push(n);
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(format!("n={n}")),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 捕获每次调用里最后一条 user 文本:证明送进**模型**的确实是带清单的
/// prompt(而不是气泡上的原话)。清单是注入给模型的,气泡只放用户原话。
///
/// 标题生成（`system == TITLE_INSTRUCTIONS`）也是"送进模型"的调用，但它送的是
/// 标题材料而非对话 prompt，故不捕获——本 provider 只关心对话轮次。
struct CapturingProvider {
    prompts: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl yi_agent_core::Provider for CapturingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::message::{ContentBlock, Role};
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        if let Some(last_user) = req.messages.iter().rev().find(|m| m.role == Role::User) {
            let text = last_user
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            self.prompts.lock().unwrap().push(text);
        }
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 记录每次调用收到的 `messages`（含内容块），用于断言图片真的以
/// `ContentBlock::Image` 进了模型上下文，而不只是回显在 item 上。
struct CapturingMessagesProvider {
    /// 每次 provider 调用一份完整消息列表。
    messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>>,
}

#[async_trait]
impl yi_agent_core::Provider for CapturingMessagesProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        self.messages.lock().unwrap().push(req.messages.clone());
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 永不自行结束的 provider:每 5ms 吐一个 delta,turn 会一直活跃,
/// 直到被 `turn/interrupt` 取消。
struct SlowProvider;

#[async_trait]
impl yi_agent_core::Provider for SlowProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = futures::stream::unfold(0u64, |i| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((
                yi_agent_core::provider::ProviderEvent::TextDelta("x".into()),
                i + 1,
            ))
        });
        Ok(events.boxed())
    }
}

/// 每次调用都返回 provider 错误:用于测试 `/compact` 的 `failed` 三态。
struct ErroringProvider;

#[async_trait]
impl yi_agent_core::Provider for ErroringProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        Err(yi_agent_core::provider::ProviderError::Network(
            "boom".into(),
        ))
    }
}

/// 返回固定标题文本的 provider:用于断言首轮标题来自 LLM。
///
/// 主 agent 的对话调用**也**带 system prompt(`config.system_prompt`),所以
/// 区分"标题请求"只能精确比较 `system == Some(TITLE_INSTRUCTIONS)`,绝不能用
/// `system.is_some()`。
struct TitleProvider;

#[async_trait]
impl yi_agent_core::Provider for TitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题请求(恰好命中 TITLE_INSTRUCTIONS)返回固定标题;其余(主 agent
        // 对话)返回正文 "ok"。
        let text = if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            "修复登录报错"
        } else {
            "ok"
        };
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(text.into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 正文正常返回;但标题请求(恰好命中 `TITLE_INSTRUCTIONS`)返回错误，
/// 用于覆盖"生成失败静默回退兜底标题"的路径。
///
/// `title_calls` 记录标题请求的命中次数。仅断言"最终标题 == 兜底标题"是**不具
/// 辨识力**的:兜底标题本就等于 `title_from(prompt)`,与 `touch` 写入的字节完全
/// 一致,哪怕标题请求一次都没发出,断言照样通过。必须能观察到"确实尝试过且失败"。
struct RejectingTitleProvider {
    title_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl yi_agent_core::Provider for RejectingTitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            self.title_calls.fetch_add(1, Ordering::SeqCst);
            return Err(yi_agent_core::provider::ProviderError::Network(
                "title boom".into(),
            ));
        }
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 首帧延迟 20ms 的 provider:保证 driver 的 interrupt 分支在 stream
/// 首次 yield 之前已被轮询到(用于确定性地测试中断标记)。
struct DelayedProvider;

#[async_trait]
impl yi_agent_core::Provider for DelayedProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            yi_agent_core::provider::ProviderEvent::TextDelta("hi".into())
        })
        .chain(futures::stream::once(async {
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            }
        }));
        Ok(events.boxed())
    }
}

/// 第一次调用产出一段助手文本 + 一个工具调用后正常结束（core 会进入 ACT、
/// 向 driver 发 `ToolCall`，translator 借此把累积文本 finalize 成一个 item）；
/// 之后的调用**永不产出**，于是 turn 一直停在"已 finalize 一块文本、但还没
/// persist"的窗口——正是 checkpoint 该已落盘的窗口。
struct CheckpointProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl yi_agent_core::Provider for CheckpointProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::ProviderEvent as E;
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // 工具入参必须是合法 JSON（`{}`），否则 accumulate 会因解析失败报错。
            let head = futures::stream::iter(vec![
                E::TextDelta("a".into()),
                E::ToolUseStart {
                    id: "t1".into(),
                    name: "noop".into(),
                },
                E::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: "{}".into(),
                },
                E::ToolUseEnd { id: "t1".into() },
                E::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ]);
            Ok(head.boxed())
        } else {
            // 永不产出的尾部：保证 turn 不收尾（否则会被正常 persist）。
            Ok(futures::stream::pending::<E>().boxed())
        }
    }
}

/// 测试用的单客户端 hub:注册 `local` 并起 `pump_stdout`,与生产的 stdio 接线同款。
fn test_hub<W>(
    writer: W,
) -> (
    Arc<crate::broadcast::Broadcaster>,
    crate::broadcast::ClientId,
)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    // 与生产的 stdio 接线同款:可靠登记,背压不摘除(见 `serve_scoped`)。
    let outbound = hub.register_reliable(client.clone());
    tokio::spawn(pump_stdout(
        outbound,
        writer,
        Arc::clone(&hub),
        client.clone(),
    ));
    (hub, client)
}

pub(crate) fn build_test_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    // 让本 mock 如实汇报「它实际会跑的模型」:与 `crate::test_support::test_config()`
    // 的 `cfg.model` 同源(`TEST_MODEL`)。`thread/start`/`thread/resume` 现在以
    // `agent.config().model` 为上报来源(任务 7 评审 Finding 1),若这里沿用
    // `AgentConfig::default()` 的默认串,就会让夹具与生产语义(会话按 cfg 的模型
    // 起跑)脱节。
    let config = yi_agent_core::AgentConfig {
        model: crate::test_support::TEST_MODEL.to_string(),
        ..yi_agent_core::AgentConfig::default()
    };
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

/// 用指定 provider 构造 agent，其余与 `build_test_agent` 一致。
///
/// 标题生成测试需要"同一个 provider 既服务主对话、又服务标题请求"，故工厂必须
/// 能注入 provider——`build_test_agent` 把 `MockProvider` 写死在体内。
fn build_test_agent_with_provider(
    provider: Arc<dyn yi_agent_core::Provider>,
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let config = yi_agent_core::AgentConfig {
        model: crate::test_support::TEST_MODEL.to_string(),
        ..yi_agent_core::AgentConfig::default()
    };
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

pub(crate) fn build_slow_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(SlowProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

fn build_delayed_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(DelayedProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

fn build_checkpoint_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CheckpointProvider {
        calls: AtomicUsize::new(0),
    });
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

/// 测试用的主题句柄：不落盘的临时 workdir，也不可被生产路径读到。
fn test_theme() -> crate::theme_tool::ThemeHandle {
    crate::theme_tool::ThemeHandle::new(std::env::temp_dir())
}

pub(crate) fn test_config() -> RuntimeConfig {
    crate::test_support::test_config()
}

/// 与 `test_config` 同值，供 `board_rpc_tests` 构造自定义 config 的 harness。
pub(crate) fn default_config() -> RuntimeConfig {
    crate::test_support::test_config()
}

/// 用两条 `duplex` 管道把 server 与测试客户端对接。
pub(crate) struct Harness {
    client_w: tokio::io::DuplexStream,
    client_r: BufReader<tokio::io::DuplexStream>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// 隔离的全局目录索引;持有它保证 tempdir 存活到 harness 结束。
    _index_dir: tempfile::TempDir,
    /// 隔离的看板登记表目录,理由同上:board/* RPC 不得写真的 `~/.yi-agent`。
    pub(crate) board_dir: tempfile::TempDir,
    /// 隔离的模型清单目录,理由同 `board_dir`:`model/*` RPC 不得写开发者
    /// 真实的 `~/.yi-agent/models.json`。字段仅用于持有 tempdir。
    pub(crate) _models_dir: tempfile::TempDir,
    /// 隔离的常驻登记目录,理由同 `board_dir`:board/create|remove 不得写
    /// 真的 `$HOME/.yi-agent`;驱动器测试还要往里注册插件,故 `pub(crate)`。
    pub(crate) resident_dir: tempfile::TempDir,
    /// 与主循环**共享**的配对状态:测试经 [`Harness::pairing`] 直接 `redeem`,
    /// 所得设备必须能被同一主循环的 `device/list` 看见。
    pairing: Arc<PairingState>,
    /// 与主循环**共享**的扇出中心:测试经 [`Harness::hub`] 断言订阅状态。
    hub: Arc<crate::broadcast::Broadcaster>,
    /// 注入的 watchman 调用记录器:`(是否安装, 使用的 home)`，与
    /// `launcher` 注入同一个理由——测试绝不能真的调 `launchctl`，也不能
    /// 碰用户真实的 `~/Library/LaunchAgents`。
    pub(crate) watchman_calls: WatchmanCalls,
    /// 注入的 watchman home（tempdir），仅用于持有它。
    pub(crate) _watchman_home: tempfile::TempDir,
    /// 隔离的「家目录」：`skill/list` / `skill/read` 从这里派生三层根，绝不读
    /// 开发者真实的 `~/.yi-agent/skills`。字段同时用于持有 tempdir。
    pub(crate) skills_home_dir: tempfile::TempDir,
    /// 与主循环**共享**的组长信箱:测试据此直接投递,验证主循环真的把它
    /// 转成了组长 thread 的一次输入。
    pub(crate) lead_inbox: crate::lead::LeadInbox,
    /// 与主循环**共享**的回信登记表:测试据此扮演等待方(`AskLeadTool`
    /// 的 `blocking: true`)并断言主循环真的把组长那轮的文本回填了回来。
    pub(crate) lead_replies: crate::lead::LeadReplies,
}

/// 记录到的 watchman 调用:安装为 `true`、卸载为 `false`，附所针对的 home。
pub(crate) type WatchmanCalls = Arc<StdMutex<Vec<(bool, PathBuf)>>>;

impl Harness {
    pub(crate) fn new() -> Self {
        Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)
    }

    /// 用自定义 config 搭建 harness（持久化测试需要自定义 workdir）。
    pub(crate) async fn with_cfg(cfg: RuntimeConfig) -> Self {
        Self::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT)
    }

    /// 用自定义 agent 工厂搭建 harness(慢 provider / 中断 / 权限测试需要)。
    fn with_factory<F>(build: F, permission_timeout: Duration) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_config(test_config(), build, permission_timeout)
    }

    /// 用自定义 config + agent 工厂搭建 harness(持久化测试需要自定义 workdir)。
    ///
    /// 生产 stdio 恒为 `Scope::Admin`,故此处固定传 `Admin`;要驱动门禁分支
    /// 用 [`Harness::with_scope`]。
    fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_config_and_scope(cfg, build, permission_timeout, Scope::Admin)
    }

    /// 把 `local` 客户端注册为给定 `scope` 的 harness。
    ///
    /// 与 [`Harness::new`] 唯一的不同是这对 `serve_scoped` 的 `client_scope`
    /// 实参;生产路径(`serve_stdio`)恒为 `Admin`,故它只服务门禁测试。
    pub(crate) async fn with_scope(scope: Scope) -> Self {
        Self::with_config_and_scope(test_config(), build_test_agent, PERMISSION_TIMEOUT, scope)
    }

    /// 用注入的 watchman 安装/卸载行为搭建 harness。
    ///
    /// 默认 harness 的记录器只记账;要驱动「失败要回传 warning」这类分支,
    /// 需要一个真的返回 `Err` 的注入点。
    pub(crate) async fn with_watchman(
        cfg: RuntimeConfig,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
    ) -> Self {
        Self::with_config_and_scope_and_watchman(
            cfg,
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            install,
            uninstall,
            Arc::new(StdMutex::new(Vec::new())),
        )
    }

    fn with_config_and_scope<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        // 默认注入:只记录调用,不碰真实 launchd / home。
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman(
            cfg,
            build,
            permission_timeout,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
        )
    }

    fn with_config_and_scope_and_watchman<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
        calls: WatchmanCalls,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let board_dir = tempfile::TempDir::new().unwrap();
        // 常驻登记也落在隔离目录里:`board/create` 不得写真的 `~/.yi-agent`。
        let resident_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        // 配对状态也落在隔离目录里:`device/list` 从空表起步,且绝不碰用户
        // 真实的 `~/.yi-agent/devices.json`。
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        // 与主循环共享同一个扇出中心,便于测试直接断言订阅状态
        // (`thread/subscribe` 的效果)。
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        // 主题句柄与 `cfg.workdir` 一致：`ui/settings/read` 从 `cfg.workdir`
        // 读、`write` 经句柄落盘，两者不同则会各看各的。
        let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
        // 值守的 home 也落在隔离目录里:安装/卸载经调用方注入的闭包,绝不
        // 触碰真实的 `~/Library/LaunchAgents`。
        let watchman_home = tempfile::TempDir::new().unwrap();
        // skill 根同样落在隔离目录里:`skill/list` 不得枚举开发者真实的
        // `~/.yi-agent/skills`。
        let skills_home = tempfile::TempDir::new().unwrap();
        // 模型清单也落在隔离目录里:`model/*` 不得写开发者真实的
        // `~/.yi-agent/models.json`(里面是明文密钥)。
        let models_dir = tempfile::TempDir::new().unwrap();
        // 组长信箱:先造一份,clone 进 `serve`,原件留在 `Harness` 上。
        let lead_inbox = crate::lead::LeadInbox::new();
        // 回信登记表:同一手法(与主循环共享同一实例)。测试据此断言
        // 「等待方登记的名字被主循环真的回填了」——自造第二份会得到一个
        // 永远等不到回信的登记表。
        let lead_replies = crate::lead::LeadReplies::new();
        let handle = tokio::spawn(serve_scoped(
            server_r,
            server_w,
            cfg,
            permission_timeout,
            workspaces,
            Arc::clone(&pairing),
            Arc::clone(&hub),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: board_dir.path().to_path_buf(),
                models_path: models_dir.path().join("models.json"),
                // 引导的 .env / preferences 也落在隔离目录里:onboarding/*
                // 不得写开发者真实的 `~/.yi-agent`。
                onboarding_env_path: index_dir.path().join(".env"),
                onboarding_preferences_path: index_dir.path().join("preferences.json"),
                // 测试里不起真进程:`board/create` 走注入的启动器,
                // 与 board_dir 注入同一个理由。
                resident_dir: resident_dir.path().to_path_buf(),
                skills_home: skills_home.path().to_path_buf(),
                plugin_command: PathBuf::from("/nonexistent/superpowers-kanban"),
                launcher: Arc::new(|_project: &Path| Ok(true)),
                theme,
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                watchman_install: install,
                watchman_uninstall: uninstall,
                watchman_home: watchman_home.path().to_path_buf(),
                // 与主循环**共享**同一份信箱(clone 共用同一个 broadcast
                // sender):测试经 `lead_inbox` 直接投递的消息必须被主循环
                // 收到,自造第二份会投进没人听的通道。
                lead_inbox: lead_inbox.clone(),
                lead_replies: lead_replies.clone(),
            },
            build,
            scope,
        ));
        Self {
            client_w,
            client_r: BufReader::new(client_r),
            handle,
            _index_dir: index_dir,
            board_dir,
            _models_dir: models_dir,
            resident_dir,
            pairing,
            hub,
            watchman_calls: calls,
            _watchman_home: watchman_home,
            skills_home_dir: skills_home,
            lead_inbox,
            lead_replies,
        }
    }

    /// 与主循环共享的扇出中心句柄:测试据此断言 `thread/subscribe` 是否
    /// 真的把订阅写进了服务端的 `Broadcaster`。
    pub(crate) fn hub(&self) -> Arc<crate::broadcast::Broadcaster> {
        Arc::clone(&self.hub)
    }

    /// 测试直接驱动的配对状态句柄,与主循环共享同一个 `Arc`:这里 `redeem`
    /// 出的设备正是 `device/list` 会读到的那一条。
    /// 隔离 home 的路径:测试据此播种 skill 与断言路径包含关系。
    pub(crate) fn skills_home(&self) -> std::path::PathBuf {
        self.skills_home_dir.path().to_path_buf()
    }

    pub(crate) fn pairing(&self) -> Arc<PairingState> {
        Arc::clone(&self.pairing)
    }

    pub(crate) async fn send(&mut self, line: &str) {
        self.client_w.write_all(line.as_bytes()).await.unwrap();
        self.client_w.write_all(b"\n").await.unwrap();
        self.client_w.flush().await.unwrap();
    }

    /// 测试专用:直接往该 harness 的组长信箱投递,不经过真实模型。
    ///
    /// 需要一个「与主循环共享的信箱句柄」——即 `Harness::lead_inbox`,它与
    /// 交给 `serve` 的是同一个 broadcast sender(克隆共享),故这里投递的消息
    /// 会被主循环的那条 `select!` 分支收到并转成组长 thread 的一次输入。
    ///
    /// `project_key` 由调用方给出的隔离 workdir 求得,不从 `Harness` 反查:
    /// 测试的 workdir 由它自己的 `TempDir` 持有(见 `isolated_harness`)。
    pub(crate) fn deliver_lead_message_for_test(
        h: &Harness,
        workdir: &std::path::Path,
        from_thread_id: &str,
        text: &str,
    ) {
        h.lead_inbox
            .deliver(crate::lead::LeadEnvelope {
                project_key: crate::thread_store::ThreadStore::new(workdir).project_key(),
                from_thread_id: from_thread_id.to_string(),
                text: text.to_string(),
                request_id: None,
            })
            .expect("deliver");
    }

    pub(crate) async fn read_value(&mut self) -> serde_json::Value {
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, self.client_r.read_line(&mut buf))
            .await
            .expect("timed out waiting for a message")
            .expect("read_line failed");
        assert!(n > 0, "unexpected EOF while waiting for a message");
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
    }

    /// 读一帧，超时（或无帧）返回 `None`。用于断言「此后不该再有帧」。
    pub(crate) async fn try_read_value(&mut self, wait: Duration) -> Option<serde_json::Value> {
        let mut buf = String::new();
        match tokio::time::timeout(wait, self.client_r.read_line(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => serde_json::from_str(buf.trim()).ok(),
            _ => None,
        }
    }

    /// 读到 `turn/completed` 为止（最多 40 帧），返回那一帧。
    pub(crate) async fn read_until_turn_completed(&mut self) -> serde_json::Value {
        for _ in 0..40 {
            let v = self.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                return v;
            }
        }
        panic!("turn/completed never arrived");
    }

    /// 关掉客户端写端(触发 EOF)并等待 server 任务结束。
    pub(crate) async fn shutdown(self) {
        let Harness {
            client_w, handle, ..
        } = self;
        drop(client_w);
        let res = tokio::time::timeout(EVENT_WAIT, handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(res.is_ok(), "serve_stdio should return Ok on EOF: {res:?}");
    }
}

pub(crate) async fn initialize(h: &mut Harness) {
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 1, "initialize must respond with the request id");
}

/// `initialize` + `thread/start`,返回新建 thread 的 id。
async fn start_thread(h: &mut Harness) -> String {
    initialize(h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    // 依次读到 thread/started 通知与 thread/start 响应,取响应里的 thread_id。
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            return v["result"]["thread_id"].as_str().unwrap().to_string();
        }
    }
    panic!("no thread/start response");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_in_flight_turn_leaves_a_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"do work"}}]}}}}"#
    ))
    .await;

    // 等到**助手 item** 的 item/completed 出现 —— 说明这一块文本已 finalize、
    // checkpoint 已被标脏。注意不能只等第一个 item/completed：turn 开启时发
    // 的用户 item 也是 item/completed，只等"第一个"会在助手文本 finalize 之前
    // 就 break，断言便退化成只校验 turn-start 那次写入（那时 flush 分支从未跑过）。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
            && v["params"]["item"]["type"] == serde_json::json!("agentMessage")
        {
            break;
        }
    }

    let partial = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.partial.json"));
    // checkpoint 是去抖写的，轮询等待。这里只接受"已 finalize 的助手 item 也
    // 在盘上"的版本：turn-start 那次写入只有用户 item（items 长度 1），
    // 助手 item 只能由 500ms tick 的 flush 分支落盘。
    let mut saved: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(t) = std::fs::read_to_string(&partial) {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["items"].as_array().map(|a| a.len() >= 2).unwrap_or(false) {
                saved = Some(v);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let saved = saved.expect(
        "checkpoint must carry the finalized assistant item; only the debounced flush can write it",
    );
    let items = saved["items"].as_array().unwrap();
    // turn-start 那次写入只有用户 item（长度 1）；助手 item 只能由 flush 落盘。
    assert!(
        items.len() >= 2,
        "checkpoint must carry the finalized assistant item (turn-start write has only the user item): {items:?}"
    );
    assert_eq!(
        items[0]["type"],
        serde_json::json!("userMessage"),
        "checkpoint must carry the opening user item: {items:?}"
    );
    assert_eq!(
        items[0]["text"],
        serde_json::json!("do work"),
        "checkpoint must carry the prompt: {items:?}"
    );
    let assistant = items
        .iter()
        .find(|i| i["type"] == serde_json::json!("agentMessage"))
        .unwrap_or_else(|| {
            panic!("checkpoint must carry the finalized assistant item; only the debounced flush can write it: {items:?}")
        });
    assert_eq!(
        assistant["text"],
        serde_json::json!("a"),
        "checkpoint must carry the finalized assistant text: {items:?}"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_turn_removes_its_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }
    let partial = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.partial.json"));
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if !partial.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !partial.exists(),
        "a finished turn must not leave a checkpoint"
    );
    h.shutdown().await;
}

/// 读到 id 匹配的那一帧响应,丢弃中间穿插的通知(如 `process/updated`)。
///
/// Task 5 之后,进程状态守望者会随时把 `process/updated` 插进同一路流里,
/// 请求响应不再保证是紧接着的下一帧,故按 id 收敛而非"读一帧就当响应"。
async fn read_response(h: &mut Harness, want: u64) -> serde_json::Value {
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(want)) {
            return v;
        }
    }
    panic!("no response with id {want}");
}

/// 把一帧回放通知展平为其携带的 items：**兼容**逐条 `item/completed` 与批量
/// `items/completed`；非 item 帧返回空 vec。既有 resume 测试都用它改造。
fn replayed_items_of(v: &serde_json::Value) -> Vec<crate::protocol::Item> {
    match v.get("method").and_then(|m| m.as_str()) {
        Some("item/completed") => {
            vec![serde_json::from_value(v["params"]["item"].clone()).unwrap()]
        }
        Some("items/completed") => v["params"]["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|it| serde_json::from_value(it.clone()).unwrap())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 读到 id==want 的响应为止,收集期间所有回放 item（经 `replayed_items_of`）。
async fn collect_replayed_items(h: &mut Harness, want: u64) -> Vec<crate::protocol::Item> {
    let mut out = Vec::new();
    for _ in 0..4096 {
        let v = h.read_value().await;
        out.extend(replayed_items_of(&v));
        if v.get("id") == Some(&serde_json::json!(want)) {
            break;
        }
    }
    out
}

/// 带总时限地读到「指定 method 的通知」,丢弃中间帧(其它通知、请求响应)。
///
/// `process/updated` 由守望者异步推来,不由任何请求直接触发,故不能靠「读下一帧」;
/// 只能按 method 收敛。逐帧读另设较短上限,以免没有帧时 `read_value` 内部 5s 超时
/// 先 panic(那会让失败信息含混);总时限到则返回 `None`,由调用方给出明确断言。
async fn await_notification(
    h: &mut Harness,
    method: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let slice = remaining.min(Duration::from_millis(1000));
        match tokio::time::timeout(slice, h.read_value()).await {
            Ok(v) => {
                if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                    return Some(v);
                }
            }
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
            }
        }
    }
}

fn summary(
    task_id: &str,
    state: &str,
    thread_id: Option<&str>,
) -> yi_agent_store::ipc::IpcTaskSummary {
    yi_agent_store::ipc::IpcTaskSummary {
        task_id: task_id.into(),
        state: state.into(),
        is_root: false,
        parent_task_id: Some("root".into()),
        thread_id: thread_id.map(str::to_string),
    }
}

/// A conversation sees only the children it spawned.
///
/// One directory can host several conversations sharing one root, so the
/// marker bound at assembly time is the only thing that separates them. An
/// untagged task belongs to no conversation rather than to all of them.
#[test]
fn children_are_scoped_by_the_conversation_marker() {
    let tasks = vec![
        summary("mine-1", "running", Some("thread-a")),
        summary("theirs", "running", Some("thread-b")),
        summary("untagged", "running", None),
        summary("mine-2", "completed", Some("thread-a")),
    ];
    let mine = children_for_thread(&tasks, "thread-a");
    assert_eq!(
        mine.iter().map(|c| c.task_id.as_str()).collect::<Vec<_>>(),
        vec!["mine-1", "mine-2"],
        "only this conversation's children, in order"
    );
    assert!(
        children_for_thread(&tasks, "thread-b")
            .iter()
            .all(|c| c.task_id == "theirs")
    );
    assert!(
        children_for_thread(&tasks, "thread-c").is_empty(),
        "a conversation with no children lists none"
    );
}

/// A root task is never a child, even when it carries the marker.
#[test]
fn the_root_is_never_listed_as_a_child() {
    let mut root = summary("root", "running", Some("thread-a"));
    root.is_root = true;
    let tasks = vec![root, summary("child", "running", Some("thread-a"))];
    let children = children_for_thread(&tasks, "thread-a");
    assert_eq!(
        children
            .iter()
            .map(|c| c.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["child"]
    );
}

/// A finished child stays in the list; the state is what marks it finished.
#[test]
fn a_finished_child_stays_in_the_list() {
    let tasks = vec![summary("done", "completed", Some("thread-a"))];
    let children = children_for_thread(&tasks, "thread-a");
    assert_eq!(children.len(), 1);
    assert!(
        is_terminal_state(&children[0].state),
        "the client marks it done"
    );
}

/// The notifications and rows carry the method names and camelCase fields
/// the desktop client keys on.
#[test]
fn the_agent_notifications_use_their_documented_wire_shape() {
    let children = Notification::AgentChildrenUpdated {
        thread_id: "thread-1".into(),
        children: vec![crate::protocol::AgentChild {
            task_id: "task-1".into(),
            objective: Some("do the thing".into()),
            state: "running".into(),
            last_step: Some("running tests".into()),
            parent_task_id: Some("root".into()),
        }],
    };
    let json = serde_json::to_value(&children).unwrap();
    assert_eq!(json["method"], "agent/children/updated");
    assert_eq!(json["params"]["threadId"], "thread-1");
    assert_eq!(json["params"]["children"][0]["taskId"], "task-1");
    assert_eq!(json["params"]["children"][0]["lastStep"], "running tests");

    let event = Notification::AgentTraceEvent {
        thread_id: "thread-1".into(),
        task_id: "task-1".into(),
        row: crate::protocol::AgentTraceRow {
            event_id: 7,
            task_id: "task-1".into(),
            kind: "assistant_text".into(),
            payload_json: "{\"type\":\"assistant_text\",\"text\":\"hi\"}".into(),
        },
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["method"], "agent/trace/event");
    assert_eq!(json["params"]["taskId"], "task-1");
    assert_eq!(json["params"]["row"]["eventId"], 7);
    assert_eq!(
        json["params"]["row"]["payloadJson"],
        "{\"type\":\"assistant_text\",\"text\":\"hi\"}"
    );
}

/// The list degrades to empty rather than failing when the conversation has
/// no runtime, so the desktop rail always renders and the turn keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn agent_children_list_is_empty_without_an_attached_runtime() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"agent/children/list","params":{{"threadId":"{thread_id}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert!(
        v.get("error").is_none(),
        "an unattached conversation answers with an empty list, not an error: {v}"
    );
    assert_eq!(v["result"]["children"], serde_json::json!([]));
    h.shutdown().await;
}

/// An unknown conversation is a caller error, not an empty list.
#[tokio::test(flavor = "multi_thread")]
async fn agent_methods_reject_an_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"agent/children/list","params":{"threadId":"thread-nope"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32011, "unknown thread: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"agent/trace/read","params":{"threadId":"thread-nope","taskId":"t"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 10);
    assert_eq!(v["error"]["code"], -32011, "unknown thread: {v}");
    h.shutdown().await;
}

/// `agent/trace/read` without a task id is a parameter error.
#[tokio::test(flavor = "multi_thread")]
async fn agent_trace_read_requires_a_task_id() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"agent/trace/read","params":{{"threadId":"{thread_id}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32602, "missing taskId: {v}");
    h.shutdown().await;
}

/// Cancelling without a preview token is refused, so the confirmation step
/// cannot be skipped by a client that calls `agent/cancel` directly.
#[tokio::test(flavor = "multi_thread")]
async fn agent_cancel_requires_a_confirmation_token() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"agent/cancel","params":{{"threadId":"{thread_id}","taskId":"task-1"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32602, "no token, no cancel: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_diff_read_reports_base_and_files() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    // init_git_repo 只建空提交；补一个文件再改它，制造「已提交 + 未提交」两段改动。
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "add a"]] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap(); // 未提交改动

    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"thread/diff/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    // thread/start 之后流里可能还夹着通知（如 process/updated），故读到 id 7 为止。
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(7)) {
        v = h.read_value().await;
    }
    assert_eq!(v["result"]["files"][0]["path"], "a.txt");
    assert!(v["result"]["unifiedDiff"].as_str().unwrap().contains("+y"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_diff_read_rejects_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":8,"method":"thread/diff/read","params":{"threadId":"thread-nope"}}"#)
        .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(8)) {
        v = h.read_value().await;
    }
    assert_eq!(
        v["error"]["code"], -32011,
        "unknown thread uses the shared code"
    );
    h.shutdown().await;
}

/// A message to an unattached conversation fails loudly rather than
/// pretending to queue: there is no daemon to queue it on.
#[tokio::test(flavor = "multi_thread")]
async fn agent_message_without_a_runtime_is_an_error() {
    // "Unattached" has to be arranged, not assumed: bring-up provisions the
    // runtime at `<workdir>/.yi-agent/runtime`, and a freshly made directory
    // is perfectly attachable. The shared `/tmp/yi-agent-app-server-test`
    // workdir made this test pass only by accident -- the attach key and the
    // lookup key used to disagree for a directory that did not exist yet
    // (`/tmp` versus `/private/tmp`), so the entry missed and the thread read
    // as unattached. `project_key` now keys both the same way, so that
    // accident is gone and any attachable workdir would reach a real daemon
    // and answer `-32603` instead of the `-32011` asserted here.
    //
    // Occupy `<workdir>/.yi-agent` with a regular file instead: the runtime
    // directory can never be created, bring-up fails deterministically, and
    // the conversation is unattached for the reason this test is about --
    // independent of `/tmp` state or of any daemon running elsewhere.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join(".yi-agent"), b"not a directory").unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let thread_id = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"agent/message","params":{{"threadId":"{thread_id}","taskId":"task-1","message":"hello"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32011, "unattached conversation: {v}");
    h.shutdown().await;
}

/// A malformed message request is a parameter error.
#[tokio::test(flavor = "multi_thread")]
async fn agent_message_requires_a_task_and_text() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"agent/message","params":{{"threadId":"{thread_id}","taskId":"task-1"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32602, "missing message: {v}");
    h.shutdown().await;
}

/// Unwatching a conversation that is not watching anything is a no-op, so a
/// client closing a detail twice is not an error.
#[tokio::test(flavor = "multi_thread")]
async fn agent_trace_unwatch_is_idempotent() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;
    for id in [9, 10] {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"agent/trace/unwatch","params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], id);
        assert_eq!(v["result"]["stopped"], true, "unwatch always succeeds: {v}");
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_responds_with_server_info() {
    let mut h = Harness::new();
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 1);
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
    assert_eq!(v["result"]["protocolVersion"], 1);
    assert!(
        v.get("error").is_none(),
        "success response must omit `error`: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn request_before_initialize_returns_not_initialized() {
    let mut h = Harness::new();
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"config/read","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 1);
    assert_eq!(v["error"]["code"], -32010);
    assert!(
        v.get("result").is_none(),
        "error response must omit `result`: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_returns_method_not_found() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"bogus","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["error"]["code"], -32601);

    // 每个域模块实现了的方法都必须路由到它自己的域。这是 `domain_for` 那张
    // 手维护表的回归网:漏掉一个前缀不会编译报错、也不会让任何测试失败,只会
    // 把一个**本来能工作**的 RPC 静默降级成 `method_not_found`(`device/*`
    // 在重构期间就是这么坏的)。共 69 行,覆盖除 `initialize`(由 `serve()`
    // 自己处理)之外的全部域方法。
    for (method, expected) in [
        // ---- Ui(`ui/*`,外加并入的 `config/read`) ----
        ("config/read", Domain::Ui),
        ("ui/settings/read", Domain::Ui),
        ("ui/settings/write", Domain::Ui),
        // ---- Model ----
        ("model/list", Domain::Model),
        ("model/test", Domain::Model),
        ("model/upsert", Domain::Model),
        ("model/delete", Domain::Model),
        ("model/setDefault", Domain::Model),
        ("model/setSubagent", Domain::Model),
        ("model/importEnv", Domain::Model),
        // ---- Onboarding ----
        ("onboarding/apply", Domain::Onboarding),
        ("onboarding/dismiss", Domain::Onboarding),
        ("onboarding/status", Domain::Onboarding),
        ("onboarding/test", Domain::Onboarding),
        // ---- Board ----
        ("board/create", Domain::Board),
        ("board/list", Domain::Board),
        ("board/remove", Domain::Board),
        // ---- Plugin(`plugin/*` 与 `plugins/*`) ----
        ("plugin/query", Domain::Plugin),
        ("plugin/settings/read", Domain::Plugin),
        ("plugin/settings/write", Domain::Plugin),
        ("plugins/list", Domain::Plugin),
        // ---- Image ----
        ("image/read", Domain::Image),
        ("image/upload/abort", Domain::Image),
        ("image/upload/begin", Domain::Image),
        ("image/upload/chunk", Domain::Image),
        ("image/upload/commit", Domain::Image),
        // ---- Workspace ----
        ("workspace/add", Domain::Workspace),
        ("workspace/link", Domain::Workspace),
        ("workspace/list", Domain::Workspace),
        ("workspace/remove", Domain::Workspace),
        ("workspace/unlink", Domain::Workspace),
        // ---- Process ----
        ("process/kill", Domain::Process),
        ("process/list", Domain::Process),
        ("process/read", Domain::Process),
        // ---- Pair(`pair/*` 与 `device/*` 共享同一 `PairingState`) ----
        ("pair/create", Domain::Pair),
        ("pair/redeem", Domain::Pair),
        ("device/list", Domain::Pair),
        ("device/revoke", Domain::Pair),
        // ---- Turn ----
        ("turn/interject", Domain::Turn),
        ("turn/interrupt", Domain::Turn),
        ("turn/start", Domain::Turn),
        // ---- Skill(技能面板:三层技能根) ----
        ("skill/list", Domain::Skill),
        ("skill/read", Domain::Skill),
        // ---- Thread ----
        ("thread/clear", Domain::Thread),
        ("thread/compact", Domain::Thread),
        ("thread/delete", Domain::Thread),
        ("thread/diff/read", Domain::Thread),
        ("thread/lead", Domain::Thread),
        ("thread/list", Domain::Thread),
        ("thread/listAll", Domain::Thread),
        ("thread/readItems", Domain::Thread),
        ("thread/rename", Domain::Thread),
        ("thread/reorderPinned", Domain::Thread),
        ("thread/resume", Domain::Thread),
        ("thread/setModel", Domain::Thread),
        ("thread/setPermissionMode", Domain::Thread),
        ("thread/setPinned", Domain::Thread),
        ("thread/setPreserveThinking", Domain::Thread),
        ("thread/setReasoningEffort", Domain::Thread),
        ("thread/setTag", Domain::Thread),
        ("thread/start", Domain::Thread),
        ("thread/subscribe", Domain::Thread),
        // ---- Agent ----
        ("agent/cancel", Domain::Agent),
        ("agent/cancel/preview", Domain::Agent),
        ("agent/children/list", Domain::Agent),
        ("agent/message", Domain::Agent),
        ("agent/trace/read", Domain::Agent),
        ("agent/trace/unwatch", Domain::Agent),
        ("agent/trace/watch", Domain::Agent),
    ] {
        assert_eq!(
            domain_for(method),
            expected,
            "`{method}` must route to its own domain (an unmapped prefix silently becomes method_not_found)"
        );
    }

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_start_emits_thread_started_and_responds() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;

    let mut notif = None;
    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("thread/started") {
            notif = Some(v);
        } else if v.get("id") == Some(&serde_json::json!(2)) {
            resp = Some(v);
        }
        if notif.is_some() && resp.is_some() {
            break;
        }
    }

    let notif = notif.expect("expected a thread/started notification");
    let resp = resp.expect("expected a thread/start response");
    let notif_thread_id = notif["params"]["thread_id"]
        .as_str()
        .expect("notification thread_id must be a string");
    assert!(!notif_thread_id.is_empty(), "thread_id must be non-empty");
    assert_eq!(notif["params"]["model"], "test-model");
    assert_eq!(resp["result"]["thread_id"], notif_thread_id);
    assert_eq!(resp["result"]["model"], "test-model");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_read_returns_redacted_view() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["api_key"], "");
    assert_eq!(v["result"]["model"], "test-model");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_add_list_remove_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&add.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(
        v["result"]["workspaces"][0]["path"],
        cwd.to_string_lossy().to_string()
    );
    assert_eq!(v["result"]["workspaces"][0]["exists"], true);

    let rm = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"workspace/remove",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&rm.to_string()).await;
    let v = h.read_value().await;
    assert!(v.get("result").is_some());

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"workspace/list","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(
        v["result"]["workspaces"].as_array().unwrap().len(),
        0,
        "removed workspace must be gone from the index: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_list_marks_missing_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&add.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

    // 目录在磁盘上消失后,索引仍保留该条目,但 list 必须标记 exists=false。
    std::fs::remove_dir_all(&cwd).unwrap();

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(
        v["result"]["workspaces"][0]["path"],
        cwd.to_string_lossy().to_string()
    );
    assert_eq!(
        v["result"]["workspaces"][0]["exists"], false,
        "missing directory must be flagged as not existing: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_remove_accepts_alternate_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&add.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

    // 用与存储值不同、但 canonicalize 后等价的路径 remove,必须命中同一条目。
    // `dir.path()` 是非 canonical 前缀(如 macOS 的 /var → /private/var),
    // 末尾再缀 `/.`,因此字符串与 canonical 存储值不同、解析后却相同。
    let alternate = dir.path().join(".").to_string_lossy().to_string();
    assert_ne!(alternate, cwd.to_string_lossy().to_string());

    let rm = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"workspace/remove",
        "params":{"path": alternate}});
    h.send(&rm.to_string()).await;
    let v = h.read_value().await;
    assert!(v.get("result").is_some(), "remove must succeed: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"workspace/list","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(
        v["result"]["workspaces"].as_array().unwrap().len(),
        0,
        "alternate path must remove the canonicalized entry: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_add_rejects_non_directory() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"workspace/add","params":{"path":"/nope/nope"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_exits_gracefully() {
    let (client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
    let index_dir = tempfile::TempDir::new().unwrap();
    let workspaces = Arc::new(WorkspaceIndex::new(
        index_dir.path().join("workspaces.json"),
    ));
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        index_dir.path().join("devices.json"),
    )));
    let handle = tokio::spawn(serve_stdio(
        server_r,
        server_w,
        test_config(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes: Arc::new(StdMutex::new(HashMap::new())),
            thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            board_dir: PathBuf::new(),
            models_path: PathBuf::new(),
            onboarding_env_path: PathBuf::new(),
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            plugin_command: PathBuf::new(),
            launcher: Arc::new(|_project: &Path| Ok(true)),
            theme: test_theme(),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            // 测试注入:只记录调用,不碰真实 launchd / home。
            watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
            watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
            watchman_home: PathBuf::new(),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
        },
        build_test_agent,
    ));

    // 什么都不发,直接关掉写端 → server 应看到 EOF 并 Ok(()) 退出。
    drop(client_w);
    let result = tokio::time::timeout(EVENT_WAIT, handle)
        .await
        .expect("server must exit within the timeout")
        .expect("server task must not panic");
    assert!(
        result.is_ok(),
        "serve_stdio should return Ok on EOF: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_returns_parse_error() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send("not json").await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32700, "expected parse error: {v}");
    assert_eq!(v["id"], 0, "malformed frame must be answered with id 0");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blank_lines_are_ignored() {
    let mut h = Harness::new();
    // 先发一个空行,再发合法的 initialize;空行不应产生任何帧。
    h.send("").await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 1, "blank line must not produce a frame: {v}");
    assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_factory_failure_returns_internal_error() {
    let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let index_dir = tempfile::TempDir::new().unwrap();
    let workspaces = Arc::new(WorkspaceIndex::new(
        index_dir.path().join("workspaces.json"),
    ));
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        index_dir.path().join("devices.json"),
    )));
    let handle = tokio::spawn(serve_stdio(
        server_r,
        server_w,
        test_config(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes: Arc::new(StdMutex::new(HashMap::new())),
            thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            board_dir: PathBuf::new(),
            models_path: PathBuf::new(),
            onboarding_env_path: PathBuf::new(),
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            plugin_command: PathBuf::new(),
            launcher: Arc::new(|_project: &Path| Ok(true)),
            theme: test_theme(),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            // 测试注入:只记录调用,不碰真实 launchd / home。
            watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
            watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
            watchman_home: PathBuf::new(),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
        },
        |_s: Option<yi_agent_core::Session>,
         _cwd: &std::path::Path,
         _mode: crate::thread_store::ThreadMode| {
            Err::<BuiltAgent, _>(anyhow::anyhow!("boom"))
        },
    ));

    let mut client_r = BufReader::new(client_r);

    client_w
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    client_w.flush().await.unwrap();
    let mut buf = String::new();
    client_r.read_line(&mut buf).await.unwrap();
    let init: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(init["id"], 1);

    client_w
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"thread/start\",\"params\":{}}\n")
        .await
        .unwrap();
    client_w.flush().await.unwrap();
    let mut buf = String::new();
    client_r.read_line(&mut buf).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(v["id"], 2);
    assert_eq!(
        v["error"]["code"], -32603,
        "factory failure must map to internal error: {v}"
    );

    drop(client_w);
    let res = tokio::time::timeout(EVENT_WAIT, handle)
        .await
        .expect("server must shut down within the timeout")
        .expect("server task must not panic");
    assert!(res.is_ok(), "serve_stdio should return Ok on EOF: {res:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_frame_returns_err() {
    let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
    let index_dir = tempfile::TempDir::new().unwrap();
    let workspaces = Arc::new(WorkspaceIndex::new(
        index_dir.path().join("workspaces.json"),
    ));
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        index_dir.path().join("devices.json"),
    )));
    let handle = tokio::spawn(serve_stdio(
        server_r,
        server_w,
        test_config(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes: Arc::new(StdMutex::new(HashMap::new())),
            thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            board_dir: PathBuf::new(),
            models_path: PathBuf::new(),
            onboarding_env_path: PathBuf::new(),
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            plugin_command: PathBuf::new(),
            launcher: Arc::new(|_project: &Path| Ok(true)),
            theme: test_theme(),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            // 测试注入:只记录调用,不碰真实 launchd / home。
            watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
            watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
            watchman_home: PathBuf::new(),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
        },
        build_test_agent,
    ));

    let mut frame = vec![b'x'; crate::protocol::MAX_FRAME_BYTES + 16];
    frame.push(b'\n');
    // duplex 缓冲小于帧,server 读到超限即报错并退出读端;写侧随后可能
    // 因 broken pipe 失败,这里忽略写错误——真正断言的是 server 的结果。
    let _ = client_w.write_all(&frame).await;
    let _ = client_w.flush().await;

    let result = tokio::time::timeout(EVENT_WAIT, handle)
        .await
        .expect("server must exit within the timeout")
        .expect("server task must not panic");
    assert!(
        result.is_err(),
        "oversized frame must surface an error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_start_emits_full_notification_sequence() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    let mut methods: Vec<String> = Vec::new();
    let mut resp_turn_id: Option<String> = None;
    let mut completed: Option<serde_json::Value> = None;
    let mut running_status: Option<String> = None;
    // 本轮开启项(用户提问)的 item/started 形状。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            methods.push(m.to_string());
            if m == "thread/status/updated" {
                running_status = v["params"]["status"].as_str().map(|s| s.to_string());
            }
            if m == "item/started" && opener.is_none() {
                opener = Some(v["params"]["item"].clone());
            }
            if m == "turn/completed" {
                completed = Some(v);
                break;
            }
        } else if v.get("id") == Some(&serde_json::json!(3)) {
            resp_turn_id = v["result"]["turn_id"].as_str().map(|s| s.to_string());
        }
    }

    assert_eq!(
        methods,
        vec![
            "turn/started",
            // 本轮开启项:用户提问(用户气泡)必须排在 agent 任何 item 之前,
            // 否则实时观看与事后回放的顺序不一致(点开时开场白跑到最底部)。
            "item/started",
            "item/completed",
            // turn/start 在 turn/started 之后立刻推 Running 状态,先于 turn 正文。
            "thread/status/updated",
            "item/started",
            "item/delta",
            // `Done` 会先 finalize 打开的 agentMessage,故 turn/completed
            // 之前必有一条 item/completed(见 Translator::finish_turn)。
            "item/completed",
            "turn/completed"
        ],
        "unexpected notification sequence"
    );
    let opener = opener.expect("the turn must open with an item");
    assert_eq!(
        opener["type"], "userMessage",
        "the first item of a turn must be the user's own message: {opener}"
    );
    assert_eq!(opener["text"], "hi");
    assert_eq!(
        opener["id"],
        format!("user-{}", resp_turn_id.as_deref().unwrap_or_default()),
        "the opener id must match the wire id, so the persisted turn and a \
         live client agree on identity: {opener}"
    );
    assert_eq!(
        running_status.as_deref(),
        Some("running"),
        "the status frame preceding the turn body must report `running`"
    );
    let resp_turn_id = resp_turn_id.expect("turn/start response must carry turn_id");
    assert!(!resp_turn_id.is_empty(), "turn_id must be non-empty");
    let completed = completed.expect("expected a turn/completed notification");
    assert_eq!(completed["params"]["status"], "completed");
    assert_eq!(completed["params"]["turn_id"], resp_turn_id);
    h.shutdown().await;
}

/// 用户从工作区外选中的文件必须被**复制**进 thread 的 cwd，元数据回显在开启项
/// 上；气泡正文仍是用户原话，注入的清单只出现在送给模型的 prompt 里。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_copies_attachments_and_echoes_them_on_the_item() {
    // 用自带 workdir 的 harness：默认 `/tmp` 目录会跨次运行残留同名附件，
    // 那样「盘上存在」就可能被旧文件蒙混过关。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 源文件放在工作区外，模拟「从 Downloads 选文件」。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"总结一下"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 本轮第一条 item/started 就是开启项:用户消息(附 attachment 元数据)。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["attachments"][0]["name"], "报告.pdf", "{item}");
    let rel = item["attachments"][0]["path"]
        .as_str()
        .expect("an attachment path must be a string");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    // 磁盘上确实复制成功（相对 thread 的 cwd = 该 harness 的 workdir）。
    assert!(
        workdir.path().join(rel).is_file(),
        "attachment was not copied: {rel}"
    );
    // 气泡正文是用户原话，不含注入的清单。
    assert_eq!(item["text"], "总结一下", "{item}");
    h.shutdown().await;
}

/// 反向断言：清单必须真的送进**模型**。只断言 `Item.text` 不含清单一事
/// 无法防住「将来把 `display_text` 传给 `agent.run`」这类静默回归。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_prompt_reaching_the_model_contains_the_attachment_manifest() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let prompts: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let prompts_factory = Arc::clone(&prompts);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingProvider {
            prompts: Arc::clone(&prompts_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"总结一下"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 等到 turn 收尾，确保 provider 至少被调用过一次。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if opener.is_none()
            && v.get("method").and_then(|m| m.as_str()) == Some("item/started")
            && v["params"]["item"]["type"] == "userMessage"
        {
            opener = Some(v["params"]["item"].clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // 送给模型的那条 user 文本必须带清单（安全化 + 加引号后的形态）。
    let seen = prompts.lock().unwrap().clone();
    let sent = seen
        .last()
        .expect("the provider must have been called at least once");
    assert!(
        sent.contains("Attached files (read them with read_document):"),
        "the manifest header never reached the model: {sent:?}"
    );
    assert!(
        sent.contains("- \"报告.pdf\" (application/pdf, 13 bytes) -> .yi-agent/attachments/"),
        "the sanitized, quoted manifest line is missing from the model prompt: {sent:?}"
    );
    assert!(
        sent.starts_with("总结一下"),
        "the model prompt must still lead with the user's words: {sent:?}"
    );

    // 气泡/Item.text 仍只是用户原话——清单不进 UI 文本。
    let item = opener.expect("the turn must open with a user item");
    assert_eq!(item["text"], "总结一下", "{item}");
    h.shutdown().await;
}

/// 桌面端从「图片」选择器选中的图（工作区外的绝对路径）必须被复制进 thread
/// 的 cwd，元数据以 `ImageRef` 回显在开启项上；气泡正文仍是用户原话，
/// 图片**不进**附件清单（它是内容块，不是文档附件）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_an_image_injects_an_image_block_and_an_item_ref() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一张 8x8 PNG，放在工作区外（模拟从「图片」选择器选）。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("shot.png");
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([1, 2, 3]));
    img.save_with_format(&src, image::ImageFormat::Png).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"这是什么"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "这是什么", "{item}");
    let rel = item["images"][0]["path"].as_str().expect("image path");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    assert!(
        workdir.path().join(rel).is_file(),
        "image not copied: {rel}"
    );
    assert_eq!(item["images"][0]["media_type"], "image/png", "{item}");
    assert_eq!(item["images"][0]["detail"], "high", "{item}");
    // 气泡文本里不得注入清单。
    assert!(
        !item["text"].as_str().unwrap().contains("Attached files"),
        "{item}"
    );

    // 落盘回合里也必须带上 images 引用：resume 回放时气泡要能重新渲染出图。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["type"], "userMessage", "{line}");
    assert_eq!(opened["images"][0]["path"], rel, "{line}");
    assert_eq!(opened["images"][0]["size"], src.metadata().unwrap().len());
    h.shutdown().await;
}

/// 回归：`images[].size` 必须取**存盘文件**的大小，而不是重编码后载荷的长度。
///
/// 上面那条测试用的是 8x8 直通 PNG：`prepare_image_file` 原样返回源字节，
/// 于是「载荷长度」与「文件大小」恰好相等，即使实现数错了也照样通过。
/// 真正会暴露问题的是**被重编码**的图：这里用一张宽 4096px 的 PNG，`High`
/// 档（最长边 2048）必然触发降采样重编码，载荷与文件大小不再相等。
///
/// 同时断言落盘的 `.jsonl` 开启项与实时 `item/started` 开启项**是同一个数**：
/// 两者若各算各的（一个读文件、一个量载荷）就会分叉，这正是被修复的缺陷。
#[tokio::test(flavor = "multi_thread")]
async fn a_re_encoded_image_persists_the_stored_file_size() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 宽 4096、高 64 的 PNG：最长边超 2048，`High` 档必降采样重编码；
    // 且整幅尺寸很小，编码后仍远低于字节预算，所以走的就是「降采样 + 重编码」
    // 这条（而不是字节超限才降级的）路径。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("wide.png");
    let img = image::RgbImage::from_fn(4096, 64, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 253) as u8, ((x ^ y) % 255) as u8])
    });
    img.save_with_format(&src, image::ImageFormat::Png).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"看看这张"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 实时开启项：`size` 必须等于存盘文件的大小（重编码不影响它）。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    let rel = item["images"][0]["path"]
        .as_str()
        .expect("image path")
        .to_string();
    let stored = workdir.path().join(&rel);
    let stored_len = stored.metadata().expect("the stored copy must exist").len();

    // 前提检查：这张图确实被 image_prep 重编码了，否则本条测试抓不到缺陷。
    //
    // 存盘文件是源字节的**逐字副本**，所以文件大小永远等于源大小；重编码的
    // 结果只存在于内存里的 base64 载荷。这里用同一个公开入口复算一次载荷，
    // 证实「载荷长度 != 文件大小」，于是「缺了这段元数据就只能去量载荷」的
    // 老实现必然落在这个数上。
    let payload_len = match yi_agent_tools::image_prep::prepare_image_file(
        &stored,
        yi_agent_core::ImageDetail::High,
        yi_agent_tools::image_prep::resolve_budget(),
    )
    .expect("the fixture must be a readable image")
    {
        yi_agent_tools::image_prep::PreparedImage::Ready {
            data, width, note, ..
        } => {
            assert!(
                width < 4096,
                "the fixture must be downscaled (width {width}) for this test to be meaningful"
            );
            assert!(
                note.is_some(),
                "the fixture must be re-encoded (no note means pass-through)"
            );
            let b64 = data.as_bytes();
            let padding = b64.iter().rev().take_while(|b| **b == b'=').count().min(2);
            ((b64.len() / 4) * 3 - padding) as u64
        }
        other => panic!("expected a Ready image, got {other:?}"),
    };
    assert_ne!(
        payload_len, stored_len,
        "the fixture must actually be re-encoded for this test to be meaningful"
    );
    assert_eq!(
        item["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the live opener must report the stored file size, not the payload length: {item}"
    );

    // 落盘开启项：与实时开启项必须是同一个数（一份来源）。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["images"][0]["path"], rel.as_str(), "{opened}");
    assert_eq!(
        opened["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the persisted opener must report the stored file size: {opened}"
    );
    assert_eq!(
        opened["images"][0]["size"], item["images"][0]["size"],
        "the persisted and live openers must agree: {opened} vs {item}"
    );
    h.shutdown().await;
}

/// 发一条 RPC 并按 id 读回响应(与 `board_rpc_tests::rpc` 同款,测试流里随时
/// 会插入 `process/updated` 等通知,不能假定下一帧就是响应)。
async fn rpc(
    h: &mut Harness,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string(),
    )
    .await;
    read_response(h, id).await
}

/// 造一张**必被降采样重编码**的宽 PNG(4096x64,最长边超 High 档的 2048):
/// 存盘文件长度与内存里 base64 载荷长度不再相等,正是 Task 5 不变量要求的分辨点。
fn write_a_re_encodable_png_into_memory() -> Vec<u8> {
    let img = image::RgbImage::from_fn(4096, 64, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 253) as u8, ((x ^ y) % 255) as u8])
    });
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

/// iOS 分片上传的完整往返:`begin → chunk(0..n) → commit → turn/start`。
///
/// 断言三件事,每件都是这条 RPC 集结的一条不变量:
/// - `commit` 返回的 `path`/`size` 就是**存盘文件**本身(`size` == 该文件字节长度);
/// - 存盘内容逐字节等于分块拼接;staging 已被清掉;
/// - `turn/start` 用 `{type:"uploaded_image", uploadId}` 摄取后,开启项
///   `images[0].size` 是**文件长度**而非 base64 载荷长度(用真正被重编码的
///   夹具把两者分开),且落盘 `.jsonl` 与实时开启项报同一个数。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_round_trips_through_commit_and_turn_start() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let bytes = write_a_re_encodable_png_into_memory();
    let total = bytes.len() as u64;

    let begin = rpc(
        &mut h,
        20,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": total
        }),
    )
    .await;
    assert!(begin.get("error").is_none(), "{begin}");
    let upload_id = begin["result"]["uploadId"]
        .as_str()
        .expect("begin must return an uploadId")
        .to_string();
    assert!(
        begin["result"]["chunkSize"].as_u64().unwrap_or(0) > 0,
        "{begin}"
    );

    // 切三块发(真实客户端也分批送);块序从 0 起递增。
    let step = bytes.len().div_ceil(3);
    for (i, part) in bytes.chunks(step).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let resp = rpc(
            &mut h,
            21 + i as u64,
            "image/upload/chunk",
            serde_json::json!({ "uploadId": upload_id, "index": i, "data": data }),
        )
        .await;
        assert!(resp.get("error").is_none(), "chunk {i} refused: {resp}");
    }

    let commit = rpc(
        &mut h,
        30,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");
    let rel = commit["result"]["path"]
        .as_str()
        .expect("commit must return the stored path")
        .to_string();
    assert!(
        rel.starts_with(&format!(".yi-agent/attachments/{tid}/")),
        "{rel}"
    );
    assert!(
        rel.ends_with("-shot.png"),
        "commit must keep the client's name: {rel}"
    );
    assert_eq!(commit["result"]["mediaType"], "image/png", "{commit}");
    let stored = workdir.path().join(&rel);
    let stored_len = stored.metadata().expect("the stored copy must exist").len();
    assert_eq!(
        commit["result"]["size"].as_u64(),
        Some(stored_len),
        "{commit}"
    );
    assert_eq!(
        std::fs::read(&stored).unwrap(),
        bytes,
        "the stored copy must be the concatenated chunks"
    );
    // staging 必须已被清掉。
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&upload_id)
            .exists(),
        "the staging file must be gone after commit"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"看看这张"}}]}}}}"#
    ))
    .await;

    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "看看这张", "{item}");
    assert_eq!(item["images"][0]["path"], rel.as_str(), "{item}");
    assert_eq!(item["images"][0]["media_type"], "image/png", "{item}");
    assert_eq!(
        item["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the live opener must report the stored file size: {item}"
    );
    // 前提检查:这张图确实被重编码了,否则「size == 文件长度」会因为两者相等
    // 而抓不到「误用载荷长度」的实现。顺便把载荷本身拿出来,做「Item 里不含
    // base64」的反向断言。
    let payload_base64 = match yi_agent_tools::image_prep::prepare_image_file(
        &stored,
        yi_agent_core::ImageDetail::High,
        yi_agent_tools::image_prep::resolve_budget(),
    )
    .expect("the fixture must be a readable image")
    {
        yi_agent_tools::image_prep::PreparedImage::Ready { data, .. } => data,
        other => panic!("expected a Ready image, got {other:?}"),
    };
    let b64 = payload_base64.as_bytes();
    let padding = b64.iter().rev().take_while(|b| **b == b'=').count().min(2);
    let payload_len = ((b64.len() / 4) * 3 - padding) as u64;
    assert_ne!(
        payload_len, stored_len,
        "the fixture must be re-encoded for this test to be meaningful"
    );

    // 落盘开启项与实时开启项必须同源(同一个数)。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["images"][0]["path"], rel.as_str(), "{opened}");
    assert_eq!(
        opened["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the persisted opener must report the stored file size: {opened}"
    );
    // Item 里绝不能出现 base64 载荷:item 是每条通知都带的东西,内联图片字节
    // 会撑爆 1 MiB 的帧(设计 §3 的关键约束)。断言只针对**落盘的 items**:
    // 同一行里的 `messages` 是原样留存的会话(供 resume),其中的内容块本就带
    // base64,那是 Task 5 既有且必需的行为。
    assert!(
        !serde_json::to_string(&line["items"])
            .unwrap()
            .contains(&payload_base64),
        "no base64 may ever appear in an Item"
    );
    h.shutdown().await;
}

/// 反向断言:上传路径也必须真的把图送进**模型**(内容块),而不是只回显 item。
/// 与桌面 `{type:"image", path}` 落到同一条 `prepare_image_file` 管线(R3)。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_reaches_the_model_as_a_content_block() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([9, 9, 9]));
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();

    let begin = rpc(
        &mut h,
        40,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": bytes.len(),
        }),
    )
    .await;
    let upload_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    rpc(
        &mut h,
        41,
        "image/upload/chunk",
        serde_json::json!({ "uploadId": upload_id, "index": 0, "data": data }),
    )
    .await;
    let commit = rpc(
        &mut h,
        42,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    let rel = commit["result"]["path"].as_str().unwrap().to_string();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":43,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"这是什么"}},{{"type":"uploaded_image","uploadId":"{upload_id}"}}]}}}}"#
    ))
    .await;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content.iter().any(
                        |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "这是什么"),
                    )
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");
    let image_path = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Image { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("the user message must carry an image content block")
        .expect("the image block must carry the stored relative path");
    assert_eq!(image_path, rel);
    assert!(
        workdir.path().join(&image_path).is_file(),
        "the block path must point at the stored copy: {image_path}"
    );
    let text = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .expect("the text block must still be there");
    assert_eq!(text, "这是什么");
    h.shutdown().await;
}

/// 未知/已 abort/已过期的 `uploadId` 不再毒死整轮：那张图被**丢弃**，turn 照常
/// 起、文本照常送进模型，开启项上不带 `images`。用户于是能把消息发出去，而不是
/// 被一张找不回来的图永久卡住（chip 会留着，重试永远同样失败）。
///
/// 判据是「失败能不能靠重试自愈」：`uploadId` 未知意味着服务端手里已经没有重试
/// 所需的任何东西（记录被 TTL 收走或从未存在），重发必然复现同一结果 → 丢弃。
/// 与之相对，显式 `{type:"image", path}` 指不到文件是**请求本身**的问题，仍硬拒
/// （见 `turn_start_rejects_a_non_image_image_input`）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_drops_an_unknown_upload_id_and_still_runs_the_turn() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":50,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"uploaded_image","uploadId":"upload-does-not-exist"}},{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 关键逆转：这条 `turn/start` 必须**成功**（旧行为是回 `invalid_params` 并
    // 拒绝整轮）。开启项照常发出，只是没有 `images`。
    let mut resp: Option<serde_json::Value> = None;
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..30 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(50)) {
            resp = Some(v.clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") && opener.is_none() {
            opener = Some(v["params"]["item"].clone());
        }
        if resp.is_some() && opener.is_some() {
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "an unrecoverable upload must not fail the whole turn: {resp}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the turn must really start: {resp}"
    );
    let item = opener.expect("the turn must still open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "hi", "the text must survive: {item}");
    assert!(
        item["images"].as_array().is_none_or(|a| a.is_empty()),
        "the dropped image must not be echoed: {item}"
    );

    // 这一轮必须真的跑到底，且状态回到 idle——「占了槽却起不来」的僵尸 turn
    // 只会停在 running（只有真正收尾的 turn 才会让主循环清掉活跃标记）。
    let mut completed = false;
    let mut idle = false;
    for _ in 0..80 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("turn/completed") => completed = true,
            Some("thread/status/updated")
                if v["params"]["thread_id"] == serde_json::json!(tid)
                    && v["params"]["status"] == "idle" =>
            {
                idle = true
            }
            _ => {}
        }
        if completed && idle {
            break;
        }
    }
    assert!(
        completed,
        "the turn must run to completion after the image is dropped"
    );
    assert!(
        idle,
        "the thread must go back to idle, not stay a zombie turn"
    );

    // 文本真的到了模型：拿到 provider 的 user 消息，其中不得有任何图片块
    // （被丢弃的图既不该进 item，也不该进模型上下文）。
    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "hi"))
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");

    assert!(
        !first_user
            .content
            .iter()
            .any(|b| matches!(b, yi_agent_core::ContentBlock::Image { .. })),
        "the dropped image must not reach the model: {first_user:?}"
    );
    assert!(
        first_user
            .content
            .iter()
            .any(|b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "hi")),
        "the text must reach the model: {first_user:?}"
    );
    h.shutdown().await;
}

/// 跨会话守卫:为 thread A 上传并 commit 的图,**不能**凭一个 `uploadId` 被摄进
/// thread B。`uploadId` 只是不透明的句柄,若反查只认 id 不认归属,任何知道/猜到
/// 该 id 的客户端就能把别人的会话里的图塞进自己的会话(跨会话取数)。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_cannot_be_ingested_into_another_thread() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid_a = start_thread(&mut h).await;
    let tid_b = start_thread(&mut h).await;

    let bytes = write_a_re_encodable_png_into_memory();
    let total = bytes.len() as u64;
    let begin = rpc(
        &mut h,
        20,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid_a, "name": "shot.png", "mime": "image/png", "size": total
        }),
    )
    .await;
    let upload_id = begin["result"]["uploadId"]
        .as_str()
        .expect("begin must return an uploadId")
        .to_string();
    let step = bytes.len().div_ceil(3);
    for (i, part) in bytes.chunks(step).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let resp = rpc(
            &mut h,
            21 + i as u64,
            "image/upload/chunk",
            serde_json::json!({ "uploadId": upload_id, "index": i, "data": data }),
        )
        .await;
        assert!(resp.get("error").is_none(), "chunk {i}: {resp}");
    }
    let commit = rpc(
        &mut h,
        30,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");

    // thread B 用 A 的 uploadId 起 turn:归属不符与「未知」同等对待——那张图被
    // **丢弃**，turn 照常起（不再毒死整轮），开启项上不带 `images`。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid_b}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"steal"}}]}}}}"#
    ))
    .await;
    let mut resp: Option<serde_json::Value> = None;
    let mut opener_b: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(31)) {
            resp = Some(v.clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") && opener_b.is_none() {
            opener_b = Some(v["params"]["item"].clone());
        }
        if resp.is_some() && opener_b.is_some() {
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "B must still run: the foreign image is dropped, not fatal: {resp}"
    );
    let item_b = opener_b.expect("thread B must open with an item");
    assert_eq!(item_b["text"], "steal", "{item_b}");
    assert!(
        item_b["images"].as_array().is_none_or(|a| a.is_empty()),
        "thread B must not receive thread A's image: {item_b}"
    );
    // 安全断言不变:A 的字节一个都没进 B 的工作区。
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid_b)
            .exists(),
        "thread B must not receive thread A's image"
    );
    // 等 B 这轮收尾,再看 A 的正控——否则 B 的开启项会被下面的循环误当成 A 的。
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // 正控:A 自己仍能正常摄取这张图。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":32,"method":"turn/start","params":{{"threadId":"{tid_a}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"mine"}}]}}}}"#
    ))
    .await;
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("thread A must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(
        item["images"][0]["path"]
            .as_str()
            .map(|p| p.starts_with(&format!(".yi-agent/attachments/{tid_a}/"))),
        Some(true),
        "thread A must ingest its own upload: {item}"
    );
    h.shutdown().await;
}

/// 上传的边界:乱序块、越界块、超 20 MiB 的声明都要被拒(错误响应,不 panic)。
#[tokio::test(flavor = "multi_thread")]
async fn image_upload_refuses_out_of_order_oversized_and_too_large_uploads() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 乱序:第一条就发 index 1。
    let begin = rpc(
        &mut h,
        60,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "x.png", "size": 6 }),
    )
    .await;
    let id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    let out_of_order = rpc(
        &mut h,
        61,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": id, "index": 1,
            "data": base64::engine::general_purpose::STANDARD.encode(b"def"),
        }),
    )
    .await;
    assert!(
        out_of_order["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("out-of-order"),
        "{out_of_order}"
    );

    // 越界:声明 4 字节却发 5 字节。
    let small = rpc(
        &mut h,
        62,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "y.png", "size": 4 }),
    )
    .await;
    let small_id = small["result"]["uploadId"].as_str().unwrap().to_string();
    let overrun = rpc(
        &mut h,
        63,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": small_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abcde"),
        }),
    )
    .await;
    assert!(
        overrun["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("overruns"),
        "{overrun}"
    );

    // 声明就超 20 MiB:begin 即拒。
    let too_big = rpc(
        &mut h,
        64,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "huge.png",
            "size": yi_agent_tools::image_prep::MAX_IMAGE_BYTES + 1,
        }),
    )
    .await;
    assert!(
        too_big["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("over"),
        "{too_big}"
    );

    // 未知会话:chunk 与 commit 都拒绝,不 panic。
    let unknown_chunk = rpc(
        &mut h,
        65,
        "image/upload/chunk",
        serde_json::json!({ "uploadId": "upload-nope", "index": 0, "data": "" }),
    )
    .await;
    assert!(unknown_chunk.get("error").is_some(), "{unknown_chunk}");
    let unknown_commit = rpc(
        &mut h,
        66,
        "image/upload/commit",
        serde_json::json!({ "uploadId": "upload-nope" }),
    )
    .await;
    assert!(unknown_commit.get("error").is_some(), "{unknown_commit}");

    h.shutdown().await;
}

/// `abort` 清 staging:`image/upload/abort` 之后该 `uploadId` 再也无法 commit
/// 或用于 `turn/start`。
#[tokio::test(flavor = "multi_thread")]
async fn image_upload_abort_discards_the_staging_file() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let begin = rpc(
        &mut h,
        70,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "z.png", "size": 3 }),
    )
    .await;
    let id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        71,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abc"),
        }),
    )
    .await;
    assert!(
        workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&id)
            .is_file(),
        "the staging file must exist before abort"
    );

    let abort = rpc(
        &mut h,
        72,
        "image/upload/abort",
        serde_json::json!({ "uploadId": id }),
    )
    .await;
    assert!(abort.get("error").is_none(), "{abort}");
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&id)
            .exists(),
        "abort must remove the staging file"
    );
    let commit = rpc(
        &mut h,
        73,
        "image/upload/commit",
        serde_json::json!({ "uploadId": id }),
    )
    .await;
    assert!(commit.get("error").is_some(), "{commit}");
    h.shutdown().await;
}

/// `thread/delete` 必须连带prune 该 thread 的上传登记。否则一条 `Receiving`
/// 记录会带着它的 staging 槽一直活到 10 分钟 TTL,进程级会话名额被死 thread
/// 占着;该 `uploadId` 也仍被当成「这个 thread 的未完成上传」而非「未知」。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_prunes_its_image_upload_sessions() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一条收了一半的会话(有 staging 文件、未 commit)……
    let begin = rpc(
        &mut h,
        80,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "half.png", "size": 6 }),
    )
    .await;
    let half_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        81,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": half_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abc"),
        }),
    )
    .await;
    // ……和一条已 commit 的会话(记录仍在,`turn/start` 靠它反查)。
    let bytes = write_a_re_encodable_png_into_memory();
    let begin = rpc(
        &mut h,
        82,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": bytes.len(),
        }),
    )
    .await;
    let done_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        83,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": done_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }),
    )
    .await;
    let commit = rpc(
        &mut h,
        84,
        "image/upload/commit",
        serde_json::json!({ "uploadId": done_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");

    let staging = |id: &str| {
        workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(id)
    };
    assert!(
        staging(&half_id).is_file(),
        "staging must exist before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":90,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(90)) {
            assert!(v.get("error").is_none(), "delete must succeed: {v}");
            break;
        }
    }

    // 收了一半的会话必须已被 prune:此刻该 id 是**未知**上传(记录已随 thread
    // 一起消失),而不是「这条 thread 的未完成上传」——后者说明记录还活着。
    let after = rpc(
        &mut h,
        91,
        "image/upload/commit",
        serde_json::json!({ "uploadId": half_id }),
    )
    .await;
    let err = after["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("upload"),
        "the deleted thread's upload must be unknown, got: {after}"
    );
    assert!(
        !staging(&half_id).exists(),
        "staging must be gone after delete"
    );
    h.shutdown().await;
}

/// 反向断言：图片必须真的以内容块送进**模型**（而不只是回显在 item 上）。
///
/// 沿用 `turn_start_prompt_reaching_the_model_contains_the_attachment_manifest`
/// 的抓取手法，但捕获的是 provider 收到的 **messages**：首条 user 消息的
/// content 里必须有一个带 `path` 的 `ContentBlock::Image`，文本块仍然存在。
#[tokio::test(flavor = "multi_thread")]
async fn an_image_reaches_the_model_as_a_content_block() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("shot.png");
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([9, 9, 9]));
    img.save_with_format(&src, image::ImageFormat::Png).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"这是什么"}},{{"type":"image","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 等到 turn 收尾，确保 provider 至少被调用过一次。
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content.iter().any(
                        |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "这是什么"),
                    )
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");
    let image = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Image { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("the user message must carry an image content block");
    let rel = image.expect("the image block must carry the stored relative path");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    assert!(
        std::path::Path::new(&workdir.path().join(&rel)).is_file(),
        "the block path must point at the stored copy: {rel}"
    );
    let text = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .expect("the text block must still be there");
    assert_eq!(text, "这是什么");
    assert!(
        !text.contains("Attached files"),
        "images are content blocks, not manifest entries: {text:?}"
    );

    h.shutdown().await;
}

/// 造一张**压不动**、必然超过一个分片的 PNG，返回其原始字节。
///
/// 伪随机像素让 PNG 的滤波器几乎无从压缩，于是 1024x1024 必然远大于
/// `IMAGE_READ_CHUNK_BYTES`（512 KiB），能真的走多块回传那条路径。
fn write_an_uncompressed_png(path: &std::path::Path) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    let img = image::RgbImage::from_fn(1024, 1024, |_, _| {
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        image::Rgb([next() as u8, next() as u8, next() as u8])
    });
    img.save_with_format(path, image::ImageFormat::Png).unwrap();
    std::fs::read(path).unwrap()
}

/// `image/read` 必须把文件的**原始字节**分块回传：客户端按 `nextOffset`
/// 循环拉取、base64 解码后拼接，结果必须与磁盘上的文件逐字节相同，
/// 且最后一块的 `nextOffset` 为 `null`。
///
/// 这条测试同时钉住「`size` 是文件字节长度」与「`mediaType` 由猜测格式得出」
/// 两项契约（Task 5 的交接：`size` 必须与取回的字节数一致）。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_round_trips_a_file_in_chunks() {
    use base64::Engine as _;

    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let rel = format!(".yi-agent/attachments/{tid}/deadbeef-big.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    let source = write_an_uncompressed_png(&stored);
    assert!(
        source.len() > 512 * 1024,
        "the fixture must span several chunks, got {} bytes",
        source.len()
    );

    let mut offset: u64 = 0;
    let mut assembled: Vec<u8> = Vec::new();
    let mut media_type: Option<String> = None;
    let mut chunks = 0usize;
    for step in 0..64 {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","offset":{offset}}}}}"#,
            100 + step
        ))
        .await;
        let resp = h.read_value().await;
        assert!(resp.get("error").is_none(), "image/read failed: {resp}");
        let result = &resp["result"];
        assert_eq!(
            result["size"].as_u64(),
            Some(source.len() as u64),
            "size must be the file's byte length: {resp}"
        );
        assert_eq!(
            result["mediaType"].as_str(),
            Some("image/png"),
            "mediaType must come from the guessed format: {resp}"
        );
        media_type = result["mediaType"].as_str().map(str::to_string);
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(result["data"].as_str().expect("data must be a string"))
            .expect("data must be valid base64");
        assert!(
            chunk.len() <= 512 * 1024,
            "a chunk must stay within IMAGE_READ_CHUNK_BYTES: {}",
            chunk.len()
        );
        assembled.extend_from_slice(&chunk);
        chunks += 1;
        match result["nextOffset"].as_u64() {
            Some(next) => {
                assert_eq!(next, offset + chunk.len() as u64, "nextOffset must advance");
                offset = next;
            }
            None => break,
        }
    }
    let expected_chunks = (source.len() as u64).div_ceil(512 * 1024) as usize;
    assert!(
        chunks == expected_chunks,
        "expected {expected_chunks} chunks of at most 512 KiB, took {chunks}"
    );
    assert!(chunks > 1, "the fixture must actually span several chunks");
    assert_eq!(media_type.as_deref(), Some("image/png"));
    assert_eq!(
        assembled.len(),
        source.len(),
        "the reassembled bytes must cover the whole file"
    );
    assert!(
        assembled == source,
        "the reassembled bytes must be byte-identical to the file on disk"
    );
}

/// 分片边界：`offset` 落在文件末尾之后按空块收尾（`nextOffset: null`），
/// 不得越界 panic；末块确实返回 `null` 而不是再一次偏移。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_reports_the_final_chunk_with_a_null_offset() {
    use base64::Engine as _;

    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let rel = format!(".yi-agent/attachments/{tid}/beef-small.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([3, 4, 5]));
    img.save_with_format(&stored, image::ImageFormat::Png)
        .unwrap();
    let source = std::fs::read(&stored).unwrap();

    // 一次就取完：小图远小于一个分片。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":200,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "image/read failed: {resp}");
    let result = &resp["result"];
    assert!(
        result["nextOffset"].is_null(),
        "the final chunk must report a null nextOffset: {resp}"
    );
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(result["data"].as_str().unwrap())
        .unwrap();
    assert!(chunk == source, "the single chunk must be the whole file");

    // 越过末尾的 offset 只能得到空块，不得读越界或 panic。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":201,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","offset":100000}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "past-the-end read: {resp}");
    assert_eq!(resp["result"]["data"].as_str(), Some(""), "{resp}");
    assert!(resp["result"]["nextOffset"].is_null(), "{resp}");

    // `maxBytes` 生效：一次只要 4 字节，`nextOffset` 必须推进 4。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":202,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","maxBytes":4}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "sized read: {resp}");
    let first = base64::engine::general_purpose::STANDARD
        .decode(resp["result"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(first.len(), 4, "{resp}");
    assert_eq!(resp["result"]["nextOffset"].as_u64(), Some(4), "{resp}");

    // `maxBytes: 0` 不得造成 `nextOffset` 不前进（否则客户端死循环）：按未
    // 指定处理，整张图一次取完。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":203,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","maxBytes":0}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "zero-sized read: {resp}");
    let whole = base64::engine::general_purpose::STANDARD
        .decode(resp["result"]["data"].as_str().unwrap())
        .unwrap();
    assert!(
        whole == source,
        "maxBytes:0 must fall back to the chunk cap"
    );
    h.shutdown().await;
}

/// 限根是安全边界：`../` 逃逸与工作区外的绝对路径都必须以 `invalid_params`
/// 拒绝——`image/read` 不得成为任意文件读取原语。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_path_outside_the_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    // cwd 是子目录，父目录里放一张**真图**：这样「被拒」只能是因为越界，
    // 而不是因为文件不存在或不是图片。
    let cwd = tmp.path().join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let secret = tmp.path().join("secret.png");
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([1, 1, 1]));
    img.save_with_format(&secret, image::ImageFormat::Png)
        .unwrap();

    let mut cfg = default_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // (a) 相对路径向上逃逸。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":300,"method":"image/read","params":{{"threadId":"{tid}","path":"../secret.png"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a relative escape must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("path escapes the workspace"),
        "the escape must be caught by the confinement check, not by a fallback: {resp}"
    );

    // (b) 工作区外的绝对路径（`Path::join` 遇绝对路径会整体替换）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":301,"method":"image/read","params":{{"threadId":"{tid}","path":"{}"}}}}"#,
        secret.display()
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an outside absolute path must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("path escapes the workspace"),
        "the absolute path must be caught by the confinement check: {resp}"
    );
    h.shutdown().await;
}

/// 只送可解码的图片：一个文本文件即使在工作区内也必须被拒。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_non_image_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    std::fs::write(workdir.path().join("notes.txt"), b"just some text\n").unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":400,"method":"image/read","params":{{"threadId":"{tid}","path":"notes.txt"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a non-image must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a supported image"),
        "the text file must be rejected by the format guess: {resp}"
    );

    // 把 PDF 伪装成 .png 的扩展名也一样：判定看的是字节，不是名字。
    std::fs::write(workdir.path().join("fake.png"), b"%PDF-1.4 fake\n").unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":401,"method":"image/read","params":{{"threadId":"{tid}","path":"fake.png"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "the extension must not decide; the bytes must: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a supported image"),
        "the mislabelled PDF must be rejected by the format guess: {resp}"
    );
    h.shutdown().await;
}

/// 超过 `IMAGE_READ_MAX_BYTES`（20 MiB）的图必须被拒——分片只是传输形态，
/// 不代表可以无上限地服务任意大文件。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_an_oversized_image() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一个宽 6000px 的 PNG 位图够 20 MiB；重新编码成 PNG 更容易超限。
    let rel = format!(".yi-agent/attachments/{tid}/huge.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    write_an_uncompressed_png(&stored);
    // 直接从磁盘读回来按字节补齐到 20 MiB 以上；文件本身仍是同一张真 PNG
    // （PNG 允许附加尾随数据，`guess_format` 只看魔数，尺寸检查据此拒绝）。
    let mut bytes = std::fs::read(&stored).unwrap();
    bytes.resize(20 * 1024 * 1024 + 1, 0);
    std::fs::write(&stored, &bytes).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":500,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an image past IMAGE_READ_MAX_BYTES must be refused: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("image too large"),
        "the refusal must come from the size gate: {resp}"
    );

    // 一个超过上限的**非图片**同样要在读取/解码之前被拒：闸门按元数据长度
    // 先设，故它报的是尺寸而不是「不是图片」——证明没有先把整个文件读进来。
    let huge_text = workdir.path().join("huge.txt");
    std::fs::write(&huge_text, vec![b'a'; 20 * 1024 * 1024 + 1]).unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":501,"method":"image/read","params":{{"threadId":"{tid}","path":"huge.txt"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an oversized file of any kind must be refused: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("image too large"),
        "the metadata gate must fire before the format guess: {resp}"
    );
    h.shutdown().await;
}

/// 非普通文件必须被拒：`thread` 的 cwd 下可达的一个 FIFO（agent 或克隆下来的
/// 仓库都可能放进来）元数据长度为 `0`，能通过尺寸闸；而 `std::fs::read` 的
/// `open()` 会在无写端时**阻塞**，把整条 serve 循环（所有客户端/thread）挂住。
/// 本 RPC 是安全边界，故 `!is_file()` 必须直接拒绝，绝不进入读取。
///
/// 此处**自建 runtime** 而非用 `#[tokio::test]`：修复回退时那个 worker 会卡在
/// `open()` 上无法 join，若由 `#[tokio::test]` 代管，进程在 teardown 时挂住、
/// 整个套件被拖死。自建 runtime 让测试用 `tokio::time::timeout` 把它变成一次
/// **可读的失败**，再 `mem::forget` 掉这个 join 不掉的 runtime，进程照常退出。
#[test]
fn image_read_refuses_a_non_regular_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let mut h = rt.block_on(Harness::with_cfg(cfg));
    let tid = rt.block_on(start_thread(&mut h));

    let fifo = workdir
        .path()
        .join(".yi-agent/attachments")
        .join(&tid)
        .join("trap.png");
    std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
    // 经 `mkfifo` 造 FIFO——`libc` 不是本 crate 的依赖，不为一条测试新增它。
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available to exercise the FIFO path");
    assert!(status.success(), "mkfifo failed for {fifo:?}");
    let md = std::fs::symlink_metadata(&fifo).unwrap();
    assert!(
        !md.is_file() && !md.is_dir(),
        "the fixture must really be a non-regular file: {fifo:?}"
    );

    rt.block_on(h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":550,"method":"image/read","params":{{"threadId":"{tid}","path":".yi-agent/attachments/{tid}/trap.png"}}}}"#
    )));
    // 修复缺失时 `image/read` 的 `fs::read` 会卡在 `open()` 上：此处**自己**
    // 给 `read_line` 设 5s 上限（而不是用 `Harness::read_value`，它内部的
    // 5s 超时会抢先 panic 掉），超时即说明它**没有**拒绝。超时后把这个卡住的
    // `read_line` future 直接丢掉，再泄露 runtime、再断言失败——给出一次
    // 干净的 RED 输出，而不是让整个测试套件挂死。
    let mut buf = String::new();
    let got = rt
        .block_on(async { tokio::time::timeout(EVENT_WAIT, h.client_r.read_line(&mut buf)).await });
    let Ok(Ok(n)) = got else {
        std::mem::forget(rt);
        panic!("image/read must not block on a non-regular file");
    };
    assert!(n > 0, "unexpected EOF while waiting for the refusal");
    let resp: serde_json::Value =
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON");
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a non-regular file must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a regular file"),
        "the refusal must come from the regular-file check: {resp}"
    );
    rt.block_on(h.shutdown());
}

/// 未知 thread 必须拒绝（`thread_cwd` 两条解析路径都落空）。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_an_unknown_thread() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":600,"method":"image/read","params":{"threadId":"thread-nope","path":"a.png"}}"#,
    )
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an unknown thread must be refused: {resp}"
    );
    h.shutdown().await;
}

/// `thread_cwd` 的两条解析路径（控制器裁定 2）+ 落空：
/// - 内存里的会话直接给 `Session.cwd`；
/// - 冷 thread 按全局索引定位其 store，再由 `<cwd>/.yi-agent/threads` 上溯两级；
/// - 两处都没有 → `None`。
#[tokio::test(flavor = "multi_thread")]
async fn thread_cwd_prefers_memory_then_the_index() {
    let warm = tempfile::TempDir::new().unwrap();
    let cold = tempfile::TempDir::new().unwrap();
    let cfg = default_config();

    // 内存路径：`threads` 里有该 id，工作区取自 `Session.cwd`。
    let (prompt_tx, _prompt_rx) = mpsc::channel(1);
    let (interrupt_tx, _interrupt_rx) = mpsc::channel(1);
    let (interject_tx, _interject_rx) = mpsc::channel(1);
    let (session_tx, _session_rx) = mpsc::channel(1);
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    threads.insert(
        "warm".to_string(),
        ThreadSession {
            thread_id: "warm".to_string(),
            cwd: warm.path().to_string_lossy().to_string(),
            model: "test".to_string(),
            active_turn_id: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::new(crate::thread_store::ThreadStore::new(warm.path())),
            status: Arc::new(StdMutex::new(ThreadStatus::Idle)),
        },
    );

    // 冷 thread：store 在磁盘上，但它不在 `threads` 里。
    let cold_store = crate::thread_store::ThreadStore::new(cold.path());
    cold_store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: "cold".to_string(),
            cwd: cold.path().to_string_lossy().to_string(),
            model: "test".to_string(),
            created_at: 0,
            updated_at: 0,
            title: None,
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();
    // 全局索引指向该工作区（canonicalize 以对齐真实写入的路径形态）。
    let index_dir = tempfile::TempDir::new().unwrap();
    let workspaces = WorkspaceIndex::new(index_dir.path().join("workspaces.json"));
    workspaces
        .add(&cold.path().canonicalize().unwrap())
        .unwrap();

    assert_eq!(
        thread_cwd(&threads, &workspaces, &cfg, "warm"),
        Some(warm.path().to_path_buf()),
        "an in-memory thread must resolve from its own session cwd"
    );
    assert_eq!(
        thread_cwd(&threads, &workspaces, &cfg, "cold"),
        Some(cold.path().canonicalize().unwrap()),
        "a cold thread must resolve from the store's root, two levels up"
    );
    assert_eq!(
        thread_cwd(&threads, &workspaces, &cfg, "ghost"),
        None,
        "a thread with neither an in-memory session nor an index entry is unknown"
    );
}

/// 只发附件、不打字也必须能起 turn（空 input 校验放宽到「既无文本又无附件」）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_accepts_an_attachment_only_message() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("only.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "attachment-only start must succeed: {resp:?}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "an attachment-only turn must still return a turn_id: {resp:?}"
    );
    h.shutdown().await;
}

/// 只发图片、不打字也必须能起 turn——空 input 校验放宽到「既无文本、又无附件、
/// 又无图片」三缺一才算空（控制器裁定 2）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_accepts_an_image_only_message() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("only.png");
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([7, 7, 7]));
    img.save_with_format(&src, image::ImageFormat::Png).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "image-only start must succeed: {resp:?}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "an image-only turn must still return a turn_id: {resp:?}"
    );
    h.shutdown().await;
}

/// 不是图片的「图片输入」（例如把 PDF 塞进 `type:"image"`）必须就地拒绝：
/// 错误提到 image，且**一个 turn 都不起**（准备失败早于占位与发帧）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_rejects_a_non_image_image_input() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("not-an-image.png");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut methods_before_response: Vec<String> = Vec::new();
    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(11)) {
            resp = Some(v);
            break;
        }
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            methods_before_response.push(m.to_string());
        }
    }
    let resp = resp.expect("turn/start must answer");
    let err = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(err.contains("image"), "got: {resp}");
    assert!(
        methods_before_response.is_empty(),
        "no turn may start on a bad image: {methods_before_response:?}"
    );
    // 而且活跃标记必须干净：同一 thread 的**下一轮**仍能正常起（不是
    // 「已占用但起不来」的僵尸 turn）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let mut second: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(12)) {
            second = Some(v);
            break;
        }
    }
    let second = second.expect("the follow-up turn/start must answer");
    assert!(
        second.get("error").is_none(),
        "a rejected image must not leave the turn slot occupied: {second:?}"
    );
    h.shutdown().await;
}

/// 显式路径的图片被拒时，错误必须带一个**结构化**判别码（`data.code = "invalid_image"`），
/// 客户端才不必去猜/子串匹配那条人话 message。数字码仍是 `invalid_params`——这次
/// 拒绝确实是"参数不合法"，加码只是让调用方能分级处理。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_reports_a_structured_code_for_a_bad_image_path() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("missing.png"); // 不创建：路径指不到任何文件

    let resp = rpc(
        &mut h,
        13,
        "turn/start",
        serde_json::json!({
            "threadId": tid,
            "input": [
                { "type": "image", "path": src.display().to_string() },
                { "type": "text", "text": "hi" }
            ],
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32602), "{resp}");
    assert_eq!(resp["error"]["data"]["code"], "invalid_image", "{resp}");
    // 人话里仍带 image，供不看码的客户端兜底。
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("image"),
        "{resp}"
    );
    h.shutdown().await;
}

/// 显式路径的图片被拒**不改变**声明大小以外的判据：非图内容（PDF 冒充 .png）也
/// 走同一条 `invalid_image`，报错的仍是路径那一侧。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_reports_a_structured_code_for_a_non_image_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("fake.png");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    let resp = rpc(
        &mut h,
        14,
        "turn/start",
        serde_json::json!({
            "threadId": tid,
            "input": [
                { "type": "image", "path": src.display().to_string() },
                { "type": "text", "text": "hi" }
            ],
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32602), "{resp}");
    assert_eq!(resp["error"]["data"]["code"], "invalid_image", "{resp}");
    h.shutdown().await;
}

/// 读不到的附件路径必须就地拒绝（错误提到 attachment），且一个 turn 都不起。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_rejects_an_unreadable_attachment() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"/definitely/not/here.pdf"}},{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 应答之前不得有任何通知:准备失败要发生在发帧之前。
    let mut methods_before_response: Vec<String> = Vec::new();
    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(10)) {
            resp = Some(v);
            break;
        }
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            methods_before_response.push(m.to_string());
        }
    }
    let resp = resp.expect("turn/start must answer");
    let err = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(err.contains("attachment"), "got: {resp}");
    assert!(
        methods_before_response.is_empty(),
        "no turn may start on a bad attachment: {methods_before_response:?}"
    );
    h.shutdown().await;
}

/// Task 7:`thread/delete` 要连带收走该会话的附件目录——附件与 thread 文件同一
/// 生命周期（设计 §6(d)）。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_removes_its_attachments() {
    // 自带 workdir:默认目录跨次运行会残留同名附件,「盘上存在」的断言会被旧
    // 文件蒙混过关。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 源文件放在工作区外,模拟「从 Downloads 选文件」。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;
    // 起带附件的 turn,读到 `turn/completed` 说明准备期已过(附件在发帧之前
    // 复制)、且 turn 已结束,删除不会落在半途。
    let mut saw_response = false;
    let mut completed = false;
    while !(saw_response && completed) {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            assert!(v.get("error").is_none(), "turn/start must succeed: {v}");
            saw_response = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = true;
        }
    }

    let att_dir = workdir.path().join(".yi-agent/attachments").join(&tid);
    assert!(
        att_dir.is_dir(),
        "attachment dir should exist before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            assert!(v.get("error").is_none(), "delete must succeed: {v}");
            break;
        }
    }

    assert!(
        !att_dir.exists(),
        "attachments must be reclaimed with the thread"
    );
    h.shutdown().await;
}

/// Task 7:`/clear` 只截断历史,会话还在(clear 之后照样能继续),附件不清——
/// 这是与 `thread/delete` 的关键区别(设计 §6(e))。
#[tokio::test(flavor = "multi_thread")]
async fn clearing_a_thread_keeps_its_attachments() {
    // 设计 §6(e):/clear 截断历史但会话还在,附件不清。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;
    // 等 turn 跑完:`thread/clear` 在 turn 进行中会被拒(turn_in_progress)。
    let mut saw_response = false;
    let mut completed = false;
    while !(saw_response && completed) {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            assert!(v.get("error").is_none(), "turn/start must succeed: {v}");
            saw_response = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = true;
        }
    }

    let att_dir = workdir.path().join(".yi-agent/attachments").join(&tid);
    assert!(att_dir.is_dir(), "attachment dir should exist before clear");

    // 主循环要等 `TurnEvent::Finished` 才清 `active_turn_id`,刚跑完一轮立刻
    // clear 可能撞上 -32012(turn 进行中)的收尾窗口——用现成的重试 helper,
    // 与其它 clear 测试同一口径。
    let resp = clear_thread_retrying(&mut h, &tid).await;
    assert!(resp.get("error").is_none(), "clear must succeed: {resp}");

    assert!(att_dir.is_dir(), "thread/clear must not remove attachments");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_start_unknown_thread_returns_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"threadId":"nope","input":[{"type":"text","text":"hi"}]}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 3);
    assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_start_missing_thread_id_returns_invalid_params() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"hi"}]}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 3);
    assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_start_empty_input_returns_invalid_params() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[]}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 3);
    assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_interject_while_running_keeps_the_same_turn() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let started_turn = loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(11)) {
            break v["result"]["turn_id"].as_str().unwrap().to_string();
        }
    };

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"also do X"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(12)) {
            assert_eq!(
                v["result"]["turn_id"].as_str().unwrap(),
                started_turn,
                "an interjection must not open a new turn: {v}"
            );
            assert!(
                v["result"]["interjection_id"].as_str().is_some(),
                "expected an interjection_id: {v}"
            );
            break;
        }
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_interject_without_an_active_turn_is_rejected() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":13,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"too early"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(13)) {
            assert_eq!(v["error"]["code"], -32013, "expected not running: {v}");
            break;
        }
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_start_while_turn_in_progress_returns_turn_in_progress() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 读到 id==3 的响应(跳过 turn/started 通知与 item/* 帧)。
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            assert!(v["result"]["turn_id"].is_string(), "expected turn_id: {v}");
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"again"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
            break;
        }
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_interrupt_cancels_active_turn() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 等 turn/started,确认 turn 已活跃。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = Some(v);
            break;
        }
    }
    let completed = completed.expect("expected a turn/completed notification");
    assert_eq!(completed["params"]["status"], "interrupted");
    h.shutdown().await;
}

/// 复现报告：运行中把模式从 normal 切到 yolo 后，Stop（turn/interrupt）必须
/// 仍然能取消这一轮。
#[tokio::test(flavor = "multi_thread")]
async fn turn_interrupt_still_cancels_after_switching_to_yolo_mid_turn() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    // 起一个永不结束的 turn。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let _ = read_until_method(&mut h, "turn/started").await;

    // 运行中切到 yolo。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;

    // 等到 setPermissionMode 的响应（或先到的 turn/completed：那就是 bug 本身）。
    let mut saw_completed_early = false;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            saw_completed_early = true;
            break;
        }
        if v.get("id") == Some(&serde_json::json!(4)) {
            break;
        }
    }
    assert!(
        !saw_completed_early,
        "switching to yolo must not end the running turn on its own"
    );

    // 现在点 Stop。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = Some(v);
            break;
        }
    }
    let completed = completed.expect("turn/interrupt must still end the turn after a mode switch");
    assert_eq!(completed["params"]["status"], "interrupted");
    h.shutdown().await;
}

/// 同一报告的第二个分支：切到 yolo 时正好有一个**待审批**的工具调用挂起，
/// 随后 Stop 必须仍能取消（审批等待期间的中断路径）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_interrupt_cancels_after_yolo_while_approval_is_pending() {
    let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    // 等审批请求（此时 turn 卡在等待决定）。
    let _approval_id = read_until_approval(&mut h).await;

    // 审批挂起期间切到 yolo。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            break;
        }
    }

    // 现在点 Stop。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = Some(v);
            break;
        }
    }
    let completed =
        completed.expect("turn/interrupt must cancel even while an approval is pending");
    assert_eq!(completed["params"]["status"], "interrupted");
    h.shutdown().await;
}

/// 报告的第三个分支：切到 yolo（沙箱放开）后，模型已经发起一个**长时间运行的
/// bash 命令**。此时 Stop 必须能真正杀掉那个命令并结束 turn，而不是卡住。
#[tokio::test(flavor = "multi_thread")]
async fn turn_interrupt_kills_a_running_tool_after_yolo() {
    let mut h = Harness::with_factory(build_long_tool_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"run sleep"}}]}}}}"#
    ))
    .await;

    // 等工具真正开始执行（toolCall item 已开始）。
    let _ = read_until_method(&mut h, "item/started").await;

    // 工具执行中切到 yolo。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            break;
        }
    }

    // 点 Stop：必须真的结束这一轮。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..60 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = Some(v);
            break;
        }
    }
    let completed = completed.expect("turn/interrupt must kill the running tool and end the turn");
    assert_eq!(completed["params"]["status"], "interrupted");
    h.shutdown().await;
}

/// 报告的第四个分支：`call_stream` 本身还在等待（请求已发出、响应头未回），
/// 期间切到 yolo 再 Stop。Stop 必须也生效——这是「THINK 阶段在 call_stream
/// 之内」被中断的窗口。
#[tokio::test(flavor = "multi_thread")]
async fn turn_interrupt_works_while_provider_call_is_blocking() {
    let mut h = Harness::with_factory(build_slow_call_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let _ = read_until_method(&mut h, "turn/started").await;

    // 精确等到 provider 已经进入 call_stream（请求在途、响应头未回）再打断，
    // 否则 Stop 会落在它之前的 Check 1，测不出这个窗口。
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if SLOW_CALL_ENTERED.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        SLOW_CALL_ENTERED.load(Ordering::SeqCst),
        "provider must have entered call_stream before we interrupt"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let started = std::time::Instant::now();
    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..60 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = Some(v);
            break;
        }
    }
    let completed =
        completed.expect("turn/interrupt must end the turn even while call_stream blocks");
    assert_eq!(completed["params"]["status"], "interrupted");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "the still-blocking call_stream must not delay the cancel: took {:?}",
        started.elapsed()
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn driver_ignores_interrupt_tagged_with_other_turn() {
    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let (hub, client) = test_hub(server_w);

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    let built = build_delayed_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();
    let handle = tokio::spawn(run_thread_driver(
        "thread-1".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(60),
        Arc::new(AtomicU64::new(1)),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    // 上一轮残留的中断(属于 turn-0)必须被忽略。
    interrupt_tx.try_send("turn-0".into()).unwrap();
    prompt_tx
        .send(TurnPrompt {
            turn_id: "turn-1".into(),
            prompt: "hi".into(),
            activate: None,
            attachments: Vec::new(),
            image_blocks: Vec::new(),
            image_refs: Vec::new(),
            display_text: "hi".into(),
            lead_request_id: None,
        })
        .await
        .unwrap();

    let mut client_r = BufReader::new(client_r);
    let mut status = None;
    for _ in 0..10 {
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, client_r.read_line(&mut buf))
            .await
            .expect("timed out waiting for a notification")
            .unwrap();
        if n == 0 {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        if v["method"] == "turn/completed" {
            status = Some(v["params"]["status"].as_str().unwrap().to_string());
            break;
        }
    }
    assert_eq!(
        status.as_deref(),
        Some("completed"),
        "a stale interrupt must not cancel the new turn"
    );

    drop(prompt_tx);
    drop(interrupt_tx);
    let ev = tokio::time::timeout(EVENT_WAIT, turn_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev, TurnEvent::Finished { .. }));
    let _ = tokio::time::timeout(EVENT_WAIT, handle).await;
}

/// Critical 2:往一个已注销的 **ws** 客户端写响应**不得**是致命错误。
///
/// 一台手机断连/被撤销、或被广播背压摘除后,主循环仍可能为它手里的一帧去写
/// 响应;那是多客户端共享主循环上的常态,不该带走所有其它客户端。回归:
/// `write_response(..)?` 曾把 `Closed` 冒泡成 `serve` 的致命错误。
///
/// 反向钉死:stdio 的 `local` 仍保持改造前的**致命**语义(出口泵死了,再写
/// 就是错),`driver_reports_finished_when_writer_fails` 依赖它。
#[tokio::test]
async fn write_response_to_a_gone_ws_client_is_not_fatal() {
    let hub = crate::broadcast::Broadcaster::new();
    let response = || ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id: RequestId::Num(1),
        result: Some(serde_json::json!({})),
        error: None,
    };

    // ws 客户端已走:非致命,返回 Ok(它同时被确认已从 hub 摘除)。
    let ws_id = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let _rx = hub.register(ws_id.clone());
    hub.unregister(&ws_id);
    assert!(
        write_response(&hub, &ws_id, response()).await.is_ok(),
        "a response to a gone ws client must not be fatal"
    );
    assert!(!hub.is_connected(&ws_id), "the ws client must be gone");

    // 反面:stdio 的 `local` 保持致命。
    let local = crate::broadcast::ClientId::local();
    let _rx = hub.register(local.clone());
    hub.unregister(&local);
    assert!(
        write_response(&hub, &local, response()).await.is_err(),
        "the stdio local client must stay fatal on write failure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn driver_reports_finished_when_writer_fails() {
    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 断开读端 → 写通知失败
    let (hub, client) = test_hub(server_w);

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    let built = build_test_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();
    let handle = tokio::spawn(run_thread_driver(
        "thread-1".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(60),
        Arc::new(AtomicU64::new(1)),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    prompt_tx
        .send(TurnPrompt {
            turn_id: "turn-1".into(),
            prompt: "hi".into(),
            activate: None,
            attachments: Vec::new(),
            image_blocks: Vec::new(),
            image_refs: Vec::new(),
            display_text: "hi".into(),
            lead_request_id: None,
        })
        .await
        .unwrap();

    let ev = tokio::time::timeout(EVENT_WAIT, turn_rx.recv())
        .await
        .expect("driver must report Finished even when writes fail")
        .expect("turn channel must stay open");
    assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

    drop(prompt_tx);
    let _ = tokio::time::timeout(EVENT_WAIT, handle).await;
}

/// 回归:driver 空闲(没有 turn 在跑)时,也必须在有限时间内响应会话命令。
///
/// 修复前 `session_rx` 只在每轮的内层 select 里被轮询,空闲 thread 的命令
/// 会一直躺在 channel 里,直到下一个 turn/start 才被读到——这个测试会超时。
#[tokio::test(flavor = "multi_thread")]
async fn idle_driver_serves_a_session_command_without_a_new_turn() {
    use crate::session::{CompactOutcome, SessionCommand};

    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 本测试不看 writer 输出
    let (hub, client) = test_hub(server_w);

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(8);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    // prompt_tx 保住不 drop:driver 应停在 idle,而不是因 prompt_rx 关闭退出。
    let _keep_prompt_tx = prompt_tx;

    let built = build_test_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();

    let handle = tokio::spawn(run_thread_driver(
        "thread-idle-1".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(5),
        Arc::new(AtomicU64::new(0)),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    // 没有任何 turn/start:直接投一条 compact,必须在有限时间内拿到回复。
    let (reply, answer) = oneshot::channel();
    session_tx
        .send(SessionCommand::Compact { reply })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(EVENT_WAIT, answer)
        .await
        .expect("an idle driver must service a session command without a new turn")
        .expect("the reply channel must be fulfilled");
    assert_eq!(
        outcome,
        CompactOutcome::NotReduced,
        "an empty session has nothing to compact"
    );

    // driver 仍在运行(命令不该把它弄停)。
    assert!(!handle.is_finished());
    handle.abort();

    // turn_rx 里的 Finished 是可选的:本路径 turn_id 为 None,不该发 Finished。
    assert!(
        turn_rx.try_recv().is_err(),
        "a command with no turn must not report a turn as finished"
    );
}

/// 切模型 = 在两轮之间**重建** agent:provider 与 config 换新,而 session `Arc`
/// (以及绑定其上的委派 caller)、工具注册表、审批路径三样必须原样保留。
///
/// 直接驱动**生产函数** `apply_session_command`(与 clear/compact 的测试同款),
/// 才能拿到重建后的 agent 逐一断言这四项保留语义。
#[tokio::test]
async fn set_model_command_rebuilds_with_the_new_model() {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 本测试不看 writer 输出
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    // 带工具注册表与审批路径的 agent,模拟真实 thread:重建时都不该被丢。
    let mut registry = yi_agent_core::ToolRegistry::new();
    registry.register(Arc::new(FakeBash));
    let tools: Arc<yi_agent_core::ToolRegistry> = Arc::new(registry);
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(false),
        std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        Arc::new(|_cmd: &str| None),
    ));
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(provider.clone(), tools.clone(), config.clone())
        .with_permission(checker.clone(), rx_arc.clone());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
        ]);
    let original_handle = agent.session_handle();
    // 宿主把委派 caller 绑到这个 session `Arc`;换成新 Arc 会让 fork 读到被丢弃的记录。
    let caller = yi_agent_subagent::CallerContext::new(original_handle.clone());
    let _keep_decision_tx = decision_tx;

    // 新条目解析出的 cfg:仍是有效的 anthropic 配置,只是 model 换了。
    let mut effective = crate::test_support::test_config();
    effective.model = "switched-model".into();

    let (reply, answer) = oneshot::channel();
    agent = apply_session_command(
        agent,
        SessionCommand::SetModel {
            effective: Box::new(effective.clone()),
            model_ref: Some("B".into()),
            reply,
        },
        &provider,
        &config,
        &store,
        "thread-set-model-1",
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    answer
        .await
        .expect("the set-model reply channel must be fulfilled")
        .expect("rebuilding against a valid cfg must succeed");

    assert_eq!(
        agent.config().model,
        "switched-model",
        "the rebuilt agent must run the newly selected model"
    );
    assert!(
        Arc::ptr_eq(&original_handle, &agent.session_handle()),
        "the rebuild must reuse the session Arc, not swap in a new one"
    );
    assert_eq!(
        agent.session().messages().len(),
        2,
        "the rebuild must preserve the session history"
    );
    assert_eq!(
        caller.snapshot().expect("the caller stays bound"),
        agent.session().messages().to_vec(),
        "the bound caller must still read the rebuilt agent's live transcript"
    );
    let kept_tools = agent.tools();
    assert!(
        Arc::ptr_eq(&kept_tools, &tools),
        "the rebuild must keep the very same tool registry"
    );
    assert!(
        kept_tools.names().iter().any(|name| name == "bash"),
        "the preserved registry must still hold its tools"
    );
    let kept_checker = agent
        .permission_checker()
        .expect("the rebuild must re-attach the permission checker");
    assert!(
        Arc::ptr_eq(&kept_checker, &checker),
        "the rebuild must keep the approval checker"
    );
    let kept_rx = agent
        .decision_rx()
        .expect("the rebuild must re-attach the decision channel");
    assert!(
        Arc::ptr_eq(&kept_rx, &rx_arc),
        "the rebuild must keep the decision channel"
    );

    assert!(
        turn_rx.try_recv().is_err(),
        "a session command with no turn must not report a turn as finished"
    );
}

/// 切 effort = 在两轮之间**重建** agent：只换 `gen_params.reasoning_effort`，
/// provider 原样复用（effort 不烤进 URL/key）。session `Arc`、工具注册表、
/// 审批路径三样必须原样保留（与 SetModel 同款重建语义）。
#[tokio::test]
async fn set_reasoning_effort_command_rebuilds_without_a_new_provider() {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 本测试不看 writer 输出
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    let mut registry = yi_agent_core::ToolRegistry::new();
    registry.register(Arc::new(FakeBash));
    let tools: Arc<yi_agent_core::ToolRegistry> = Arc::new(registry);
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(false),
        std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        Arc::new(|_cmd: &str| None),
    ));
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(provider.clone(), tools.clone(), config.clone())
        .with_permission(checker.clone(), rx_arc.clone());
    let original_handle = agent.session_handle();
    let _keep_decision_tx = decision_tx;
    assert_eq!(
        agent.config().gen_params.reasoning_effort,
        None,
        "the fixture starts with no effort"
    );

    let (reply, answer) = oneshot::channel();
    agent = apply_session_command(
        agent,
        SessionCommand::SetReasoningEffort {
            effort: Some("high".into()),
            reply,
        },
        &provider,
        &config,
        &store,
        "thread-set-effort-1",
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    answer
        .await
        .expect("the set-effort reply channel must be fulfilled")
        .expect("rebuilding with a valid effort must succeed");

    assert_eq!(
        agent.config().gen_params.reasoning_effort.as_deref(),
        Some("high"),
        "the rebuilt agent must run the newly selected effort"
    );
    assert!(
        Arc::ptr_eq(&original_handle, &agent.session_handle()),
        "the rebuild must reuse the session Arc, not swap in a new one"
    );
    let kept_tools = agent.tools();
    assert!(
        Arc::ptr_eq(&kept_tools, &tools),
        "the rebuild must keep the very same tool registry"
    );
    let kept_checker = agent
        .permission_checker()
        .expect("the rebuild must re-attach the permission checker");
    assert!(
        Arc::ptr_eq(&kept_checker, &checker),
        "the rebuild must keep the approval checker"
    );
    let kept_rx = agent
        .decision_rx()
        .expect("the rebuild must re-attach the decision channel");
    assert!(
        Arc::ptr_eq(&kept_rx, &rx_arc),
        "the rebuild must keep the decision channel"
    );
    assert!(
        turn_rx.try_recv().is_err(),
        "a session command with no turn must not report a turn as finished"
    );
}

/// 切 `preserve_thinking = false` 必须**真的**把历史轮的思考凭证从共享 session
/// 本体里剥掉:不只是换请求副本,而是把落盘/展示的权威 transcript 一并剥离。
/// 否则重开会话后凭证会「复活」——与「关闭保留」直接矛盾(spec §4.1)。
///
/// 这条测试刻意驱动真实的 `apply_session_command`(不是 mock 出来的中间态):
/// 凭证先**确实存在**,调用后**确实消失**,以此证伪「没接到 session」的假设。
#[tokio::test]
async fn set_preserve_thinking_false_strips_the_shared_session() {
    use yi_agent_core::{ContentBlock, Message, ThinkingToken};

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 本测试不看 writer 输出
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    let mut registry = yi_agent_core::ToolRegistry::new();
    registry.register(Arc::new(FakeBash));
    let tools: Arc<yi_agent_core::ToolRegistry> = Arc::new(registry);
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(false),
        std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        Arc::new(|_cmd: &str| None),
    ));
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(provider.clone(), tools.clone(), config.clone())
        .with_permission(checker.clone(), rx_arc.clone());
    // 会话里放一条**带思考凭证**的 assistant 消息——这是待剥离的对象。
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![
            Message::user("q"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "reasoning".into(),
                    token: ThinkingToken::Signature("SIG-XYZ".into()),
                },
                ContentBlock::Text("a".into()),
            ]),
        ]);
    let original_handle = agent.session_handle();
    let _keep_decision_tx = decision_tx;

    // 前置:凭证此刻**确实在**(否则后面的断言可能因 fixture 错而假绿)。
    let before = agent.session().messages().to_vec();
    assert!(
        before
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "fixture must start with a Thinking block in the session"
    );

    let (reply, answer) = oneshot::channel();
    agent = apply_session_command(
        agent,
        SessionCommand::SetPreserveThinking {
            preserve: false,
            reply,
        },
        &provider,
        &config,
        &store,
        "thread-set-preserve-1",
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    answer
        .await
        .expect("the set-preserve reply channel must be fulfilled")
        .expect("toggling the flag must succeed");

    assert!(
        !agent.config().preserve_thinking,
        "the rebuilt agent must carry the new flag"
    );
    assert!(
        Arc::ptr_eq(&original_handle, &agent.session_handle()),
        "the rebuild must reuse the session Arc, not swap in a new one"
    );
    let after = agent.session().messages().to_vec();
    assert!(
        !after
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "turning the flag off must strip Thinking blocks from the shared session: {after:?}"
    );
    // 剥离只该去凭证:正文消息与数量必须原样。
    assert_eq!(
        after.len(),
        before.len(),
        "stripping must not drop messages"
    );
    assert!(
        after
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Text(t) if t == "a")),
        "stripping must leave the neighbouring text block intact"
    );
    let kept_tools = agent.tools();
    assert!(
        Arc::ptr_eq(&kept_tools, &tools),
        "the rebuild must keep the very same tool registry"
    );
    assert!(
        Arc::ptr_eq(
            &agent.permission_checker().expect("checker re-attached"),
            &checker
        ),
        "the rebuild must keep the approval checker"
    );
    assert!(
        Arc::ptr_eq(
            &agent.decision_rx().expect("decision channel re-attached"),
            &rx_arc
        ),
        "the rebuild must keep the decision channel"
    );
    assert!(
        turn_rx.try_recv().is_err(),
        "a session command with no turn must not report a turn as finished"
    );
}

/// 反向:切 `preserve_thinking = true` 不该动 session 的既有内容(凭证原样留下)。
#[tokio::test]
async fn set_preserve_thinking_true_leaves_the_shared_session_untouched() {
    use yi_agent_core::{ContentBlock, Message, ThinkingToken};

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig {
        preserve_thinking: false,
        ..yi_agent_core::AgentConfig::default()
    };
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, _turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![Message::assistant(vec![ContentBlock::Thinking {
            text: "r".into(),
            token: ThinkingToken::Signature("SIG-KEEP".into()),
        }])]);
    let before = agent.session().messages().to_vec();

    let (reply, answer) = oneshot::channel();
    agent = apply_session_command(
        agent,
        SessionCommand::SetPreserveThinking {
            preserve: true,
            reply,
        },
        &provider,
        &config,
        &store,
        "thread-set-preserve-2",
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    answer.await.unwrap().unwrap();

    assert!(agent.config().preserve_thinking);
    assert_eq!(
        agent.session().messages().to_vec(),
        before,
        "enabling preserve must not touch the session transcript"
    );
}

/// F1(评审修正 important):关闭 `preserve_thinking` 后**重开**会话,工厂必须
/// 从 `meta.preserve_thinking` 重建,而不是回落到默认 `true`。否则磁盘与所有
/// 客户端都说「已关闭」,重开的会话却仍在回传跨轮凭证——磁盘对运行态说谎,
/// 也违反 spec §7.4「会话重开后(往返)一致」。
///
/// 用 `production_factory` 驱动重开路径(它经 `rebuild_thread_agent_with_theme`,
/// 正是注入点所在);响应里回显的 `preserve_thinking` 与重建后的 agent 同源。
#[tokio::test(flavor = "multi_thread")]
async fn resume_honors_a_persisted_preserve_thinking_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let factory = production_factory(cfg.clone(), test_theme(), fixed_watcher(test_catalog()));
    let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 活着时关掉开关。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}","preserve":false}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
    // 确认确实落了盘(重开读的就是这份 meta)。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert!(!loaded.meta.preserve_thinking, "the flag must be persisted");

    // 重开:必须仍为 false(修 F1 前这里会回落 true)。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resumed = None;
    for _ in 0..24 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            resumed = Some(v);
            break;
        }
    }
    let resumed = resumed.expect("thread/resume must respond");
    assert_eq!(
        resumed["result"]["preserve_thinking"], false,
        "resume must honor the persisted switch, not fall back to the default: {resumed}"
    );
    h.shutdown().await;
}

/// F2(评审修正):切换开关必须**立即**落盘剥离后的会话状态。否则「关闭后、
/// 下一轮开始前」重开会话,日志最后一行仍是切换前那条带凭证的记录,凭证复活
/// (spec §7.4「关闭时落盘结果同样被剥离」)。
///
/// 直接驱动生产函数 `apply_session_command`(与 clear/compact 测试同款),并从
/// 真实 store 读回落盘副本断言其不含 `Thinking`。
#[tokio::test]
async fn toggling_preserve_off_persists_a_stripped_transcript() {
    use yi_agent_core::{ContentBlock, Message, ThinkingToken};

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, _turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());
    let tid = "thread-persist-toggle";
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: Default::default(),
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();

    let agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![
            Message::user("q"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "r".into(),
                    token: ThinkingToken::Signature("SIG-PERSIST".into()),
                },
                ContentBlock::Text("a".into()),
            ]),
        ]);

    let (reply, answer) = oneshot::channel();
    let agent = apply_session_command(
        agent,
        SessionCommand::SetPreserveThinking {
            preserve: false,
            reply,
        },
        &provider,
        &config,
        &store,
        tid,
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    answer.await.unwrap().unwrap();
    drop(agent);

    // 落盘副本里不得再有凭证(修 F2 前:切换不 append,盘上仍是旧行→此处失败)。
    let loaded = store
        .load(tid)
        .expect("load must not fail")
        .expect("the toggle must have appended");
    assert!(
        !loaded
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "the persisted transcript must be stripped once retention is off: {:?}",
        loaded.messages
    );
    // 正文必须原样留下(剥离只去凭证)。
    assert!(
        loaded
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Text(t) if t == "a")),
        "stripping must leave the neighbouring text block intact"
    );
}

/// F3(评审修正):关闭保留后**新一轮**的落盘也必須无凭证。`run_loop` 只剥请求
/// 副本,session 本体在关闭后仍可能持有当轮凭证;`persist_and_finish_turn` 必须
/// 剥落盘副本,否则每轮日志都追加带凭证的行,重开即复活。
///
/// 直接驱动生产函数 `persist_and_finish_turn`,用一个 `preserve_thinking=false`
/// 且 session 含凭证的 agent:落盘副本应无凭证,而 session 本体不变(它仍是
/// 展示/请求的权威)。
#[tokio::test]
async fn a_turn_while_off_persists_without_credentials() {
    use yi_agent_core::{ContentBlock, Message, ThinkingToken};

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig {
        preserve_thinking: false,
        ..yi_agent_core::AgentConfig::default()
    };
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, _turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());
    let tid = "thread-persist-turn";
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: Default::default(),
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: false,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();

    let agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![Message::assistant(vec![
            ContentBlock::Thinking {
                text: "r".into(),
                token: ThinkingToken::Signature("SIG-TURN".into()),
            },
            ContentBlock::Text("a".into()),
        ])]);

    persist_and_finish_turn(
        tid,
        None,
        None,
        &[],
        &[],
        None,
        &agent,
        Vec::new(),
        None,
        &store,
        &hub,
        &turn_tx,
        &provider,
        &config,
        &ThreadSession::new_status(),
    )
    .await;

    let loaded = store
        .load(tid)
        .expect("load must not fail")
        .expect("the turn must have been persisted");
    assert!(
        !loaded
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "a turn persisted while retention is off must carry no credentials: {:?}",
        loaded.messages
    );
    // session 本体不动:它仍是展示/请求的权威。
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "the persist-time strip must not mutate the live session"
    );
}

/// 零回归:默认 `true` 时,落盘副本必须与会话内容**逐字相同**(一条凭证都不少)。
#[tokio::test]
async fn a_turn_with_default_retention_persists_the_session_verbatim() {
    use yi_agent_core::{ContentBlock, Message, ThinkingToken};

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    assert!(config.preserve_thinking, "the default must be retention on");
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, _turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());
    let tid = "thread-persist-default";
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: Default::default(),
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();

    let agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    agent
        .session_handle()
        .lock()
        .unwrap()
        .replace_messages(vec![Message::assistant(vec![
            ContentBlock::Thinking {
                text: "r".into(),
                token: ThinkingToken::Signature("SIG-VERBATIM".into()),
            },
            ContentBlock::Text("a".into()),
        ])]);

    persist_and_finish_turn(
        tid,
        None,
        None,
        &[],
        &[],
        None,
        &agent,
        Vec::new(),
        None,
        &store,
        &hub,
        &turn_tx,
        &provider,
        &config,
        &ThreadSession::new_status(),
    )
    .await;

    let loaded = store
        .load(tid)
        .expect("load must not fail")
        .expect("the turn must have been persisted");
    assert_eq!(
        loaded.messages,
        agent.session().messages().to_vec(),
        "default retention must persist the session transcript verbatim"
    );
}
/// provider 构造失败时(如未授权的 provider 名):回执 `Err`,且**原 agent 原样保留**,
/// 绝不留下半重建状态(沿用既有重建的原子性)。
#[tokio::test]
async fn set_model_command_keeps_the_agent_when_the_provider_fails() {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, _turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    agent
        .session_handle()
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::user("keep-me"));
    let original_handle = agent.session_handle();

    let mut broken = crate::test_support::test_config();
    broken.provider = "not-a-provider".into();
    broken.model = "switched-model".into();

    let (reply, answer) = oneshot::channel();
    agent = apply_session_command(
        agent,
        SessionCommand::SetModel {
            effective: Box::new(broken),
            model_ref: Some("B".into()),
            reply,
        },
        &provider,
        &config,
        &store,
        "thread-set-model-fail",
        &hub,
        &turn_tx,
        &ThreadSession::new_status(),
    )
    .await;
    let error = answer
        .await
        .expect("the set-model reply channel must be fulfilled")
        .expect_err("an unknown provider must fail the rebuild");
    assert!(
        error.contains("not-a-provider"),
        "the error must name the offending provider: {error}"
    );
    assert_eq!(
        agent.config().model,
        config.model,
        "a failed switch must leave the old model in place"
    );
    assert!(
        Arc::ptr_eq(&original_handle, &agent.session_handle()),
        "a failed switch must leave the session Arc untouched"
    );
    assert_eq!(
        agent.session().messages().len(),
        1,
        "a failed switch must not disturb the session history"
    );
}

/// 空闲 driver(没有 turn 在跑)也必须在有限时间内响应 `SetModel`,且不把它弄停、
/// 不为无 turn 的命令发 Finished——与既有 clear/compact 空闲命令路径同款。
#[tokio::test(flavor = "multi_thread")]
async fn idle_driver_serves_a_set_model_command_without_a_new_turn() {
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r);
    let (hub, client) = test_hub(server_w);

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(8);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    // prompt_tx 保住不 drop:driver 应停在 idle,而不是因 prompt_rx 关闭退出。
    let _keep_prompt_tx = prompt_tx;

    let built = build_test_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();

    let handle = tokio::spawn(run_thread_driver(
        "thread-idle-setmodel".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(5),
        Arc::new(AtomicU64::new(0)),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    let mut effective = crate::test_support::test_config();
    effective.model = "switched-model".into();
    let (reply, answer) = oneshot::channel();
    session_tx
        .send(SessionCommand::SetModel {
            effective: Box::new(effective),
            model_ref: Some("B".into()),
            reply,
        })
        .await
        .unwrap();

    let result = tokio::time::timeout(EVENT_WAIT, answer)
        .await
        .expect("an idle driver must service SetModel without a new turn")
        .expect("the reply channel must be fulfilled");
    assert_eq!(result, Ok(()), "a valid cfg must rebuild successfully");

    // driver 仍在运行(命令不该把它弄停)。
    assert!(!handle.is_finished());
    handle.abort();
    assert!(
        turn_rx.try_recv().is_err(),
        "a command with no turn must not report a turn as finished"
    );
}

/// 一个 thread 的 driver 复用同一个 `Translator`,因此 item id 跨 turn 单调
/// 递增,不会出现两轮都用 `item-1` 的碰撞(回归 Fix #4)。
#[tokio::test(flavor = "multi_thread")]
async fn driver_uses_unique_item_ids_across_turns() {
    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let (hub, client) = test_hub(server_w);

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    let built = build_test_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();
    let handle = tokio::spawn(run_thread_driver(
        "thread-1".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(60),
        Arc::new(AtomicU64::new(1)),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    let mut client_r = BufReader::new(client_r);
    // 每轮取第一个 `item/started` 的 item id(agentMessage 的起始项)。
    let mut item_ids: Vec<String> = Vec::new();
    for turn in ["turn-1", "turn-2"] {
        prompt_tx
            .send(TurnPrompt {
                turn_id: turn.into(),
                prompt: "hi".into(),
                activate: None,
                attachments: Vec::new(),
                image_blocks: Vec::new(),
                image_refs: Vec::new(),
                display_text: "hi".into(),
                lead_request_id: None,
            })
            .await
            .unwrap();
        loop {
            let mut buf = String::new();
            let n = tokio::time::timeout(EVENT_WAIT, client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a notification")
                .unwrap();
            assert!(n > 0, "unexpected EOF while waiting for turn/completed");
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            let method = v["method"].as_str().unwrap_or("");
            if method == "item/started" {
                if let Some(id) = v["params"]["item"]["id"].as_str() {
                    item_ids.push(id.to_string());
                }
            }
            if method == "turn/completed" {
                break;
            }
        }
        let ev = tokio::time::timeout(EVENT_WAIT, turn_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { .. }));
    }

    assert!(
        item_ids.len() >= 2,
        "expected items from both turns: {item_ids:?}"
    );
    let mut unique = item_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        item_ids.len(),
        "item ids must be unique across turns: {item_ids:?}"
    );

    drop(prompt_tx);
    let _ = tokio::time::timeout(EVENT_WAIT, handle).await;
}

/// 第一次调用触发一个受权限管控的 bash 工具调用;第二次调用返回文本并结束。
struct PermissionMockProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl yi_agent_core::Provider for PermissionMockProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            vec![
                ProviderEvent::ToolUseStart {
                    id: "c1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "c1".into(),
                    partial_json: r#"{"cmd":"ls"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "c1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        } else {
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

struct FakeBash;

#[async_trait]
impl yi_agent_core::Tool for FakeBash {
    fn name(&self) -> &str {
        "bash"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "fake bash"
    }
    async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
        yi_agent_core::ToolResult::text("ran")
    }
}

/// 一个 provider：`call_stream` 先挂起很久（模拟"请求已发出、响应头还没回来"
/// 的窗口），然后才返回一个只会输出文本的流。用来测试「THINK 阶段在
/// `call_stream` 之内被中断」是否会被观察到。
struct SlowCallProvider;

/// 置位表示 `SlowCallProvider::call_stream` 已经进入（即请求已在途、尚未
/// 返回流）。测试据此把 Stop 精确地打进这个窗口，而不是打在它之前的
/// Check 1。
static SLOW_CALL_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[async_trait]
impl yi_agent_core::Provider for SlowCallProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        SLOW_CALL_ENTERED.store(true, Ordering::SeqCst);
        // 请求发出后、响应头到达前的等待：真实 provider 在这里等 HTTP。
        tokio::time::sleep(Duration::from_secs(30)).await;
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let events = vec![
            ProviderEvent::TextDelta("late".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn build_slow_call_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(SlowCallProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

/// 一个会跑很久的真实 `bash` 工具调用（`sleep 60`），用于在 ACT 阶段测试
/// 「切到 yolo 后 Stop 仍能杀掉正在执行的命令」。provider 只发一次工具调用，
/// 之后永不收尾，于是 turn 一直停在工具执行中。
struct LongToolProvider;
#[async_trait]
impl yi_agent_core::Provider for LongToolProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "c1".into(),
                partial_json: r#"{"command":"sleep 60"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "c1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

/// 真 bash + yolo 时沙箱放开；工具调用会真的执行 `sleep 60`。
fn build_long_tool_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(LongToolProvider);
    // 权限层直接放行（yolo 打开），不必走审批，直接进入 ACT。
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(true),
        std::path::PathBuf::from("/tmp"),
        Arc::new(|_cmd: &str| None),
    ));
    let registry =
        yi_agent_runtime::bootstrap::build_tool_setup(&crate::test_support::test_config(), false)
            .unwrap();
    let config = yi_agent_core::AgentConfig::default();
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::clone(&registry.tools),
        config.clone(),
    )
    .with_permission(checker.clone(), rx_arc.clone());
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: Some(decision_tx),
        decision_rx: Some(rx_arc),
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(true),
        process_manager: Arc::clone(&registry.process_manager),
    })
}

/// 构造一个会触发 bash 审批的 agent,并把决定通道交给 driver。
fn build_permission_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(PermissionMockProvider {
        calls: AtomicUsize::new(0),
    });
    let mut registry = yi_agent_core::ToolRegistry::new();
    registry.register(Arc::new(FakeBash));
    let config = yi_agent_core::AgentConfig::default();
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(false),
        std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        Arc::new(|_cmd: &str| None),
    ));
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
        .with_permission(checker.clone(), rx_arc.clone());
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: Some(decision_tx),
        decision_rx: Some(rx_arc),
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

/// Test-only ws entry: same multi-client server as production, but every
/// thread is built by `build_permission_agent` so a `turn/start` triggers a
/// real approval round trip. `ws.rs`'s two-client E2E needs this seam.
pub(crate) async fn serve_ws_with_permission_agent(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
) -> anyhow::Result<()> {
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    crate::ws::serve_ws_inner(
        listener,
        cfg,
        workspaces,
        pairing,
        theme,
        build_permission_agent,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_request_round_trip_allows_tool() {
    let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    let mut saw_request = false;
    let mut saw_tool_started = false;
    let mut perm_id: Option<String> = None;
    for _ in 0..20 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("item/toolCall/requestApproval") => {
                assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                assert_eq!(v["params"]["tool_name"], "bash", "reverse params: {v}");
                perm_id = v["id"].as_str().map(|s| s.to_string());
                saw_request = true;
                break;
            }
            Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                saw_tool_started = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_request,
        "expected an item/toolCall/requestApproval request"
    );
    let perm_id = perm_id.expect("reverse request must carry a string id");

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":"{perm_id}","result":{{"decision":"allow_once"}}}}"#
    ))
    .await;

    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..20 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                saw_tool_started = true;
            }
            Some("turn/completed") => {
                completed = Some(v);
                break;
            }
            _ => {}
        }
    }
    let completed = completed.expect("expected a turn/completed notification");
    assert_eq!(completed["params"]["status"], "completed");
    assert!(
        saw_tool_started,
        "an allowed tool must emit an item/started toolCall item"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_timeout_defaults_to_deny() {
    let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    let mut saw_request = false;
    let mut saw_tool_started = false;
    for _ in 0..20 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("item/toolCall/requestApproval") => {
                assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                saw_request = true;
                break;
            }
            Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                saw_tool_started = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_request,
        "expected an item/toolCall/requestApproval request"
    );

    // 不回复:driver 应在超时后按 Deny 处理,turn 仍能正常完成。
    let mut completed: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                saw_tool_started = true;
            }
            Some("turn/completed") => {
                completed = Some(v);
                break;
            }
            _ => {}
        }
    }
    let completed = completed.expect("expected a turn/completed notification");
    assert_eq!(
        completed["params"]["status"], "completed",
        "the agent must not hang after a permission timeout"
    );
    assert!(
        !saw_tool_started,
        "a denied tool must not emit an item/started toolCall item"
    );
    h.shutdown().await;
}

/// 读到下一个 `item/toolCall/requestApproval` 通知,返回其 id。
async fn read_until_approval(h: &mut Harness) -> String {
    for _ in 0..20 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/toolCall/requestApproval") {
            return v["id"]
                .as_str()
                .expect("approval id must be a string")
                .to_string();
        }
    }
    panic!("expected an item/toolCall/requestApproval notification");
}

/// 读帧直到出现 `id == want` 的响应,返回其 `result.thread_id`。
async fn read_thread_start_response(h: &mut Harness, want: u64) -> String {
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(want)) {
            return v["result"]["thread_id"]
                .as_str()
                .expect("thread/start response must carry thread_id")
                .to_string();
        }
    }
    panic!("no thread/start response for id {want}");
}

/// 两个并发 thread 的审批 id 必须不同:否则一个 thread 的 Allow 会被
/// 应用到另一个 thread 的工具上(跨 thread 授权错配)。
#[tokio::test(flavor = "multi_thread")]
async fn permission_request_ids_are_unique_across_threads() {
    let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // thread A。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid_a = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid_a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let id_a = read_until_approval(&mut h).await;

    // thread B(其 PermissionChecker 的 request_id 也从 1 开始)。
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
        .await;
    let tid_b = read_thread_start_response(&mut h, 4).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid_b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let id_b = read_until_approval(&mut h).await;

    assert_ne!(id_a, id_b, "approval ids must be unique across threads");
    h.shutdown().await;
}

/// 上一轮残留的中断信号不得取消当前 turn 的审批等待(同 `e6068b0` 的
/// bug 类:中断必须按目标 turn id 过滤)。
#[tokio::test(flavor = "multi_thread")]
async fn stale_interrupt_does_not_cancel_turn_during_approval() {
    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let (hub, client) = test_hub(server_w);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let perm_seq = Arc::new(AtomicU64::new(1));

    let store_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

    let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    let built = build_permission_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();
    let handle = tokio::spawn(run_thread_driver(
        "thread-1".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        client,
        turn_tx,
        built.decision_tx,
        Arc::clone(&pending),
        Duration::from_secs(60),
        Arc::clone(&perm_seq),
        None,
        store,
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    prompt_tx
        .send(TurnPrompt {
            turn_id: "turn-1".into(),
            prompt: "hi".into(),
            activate: None,
            attachments: Vec::new(),
            image_blocks: Vec::new(),
            image_refs: Vec::new(),
            display_text: "hi".into(),
            lead_request_id: None,
        })
        .await
        .unwrap();

    // 读到反向请求 id(此时 driver 已进入内层审批等待)。
    let mut client_r = BufReader::new(client_r);
    let mut approval_id = None;
    for _ in 0..20 {
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, client_r.read_line(&mut buf))
            .await
            .expect("timed out")
            .unwrap();
        if n == 0 {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        if v["method"] == "item/toolCall/requestApproval" {
            approval_id = Some(v["id"].as_str().unwrap().to_string());
            break;
        }
    }
    let approval_id = approval_id.expect("expected a reverse request");

    // 现在投递一个属于上一轮(turn-0)的残留中断:它必须被忽略。
    interrupt_tx.try_send("turn-0".into()).unwrap();

    // 给 driver 足够时间处理该信号;若残留中断未被过滤,它会在此刻
    // 取走 pending 项并取消本轮。
    tokio::time::sleep(Duration::from_millis(80)).await;

    // 用正确的决定回应;若 pending 项已被残留中断移除,这里为 None → 断言失败。
    let sender = pending
        .lock()
        .await
        .remove(&approval_id)
        .expect("pending approval must survive a stale interrupt");
    sender.send(Decision::AllowOnce).unwrap();

    let ev = tokio::time::timeout(EVENT_WAIT, turn_rx.recv())
        .await
        .expect("turn must finish")
        .unwrap();
    assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

    drop(prompt_tx);
    let _ = tokio::time::timeout(EVENT_WAIT, handle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_start_allocates_uuid_id_and_writes_meta() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);

    let tid = start_thread(&mut h).await;
    let uuid_part = tid
        .strip_prefix("thread-")
        .unwrap_or_else(|| panic!("expected thread-<uuid>, got {tid}"));
    assert!(
        uuid_part.parse::<uuid::Uuid>().is_ok(),
        "suffix must be a uuid: {tid}"
    );

    // 通过 store API 读取,锁住落盘内容(不依赖文件布局细节)。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread/start must persist meta");
    assert_eq!(loaded.meta.thread_id, tid);
    assert_eq!(loaded.meta.model, "test-model");
    assert_eq!(loaded.meta.cwd, dir.path().display().to_string());
    assert!(loaded.meta.title.is_none(), "title starts empty");
    assert!(loaded.items.is_empty(), "no turns yet");

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_list_empty_returns_empty_array() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/list","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["threads"], serde_json::json!([]));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_list_returns_created_threads() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid_a = read_thread_start_response(&mut h, 2).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
        .await;
    let tid_b = read_thread_start_response(&mut h, 3).await;

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
        .await;
    let mut listed = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            listed = Some(v);
            break;
        }
    }
    let v = listed.expect("thread/list must respond");
    let threads = v["result"]["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    let ids: std::collections::HashSet<&str> = threads
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(tid_a.as_str()) && ids.contains(tid_b.as_str()));
    // 两次 start 可能同毫秒,顺序不定(排序由 thread_store 单测锁定);
    // 此处只断言每个条目都带时间戳字段。
    for t in threads {
        assert!(t["created_at"].is_number());
        assert!(t["updated_at"].is_number());
    }
    h.shutdown().await;
}

/// Task 11:`thread/list` / `thread/listAll` 的条目必须显式带出
/// `permission_mode`(wire 契约与存储解耦,但字段必须存在且为 lowercase)。
#[tokio::test(flavor = "multi_thread")]
async fn list_exposes_permission_mode() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid_normal = read_thread_start_response(&mut h, 2).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
        .await;
    let tid_yolo = read_thread_start_response(&mut h, 3).await;

    // 把第二个线程切成 yolo。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid_yolo}","mode":"yolo"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert!(
        v.get("error").is_none(),
        "setPermissionMode must succeed: {v}"
    );

    // thread/list:每个条目带 lowercase permission_mode。
    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/list","params":{}}"#)
        .await;
    let mut listed = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            listed = Some(v);
            break;
        }
    }
    let v = listed.expect("thread/list must respond");
    let threads = v["result"]["threads"].as_array().unwrap();
    let mode_of = |tid: &str| -> serde_json::Value {
        threads
            .iter()
            .find(|t| t["thread_id"].as_str() == Some(tid))
            .unwrap_or_else(|| panic!("thread {tid} missing from thread/list: {v}"))["permission_mode"]
            .clone()
    };
    assert_eq!(mode_of(&tid_normal), serde_json::json!("normal"));
    assert_eq!(mode_of(&tid_yolo), serde_json::json!("yolo"));

    // thread/listAll:分组内的条目同样带该字段。
    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
        .await;
    let mut listed = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(6)) {
            listed = Some(v);
            break;
        }
    }
    let v = listed.expect("thread/listAll must respond");
    let all_threads: Vec<&serde_json::Value> = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap().iter())
        .collect();
    let mode_of_all = |tid: &str| -> serde_json::Value {
        all_threads
            .iter()
            .find(|t| t["thread_id"].as_str() == Some(tid))
            .unwrap_or_else(|| panic!("thread {tid} missing from thread/listAll: {v}"))["permission_mode"]
            .clone()
    };
    assert_eq!(mode_of_all(&tid_normal), serde_json::json!("normal"));
    assert_eq!(mode_of_all(&tid_yolo), serde_json::json!("yolo"));

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_start_with_cwd_writes_meta_cwd() {
    // 全局 workdir 与显式 cwd 指向不同目录:用于区分「按 thread 的 cwd 路由」
    // 与旧的「一律写全局 workdir」行为。
    let global = tempfile::TempDir::new().unwrap();
    let target = tempfile::TempDir::new().unwrap();
    let w = global.path().canonicalize().unwrap();
    let c = target.path().canonicalize().unwrap();

    let mut cfg = test_config();
    cfg.workdir = w.clone();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": c.to_string_lossy() }});
    h.send(&req.to_string()).await;

    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("thread/start response");
    assert_eq!(
        resp["result"]["cwd"].as_str().unwrap(),
        c.to_string_lossy().to_string()
    );
    let thread_id = resp["result"]["thread_id"].as_str().unwrap().to_string();

    let store = crate::thread_store::ThreadStore::new(&c);
    let metas = store.list().unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].cwd, c.to_string_lossy().to_string());
    assert!(
        !crate::thread_store::ThreadStore::new(&w).exists(&thread_id),
        "thread must not leak into the global workdir store"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_start_with_bad_cwd_returns_invalid_params() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": "/nonexistent/definitely/not/here" }});
    h.send(&req.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}

/// 索引键必须规范化为 canonical,与 `workspace/remove` 的规范化对齐:否则符号
/// 链接形式的缺省 cwd 会以非 canonical 形式进索引,后续 remove 静默 no-op。
/// 用符号链接制造 `canonical != raw`,确保本测试确实覆盖该回归面。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_default_cwd_indexes_canonical_path() {
    let real = tempfile::TempDir::new().unwrap();
    let link_parent = tempfile::TempDir::new().unwrap();
    let link = link_parent.path().join("linked");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();

    let raw = link.to_string_lossy().to_string();
    let canonical = link.canonicalize().unwrap();
    assert_ne!(
        raw,
        canonical.to_string_lossy().to_string(),
        "symlink must be non-canonical for this test to be meaningful"
    );

    let mut cfg = test_config();
    cfg.workdir = link.clone();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // 不带 cwd → 走缺省 cfg.workdir(即符号链接路径)。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let _tid = read_thread_start_response(&mut h, 2).await;

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
        .await;
    let mut listed = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            listed = Some(v);
            break;
        }
    }
    let v = listed.expect("workspace/list must respond");
    let path = v["result"]["workspaces"][0]["path"]
        .as_str()
        .expect("index must contain the default cwd");
    let recanon = Path::new(path)
        .canonicalize()
        .expect("indexed path must be canonicalizable");
    assert_eq!(
        recanon.to_string_lossy(),
        path,
        "index entry must already be canonical: {path}"
    );
    h.shutdown().await;
}

/// Design §11 的头号风险是「cwd 传递正确」:agent 工厂必须拿到该 thread 的 cwd,
/// 而非全局 workdir。用记录型工厂把实参钉死。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_factory_receives_thread_cwd() {
    let target = tempfile::TempDir::new().unwrap();
    let c = target.path().canonicalize().unwrap();

    let seen: Arc<std::sync::Mutex<Vec<std::path::PathBuf>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_factory = Arc::clone(&seen);
    let build = move |session: Option<yi_agent_core::Session>,
                      cwd: &std::path::Path,
                      mode: crate::thread_store::ThreadMode| {
        seen_factory.lock().unwrap().push(cwd.to_path_buf());
        build_test_agent(session, cwd, mode)
    };
    let mut h = Harness::with_factory(build, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": c.to_string_lossy() }});
    h.send(&req.to_string()).await;
    let _tid = read_thread_start_response(&mut h, 2).await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![c.clone()],
        "agent factory must receive the thread's canonical cwd"
    );
    h.shutdown().await;
}

/// 两个目录各起一个 thread:`thread/listAll` 必须按 workspace 分成两组,每组
/// 恰好含该目录的 thread(cwd 与 group.workspace 一致,thread_id 与 start 响应一致)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_list_all_groups_by_workspace() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let a = dir_a
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let b = dir_b
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let mut h = Harness::new();
    initialize(&mut h).await;

    // workspace 路径 → 在该目录 start 出的 thread_id。
    let mut expected: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (id, path) in [(2u64, &a), (3u64, &b)] {
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start",
            "params":{"cwd": path}});
        h.send(&req.to_string()).await;
        let tid = read_thread_start_response(&mut h, id).await;
        expected.insert(path.clone(), tid);
    }

    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
        .await;
    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("thread/listAll response");
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "两个目录 → 两个分组: {v}");

    // 索引按「最近使用在前」排序:先 start a 再 start b → 组顺序为 [b, a]。
    // start 请求串行(读罢 a 的响应才发 b),故 workspace/add 的先后确定。
    assert_eq!(
        groups[0]["workspace"],
        b.as_str(),
        "most-recent workspace must come first"
    );
    assert_eq!(
        groups[1]["workspace"],
        a.as_str(),
        "the earlier workspace must follow"
    );

    for g in groups {
        let ws = g["workspace"].as_str().unwrap();
        assert!(
            ws == a.as_str() || ws == b.as_str(),
            "unexpected workspace {ws}"
        );
        assert_eq!(g["exists"], true, "canonical tempdir must exist: {g}");
        let threads = g["threads"].as_array().unwrap();
        assert_eq!(
            threads.len(),
            1,
            "each workspace holds exactly one thread: {g}"
        );
        assert_eq!(
            threads[0]["cwd"].as_str().unwrap(),
            ws,
            "thread cwd must match its group workspace"
        );
        assert_eq!(
            threads[0]["thread_id"].as_str().unwrap(),
            expected[ws].as_str(),
            "group must carry the thread started in that workspace"
        );
    }
    h.shutdown().await;
}

/// 失效目录(索引里有、磁盘上已删)仍必须出现为一个组:`exists:false` 且
/// `threads` 为空(不深扫)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_list_all_includes_missing_dir_with_empty_threads() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let path_str = path.to_string_lossy().to_string();

    let mut h = Harness::new();
    initialize(&mut h).await;

    // 先让目录存在时写入索引,再从磁盘删除。
    let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
        "params":{"path": path_str}});
    h.send(&add.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert!(v.get("error").is_none(), "workspace/add must succeed: {v}");
    std::fs::remove_dir_all(&path).unwrap();

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
        .await;
    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("thread/listAll response");
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "索引里恰有一个目录: {v}");
    let g = &groups[0];
    assert_eq!(g["workspace"].as_str().unwrap(), path_str);
    assert_eq!(g["exists"], false, "失效目录必须报 exists:false: {g}");
    assert_eq!(
        g["threads"].as_array().unwrap().len(),
        0,
        "失效目录不深扫,threads 必须为空: {g}"
    );
    h.shutdown().await;
}

async fn list_all(h: &mut Harness) -> serde_json::Value {
    h.send(r#"{"jsonrpc":"2.0","id":901,"method":"thread/listAll","params":{}}"#)
        .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(901)) {
            return v;
        }
    }
    panic!("thread/listAll must respond");
}

/// 从 `thread/listAll` 响应里取某 thread 的 `content_epoch`。
fn content_epoch_of(list: &serde_json::Value, thread_id: &str) -> u64 {
    let row = find_listed(list, thread_id);
    row["content_epoch"]
        .as_u64()
        .unwrap_or_else(|| panic!("content_epoch must be present: {row}"))
}

/// 从 `thread/listAll` 响应里取某 thread 的 `last_item_id`（缺省/`null` → None）。
fn last_item_id_of(list: &serde_json::Value, thread_id: &str) -> Option<String> {
    find_listed(list, thread_id)["last_item_id"]
        .as_str()
        .map(str::to_string)
}

/// 在 groups 与 pinned 里查找某 thread 的 summary（两处都可能出现）。
fn find_listed<'a>(list: &'a serde_json::Value, thread_id: &str) -> &'a serde_json::Value {
    let in_groups = list["result"]["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["threads"].as_array().into_iter().flatten());
    let in_pinned = list["result"]["pinned"].as_array().into_iter().flatten();
    in_groups
        .chain(in_pinned)
        .find(|t| t["thread_id"] == serde_json::json!(thread_id))
        .unwrap_or_else(|| panic!("thread {thread_id} must be listed: {list}"))
}

/// 列表必须携带版本字段，客户端才能自判「我是否落后」而不必逐个读日志。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_carries_content_epoch_and_last_item_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let listed = list_all(&mut h).await;
    assert_eq!(
        content_epoch_of(&listed, &tid),
        0,
        "a fresh thread must be listed at epoch 0: {listed}"
    );
    assert_eq!(
        last_item_id_of(&listed, &tid),
        None,
        "a fresh thread has no items yet: {listed}"
    );

    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;

    let listed = list_all(&mut h).await;
    assert!(
        last_item_id_of(&listed, &tid).is_some(),
        "a persisted turn must record last_item_id: {listed}"
    );
    h.shutdown().await;
}

async fn add_workspace(h: &mut Harness, id: u64, path: &str) {
    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"workspace/add",
        "params":{"path": path}})
        .to_string(),
    )
    .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(id)) {
            return;
        }
    }
    panic!("workspace/add must respond");
}

fn write_meta(
    dir: &Path,
    id: &str,
    title: &str,
    driver_project: Option<&str>,
    driver_ref: Option<&str>,
) {
    let meta = crate::thread_store::ThreadMeta {
        thread_id: id.into(),
        cwd: dir.to_string_lossy().into(),
        model: "m".into(),
        created_at: 0,
        updated_at: 0,
        title: Some(title.into()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        driver_project: driver_project.map(str::to_string),
        driver_ref: driver_ref.map(str::to_string),
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        content_epoch: 0,
        last_item_id: None,
        extra_roots: Vec::new(),
        role: None,
    };
    crate::thread_store::ThreadStore::new(dir)
        .create(&meta)
        .unwrap();
}

/// 手工落一份 `role:"lead"` 的 meta(复刻真机里误落盘的幽灵组长)。
fn write_lead_meta(dir: &Path, id: &str) {
    let meta = crate::thread_store::ThreadMeta {
        thread_id: id.into(),
        cwd: dir.to_string_lossy().into(),
        model: String::new(),
        created_at: 0,
        updated_at: 0,
        title: Some("组长".into()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        content_epoch: 0,
        last_item_id: None,
        extra_roots: Vec::new(),
        role: Some(crate::thread_store::LEAD_ROLE.to_string()),
    };
    crate::thread_store::ThreadStore::new(dir)
        .create(&meta)
        .unwrap();
}

/// 建一个真 git 仓库（main 分支 + 一次提交），返回 canonical 路径。
fn git_project(dir: &Path) -> PathBuf {
    let run = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    };
    std::fs::create_dir_all(dir).unwrap();
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    // 复刻真实项目的 .gitignore：宿主运行态 `.yi-agent/` 本就被忽略（见仓库根
    // .gitignore）。少了它，`write_meta` 写下的 `<cwd>/.yi-agent/…` 在 worktree
    // 内是 untracked——git 会判"脏"，`worktree remove` 也会被拒。这不是测试造作，
    // 而是生产里同样被忽略：不补它，"干净且已并入 → 静默删"那条根本走不到。
    std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
    std::fs::write(dir.join("f.txt"), "hi").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
    dir.canonicalize().unwrap()
}

/// 在 `<project>/.worktrees/kanban/<slug>` 建真 worktree，返回其 canonical 路径。
fn git_kanban_worktree(project: &Path, card_id: &str) -> PathBuf {
    let slug = crate::worktree_reclaim::slugify(card_id);
    let path = project.join(".worktrees/kanban").join(&slug);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["worktree", "add", "-q"])
            .arg(&path)
            .args(["-b", &format!("kanban/{slug}")])
            .status()
            .unwrap()
            .success()
    );
    path.canonicalize().unwrap()
}

/// 驱动会话（cwd 在 worktree、driver_project=项目）删除：干净且已并入 → 会话与 worktree 一并消失。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_card_session_also_removes_its_worktree() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&project_dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");
    write_meta(
        &wt,
        "t-card",
        "看板 · card-1",
        Some(&project.to_string_lossy()),
        Some("card-1"),
    );

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &wt.to_string_lossy()).await;

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert!(v.get("error").is_none(), "delete must succeed: {v}");
    // 空成功体是契约：无损静默删除不新增任何成功字段（spec §4.4 实现修正注里
    // 唯一的例外是"该删没删成"，此用例是真删成功，故必须恰好为空）。
    assert_eq!(v["result"], serde_json::json!({}), "{v}");
    assert!(
        !wt.join(".yi-agent/threads/t-card.meta.json").exists(),
        "session gone"
    );
    assert!(!wt.exists(), "clean+merged worktree must be removed");
    h.shutdown().await;
}

/// 脏 worktree：未 force 返回 needs_confirmation 且 worktree.action="remove"；会话仍在。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_card_session_with_dirty_worktree_needs_confirmation() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&project_dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");
    std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
    write_meta(
        &wt,
        "t-card",
        "看板 · card-1",
        Some(&project.to_string_lossy()),
        Some("card-1"),
    );

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &wt.to_string_lossy()).await;

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["result"]["status"], "needs_confirmation", "{v}");
    assert_eq!(v["result"]["worktree"]["action"], "remove", "{v}");
    assert!(
        v["result"]["worktree"]["reason"]
            .as_str()
            .unwrap()
            .contains("未提交"),
        "{v}"
    );
    assert!(
        wt.join(".yi-agent/threads/t-card.meta.json").exists(),
        "session untouched"
    );

    // force 重发 → 一并删除。
    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/delete","params":{"threadId":"t-card","force":true}}"#)
        .await;
    let v = read_response(&mut h, 6).await;
    assert!(v.get("error").is_none(), "forced delete succeeds: {v}");
    assert!(!wt.exists(), "worktree destroyed after confirmation");
    h.shutdown().await;
}

/// 普通会话：响应不含 worktree 字段，cwd 目录原样存在（D7 的钉子）。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_plain_session_leaves_its_directory_alone() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    write_meta(&cwd, "t-plain", "plain", None, None);
    let marker = cwd.join("keep-me.txt");
    std::fs::write(&marker, "x").unwrap();

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &cwd.to_string_lossy()).await;

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-plain"}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert!(v.get("error").is_none(), "{v}");
    assert!(
        v["result"].get("worktree").is_none(),
        "no worktree field for plain sessions: {v}"
    );
    // 比"无 worktree 字段"更强：普通会话的成功体必须**恰好**是空对象。
    assert_eq!(v["result"], serde_json::json!({}), "{v}");
    assert!(
        marker.exists(),
        "the user's own directory must be untouched"
    );
    h.shutdown().await;
}

/// 防呆：cwd 在项目根而非看板 worktree 路径 → 即便带 card_id 也硬保留。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_card_session_outside_the_kanban_dir_keeps_the_directory() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&project_dir.path().join("proj"));
    // cwd = 项目根（不是 .worktrees/kanban/... 下的 worktree）。
    write_meta(
        &project,
        "t-card",
        "看板 · odd",
        Some(&project.to_string_lossy()),
        Some("card-1"),
    );
    let marker = project.join("keep-me.txt");
    std::fs::write(&marker, "x").unwrap();

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &project.to_string_lossy()).await;

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card"}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["result"]["worktree"]["action"], "keep", "{v}");
    // force 也不删。
    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/delete","params":{"threadId":"t-card","force":true}}"#)
        .await;
    let _ = read_response(&mut h, 6).await;
    assert!(
        marker.exists() && project.join("f.txt").exists(),
        "hard-refused: dir intact"
    );
    h.shutdown().await;
}

/// 契约（D9 两半）：worktree 删除失败必须如实回报，且不得阻断会话删除。
///
/// 用户刚被告知 worktree 会被删掉；此刻会话文件已摘除、在应用内无法再删一次，
/// 而按 D10 卡片上的 thread_id 也已悬空——若删除默默失败，worktree 就无声滞留、
/// 用户永远看不见。故 force 路径下"该删却没删成"必须在响应里现身
/// （action=remove、removed=false、非空 reason/path），同时会话删除本身照常完成。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_card_session_reports_a_failed_worktree_removal() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&project_dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");
    // 未提交文件 → 判定为 DestructiveDelete，必须 force 才走到真删。
    std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
    // 锁定 worktree：即便 `--force` 也删不掉（git 报
    // "cannot remove a locked working tree"），从而确定性复现"删除失败"这条路径，
    // 无需借助权限/IO 造错。
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["worktree", "lock"])
            .arg(&wt)
            .status()
            .unwrap()
            .success(),
        "git worktree lock must succeed"
    );
    write_meta(
        &wt,
        "t-card",
        "看板 · card-1",
        Some(&project.to_string_lossy()),
        Some("card-1"),
    );

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &wt.to_string_lossy()).await;

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{"threadId":"t-card","force":true}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert!(v.get("error").is_none(), "delete must still succeed: {v}");
    assert_eq!(v["result"]["worktree"]["action"], "remove", "{v}");
    assert_eq!(v["result"]["worktree"]["removed"], false, "{v}");
    assert!(
        !v["result"]["worktree"]["reason"]
            .as_str()
            .unwrap()
            .is_empty(),
        "the failure reason must be visible: {v}"
    );
    assert!(
        !v["result"]["worktree"]["path"].as_str().unwrap().is_empty(),
        "the stranded worktree path must be named: {v}"
    );
    assert!(wt.exists(), "a locked worktree survives the failed removal");
    assert!(
        !wt.join(".yi-agent/threads/t-card.meta.json").exists(),
        "session deletion must complete despite the worktree failure"
    );
    h.shutdown().await;
}

/// 驱动会话(在 worktree、driver_project=项目) 折进项目组；worktree 不再顶层成组。
#[tokio::test(flavor = "multi_thread")]
async fn a_board_thread_folds_into_its_project_group() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let worktree_dir = tempfile::TempDir::new().unwrap();
    let project = project_dir.path().canonicalize().unwrap();
    let worktree = worktree_dir.path().canonicalize().unwrap();
    let p = project.to_string_lossy().to_string();
    let w = worktree.to_string_lossy().to_string();

    write_meta(&project, "t-plain", "plain", None, None);
    write_meta(
        &worktree,
        "t-card",
        "看板 · card-1",
        Some(&p),
        Some("card-1"),
    );

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &w).await; // worktree 进索引(=卡片会话落盘后的样子)
    add_workspace(&mut h, 12, &p).await;

    let v = list_all(&mut h).await;
    let groups = v["result"]["groups"].as_array().unwrap();
    let ws: Vec<&str> = groups
        .iter()
        .map(|g| g["workspace"].as_str().unwrap())
        .collect();
    assert!(
        ws.contains(&p.as_str()),
        "the project group must exist: {ws:?}"
    );
    assert!(
        !ws.contains(&w.as_str()),
        "the worktree must not be a top-level group: {ws:?}"
    );

    let project_group = groups
        .iter()
        .find(|g| g["workspace"] == p.as_str())
        .unwrap();
    let ids: Vec<&str> = project_group["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"t-plain"), "own thread stays: {ids:?}");
    assert!(ids.contains(&"t-card"), "card thread folds in: {ids:?}");
    let card = project_group["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["thread_id"] == "t-card")
        .unwrap();
    assert_eq!(card["driver_ref"], "card-1", "wire must expose driver_ref");
    assert_eq!(card["driver_project"], p.as_str());
    h.shutdown().await;
}

/// 零回归:一个普通目录的会话仍按目录成组,顺序/存在性与今天一致。
#[tokio::test(flavor = "multi_thread")]
async fn plain_workspaces_still_group_by_directory() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let a = dir_a
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let b = dir_b
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    write_meta(Path::new(&a), "t-a", "a", None, None);
    write_meta(Path::new(&b), "t-b", "b", None, None);

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 21, &a).await;
    add_workspace(&mut h, 22, &b).await;

    let v = list_all(&mut h).await;
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "two plain dirs → two groups: {v}");
    assert_eq!(groups[0]["workspace"], b.as_str(), "most-recent first");
    assert_eq!(groups[1]["workspace"], a.as_str());
    h.shutdown().await;
}

/// 建一个 workdir 隔离的 harness,返回 (harness, 该 workdir 的持有者)。
///
/// `Harness::new()` 的 workdir 是**固定的共享路径**
/// (`/tmp/yi-agent-app-server-test`),多个测试共用会互相污染、还会往真实路径
/// 写 meta。持有者必须在测试结束前存活,否则 tempdir 被删、meta 无处可写。
async fn isolated_harness() -> (Harness, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = crate::test_support::test_config();
    cfg.workdir = dir.path().to_path_buf();
    (Harness::with_cfg(cfg).await, dir)
}

/// `thread/lead` 必须幂等:同一 cwd 反复调用只造一个组长,第二次 `created=false`。
#[tokio::test(flavor = "multi_thread")]
async fn thread_lead_is_idempotent_per_project() {
    let (mut h, _dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;

    let first = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
    let first_id = first["result"]["thread_id"]
        .as_str()
        .expect("thread/lead must answer with a thread_id")
        .to_string();
    assert_eq!(first["result"]["created"], serde_json::json!(true));

    let second = rpc(&mut h, 3, "thread/lead", serde_json::json!({})).await;
    assert_eq!(second["result"]["thread_id"].as_str().unwrap(), first_id);
    assert_eq!(second["result"]["created"], serde_json::json!(false));

    // 落盘上只有一个组长 meta——「至多一个组长」是落盘事实,不只是返回值口径。
    let leads: Vec<String> = crate::thread_store::ThreadStore::new(_dir.path())
        .list()
        .unwrap()
        .into_iter()
        .filter(|m| m.role.as_deref() == Some(crate::thread_store::LEAD_ROLE))
        .map(|m| m.thread_id)
        .collect();
    assert_eq!(leads, vec![first_id], "exactly one lead meta on disk");

    h.shutdown().await;
}

/// 组长必须出现在 `thread/listAll` 的目录分组里,否则侧栏点不进去。
#[tokio::test(flavor = "multi_thread")]
async fn the_lead_appears_in_list_all() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
    let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();
    // `listAll` 只列举全局目录索引里的目录:把隔离 workdir 加进去,等于
    // 「用户打开过这个项目」。不加的话它根本不在 `groups` 里,与懒创建无关。
    add_workspace(&mut h, 3, &dir.path().to_string_lossy()).await;

    let all = list_all(&mut h).await;
    let found = all["result"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
        .any(|t| t["thread_id"].as_str() == Some(lead_id.as_str()));
    assert!(found, "lead thread must be listed: {all}");

    h.shutdown().await;
}

/// 投递到组长信箱的消息必须转成组长 thread 的一次输入;组长尚未被打开时
/// 也必须能被拉起(driver 按需建)。
#[tokio::test(flavor = "multi_thread")]
async fn a_delivered_message_reaches_the_lead_thread() {
    let (mut h, _dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
    let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

    // 测试专用入口直接投递,避免依赖真实模型。
    Harness::deliver_lead_message_for_test(&h, _dir.path(), "thread-sender-1", "需要你裁决 X");

    // 组长 thread 的 `.jsonl` 必须出现那条消息(以用户消息形式注入)。
    // 断言直接钉在**主日志**上:`ThreadStore::load` 在崩溃残留时也会采纳
    // checkpoint,只看 `load` 的结果无法区分「已 append」与「只写了
    // partial」,而本任务要求的正是那条 append。
    let log = _dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{lead_id}.jsonl"));
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    loop {
        let appended = std::fs::read_to_string(&log)
            .map(|text| text.contains("需要你裁决 X"))
            .unwrap_or(false);
        // `_dir` 持有隔离 workdir;它就在本测试的栈上,直接复用它。
        let store = crate::thread_store::ThreadStore::new(_dir.path());
        let loaded = store.load(&lead_id).ok().flatten().is_some_and(|loaded| {
            loaded.messages.iter().any(|m| {
                m.content.iter().any(|b| match b {
                    yi_agent_core::ContentBlock::Text(t) => t.contains("需要你裁决 X"),
                    _ => false,
                })
            })
        });
        if appended && loaded {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "the delivered message never reached the lead thread (appended={appended}, loaded={loaded})"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // 组长被拉起时,`start_thread_core` 不得把它落盘的 `role` 抹掉。
    let meta = crate::thread_store::ThreadStore::new(_dir.path())
        .load(&lead_id)
        .unwrap()
        .expect("the lead thread must have been persisted")
        .meta;
    assert_eq!(
        meta.role.as_deref(),
        Some(crate::thread_store::LEAD_ROLE),
        "the lead's role must survive being opened"
    );

    h.shutdown().await;
}

/// 造一个只够后台 tick 用的会话:`store` 指向 `project`(分组键由它算出),
/// `status` 由调用方给定。其余字段(通道、进程管理器)只为把 struct 填满,
/// tick 一个都不碰。
fn tick_test_session(project: &Path, thread_id: &str, status: ThreadStatus) -> ThreadSession {
    let (prompt_tx, _prompt_rx) = mpsc::channel(1);
    let (interrupt_tx, _interrupt_rx) = mpsc::channel(1);
    let (interject_tx, _interject_rx) = mpsc::channel(1);
    let (session_tx, _session_rx) = mpsc::channel(1);
    ThreadSession {
        thread_id: thread_id.to_string(),
        cwd: project.display().to_string(),
        model: "test".to_string(),
        active_turn_id: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        prompt_tx,
        interrupt_tx,
        interject_tx,
        session_tx,
        store: Arc::new(crate::thread_store::ThreadStore::new(project)),
        status: Arc::new(StdMutex::new(status)),
    }
}

/// 后台循环:一个卡住(等待审批)的 thread 必须让组长被唤醒,而且唤醒时注入的
/// 文本里**带着这个 thread_id**。
///
/// 直接驱动抽出来的 `lead_tick_once`,而不是等 30s 的真实 tick:那条 `select!`
/// 分支唯一做的事就是「把 `lead_tick_once` 的结果逐条投递」,断言它的返回值
/// 等于断言「组长会被唤醒、收到什么」,不必观测一次真实的 LLM 轮。投递那半
/// 由既有用例覆盖(`a_delivered_message_reaches_the_lead_thread` 走的正是同一条
/// `deliver_to_lead`)。
///
/// 「卡住」用测试后门直接摆出 `status`:状态只活在进程内(`ThreadMeta` 没有
/// status 字段),走一次真实审批请求需要拉整条 driver + provider 链路,那是在
/// 测 driver,不是测本轮。
#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_thread_wakes_the_lead_with_a_digest_naming_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().to_path_buf();
    // 组长必须**先**存在:后台 tick 只认已有组长,绝不懒创建(懒创建是
    // `thread/lead` 与 `resolve_lead_target` 的职责)。
    let store = crate::thread_store::ThreadStore::new(&project);
    let (lead_meta, _) = crate::lead::ensure_lead_meta(&store, &project).expect("lead meta");

    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    // 被唤醒者:卡在等待审批上的会话。
    threads.insert(
        "thread-stuck".to_string(),
        tick_test_session(&project, "thread-stuck", ThreadStatus::AwaitingApproval),
    );
    // 对照组:同一项目、同样有组长,但它没变化,不许被展开。
    threads.insert(
        "thread-idle".to_string(),
        tick_test_session(&project, "thread-idle", ThreadStatus::Idle),
    );

    let deliveries = lead_tick_once(&mut threads);
    assert_eq!(deliveries.len(), 1, "只有一个项目有变化");
    let delivery = &deliveries[0];
    assert_eq!(
        delivery.lead.thread_id, lead_meta.thread_id,
        "唤醒的必须是该项目的组长"
    );
    assert!(
        delivery.text.contains("thread-stuck"),
        "注入必须点名那个卡住的 thread:{}",
        delivery.text
    );
    assert!(
        !delivery.text.contains("thread-idle"),
        "没变化的 thread 不该出现在注入里:{}",
        delivery.text
    );

    // 反向:卡住解除后就不许再唤醒——低频兜底的意义是省钱,不是定时烧 token。
    *threads
        .get("thread-stuck")
        .unwrap()
        .status
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = ThreadStatus::Idle;
    assert!(
        lead_tick_once(&mut threads).is_empty(),
        "全部 idle 时后台兜底不得唤醒任何人"
    );
}

/// 后台 tick **绝不**替项目懒创建组长:没有组长就没有投递(落盘也不许冒出
/// 一个被兜底顺手造出来的组长)。
#[tokio::test(flavor = "multi_thread")]
async fn the_background_tick_never_creates_a_lead() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().to_path_buf();
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    threads.insert(
        "thread-stuck".to_string(),
        tick_test_session(&project, "thread-stuck", ThreadStatus::AwaitingApproval),
    );

    assert!(
        lead_tick_once(&mut threads).is_empty(),
        "没有组长时该整组跳过,而不是被兜底顺手造一个"
    );
    assert!(
        crate::thread_store::ThreadStore::new(&project)
            .find_lead()
            .unwrap()
            .is_none(),
        "落盘上不得出现任何由后台循环创建的组长"
    );
}

/// 用 `RecordingProvider` 建 agent,并把每次对话调用看到的 message 数记进 `seen`。
/// 与 `build_test_agent_with_provider` 同一夹具,只是把 provider 换成记账的那款。
/// 组长那轮的回复文本必须是**本条纯函数**取出的那条助手消息。
///
/// 这是阻塞式 `ask_lead` 的最后一环:登记表(`LeadReplies`)按 `request_id`
/// 收口,而收口时回填的正是这里的返回值。抽取逻辑本身复用
/// `completed_items_agent_text`(与标题生成同口径),故本用例同时钉住
/// 「抽出来的就是本轮助手文本」与「它能被回填到对应 request_id」。
#[tokio::test]
async fn a_registered_request_id_receives_the_leads_reply_text() {
    let items = vec![
        crate::protocol::Item::UserMessage {
            id: "u1".to_string(),
            text: "[来自会话 thread-sender-1] 需要你裁决 X".to_string(),
            attachments: Vec::new(),
            images: Vec::new(),
        },
        crate::protocol::Item::AgentMessage {
            id: "a1".to_string(),
            text: "先别动".to_string(),
        },
    ];

    let replies = crate::lead::LeadReplies::new();
    let rx = replies.register("lead-ask-x".to_string());
    let reply = lead_reply_text(&items).expect("本轮有非空助手消息,必须取出它的文本作为组长的回复");
    assert_eq!(reply, "先别动");

    assert!(
        replies.resolve("lead-ask-x", reply.clone()),
        "登记过的 request_id 必须能被回填"
    );
    assert_eq!(
        rx.await.expect("回填后回信通道不应关闭"),
        reply,
        "等待方收到的必须正是组长那轮的助手文本"
    );
}

/// 没有助手消息的一轮(纯工具轮 / 失败轮)不得凭空造出一条回复。
#[test]
fn lead_reply_text_is_none_without_an_agent_message() {
    let only_user = vec![crate::protocol::Item::UserMessage {
        id: "u1".to_string(),
        text: "需要你裁决 X".to_string(),
        attachments: Vec::new(),
        images: Vec::new(),
    }];
    assert!(
        lead_reply_text(&only_user).is_none(),
        "只有用户提问的一轮没有可回填的回复"
    );

    // 空白助手消息视同不存在:与 `completed_items_agent_text` 同口径,
    // 否则等待方会收到一条空回复,比超时更难排查。
    let blank = vec![crate::protocol::Item::AgentMessage {
        id: "a1".to_string(),
        text: "   ".to_string(),
    }];
    assert!(
        lead_reply_text(&blank).is_none(),
        "空文本的助手消息不算回复"
    );
}

/// 阻塞式 `ask_lead` 的整条生产链路:工具投递 → 主循环转成组长的一轮 →
/// 组长那轮收尾时把回复按 `request_id` 回填给等待中的 thread。
///
/// 刻意走**真的** `AskLeadTool` + `Harness::lead_repliers`(与主循环共享同一
/// 实例):装配漏掉 `with_replies` 时,这里会退化成 P1 的显式拒绝而非静默
/// 超时,故本用例同时钉住「装配接上了」与「主循环回填了」两件事。
#[tokio::test(flavor = "multi_thread")]
async fn a_blocking_ask_lead_receives_the_lead_reply() {
    use yi_agent_core::Tool as _;

    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    // 先确保项目里有组长(与真实用户路径一致:侧栏点过组长会话)。
    let _ = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;

    let tool = crate::lead::AskLeadTool::new(
        h.lead_inbox.clone(),
        crate::thread_store::ThreadStore::new(dir.path()).project_key(),
        "thread-sender-1".to_string(),
    )
    .with_replies(h.lead_replies.clone(), crate::lead::ASK_LEAD_TIMEOUT);

    // 等待方就是这条工具调用本身:它内部 `timeout(ASK_LEAD_TIMEOUT, rx)`
    // 之外再套一层更短的看门狗,失败时给出「没收到回复」而非挂满 2 分钟。
    let result = tokio::time::timeout(
        EVENT_WAIT,
        tool.call(serde_json::json!({
            "message": "需要你裁决 X",
            "blocking": true,
        })),
    )
    .await
    .expect("blocking ask_lead must be answered by the lead turn, not left hanging");

    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(!result.is_error, "blocking ask_lead must not fail: {text}");
    assert!(
        text.contains("组长回复："),
        "must be the reply path, not the one-way delivery: {text}"
    );
    // `MockProvider` 的助手文本恒为 "hi":断言回填的确实是**组长那轮**的文本。
    assert!(
        text.contains("hi"),
        "the waiting thread must receive the lead turn's own reply: {text}"
    );

    h.shutdown().await;
}

/// 从工具结果里取出文本块(本文件既有的断言都这么取,抽出来只为复用)。
fn tool_text(result: &yi_agent_core::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<String>()
}

/// 迟到的投递之后停留多久(见 [`LeadEchoProvider`])。
///
/// 主循环此刻是空闲的 `select` 循环,吃下这条投递只需微秒级;这个窗口只是把
/// 「机器忙」从复现条件里排除掉,不承担任何断言强度。
const LATE_ASK_WINDOW: Duration = Duration::from_millis(300);

/// 组长那轮的 provider:回显本轮最后一条用户文本,于是**每一轮的回复天然可
/// 辨识**(回复里带着"这一轮在回答谁"的记号)。这是下面两个用例能区分"A 的
/// 回复"与"B 的回复"的前提——`MockProvider` 的文本恒为 "hi",两个提问人收到
/// 谁的回信都一样,根本钉不住"第二个拿到了第一个的回复"。
///
/// 首次对话调用(标题请求不计)时额外做一件事:把一个**阻塞式 `ask_lead`**
/// 扔进这一轮的窗口里,再停留 [`LATE_ASK_WINDOW`]。这是复刻"同一组长已经有
/// 一轮在跑,另一个会话才阻塞进来"的唯一可靠窗口:投递发生在组长那轮**收尾
/// 之前**,而主循环此刻唯一的就绪分支就是这条投递(`turn_rx` 还没有任何事件),
/// 故这条 `request_id` 必定与那一轮同时挂在同一组长的待回填队列上。
struct LeadEchoProvider {
    /// 对话调用计数(标题请求不计),让上面那件事只发生一次。
    conversation_calls: AtomicUsize,
    /// 首次调用时投递的"迟到提问人"。用例在 harness 建好之后才填进来(它要用
    /// harness 的共享信箱与登记表),故用 `Mutex` 而非构造期注入。
    late_asker: std::sync::Mutex<Option<Arc<crate::lead::AskLeadTool>>>,
    /// 迟到提问人收到的文本。
    late_reply: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl LeadEchoProvider {
    fn new(late_reply: Arc<tokio::sync::Mutex<Option<String>>>) -> Self {
        Self {
            conversation_calls: AtomicUsize::new(0),
            late_asker: std::sync::Mutex::new(None),
            late_reply,
        }
    }

    fn set_late_asker(&self, tool: Arc<crate::lead::AskLeadTool>) {
        *self.late_asker.lock().unwrap_or_else(|p| p.into_inner()) = Some(tool);
    }
}

#[async_trait]
impl yi_agent_core::Provider for LeadEchoProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::message::{ContentBlock, Role};

        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        if self.conversation_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let late = self
                .late_asker
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(tool) = late {
                let slot = Arc::clone(&self.late_reply);
                tokio::spawn(async move {
                    use yi_agent_core::Tool as _;
                    let result = tool
                        .call(serde_json::json!({
                            "message": "要你裁决 A",
                            "blocking": true,
                        }))
                        .await;
                    *slot.lock().await = Some(tool_text(&result));
                });
                // 等主循环把这条迟到投递叠进**本轮**组长的待回填队列。
                tokio::time::sleep(LATE_ASK_WINDOW).await;
            }
        }
        let text = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| {
                m.content.iter().find_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(text),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 等迟到提问人的回信落进槽位(失败信息直指"谁没收到回复",而不是一句超时)。
async fn await_reply_slot(slot: &Arc<tokio::sync::Mutex<Option<String>>>, who: &str) -> String {
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    loop {
        if let Some(text) = slot.lock().await.clone() {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{who} never received a reply"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 用回显 provider 建一个 harness,并返回 (harness, workdir 持有者, 迟到回信槽位)。
///
/// 与 [`isolated_harness`] 的差别只有 provider:后者是 `MockProvider`(文本恒为
/// "hi"),无法区分两个提问人收到的回信。
async fn echo_lead_harness() -> (
    Harness,
    tempfile::TempDir,
    Arc<LeadEchoProvider>,
    Arc<tokio::sync::Mutex<Option<String>>>,
) {
    let dir = tempfile::TempDir::new().unwrap();
    let late_reply = Arc::new(tokio::sync::Mutex::new(None));
    let provider = Arc::new(LeadEchoProvider::new(Arc::clone(&late_reply)));
    let factory_provider: Arc<dyn yi_agent_core::Provider> = provider.clone();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(Arc::clone(&factory_provider), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    (h, dir, provider, late_reply)
}

/// 建一个指向该 harness 共享信箱/登记表的阻塞式提问人工具。
fn ask_lead_tool(h: &Harness, workdir: &Path, from: &str) -> Arc<crate::lead::AskLeadTool> {
    Arc::new(
        crate::lead::AskLeadTool::new(
            h.lead_inbox.clone(),
            crate::thread_store::ThreadStore::new(workdir).project_key(),
            from.to_string(),
        )
        .with_replies(h.lead_replies.clone(), crate::lead::ASK_LEAD_TIMEOUT),
    )
}

/// **同一个组长被两个会话同时阻塞等待时,第二个提问人不得收到第一个的回复。**
///
/// 复刻:提问人 A 先阻塞,于是组长跑起第一轮;A 那一轮的 provider 在收尾**之前**
/// 把提问人 B 也投了进来——组长的待回填队列此刻是 `[A, B]`。那一轮收尾时只该用
/// **这一轮**的文本回填 A;B 必须等它自己那一轮,而不是拿到 A 的答案。
///
/// 修复前:`settle_pending_lead_replies` 把整条队列一次取走,两个 id 都用 A 那轮
/// 的文本回填,B 于是收到 A 的答案(下面的 `assert_ne` / `!contains` 会失败)。
#[tokio::test(flavor = "multi_thread")]
async fn a_second_asker_on_the_same_lead_gets_its_own_reply() {
    use yi_agent_core::Tool as _;

    let (mut h, dir, provider, late_reply) = echo_lead_harness().await;
    // 与真实路径一致:侧栏点过组长会话(否则投递时才懒建 meta,不影响本用例)。
    let lead = rpc(&mut h, 2, "thread/lead", json!({})).await;
    assert!(lead.get("error").is_none(), "{lead}");

    provider.set_late_asker(ask_lead_tool(&h, dir.path(), "thread-asker-B"));

    // 提问人 A 先阻塞:组长因此跑起第一轮;A 那一轮的 provider 会在收尾前把 B
    // 也投进来,于是两个 request_id 挂在同一个组长的队列上。
    let asker_a = ask_lead_tool(&h, dir.path(), "thread-asker-A");
    let first = tokio::spawn({
        let tool = Arc::clone(&asker_a);
        async move {
            tool.call(serde_json::json!({
                "message": "要你裁决 A",
                "blocking": true,
            }))
            .await
        }
    });
    let a_text = tool_text(
        &tokio::time::timeout(EVENT_WAIT, first)
            .await
            .expect("the first asker must be answered by the lead turn, not left hanging")
            .expect("the first asker's task must not panic"),
    );
    let b_text = await_reply_slot(&late_reply, "the second asker").await;

    assert!(
        a_text.contains("thread-asker-A"),
        "the first asker must receive the reply of its own turn: {a_text}"
    );
    assert!(
        b_text.contains("thread-asker-B"),
        "the second asker must receive the reply of its own turn: {b_text}"
    );
    assert_ne!(
        a_text, b_text,
        "two askers queued on one lead must receive distinct replies"
    );
    assert!(
        !b_text.contains("thread-asker-A"),
        "the second asker must NOT receive the first asker's reply: {b_text}"
    );

    h.shutdown().await;
}

/// **一轮不携带 `lead_request_id` 的组长 turn 不得回填任何等待方。**
///
/// 复刻:用户单向投递给组长(与 `ask_lead` 的 `blocking: false` 同款,没有
/// `request_id`),这一轮的 provider 在收尾前把提问人 A 也投了进来。用户那一轮
/// 收尾时不能把自己的文本塞给正在等回信的 A——A 只能等**它自己**那一轮。
///
/// 修复前:整条队列被那一轮取走,A 收到用户的话(下面的断言会失败)。
#[tokio::test(flavor = "multi_thread")]
async fn a_lead_turn_without_a_request_id_answers_nobody() {
    const USER_TEXT: &str = "用户直接问组长";

    let (h, dir, provider, late_reply) = echo_lead_harness().await;
    provider.set_late_asker(ask_lead_tool(&h, dir.path(), "thread-asker-A"));

    // 用户单向说话:这一轮不含任何 `lead_request_id`。
    Harness::deliver_lead_message_for_test(&h, dir.path(), "thread-sender-U", USER_TEXT);

    let a_text = await_reply_slot(&late_reply, "the waiting asker").await;
    assert!(
        a_text.contains("thread-asker-A"),
        "the waiting asker must be answered by its own turn: {a_text}"
    );
    assert!(
        !a_text.contains(USER_TEXT),
        "a lead turn carrying no request_id must not answer a waiting asker: {a_text}"
    );

    h.shutdown().await;
}

/// 闸门式 provider:第 1 次**对话**调用挂起在闸门上(模拟"后台的 LLM 调用
/// 已在飞、响应还没回来"的窗口),放行后吐一个工具调用;第 2 次对话调用把收到
/// 的消息原样记下来再收尾。
///
/// 标题生成(`system == TITLE_INSTRUCTIONS`)不是对话轮次,立刻答:否则它会把
/// 闸门的第 1 次让掉,测试就永远等不到「后台正在跑」这个落点。
struct GatedToolProvider {
    conversation_calls: AtomicUsize,
    /// 后台已进入第 1 次对话调用(后台 turn 确实在跑)。
    entered: Arc<tokio::sync::Notify>,
    /// 放行第 1 次调用(测试用它把「后台的模型调用在飞」这一段精确框出来)。
    release: Arc<tokio::sync::Notify>,
    /// 第 2 次及以后每次对话调用收到的完整消息列表。
    later_messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>>,
}

#[async_trait]
impl yi_agent_core::Provider for GatedToolProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::{ProviderEvent as E, StopReason};
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(futures::stream::iter(vec![
                E::TextDelta("t".into()),
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        if self.conversation_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // 后台这一轮停在"模型调用在飞"的窗口里,直到测试放行。
            self.entered.notify_one();
            self.release.notified().await;
            return Ok(futures::stream::iter(vec![
                E::ToolUseStart {
                    id: "c1".into(),
                    name: "glob".into(),
                },
                E::ToolUseDelta {
                    id: "c1".into(),
                    partial_json: r#"{"pattern":"*.md"}"#.into(),
                },
                E::ToolUseEnd { id: "c1".into() },
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        self.later_messages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req.messages.clone());
        Ok(futures::stream::iter(vec![
            E::TextDelta("background done".into()),
            E::Stop {
                reason: StopReason::EndTurn,
            },
        ])
        .boxed())
    }
}

fn gated_lead_factory(
    provider: Arc<GatedToolProvider>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    let provider: Arc<dyn yi_agent_core::Provider> = provider;
    move |session, cwd, mode| {
        build_test_agent_with_provider(Arc::clone(&provider), session, cwd, mode)
    }
}

/// **前台请求让后台 turn 在工具边界让位**(spec §6.3;计划 Task 8)。
///
/// 复刻:组长的一轮**后台** turn 正在跑(provider 已进入第 1 次对话调用、还
/// 没回来——即"后台的 LLM 调用在飞"),此刻前台来一条请求。断言四件事:
///
/// 1. 前台请求**当场被接受**(`turn/interject` 立刻返回 `turn_id` +
///    `interjection_id`),不必等后台那次模型调用回来;
/// 2. 投影(`thread/list`)在这期间照常秒回——它读同一份状态,不等后台的 LLM
///    调用(spec §6.3 第三条「前台是投影」);
/// 3. 放行后台的模型调用后,它在**工具边界**(ACT 之后、下一次 THINK 之前)
///    折叠前台文本(`inject_pending`),文本带 `INTERJECTION_PREFIX` 进了同一
///    turn 的下一次请求,且**已完成那一轮的进度不丢**(工具结果仍在上下文里);
/// 4. 这一切都发生在**同一个 turn** 里(收尾的 `turn_id` 与前台请求拿到的
///    `turn_id` 相同),前台文本也**没有**被退回给客户端。
///
/// 这一整条链**只用既有原语**:driver 的 `select!` 在 turn 运行期间就轮询
/// `interject_rx`(`server.rs` 的 driver 内层 select),消息进 core 的 inbox,
/// 由 `inject_pending` 在边界折叠。故本任务零 core 改动。
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_request_makes_the_background_turn_yield() {
    const BACKGROUND: &str = "后台自己找的活";
    const FOREGROUND: &str = "用户刚说的话";
    /// `INTERJECTION_PREFIX` 里稳定的一段:证明文本是**折叠**进来的,而不是
    /// 被当成一个全新 prompt 重放。
    const PREFIX_MARKER: &str = "Fold it into the current task";

    let dir = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(GatedToolProvider {
        conversation_calls: AtomicUsize::new(0),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        later_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(
        cfg,
        gated_lead_factory(Arc::clone(&provider)),
        PERMISSION_TIMEOUT,
    );
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    // 组长 meta 先落盘(真实用户路径:侧栏点过组长会话)。此刻它**还没有**
    // driver,故下面的投递走冷启动路径——`prepare_turn_core` 会占住
    // `active_turn_id`,这正是前台请求能被路由到**这一轮**的凭据。
    let lead = rpc(&mut h, 2, "thread/lead", json!({})).await;
    let lead_id = lead["result"]["thread_id"]
        .as_str()
        .expect("thread/lead must answer with a thread_id")
        .to_string();

    // ---- 后台 turn 起来:真走生产投递路径(lead_inbox → 主循环 → 组长 driver)----
    Harness::deliver_lead_message_for_test(&h, dir.path(), "thread-sender-1", BACKGROUND);

    // 等后台这一轮真的跑起来(provider 已进入第 1 次对话调用、尚未返回)。
    tokio::time::timeout(EVENT_WAIT, provider.entered.notified())
        .await
        .expect("the background lead turn must reach its provider call");

    // ---- 前台请求:后台还在跑(模型调用在飞)时就投,必须当场被接受 ----
    let accepted_at = std::time::Instant::now();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/interject","params":{{"threadId":"{lead_id}","input":[{{"type":"text","text":"{FOREGROUND}"}}]}}}}"#
    ))
    .await;
    let interjected = read_response(&mut h, 3).await;
    assert!(
        interjected.get("error").is_none(),
        "a foreground request must be accepted while the background turn runs: {interjected}"
    );
    let turn_id = interjected["result"]["turn_id"]
        .as_str()
        .expect("an accepted interjection must name its turn")
        .to_string();
    assert!(
        interjected["result"]["interjection_id"].as_str().is_some(),
        "expected an interjection_id: {interjected}"
    );
    assert!(
        accepted_at.elapsed() < Duration::from_secs(5),
        "accepting a foreground request must not wait for the background LLM call: took {:?}",
        accepted_at.elapsed()
    );

    // ---- 投影:「前台是投影」——读同一份状态的 RPC 此刻照常秒回 ----
    let projected_at = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
        .await;
    let listed = read_response(&mut h, 4).await;
    assert!(
        listed["result"]["threads"].is_array(),
        "thread/list must answer while the background turn is running: {listed}"
    );
    assert!(
        projected_at.elapsed() < Duration::from_secs(5),
        "the projection must not wait for the background turn: took {:?}",
        projected_at.elapsed()
    );
    // 后台那一轮此刻仍卡在它的模型调用上——上面的「秒回」才有意义。
    assert!(
        provider
            .later_messages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty(),
        "the background turn must still be blocked in its provider call here"
    );

    // ---- 放行后台的模型调用:它做完工具调用后,在下一次 THINK 前折叠前台文本 ----
    provider.release.notify_one();

    let mut completed: Option<serde_json::Value> = None;
    let mut folded_item: Option<serde_json::Value> = None;
    let mut returned: Vec<serde_json::Value> = Vec::new();
    for _ in 0..200 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("item/started") | Some("item/completed") => {
                let item = &v["params"]["item"];
                if item["type"] == "userInterjection" {
                    folded_item = Some(item.clone());
                }
            }
            Some("turn/interjectionsReturned") => returned.push(v.clone()),
            Some("turn/completed") => {
                completed = Some(v);
                break;
            }
            _ => {}
        }
    }

    let completed = completed.expect("the background turn must finish after its tool round");
    assert_eq!(
        completed["params"]["status"], "completed",
        "the background turn must run to completion after yielding, not be killed: {completed}"
    );
    assert_eq!(
        completed["params"]["turn_id"].as_str().unwrap(),
        turn_id,
        "the foreground request must be folded into the SAME turn the background was running"
    );
    assert_eq!(
        completed["params"]["thread_id"].as_str().unwrap(),
        lead_id,
        "the yield must happen on the lead thread: {completed}"
    );
    assert!(
        returned.is_empty(),
        "an accepted foreground request must not be handed back to the client: {returned:?}"
    );
    let folded =
        folded_item.expect("the folded foreground request must surface as a userInterjection");
    assert_eq!(
        folded["text"], FOREGROUND,
        "the folded item carries the user's own words, not the prefixed copy: {folded}"
    );

    // ---- 让位不丢进度 + 文本确实折叠进了同一轮的下一次请求 ----
    let later = provider
        .later_messages
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let second = later
        .last()
        .expect("the turn must reach another THINK after its tool round")
        .clone();
    let injected = second
        .iter()
        .find(|m| {
            m.content.iter().any(
                |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t.contains(FOREGROUND)),
            )
        })
        .unwrap_or_else(|| {
            panic!("the folded foreground text must reach the next request: {second:?}")
        });
    assert!(
        injected.content.iter().any(
            |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t.contains(PREFIX_MARKER))
        ),
        "the folded text must carry the mid-turn prefix (folded, not replayed as a new prompt): {injected:?}"
    );
    assert!(
        second.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, yi_agent_core::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "c1"))),
        "the completed tool round must survive the yield (checkpoint semantics: 让位不丢进度): {second:?}"
    );

    h.shutdown().await;
}

fn recording_agent_factory(
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingProvider {
            seen: Arc::clone(&seen),
        });
        build_test_agent_with_provider(provider, session, cwd, mode)
    }
}

/// 等组长这一轮**完全收尾**:历史已 append 进主 `.jsonl`、`load` 能看到全部给定
/// 文本、且崩溃残留 checkpoint 已清除。
///
/// 三个条件缺一不可:只看 append 会在 driver 尚未清 checkpoint 时放行,而
/// `load` 会把未收尾的 partial 合流进来,让下一条 harness 看到的状态与本测试
/// 断言的状态不同源。
async fn wait_for_lead_turn_to_settle(workdir: &Path, thread_id: &str, texts: &[&str]) {
    let log = workdir
        .join(".yi-agent/threads")
        .join(format!("{thread_id}.jsonl"));
    let partial = workdir
        .join(".yi-agent/threads")
        .join(format!("{thread_id}.partial.json"));
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    loop {
        let appended = std::fs::read_to_string(&log)
            .map(|line| texts.iter().all(|t| line.contains(t)))
            .unwrap_or(false);
        let loaded = crate::thread_store::ThreadStore::new(workdir)
            .load(thread_id)
            .ok()
            .flatten()
            .is_some_and(|loaded| transcript_contains(&loaded.messages, texts));
        if appended && loaded && !partial.exists() {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "the lead turn never settled for {texts:?} \
                 (appended={appended}, loaded={loaded}, partial={})",
                partial.exists()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 这些文本是否都能在给定的消息列表里找到。
fn transcript_contains(messages: &[yi_agent_core::Message], texts: &[&str]) -> bool {
    texts.iter().all(|want| {
        messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                yi_agent_core::ContentBlock::Text(text) => text.contains(want),
                _ => false,
            })
        })
    })
}

/// 冷启动投递必须带回组长既有对话的历史。
///
/// 「冷启动」= 组长 thread 不在本进程的 `threads` 里(进程重启,或它的 driver
/// 已随 `prompt_rx` 关闭而收尾),而磁盘上的 `.jsonl` 仍记着此前投递过的每一轮。
/// 修复前该路径拿**空会话**拉起组长:同一条 thread 经 `thread/resume` 打开有
/// 记忆、经投递冷启却失忆,两条入口语义分裂(规格决策 9:组长就是一个 thread,
/// 该有普通 thread 的对话连续性)。
///
/// 用两个共享同一 workdir 的 harness 复刻重启:第一个里真的投递一轮(于是
/// `.jsonl` 由生产路径写出),关掉它(driver 随之消失),第二个用同一目录再投
/// 一轮,断言模型这轮看到的是「历史 + 本次」而非只有本次。
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_lead_delivery_restores_the_persisted_history() {
    const FIRST: &str = "需要你裁决 X";
    const SECOND: &str = "还有一件事要你裁决 Y";
    let dir = tempfile::TempDir::new().unwrap();
    let workdir = dir.path().to_path_buf();

    // ---- 第一次投递:真走生产路径,让 `.jsonl` 得到一轮由投递产生的历史 ----
    let seen_1 = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
    let mut h1 = {
        let mut cfg = test_config();
        cfg.workdir = workdir.clone();
        Harness::with_config(
            cfg,
            recording_agent_factory(Arc::clone(&seen_1)),
            PERMISSION_TIMEOUT,
        )
    };
    h1.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h1, 1).await;
    let lead = rpc(&mut h1, 2, "thread/lead", json!({})).await;
    let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();
    Harness::deliver_lead_message_for_test(&h1, &workdir, "thread-sender-1", FIRST);
    wait_for_lead_turn_to_settle(&workdir, &lead_id, &[FIRST]).await;
    // 新建的组长没有历史:这一轮只带本次投递这一条 user 消息。
    assert_eq!(
        *seen_1.lock().unwrap(),
        vec![1],
        "a brand-new lead has nothing to restore: the first turn must carry only its own message"
    );
    // 关掉:serve 主循环结束 → `threads` 被 drop → `prompt_tx` 关闭 →
    // 组长的 driver 读到 `prompt_rx` 关闭后收尾退出。这正是「重启后该 thread
    // 不在内存」的复刻,也是本 finding 描述的冷启动入口。
    h1.shutdown().await;

    // ---- 第二次投递:同一 workdir,历史在盘上,而内存里没有该 thread ----
    let seen_2 = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
    let mut h2 = {
        let mut cfg = test_config();
        cfg.workdir = workdir.clone();
        Harness::with_config(
            cfg,
            recording_agent_factory(Arc::clone(&seen_2)),
            PERMISSION_TIMEOUT,
        )
    };
    h2.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h2, 1).await;
    let again = rpc(&mut h2, 2, "thread/lead", json!({})).await;
    assert_eq!(
        again["result"]["thread_id"].as_str().unwrap(),
        lead_id,
        "one project keeps one lead thread across a restart"
    );
    Harness::deliver_lead_message_for_test(&h2, &workdir, "thread-sender-2", SECOND);
    // 等**本次**投递收尾(历史是否被恢复不在本等待里判:那是下面的断言,
    // 让失败信息直指「历史丢了」而非一句超时)。
    wait_for_lead_turn_to_settle(&workdir, &lead_id, &[SECOND]).await;

    // 本轮落盘的消息 = 恢复后的完整会话:先前的提问与回复都在,再加本次。
    // 修复前这里是 [user(SECOND), assistant],`FIRST` 已丢——断言会失败。
    let loaded = crate::thread_store::ThreadStore::new(&workdir)
        .load(&lead_id)
        .unwrap()
        .expect("the lead thread must be persisted");
    assert!(
        transcript_contains(&loaded.messages, &[FIRST]),
        "the cold-start turn must persist the restored history, not just this \
         message: {:?}",
        loaded.messages
    );
    assert!(
        transcript_contains(&loaded.messages, &[SECOND]),
        "the cold-start turn must also persist its own message: {:?}",
        loaded.messages
    );
    // 模型这一轮看到的是「历史(2)+ 本次(1)」,不是只有本次(1)。
    assert_eq!(
        *seen_2.lock().unwrap(),
        vec![3],
        "the cold-start turn must send the restored history to the model"
    );

    h2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_completion_persists_items_and_messages() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = {
        let mut c = test_config();
        c.workdir = dir.path().to_path_buf();
        c
    };
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let log = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    // 落盘是尽力而为且发生在 driver 写完 turn/completed 之后,故轮询等待。
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        match std::fs::read_to_string(&log) {
            Ok(t) if !t.trim().is_empty() => {
                text = t;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(!text.is_empty(), "turn must be persisted");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one turn = one line");

    // 第一行必须含合成的 userMessage item 与 agent 回复。
    assert!(
        text.contains(r#""type":"userMessage""#),
        "missing user item: {text}"
    );
    assert!(text.contains("hello"), "missing prompt text: {text}");
    assert!(
        text.contains(r#""type":"agentMessage""#),
        "missing agent item: {text}"
    );
    assert!(
        text.contains(r#""role":"User""#),
        "missing core message: {text}"
    );

    h.shutdown().await;
}

/// 崩溃残留：主 jsonl 有一轮，另有未收尾的 partial。resume 必须回放两轮的
/// items，并以一条 interrupted 的 turn/completed 收尾。
#[tokio::test(flavor = "multi_thread")]
async fn resume_flags_a_crashed_partial_turn_as_interrupted() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    // `Harness::new()` 的 workdir 是 test_config() 的默认路径，与下面 store
    // 用的 tempdir 不一致会让 resume 找不到该 thread，故必须走 with_config。
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    // 直接用 store 造盘面：一轮已落盘 + 一段 partial 残留。
    let store = crate::thread_store::ThreadStore::new(dir.path());
    let tid = "thread-crash";
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.into(),
            cwd: dir.path().to_string_lossy().to_string(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();
    store
        .append_turn(
            tid,
            &crate::thread_store::TurnLine::Turn {
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-t1".into(),
                    text: "first".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                usage: None,
                messages: vec![],
            },
        )
        .unwrap();
    store
        .write_partial(
            tid,
            &crate::thread_store::PartialTurn {
                turn_id: "turn-t2".into(),
                items: vec![
                    crate::protocol::Item::UserMessage {
                        id: "user-turn-t2".into(),
                        text: "second".into(),
                        attachments: Vec::new(),
                        images: Vec::new(),
                    },
                    crate::protocol::Item::AgentMessage {
                        id: "item-turn-t2-1".into(),
                        text: "half".into(),
                    },
                ],
                messages: vec![],
                usage: None,
            },
        )
        .unwrap();

    initialize(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let mut saw_half = false;
    let mut interrupted = false;
    for _ in 0..40 {
        let v = h.read_value().await;
        if replayed_items_of(&v).iter().any(
            |it| matches!(it, crate::protocol::Item::AgentMessage { text, .. } if text == "half"),
        ) {
            saw_half = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed")
            && v["params"]["status"] == "interrupted"
        {
            interrupted = true;
            break;
        }
        if v.get("id") == Some(&serde_json::json!(5)) && interrupted {
            break;
        }
    }
    assert!(
        saw_half,
        "the crashed turn's finished items must be replayed"
    );
    assert!(
        interrupted,
        "a crashed partial turn must be flagged interrupted"
    );
    h.shutdown().await;
}

/// C1 回归：resume 采纳的崩溃轮必须被**升格**（append）进主 jsonl。否则该 partial
/// 是这一轮唯一的持久副本，用户下一条消息的 turn-start checkpoint 会原子覆盖它，
/// 崩溃轮的 items 永久丢失——而 session 上下文仍"记得"它们，造成 item/上下文不一致。
///
/// 走完整链路：崩溃（一轮 jsonl + 一段 partial）→ resume（回放 + interrupted，
/// 且必须已升格）→ 第二个 turn 正常收尾 → store 冷 load：崩溃轮 items 仍在、
/// 只一份、不重复。
#[tokio::test(flavor = "multi_thread")]
async fn resume_promotes_the_crashed_turn_so_a_later_turn_cannot_drop_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let store = crate::thread_store::ThreadStore::new(dir.path());
    let tid = "thread-promote";
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.into(),
            cwd: dir.path().to_string_lossy().to_string(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            content_epoch: 0,
            last_item_id: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();
    // 一轮正常收尾过的历史。
    store
        .append_turn(
            tid,
            &crate::thread_store::TurnLine::Turn {
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-t1".into(),
                    text: "first".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                usage: None,
                messages: vec![],
            },
        )
        .unwrap();
    // 崩溃残留：这一轮只有 partial，jsonl 里没有它的首 item id。
    store
        .write_partial(
            tid,
            &crate::thread_store::PartialTurn {
                turn_id: "turn-t2".into(),
                items: vec![
                    crate::protocol::Item::UserMessage {
                        id: "user-turn-t2".into(),
                        text: "second".into(),
                        attachments: Vec::new(),
                        images: Vec::new(),
                    },
                    crate::protocol::Item::AgentMessage {
                        id: "item-turn-t2-1".into(),
                        text: "half".into(),
                    },
                ],
                messages: vec![],
                usage: None,
            },
        )
        .unwrap();

    initialize(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    // resume 必须回放崩溃轮的 items 并标注 interrupted，随后响应。
    let mut saw_half = false;
    let mut interrupted = false;
    let mut resumed = false;
    for _ in 0..40 {
        let v = h.read_value().await;
        if replayed_items_of(&v).iter().any(
            |it| matches!(it, crate::protocol::Item::AgentMessage { text, .. } if text == "half"),
        ) {
            saw_half = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed")
            && v["params"]["status"] == "interrupted"
        {
            interrupted = true;
        }
        if v.get("id") == Some(&serde_json::json!(5)) {
            resumed = true;
            break;
        }
    }
    assert!(
        saw_half,
        "the crashed turn's finished items must be replayed"
    );
    assert!(
        interrupted,
        "a crashed partial turn must be flagged interrupted"
    );
    assert!(resumed, "resume must respond");

    // 第二个 turn：正常走完。它的 turn-start checkpoint 会覆盖盘上的 partial
    // 文件——若 resume 没有先升格，这一步就是崩溃轮数据的丢失点。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":6,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"third"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            assert_eq!(v["params"]["status"], "completed", "second turn: {v}");
            break;
        }
    }

    // 落盘是尽力而为且发生在 driver 收尾之后（见既有持久化测试），故轮询等待。
    let log = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("item-turn-t2-1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        text.contains("item-turn-t2-1") && text.contains("half"),
        "the recovered crashed turn must survive a later turn on disk; \
         log now contains {} turn(s): {text}",
        text.lines().filter(|l| !l.trim().is_empty()).count()
    );

    // 冷 load：崩溃轮的内容仍在，且只一份。
    let loaded = store.load(tid).unwrap().unwrap();
    let ids: Vec<String> = loaded
        .items
        .iter()
        .map(|i| crate::server::item_id(i).unwrap().to_string())
        .collect();
    assert_eq!(
        &ids[..3],
        &["user-t1", "user-turn-t2", "item-turn-t2-1"],
        "the crashed turn must be first and intact: {ids:?}"
    );
    assert_eq!(
        ids.iter().filter(|i| *i == "item-turn-t2-1").count(),
        1,
        "the crashed turn must not be double-counted: {ids:?}"
    );
    assert!(
        !loaded.pending_turn,
        "promotion must clear the pending flag"
    );
    h.shutdown().await;
}

/// I1 回归：等待审批期间**发起客户端断连**（真实的重连场景，不是崩溃）时，
/// driver 就此收尾并 `return`，必须清掉 checkpoint；否则盘上留一条已结束 turn
/// 的残留，重启后 resume 会把最后这一轮误报为 interrupted。
///
/// 直接以「发起方已在 hub 上注销」接线 `is_connected` 判据；生产里发起方是
/// stdio 的 `local`。
#[tokio::test(flavor = "multi_thread")]
async fn driver_clears_the_checkpoint_when_the_initiator_disconnects_during_approval() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(crate::thread_store::ThreadStore::new(dir.path()));
    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
    let _keep_session_tx = session_tx;
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);

    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    // 发起方：注册后立刻摘除 = 断连。
    let initiator = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let _init_rx = hub.register(initiator.clone());
    hub.unregister(&initiator);

    let built = build_permission_agent(
        None,
        std::path::Path::new("/tmp"),
        crate::thread_store::ThreadMode::Normal,
    )
    .unwrap();
    let handle = tokio::spawn(run_thread_driver(
        "thread-disconnect".into(),
        built.agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        hub,
        initiator,
        turn_tx,
        None,
        Arc::new(Mutex::new(HashMap::new())),
        Duration::from_secs(60),
        Arc::new(AtomicU64::new(1)),
        None,
        Arc::clone(&store),
        ThreadSession::new_status(),
        built.provider,
        built.config,
    ));

    prompt_tx
        .send(TurnPrompt {
            turn_id: "turn-1".into(),
            prompt: "hi".into(),
            activate: None,
            attachments: Vec::new(),
            image_blocks: Vec::new(),
            image_refs: Vec::new(),
            display_text: "hi".into(),
            lead_request_id: None,
        })
        .await
        .unwrap();

    // 审批请求命中"发起方已断连"分支 → 收尾并 return。
    let ev = tokio::time::timeout(EVENT_WAIT, turn_rx.recv())
        .await
        .expect("driver must report Finished when the initiator is gone")
        .expect("turn channel must stay open");
    assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

    // clear_partial 发生在 Finished 之后；轮询等待（文件可能已被删除）。
    let partial = dir
        .path()
        .join(".yi-agent/threads/thread-disconnect.partial.json");
    for _ in 0..100 {
        if !partial.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !partial.exists(),
        "a disconnect during approval must clear the turn checkpoint"
    );

    drop(prompt_tx);
    let _ = tokio::time::timeout(EVENT_WAIT, handle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_resume_replays_history_and_restores_context() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_factory = Arc::clone(&seen);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingProvider {
            seen: Arc::clone(&seen_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // turn 1
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"first"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // resume:期望 thread/started → item/completed(含 user + agent) → 响应
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut replayed: Vec<String> = Vec::new();
    let mut item_ids: Vec<String> = Vec::new();
    let mut resumed = false;
    for _ in 0..12 {
        let v = h.read_value().await;
        for it in replayed_items_of(&v) {
            replayed.push(
                serde_json::to_value(&it).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            item_ids.push(item_id(&it).unwrap().to_string());
        }
        if v.get("id") == Some(&serde_json::json!(4)) {
            assert_eq!(v["result"]["thread_id"], tid);
            resumed = true;
            break;
        }
    }
    assert!(resumed, "thread/resume must respond");
    assert!(
        replayed.contains(&"userMessage".to_string()),
        "replay must include the user item: {replayed:?}"
    );
    assert!(
        replayed.contains(&"agentMessage".to_string()),
        "replay must include the agent item: {replayed:?}"
    );

    // turn 2:provider 应看到恢复后的完整上下文(user + assistant + 新 user)
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"second"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
            item_ids.push(v["params"]["item"]["id"].as_str().unwrap().to_string());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let counts = seen.lock().unwrap().clone();
    assert_eq!(
        counts,
        vec![1, 3],
        "resumed turn must carry prior context (user + assistant + new user): {counts:?}"
    );

    // 回归:resume 后新 turn 的 item id 不得与回放的历史 id 冲突,
    // 否则前端按 id upsert 会覆盖历史。
    let mut unique = item_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        item_ids.len(),
        "item ids must be unique across replay + resumed turn: {item_ids:?}"
    );

    h.shutdown().await;
}

/// 长 thread 的 resume 必须回放**全部** items：这是「重启后长会话丢尾部」的
/// 回归测试。旧实现逐条 try_send,超过 256 格队列即静默丢帧,本测试必失败。
#[tokio::test(flavor = "multi_thread")]
async fn resume_replays_every_item_for_long_threads() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 直接向 store 落一轮含 600 个 item 的历史(远超 CLIENT_QUEUE=256)。
    let store = crate::thread_store::ThreadStore::new(dir.path());
    let total = 600usize;
    let items: Vec<crate::protocol::Item> = (0..total)
        .map(|i| crate::protocol::Item::AgentMessage {
            id: format!("item-{i}"),
            text: format!("m{i}"),
        })
        .collect();
    store
        .append_turn(
            &tid,
            &crate::thread_store::TurnLine::Turn {
                items: items.clone(),
                usage: None,
                messages: Vec::new(),
            },
        )
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;

    let replayed = collect_replayed_items(&mut h, 4).await;
    let replayed_ids: std::collections::HashSet<&str> =
        replayed.iter().filter_map(|it| item_id(it)).collect();
    let expected_ids: std::collections::HashSet<String> =
        items.iter().filter_map(item_id_owned).collect();
    let mut missing: Vec<&String> = expected_ids
        .iter()
        .filter(|id| !replayed_ids.contains(id.as_str()))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "resume must replay every item; {} missing (e.g. {:?})",
        missing.len(),
        missing.iter().take(5).collect::<Vec<_>>()
    );
    h.shutdown().await;
}

fn item_id_owned(it: &crate::protocol::Item) -> Option<String> {
    item_id(it).map(|s| s.to_string())
}

/// 发 thread/clear，被 `-32012` 拒时重试。
///
/// 主循环要等 `TurnEvent::Finished` 才清 `active_turn_id`，而 driver 是在
/// `turn/completed` 之后（落盘之后）才发该事件；所以刚结束一轮就立刻 clear
/// 可能撞上这个窗口。这是已接受的残留限制（方案 A），客户端重试即可。
async fn clear_thread_retrying(h: &mut Harness, tid: &str) -> serde_json::Value {
    for attempt in 0..20 {
        let id = 100 + attempt;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) {
                if v["error"]["code"] != -32012 {
                    return v;
                }
                break; // 还在 turn 收尾窗口内：等一会儿再试
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    panic!("thread/clear kept answering -32012");
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_clear_empties_the_context_and_resume_does_not_revive_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 先跑一轮，制造可被清空的上下文；必须等本轮真正收尾（日志已 append 且
    // turn/completed 已到）再发 clear——落盘发生在 turn/completed 之后,
    // 早发会被主循环以 -32012（turn 进行中）拒绝。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
    ))
    .await;
    let log = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    let mut persisted = false;
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(t) = std::fs::read_to_string(&log) {
            if !t.trim().is_empty() {
                persisted = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(persisted, "turn must be persisted before we clear");
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // clear（可能撞上收尾窗口，重试即可）。
    let v = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        v.get("error").is_none(),
        "clear must eventually succeed: {v}"
    );

    // 关键回归：resume 不得把旧消息回放回来。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut replayed = Vec::new();
    for _ in 0..8 {
        let v = h.read_value().await;
        for it in replayed_items_of(&v) {
            if let crate::protocol::Item::UserMessage { text, .. }
            | crate::protocol::Item::AgentMessage { text, .. } = it
            {
                replayed.push(text);
            }
        }
        if v.get("id") == Some(&serde_json::json!(5)) {
            assert!(v.get("error").is_none(), "resume must still work: {v}");
            break;
        }
    }
    assert!(
        !replayed.iter().any(|t| t == "hello"),
        "cleared context must not come back on resume: {replayed:?}"
    );

    h.shutdown().await;
}

/// 跑一轮，并等到它**真正落盘**。
///
/// 落盘发生在 `turn/completed` **之后**（见
/// `thread_clear_empties_the_context_and_resume_does_not_revive_it` 的注释），
/// 所以不能只读到 `turn/completed` 就认为盘上有了：必须轮询 `.jsonl`。
/// 随后把 `turn/completed` 排空，免得它污染后续断言。
///
/// 后续任务（clear 世代、listAll 版本字段、锚点校验）都复用它。
async fn run_a_turn_and_wait_for_persist(
    h: &mut Harness,
    workdir: &std::path::Path,
    tid: &str,
    text: &str,
) {
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 700, "method": "turn/start",
            "params": { "threadId": tid, "input": [{ "type": "text", "text": text }] },
        })
        .to_string(),
    )
    .await;
    let log = workdir
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let mut persisted = false;
    while tokio::time::Instant::now() < deadline {
        if std::fs::read_to_string(&log).is_ok_and(|t| !t.trim().is_empty()) {
            persisted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(persisted, "turn for {tid} must reach disk");
    for _ in 0..60 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            return;
        }
    }
    panic!("turn/completed never arrived for {tid}");
}

/// `/clear` 是**唯一**的世代递增点：它让服务端 item 集合从"有"变"无"，
/// 客户端手上的内容就此结构性不一致。断言直接读 meta 文件，故本用例不依赖
/// `thread/listAll` 是否已携带 `content_epoch`。
#[tokio::test(flavor = "multi_thread")]
async fn clearing_a_thread_bumps_its_content_epoch() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let store = crate::thread_store::ThreadStore::new(dir.path());

    assert_eq!(
        store.load(&tid).unwrap().unwrap().meta.content_epoch,
        0,
        "a fresh thread must start at epoch 0"
    );

    let cleared = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        cleared.get("error").is_none(),
        "clear must succeed: {cleared}"
    );

    assert_eq!(
        store.load(&tid).unwrap().unwrap().meta.content_epoch,
        1,
        "clear must bump the content epoch"
    );
    h.shutdown().await;
}

/// 不带锚点（桌面 / 老客户端）→ 不做任何校验。这是零回归门禁。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_without_a_context_anchor_is_not_validated() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    // epoch 被 clear 推到 1，但客户端不带锚点 ⇒ 照常准入（服务端不做推断）。
    let cleared = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        cleared.get("error").is_none(),
        "clear must succeed: {cleared}"
    );

    let resp = rpc(
        &mut h,
        71,
        "turn/start",
        json!({ "threadId": tid, "input": [{ "type": "text", "text": "hi" }] }),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "no anchor must mean no validation: {resp}"
    );
    h.shutdown().await;
}

/// 锚点世代已失效（会话在客户端不知情时被 clear）→ 拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_a_stale_epoch_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let cleared = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        cleared.get("error").is_none(),
        "clear must succeed: {cleared}"
    );

    let resp = rpc(
        &mut h,
        72,
        "turn/start",
        json!({
            "threadId": tid,
            "input": [{ "type": "text", "text": "hi" }],
            "contextAnchor": { "contentEpoch": 0 },
        }),
    )
    .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32026),
        "must be stale_context: {resp}"
    );
    assert_eq!(resp["error"]["data"]["code"], json!("stale_context"));
    h.shutdown().await;
}

/// **关键正向用例**：锚点仍在但落后于末条 → 放行。
/// 落后是两端并存的正常形状；拒绝它会破坏父 spec 的核心场景。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_an_anchor_behind_the_tail_is_accepted() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "first").await;

    // 取一个早期的 item 作为"落后"的锚点。
    let read = rpc(&mut h, 73, "thread/readItems", json!({ "threadId": tid })).await;
    let early = read["result"]["items"][0]["id"]
        .as_str()
        .expect("the first turn must have items")
        .to_string();

    // 落盘发生在 turn/completed 之后：收尾窗口内 turn/start 会被 -32012 拒。
    // 锚点校验的正向用例不该被这个无关竞态判红，故重试。
    let mut resp = json!(null);
    for attempt in 0..20 {
        resp = rpc(
            &mut h,
            74,
            "turn/start",
            json!({
                "threadId": tid,
                "input": [{ "type": "text", "text": "second" }],
                "contextAnchor": { "contentEpoch": 0, "lastItemId": early },
            }),
        )
        .await;
        if resp["error"]["code"] != json!(-32012) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let _ = attempt;
    }
    assert!(
        resp.get("error").is_none(),
        "a behind-but-present anchor must pass: {resp}"
    );
    h.shutdown().await;
}

/// 锚点在世代相同的会话里已不在日志中 → 拒绝。
/// 这是 `anchor_gone` 分支的正向钉：只钉「世代不符」会漏掉同世代的失效锚点。
/// 用全新会话（epoch 0、无 item）直接构造：任何 lastItemId 都不存在，
/// 且不必先跑一轮（避免撞上 turn 收尾窗口的 `-32012`）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_a_same_epoch_but_absent_anchor_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let resp = rpc(
        &mut h,
        75,
        "turn/start",
        json!({
            "threadId": tid,
            "input": [{ "type": "text", "text": "second" }],
            "contextAnchor": { "contentEpoch": 0, "lastItemId": "item-does-not-exist" },
        }),
    )
    .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32026),
        "an absent anchor must be stale_context: {resp}"
    );
    assert_eq!(resp["error"]["data"]["code"], json!("stale_context"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_clear_rejects_an_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/clear","params":{"threadId":"nope"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
    h.shutdown().await;
}

/// 构造一份「可压缩」的测试 agent 工厂:初始 session 有 3 条消息
/// (user/assistant/user),压缩后会变成 2 条。
///
/// 注意只放 1 条 user 消息是**不可压缩**的:`plan_compaction` 会把历史里
/// 所有 user 消息合并成一条,消息数不减少即返回 `None`(→ `not_reduced`)。
fn compactable_factory(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let session = session.unwrap_or_else(|| {
        let mut s = yi_agent_core::Session::new();
        s.replace_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
            yi_agent_core::Message::user("second"),
        ]);
        s
    });
    Ok(BuiltAgent {
        agent: yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        )
        .with_session(session),
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    })
}

/// 起一个 thread,然后调 `thread/compact`,返回响应的信封。
async fn compact_thread(h: &mut Harness, tid: &str) -> serde_json::Value {
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        // 跳过沿线可能出现的通知(thread/status/updated 等),只等响应。
        if v.get("id") == Some(&serde_json::json!(9)) {
            return v;
        }
    }
    panic!("thread/compact must respond");
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_compact_reports_compacted_when_history_shrinks() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, compactable_factory, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let v = compact_thread(&mut h, &tid).await;
    assert_eq!(
        v["result"]["status"], "compacted",
        "expected compaction: {v}"
    );
    h.shutdown().await;
}

/// `compact` **不得**抹掉 `last_item_id`。
///
/// 机制：`compact` 走的 `persist_and_finish_turn` 传空 prompt + 空 items，
/// 且它刻意**不**递增 epoch、也**不**删除日志里既有的 item 行（读取时按行
/// 拼接）。若空 items 的落盘把游标写成 `None`，`thread/listAll` 就会把一个
/// 「仍有 items」的会话报成「从未有过 item」——客户端据此会认为本地锚点毫无
/// 意义，`listAll` 的「是否落后」信号被永久削弱。故空落盘必须**不**覆盖游标。
#[tokio::test(flavor = "multi_thread")]
async fn compact_does_not_erase_last_item_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, compactable_factory, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 先跑一轮真实 turn，让日志里真的落进 item（游标由此被写上）。
    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;

    let before = list_all(&mut h).await;
    let anchor = last_item_id_of(&before, &tid);
    assert!(
        anchor.is_some(),
        "a persisted turn must record last_item_id before compact: {before}"
    );
    let epoch = content_epoch_of(&before, &tid);

    let v = compact_thread(&mut h, &tid).await;
    assert_eq!(
        v["result"]["status"], "compacted",
        "compaction must actually happen, else this test proves nothing: {v}"
    );

    let after = list_all(&mut h).await;
    assert_eq!(
        last_item_id_of(&after, &tid),
        anchor,
        "compact must NOT erase last_item_id: the items are still there \
         (compact appends an empty line and never deletes the log): {after}"
    );
    assert_eq!(
        content_epoch_of(&after, &tid),
        epoch,
        "compact is deliberately not an epoch bump: {after}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_compact_reports_not_reduced_for_a_short_history() {
    let mut h = Harness::new(); // build_test_agent 的 session 为空
    let tid = start_thread(&mut h).await;
    let v = compact_thread(&mut h, &tid).await;
    assert_eq!(v["result"]["status"], "not_reduced", "expected no-op: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_compact_reports_failure_without_breaking_the_connection() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let build = |session: Option<yi_agent_core::Session>,
                 _cwd: &std::path::Path,
                 _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(ErroringProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut session = session.unwrap_or_default();
        session.replace_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
            yi_agent_core::Message::user("second"),
        ]);
        Ok(BuiltAgent {
            agent: yi_agent_core::Agent::new(
                provider.clone(),
                Arc::new(yi_agent_core::ToolRegistry::new()),
                config.clone(),
            )
            .with_session(session),
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let v = compact_thread(&mut h, &tid).await;
    assert_eq!(v["result"]["status"], "failed", "expected failure: {v}");
    assert!(
        v["result"]["error"].is_string(),
        "must carry the reason: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_compact_is_rejected_while_a_turn_is_running() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            break;
        }
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
    let mut rejected = None;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            rejected = Some(v);
            break;
        }
    }
    let v = rejected.expect("thread/compact must respond");
    assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_clear_is_rejected_while_a_turn_is_running() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    // 等到 turn 真正开始（turn/started）再发 clear。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            break;
        }
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
    let mut rejected = None;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            rejected = Some(v);
            break;
        }
    }
    let v = rejected.expect("thread/clear must respond");
    assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
    h.shutdown().await;
}

/// Task 9:resume 必须读取该 thread 持久化的 `permission_mode` 并透传给 agent
/// 工厂,使 yolo 线程重开后仍以 yolo 重建。工厂内部 `mode → cfg.yolo → 共享
/// YoloSwitch` 的映射由 `bootstrap.rs::interactive_yolo_config_starts_switch_on`
/// 钉死;此处只钉死「app-server 侧 resume 读了持久化模式并透传」这一契约
/// (server 内存中的 switch 不可从外部观测,故用记录型工厂观察传参)。
#[tokio::test(flavor = "multi_thread")]
async fn resume_passes_persisted_mode_to_factory() {
    use crate::thread_store::{ThreadMode, ThreadStore};

    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();

    let seen: Arc<std::sync::Mutex<Vec<ThreadMode>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let build =
        move |session: Option<yi_agent_core::Session>, cwd: &std::path::Path, mode: ThreadMode| {
            seen_c.lock().unwrap().push(mode);
            build_test_agent(session, cwd, mode)
        };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // thread/start 必须走 Normal。
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[ThreadMode::Normal],
        "thread/start must build with Normal"
    );

    // 绕过 RPC,直接把该线程持久化的 permission_mode 改成 Yolo。
    let store = ThreadStore::new(dir.path());
    assert!(
        store.set_permission_mode(&tid, ThreadMode::Yolo).unwrap(),
        "thread meta must exist after thread/start"
    );

    // resume 应读取持久化的 Yolo 并透传给工厂。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resumed = false;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            assert_eq!(v["result"]["thread_id"], tid);
            resumed = true;
            break;
        }
    }
    assert!(resumed, "thread/resume must respond");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.last(),
        Some(&ThreadMode::Yolo),
        "resume must pass the persisted mode to the factory: {seen:?}"
    );
    h.shutdown().await;
}

/// Task 10:`thread/setPermissionMode` 必须实时翻转该线程的 `YoloSwitch`
/// (运行期立即生效)并把 `permission_mode` 落盘(resume 后仍读得到)。
/// server 内存里的 switch 外部不可观测,故用记录型工厂捕获工厂实际交给
/// 该线程的同一个 `YoloSwitch`,以它断言运行期开关被翻转。
#[tokio::test(flavor = "multi_thread")]
async fn set_permission_mode_toggles_and_persists() {
    use crate::thread_store::{ThreadMode, ThreadStore};

    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();

    let seen: Arc<std::sync::Mutex<Vec<yi_agent_core::autonomy::YoloSwitch>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let build =
        move |session: Option<yi_agent_core::Session>, cwd: &std::path::Path, mode: ThreadMode| {
            let sw = yi_agent_core::autonomy::YoloSwitch::new(mode == ThreadMode::Yolo);
            seen_c.lock().unwrap().push(sw.clone());
            let mut built = build_test_agent(session, cwd, mode)?;
            built.yolo = sw;
            Ok(built)
        };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let sw = seen.lock().unwrap().last().cloned().unwrap();
    assert!(!sw.get(), "a freshly started thread must not be in yolo");

    // 切到 yolo:响应成功,运行期开关立即为 true,且已落盘。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert!(
        v.get("error").is_none(),
        "setPermissionMode must succeed: {v}"
    );
    assert_eq!(v["result"], serde_json::json!({}));
    assert!(sw.get(), "the thread's YoloSwitch must turn on immediately");
    let store = ThreadStore::new(dir.path());
    assert_eq!(
        store
            .load(&tid)
            .unwrap()
            .expect("thread meta must exist")
            .meta
            .permission_mode,
        ThreadMode::Yolo,
        "yolo must persist to disk"
    );

    // 切回 normal:开关关掉,落盘也回到 normal。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"normal"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert!(
        v.get("error").is_none(),
        "setPermissionMode must succeed: {v}"
    );
    assert!(
        !sw.get(),
        "the thread's YoloSwitch must turn off immediately"
    );
    assert_eq!(
        store
            .load(&tid)
            .unwrap()
            .expect("thread meta must exist")
            .meta
            .permission_mode,
        ThreadMode::Normal,
        "normal must persist to disk"
    );

    h.shutdown().await;
}

/// Task 10:非法 `mode` 报 `-32602`(且优先于 thread 存在性校验);未知
/// `threadId` 报 `-32011`。两者分别用独立请求覆盖。
#[tokio::test(flavor = "multi_thread")]
async fn set_permission_mode_rejects_bad_mode_and_unknown_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 非法 mode:即便 thread 真实存在也必须 `-32602`。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"bogus"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert_eq!(
        v["error"]["code"], -32602,
        "invalid mode must be invalid_params: {v}"
    );

    // 未知 thread + 合法 mode → `-32011`。
    h.send(
        r#"{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"yolo"}}"#,
    )
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert_eq!(
        v["error"]["code"], -32011,
        "unknown thread must be unknown_thread: {v}"
    );

    // 非法 mode + 未知 thread → 仍是 `-32602`:证明 mode 校验先于 thread
    // 存在性,不会因 threadId 未知而退化成 `-32011`。
    h.send(
        r#"{"jsonrpc":"2.0","id":5,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"bogus"}}"#,
    )
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert_eq!(
        v["error"]["code"], -32602,
        "invalid mode must win over thread existence: {v}"
    );

    // 缺少 threadId → `-32602`(invalid params)。
    h.send(
        r#"{"jsonrpc":"2.0","id":6,"method":"thread/setPermissionMode","params":{"mode":"yolo"}}"#,
    )
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(6)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert_eq!(
        v["error"]["code"], -32602,
        "missing threadId must be invalid_params: {v}"
    );

    // 非字符串 mode(数字)→ `-32602`:`as_str()` 落空,走 `_` 分支。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":5}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(7)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("setPermissionMode must respond");
    assert_eq!(
        v["error"]["code"], -32602,
        "non-string mode must be invalid_params: {v}"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_resume_unknown_returns_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{"threadId":"nope"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["error"]["code"], -32011);
    h.shutdown().await;
}

/// Design §8:resume 的目标目录已不存在时必须明确报错,不得静默回退 `$HOME`。
/// 对内存中的 thread,其权威 cwd 已知(见 `ThreadSession`),故这是可达且必须
/// 报错的路径。冷 thread 的元数据本就存在 workspace 目录内,目录删除后不可读,
/// 那种情况仍走 `-32011`,不在此覆盖。
#[tokio::test(flavor = "multi_thread")]
async fn thread_resume_missing_cwd_errors() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();

    let mut cfg = test_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": cwd.to_string_lossy() }});
    h.send(&req.to_string()).await;
    let tid = read_thread_start_response(&mut h, 2).await;

    // 目录从磁盘消失:thread 仍在内存,但 cwd 已不可用。
    std::fs::remove_dir_all(&cwd).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("thread/resume must respond");
    assert_eq!(
        v["error"]["code"], -32602,
        "resume into a vanished cwd must be invalid_params, not a silent fallback: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_ops_target_thread_cwd_not_global_workdir() {
    // 两个不同目录:thread 建在 `d`,全局 workdir 在 `w`。用于锁住
    // rename/delete 走 thread 自身 cwd,而不是全局 workdir / 索引回退。
    let w = tempfile::TempDir::new().unwrap();
    let d = tempfile::TempDir::new().unwrap();
    let w_dir = w.path().canonicalize().unwrap();
    let d_dir = d.path().canonicalize().unwrap();

    let mut cfg = test_config();
    cfg.workdir = w_dir.clone();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    let start = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "thread/start",
        "params": { "cwd": d_dir.to_string_lossy() },
    });
    h.send(&start.to_string()).await;
    let mut tid = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            assert!(v.get("error").is_none(), "thread/start must succeed: {v}");
            tid = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
            break;
        }
    }
    let tid = tid.expect("no thread/start response");

    // 把 `d` 从全局索引移除。此后只有「内存中该 thread 的权威 store」还能
    // 指向 `d`;若 rename/delete 退化成 store_for(索引 → cfg.workdir),
    // 就会落到全局 workdir `w` 上,从而暴露路由回归。
    let remove = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "workspace/remove",
        "params": { "path": d_dir.to_string_lossy() },
    });
    h.send(&remove.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 3);
    assert!(
        v.get("error").is_none(),
        "workspace/remove must succeed: {v}"
    );

    // rename 必须落在 `d`,而不是全局 workdir `w`。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/rename","params":{{"threadId":"{tid}","title":"scoped"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 4);
    assert!(v.get("error").is_none(), "rename must succeed: {v}");

    let in_d = crate::thread_store::ThreadStore::new(&d_dir)
        .load(&tid)
        .expect("load in d must not fail")
        .expect("thread must live in d");
    assert_eq!(in_d.meta.title.as_deref(), Some("scoped"));
    assert!(
        !crate::thread_store::ThreadStore::new(&w_dir).exists(&tid),
        "nothing must leak into the global workdir"
    );

    // delete 同样必须作用在 `d`。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 5);
    assert!(v.get("error").is_none(), "delete must succeed: {v}");
    assert!(
        !crate::thread_store::ThreadStore::new(&d_dir).exists(&tid),
        "thread must be actually deleted in d"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_rename_updates_title() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"my chat"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 3);
    assert!(v.get("error").is_none(), "rename must succeed: {v}");

    let meta = std::fs::read_to_string(
        dir.path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.meta.json")),
    )
    .unwrap();
    assert!(
        meta.contains("my chat"),
        "meta must carry the title: {meta}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_rename_rejects_empty_title() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"   "}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(
        v["error"]["code"], -32602,
        "blank title must be rejected: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_rename_unknown_returns_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/rename","params":{"threadId":"nope","title":"x"}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32011);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_turn_title_comes_from_llm() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(TitleProvider);
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"登录页报错了"}}]}}}}"#
    ))
    .await;
    // 读到 turn/completed 后，meta 标题应已是 LLM 给出的标题。标题生成排在
    // 「本轮已落盘」广播之后（绝不拖住 turn 收尾），故此刻可能先看到兜底标题，
    // 需轮询到期望的 LLM 标题出现（最多 ~1s）再断言。
    let mut title = None;
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        let store = crate::thread_store::ThreadStore::new(dir.path());
        if let Ok(Some(loaded)) = store.load(&tid) {
            if let Some(t) = loaded.meta.title {
                if t == "修复登录报错" {
                    title = Some(t);
                    break;
                }
                title = Some(t);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(title.as_deref(), Some("修复登录报错"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_turn_title_falls_back_when_generation_fails() {
    // 用 RejectingTitleProvider：正文正常、但标题请求（带 system）返回错误。
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let title_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RejectingTitleProvider {
        title_calls: title_calls.clone(),
    });
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"登录页报错了"}}]}}}}"#
    ))
    .await;
    // 等待的是**被断言的信号本身**(标题请求已发出并失败),而不是「标题非空」:
    // 兜底标题在回合开始就由 `touch` 写好,轮询它必然立刻退出,而标题请求发生在
    // 回合收尾之后——满载时它可能还没发出,抢先断言就会把宿主机负载误报成产品
    // bug(本用例正是在 12 路负载下暴露的)。
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while title_calls.load(Ordering::SeqCst) < 1 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let title = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .ok()
        .flatten()
        .and_then(|loaded| loaded.meta.title);
    // 关键：标题生成请求**必须真的发出过且失败**。仅断言标题等于 fallback 是
    // 不具辨识力的——fallback 与 `touch` 写入的字节完全相同，标题请求一次不发
    // 也照样通过（在基线 commit 上同样通过）。这里以命中计数证明失败路径被走到。
    assert!(
        title_calls.load(Ordering::SeqCst) >= 1,
        "a title-generation request must have been attempted (and failed)"
    );
    // 且失败后静默保留兜底标题 —— 即 title_from("登录页报错了")（原样截断）。
    assert_eq!(title.as_deref(), Some("登录页报错了"));
    h.shutdown().await;
}

/// 标题生成失败时**不得**广播 `thread/titleChanged`：磁盘标题仍是兜底截断值，
/// 广播一个未被写入的标题会让客户端显示一个不存在的名字。
#[tokio::test(flavor = "multi_thread")]
async fn failed_title_generation_broadcasts_no_title_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let title_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RejectingTitleProvider {
        title_calls: title_calls.clone(),
    });
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"登录页报错了"}}]}}}}"#
    ))
    .await;

    // 先等标题请求确实发出且失败（与回退用例同款：轮询被断言的信号本身）。
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while title_calls.load(Ordering::SeqCst) < 1 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        title_calls.load(Ordering::SeqCst) >= 1,
        "title request must fire"
    );
    // 读到 turn/completed（收尾已发生），确认此后没有 titleChanged 尾随。
    let _ = read_until_method(&mut h, "turn/completed").await;
    // 负等待：收尾后再给一个短窗口，断言这段里没有任何 titleChanged 帧。
    let mut saw_title_change = false;
    while let Some(v) = h.try_read_value(Duration::from_millis(300)).await {
        if v.get("method").and_then(|m| m.as_str()) == Some("thread/titleChanged") {
            saw_title_change = true;
        }
    }
    assert!(
        !saw_title_change,
        "a failed title generation must not broadcast thread/titleChanged"
    );
    h.shutdown().await;
}

/// 首轮 LLM 标题落盘后必须**主动广播** `thread/titleChanged`，携带权威标题。
///
/// 这是本条修复的核心：`turn/completed` 早于标题落盘发出，桌面端在该通知触发
/// 的 `refreshThreads()` 只能读到兜底截断标题；没有这条广播，LLM 标题要等到一次
/// 无关的列表刷新才可见。断言的是**通知真的发出**且**带的是 LLM 标题**（而非
/// 兜底值 `title_from("登录页报错了")`）。
#[tokio::test(flavor = "multi_thread")]
async fn first_turn_broadcasts_llm_title_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(TitleProvider);
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"登录页报错了"}}]}}}}"#
    ))
    .await;

    // 标题生成排在 turn 收尾广播之后，故要读到它可能跨若干帧（状态、正文、完成）。
    let ev = read_until_method(&mut h, "thread/titleChanged").await;
    assert_eq!(ev["params"]["thread_id"], tid);
    assert_eq!(
        ev["params"]["title"], "修复登录报错",
        "通知必须携带 LLM 标题，而不是兜底截断值"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_delete_removes_files_and_unknown_is_error() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert!(v.get("error").is_none(), "delete must succeed: {v}");
    assert!(
        !dir.path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.meta.json"))
            .exists(),
        "meta must be gone"
    );

    // 再次删除:thread 已完全未知 → -32011。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32011);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_delete_active_thread_removes_it_from_memory() {
    // 用隔离 workdir:删除活跃 thread 时必须能断言磁盘状态。
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    // 等 turn 活跃。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            break;
        }
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut deleted = false;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            assert!(v.get("error").is_none(), "delete must succeed: {v}");
            deleted = true;
            break;
        }
    }
    assert!(deleted, "thread/delete must respond");

    // 留出窗口让「未等待落盘」的实现迟到写入:driver 收到中断后仍会
    // append_turn,若 delete 抢在它之前删文件,日志会被复活。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 删除后磁盘上两个文件都必须消失(活跃 turn 落盘不得复活 .jsonl)。
    let threads_dir = dir.path().join(".yi-agent/threads");
    assert!(
        !threads_dir.join(format!("{tid}.meta.json")).exists(),
        "meta must be gone after deleting an active thread"
    );
    assert!(
        !threads_dir.join(format!("{tid}.jsonl")).exists(),
        "log must be gone after deleting an active thread"
    );

    // 删除后该 thread 已不在内存:再发 turn 应得 -32011。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"x"}}]}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            assert_eq!(
                v["error"]["code"], -32011,
                "deleted thread must be unknown: {v}"
            );
            break;
        }
    }

    // 磁盘无日志 → resume 不能复活已删除的 thread。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":6,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(6)) {
            assert_eq!(
                v["error"]["code"], -32011,
                "resume must not resurrect a deleted thread: {v}"
            );
            break;
        }
    }
    h.shutdown().await;
}

/// 读到下一个指定 method 的通知(忽略其它帧)。
async fn read_until_method(h: &mut Harness, method: &str) -> serde_json::Value {
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
            return v;
        }
    }
    panic!("expected a {method} notification");
}

/// 正常一轮结束后,状态序列为 running → idle。
#[tokio::test(flavor = "multi_thread")]
async fn turn_emits_running_then_idle_status() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    let running = read_until_method(&mut h, "thread/status/updated").await;
    assert_eq!(running["params"]["thread_id"], tid);
    assert_eq!(running["params"]["status"], "running");

    let idle = loop {
        let v = read_until_method(&mut h, "thread/status/updated").await;
        if v["params"]["status"] == "idle" {
            break v;
        }
    };
    assert_eq!(idle["params"]["thread_id"], tid);
    h.shutdown().await;
}

/// 两个 thread 的 turn 可同时在跑:各自的 listAll 状态同为 running。
#[tokio::test(flavor = "multi_thread")]
async fn two_threads_run_turns_concurrently() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let a = read_thread_start_response(&mut h, 2).await;
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
        .await;
    let b = read_thread_start_response(&mut h, 4).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 两个 turn/started 都应到达(无跨 thread 的 -32012)。
    let mut started = std::collections::HashSet::new();
    for _ in 0..20 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            started.insert(v["params"]["thread_id"].as_str().unwrap().to_string());
        }
        if started.contains(&a) && started.contains(&b) {
            break;
        }
    }
    assert!(
        started.contains(&a) && started.contains(&b),
        "both turns must start: {started:?}"
    );

    // 此刻 listAll 里两个 thread 都是 running。
    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
        .await;
    let all = loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(6)) {
            break v;
        }
    };
    let status_of = |id: &str| -> String {
        all["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap())
            .find(|t| t["thread_id"] == id)
            .map(|t| t["status"].as_str().unwrap().to_string())
            .expect("thread must be listed")
    };
    assert_eq!(status_of(&a), "running");
    assert_eq!(status_of(&b), "running");

    // 收尾:中断两个 turn 以便优雅退出。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"turn/interrupt","params":{{"threadId":"{a}"}}}}"#
    ))
    .await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":8,"method":"turn/interrupt","params":{{"threadId":"{b}"}}}}"#
    ))
    .await;
    h.shutdown().await;
}

/// 审批超时后状态必须离开 awaiting_approval(前端据此清掉残留审批框)。
#[tokio::test(flavor = "multi_thread")]
async fn approval_timeout_leaves_awaiting_status() {
    let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 应先看到 awaiting_approval。
    let mut saw_awaiting = false;
    // 之后必须离开 awaiting_approval(超时 → 回 running → idle)。
    let mut left_awaiting = false;
    for _ in 0..60 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("thread/status/updated") {
            match v["params"]["status"].as_str().unwrap() {
                "awaiting_approval" => saw_awaiting = true,
                _ if saw_awaiting => {
                    left_awaiting = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(saw_awaiting, "must enter awaiting_approval");
    assert!(left_awaiting, "must leave awaiting_approval after timeout");
    h.shutdown().await;
}

/// cold thread（未 resume）在 listAll 里状态为 idle。
#[tokio::test(flavor = "multi_thread")]
async fn cold_thread_reports_idle() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await; // 已 start，但未 resume、未起 turn
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
        .await;
    let all = loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            break v;
        }
    };
    let listed = all["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap())
        .find(|t| t["thread_id"] == tid.as_str())
        .expect("thread must be listed");
    assert_eq!(listed["status"], "idle");
    h.shutdown().await;
}

#[tokio::test]
async fn process_list_is_empty_for_a_fresh_thread_and_for_an_unknown_one() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;

    // 新 thread:没有进程,列表为空(不是错误)。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
    ))
    .await;
    let listed = h.read_value().await;
    assert_eq!(listed["id"], 3);
    assert!(listed["error"].is_null(), "{listed}");
    assert_eq!(listed["result"]["processes"].as_array().unwrap().len(), 0);

    // 未知 thread:仍返回空列表而非错误(切走再切回是正常路径)。
    h.send(
        r#"{"jsonrpc":"2.0","id":4,"method":"process/list","params":{"thread_id":"thread-nope"}}"#,
    )
    .await;
    let unknown = h.read_value().await;
    assert_eq!(unknown["id"], 4);
    assert!(unknown["error"].is_null(), "{unknown}");
    assert_eq!(unknown["result"]["processes"].as_array().unwrap().len(), 0);

    h.shutdown().await;
}

#[tokio::test]
async fn process_read_reports_an_unknown_process_as_an_error() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
    ))
    .await;
    let bad = h.read_value().await;
    assert_eq!(bad["id"], 3);
    assert!(bad["result"].is_null(), "{bad}");
    assert!(
        bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("process not found")
    );

    h.shutdown().await;
}

/// 生效的那一份 manager 才被看见:thread 必须采用工厂给出的 manager,而不是
/// 自建一份——否则进程面板显示空列表,而 agent 明明能起进程(设计 §4.2 的陷阱)。
///
/// 顺带验证 `process/read` 的游标增量语义:两次读不重不漏。
///
/// 隔离:`cfg.provider` 设成非 anthropic/openai,委派装配会在 `worker_factory` 处
/// 确定性失败,`thread/start` 因而**不会**经 `wrap_for_delegation` 换掉工厂给出的
/// manager——本测试的前提(工厂那一份生效)才成立,且与 `/tmp` 残留状态无关。
#[tokio::test]
async fn process_list_and_read_observe_the_managers_the_factory_handed_over() {
    use std::sync::Arc;

    // 修正:既有 API `ProcessManager::new` 直接返回 `Arc<Self>`,不能再套 `Arc::new`。
    let held: Arc<yi_agent_tools::ProcessManager> =
        yi_agent_tools::ProcessManager::new(std::env::temp_dir());
    let for_factory = Arc::clone(&held);

    let mut cfg = test_config();
    cfg.provider = "t5-isolated".to_string();
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            let mut built = build_test_agent(session, cwd, mode)?;
            // 这一份才是「生效」的:thread 必须采用它。
            built.process_manager = Arc::clone(&for_factory);
            Ok(built)
        },
        PERMISSION_TIMEOUT,
    );
    let thread_id = start_thread(&mut h).await;

    // ready_pattern 让 start() 等到输出出现才返回,断言因此是确定性的。
    let started = held
        .start(yi_agent_tools::ProcessStartOptions {
            command: "printf alpha".into(),
            name: Some("t4-probe".into()),
            cwd: None,
            env: Default::default(),
            on_exit: Default::default(),
            ready_pattern: Some("alpha".into()),
            ready_timeout_sec: Some(5),
        })
        .await
        .expect("start");

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
    ))
    .await;
    let listed = read_response(&mut h, 3).await;
    assert!(listed["error"].is_null(), "{listed}");
    let names: Vec<&str> = listed["result"]["processes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"t4-probe"),
        "thread must see the held manager: {listed}"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
        started.process_id
    ))
    .await;
    let first = read_response(&mut h, 4).await;
    assert!(first["error"].is_null(), "{first}");
    assert!(
        first["result"]["stdout"]
            .as_str()
            .unwrap()
            .contains("alpha"),
        "{first}"
    );
    let cursor = first["result"]["next_cursor"].as_u64().unwrap();
    assert!(cursor > 0, "{first}");

    // 从上一轮游标继续读:没有新输出(不重不漏)。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}","cursor":{cursor}}}}}"#,
        started.process_id
    ))
    .await;
    let second = read_response(&mut h, 5).await;
    assert!(second["error"].is_null(), "{second}");
    assert_eq!(second["result"]["stdout"].as_str().unwrap(), "", "{second}");

    let _ = held.shutdown().await;
    h.shutdown().await;
}

#[tokio::test]
async fn process_kill_reports_an_unknown_process_as_an_error() {
    let mut h = Harness::new();
    let thread_id = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
    ))
    .await;
    let bad = read_response(&mut h, 3).await;
    assert!(bad["result"].is_null(), "{bad}");
    assert!(!bad["error"].is_null());

    h.shutdown().await;
}

/// kill 真的能终止进程,并把状态推到 `exited`/`killed`。
///
/// 隔离:`cfg.provider` 设成非 anthropic/openai 时,委派装配会在 `worker_factory`
/// 处**确定性**失败(不需要起 daemon、也不看 `/tmp` 残留状态),于是 `thread/start`
/// 不会经 `wrap_for_delegation` 把工厂给出的 manager 换成新建的那一份——工厂那一份
/// 才是本 thread 生效的 manager,正是本测试要验证的对象。
#[tokio::test]
async fn process_kill_terminates_a_held_manager_process() {
    use std::sync::Arc;

    for run in 0..8 {
        // 修正:既有 API `ProcessManager::new` 已返回 `Arc<Self>`,不能再套 `Arc::new`。
        let held: Arc<yi_agent_tools::ProcessManager> =
            yi_agent_tools::ProcessManager::new(std::env::temp_dir());
        let for_factory = Arc::clone(&held);
        let mut cfg = test_config();
        cfg.provider = "t5-isolated".to_string();
        let mut h = Harness::with_config(
            cfg,
            move |session, cwd, mode| {
                let mut built = build_test_agent(session, cwd, mode)?;
                built.process_manager = Arc::clone(&for_factory);
                Ok(built)
            },
            PERMISSION_TIMEOUT,
        );
        let thread_id = start_thread(&mut h).await;

        let started = held
            .start(yi_agent_tools::ProcessStartOptions {
                command: "sleep 300".into(),
                name: Some("t5-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: None,
                ready_timeout_sec: None,
            })
            .await
            .expect("start");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
            started.process_id
        ))
        .await;
        let killed = read_response(&mut h, 3).await;
        assert!(killed["error"].is_null(), "run {run}: {killed}");
        assert_eq!(killed["result"]["ok"], true, "run {run}");

        // 状态必须落到终态之一,而不是仍显示 running。
        let deadline = std::time::Instant::now() + EVENT_WAIT;
        loop {
            let snap = held
                .list()
                .into_iter()
                .find(|p| p.process_id == started.process_id)
                .expect("process must still be listed after kill");
            let state = serde_json::to_value(&snap.status).unwrap();
            let label = state["state"].as_str().unwrap_or("").to_string();
            if label == "killed" || label == "exited" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process never reached a terminal state: {state}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = held.shutdown().await;
        h.shutdown().await;
    }
}

/// 回归(Task 5 缺陷):resume 一个**仍在内存的活 thread** 时,该 thread 生效的
/// manager 会被新工厂产出的一份替换;守望者必须跟着换到新 manager 上。
///
/// 修复前:`thread/resume` 的守望者启动被 `if !process_watches.contains_key(..)`
/// 拦住,仍旧订阅**旧的、已被丢弃**的 manager。于是 `process/list` 读得到新
/// manager 的进程,`process/updated` 却永远不来——面板静默失联。本测试正是抓这个:
/// 对新建后未起 turn 的活 thread 直接 resume(得到生效 manager 数组下标 1),在其上
/// 起进程,断言能从客户端流里读到 process_id 匹配的 `process/updated`。
///
/// 隔离:`cfg.provider` 设成非 anthropic/openai,委派装配确定性失败,`thread/start`
/// 与 `thread/resume` 都不会经 `wrap_for_delegation` 换掉工厂给出的 manager,因此
/// 「工厂第 N 次产出的 manager 即该路径生效的 manager」成立。
///
/// 收敛读带超时(见 `await_notification`),修复前是「等不到」而非挂死。
#[tokio::test(flavor = "multi_thread")]
async fn resume_of_a_live_thread_rebinds_process_watcher_to_the_effective_manager() {
    use std::sync::Arc;

    // 工厂每次被调用都把「本次生效的 manager」推进这里:start 产出下标 0,
    // resume 产出下标 1(即 resume 后生效的那一份)。
    let seen: Arc<std::sync::Mutex<Vec<Arc<yi_agent_tools::ProcessManager>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let for_factory = Arc::clone(&seen);

    let mut cfg = test_config();
    cfg.provider = "t5-isolated".to_string();
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            let built = build_test_agent(session, cwd, mode)?;
            let manager = Arc::clone(&built.process_manager);
            for_factory.lock().unwrap().push(manager);
            Ok(built)
        },
        PERMISSION_TIMEOUT,
    );
    let thread_id = start_thread(&mut h).await;

    // 活 thread:未起 turn 时直接 resume,走的是「仍在内存中的活 thread」那条分支。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{thread_id}"}}}}"#
    ))
    .await;
    let resumed = read_response(&mut h, 3).await;
    assert_eq!(
        resumed["result"]["thread_id"],
        thread_id.as_str(),
        "{resumed}"
    );

    //resume 之后该 thread 生效的 manager 就是工厂第二次产出的那一份。
    let managers = seen.lock().unwrap().clone();
    assert_eq!(
        managers.len(),
        2,
        "factory must run once for start and once for resume: {}",
        managers.len()
    );
    let effective = Arc::clone(&managers[1]);

    // 在生效 manager 上起一个进程;守望者若正确重订,应推送 process/updated。
    let started = effective
        .start(yi_agent_tools::ProcessStartOptions {
            command: "sleep 300".into(),
            name: Some("t5-resume-probe".into()),
            cwd: None,
            env: Default::default(),
            on_exit: Default::default(),
            ready_pattern: None,
            ready_timeout_sec: None,
        })
        .await
        .expect("start");

    let note = await_notification(&mut h, "process/updated", Duration::from_secs(10))
        .await
        .unwrap_or_else(|| {
            panic!(
                "resume 后仍未重订守望者:等不到 process/updated(process_id={})",
                started.process_id
            )
        });
    assert_eq!(
        note["params"]["process_id"],
        started.process_id.as_str(),
        "{note}"
    );
    assert_eq!(note["params"]["thread_id"], thread_id.as_str(), "{note}");
    // `start` 返回时进程已在运行,守望者至少应报出一个非终态标签。
    let state = note["params"]["state"].as_str().unwrap_or("");
    assert!(
        matches!(state, "starting" | "running" | "ready"),
        "unexpected state on a running process: {note}"
    );

    let _ = effective.shutdown().await;
    h.shutdown().await;
}

fn pin_test_config(dir: &std::path::Path) -> RuntimeConfig {
    let mut c = test_config();
    c.workdir = dir.to_path_buf();
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn set_pinned_puts_thread_on_top_of_list_all_pinned() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
    h.send(&start.to_string()).await;
    let tid = read_thread_start_response(&mut h, 2).await;

    let pin = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setPinned",
        "params":{"threadId": tid, "pinned": true}});
    h.send(&pin.to_string()).await;
    let resp = read_response(&mut h, 3).await;
    assert!(
        resp.get("error").is_none(),
        "setPinned must succeed: {resp}"
    );

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    let pinned = v["result"]["pinned"].as_array().expect("顶层 pinned 数组");
    assert_eq!(pinned.len(), 1, "恰一个置顶: {v}");
    assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tid);
    assert_eq!(pinned[0]["pinned"], true);

    // 仍在原分组内,且带 pinned:true。
    let all: Vec<&serde_json::Value> = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap().iter())
        .collect();
    let me = all
        .iter()
        .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
        .expect("in group");
    assert_eq!(me["pinned"], true);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_pinned_false_removes_from_pinned_list() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;
    let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
    h.send(&start.to_string()).await;
    let tid = read_thread_start_response(&mut h, 2).await;

    for (id, pinned) in [(3u64, true), (4u64, false)] {
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/setPinned",
            "params":{"threadId": tid, "pinned": pinned}});
        h.send(&req.to_string()).await;
        let r = read_response(&mut h, id).await;
        assert!(r.get("error").is_none(), "{r}");
    }
    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["result"]["pinned"].as_array().unwrap().len(), 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_pinned_rejects_non_boolean_and_unknown_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":"yes"}}"#).await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["code"], -32602, "非布尔 → invalid_params: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":true}}"#).await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["error"]["code"], -32011, "未知 id → unknown_thread: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reorder_pinned_rewrites_order_and_rejects_bad_input() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    let mut tids = Vec::new();
    for id in [2u64, 3u64] {
        let start =
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start","params":{}});
        h.send(&start.to_string()).await;
        tids.push(read_thread_start_response(&mut h, id).await);
    }
    // 两个都置顶(后置顶的 tids[1] 在顶)。
    for (i, tid) in tids.iter().enumerate() {
        let req = serde_json::json!({"jsonrpc":"2.0","id":10+i as u64,"method":"thread/setPinned",
            "params":{"threadId": tid, "pinned": true}});
        h.send(&req.to_string()).await;
        let r = read_response(&mut h, 10 + i as u64).await;
        assert!(r.get("error").is_none(), "{r}");
    }
    // 反转顺序：tids[0] 放到最顶。
    let rev = serde_json::json!({"jsonrpc":"2.0","id":20,"method":"thread/reorderPinned",
        "params":{"threadIds": tids}});
    h.send(&rev.to_string()).await;
    let r = read_response(&mut h, 20).await;
    assert!(r.get("error").is_none(), "reorder must succeed: {r}");

    h.send(r#"{"jsonrpc":"2.0","id":21,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 21).await;
    let pinned = v["result"]["pinned"].as_array().unwrap();
    assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tids[0]);

    // 含未置顶 id → invalid_params。
    let bad = serde_json::json!({"jsonrpc":"2.0","id":22,"method":"thread/reorderPinned",
        "params":{"threadIds": ["thread-nope"]}});
    h.send(&bad.to_string()).await;
    let v = read_response(&mut h, 22).await;
    assert_eq!(
        v["error"]["code"], -32602,
        "未置顶 id → invalid_params: {v}"
    );

    // 重复 id → invalid_params。
    let dup = serde_json::json!({"jsonrpc":"2.0","id":23,"method":"thread/reorderPinned",
        "params":{"threadIds": [tids[0].clone(), tids[0].clone()]}});
    h.send(&dup.to_string()).await;
    let v = read_response(&mut h, 23).await;
    assert_eq!(v["error"]["code"], -32602, "重复 id → invalid_params: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_tag_persists_and_reports_in_list() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;

    let set = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setTag",
        "params":{"threadId": tid, "tag": "red"}});
    h.send(&set.to_string()).await;
    let resp = read_response(&mut h, 3).await;
    assert!(resp.get("error").is_none(), "setTag 应成功: {resp}");

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    let me = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap().iter())
        .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
        .expect("in group");
    assert_eq!(me["tag"], "red");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_tag_null_clears() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;

    for (id, tag) in [
        (3u64, serde_json::json!("green")),
        (4, serde_json::Value::Null),
    ] {
        let req = serde_json::json!({"jsonrpc":"2.0","id": id, "method":"thread/setTag",
            "params":{"threadId": tid, "tag": tag}});
        h.send(&req.to_string()).await;
        let resp = read_response(&mut h, id).await;
        assert!(resp.get("error").is_none(), "setTag 应成功: {resp}");
    }

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    let me = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap().iter())
        .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
        .expect("in group");
    assert!(me["tag"].is_null(), "清除后应为 null: {me}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_tag_rejects_unknown_color_and_unknown_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    // 先校验颜色：非法值一律 -32602，无论 thread 是否存在。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/setTag","params":{"threadId":"thread-x","tag":"chartreuse"}}"#).await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/setTag","params":{"threadId":"thread-x","tag":"red"}}"#).await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(-32011),
        "合法颜色 + 未知 thread 应为 unknown_thread: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_tag_broadcasts_tag_changed() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;

    let set = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setTag",
        "params":{"threadId": tid, "tag": "purple"}});
    h.send(&set.to_string()).await;
    let _ = read_response(&mut h, 3).await;

    let ev = read_until_method(&mut h, "thread/tagChanged").await;
    assert_eq!(ev["params"]["thread_id"], tid);
    assert_eq!(ev["params"]["tag"], "purple");
    h.shutdown().await;
}

/// 手机（`Control`）必须能删除会话：长按菜单的「删除」与重命名/置顶同档，
/// 且 `thread/rename` 早已是 `Control` 放行。原先把 `thread/delete` 卡在
/// `Admin` 让手机长按菜单的删除项结构性必然报 `-32014`。
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_may_delete_a_thread() {
    let mut h = Harness::with_scope(Scope::Control).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    assert!(v["result"].is_object(), "Control 客户端必须能删除会话: {v}");
    h.shutdown().await;
}

/// 最低档 `Observe` 仍必须被拒：只读客户端不得删除会话。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_cannot_delete_a_thread() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["error"]["code"], -32014, "Observe 客户端必须被拒: {v}");
    h.shutdown().await;
}

/// 手机（Control）必须能切换会话权限模式：设计 §2 的「引导去桌面端授权」
/// 入口在代码里并不存在，admin-only 会让手机端的 YOLO 切换永远是死路。
/// 与 `thread/setModel` 同档（Control）。
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_may_switch_permission_mode() {
    let mut h = Harness::with_scope(Scope::Control).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    assert!(
        v["result"].is_object(),
        "Control 客户端必须能切换到 yolo: {v}"
    );
    h.shutdown().await;
}

/// 最低档 `Observe` 仍必须被拒：读-only 客户端不得放开沙箱。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_cannot_switch_permission_mode() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["error"]["code"], -32014, "Observe 客户端必须被拒: {v}");
    h.shutdown().await;
}

/// `thread/start` 响应必须直接带权威 `permission_mode`（新线程恒 normal）。
/// 客户端靠它建起 mode chip,不必再回读 listAll(那条路径异步、可失败、可竞态)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_response_carries_permission_mode() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":20,"method":"thread/start","params":{}}"#)
        .await;
    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(20)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("thread/start response");
    assert_eq!(
        resp["result"]["permission_mode"].as_str(),
        Some("normal"),
        "thread/start must carry permission_mode: {resp}"
    );
    h.shutdown().await;
}

/// `thread/resume` 响应必须带该会话**持久化**的模式:手机打开一个电脑上
/// 已置 yolo 的会话,必须一次请求就看到 yolo,而不是「未知 / 等回读」。
#[tokio::test(flavor = "multi_thread")]
async fn thread_resume_response_carries_persisted_permission_mode() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    // 切 yolo（走真实 RPC，确保落盘）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            break;
        }
    }
    // 再 resume 同一 thread。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":30,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..24 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(30)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("thread/resume response");
    assert_eq!(
        resp["result"]["permission_mode"].as_str(),
        Some("yolo"),
        "thread/resume must carry the persisted mode: {resp}"
    );
    h.shutdown().await;
}

/// 切换模式必须**广播** `thread/permissionModeChanged`:这是跨客户端同步
/// （桌面改 → 手机同步、手机改 → 桌面 chip 同步）的唯一通道。
#[tokio::test(flavor = "multi_thread")]
async fn switching_permission_mode_broadcasts_the_change() {
    let mut h = Harness::new(); // stdio = Admin,但广播与 scope 无关
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
    ))
    .await;

    // 响应与通知都在流上,顺序不保证;分别收敛。
    let mut response: Option<serde_json::Value> = None;
    let mut changed: Option<serde_json::Value> = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            response = Some(v);
        } else if v.get("method").and_then(|m| m.as_str()) == Some("thread/permissionModeChanged") {
            changed = Some(v);
        }
        if response.is_some() && changed.is_some() {
            break;
        }
    }
    assert!(response.is_some(), "setPermissionMode 必须有响应");
    let changed = changed.expect("must broadcast thread/permissionModeChanged");
    assert_eq!(changed["params"]["thread_id"].as_str(), Some(tid.as_str()));
    assert_eq!(changed["params"]["mode"].as_str(), Some("yolo"));
    h.shutdown().await;
}

/// The desktop stdio client is `Admin`: the same call must go through.
#[tokio::test(flavor = "multi_thread")]
async fn an_admin_client_may_delete_a_thread() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    assert!(
        v["result"].is_object(),
        "admin client must be allowed to delete: {v}"
    );
    h.shutdown().await;
}

/// `model/list` 走宿主 dispatch 时也不回传明文 key:证明接线用的是注入的
/// 隔离清单路径,而非开发者真实的 `~/.yi-agent/models.json`。
#[tokio::test(flavor = "multi_thread")]
async fn model_list_over_dispatch_masks_the_key() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":9,"method":"model/upsert","params":{"name":"A","provider":"anthropic","api_url":"https://a","model":"m","api_key":"sk-secret-1234"}}"#,
    )
    .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["result"]["ok"], true, "upsert must land: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"model/list","params":{}}"#)
        .await;
    let v = read_response(&mut h, 10).await;
    let text = v.to_string();
    assert!(
        !text.contains("sk-secret-1234"),
        "the raw key must never cross the wire: {text}"
    );
    assert_eq!(v["result"]["models"][0]["has_key"], true);
    assert_eq!(v["result"]["models"][0]["api_key_masked"], "••••1234");

    // An unknown default is the structured `model_not_found`, over the wire too.
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"model/setDefault","params":{"name":"nope"}}"#)
        .await;
    let v = read_response(&mut h, 11).await;
    assert_eq!(v["error"]["code"], -32025);
    assert_eq!(v["error"]["data"]["code"], "model_not_found");
    h.shutdown().await;
}

/// An `Observe`-only client may read the (masked) catalog but must be refused
/// every write. Production stdio is `Admin`; a paired phone defaults to
/// `Control`, which is *allowed* to write, so this uses the lowest tier.
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_may_list_but_not_write_models() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"model/list","params":{}}"#)
        .await;
    let v = read_response(&mut h, 9).await;
    assert!(
        v["result"]["models"].is_array(),
        "Observe may read the masked catalog: {v}"
    );

    h.send(
        r#"{"jsonrpc":"2.0","id":10,"method":"model/upsert","params":{"name":"A","provider":"anthropic","api_url":"https://a","model":"m","api_key":"k"}}"#,
    )
    .await;
    let v = read_response(&mut h, 10).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "writes require Control, Observe must be refused: {v}"
    );
    h.shutdown().await;
}

/// `onboarding/status` 只读,`Observe` 即可:空白机器上「需要引导」且未 dismiss。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_status_is_readable_and_reports_needed() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/status","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["needed"], true, "got {v}");
    assert_eq!(v["result"]["dismissed"], false, "got {v}");
}

/// `onboarding/apply` 落 `.env` 与清单;随后 `status` 立刻翻成 ready。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_apply_lands_the_env_and_catalog() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply","params":{"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-x"}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["env_written"], true, "got {v}");
    assert_eq!(v["result"]["imported"], true, "got {v}");

    // 状态随即翻成 ready。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"onboarding/status","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["needed"], false, "got {v}");
}

/// `onboarding/test` 用注入的临时 provider 真发一次最小请求。桩必须回
/// **SSE** 流(`text/event-stream`)——provider 消费的是流,不是一次性 JSON。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_test_probes_the_endpoint() {
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let mut h = Harness::new();
    initialize(&mut h).await;
    let req = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"onboarding/test",
        "params":{"provider":"anthropic","model":"m",
                  "api_url": server.uri(), "api_key":"sk-x"}
    });
    h.send(&req.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
}

/// `onboarding/apply` 是写操作:最低权 `Observe` 必须被拒(-32014)。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_cannot_apply_onboarding() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply","params":{"provider":"openai","model":"m","api_url":"","api_key":"k"}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32014, "got {v}");
}

/// 经真实 `model/upsert` 分发把一条模型写进 harness 的隔离清单,返回其响应。
async fn upsert_model(
    h: &mut Harness,
    id: u64,
    name: &str,
    provider: &str,
    model: &str,
) -> serde_json::Value {
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"model/upsert","params":{{"name":"{name}","provider":"{provider}","api_url":"https://{name}.example","model":"{model}","api_key":"key-{name}"}}}}"#
    ))
    .await;
    read_response(h, id).await
}

/// `thread/setModel` 必须落盘 `model_ref` 与解析出的生效串,并在响应里回生效串;
/// 之后 `thread/resume` 从盘上读回的 meta 与之一致。
#[tokio::test(flavor = "multi_thread")]
async fn thread_set_model_persists_and_reports_the_effective_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    assert_eq!(
        upsert_model(&mut h, 10, "A", "anthropic", "model-a").await["result"]["ok"],
        true
    );
    assert_eq!(
        upsert_model(&mut h, 11, "B", "openai", "model-b").await["result"]["ok"],
        true
    );
    h.send(r#"{"jsonrpc":"2.0","id":12,"method":"model/setDefault","params":{"name":"A"}}"#)
        .await;
    assert_eq!(read_response(&mut h, 12).await["result"]["ok"], true);

    let tid = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setModel","params":{{"threadId":"{tid}","name":"B"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(v["result"]["ok"], true, "setModel must succeed: {v}");
    assert_eq!(
        v["result"]["model"], "model-b",
        "the response must report the resolved effective model string: {v}"
    );

    // resume 从盘上回放:meta 必须已带 ref 与生效串。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":21,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let _ = read_response(&mut h, 21).await;

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("setModel must persist meta");
    assert_eq!(loaded.meta.model_ref.as_deref(), Some("B"));
    assert_eq!(loaded.meta.model, "model-b");
    h.shutdown().await;
}

/// `thread/setModel` 的 `name: null` 清除覆盖:meta 的 `model_ref` 归 `None`,
/// 生效串回落到清单全局默认(A)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_set_model_to_null_clears_the_override() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    upsert_model(&mut h, 10, "A", "anthropic", "model-a").await;
    upsert_model(&mut h, 11, "B", "openai", "model-b").await;
    h.send(r#"{"jsonrpc":"2.0","id":12,"method":"model/setDefault","params":{"name":"A"}}"#)
        .await;
    read_response(&mut h, 12).await;

    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setModel","params":{{"threadId":"{tid}","name":"B"}}}}"#
    ))
    .await;
    assert_eq!(
        read_response(&mut h, 20).await["result"]["model"],
        "model-b"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":21,"method":"thread/setModel","params":{{"threadId":"{tid}","name":null}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 21).await;
    assert_eq!(v["result"]["ok"], true, "clearing must succeed: {v}");
    assert_eq!(v["result"]["model"], "model-a");

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .unwrap()
        .expect("thread persists");
    assert_eq!(loaded.meta.model_ref, None, "the override must be cleared");
    assert_eq!(loaded.meta.model, "model-a");
    h.shutdown().await;
}

/// 未知的显示名必须回 `model_not_found`,且**零副作用**:meta 不变、会话不中断
/// (随后一次合法的 setModel 仍能成功)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_set_model_to_unknown_name_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    upsert_model(&mut h, 10, "A", "anthropic", "model-a").await;
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"model/setDefault","params":{"name":"A"}}"#)
        .await;
    read_response(&mut h, 11).await;

    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setModel","params":{{"threadId":"{tid}","name":"nope"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(
        v["error"]["code"], -32025,
        "unknown name → model_not_found: {v}"
    );
    assert_eq!(v["error"]["data"]["code"], "model_not_found");

    // 零副作用:meta 原样(仍无覆盖、生效串仍是 cfg 兜底)。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .unwrap()
        .expect("thread persists");
    assert_eq!(loaded.meta.model_ref, None);
    assert_eq!(loaded.meta.model, "test-model");

    // 会话未被拒绝的请求打断:随后一次合法 setModel 仍成功。
    upsert_model(&mut h, 21, "B", "openai", "model-b").await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":22,"method":"thread/setModel","params":{{"threadId":"{tid}","name":"B"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 22).await;
    assert_eq!(
        v["result"]["model"], "model-b",
        "the session must stay usable: {v}"
    );
    h.shutdown().await;
}

/// `thread/setReasoningEffort` 必须落盘 meta 的 `reasoning_effort` 并广播
/// `thread/reasoningEffortChanged`(跨客户端同步 effort 控件的唯一通道)。
/// 形状照 `thread_set_model_persists_and_reports_the_effective_model`。
#[tokio::test(flavor = "multi_thread")]
async fn thread_set_reasoning_effort_persists_and_broadcasts() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 只订阅这一个 thread:内容层/列表层的过滤都不能把它挡掉
    // (effort 变更必须像 modelChanged 一样恒推)。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/subscribe","params":{{"threadIds":["{tid}"]}}}}"#
    ))
    .await;
    assert_eq!(
        read_response(&mut h, 9).await["result"]["subscribed"][0].as_str(),
        Some(tid.as_str())
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"high"}}}}"#
    ))
    .await;

    // 响应与广播都在流上,顺序不保证;分别收敛。
    let mut response: Option<serde_json::Value> = None;
    let mut changed: Option<serde_json::Value> = None;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(20)) {
            response = Some(v);
        } else if v.get("method").and_then(|m| m.as_str()) == Some("thread/reasoningEffortChanged")
        {
            changed = Some(v);
        }
        if response.is_some() && changed.is_some() {
            break;
        }
    }
    let response = response.expect("setReasoningEffort must respond");
    assert_eq!(response["result"]["ok"], true, "got {response}");
    assert_eq!(
        response["result"]["effort"], "high",
        "the response must echo the applied effort: {response}"
    );
    let changed = changed.expect("must broadcast thread/reasoningEffortChanged");
    assert_eq!(changed["params"]["thread_id"].as_str(), Some(tid.as_str()));
    assert_eq!(changed["params"]["effort"].as_str(), Some("high"));

    // 落盘:resume 走的正是这份 meta。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("setReasoningEffort must persist meta");
    assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("high"));
    h.shutdown().await;
}

/// 与 `thread/setModel` 同档:切 effort 会改推理行为(计费/能力面),
/// `Observe` 客户端必须被拒 `-32014`。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_cannot_set_reasoning_effort() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{"threadId":"t1","effort":"high"}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32014, "got {v}");
    h.shutdown().await;
}

/// 档位必须在七档白名单内:非法值 `-32602`,且**零副作用**(不落盘、不广播)。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_effort_value_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"bogus"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(v["error"]["code"], -32602, "got {v}");

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert_eq!(
        loaded.meta.reasoning_effort, None,
        "rejection must not write"
    );
    h.shutdown().await;
}

/// `thread/setPreserveThinking` 必须落盘 meta 的 `preserve_thinking` 并广播
/// `thread/preserveThinkingChanged`(跨客户端同步开关的唯一通道)。形状照
/// `thread_set_reasoning_effort_persists_and_broadcasts`。
#[tokio::test(flavor = "multi_thread")]
async fn thread_set_preserve_thinking_persists_and_broadcasts() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/subscribe","params":{{"threadIds":["{tid}"]}}}}"#
    ))
    .await;
    assert_eq!(
        read_response(&mut h, 9).await["result"]["subscribed"][0].as_str(),
        Some(tid.as_str())
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}","preserve":false}}}}"#
    ))
    .await;

    let mut response: Option<serde_json::Value> = None;
    let mut changed: Option<serde_json::Value> = None;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(20)) {
            response = Some(v);
        } else if v.get("method").and_then(|m| m.as_str()) == Some("thread/preserveThinkingChanged")
        {
            changed = Some(v);
        }
        if response.is_some() && changed.is_some() {
            break;
        }
    }
    let response = response.expect("setPreserveThinking must respond");
    assert_eq!(response["result"]["ok"], true, "got {response}");
    assert_eq!(
        response["result"]["preserve"], false,
        "the response must echo the applied flag: {response}"
    );
    let changed = changed.expect("must broadcast thread/preserveThinkingChanged");
    assert_eq!(changed["params"]["thread_id"].as_str(), Some(tid.as_str()));
    assert_eq!(changed["params"]["preserve"], false);

    // 落盘:resume 走的正是这份 meta。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("setPreserveThinking must persist meta");
    assert!(
        !loaded.meta.preserve_thinking,
        "the flag must be persisted to disk"
    );
    h.shutdown().await;
}

/// 与 `thread/setReasoningEffort` 同档:`Observe` 客户端必须被拒 `-32014`。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_cannot_set_preserve_thinking() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":20,"method":"thread/setPreserveThinking","params":{"threadId":"t1","preserve":false}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32014, "got {v}");
    h.shutdown().await;
}

/// `preserve` 必须是 JSON 布尔:缺失/null/字符串/数字一律 `-32602`,且**零副作用**
/// (不落盘、不广播)。这里刻意不静默默认——静默默认会让调用方以为开关已按其所愿
/// 设置,而实际可能停在相反的值。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_preserve_value_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 订阅本会话：若拒绝路径错误地广播了变更，订阅者会收到，下面的
    // `try_read_value` 就能捕获。未订阅则内容层通知会被过滤，断言失去意义。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":8,"method":"thread/subscribe","params":{{"threadIds":["{tid}"]}}}}"#
    ))
    .await;
    assert_eq!(
        read_response(&mut h, 8).await["result"]["subscribed"][0].as_str(),
        Some(tid.as_str())
    );
    for (i, raw) in [
        format!(
            r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}"}}}}"#
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":21,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}","preserve":null}}}}"#
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":22,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}","preserve":"yes"}}}}"#
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":23,"method":"thread/setPreserveThinking","params":{{"threadId":"{tid}","preserve":1}}}}"#
        ),
    ]
    .into_iter()
    .enumerate()
    {
        h.send(&raw).await;
        let v = read_response(&mut h, 20 + i as u64).await;
        assert_eq!(v["error"]["code"], -32602, "got {v}");
    }

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert!(
        loaded.meta.preserve_thinking,
        "rejection must not write: the default must stay true"
    );
    // 「零副作用」的另一半：拒绝路径**不得**广播变更。四次非法请求后不该有
    // 任何 `thread/preserveThinkingChanged`（订阅者仍连着，故若广播会收到）。
    while let Some(v) = h.try_read_value(Duration::from_millis(150)).await {
        assert_ne!(
            v.get("method").and_then(|m| m.as_str()),
            Some("thread/preserveThinkingChanged"),
            "a rejected request must not broadcast a change: {v}"
        );
    }
    h.shutdown().await;
}

/// 评审修正(important):显式清除覆盖后,解析链第 2 级 —— **条目默认** ——
/// 必须生效,而不是落到「未设置」。
///
/// 同一份 meta(无覆盖 + 条目默认 `low`)在两条路径上必须落到同一结果:
/// 活线程的清理由 driver 把 `gen_params.reasoning_effort` 置 `None`、provider
/// 再自补其烤入的条目档;冷线程 resume 走工厂重建,工厂**不得**把
/// `resolve_effective` 刚带出的条目默认覆盖掉。故 resume 报出的生效值必须是
/// 条目默认 `low`,而非 `null`(修此前「重启后两手感」的内部不一致)。
#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_override_falls_back_to_the_entry_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    cfg.provider = "openai".to_string();
    cfg.api_url = "https://a.example".to_string();
    cfg.api_key = "key-a".to_string();
    cfg.model = "model-a".to_string();
    // 注入清单:全局默认即条目 A,且 A 带默认档 "low"(三个字段同源,免动 HOME)。
    let mut catalog = model_catalog_entries(Some("A"));
    catalog.models[0].reasoning_effort = Some("low".into());
    let factory = production_factory(cfg.clone(), test_theme(), fixed_watcher(Arc::new(catalog)));
    let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // (c) 无覆盖 + 条目默认 low → start 必须上报实际运行的 "low"
    // (此前硬编码 null,与本会话真正跑的值不符)。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let mut start = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            start = Some(v);
            break;
        }
    }
    let start = start.expect("thread/start must respond");
    let tid = start["result"]["thread_id"].as_str().unwrap().to_string();
    assert_eq!(
        start["result"]["reasoning_effort"], "low",
        "start must report the effort the built agent runs: {start}"
    );

    // (a) 设覆盖 high → 落盘。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"high"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true, "got {v}");

    // (b) 清除覆盖(effort: null)。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":null}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
    assert!(
        v["result"]["effort"].is_null(),
        "the response must echo the cleared override: {v}"
    );
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert_eq!(
        loaded.meta.reasoning_effort, None,
        "a clear must drop the persisted override"
    );

    // (b) 冷 resume:工厂按 meta(无覆盖)重建 ⇒ 条目默认 low 必须生效。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resumed = None;
    for _ in 0..24 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            resumed = Some(v);
            break;
        }
    }
    let resumed = resumed.expect("thread/resume must respond");
    assert_eq!(
        resumed["result"]["reasoning_effort"], "low",
        "resume with a cleared override must fall back to the entry default: {resumed}"
    );
    h.shutdown().await;
}

/// 回归任务 7 评审 Finding 1:上报/落盘的 `model` 必须等于工厂**实际建出**的
/// agent 的 `agent.config().model`,而不是另按清单再解析一次的结果。二者在
/// 「工厂持有启动时快照、另解析却重读磁盘」时会分歧(运行期 `model/upsert`
/// 新增条目即触发)。这里用注入了清单的 `production_factory` 把工厂一侧钉死,
/// 断言 `thread/start` / `thread/resume` 上报的模型正是该工厂解析出的条目模型。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_and_resume_report_the_built_agents_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    cfg.provider = "openai".to_string();
    cfg.api_key = "k".to_string();
    // 注入清单且**无全局默认**:只有显式的 modelRef 才命中条目。生产工厂按
    // "B" 解析 ⇒ agent 实际跑 "model-b"。若实现改用「另读一次磁盘清单」来上报,
    // 在该清单里 B 不存在,就会报出别的模型,断言即失败。
    let factory = production_factory(
        cfg.clone(),
        test_theme(),
        fixed_watcher(test_catalog_without_default()),
    );
    let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{"modelRef":"B"}}"#)
        .await;
    let mut started = None;
    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("thread/started") {
            started = Some(v);
        } else if v.get("id") == Some(&serde_json::json!(2)) {
            resp = Some(v);
        }
        if started.is_some() && resp.is_some() {
            break;
        }
    }
    let resp = resp.expect("thread/start must respond");
    let tid = resp["result"]["thread_id"].as_str().unwrap().to_string();
    assert_eq!(
        resp["result"]["model"], "model-b",
        "start must report the model the agent actually runs: {resp}"
    );
    assert_eq!(
        resp["result"]["model_ref"], "B",
        "start must echo the selected catalog entry's display name: {resp}"
    );
    let started = started.expect("thread/started must be delivered");
    assert_eq!(started["params"]["model"], "model-b");
    assert_eq!(
        started["params"]["model_ref"], "B",
        "the thread/started notification must carry the selected ref: {started}"
    );

    // resume 与 start 同口径:仍按 meta.model_ref=B 由工厂解析。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resumed = None;
    for _ in 0..24 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            resumed = Some(v);
            break;
        }
    }
    let resumed = resumed.expect("thread/resume must respond");
    assert_eq!(
        resumed["result"]["model"], "model-b",
        "resume must report the same model the agent runs: {resumed}"
    );
    assert_eq!(
        resumed["result"]["model_ref"], "B",
        "resume must report the loaded meta's model_ref: {resumed}"
    );
    h.shutdown().await;
}

/// Design §11: `thread/list` 必须暴露会话选中的清单条目显示名(`model_ref`),
/// 供客户端回显;无覆盖的会话该字段为 `null`(跟随全局默认)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_list_exposes_model_ref_override_and_null() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let factory = production_factory(cfg.clone(), test_theme(), fixed_watcher(test_catalog()));
    let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // 一个带覆盖(B)、一个无覆盖(null＝跟随清单全局默认 A)。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{"modelRef":"B"}}"#)
        .await;
    let tid_override = read_thread_start_response(&mut h, 2).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
        .await;
    let tid_default = read_thread_start_response(&mut h, 3).await;

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
        .await;
    let mut listed = None;
    for _ in 0..6 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            listed = Some(v);
            break;
        }
    }
    let v = listed.expect("thread/list must respond");
    let threads = v["result"]["threads"].as_array().unwrap();
    let find = |id: &str| {
        threads
            .iter()
            .find(|t| t["thread_id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("thread {id} must be listed: {v}"))
    };
    assert_eq!(
        find(&tid_override)["model_ref"],
        "B",
        "an overridden session must expose its selected display name: {v}"
    );
    assert!(
        find(&tid_default)["model_ref"].is_null(),
        "a session without an override must expose null: {v}"
    );
    h.shutdown().await;
}

/// The deliberately-extended admin gate: minting a pairing code
/// (`pair/create`) or kicking a device (`device/revoke`) is desktop-privileged,
/// so a `Control` phone must be rejected from both too.
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_cannot_pair_or_revoke() {
    let mut h = Harness::with_scope(Scope::Control).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"pair/create","params":{}}"#)
        .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "pair/create from a control client: {v}"
    );
    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"device/revoke","params":{"device_id":"d1"}}"#)
        .await;
    let v = read_response(&mut h, 10).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "device/revoke from a control client: {v}"
    );
    h.shutdown().await;
}

/// Frame-level pairing (`pair/redeem`) is the path that works *through the
/// relay*: the relay forwards ws frames but never the upgrade query string,
/// so a phone's `?pair=` cannot reach the local app-server. A code minted
/// here must redeem over a normal (non-admin) initialized connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_can_redeem_a_code_with_pair_redeem() {
    let mut h = Harness::with_scope(Scope::Control).await;
    initialize(&mut h).await;
    // Desktop mints the code (same PairingState as the serving loop here).
    let code = h.pairing().create_code().code;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"pair/redeem","params":{{"code":"{code}","device_name":"iPhone"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 9).await;
    let token = v["result"]["token"]
        .as_str()
        .expect("redeem must return a token");
    assert!(token.starts_with("yia_"), "unexpected token: {token}");
    assert_eq!(v["result"]["scope"], "control");

    // A bad code is refused with the pairing-code error, not a crash.
    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"pair/redeem","params":{"code":"NOPE-0000"}}"#)
        .await;
    let v = read_response(&mut h, 10).await;
    assert_eq!(v["error"]["code"], -32001, "bad code must be refused: {v}");
    h.shutdown().await;
}

/// `thread/subscribe` 整体替换订阅集合并回显;超上限报 invalid_params。
#[tokio::test(flavor = "multi_thread")]
async fn a_client_can_subscribe_to_a_thread_set() {
    let mut h = Harness::with_scope(Scope::Control).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/subscribe","params":{"threadIds":["t1","t2"]}}"#)
        .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["result"]["subscribed"].as_array().unwrap().len(), 2);
    assert!(h.hub().has_subscribed_clients(), "订阅后 hub 必须认得");

    // 超过 16 → invalid_params。
    let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let req = serde_json::json!({"jsonrpc":"2.0","id":10,"method":"thread/subscribe","params":{"threadIds":many}});
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 10).await;
    assert_eq!(v["error"]["code"], -32602, "超上限必须报错: {v}");
    h.shutdown().await;
}

/// 通知按 thread 键路由：订阅 t1 的客户端只收 t1，未订阅的收全部。
#[tokio::test(flavor = "multi_thread")]
async fn notifications_are_routed_by_thread_key() {
    let hub = crate::broadcast::Broadcaster::new();
    let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
    let all = crate::broadcast::ClientId::local();
    let mut sub_rx = hub.register(sub.clone());
    let mut all_rx = hub.register(all.clone());
    hub.subscribe(&sub, vec!["t1".to_string()]);

    let content_t1 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t1"}});
    let content_t2 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t2"}});
    hub.broadcast_for(Some("t1"), content_t1);
    hub.broadcast_for(Some("t2"), content_t2);

    assert_eq!(sub_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
    assert!(sub_rx.try_recv().is_err(), "订阅 t1 不该收到 t2");
    assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
    assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t2");
}

/// `Notification::thread_key` 与协议字段一致（全局帧无键）。
#[test]
fn notification_thread_key_matches_the_wire() {
    use crate::protocol::Notification;
    assert_eq!(
        Notification::TurnStarted {
            thread_id: "t1".into(),
            turn_id: "x".into()
        }
        .thread_key(),
        Some("t1")
    );
    assert_eq!(
        Notification::UiSettingsUpdated {
            theme: "dark".into()
        }
        .thread_key(),
        None
    );
    assert_eq!(
        Notification::Error {
            message: "x".into()
        }
        .thread_key(),
        None
    );
    assert_eq!(
        Notification::ToolCallApprovalResolved {
            perm_id: "p".into(),
            by: "c".into(),
            decision: "allow_once".into()
        }
        .thread_key(),
        None
    );
}

/// 合并器：同一 item_id 的相邻 delta 拼接；跨 item_id 先刷旧的。
#[test]
fn delta_coalescer_concat_within_an_item_and_flush_across_items() {
    let mut c = DeltaCoalescer::default();
    // 同一 item 连续追加不立即发。
    assert!(c.push("t1", "i1", "Hel").is_empty());
    assert!(c.push("t1", "i1", "lo").is_empty());
    // 取出即 "Hello"。
    assert_eq!(c.take("t1"), Some(("i1".to_string(), "Hello".to_string())));
    assert_eq!(c.take("t1"), None, "取走后为空");

    // 跨 item_id：追新的之前先把旧的返回（顺序优先）。
    assert!(c.push("t1", "i1", "a").is_empty());
    assert_eq!(
        c.push("t1", "i2", "b"),
        vec![("i1".to_string(), "a".to_string())]
    );
    assert_eq!(c.take("t1"), Some(("i2".to_string(), "b".to_string())));

    // 超过 4KB 自动刷出。
    let big = "x".repeat(DeltaCoalescer::FLUSH_BYTES);
    let flushed = c.push("t1", "i3", &big);
    assert_eq!(flushed.len(), 1, "超上限必须立即返回: {flushed:?}");
    assert_eq!(c.take("t1"), None);
}

/// 订阅者存在时，合并器把 delta 攒起来。若屏障缺失，`item/completed` 会
/// **越过**自己那条尚未刷出的 delta 先发出——客户端先按 id 整条替换成完整
/// 文本，随后那条迟到的 delta 又被就地追加一遍，于是*尾部*被重复一次。
///
/// 契约：某 item 的 `item/completed` 之后，不得再出现该 item 的 `item/delta`。
#[tokio::test(flavor = "multi_thread")]
async fn a_completed_item_never_trails_its_own_buffered_delta() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    // 制造「存在已订阅客户端」：这会让驱动走合并路径（生产里由手机订阅触发）。
    h.hub()
        .subscribe(&crate::broadcast::ClientId::local(), vec![tid.clone()]);
    assert!(
        h.hub().has_subscribed_clients(),
        "订阅后必须走合并路径，否则本用例测不到屏障"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 收集到 turn/completed 为止，记录 agent item 的 completed 位置与其 id。
    let mut completed_agent_id: Option<String> = None;
    let mut saw_turn_completed = false;
    let mut frames: Vec<serde_json::Value> = Vec::new();
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            saw_turn_completed = true;
            frames.push(v);
            break;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
            && v["params"]["item"]["type"] == "agentMessage"
        {
            completed_agent_id = v["params"]["item"]["id"].as_str().map(str::to_string);
        }
        frames.push(v);
    }
    assert!(saw_turn_completed, "turn 必须完成: {frames:?}");
    let agent_id = completed_agent_id.expect("agent message 必须有 item/completed");

    // 完成之后，同一条 item 的 delta 绝不能再出现。
    let mut stray: Vec<serde_json::Value> = Vec::new();
    for _ in 0..5 {
        match h.try_read_value(Duration::from_millis(200)).await {
            Some(v) => {
                if v.get("method").and_then(|m| m.as_str()) == Some("item/delta")
                    && v["params"]["item_id"] == serde_json::json!(agent_id)
                {
                    stray.push(v);
                }
            }
            None => break,
        }
    }
    assert!(
        stray.is_empty(),
        "item/completed 之后不得再追加同一 item 的 delta（尾部会被重复）: {stray:?}"
    );
    h.shutdown().await;
}

/// 触发条件：**无订阅者**（纯桌面 stdio，无手机/中继）时合并器恒空，
/// delta 逐条直发，顺序天然成立——这正是规格承诺的「零回归」。它同时钉住
/// 上一条用例的前提：重复只在实例内出现订阅者（手机经中继订阅同一会话）时发生。
#[tokio::test(flavor = "multi_thread")]
async fn without_subscribers_deltas_stay_immediate_and_in_order() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    assert!(
        !h.hub().has_subscribed_clients(),
        "纯 stdio 实例不该有任何订阅者"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    let mut completed_agent_id: Option<String> = None;
    let mut saw_turn_completed = false;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            saw_turn_completed = true;
            break;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
            && v["params"]["item"]["type"] == "agentMessage"
        {
            completed_agent_id = v["params"]["item"]["id"].as_str().map(str::to_string);
        }
    }
    assert!(saw_turn_completed);
    let agent_id = completed_agent_id.expect("agent message 必须有 item/completed");

    for _ in 0..5 {
        match h.try_read_value(Duration::from_millis(200)).await {
            Some(v) => {
                let is_stray = v.get("method").and_then(|m| m.as_str()) == Some("item/delta")
                    && v["params"]["item_id"] == serde_json::json!(agent_id);
                assert!(!is_stray, "无订阅者时不得出现迟到 delta: {v}");
            }
            None => break,
        }
    }
    h.shutdown().await;
}

#[test]
fn chunk_items_for_replay_preserves_order_and_splits_by_count() {
    use crate::protocol::Item;
    let items: Vec<Item> = (0..(REPLAY_CHUNK_MAX_ITEMS * 2 + 5))
        .map(|i| Item::AgentMessage {
            id: format!("i-{i}"),
            text: "x".to_string(),
        })
        .collect();
    let chunks = chunk_items_for_replay(items);
    assert_eq!(chunks.len(), 3, "200*2+5 must split into 3 chunks");
    assert_eq!(chunks[0].len(), REPLAY_CHUNK_MAX_ITEMS);
    assert_eq!(chunks[1].len(), REPLAY_CHUNK_MAX_ITEMS);
    assert_eq!(chunks[2].len(), 5);
    // 顺序保持：展平后 id 与原始一致。
    let flat: Vec<String> = chunks
        .iter()
        .flatten()
        .map(|it| match it {
            Item::AgentMessage { id, .. } => id.clone(),
            _ => unreachable!(),
        })
        .collect();
    for (i, id) in flat.iter().enumerate() {
        assert_eq!(id, &format!("i-{i}"));
    }
}

#[test]
fn chunk_items_for_replay_splits_by_bytes() {
    use crate::protocol::Item;
    // 每条 ~64KiB 文本;预算 256KiB → 每块约 4 条。
    let big = "y".repeat(64 * 1024);
    let items: Vec<Item> = (0..10)
        .map(|i| Item::AgentMessage {
            id: format!("b-{i}"),
            text: big.clone(),
        })
        .collect();
    let chunks = chunk_items_for_replay(items);
    assert!(chunks.len() >= 3, "byte budget must force multiple chunks");
    for c in &chunks {
        let bytes: usize = c
            .iter()
            .map(|it| serde_json::to_vec(it).unwrap().len())
            .sum();
        // 允许「最后一条超预算」的余量,但每块仍须远小于 1MiB 硬上限。
        assert!(bytes < 900 * 1024, "chunk must stay under the frame limit");
    }
}

/// Approval is one-question/one-answer even with many clients: the first
/// answer routes to the driver, a second answer for the same `perm_id` is a
/// no-op success (not an error — the other device may simply have tapped
/// "allow" a beat later).
///
/// The full two-client ws round trip (including the `approvalResolved`
/// broadcast) belongs to Task 5's E2E; here the routing core is pinned.
#[tokio::test(flavor = "multi_thread")]
async fn the_second_approval_answer_is_a_noop_and_the_first_wins() {
    let pending: Mutex<HashMap<String, oneshot::Sender<Decision>>> = Mutex::new(HashMap::new());
    let (tx, mut rx) = oneshot::channel::<Decision>();
    pending.lock().await.insert("perm-1".to_string(), tx);

    let resp = ClientResponse {
        jsonrpc: Some("2.0".to_string()),
        id: RequestId::Str("perm-1".to_string()),
        result: Some(json!({ "decision": "allow_once" })),
        error: None,
    };

    let first = route_client_response(resp.clone(), &pending)
        .await
        .expect("the first answer must route to the waiting driver");
    assert_eq!(first.0, "perm-1");
    assert!(matches!(first.1, Decision::AllowOnce));

    // The same client (or a second one) answers again: no-op, not an error.
    let second = route_client_response(resp, &pending).await;
    assert!(
        second.is_none(),
        "a second answer for an already-resolved approval must be a no-op"
    );

    // The driver saw exactly one decision.
    assert!(matches!(rx.try_recv(), Ok(Decision::AllowOnce)));
    assert!(
        rx.try_recv().is_err(),
        "the driver must receive exactly one decision"
    );
}

/// `pair/create` mints a one-time code, `device/list` reflects the shared
/// store, and `device/revoke` kicks the device back out of it. `Harness::new`
/// is `Admin`, so the admin gate does not interfere here.
#[tokio::test(flavor = "multi_thread")]
async fn pair_create_returns_a_code_and_device_revoke_invalidates_it() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"pair/create","params":{}}"#)
        .await;
    let created = h.read_value().await;
    let code = created["result"]["code"].as_str().unwrap().to_string();
    assert!(created["result"]["expires_in"].as_u64().unwrap() > 0);

    // 设备表初始为空。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"device/list","params":{}}"#)
        .await;
    let listed = h.read_value().await;
    assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 0);

    // 用码换 token,设备表出现一条。
    let (device, _token) = h.pairing().redeem(&code, "iPhone 15").unwrap();
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"device/list","params":{}}"#)
        .await;
    let listed = h.read_value().await;
    assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 1);

    // 撤销。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"device/revoke","params":{{"device_id":"{}"}}}}"#,
        device.id
    ))
    .await;
    let revoked = h.read_value().await;
    assert_eq!(revoked["result"]["revoked"], true);
    h.shutdown().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_read_and_write_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["theme"], "dark", "default before any write");
    assert_eq!(v["result"]["board_watchman_enabled"], true, "缺省为开:{v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"theme":"light"}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true);

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["result"]["theme"], "light");

    // 落盘可被另一个进程读回
    assert_eq!(
        crate::settings_store::load(dir.path()),
        crate::settings_store::Theme::Light
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_read_reports_the_stored_relay_url() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    crate::settings_store::save_relay_url(
        dir.path(),
        Some("wss://relay.example.com/connect?session=x"),
    )
    .unwrap();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(
        v["result"]["relay_url"], "wss://relay.example.com/connect?session=x",
        "{v}"
    );
    h.shutdown().await;
}

/// 未配置时 `relay_url` 是显式的 `null`（不是缺键）：桌面设置页据此回填空串，
/// 缺键与 `null` 在 JSON 里对前端是两码事。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_read_reports_null_when_no_relay_url_is_stored() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["relay_url"], json!(null), "{v}");
    assert!(
        v["result"].get("relay_url").is_some(),
        "key must be present: {v}"
    );
    h.shutdown().await;
}

/// 写 `relay_url` 后读回一致，且落盘可被另一个进程（如侧车）读回。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_then_read_round_trips_the_relay_url() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"relay_url":"wss://relay.example.com/connect?session=x"}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(
        v["result"]["relay_url"], "wss://relay.example.com/connect?session=x",
        "{v}"
    );
    assert_eq!(
        crate::settings_store::load_relay_url(dir.path()).as_deref(),
        Some("wss://relay.example.com/connect?session=x")
    );
    h.shutdown().await;
}

/// `null` / 空串 → 清除；清除后 read 报 `null`。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_clears_the_relay_url() {
    for clear in [json!(null), json!(""), json!("   ")] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        crate::settings_store::save_relay_url(
            dir.path(),
            Some("wss://relay.example.com/connect?session=x"),
        )
        .unwrap();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "ui/settings/write",
                "params": { "relay_url": clear },
            })
            .to_string(),
        )
        .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["ok"], true, "clear={clear}: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["result"]["relay_url"], json!(null), "clear={clear}: {v}");
        assert_eq!(
            crate::settings_store::load_relay_url(dir.path()),
            None,
            "clear={clear}"
        );
        h.shutdown().await;
    }
}

/// 写错类型（非串非 null）不改动既有配置：静默清除是破坏性的。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_ignores_a_mistyped_relay_url() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    crate::settings_store::save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"relay_url":5}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        crate::settings_store::load_relay_url(dir.path()).as_deref(),
        Some("wss://r/connect?session=x"),
        "a mistyped field must not wipe the stored url"
    );
    h.shutdown().await;
}

/// 只有 `relay_url` 的写不能影响 theme：两者共处一个文件。
#[tokio::test(flavor = "multi_thread")]
async fn writing_the_relay_url_keeps_the_theme() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"light"}}"#)
        .await;
    let _ = read_response(&mut h, 2).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"relay_url":"wss://r/connect?session=x"}}"#,
    )
    .await;
    let _ = read_response(&mut h, 3).await;

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["result"]["theme"], "light", "{v}");
    assert_eq!(v["result"]["relay_url"], "wss://r/connect?session=x", "{v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_rejects_an_unknown_theme() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"sepia"}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert!(
        v.get("error").is_some(),
        "unsupported theme must be rejected"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_notifies_subscribers() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"light"}}"#)
        .await;
    // 响应(主循环写)与通知(watcher 任务写)共用一路流,到达顺序不定;
    // 逐帧收敛,两者都必须见到。
    let mut responded = false;
    let mut notified = None;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            assert_eq!(v["result"]["ok"], true);
            responded = true;
        } else if v.get("method").and_then(|m| m.as_str()) == Some("ui/settings/updated") {
            notified = Some(v);
        }
        if responded && notified.is_some() {
            break;
        }
    }
    assert!(responded, "ui/settings/write must respond");
    let n = notified.expect("theme change must push ui/settings/updated");
    assert_eq!(n["params"]["theme"], "light");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_read_returns_tag_names_overrides() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    // 先写 1 个覆盖，再读回：应恰有这 1 个键（默认名不由后端补齐）。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"red":"紧急"}}}"#).await;
    let _ = read_response(&mut h, 2).await;

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let names = v["result"]["tag_names"]
        .as_object()
        .expect("tag_names 必须是对象");
    assert_eq!(names.len(), 1, "只回用户覆盖项: {v}");
    assert_eq!(names["red"], "紧急");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_rejects_invalid_tag_name_key_and_empty_value() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    // 非法颜色 id。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"gray":"灰"}}}"#).await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

    // 空白名字。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"tag_names":{"red":"   "}}}"#).await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

    // 超长名字（>24）。
    let long = "长".repeat(25);
    let req = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"ui/settings/write",
        "params":{"tag_names":{"red": long}}});
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");
    h.shutdown().await;
}

/// 上限 `TAG_NAME_MAX_LEN` 按**字符**而非**字节**计。
///
/// 这是一个可判别的钉子:每个 CJK 字符占 3 字节,故 9 个字符(27 字节)与
/// 恰好 24 个字符(72 字节)都必须被接受——若实现退化成 `name.len()` 字节
/// 计数,这两条会双双被误拒而失败。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_tag_name_length_counts_characters_not_bytes() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    // 9 个 CJK 字符 = 27 字节:字节计数会误拒的最小反例。
    let nine = "字".repeat(9);
    assert_eq!(nine.chars().count(), 9);
    assert_eq!(nine.len(), 27);
    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"ui/settings/write",
        "params":{"tag_names":{"green": nine}}});
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 2).await;
    assert!(
        v.get("error").is_none(),
        "9 个 CJK 字符(27 字节)必须被接受: {v}"
    );

    // 恰好 24 个 CJK 字符 = 72 字节:边界上必须接受,字节计数会误拒。
    let max = "字".repeat(TAG_NAME_MAX_LEN);
    assert_eq!(max.chars().count(), TAG_NAME_MAX_LEN);
    assert_eq!(max.len(), 72);
    let req = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"ui/settings/write",
        "params":{"tag_names":{"blue": max}}});
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 3).await;
    assert!(
        v.get("error").is_none(),
        "恰好 {TAG_NAME_MAX_LEN} 个 CJK 字符(72 字节)必须被接受: {v}"
    );

    // 回读确认确实落盘(而非只是"没报错")。
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    let names = v["result"]["tag_names"]
        .as_object()
        .expect("tag_names 必须是对象");
    assert_eq!(names["green"], nine.as_str(), "{v}");
    assert_eq!(names["blue"], max.as_str(), "{v}");

    // 边界另一侧:25 个字符必须被拒。
    let too_long = "字".repeat(TAG_NAME_MAX_LEN + 1);
    assert_eq!(too_long.chars().count(), 25);
    let req = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"ui/settings/write",
        "params":{"tag_names":{"red": too_long}}});
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_broadcasts_tag_names_updated() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"blue":"参考"}}}"#).await;
    let v = read_response(&mut h, 2).await;
    assert!(v.get("error").is_none(), "{v}");

    let ev = read_until_method(&mut h, "ui/tagNames/updated").await;
    assert_eq!(ev["params"]["tag_names"]["blue"], "参考");
    h.shutdown().await;
}

/// 校验先于落盘：非法标签名不得把同一次请求里有效的 `relay_url` 写了一半。
/// 同时定住另外两条语义：**缺** `tag_names` 键是「不动」，`{}` 是空合并
/// （按键合并保留既有覆盖），广播出去的是落盘后读回的**权威**映射。
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_tag_name_validation_precedes_the_disk_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"red":"紧急"}}}"#).await;
    let _ = read_response(&mut h, 2).await;

    // 非法名字与有效 relay_url 同请求：整体失败，磁盘一字未动。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"relay_url":"wss://r/connect?session=x","tag_names":{"gray":"灰"}}}"#).await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");
    assert_eq!(
        crate::settings_store::load_relay_url(dir.path()),
        None,
        "被拒的标签名不得把 relay_url 写了一半"
    );

    // `{}`：合法空合并，既有覆盖原样保留；广播回读的权威映射。
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/write","params":{"tag_names":{}}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert!(v.get("error").is_none(), "{v}");
    let ev = read_until_method(&mut h, "ui/tagNames/updated").await;
    assert_eq!(
        ev["params"]["tag_names"],
        serde_json::json!({ "red": "紧急" }),
        "广播必须是落盘后读回的合并映射: {ev}"
    );

    // 缺 `tag_names`（只写 theme）：名字不动。
    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"ui/settings/write","params":{"theme":"light"}}"#)
        .await;
    let _ = read_response(&mut h, 5).await;

    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 6).await;
    assert_eq!(v["result"]["tag_names"]["red"], "紧急", "{v}");
    assert_eq!(
        v["result"]["tag_names"].as_object().unwrap().len(),
        1,
        "缺键不清除、空对象不清除: {v}"
    );
    h.shutdown().await;
}

/// `board_watchman_enabled` 的写路径:`false` 卸载、`true` 安装,均经注入的
/// 记录器;偏好落盘且读回一致。
#[tokio::test(flavor = "multi_thread")]
async fn writing_the_watchman_setting_installs_or_uninstalls_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    let calls = Arc::clone(&h.watchman_calls);
    let home = h._watchman_home.path().to_path_buf();

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":false}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        v["result"]["warning"],
        json!(null),
        "注入的卸载不会失败:{v}"
    );
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "恰好一次卸载:{recorded:?}");
    assert!(!recorded[0].0, "false triggers uninstall");
    assert_eq!(recorded[0].1, home, "必须针对注入的 home");
    assert!(!crate::settings_store::load_watchman_enabled(dir.path()));

    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
    )
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "第二次是安装:{recorded:?}");
    assert!(recorded[1].0, "true triggers install");

    // 开关与 theme 同处一个文件,互不覆盖。
    assert!(crate::settings_store::load_watchman_enabled(dir.path()));
    assert_eq!(
        crate::settings_store::load(dir.path()),
        crate::settings_store::Theme::Dark
    );

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["result"]["board_watchman_enabled"], true, "{v}");
    h.shutdown().await;
}

/// 安装失败不静默:落盘仍成功,回复带 `warning`,并由 `ok: true` 表明偏好已写入。
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_watchman_install_reports_a_warning_without_failing_the_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_watchman(
        cfg,
        Arc::new(|_exe: &Path, _home: &Path| Err("launchctl is unavailable".to_string())),
        Arc::new(|_home: &Path| Ok(())),
    )
    .await;
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        v["result"]["warning"], "launchctl is unavailable",
        "失败原因必须回传给前端:{v}"
    );
    // 偏好仍按用户的意图落盘:下次启动会再试。
    assert!(crate::settings_store::load_watchman_enabled(dir.path()));
    h.shutdown().await;
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

/// 注入式清单:两条不同 provider 的条目,免动进程级 `HOME`。
fn test_catalog() -> Arc<yi_agent_runtime::models::ModelCatalog> {
    Arc::new(model_catalog_entries(Some("A")))
}

/// 固定快照 watcher:测试要的是「注入一份清单」,不是「跟随磁盘」。
fn fixed_watcher(
    catalog: Arc<yi_agent_runtime::models::ModelCatalog>,
) -> Arc<yi_agent_runtime::models::CatalogWatcher> {
    yi_agent_runtime::models::CatalogWatcher::from_catalog(
        std::path::PathBuf::new(),
        (*catalog).clone(),
    )
}

/// 运行期改 `models.json` 后,**新建** thread 必须用上新条目——这是
/// app-server 侧「工厂不再持启动快照」的直接断言。
#[tokio::test(flavor = "multi_thread")]
async fn a_thread_started_after_a_catalog_change_uses_the_new_entry() {
    let dir = tempfile::TempDir::new().unwrap();
    let models_path = dir.path().join("models.json");
    std::fs::write(
        &models_path,
        r#"{"models":[
            {"name":"A","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-a","api_key":"k"},
            {"name":"B","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-b","api_key":"k"}
        ],"default_model":"A"}"#,
    )
    .unwrap();

    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    cfg.provider = "openai".to_string();
    cfg.api_key = "k".to_string();

    let watcher = yi_agent_runtime::models::CatalogWatcher::new(models_path.clone());
    let factory = production_factory(cfg.clone(), test_theme(), Arc::clone(&watcher));
    let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // 第一个 thread:走默认条目 A。`Harness` 无 `read_until_id`,用既有
    // 「循环 read_value 直到 id 匹配」的写法(见 server.rs 的 thread/start 测试)。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let first = loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            break v;
        }
    };
    assert_eq!(
        first["result"]["model"].as_str(),
        Some("model-a"),
        "the first thread must run the entry present at startup"
    );

    // 运行期换默认条目(改 mtime + size),**不重启 app-server**。
    std::fs::write(
        &models_path,
        r#"{"models":[
            {"name":"A","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-a","api_key":"k"},
            {"name":"B","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-b","api_key":"k"}
        ],"default_model":"B"}"#,
    )
    .unwrap();

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
        .await;
    let second = loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            break v;
        }
    };
    assert_eq!(
        second["result"]["model"].as_str(),
        Some("model-b"),
        "a thread started after the change must use the new default entry"
    );
    h.shutdown().await;
}

/// 同 [`test_catalog`] 但没有全局默认,用于断言「无默认时回退 cfg」。
fn test_catalog_without_default() -> Arc<yi_agent_runtime::models::ModelCatalog> {
    Arc::new(model_catalog_entries(None))
}

fn model_catalog_entries(default_model: Option<&str>) -> yi_agent_runtime::models::ModelCatalog {
    use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider};
    ModelCatalog {
        models: vec![
            ModelEntry {
                name: "A".into(),
                provider: ModelProvider::Anthropic,
                api_url: "https://a.example".into(),
                model: "model-a".into(),
                api_key: "key-a".into(),
                api_style: None,
                reasoning_effort: None,
                context_length: None,
            },
            ModelEntry {
                name: "B".into(),
                provider: ModelProvider::Openai,
                api_url: "https://b.example".into(),
                model: "model-b".into(),
                api_key: "key-b".into(),
                api_style: None,
                reasoning_effort: None,
                context_length: None,
            },
        ],
        default_model: default_model.map(str::to_string),
        subagent_model: None,
    }
}

/// 工厂必须按会话的 `model_ref` 从清单条目解析 provider/api_url/api_key/model,
/// 而不是照抄全局 `cfg`(此处 cfg.model 为占位的 `test-model`)。
#[test]
fn a_session_model_ref_selects_the_catalog_entry() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config();
    assert_ne!(cfg.model, "model-b", "precondition: cfg differs from entry");
    let factory = production_factory(cfg, test_theme(), fixed_watcher(test_catalog()));

    let built = with_session_model_ref(Some("B".to_string()), || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .expect("an injected catalog must bootstrap without network");

    assert_eq!(built.agent.config().model, "model-b");
}

/// 覆盖名不在清单里时退回清单的全局默认(Task 2 已定优先级),而非 cfg。
#[test]
fn an_unknown_model_ref_falls_back_to_the_catalog_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let factory = production_factory(test_config(), test_theme(), fixed_watcher(test_catalog()));

    let built = with_session_model_ref(Some("nope".to_string()), || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .expect("bootstrap");

    assert_eq!(built.agent.config().model, "model-a");
}

/// 既无会话覆盖、清单又无全局默认时才回退 `cfg`(空清单即今天的旧行为)。
#[test]
fn no_override_and_no_default_falls_back_to_the_global_cfg() {
    let dir = tempfile::TempDir::new().unwrap();
    let factory = production_factory(
        test_config(),
        test_theme(),
        fixed_watcher(test_catalog_without_default()),
    );

    let built = with_session_model_ref(None, || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .expect("bootstrap");

    assert_eq!(built.agent.config().model, "test-model");
}

/// `thread/resume` 重建 agent 时必须只认该 thread 的 `model_ref`;
/// 读到别的会话(无覆盖)时退回默认,不得沿用上一次的覆盖。
#[test]
fn the_session_model_ref_does_not_leak_across_calls() {
    let dir = tempfile::TempDir::new().unwrap();
    let factory = production_factory(test_config(), test_theme(), fixed_watcher(test_catalog()));

    let built = with_session_model_ref(Some("B".to_string()), || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .unwrap();
    assert_eq!(built.agent.config().model, "model-b");

    let built = factory(None, dir.path(), crate::thread_store::ThreadMode::Normal).unwrap();
    assert_eq!(
        built.agent.config().model,
        "model-a",
        "the override must not leak past with_session_model_ref"
    );
}

/// 关联根槽位必须「进入时设定、退出时还原」：daemon 的 worker factory 按项目
/// 目录构建、被同目录多个会话共享，槽位若不还原，A 会话的关联根会泄漏给同目录
/// 的下一个会话（拿到不该有的写权限）。
#[test]
fn session_extra_roots_slot_scopes_and_restores() {
    assert!(current_session_extra_roots().is_empty());
    let roots = vec![
        std::path::PathBuf::from("/tmp/slot-a"),
        std::path::PathBuf::from("/tmp/slot-b"),
    ];
    let seen = with_session_extra_roots(roots.clone(), current_session_extra_roots);
    assert_eq!(seen, roots);
    assert!(current_session_extra_roots().is_empty());
}

/// 解析链 §4.2 的工厂一侧：会话 effort 覆盖优先于条目默认。
#[test]
fn a_session_effort_override_beats_the_entry_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut catalog = model_catalog_entries(Some("A"));
    catalog.models[0].reasoning_effort = Some("low".into());
    let factory = production_factory(
        test_config(),
        test_theme(),
        fixed_watcher(Arc::new(catalog)),
    );

    // 会话覆盖 high → 胜过条目默认 low。
    let built = with_session_reasoning_effort(Some("high".to_string()), || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .expect("bootstrap");
    assert_eq!(
        built.agent.config().gen_params.reasoning_effort.as_deref(),
        Some("high"),
        "the session override must win over the entry default"
    );

    // 未设槽位 → 跟随条目默认 low。
    let built = factory(None, dir.path(), crate::thread_store::ThreadMode::Normal).unwrap();
    assert_eq!(
        built.agent.config().gen_params.reasoning_effort.as_deref(),
        Some("low"),
        "without an override the entry default must apply"
    );

    // 清除覆盖后的 resume 走的是**同一入参**：`thread/resume` 把 meta 的
    // `None` 直接交给槽位，故「清除」与「从未设过」在工厂侧不可区分——这正是
    // 要求的收敛：二者都必须保留条目默认 low（第 2 级），而不是「不发该字段」。
    // 活线程的清除路径同样落到条目默认（driver 把 gen_params 置空后 provider
    // 自补其烤入的同一条目档），两条路径手感一致。
    let cleared = with_session_reasoning_effort(None, || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .unwrap();
    assert_eq!(
        cleared
            .agent
            .config()
            .gen_params
            .reasoning_effort
            .as_deref(),
        Some("low"),
        "a cleared override must fall back to the entry default, not to unset"
    );
}

/// 条目无默认 + 未设覆盖 ⇒ 不发该字段(零回归)。
#[test]
fn no_effort_anywhere_stays_unset() {
    let dir = tempfile::TempDir::new().unwrap();
    let factory = production_factory(test_config(), test_theme(), fixed_watcher(test_catalog()));
    let built = factory(None, dir.path(), crate::thread_store::ThreadMode::Normal).unwrap();
    assert_eq!(built.agent.config().gen_params.reasoning_effort, None);
}

/// 覆盖不得跨调用泄漏（与 `model_ref` 同一契约）。
#[test]
fn the_session_effort_override_does_not_leak_across_calls() {
    let dir = tempfile::TempDir::new().unwrap();
    let factory = production_factory(test_config(), test_theme(), fixed_watcher(test_catalog()));
    let built = with_session_reasoning_effort(Some("max".to_string()), || {
        factory(None, dir.path(), crate::thread_store::ThreadMode::Normal)
    })
    .unwrap();
    assert_eq!(
        built.agent.config().gen_params.reasoning_effort.as_deref(),
        Some("max")
    );

    let built = factory(None, dir.path(), crate::thread_store::ThreadMode::Normal).unwrap();
    assert_eq!(
        built.agent.config().gen_params.reasoning_effort,
        None,
        "the override must not leak past with_session_reasoning_effort"
    );
}

/// `thread/readItems` 返回已落盘的 items；`afterItemId` 只返回其后的部分。
#[tokio::test(flavor = "multi_thread")]
async fn read_items_returns_persisted_items_and_slices_after_id() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    // 跑完一轮，产生至少一条已落盘的 item（user + agent 消息）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    // 读到 turn/completed 为止，确认落盘完成。
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 4).await;
    let items = v["result"]["items"].as_array().expect("items array");
    assert!(!items.is_empty(), "readItems 必须返回已落盘的 items: {v}");
    let first_id = items[0]["id"].as_str().unwrap().to_string();

    // afterItemId = 第一条 → 不包含第一条。
    let req = serde_json::json!({
        "jsonrpc":"2.0","id":5,"method":"thread/readItems",
        "params":{"threadId": tid, "afterItemId": first_id}
    });
    h.send(&req.to_string()).await;
    let v = read_response(&mut h, 5).await;
    let after = v["result"]["items"].as_array().unwrap();
    assert!(
        after
            .iter()
            .all(|i| i["id"].as_str() != Some(first_id.as_str())),
        "afterItemId 之后不该再含该 id: {v}"
    );

    // 未知 thread → unknown_thread(-32011)。
    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/readItems","params":{"threadId":"nope"}}"#)
        .await;
    let v = read_response(&mut h, 6).await;
    assert_eq!(v["error"]["code"], -32011, "未知 thread 必须报错: {v}");
    h.shutdown().await;
}

/// `thread/readItems` 对**正在跑**的会话是只读的：调用它**不得**中断回合。
///
/// 这正是它相对 `thread/resume` 的关键差异（resume 会先 interrupt 在跑的 turn）。
#[tokio::test(flavor = "multi_thread")]
async fn read_items_does_not_interrupt_a_running_turn() {
    let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    // 等回合真正开始。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
            break;
        }
    }
    // 只读补齐：slow provider 期间会持续推 item/delta，按 id 找响应本身。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut resp = None;
    for _ in 0..12 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            resp = Some(v);
            break;
        }
    }
    let v = resp.expect("thread/readItems must respond");
    assert!(v.get("error").is_none(), "readItems 不应出错: {v}");
    // SlowProvider 每 5ms 推一段 item/delta、且永不结束。若 readItems 打断了
    // 回合，此后就再不会有任何 delta；反之则必有。故"响应之后还收得到 delta"
    // 是"回合仍在跑"的确定性判据。
    let mut deltas_after = 0usize;
    for _ in 0..20 {
        let n = h.read_value().await;
        match n.get("method").and_then(|m| m.as_str()) {
            Some("item/delta") => {
                deltas_after += 1;
                if deltas_after >= 2 {
                    break;
                }
            }
            Some("turn/completed") => break,
            _ => {}
        }
    }
    assert!(
        deltas_after >= 2,
        "readItems 之后回合必须仍在跑（仍持续产生 item/delta），实际 {deltas_after} 段"
    );
    h.shutdown().await;
}

/// Rebuilding a thread agent must reuse the *same* session `Arc`.
///
/// The delegation tools read the caller's live transcript through a
/// `CallerContext` bound to that `Arc` (see [`wrap_for_delegation`]). If a
/// rebuild swapped in a fresh `Arc` (as `with_session` does), the bound
/// handle would point at the discarded session and `fork: true` would
/// silently upload nothing. This drives every rebuild path.
#[test]
fn rebuilding_a_thread_agent_keeps_one_session_handle() {
    // 1. Delegation path: `wrap_for_delegation` swaps the tool registry and
    //    binds the caller in one step.
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
        "thread-handle",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        "proj-test".to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
    )
    .expect("tooling");
    // Taken *before* the rebuild: a clone shares the caller slot the tools
    // hold, so it observes whatever `wrap_for_delegation` binds.
    let caller = tooling.caller.clone();

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(
        &mut agent,
        Some(yi_agent_core::Session::from_messages(vec![
            yi_agent_core::Message::user("first"),
        ])),
    );
    let handle_a = agent.session_handle();
    handle_a
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::assistant(vec![
            yi_agent_core::ContentBlock::Text("reply".into()),
        ]));

    let built = BuiltAgent {
        agent,
        provider: provider.clone(),
        config: config.clone(),
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
    };
    let rebuilt = wrap_for_delegation(built, tooling);
    assert!(
        Arc::ptr_eq(&handle_a, &rebuilt.agent.session_handle()),
        "wrap_for_delegation must reuse the thread's session Arc"
    );
    let seen = caller
        .snapshot()
        .expect("wrap_for_delegation must bind the caller to the live session");
    assert_eq!(
        seen,
        handle_a.lock().unwrap().messages().to_vec(),
        "the bound caller must read the rebuilt agent's live transcript"
    );

    // 2. Theme rebuild path (no resume): `rebuild_thread_agent_with_theme`.
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let bootstrapped = yi_agent_runtime::bootstrap::bootstrap_agent(
        &cfg,
        yi_agent_runtime::bootstrap::PermissionMode::Interactive,
    )
    .expect("a default config bootstraps without network");
    let handle_b = bootstrapped.agent.session_handle();
    handle_b
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::user("in-flight"));
    let rebuilt_theme = rebuild_thread_agent_with_theme(bootstrapped, None, test_theme());
    assert!(
        Arc::ptr_eq(&handle_b, &rebuilt_theme.agent.session_handle()),
        "rebuild_thread_agent_with_theme must reuse the session Arc"
    );
    assert_eq!(
        rebuilt_theme.agent.session().messages().len(),
        1,
        "the rebuilt agent must still see the in-flight message"
    );

    // 3. Resume path: `apply_session(Some(..))` applies contents in place,
    //    so the loaded history lands on the same Arc.
    let bootstrapped = yi_agent_runtime::bootstrap::bootstrap_agent(
        &cfg,
        yi_agent_runtime::bootstrap::PermissionMode::Interactive,
    )
    .expect("a default config bootstraps without network");
    let handle_c = bootstrapped.agent.session_handle();
    let mut loaded = yi_agent_core::Session::from_messages(vec![
        yi_agent_core::Message::user("loaded-one"),
        yi_agent_core::Message::user("loaded-two"),
    ]);
    // A resumed session carries the previous turn's usage; the rebuild must
    // land it on the live handle, or the first post-resume turn's
    // auto-compact check reads `None` and short-circuits.
    loaded.set_last_input_tokens(Some(1234));
    let rebuilt_loaded = rebuild_thread_agent_with_theme(bootstrapped, Some(loaded), test_theme());
    assert!(
        Arc::ptr_eq(&handle_c, &rebuilt_loaded.agent.session_handle()),
        "apply_session must keep the session Arc, not swap in a new one"
    );
    assert_eq!(
        rebuilt_loaded.agent.session().messages().len(),
        2,
        "apply_session must still apply the resumed history"
    );
    assert_eq!(
        rebuilt_loaded.agent.session().last_input_tokens(),
        Some(1234),
        "apply_session must carry the resumed input-token count onto the live handle"
    );
}

/// Compaction replaces the messages in place and keeps the session `Arc`.
///
/// `with_session(compacted)` would drop the old handle, so a `CallerContext`
/// taken before the compact would keep reading the pre-compaction
/// transcript.
#[tokio::test(flavor = "multi_thread")]
async fn compaction_replaces_messages_without_changing_the_handle() {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let session = yi_agent_core::Session::from_messages(vec![
        yi_agent_core::Message::user("first"),
        yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text("reply".into())]),
        yi_agent_core::Message::user("second"),
    ]);
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, Some(session));

    let handle = agent.session_handle();
    // The host binds this handle once; it must observe the compacted text.
    let caller = yi_agent_subagent::CallerContext::new(handle.clone());

    let snapshot = agent.session();
    let compacted = yi_agent_core::compact_session(&provider, agent.config(), &snapshot)
        .await
        .expect("compaction must run")
        .expect("the three-message history must reduce");
    let expected = compacted.messages().to_vec();
    agent.set_session_messages(compacted.messages().to_vec());

    assert!(
        Arc::ptr_eq(&handle, &agent.session_handle()),
        "compaction must not change the session Arc"
    );
    assert_eq!(
        agent.session().messages(),
        expected.as_slice(),
        "compaction must install the compacted messages"
    );
    assert_eq!(
        caller.snapshot().expect("the caller stays bound"),
        expected,
        "a caller bound before the compact must see the compacted transcript"
    );
}

/// `/clear` 与 `/compact` 都必须把陈旧的 `last_input_tokens` 清零。
///
/// Task 8 把 `with_session(Session::new())` 换成 `set_session_messages(...)`
/// 以保住 session `Arc`,但后者只换消息、不碰 token 计数:上一轮留下的(很大的)
/// `last_input_tokens` 会存活到 `maybe_auto_compact`,让下一轮立刻误触发一次
/// 无谓的自动压缩。本测试驱动**生产函数** `apply_session_command`,断言计数归零、
/// 而 `Arc` 未变。
#[tokio::test]
async fn session_commands_clear_the_stale_input_tokens_without_changing_the_handle() {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    drop(client_r); // 本测试不看 writer 输出
    let (hub, _client) = test_hub(server_w);
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = crate::thread_store::ThreadStore::new(store_dir.path());

    // Clear 会 truncate 日志,故线程 id 必须合法。
    let cases = [
        ("/clear", "thread-tokens-clear"),
        ("/compact", "thread-tokens-compact"),
    ];
    for (label, thread_id) in cases {
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        let handle = agent.session_handle();
        let caller = yi_agent_subagent::CallerContext::new(handle.clone());
        // 可压缩的历史(两个 user 轮次)+ 上一轮留下的大计数,正是 finding 描述的
        // 陈旧状态。
        handle.lock().unwrap().replace_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
            yi_agent_core::Message::user("second"),
        ]);
        handle.lock().unwrap().set_last_input_tokens(Some(150_000));

        let status = ThreadSession::new_status();
        if label == "/clear" {
            let (reply, answer) = oneshot::channel();
            agent = apply_session_command(
                agent,
                SessionCommand::Clear { reply },
                &provider,
                &config,
                &store,
                thread_id,
                &hub,
                &turn_tx,
                &status,
            )
            .await;
            answer
                .await
                .expect("the clear reply channel must be fulfilled")
                .expect("truncating a never-written log must succeed");
        } else {
            let (reply, answer) = oneshot::channel();
            agent = apply_session_command(
                agent,
                SessionCommand::Compact { reply },
                &provider,
                &config,
                &store,
                thread_id,
                &hub,
                &turn_tx,
                &status,
            )
            .await;
            assert_eq!(
                answer
                    .await
                    .expect("the compact reply channel must be fulfilled"),
                CompactOutcome::Compacted,
                "{label} must compact the two-user-turn history"
            );
        }

        assert_eq!(
            agent.session().last_input_tokens(),
            None,
            "{label} must clear the stale input-token count"
        );
        assert!(
            Arc::ptr_eq(&handle, &agent.session_handle()),
            "{label} must keep the session Arc"
        );
        assert_eq!(
            caller.snapshot().expect("the caller stays bound"),
            agent.session().messages().to_vec(),
            "{label} must land its outcome on the handle the caller holds"
        );
    }

    assert!(
        turn_rx.try_recv().is_err(),
        "a session command with no turn must not report a turn as finished"
    );
}
// ---------- merged stdio + loopback ws (serve_stdio_with_relay) ----------

/// 合并模式接线:stdio 的 `reader`/`writer` + 一条已绑定的环回 ws listener,
/// 共用**一个** `serve()` 与同一套 hub/表。
///
/// 与 [`Harness`] 不同,这里同时驱动两条前端(stdio 的 `local` 与 ws 的
/// `ws-<uuid>`),测试因此能证明两者共享同一张 `threads`。测试**不**起真中继:
/// ws 客户端直接用注入 `pairing` 铸出的 token 连环回 listener。
struct MergedHarness {
    client_w: tokio::io::DuplexStream,
    client_r: tokio::io::BufReader<tokio::io::DuplexStream>,
    addr: std::net::SocketAddr,
    token: String,
    #[allow(dead_code)]
    hub: Arc<crate::broadcast::Broadcaster>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// 隔离的全局索引;持有它保证 tempdir 活到 harness 结束。
    _index_dir: tempfile::TempDir,
    /// 隔离的看板登记表目录,理由同 [`Harness::board_dir`]。
    _board_dir: tempfile::TempDir,
}

impl MergedHarness {
    async fn new() -> Self {
        // 单测不起真中继。
        Self::new_with_relay(None).await
    }

    /// [`MergedHarness::new`] 但可注入 `relay_url`:传 `Some` 时走生产路径
    /// ——起中继客户端(可指向一个不可达端点,它只会退避重连)。用来验证
    /// 中继接线不打断合并主循环、且 stdio EOF 时收尾仍返回。
    async fn new_with_relay(relay_url: Option<String>) -> Self {
        let cfg = test_config();
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let board_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // stdin 侧本地客户端仍是生产 stdio 的 Admin。
        let handle = tokio::spawn(serve_scoped_with_loopback(
            server_r,
            server_w,
            cfg,
            PERMISSION_TIMEOUT,
            workspaces,
            Arc::clone(&pairing),
            Arc::clone(&hub),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: board_dir.path().to_path_buf(),
                models_path: PathBuf::new(),
                onboarding_env_path: PathBuf::new(),
                onboarding_preferences_path: PathBuf::new(),
                resident_dir: PathBuf::new(),
                // 空路径 = 不读真实 home；这些用例不查 skill。
                skills_home: PathBuf::new(),
                plugin_command: PathBuf::new(),
                launcher: Arc::new(|_project: &Path| Ok(true)),
                theme,
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                // 测试注入:只记账,不碰真实 launchd / home。
                watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                watchman_home: PathBuf::new(),
                lead_inbox: crate::lead::LeadInbox::new(),
                lead_replies: crate::lead::LeadReplies::new(),
            },
            build_test_agent,
            Scope::Admin,
            Some(listener),
            relay_url,
        ));
        // 手机侧凭据:与主循环**共享**的 `pairing` 取一枚(按名字幂等),故 ws 前端认得。
        let token = pairing.seed_local_device("relay-bridge");
        Self {
            client_w,
            client_r: tokio::io::BufReader::new(client_r),
            addr,
            token,
            hub,
            handle,
            _index_dir: index_dir,
            _board_dir: board_dir,
        }
    }

    async fn send(&mut self, line: &str) {
        use tokio::io::AsyncWriteExt;
        self.client_w.write_all(line.as_bytes()).await.unwrap();
        self.client_w.write_all(b"\n").await.unwrap();
        self.client_w.flush().await.unwrap();
    }

    async fn read_value(&mut self) -> serde_json::Value {
        use tokio::io::AsyncBufReadExt;
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, self.client_r.read_line(&mut buf))
            .await
            .expect("timed out waiting for a message")
            .expect("read_line failed");
        assert!(n > 0, "unexpected EOF while waiting for a message");
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
    }

    async fn shutdown(self) {
        let MergedHarness {
            client_w, handle, ..
        } = self;
        drop(client_w);
        let res = tokio::time::timeout(EVENT_WAIT, handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(
            res.is_ok(),
            "merged server should return Ok on stdio EOF: {res:?}"
        );
    }

    async fn connect_ws(
        &self,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let uri: axum::http::Uri = format!("ws://{}/ws", self.addr).parse().unwrap();
        let request = tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {}", self.token))
            .into_client_request()
            .unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws client must authenticate with the seeded token");
        ws
    }
}

async fn ws_send_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, line: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures::SinkExt as _;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(line.into()))
        .await
        .unwrap();
}

async fn ws_recv_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let msg = tokio::time::timeout(EVENT_WAIT, ws.next())
        .await
        .expect("timed out waiting for a ws frame")
        .expect("ws stream ended before a frame arrived")
        .expect("ws transport error");
    serde_json::from_str(msg.to_text().expect("server sent a non-text frame"))
        .expect("server sent invalid JSON")
}

/// 读到 id 匹配的 ws 响应,丢弃中间穿插的通知/其它响应。
async fn ws_read_response<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    want: u64,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for _ in 0..16 {
        let v = ws_recv_json(ws).await;
        if v.get("id") == Some(&serde_json::json!(want)) {
            return v;
        }
    }
    panic!("no ws response with id {want}");
}

/// 读到指定 method 的 ws 通知,丢弃中间帧;超时即失败。
async fn ws_await_notification<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    method: &str,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for _ in 0..16 {
        let v = ws_recv_json(ws).await;
        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
            return v;
        }
    }
    panic!("no ws notification `{method}`");
}

/// 合并模式的扇出证明:stdio 的 `local` 与经环回 ws 的手机跑在**同一个**
/// `serve()` 上——stdio `thread/start` 后,ws 客户端既收到 `thread/started`
/// 通知,`thread/listAll` 也看到同一个 thread id。
#[tokio::test(flavor = "multi_thread")]
async fn merged_loop_fans_out_stdio_notifications_to_ws_client() {
    let mut h = MergedHarness::new().await;
    let mut ws = h.connect_ws().await;
    crate::test_support::initialize(&mut ws).await;

    // stdio:initialize + thread/start。
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 1);
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let mut thread_id = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            thread_id = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
            break;
        }
    }
    let thread_id = thread_id.expect("thread/start response");

    // ws:同一个 thread 的通知必须扇出到这里。
    let notif = ws_await_notification(&mut ws, "thread/started").await;
    assert_eq!(
        notif["params"]["thread_id"].as_str(),
        Some(thread_id.as_str()),
        "ws 客户端必须收到 stdio thread/start 触发的 thread/started"
    );

    // ws:thread/listAll 读到与 stdio 同一个 thread id(同一张表 + 同一工作区)。
    ws_send_json(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":2,"method":"thread/listAll","params":{}}"#,
    )
    .await;
    let v = ws_read_response(&mut ws, 2).await;
    let found = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().unwrap())
        .any(|t| t["thread_id"].as_str() == Some(thread_id.as_str()));
    assert!(
        found,
        "ws thread/listAll must see the stdio-started thread {thread_id}: {v}"
    );

    h.shutdown().await;
}

/// 生产路径(带 `relay_url`)也要收敛:中继端点不可达时 `run_client` 只退避
/// 重连、不返回;stdio EOF 仍必须结束整个合并会话并返回 `Ok(())`。
#[tokio::test(flavor = "multi_thread")]
async fn merged_loop_returns_ok_on_stdio_eof_with_relay_configured() {
    // 未监听端口 = 中继不可达:客户端会失败并退避重连,永不阻塞启动。
    let h =
        MergedHarness::new_with_relay(Some("ws://127.0.0.1:1/connect?session=unit".to_string()))
            .await;
    // 手机仍能经环回前端握手(relay 只影响出站桥,不影响本地前端)。
    let mut ws = h.connect_ws().await;
    crate::test_support::initialize(&mut ws).await;
    h.shutdown().await;
}

/// 合并模式里手机仍是 `Control`:Admin-only 的 `pair/create` 必须被拒。
#[tokio::test(flavor = "multi_thread")]
async fn merged_loop_control_client_is_denied_admin_rpc() {
    let h = MergedHarness::new().await;
    let mut ws = h.connect_ws().await;
    crate::test_support::initialize(&mut ws).await;
    ws_send_json(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":2,"method":"pair/create","params":{}}"#,
    )
    .await;
    let v = ws_read_response(&mut ws, 2).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "control client must be denied admin rpc: {v}"
    );
    h.shutdown().await;
}

/// 跨客户端同步的端到端证明:桌面(stdio)改 mode,手机(ws,Control)必须收到
/// `thread/permissionModeChanged`——修复前没有任何 mode 通知,手机停在旧值。
#[tokio::test(flavor = "multi_thread")]
async fn merged_loop_fans_out_permission_mode_change_to_ws_client() {
    let mut h = MergedHarness::new().await;
    let mut ws = h.connect_ws().await;
    crate::test_support::initialize(&mut ws).await;

    // stdio 起一个 thread。
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = h.read_value().await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let mut thread_id = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) {
            thread_id = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
            break;
        }
    }
    let thread_id = thread_id.expect("thread/start response");

    // ws 客户端先收到 thread/started（建立它的视图），再等 mode 通知。
    let _ = ws_await_notification(&mut ws, "thread/started").await;

    // stdio 切 yolo。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{thread_id}","mode":"yolo"}}}}"#
    ))
    .await;

    let notif = ws_await_notification(&mut ws, "thread/permissionModeChanged").await;
    assert_eq!(
        notif["params"]["thread_id"].as_str(),
        Some(thread_id.as_str())
    );
    assert_eq!(notif["params"]["mode"].as_str(), Some("yolo"));
    h.shutdown().await;
}
/// 裁决是纯函数：给定 hints 与服务端每会话的 (epoch, 锚点是否仍在)，
/// 产出每个 hint 的动作、诊断原因、以及服务端 epoch。四条判定按优先级
/// 短路（spec §4.2）。`server` 返回 `None` 表示该 thread 已不存在。
#[test]
fn plan_sync_covers_the_four_verdicts() {
    let hint = |id: &str, e: u64, anchor: Option<&str>| SyncHint {
        thread_id: id.to_string(),
        content_epoch: e,
        last_item_id: anchor.map(str::to_string),
    };
    let server = |id: &str| -> Option<(u64, bool)> {
        match id {
            "t-deleted" => None,
            "t-stale" => Some((5, true)),
            "t-lost" => Some((3, false)),
            "t-ok" => Some((3, true)),
            "t-behind" => Some((3, true)),
            _ => None,
        }
    };
    let hints = vec![
        hint("t-deleted", 1, Some("a")),
        hint("t-stale", 3, Some("a")),
        hint("t-lost", 3, Some("gone")),
        hint("t-ok", 3, Some("a")),
        hint("t-behind", 3, Some("older")),
    ];

    let plan = plan_sync(&hints, &server);
    let row = |id: &str| {
        plan.iter()
            .find(|(t, _, _, _)| t == id)
            .map(|(_, a, r, e)| (*a, *r, *e))
    };
    assert_eq!(row("t-deleted"), Some((SyncAction::Drop, "deleted", 0)));
    assert_eq!(row("t-stale"), Some((SyncAction::Full, "epoch_changed", 5)));
    assert_eq!(row("t-lost"), Some((SyncAction::Full, "anchor_missing", 3)));
    assert_eq!(row("t-ok"), Some((SyncAction::None, "match", 3)));
    // 「落后」不是失败：锚点仍在 → 缓存有效，补齐交给常规路径（spec §7.2）。
    assert_eq!(row("t-behind"), Some((SyncAction::None, "match", 3)));
    // 顺序必须与 hints 一一对应：调用方按序写回 syncPlan，错位即张冠李戴。
    assert_eq!(
        plan.iter().map(|(t, ..)| t.as_str()).collect::<Vec<_>>(),
        vec!["t-deleted", "t-stale", "t-lost", "t-ok", "t-behind"],
        "plan must preserve the hint order"
    );
}

/// 空 hints（桌面端 / 老客户端）→ 空 plan。这是零回归的门禁。
#[test]
fn plan_sync_is_empty_for_no_hints() {
    assert!(plan_sync(&[], &|_| panic!("must not be called")).is_empty());
}

/// `lastItemId` 为 None（会话从未有过 item）时不得当成"锚点缺失"而误判全量。
#[test]
fn plan_sync_treats_a_null_anchor_as_present_when_epochs_match() {
    let hints = vec![SyncHint {
        thread_id: "t".into(),
        content_epoch: 0,
        last_item_id: None,
    }];
    let plan = plan_sync(&hints, &|_| Some((0, true)));
    assert_eq!(plan[0].1, SyncAction::None);
}

/// 会话删除后，hints 里对它的条目必须判 `drop` —— 端到端跑一遍：
/// `thread/delete` 真的把会话从盘上抹掉，随后拿**生产用的那个查询**
/// （`ThreadStore::load`）当 `plan_sync` 的 `server` 回调，断言 `Drop` /
/// `"deleted"` / epoch 0。
///
/// 相对纯函数里的 stub 用例，这里额外钉住的是「删除在 service 层真的可见，
/// 且 `load` 的 `None` 与 `plan_sync` 约定的 `None` 同义」：stub 只证明算法，
/// 证明不了是哪个查询在喂它。
#[tokio::test(flavor = "multi_thread")]
async fn plan_sync_drops_a_deleted_thread_end_to_end() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let store = crate::thread_store::ThreadStore::new(dir.path());

    // 删除前该会话存在：epoch 0，锚点视为仍在（从未有过 item）。
    let hints = vec![SyncHint {
        thread_id: tid.clone(),
        content_epoch: 0,
        last_item_id: None,
    }];
    let live = |id: &str| {
        store
            .load(id)
            .unwrap()
            .map(|t| (t.meta.content_epoch, true))
    };
    assert_eq!(
        plan_sync(&hints, &live)[0].1,
        SyncAction::None,
        "a live thread must not be dropped"
    );

    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 12, "method": "thread/delete",
            "params": { "threadId": tid },
        })
        .to_string(),
    )
    .await;
    let deleted = read_response(&mut h, 12).await;
    assert!(
        deleted.get("error").is_none(),
        "delete must succeed: {deleted}"
    );

    assert!(
        store.load(&tid).unwrap().is_none(),
        "delete must remove the thread"
    );
    // 同一条 hint：会话消失后必须整条 drop，而不是静默留一个空缓存。
    assert_eq!(
        plan_sync(&hints, &live)[0],
        (tid.clone(), SyncAction::Drop, "deleted", 0),
        "a deleted thread must be dropped"
    );
    h.shutdown().await;
}

/// 无 hints（桌面端 / 老客户端）→ 空 plan，其余字段不变。桌面零回归门禁。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_without_hints_reports_an_empty_sync_plan() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    // `initialize` 助手的响应在流里，重新发一次带 id 的以便断言。
    let resp = rpc(&mut h, 61, "initialize", json!({})).await;
    assert_eq!(
        resp["result"]["syncPlan"],
        json!([]),
        "no hints → empty plan: {resp}"
    );
    assert!(
        resp["result"]["serverInfo"].is_object(),
        "existing fields must survive: {resp}"
    );
    h.shutdown().await;
}

/// 带 hints 时按 §4.2 裁决：clear 过的会话被判 full（epoch 变了）。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_plans_a_full_resync_for_a_cleared_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let cleared = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        cleared.get("error").is_none(),
        "clear must succeed: {cleared}"
    );

    // 客户端手上有 epoch 0 的缓存，但服务端已经是 1。
    let resp = rpc(
        &mut h,
        62,
        "initialize",
        json!({ "resumeHints": [{ "threadId": tid, "contentEpoch": 0 }] }),
    )
    .await;
    let plan = resp["result"]["syncPlan"]
        .as_array()
        .unwrap_or_else(|| panic!("syncPlan must be an array: {resp}"));
    assert_eq!(plan.len(), 1, "one hint must yield one entry: {resp}");
    assert_eq!(plan[0]["threadId"], json!(tid));
    assert_eq!(plan[0]["action"], json!("full"));
    assert_eq!(plan[0]["reason"], json!("epoch_changed"));
    assert_eq!(
        plan[0]["epoch"],
        json!(1),
        "the entry must carry the server epoch"
    );
    h.shutdown().await;
}

/// epoch 一致且锚点仍在 → none（缓存有效，不必重同步）。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_plans_nothing_when_the_hint_matches() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;
    let listed = list_all(&mut h).await;
    let anchor = last_item_id_of(&listed, &tid).expect("a turn must record an anchor");

    let resp = rpc(
        &mut h,
        63,
        "initialize",
        json!({ "resumeHints": [{
            "threadId": tid, "contentEpoch": 0, "lastItemId": anchor,
        }]}),
    )
    .await;
    let plan = resp["result"]["syncPlan"].as_array().unwrap();
    assert_eq!(plan[0]["action"], json!("none"));
    assert_eq!(plan[0]["reason"], json!("match"));
    h.shutdown().await;
}

/// 会话存在但**从未有过 item**（hint 不带 `lastItemId`）→ none。锚点为 null 时
/// 不得当成"锚点缺失"去查盘或判全量：那会把"还没有内容"误读成"缓存失效"。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_plans_nothing_for_a_thread_that_has_no_anchor() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let resp = rpc(
        &mut h,
        65,
        "initialize",
        json!({ "resumeHints": [{ "threadId": tid, "contentEpoch": 0 }] }),
    )
    .await;
    let plan = resp["result"]["syncPlan"].as_array().unwrap();
    assert_eq!(plan[0]["epoch"], json!(0), "epoch must come from the store");
    assert_eq!(plan[0]["action"], json!("none"));
    assert_eq!(plan[0]["reason"], json!("match"));
    h.shutdown().await;
}

/// 锚点已被抹掉（clear 过内容但 epoch 未变的极端形状）→ full / anchor_missing，
/// 证明 `lastItemId` 确实拿去查了真实 jsonl，而不是被忽略或凭 meta 断言。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_plans_a_full_resync_when_the_anchor_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let resp = rpc(
        &mut h,
        66,
        "initialize",
        json!({ "resumeHints": [{
            "threadId": tid, "contentEpoch": 0, "lastItemId": "item-never-existed",
        }]}),
    )
    .await;
    let plan = resp["result"]["syncPlan"].as_array().unwrap();
    assert_eq!(plan[0]["action"], json!("full"));
    assert_eq!(plan[0]["reason"], json!("anchor_missing"));
    h.shutdown().await;
}

/// 不存在的会话 → drop。
#[tokio::test(flavor = "multi_thread")]
async fn initialize_plans_a_drop_for_a_deleted_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    let resp = rpc(
        &mut h,
        64,
        "initialize",
        json!({ "resumeHints": [{ "threadId": "thread-gone", "contentEpoch": 0 }] }),
    )
    .await;
    let plan = resp["result"]["syncPlan"].as_array().unwrap();
    assert_eq!(plan[0]["action"], json!("drop"));
    assert_eq!(plan[0]["reason"], json!("deleted"));
    h.shutdown().await;
}

/// `parse_resume_hints` 是纯函数：畸形条目静默跳过，上限 20 条按 cachedAt 最新优先。
#[test]
fn parse_resume_hints_is_bounded_and_skips_malformed() {
    let raw = json!([
        { "threadId": "t1", "contentEpoch": 2, "lastItemId": "i1", "cachedAt": 5 },
        { "contentEpoch": 9 },                       // 缺 threadId → 跳过
        { "threadId": "t2", "cachedAt": 50 },        // 缺 epoch → 0
        "not-an-object",                             // 畸形 → 跳过
    ]);
    let hints = parse_resume_hints(Some(&raw));
    assert_eq!(
        hints.len(),
        2,
        "malformed entries must be skipped: {hints:?}"
    );
    assert_eq!(hints[0].thread_id, "t2", "newest cachedAt must come first");
    assert_eq!(hints[0].content_epoch, 0);
    assert_eq!(hints[1].thread_id, "t1");
    assert_eq!(hints[1].last_item_id.as_deref(), Some("i1"));

    // 上限 20：造 25 条，取 cachedAt 最大的 20 条。
    let many: Vec<serde_json::Value> = (0..25)
        .map(|i| json!({ "threadId": format!("t{i}"), "cachedAt": i }))
        .collect();
    let hints = parse_resume_hints(Some(&json!(many)));
    assert_eq!(hints.len(), 20);
    assert_eq!(hints[0].thread_id, "t24", "the newest must be kept");

    assert!(parse_resume_hints(None).is_empty());
    assert!(parse_resume_hints(Some(&json!("nope"))).is_empty());
}

/// 记录型工厂：把每次 `build_agent` 时**槽位里实际可见**的关联根记下来。
///
/// 只断言 meta 里的字符串是不够的——那条链「meta ← 索引」通、「槽位 → 工厂」
/// 断掉时，关联根同样是形同虚设的；本夹具把后半段钉死。
fn recording_roots_factory(
    seen: Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        seen.lock().unwrap().push(current_session_extra_roots());
        build_test_agent(session, cwd, mode)
    }
}

/// 读某个 cwd 下某 thread 的 `.meta.json`（真实布局是
/// `<cwd>/.yi-agent/threads/<tid>.meta.json`）。
fn read_meta(cwd: &Path, tid: &str) -> serde_json::Value {
    let path = cwd
        .join(".yi-agent")
        .join("threads")
        .join(format!("{tid}.meta.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

/// Task 6 §2：`thread/start` 把主根当前关联的附属根快照进 meta；
/// `thread/resume` 从 meta 重建同一根集并再次下发到工厂。
///
/// 两侧都断言：meta 里的字符串（持久化）与工厂槽位里的值（真正生效）。
/// resume 前先 `unlink`，从而证明重建来源是 **meta 快照**而非再读索引。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_records_linked_roots_and_resume_rebuilds_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let primary = dir.path().join("primary");
    let secondary = dir.path().join("secondary");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::create_dir_all(&secondary).unwrap();
    let primary = primary.canonicalize().unwrap();
    let secondary = secondary.canonicalize().unwrap();

    let seen: Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>> = Arc::new(std::sync::Mutex::new(vec![]));
    let mut h = Harness::with_factory(
        recording_roots_factory(Arc::clone(&seen)),
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;

    // 关联：primary ← secondary。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"workspace/link","params":{{"primary":"{}","child":"{}"}}}}"#,
        primary.display(),
        secondary.display()
    ))
    .await;
    let linked = read_response(&mut h, 2).await;
    assert_eq!(linked["result"], json!({}), "{linked}");

    // 在 primary 建会话 → meta 应记下 secondary，工厂也必须拿到它。
    h.send(
        &json!({"jsonrpc":"2.0","id":3,"method":"thread/start",
        "params":{"cwd": primary.to_string_lossy()}})
        .to_string(),
    )
    .await;
    let tid = read_thread_start_response(&mut h, 3).await;

    let meta = read_meta(&primary, &tid);
    assert_eq!(
        meta["extra_roots"],
        json!([secondary.to_string_lossy()]),
        "{meta}"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec![secondary.clone()]],
        "工厂槽位里必须是本会话的关联根"
    );

    // 取消关联后再 resume：根集只能来自 meta 快照（索引此时已空）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"workspace/unlink","params":{{"child":"{}"}}}}"#,
        secondary.display()
    ))
    .await;
    let unlinked = read_response(&mut h, 4).await;
    assert_eq!(unlinked["result"], json!({}), "{unlinked}");

    h.send(
        &json!({"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{"threadId": tid}})
            .to_string(),
    )
    .await;
    let resumed = read_response(&mut h, 5).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec![secondary.clone()], vec![secondary.clone()]],
        "resume 必须从 meta 重建同一根集，而非再读索引"
    );
    h.shutdown().await;
}

/// 关联是**会话快照**，不是目录的活属性：同一 cwd 后建的会话拿到新关联，
/// 先建会话的 meta 不被追改，其槽位也不会忽然多出一根。
#[tokio::test(flavor = "multi_thread")]
async fn linked_roots_do_not_leak_across_threads_in_one_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    let other = dir.path().join("other");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let cwd = cwd.canonicalize().unwrap();
    let other = other.canonicalize().unwrap();

    let seen: Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>> = Arc::new(std::sync::Mutex::new(vec![]));
    let mut h = Harness::with_factory(
        recording_roots_factory(Arc::clone(&seen)),
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;

    // 未关联时建的会话：根集为空（meta 与槽位两侧）。
    h.send(
        &json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{"cwd": cwd.to_string_lossy()}})
        .to_string(),
    )
    .await;
    let first = read_thread_start_response(&mut h, 2).await;
    assert_eq!(read_meta(&cwd, &first)["extra_roots"], json!([]));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Vec::<PathBuf>::new()],
        "未关联即单根会话"
    );

    // 之后才关联 cwd ← other。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"workspace/link","params":{{"primary":"{}","child":"{}"}}}}"#,
        cwd.display(),
        other.display()
    ))
    .await;
    let linked = read_response(&mut h, 3).await;
    assert_eq!(linked["result"], json!({}), "{linked}");

    h.send(
        &json!({"jsonrpc":"2.0","id":4,"method":"thread/start",
        "params":{"cwd": cwd.to_string_lossy()}})
        .to_string(),
    )
    .await;
    let second = read_thread_start_response(&mut h, 4).await;
    assert_ne!(first, second, "两个会话必须是不同的 thread");
    assert_eq!(
        read_meta(&cwd, &second)["extra_roots"],
        json!([other.to_string_lossy()]),
        "后建的会话应拿到新关联"
    );
    assert_eq!(
        read_meta(&cwd, &first)["extra_roots"],
        json!([]),
        "先建会话的 meta 不得被追改"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec![], vec![other.clone()]],
        "同一 cwd 的两个会话各拿自己的根集"
    );
    h.shutdown().await;
}

/// `workspace/link` 的输入校验：自关联与不存在的目录一律 `-32602`
/// （与 `workspace/add` 的非目录拒绝同码，UI 直接显示服务端原因）。
#[tokio::test(flavor = "multi_thread")]
async fn workspace_link_rejects_self_and_unknown_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"workspace/link","params":{{"primary":"{p}","child":"{p}"}}}}"#,
        p = a.display()
    ))
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"workspace/link","params":{{"primary":"{p}","child":"{n}"}}}}"#,
        p = a.display(),
        n = dir.path().join("nope").display()
    ))
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    h.shutdown().await;
}

/// `workspace/list` 的每项带 `linked_to`：附属根指向主根，主根自身为 `null`。
/// 附属根必须仍在 `dirs` 里（`thread/listAll` 靠它发现冷会话）。
#[tokio::test(flavor = "multi_thread")]
async fn workspace_list_reports_linked_to() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let a = a.canonicalize().unwrap();
    let b = b.canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    // 主根先在索引里（真实流程：用户从侧栏某目录的组头菜单里「关联文件夹」，
    // 该目录必然已因建会话或 Add folder 进了索引）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"workspace/add","params":{{"path":"{}"}}}}"#,
        a.display()
    ))
    .await;
    let added = read_response(&mut h, 2).await;
    assert!(added.get("error").is_none(), "{added}");

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"workspace/link","params":{{"primary":"{a}","child":"{b}"}}}}"#,
        a = a.display(),
        b = b.display()
    ))
    .await;
    let linked = read_response(&mut h, 3).await;
    assert_eq!(linked["result"], json!({}), "{linked}");

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"workspace/list","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    let list = v["result"]["workspaces"].as_array().unwrap();
    let entry = list
        .iter()
        .find(|w| w["path"] == json!(b.to_string_lossy()))
        .unwrap_or_else(|| panic!("linked dir must be listed: {v}"));
    assert_eq!(entry["linked_to"], json!(a.to_string_lossy()), "{v}");
    // `workspace/link` 顺带把附属根 add 进索引：`thread/listAll` 靠它发现冷会话。
    assert_eq!(entry["exists"], json!(true), "{v}");
    let primary = list
        .iter()
        .find(|w| w["path"] == json!(a.to_string_lossy()))
        .unwrap_or_else(|| panic!("primary must be listed too: {v}"));
    assert_eq!(primary["linked_to"], serde_json::Value::Null, "{v}");

    // dangling link 自愈（spec §7）：主根被「Remove from list」后，附属根必须
    // 回到顶层显示（`linked_to` 不再上报），而 `links` 记录本身不被抹掉。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"workspace/remove","params":{{"path":"{}"}}}}"#,
        a.display()
    ))
    .await;
    let removed = read_response(&mut h, 5).await;
    assert!(removed.get("error").is_none(), "{removed}");

    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"workspace/list","params":{}}"#)
        .await;
    let v = read_response(&mut h, 6).await;
    let entry = v["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["path"] == json!(b.to_string_lossy()))
        .unwrap_or_else(|| panic!("linked dir must still be listed: {v}"));
    assert_eq!(
        entry["linked_to"],
        serde_json::Value::Null,
        "主根掉出索引后附属根必须回到顶层: {v}"
    );
    h.shutdown().await;
}
/// `role` 必须在列表条目上暴露;普通会话不得带该 key(旧客户端字节兼容)。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_exposes_the_lead_role_only_for_the_lead() {
    let (mut h, _dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let start = start_thread(&mut h).await;
    let lead = rpc(&mut h, 5, "thread/lead", serde_json::json!({})).await;
    let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

    let all = list_all(&mut h).await;
    // 朴素收集:`.leak()` 会永久泄漏内存,只为借出一个 `&Value`;克隆更清楚。
    let entries: Vec<serde_json::Value> = all["result"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
        .collect();

    let lead_entry = entries
        .iter()
        .find(|t| t["thread_id"].as_str() == Some(lead_id.as_str()))
        .expect("lead entry");
    assert_eq!(lead_entry["role"], serde_json::json!("lead"));

    let plain_entry = entries
        .iter()
        .find(|t| t["thread_id"].as_str() == Some(start.as_str()))
        .expect("plain entry");
    assert!(
        plain_entry.get("role").is_none(),
        "a plain thread must not carry a role key: {plain_entry}"
    );

    h.shutdown().await;
}

/// 启动期迁移的接线契约：服务端入口调用的函数确实把存量 `.env` 模型键搬进清单。
///
/// 入口点（`run` / `serve_stdio_with_relay`）自身的调用由生产 `HOME` 注入的路径
/// 决定，测试不便直接驱动；这里锁定「入口调用的那个函数」的行为，与计划 Task 6
/// Step 1 的意图一致。
#[test]
fn startup_migrates_legacy_model_keys_into_the_catalog() {
    let dir = tempfile::TempDir::new().unwrap();
    let env_path = dir.path().join(".env");
    std::fs::write(
        &env_path,
        "YI_AGENT_PROVIDER=openai\nMODEL_API_KEY=sk-old\n",
    )
    .unwrap();
    let models_path = dir.path().join("models.json");

    yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);

    let catalog = yi_agent_runtime::models::load_catalog_from(&models_path);
    assert_eq!(catalog.models.len(), 1);
    assert!(
        !std::fs::read_to_string(&env_path)
            .unwrap()
            .contains("\nMODEL_API_KEY=sk-old")
    );
}

/// `log_outcome` 必须对每一种结果都不 panic（日志措辞由 tracing 承接）。
#[test]
fn log_outcome_handles_every_variant() {
    use yi_agent_runtime::migrate::{MigrationOutcome, log_outcome};
    let env = std::path::Path::new("/tmp/yi-agent-migrate-probe/.env");
    log_outcome(&MigrationOutcome::NothingToDo, env);
    log_outcome(&MigrationOutcome::Migrated { entry: "m".into() }, env);
    log_outcome(&MigrationOutcome::AlreadyCategorized, env);
    log_outcome(
        &MigrationOutcome::EnvNotWritable {
            reason: "ro".into(),
        },
        env,
    );
    log_outcome(
        &MigrationOutcome::CatalogNotWritable {
            reason: "ro".into(),
        },
        env,
    );
    log_outcome(
        &MigrationOutcome::InvalidProvider {
            reason: "unknown provider: azure".into(),
        },
        env,
    );
}

/// 看板卡片会话所在的 worktree **不是项目**,`thread/lead` 不得为它种组长。
///
/// 这正是真机上的幽灵组长来源:卡片会话的 cwd 是 `<项目>/.worktrees/kanban/<slug>`,
/// 它调 `ask_lead` → 主循环 `resolve_lead_target` → 这条 RPC 的同一个
/// `ensure_lead_meta`,于是 worktree 里凭空多出一个 `role:"lead"`,侧栏里那个
/// worktree 就变成一个带组长的顶层项目组。真机数据里已有 9 例。
#[tokio::test(flavor = "multi_thread")]
async fn thread_lead_refuses_to_create_a_lead_in_a_card_worktree() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");

    let mut h = Harness::new();
    initialize(&mut h).await;

    let v = rpc(
        &mut h,
        5,
        "thread/lead",
        serde_json::json!({ "cwd": wt.to_string_lossy() }),
    )
    .await;
    assert!(
        v.get("error").is_some(),
        "a card worktree has no project to lead; the call must be refused: {v}"
    );
    assert!(
        crate::thread_store::ThreadStore::new(&wt)
            .find_lead()
            .unwrap()
            .is_none(),
        "nothing may be persisted inside the card worktree"
    );

    // 项目根照旧:拒绝的是路径,不是功能。
    let ok = rpc(
        &mut h,
        6,
        "thread/lead",
        serde_json::json!({ "cwd": project.to_string_lossy() }),
    )
    .await;
    assert!(
        ok["result"]["thread_id"].as_str().is_some(),
        "a real project still gets its lead: {ok}"
    );

    h.shutdown().await;
}

/// `listAll` 不得给「只含卡片会话的 worktree」懒创建组长,也不得把 worktree 里
/// 的假组长当作组长暴露出去。
///
/// 两条懒创建/暴露路径都必须与 `find_lead` 同口径,否则 worktree 会在侧栏里
/// 顶层成组、并多出一条「组长」行——正是用户报的现象。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_never_treats_a_card_worktree_as_a_project_with_a_lead() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");
    let p = project.to_string_lossy().to_string();
    let w = wt.to_string_lossy().to_string();
    write_meta(&wt, "t-card", "看板 · card-1", Some(&p), Some("card-1"));
    // 复刻真机落盘状态:worktree 里还躺着一个幽灵组长(`role:"lead"`,
    // `driver_project: None`)。它必须既不顶层成组、也不暴露为组长。
    write_lead_meta(&wt, "t-phantom-lead");

    let mut h = Harness::new();
    initialize(&mut h).await;
    // worktree 进索引 = 卡片会话落盘后的样子;项目根也进索引。
    add_workspace(&mut h, 11, &w).await;
    add_workspace(&mut h, 12, &p).await;

    let all = list_all(&mut h).await;
    let groups = all["result"]["groups"].as_array().unwrap();
    assert!(
        !groups.iter().any(|g| g["workspace"] == w.as_str()),
        "the card worktree must not become a top-level group: {all}"
    );

    let entries: Vec<serde_json::Value> = groups
        .iter()
        .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
        .collect();
    // 只有**卡片 worktree 里**的条目不许被暴露为组长:项目根自己那个组长
    // (listAll 为它懒创建,合法)照旧带 `role:"lead"`。
    assert!(
        !entries.iter().any(|t| {
            t["role"] == serde_json::json!("lead") && t["cwd"] == serde_json::json!(w)
        }),
        "the card worktree must not expose a lead row: {all}"
    );
    assert!(
        !entries
            .iter()
            .any(|t| t["thread_id"] == serde_json::json!("t-phantom-lead")),
        "the phantom lead is not a project's session and must not be listed: {all}"
    );

    h.shutdown().await;
}

/// 卡片会话（项目键落在 `.worktrees/kanban/` 下）不该拿到 `ask_lead`:它没有组长,
/// 注册了只会投进空邮箱,并在主循环里造一个假组长。判定由装配期传入的项目键决定。
#[test]
fn a_card_session_does_not_get_ask_lead() {
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

    // 一个真实的卡片会话项目键:<worktree>/.yi-agent。
    let card_key = repo
        .path()
        .join(".worktrees/kanban/card-1")
        .join(".yi-agent")
        .display()
        .to_string();
    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-card",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        card_key,
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
    )
    .expect("tooling")
    .registry
    .names();
    assert!(
        !names.contains(&"ask_lead".to_string()),
        "a card session has no lead to ask, got {names:?}"
    );

    // 普通会话照旧有 `ask_lead`:排除的是卡片会话,不是功能。
    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-plain",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        repo.path().join(".yi-agent").display().to_string(),
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
    )
    .expect("tooling")
    .registry
    .names();
    assert!(
        names.contains(&"ask_lead".to_string()),
        "a plain session keeps ask_lead, got {names:?}"
    );
}

/// 组长**不该**拿到 `ask_lead`:它自己就是那个收件人,投递等于自问自答。
///
/// 根因:装配期只排除了卡片 worktree(见上一条测试),而组长的项目键就是它自己
/// 的 cwd/项目根,于是它也被注册了 `ask_lead`。组长调它时,主循环的
/// `resolve_lead_target(project_key)` 反查出来的正是**它自己**(`find_lead` 返回
/// 该组长),消息便投回本 thread —— 一个自环:单向模式下每一轮都往自己信箱里塞
/// 一条 `[来自会话 <自己>]`,阻塞模式下则登记一个只有自己才能回填的 request_id,
/// 一路上无人应答、只能等满 120s 超时。正常实现里组长要影响自己直接写就行,不
/// 需要、也不该有这个工具。
///
/// 判定复用**唯一权威** `lead_memory::is_lead_thread`(读落盘的 `ThreadMeta.role`),
/// 与同函数内 lead_memory 的注册同源;不靠标题/cwd/名字猜。
#[test]
fn the_lead_does_not_get_ask_lead() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();
    let project_root = repo.path().to_path_buf();

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

    // 正臂的组长走生产的落盘路径(`ensure_lead_meta`),不手搓一个「像组长」的形状。
    let store = crate::thread_store::ThreadStore::new(&project_root);
    let (lead, _created) = crate::lead::ensure_lead_meta(&store, &project_root).expect("lead meta");
    assert_eq!(
        lead.role.as_deref(),
        Some(crate::thread_store::LEAD_ROLE),
        "the production lead path must persist the role"
    );

    let build = |thread_id: &str| {
        build_runtime_tooling(
            &cfg,
            &root,
            thread_id,
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
            crate::git_diff_tool::GitDiffHandle::new(),
            Vec::new(),
            project_root.join(".yi-agent").display().to_string(),
            crate::lead::LeadInbox::new(),
            crate::lead::LeadReplies::new(),
        )
        .expect("tooling")
    };

    let lead_names = build(&lead.thread_id).registry.names();
    assert!(
        !lead_names.contains(&"ask_lead".to_string()),
        "the lead is its own recipient and must not ask itself, got {lead_names:?}"
    );
    // 非空 witness:「看不到」必须是真的缺席,而不是整个 registry 空掉——组长
    // 照旧拿得到自己该有的东西(委派工具与账本)。
    assert!(
        lead_names.contains(&"spawn_agent".to_string()),
        "the lead still gets the shared tools, got {lead_names:?}"
    );
    assert!(
        lead_names.contains(&"lead_remember".to_string()),
        "the lead still gets its ledger, got {lead_names:?}"
    );

    // 负臂:落过盘的普通会话照旧有 `ask_lead`——排除的是组长,不是功能。
    let plain = crate::thread_store::ThreadMeta {
        thread_id: "thread-plain".to_string(),
        cwd: project_root.display().to_string(),
        model: String::new(),
        created_at: 1,
        updated_at: 1,
        title: Some("普通会话".to_string()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        extra_roots: Vec::new(),
        role: None,
        content_epoch: 0,
        last_item_id: None,
    };
    store.create(&plain).expect("a plain meta must persist");
    let plain_names = build("thread-plain").registry.names();
    assert!(
        plain_names.contains(&"ask_lead".to_string()),
        "a plain session keeps ask_lead, got {plain_names:?}"
    );
}
