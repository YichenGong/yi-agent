use super::*;

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
            plugin_bindings: Vec::new(),
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
            crate::lead_control::LeadControlInbox::new(),
            crate::lead_control::LeadControlReplies::new(),
            None,
            // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
            // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
            Some(true),
            Vec::new(),
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

#[test]
fn only_the_lead_sees_the_view_tools() {
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
    let project_root = repo.path().canonicalize().expect("repo path canonicalizes");

    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    store
        .create(&crate::thread_store::ThreadMeta {
            plugin_bindings: Vec::new(),
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
        })
        .expect("a plain meta must persist");
    // 正臂走生产的组长落盘路径,而不是手搓一个 role 字段。
    let (lead, _created) = crate::lead::ensure_lead_meta(&store, &project_root).expect("lead meta");

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
            crate::lead_control::LeadControlInbox::new(),
            crate::lead_control::LeadControlReplies::new(),
            None,
            // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
            // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
            Some(true),
            Vec::new(),
        )
        .expect("tooling")
    };

    let lead_names = build(&lead.thread_id).registry.names();
    assert!(
        lead_names.contains(&"lead_list_threads".to_string()),
        "the lead must see lead_list_threads, got {lead_names:?}"
    );
    assert!(
        lead_names.contains(&"lead_read_thread".to_string()),
        "the lead must see lead_read_thread, got {lead_names:?}"
    );

    // 负臂:落过盘的普通会话,以及从未落过盘的会话。
    for plain in ["thread-plain", "thread-never-persisted"] {
        let names = build(plain).registry.names();
        assert!(
            !names.contains(&"lead_list_threads".to_string()),
            "{plain} is not the lead and must not see lead_list_threads, got {names:?}"
        );
        assert!(
            !names.contains(&"lead_read_thread".to_string()),
            "{plain} is not the lead and must not see lead_read_thread, got {names:?}"
        );
        // 非空 witnesses:「看不到」必须是真的缺席,而不是整个 registry 空掉。
        assert!(
            names.contains(&"ask_lead".to_string()),
            "{plain} still gets the shared tools, got {names:?}"
        );
    }
}

#[test]
fn only_the_lead_sees_the_control_tools() {
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

    // 两个写工具是组长写通道的消费侧:装配期注入一对独立实例即可(本测试只断言
    // 「注册表里有没有」,不发指令;发指令的集成测试是 Task 5 的事)。
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
            crate::lead_control::LeadControlInbox::new(),
            crate::lead_control::LeadControlReplies::new(),
            None,
            // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
            // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
            Some(true),
            Vec::new(),
        )
        .expect("tooling")
    };

    // 负臂:从未落过盘的普通会话。
    let plain_names = build("thread-never-persisted").registry.names();
    assert!(
        !plain_names.contains(&"lead_interrupt_thread".to_string())
            && !plain_names.contains(&"lead_reject_thread".to_string()),
        "a plain thread must not see the control tools, got {plain_names:?}"
    );
    // 非空 witness:缺席是真的缺席,而不是整个 registry 空掉。
    assert!(
        plain_names.contains(&"ask_lead".to_string()),
        "a plain thread still gets the shared tools, got {plain_names:?}"
    );

    // 正臂走生产的组长落盘路径,而不是手搓一个 role 字段。`project_root` 取
    // `canonicalize()` 形态(与 `only_the_lead_sees_the_memory_tools` 同一手法);
    // store 仍用生产拼写(`cfg.workdir`,非 canonical),与装配里读 role 的那份同源。
    let project_root = repo.path().canonicalize().expect("repo path canonicalizes");
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    let (lead_meta, _created) = crate::lead::ensure_lead_meta(&store, &project_root)
        .expect("the production lead path must persist a lead meta");
    assert_eq!(
        lead_meta.role.as_deref(),
        Some(crate::thread_store::LEAD_ROLE),
        "the production lead path must persist the role"
    );

    let lead_names = build(&lead_meta.thread_id).registry.names();
    assert!(
        lead_names.contains(&"lead_interrupt_thread".to_string())
            && lead_names.contains(&"lead_reject_thread".to_string()),
        "the lead must see both control tools, got {lead_names:?}"
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

/// `None`（装配方漏传）必须等价于关——失败关闭。
#[test]
fn a_missing_flag_value_registers_no_lead_tools() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let names = tooling_names_for(
        repo.path(),
        runtime.path(),
        "thread-plain",
        repo.path().join(".yi-agent").display().to_string(),
        None,
    );
    assert!(
        !names.iter().any(|n| n.contains("lead")),
        "a missing flag must fail closed: {names:?}"
    );
}

/// 组长开关关掉时，`thread/lead` 必须是「方法不存在」，而不是「存在但拒绝」。
///
/// 前端据此只需看 `build/info` + `flags/list`，不必再判断权限；把错误码换成
/// 「拒绝」会让前端多一条永远走不到的权限分支。
#[tokio::test]
async fn thread_lead_is_method_not_found_when_the_flag_is_off() {
    let mut h = Harness::with_lead_flag(false).await;
    initialize(&mut h).await;
    let response = rpc(&mut h, 2, "thread/lead", json!({})).await;
    assert_eq!(
        response["error"]["code"],
        json!(-32601),
        "a disabled lead must look non-existent, not forbidden: {response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("thread/lead"),
        "the error must name the method so the client can recognize it: {response}"
    );
    h.shutdown().await;
}

/// 开关关掉时**先判开关、后校参数**：传错参数也必须是 `-32601`。
///
/// 顺序反了的话，关掉开关时调用方会收到 `invalid_params`，看起来像自己写错了
/// 参数，而不是「这个能力不存在」。
#[tokio::test]
async fn thread_lead_reports_not_found_before_validating_params_when_off() {
    let mut h = Harness::with_lead_flag(false).await;
    initialize(&mut h).await;
    let response = rpc(&mut h, 2, "thread/lead", json!({ "cwd": 12345 })).await;
    assert_eq!(
        response["error"]["code"],
        json!(-32601),
        "the gate must run before param validation: {response}"
    );
    h.shutdown().await;
}

/// 开关打开时 `thread/lead` 仍然照常工作（门禁没有把它一起挡掉）。
#[tokio::test]
async fn thread_lead_still_works_when_the_flag_is_on() {
    let (mut h, dir) = isolated_lead_harness().await;
    initialize(&mut h).await;
    let response = rpc(
        &mut h,
        2,
        "thread/lead",
        json!({ "cwd": dir.path().display().to_string() }),
    )
    .await;
    assert!(
        response["result"]["thread_id"].is_string(),
        "with the flag on, thread/lead must answer as before: {response}"
    );
    h.shutdown().await;
}

/// 开关关掉时，`thread/listAll` 绝不能**往磁盘上**种组长。
///
/// 这是最容易漏的一条：响应里不带组长条目很容易做到（过滤即可），但
/// `scan_workspace_metas` 会在列举空项目时顺手 `ensure_lead_meta`。副作用一旦
/// 落盘，关掉开关也删不掉，用户会看到一个个空的"组长"冒出来。故这里断言的是
/// **磁盘**，不是响应。
///
/// 直接测纯函数而不走 `thread/listAll`：索引在 `serve` 内部，测试无法注入
/// 一个"空项目"；而种不种组长恰好是这个函数唯一的分支。
#[test]
fn list_all_does_not_seed_a_lead_when_the_flag_is_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let dirs = vec![dir.path().display().to_string()];

    let _ = crate::rpc::thread::scan_workspace_metas_for_test(&dirs, false);
    assert!(
        !has_lead_meta(dir.path()),
        "a disabled flag must not write a lead meta to disk"
    );

    // 正向对照：同一个目录、同一个函数，只把开关打开就必须种出来。
    // 没有这一半的话，上面那条在"函数根本不种组长"时也会通过。
    let _ = crate::rpc::thread::scan_workspace_metas_for_test(&dirs, true);
    assert!(
        has_lead_meta(dir.path()),
        "with the flag on, an empty project must still get its lead"
    );
}

/// `thread/lead` 必须幂等:同一 cwd 反复调用只造一个组长,第二次 `created=false`。
#[tokio::test(flavor = "multi_thread")]
async fn thread_lead_is_idempotent_per_project() {
    let (mut h, _dir) = isolated_lead_harness().await;
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
    let (mut h, dir) = isolated_lead_harness().await;
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
    let (mut h, _dir) = isolated_lead_harness().await;
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

/// 同一段 digest 文本不许被投递两次——审批期的重复 tick 必须被压掉。
///
/// 背景:`decide_wake` 对每个 `AwaitingApproval` **无条件**产出 `ThreadStuck`,而
/// `LEAD_TICK = 30s`、`PERMISSION_TIMEOUT = 300s`——一次审批窗会让组长被同一条文本
/// 唤醒最多 10 次,每次都是带全部上下文的完整 LLM turn(纯噪声 + 真金白银)。
/// 去重键取**文本本身**:不用理解状态机就天然覆盖所有 wake reason;态势每轮照常现算
/// (`lead_tick_once` 不动),只是不重复**投递**一模一样的文本。
#[test]
fn identical_digests_are_not_delivered_twice() {
    let mut latch = HashMap::new();
    let mk = |text: &str| LeadTickDelivery {
        project_key: "/p".to_string(),
        lead: LeadTarget {
            cwd: "/p".to_string(),
            thread_id: "lead-1".to_string(),
        },
        text: text.to_string(),
    };
    // 第一轮：投递。
    let first = take_changed_digests(vec![mk("卡住:thread-a")], &mut latch);
    assert_eq!(first.len(), 1);
    // 第二轮：同样的文本 → 不再投递（审批期的 10 次重复从此消失）。
    let second = take_changed_digests(vec![mk("卡住:thread-a")], &mut latch);
    assert!(second.is_empty(), "同一文本不得二次投递");
    // 第三轮：内容变了（新 thread 卡住）→ 恢复投递。
    let third = take_changed_digests(vec![mk("卡住:thread-a, thread-b")], &mut latch);
    assert_eq!(third.len(), 1);
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
        Harness::with_config_and_lead(
            cfg,
            recording_agent_factory(Arc::clone(&seen_1)),
            PERMISSION_TIMEOUT,
            true,
        )
        .await
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
        Harness::with_config_and_lead(
            cfg,
            recording_agent_factory(Arc::clone(&seen_2)),
            PERMISSION_TIMEOUT,
            true,
        )
        .await
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
async fn thread_rename_does_not_change_lead_title() {
    let parent = tempfile::TempDir::new().unwrap();
    let project = parent.path().join("my-app");
    std::fs::create_dir_all(&project).unwrap();
    let mut cfg = test_config();
    cfg.workdir = project.clone();
    let mut h =
        Harness::with_config_and_lead(cfg, build_test_agent, PERMISSION_TIMEOUT, true).await;
    initialize(&mut h).await;

    let lead = rpc(&mut h, 2, "thread/lead", json!({})).await;
    let thread_id = lead["result"]["thread_id"].as_str().unwrap().to_string();
    let renamed = rpc(
        &mut h,
        3,
        "thread/rename",
        json!({ "threadId": thread_id, "title": "manual override" }),
    )
    .await;
    assert!(
        renamed.get("error").is_none(),
        "rename response stays compatible: {renamed}"
    );

    let loaded = crate::thread_store::ThreadStore::new(&project)
        .load(&thread_id)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.meta.title.as_deref(), Some("my-app 组长"));
    h.shutdown().await;
}

/// 组长会话即便标题为空也不生成标题（`is_lead_meta` 这条分支本身的判据）。
///
/// 真机出现过 9 例「幽灵组长」（`ask_lead` 懒创建、未命名）。若只用「标题已存在」
/// 来挡，这类会话会被误当成首轮而改名；这条用例把 `role:"lead"` 单独钉住。
#[tokio::test(flavor = "multi_thread")]
async fn a_lead_thread_never_calls_the_title_model_even_without_a_title() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let tid = "thread-ghost-lead";
    {
        let store = crate::thread_store::ThreadStore::new(dir.path());
        store
            .create(&crate::thread_store::ThreadMeta {
                plugin_bindings: Vec::new(),
                thread_id: tid.into(),
                cwd: dir.path().to_string_lossy().into(),
                model: "m".into(),
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
                anchor_sha: None,
                last_read_at: 0,
                last_completed_at: 0,
                forked_from: None,
                extra_roots: Vec::new(),
                role: Some(crate::thread_store::LEAD_ROLE.into()),
            })
            .unwrap();
    }

    let title_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CountingTitleProvider {
        title_calls: Arc::clone(&title_calls),
    });
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let resumed = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    assert!(
        resumed.get("error").is_none(),
        "resume must succeed: {resumed}"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"登录页报错了"}}]}}}}"#
    ))
    .await;
    let _ = read_until_method(&mut h, "turn/completed").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        title_calls.load(Ordering::SeqCst),
        0,
        "lead threads must never be auto-renamed"
    );
    // `touch` 对组长直接跳过，连兜底标题都不写。
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(tid)
        .ok()
        .flatten()
        .expect("meta must survive");
    assert_eq!(loaded.meta.title, None, "lead meta must stay untitled");
    h.shutdown().await;
}

/// `role` 必须在列表条目上暴露;普通会话不得带该 key(旧客户端字节兼容)。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_exposes_the_lead_role_only_for_the_lead() {
    let (mut h, _dir) = isolated_lead_harness().await;
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

    // 开关打开：这条用例钉的是「卡片 worktree 不种组长」，关掉开关时
    // 它会被上一步的 -32601 提前挡掉，测不到目标逻辑。
    let mut h = Harness::with_lead_flag(true).await;
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

    // 修复的正面判据:卡片会话**必须**折进它所属的项目组(靠 meta 里的
    // `driver_project`)。少了这一步,它自己的 worktree 组又被上面抑制——两头
    // 落空,会话在侧栏彻底不可见(这正是用户报的现象)。
    let project_group = groups
        .iter()
        .find(|g| g["workspace"] == p.as_str())
        .unwrap_or_else(|| panic!("the project root must be a group: {all}"));
    assert!(
        project_group["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["thread_id"] == serde_json::json!("t-card")),
        "the card session must fold into its project group, not vanish: {all}"
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
        names.contains(&"ask_lead".to_string()),
        "a plain session keeps ask_lead, got {names:?}"
    );
}

/// 合并轮会话（项目键落在 `.worktrees/kanban-merge/` 下）同样不该拿到 `ask_lead`。
///
/// 合并轮把 base 检出到 `<项目>/.worktrees/kanban-merge/<slug>`（仅当 base 未被
/// 任何 worktree 检出时），独立合并卡的新会话就起在那里——与卡片会话同属「看板
/// 产生的会话」，那个目录同样不是一个项目。判定若只认 `.worktrees/kanban`，
/// 合并轮会话就会被注册 `ask_lead`，重蹈卡片会话的覆辙（投空邮箱 + 在主循环里给
/// `kanban-merge` 造一个假组长）。
#[test]
fn a_merge_round_session_does_not_get_ask_lead() {
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

    // 一个真实的合并轮会话项目键:<kanban-merge worktree>/.yi-agent。
    let merge_key = repo
        .path()
        .join(".worktrees/kanban-merge/main")
        .join(".yi-agent")
        .display()
        .to_string();
    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-merge",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        merge_key,
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
        !names.contains(&"ask_lead".to_string()),
        "a merge round session has no lead to ask, got {names:?}"
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
            crate::lead_control::LeadControlInbox::new(),
            crate::lead_control::LeadControlReplies::new(),
            None,
            // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
            // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
            Some(true),
            Vec::new(),
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
        plugin_bindings: Vec::new(),
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
        anchor_sha: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
    };
    store.create(&plain).expect("a plain meta must persist");
    let plain_names = build("thread-plain").registry.names();
    assert!(
        plain_names.contains(&"ask_lead".to_string()),
        "a plain session keeps ask_lead, got {plain_names:?}"
    );
}
