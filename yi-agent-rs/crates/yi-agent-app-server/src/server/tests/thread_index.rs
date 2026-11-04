use super::*;

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

#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_returns_null_before_any_frame() {
    let mut cfg = test_config();
    cfg.workdir = tempfile::TempDir::new().unwrap().path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/view/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(9)) {
        v = h.read_value().await;
    }
    // `result` 必须是**成功响应里的 null**，不是「没有 result 键」——方法与错误
    // 分支同样给不出 result，只断言 `is_null()` 会把 method_not_found 放过去。
    assert!(v.get("error").is_none(), "must be a success: {v}");
    assert!(v.as_object().unwrap().contains_key("result"), "{v}");
    assert!(
        v["result"].is_null(),
        "no frame yet must read back as null: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_refresh_produces_a_structured_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(10)) {
        v = h.read_value().await;
    }
    // 无帧时 refresh 走结构化默认：应是 diff 原语、基准是会话锚点、含未跟踪内容。
    assert_eq!(v["result"]["asKind"], "diff", "frame: {v}");
    assert_eq!(
        v["result"]["baseKind"], "session",
        "the session anchor is the base: {v}"
    );
    assert_eq!(v["result"]["command"], serde_json::Value::Null);
    assert!(
        v["result"]["stdout"].as_str().unwrap().contains("+y"),
        "frame: {v}"
    );
    h.shutdown().await;
}

/// 第二帧必须**重算**，不能是第一次的缓存：命令帧重跑同一条命令，故新写入的
/// 内容会出现在重读结果里。没有这条断言，「refresh 只是把缓存又读了一遍」也
/// 能让上面那条测试全绿。
#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_refresh_reruns_a_command_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    {
        // 帧表与主循环共享同一 `Arc`（`RuntimeAttachments::view_frames` 正是为此
        // 注入）：这里塞进一条命令帧，模拟 Task 4 的工具写帧 / Task 9 的直接注入。
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: Some("git log --oneline".to_string()),
                as_kind: "text".to_string(),
                exit_code: Some(0),
                stdout: "stale\n".to_string(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: None,
                base_kind: String::new(),
            },
        );
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(12)) {
        v = h.read_value().await;
    }
    assert_eq!(
        v["result"]["command"], "git log --oneline",
        "a command frame keeps its command: {v}"
    );
    let stdout = v["result"]["stdout"].as_str().unwrap();
    assert!(
        stdout.contains("init"),
        "the command must have been re-run, not re-read: {stdout:?}"
    );
    assert!(!stdout.contains("stale"), "stale frame: {stdout:?}");
    h.shutdown().await;
}

/// refresh 重算结构化帧时必须**保留原基准的来历**：模型用 `base` 指定的帧
/// (`baseKind == "explicit"`)重算后仍得是 `explicit`。若重算时丢掉那个 base、
/// 只按会话锚点算，帧就从「跟 X 比」变成「本次会话以来」——回执与面板一起骗人，
/// 正是 D3 要杜绝的那类误导。
#[tokio::test(flavor = "multi_thread")]
async fn refresh_preserves_an_explicit_base() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
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
    // HEAD 之上再造一个可当显式基准的提交。
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-qam", "edit a"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    // 显式基准取**第一个**提交（HEAD 的父），与会话锚点（= thread/start 时的
    // HEAD）不同：这样 base 与 baseKind 两条断言都能真正区分「保留了原基准」
    // 与「退化成了会话锚点」。
    let base = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD~1"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    {
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: None,
                as_kind: "diff".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: Some(base.clone()),
                base_kind: "explicit".to_string(),
            },
        );
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":14,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(14)) {
        v = h.read_value().await;
    }
    assert_eq!(
        v["result"]["baseKind"], "explicit",
        "a refresh must keep an explicit base explicit: {v}"
    );
    assert_eq!(
        v["result"]["base"], base,
        "the explicit base must survive: {v}"
    );
    h.shutdown().await;
}

/// refresh 重算期间若有更晚的写入者落帧，**不得**被这次 refresh 覆盖回去。
///
/// 窗口确实存在：「读缓存命令」与「写回新帧」之间隔了一次 await（重跑命令，
/// 上限 `VIEW_TIMEOUT`）。老实现无条件覆盖，于是「刚请求的命令」会被一个更晚的
/// 命令盖掉，面板显示的内容与回执不符——正是 D3 要杜绝的误导。
///
/// 用一条会睡 2 秒的命令把窗口撑开，使本测试确定可复现：先让 refresh 读到这条
/// 慢命令并开始重跑，随后注入一条「更晚的」命令帧。老实现会在慢命令跑完后把
/// 它盖回去（断言失败）；CAS 实现则让位，返回更晚的那帧。
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn a_later_frame_written_during_a_refresh_is_not_clobbered() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let frame = |command: Option<&str>, stdout: &str| crate::git_diff::ViewFrame {
        command: command.map(str::to_string),
        as_kind: "text".to_string(),
        exit_code: Some(0),
        stdout: stdout.to_string(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
        note: None,
        base: None,
        base_kind: String::new(),
    };
    {
        // 播下一条「慢命令」帧：refresh 会读到它、并真的去重跑它。
        let mut frames = h.view_frames.lock().await;
        frames.insert(tid.clone(), frame(Some("sleep 2"), ""));
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":13,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    // 给主循环留出「读快照 + 进入重跑」的时间，此时它正卡在 `sleep 2` 上。
    tokio::time::sleep(Duration::from_millis(400)).await;
    {
        // 更晚的写入者（真实场景是另一次 refresh，或 Task 4 的 watcher 写帧）。
        let mut frames = h.view_frames.lock().await;
        frames.insert(tid.clone(), frame(Some("git log --oneline"), "LATER\n"));
    }
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(13)) {
        v = h.read_value().await;
    }
    assert_eq!(
        v["result"]["command"], "git log --oneline",
        "a later frame must win over the stale refresh: {v}"
    );
    assert_eq!(
        v["result"]["stdout"], "LATER\n",
        "the refresh must not clobber a newer frame: {v}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_rejects_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"thread/view/read","params":{"threadId":"thread-nope"}}"#)
        .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(11)) {
        v = h.read_value().await;
    }
    assert_eq!(
        v["error"]["code"], -32011,
        "unknown thread uses the shared code"
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
        ("settings/read", Domain::Ui),
        ("settings/write", Domain::Ui),
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
        // ---- Resident(通用常驻登记) ----
        ("resident/list", Domain::Resident),
        // ---- Plugin(`plugin/*` 与 `plugins/*`) ----
        ("plugin/query", Domain::Plugin),
        ("plugin/install", Domain::Plugin),
        ("plugin/uninstall", Domain::Plugin),
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
        // ---- Mcp(设置页 MCP 面板:项目级 mcp.json + schema 缓存) ----
        ("mcp/list", Domain::Mcp),
        // ---- Thread ----
        ("thread/clear", Domain::Thread),
        ("thread/compact", Domain::Thread),
        ("thread/delete", Domain::Thread),
        ("thread/diff/read", Domain::Thread),
        ("thread/fork", Domain::Thread),
        ("thread/lead", Domain::Thread),
        ("thread/list", Domain::Thread),
        ("thread/listAll", Domain::Thread),
        ("thread/markRead", Domain::Thread),
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
        ("thread/view/read", Domain::Thread),
        // ---- Agent ----
        ("agent/cancel", Domain::Agent),
        ("agent/cancel/preview", Domain::Agent),
        ("agent/children/list", Domain::Agent),
        ("agent/message", Domain::Agent),
        ("agent/trace/read", Domain::Agent),
        ("agent/trace/unwatch", Domain::Agent),
        ("agent/trace/watch", Domain::Agent),
        // ---- Runtime（只读运行时状态） ----
        ("runtime/status", Domain::Runtime),
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
            models_path: PathBuf::new(),
            websearch_path: PathBuf::new(),
            settings_path: PathBuf::new(),
            lead_agent_enabled: false,
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            remote_dir: PathBuf::new(),
            // 旁支宿主资源必须落在隔离目录:空 remote_dir + None 会让 serve()
            // 把 side-conversations/ 建到当前工作目录(源码树)。
            side_registry: Some(crate::server::tests::test_side_registry(
                index_dir.path(),
                None,
            )),
            theme: test_theme(),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            // 测试注入:只记录调用,不碰真实 launchd / home。
            watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
            watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
            watchman_home: PathBuf::new(),
            // 测试注入:绝不为一次安装真的拉起 detached daemon(那会拿测试
            // 二进制当 daemon、并白等 10s)。
            resolve_plugin: Arc::new(|_name: &str| -> Option<PathBuf> { None }),
            ensure_daemon: Arc::new(|_project: &Path| -> Result<bool, String> { Ok(true) }),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
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
            models_path: PathBuf::new(),
            websearch_path: PathBuf::new(),
            settings_path: PathBuf::new(),
            lead_agent_enabled: false,
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            remote_dir: PathBuf::new(),
            // 旁支宿主资源必须落在隔离目录:空 remote_dir + None 会让 serve()
            // 把 side-conversations/ 建到当前工作目录(源码树)。
            side_registry: Some(crate::server::tests::test_side_registry(
                index_dir.path(),
                None,
            )),
            theme: test_theme(),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            // 测试注入:只记录调用,不碰真实 launchd / home。
            watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
            watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
            watchman_home: PathBuf::new(),
            // 测试注入:绝不为一次安装真的拉起 detached daemon(那会拿测试
            // 二进制当 daemon、并白等 10s)。
            resolve_plugin: Arc::new(|_name: &str| -> Option<PathBuf> { None }),
            ensure_daemon: Arc::new(|_project: &Path| -> Result<bool, String> { Ok(true) }),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
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

/// 列表扫描必须离开主循环：扫描卡住时，其它 RPC 仍然能被处理。
///
/// 判据是**确定性**的：`ThreadStore::list` 用 `read_to_string` 逐个读
/// `*.meta.json`。把一个这样的文件名做成 FIFO（无写端），那次读取就会永远
/// 阻塞。改造前 `thread/listAll` 在主循环 task 里内联扫描，这条请求会把
/// `select!` 整个占住——紧接着的 `thread/lead`（一次与列表完全无关的请求）
/// 永远读不到响应；用户在那段时间点 Stop 也是同一个结果。改造后扫描在独立
/// 的阻塞任务里，主循环照常派发。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn list_all_scan_does_not_block_the_main_loop() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;

    // 一个有会话状态目录的 workspace，其中放一个「读起来会卡住」的 meta。
    let workspace = dir.path().join("workspace");
    let threads = workspace.join(".yi-agent").join("threads");
    std::fs::create_dir_all(&threads).unwrap();
    let blocker = threads.join("blocker.meta.json");
    let mkfifo = std::process::Command::new("mkfifo")
        .arg(&blocker)
        .status()
        .expect("mkfifo");
    assert!(mkfifo.success(), "mkfifo must succeed");

    let add = serde_json::json!({"jsonrpc":"2.0","id":6,"method":"workspace/add",
        "params":{"path": workspace.to_string_lossy()}});
    h.send(&add.to_string()).await;
    let _ = read_response(&mut h, 6).await;

    // 这条请求会挂住（它的扫描永远等 FIFO）。故意不 await 它。
    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"thread/listAll","params":{}}"#)
        .await;

    // 主循环若被占住，这条请求的响应永远不会到（`read_response` 内部带
    // EVENT_WAIT 超时）。它必须正常应答——这就是「那段时间里点 Stop 也能进来」
    // 的同一条通道。
    let started = std::time::Instant::now();
    let answered = tokio::time::timeout(
        EVENT_WAIT / 2,
        rpc(&mut h, 8, "thread/lead", serde_json::json!({})),
    )
    .await;
    assert!(
        answered.is_ok(),
        "a blocking workspace scan must not starve the main loop (waited {:?})",
        started.elapsed()
    );

    // 放开 FIFO：给写端并立刻关闭，让挂住的读取收到 EOF，扫描才能收尾。
    let _ = std::fs::OpenOptions::new().write(true).open(&blocker);
    let _ = std::fs::remove_file(&blocker);
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

/// 看板卡片会话落进索引后，`workspace/list` 不得把它带进「New thread」菜单。
///
/// 卡片会话的 cwd 是 `<项目>/.worktrees/kanban/<slug>`，`thread/start` 会把它
/// 当普通目录写进索引（`thread/listAll` 靠这条发现冷会话）。但那个目录不是用户
/// 挑的文件夹：菜单里堆十几条 `2026-10-31-xxx-design-...` 只会让用户分不清
/// 哪个才是自己的项目。
#[tokio::test(flavor = "multi_thread")]
async fn workspace_list_hides_card_worktrees() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = git_project(&dir.path().join("proj"));
    let wt = git_kanban_worktree(&project, "card-1");
    // 卡片会话的落盘形态：cwd 在 worktree，归属靠 `driver_project`。
    write_meta(
        &wt,
        "t-card",
        "看板 · card-1",
        Some(&project.to_string_lossy()),
        Some("card-1"),
    );

    let mut h = Harness::new();
    initialize(&mut h).await;
    add_workspace(&mut h, 11, &project.to_string_lossy()).await;
    add_workspace(&mut h, 12, &wt.to_string_lossy()).await;

    let v = rpc(&mut h, 13, "workspace/list", serde_json::json!({})).await;
    let paths: Vec<&str> = v["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["path"].as_str())
        .collect();
    assert!(
        !paths.iter().any(|p| p.contains("/.worktrees/kanban")),
        "the card worktree must stay out of the New-thread menu: {v}"
    );
    assert!(
        paths.contains(&project.to_string_lossy().as_ref()),
        "the real project must still be offered: {v}"
    );

    // 索引本身保留该目录：`thread/listAll` 仍要靠它发现卡片会话，并折进项目组。
    let all = list_all(&mut h).await;
    let card_listed = all["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["threads"].as_array().cloned().unwrap_or_default())
        .any(|t| t["thread_id"] == serde_json::json!("t-card"));
    assert!(
        card_listed,
        "hiding the worktree from the picker must not hide the card session: {all}"
    );
    h.shutdown().await;
}

/// `thread/listAll` 的每个目录分组都带 `groups`：真实成员下发给客户端，
/// 悬空 id（会话已删/不属于本目录）在读取路径上被剔除。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_carries_groups_and_hides_stale_members() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let cwd = dir.path().canonicalize().unwrap();

    // 直接落盘：一个含真实成员 + 一个悬空 id 的组。
    crate::group_store::save(
        &cwd,
        &[crate::group_store::Group {
            id: "grp-a".into(),
            name: "登录模块".into(),
            description: String::new(),
            thread_ids: vec![tid.clone(), "thread-gone".into()],
        }],
    )
    .unwrap();

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let group = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| {
            g["workspace"].as_str().is_some_and(|w| {
                std::path::Path::new(w).canonicalize().ok().as_deref() == Some(cwd.as_path())
            })
        })
        .expect("workspace group present");
    let groups = group["groups"]
        .as_array()
        .expect("groups key present on every workspace group");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "登录模块");
    assert_eq!(groups[0]["threadIds"], serde_json::json!([tid]));
    h.shutdown().await;
}

/// 无分组的目录也带 `groups: []`——客户端据此判断「这个键恒在」，
/// 不必到处写 `?? []`（虽然仍要容忍旧服务端缺键）。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_always_includes_an_empty_groups_array() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let _ = read_thread_start_response(&mut h, 2).await;

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    for group in v["result"]["groups"].as_array().unwrap() {
        assert!(
            group["groups"].is_array(),
            "each workspace group must carry a groups array: {group}"
        );
    }
    h.shutdown().await;
}

/// 分组已是常开能力：建组照常成功，列表照常下发组表。
///
/// （2026-10-29 转正：`session_groups` 开关连同服务端闸门一起删除，所以这里不再
/// 有"关掉时看不见、改不动"的另一半——那条断言随闸门一起去掉了。）
#[tokio::test(flavor = "multi_thread")]
async fn groups_are_listed_and_writable() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap().display().to_string();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "登录模块", "description": "", "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let v = read_response(&mut h, 3).await;
    assert!(v.get("error").is_none(), "create must succeed: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    let ws_group = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["workspace"] == ws.as_str())
        .expect("the workspace must be listed")
        .clone();
    assert_eq!(ws_group["groups"][0]["name"], "登录模块");
    h.shutdown().await;
}

/// 建组：成功后应答带回新组，`thread/listAll` 能读到；跨目录成员、未索引目录、
/// 空组名一律 `-32602`。
#[tokio::test(flavor = "multi_thread")]
async fn group_create_persists_and_validates_workspace() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();
    let ws_text = ws.display().to_string();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws_text, "name": "登录模块", "description": "三条线",
                  "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let v = read_response(&mut h, 3).await;
    assert!(v.get("error").is_none(), "create must succeed: {v}");
    assert_eq!(v["result"]["group"]["name"], "登录模块");
    assert_eq!(v["result"]["group"]["description"], "三条线");
    assert_eq!(v["result"]["group"]["threadIds"], serde_json::json!([tid]));
    let gid = v["result"]["group"]["id"].as_str().unwrap().to_string();
    assert!(gid.starts_with("grp-"), "id must be prefixed: {gid}");

    // 陌生成员：伪造一个不在本目录的 id → invalid_params。
    let stranger = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"thread/group/create",
        "params":{"workspace": ws_text, "name": "x", "threadIds":["thread-from-elsewhere"]}});
    h.send(&stranger.to_string()).await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["error"]["code"], -32602, "跨目录成员必须被拒: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not in this workspace"),
        "{v}"
    );

    // 未索引目录：分组是目录级概念，未知目录一律拒绝。
    let unindexed = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"thread/group/create",
        "params":{"workspace": "/nonexistent-dir-xyz", "name": "x", "threadIds":[tid]}});
    h.send(&unindexed.to_string()).await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["error"]["code"], -32602, "未索引目录必须被拒: {v}");

    let empty_name = serde_json::json!({"jsonrpc":"2.0","id":6,"method":"thread/group/create",
        "params":{"workspace": ws_text, "name": "   ", "threadIds":[tid]}});
    h.send(&empty_name.to_string()).await;
    let v = read_response(&mut h, 6).await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}

/// 更新名/描述、换成员、删除；删到空时组自动消失；未知 groupId → -32602。
#[tokio::test(flavor = "multi_thread")]
async fn group_update_set_members_and_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap().display().to_string();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "a", "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let gid = read_response(&mut h, 3).await["result"]["group"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let update = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"thread/group/update",
        "params":{"workspace": ws, "groupId": gid, "name": "b", "description": "d"}});
    h.send(&update.to_string()).await;
    assert!(read_response(&mut h, 4).await.get("error").is_none());

    // 名字确实落盘了（读 listAll 而不是读应答，避免「应答正确但没写」）。
    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 5).await;
    let boxed: Vec<_> = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["groups"].as_array().cloned().unwrap_or_default())
        .collect();
    assert_eq!(boxed.len(), 1, "{boxed:?}");
    assert_eq!(boxed[0]["name"], "b");
    assert_eq!(boxed[0]["description"], "d");

    // 空成员表 = 解散。
    let clear = serde_json::json!({"jsonrpc":"2.0","id":6,"method":"thread/group/setMembers",
        "params":{"workspace": ws, "groupId": gid, "threadIds":[]}});
    h.send(&clear.to_string()).await;
    assert!(read_response(&mut h, 6).await.get("error").is_none());

    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 7).await;
    let all: Vec<_> = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["groups"].as_array().cloned().unwrap_or_default())
        .collect();
    assert!(all.is_empty(), "空组不下发: {all:?}");

    // 未知 groupId → -32602。
    let unknown = serde_json::json!({"jsonrpc":"2.0","id":8,"method":"thread/group/delete",
        "params":{"workspace": ws, "groupId": "grp-nope"}});
    h.send(&unknown.to_string()).await;
    let v = read_response(&mut h, 8).await;
    assert_eq!(v["error"]["code"], -32602);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown group")
    );
    h.shutdown().await;
}

/// 删会话时成员表跟着清；成员删空后组消失。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_prunes_it_from_groups() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();
    crate::group_store::save(
        &ws,
        &[crate::group_store::Group {
            id: "grp-a".into(),
            name: "a".into(),
            description: String::new(),
            thread_ids: vec![tid.clone()],
        }],
    )
    .unwrap();

    let del = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/delete",
        "params":{"threadId": tid, "force": true}});
    h.send(&del.to_string()).await;
    let _ = read_response(&mut h, 3).await;

    assert!(crate::group_store::load(&ws).is_empty(), "成员删空即解散");
    h.shutdown().await;
}

/// 变更广播只带 workspace，不带分组正文。
#[tokio::test(flavor = "multi_thread")]
async fn group_mutations_broadcast_groups_changed() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap().display().to_string();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "a", "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let _ = read_response(&mut h, 3).await;

    let n = read_notification(&mut h, "thread/groupsChanged").await;
    assert_eq!(n["params"]["workspace"], ws);
    assert!(n["params"].get("groups").is_none(), "只带 workspace: {n}");
    h.shutdown().await;
}

/// 已读水位必须经 `thread/listAll` 下发——这是断线重连后重建未读的唯一来源。
///
/// 若只把水位写在 meta 里而下不到线上，手机重连后永远算不出「离线期间跑完的
/// 回合」，正是用户报的那个不一致。
#[tokio::test(flavor = "multi_thread")]
async fn list_all_reports_the_read_watermarks() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let listed = list_all(&mut h).await;
    assert_eq!(
        watermark_of(&listed, &tid, "last_read_at"),
        0,
        "a fresh thread has never been read: {listed}"
    );
    assert_eq!(
        watermark_of(&listed, &tid, "last_completed_at"),
        0,
        "a fresh thread has never completed a turn: {listed}"
    );

    // 跑完一轮：完成时间推进，已读水位不动 ⇒ 该会话未读。
    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;
    let listed = list_all(&mut h).await;
    let completed = watermark_of(&listed, &tid, "last_completed_at");
    assert!(
        completed > 0,
        "a finished turn must stamp completion: {listed}"
    );
    assert_eq!(
        watermark_of(&listed, &tid, "last_read_at"),
        0,
        "running a turn is not reading it: {listed}"
    );

    // 标记已读：水位追上完成时间，该会话不再未读。
    let resp = rpc(
        &mut h,
        902,
        "thread/markRead",
        serde_json::json!({ "threadId": tid }),
    )
    .await;
    assert!(resp.get("error").is_none(), "markRead must succeed: {resp}");
    let read = resp["result"]["last_read_at"]
        .as_u64()
        .expect("markRead must report the new watermark");
    assert!(
        read >= completed,
        "reading must reach the completion: {resp}"
    );

    let listed = list_all(&mut h).await;
    assert_eq!(
        watermark_of(&listed, &tid, "last_read_at"),
        read,
        "the read watermark must survive the round trip: {listed}"
    );

    h.shutdown().await;
}

/// 标记已读必须落盘并且单调：重复调用不倒退、不报错。
#[tokio::test(flavor = "multi_thread")]
async fn mark_read_persists_and_is_idempotent() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let first = rpc(
        &mut h,
        910,
        "thread/markRead",
        serde_json::json!({ "threadId": tid }),
    )
    .await;
    assert!(first.get("error").is_none(), "first markRead: {first}");
    let a = first["result"]["last_read_at"].as_u64().expect("watermark");

    let second = rpc(
        &mut h,
        911,
        "thread/markRead",
        serde_json::json!({ "threadId": tid }),
    )
    .await;
    assert!(second.get("error").is_none(), "second markRead: {second}");
    let b = second["result"]["last_read_at"]
        .as_u64()
        .expect("watermark");
    assert!(b >= a, "the watermark must never go backwards: {a} -> {b}");

    // 直接读盘：水位必须真的写进 meta，而不只是活在响应里。
    let meta_path = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.meta.json"));
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        on_disk["last_read_at"].as_u64(),
        Some(b),
        "the watermark must be persisted: {on_disk}"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_read_rejects_an_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    let resp = rpc(
        &mut h,
        912,
        "thread/markRead",
        serde_json::json!({ "threadId": "thread-nope" }),
    )
    .await;
    assert_eq!(
        resp["error"]["code"], -32011,
        "an unknown thread must be reported, not silently accepted: {resp}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_read_requires_control_scope() {
    // Observe 是掩码级只读视图：它不得推进共享水位（那会让只读客户端
    // 悄悄改变另一端的未读显示）。
    let mut h = Harness::with_scope(Scope::Observe).await;
    initialize(&mut h).await;
    let resp = rpc(
        &mut h,
        913,
        "thread/markRead",
        serde_json::json!({ "threadId": "thread-any" }),
    )
    .await;
    assert_eq!(
        resp["error"]["code"], -32014,
        "markRead must require Control: {resp}"
    );
    h.shutdown().await;
}

/// 跨端清除蓝点的通道：`markRead` 成功后广播，且属列表层（恒推）。
///
/// 若被归为内容层，远程客户端只为窗口内会话订阅 → 窗口外会话的「已读」
/// 同步不过去，另一端会一直亮着蓝点。
#[tokio::test(flavor = "multi_thread")]
async fn mark_read_broadcasts_a_list_level_read_state_update() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    rpc(
        &mut h,
        920,
        "thread/markRead",
        serde_json::json!({ "threadId": tid }),
    )
    .await;

    let frame = await_notification(&mut h, "thread/readStateChanged", EVENT_WAIT)
        .await
        .expect("markRead must broadcast the new read state");
    assert_eq!(frame["params"]["thread_id"], serde_json::json!(tid));
    assert!(
        frame["params"]["last_read_at"].as_u64().unwrap_or(0) > 0,
        "the broadcast must carry the watermark: {frame}"
    );

    h.shutdown().await;
}

/// 未订阅该会话的客户端也必须收到已读更新（钉住「恒推」这一层）。
///
/// 这是本设计的核心断言之一：远程客户端只为少数会话订阅内容。若把已读更新当
/// 内容层过滤，订阅窗口**外**会话的「已读」就同步不到另一端——那正是用户报的
/// 「手机读过、电脑仍亮」。故这里不只看 `delivery()` 的标签，而是真的经
/// `write_notification` 往一个**只订阅了别的会话**的客户端投一帧。
#[tokio::test(flavor = "multi_thread")]
async fn read_state_reaches_an_unsubscribed_client() {
    use crate::protocol::{Delivery, Notification};

    let notification = Notification::ReadStateChanged {
        thread_id: "t2".into(),
        last_read_at: 5,
        last_completed_at: 1,
    };
    assert_eq!(
        notification.delivery(),
        Delivery::List,
        "read state is a list-level fact: filtering it by subscription would \
         leave the other end's dot on forever"
    );

    let hub = crate::broadcast::Broadcaster::new();
    let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(7));
    let mut rx = hub.register(sub.clone());
    // 只订阅 t1：t2 的已读更新必须照样到达。
    hub.subscribe(&sub, vec!["t1".to_string()]);

    write_notification(&hub, &notification).await.unwrap();
    let frame = rx
        .try_recv()
        .expect("an unsubscribed thread's read state must arrive");
    assert_eq!(frame["method"], "thread/readStateChanged");
    assert_eq!(frame["params"]["thread_id"], "t2");
    assert_eq!(frame["params"]["last_completed_at"], 1);
}

/// 新 RPC 必须归到 thread 域，否则会被静默映射成 `method_not_found`。
#[tokio::test(flavor = "multi_thread")]
async fn mark_read_routes_to_the_thread_domain() {
    assert_eq!(domain_for("thread/markRead"), Domain::Thread);
}

/// `thread/delete` 必须把该会话的视图帧一并收走。
///
/// 帧表以 `thread_id` 为键、进程生命周期内常驻，且全仓库只有插没有删。
/// `thread/view/read` 有 `require_known_thread` 前挡，故残留不影响正确性,
/// 但每删一个看过 diff 的会话就永久留一份(单条上限 256 KiB)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_delete_drops_the_view_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    {
        // 直接注入一帧，等价于 `pump_git_diff_notifications` 写帧。
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: Some("git log --oneline".to_string()),
                as_kind: "text".to_string(),
                exit_code: Some(0),
                stdout: "x".repeat(1024),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: None,
                base_kind: String::new(),
            },
        );
    }
    assert!(
        h.view_frames.lock().await.contains_key(&tid),
        "precondition: the frame must be present before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":30,"method":"thread/delete","params":{{"threadId":"{tid}","force":true}}}}"#
    ))
    .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(30)))
        .await;

    assert!(
        !h.view_frames.lock().await.contains_key(&tid),
        "thread/delete must drop the view frame for {tid}"
    );
    h.shutdown().await;
}

/// `thread/diff/read` 不得把主循环占住。
///
/// 该臂在 async handler 里同步跑 git(5+ 次子进程)并全量读所有未跟踪文件。
/// 判据:一条需要扫大量未跟踪文件的 diff 读取之后,紧随其后的 `thread/list`
/// 必须很快应答——它证明主循环已经回到 `select!`。
#[tokio::test(flavor = "multi_thread")]
async fn diff_read_does_not_block_the_main_loop() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一个没写进 .gitignore 的产物目录：60 个 8 MiB 未跟踪文件。
    let junk = dir.path().join("dist");
    std::fs::create_dir_all(&junk).unwrap();
    let chunk = "y".repeat(1024 * 1024);
    for n in 0..60 {
        std::fs::write(junk.join(format!("chunk{n}.log")), &chunk).unwrap();
    }

    // 先量基线：空载下一条 thread/list 的往返。
    let t0 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":70,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(70)))
        .await;
    let baseline = t0.elapsed();

    // 发一条 diff 读取，紧接着发 list：后者必须不被前者挡住。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":71,"method":"thread/diff/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let t1 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":72,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(72)))
        .await;
    let blocked = t1.elapsed();

    // 相对判据：同一进程同一磁盘上，被挡住时是 1000 倍量级（实测 1101x）。
    // 用 50 倍留足 CI 抖动余量，同时远小于阻塞量级。
    assert!(
        blocked < baseline * 50 + std::time::Duration::from_millis(100),
        "a diff read must not block the main loop: \
         baseline={baseline:?} list_behind_diff={blocked:?}"
    );
    // 两条响应都得回来，确认 diff 本身也没被弄坏。
    let v = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(71)))
        .await;
    assert!(v["result"].is_object(), "diff/read must still answer: {v}");
    h.shutdown().await;
}

/// `thread/diff/read` 的 panic 兜底必须按**调用方期望的形状**返回。
///
/// 整线程请求（只带 `threadId`，`useDiffState` 的 `loadThreadDiff` 发的就是
/// 这个）期望 `ThreadDiffResult`；带 `commit` / `path` 的单文件请求期望
/// `DiffTextResult`。两者字段不同，而前端 `setThreadDiff` 不做形状检查、
/// `GitDiffView` 直接解引用 `files.length`——整线程请求若收到单文件形状，
/// 面板必然崩。故钉死这两条兜底形状。
#[test]
fn diff_read_panic_fallback_matches_the_callers_shape() {
    // 单文件请求：原样保留 DiffTextResult 形状。
    let text =
        crate::rpc::thread::content::diff_read::diff_read_fallback(true, Some("origin/main"));
    assert_eq!(
        text,
        serde_json::json!({ "unifiedDiff": "", "truncated": false }),
        "commit/path requests expect DiffTextResult and nothing else"
    );

    // 整线程请求：必须是字段齐全的空 ThreadDiffResult。
    let whole =
        crate::rpc::thread::content::diff_read::diff_read_fallback(false, Some("origin/main"));
    assert_eq!(
        whole,
        serde_json::json!({
            "base": "origin/main",
            "baseKind": "none",
            "mergeBase": serde_json::Value::Null,
            "commits": [],
            "files": [],
            "unifiedDiff": "",
            "truncated": false,
        }),
        "whole-thread requests expect a well-formed ThreadDiffResult"
    );

    // 无 `base` 时必须显式给 `null`，而不是把这个字段整个省掉。
    let whole_no_base = crate::rpc::thread::content::diff_read::diff_read_fallback(false, None);
    assert_eq!(whole_no_base["base"], serde_json::Value::Null);
}

/// `thread/view/read?refresh=true` 同样不得占住主循环。
///
/// 它的 `structured_frame` 分支同步跑 thread_diff(5+ 次 git);命令帧分支虽然
/// 是 async,但 `command_frame` 开头同步构造 SandboxPolicy,后者调
/// `Path::canonicalize`(在已卸载的挂载点上会卡住),也不经过任何 await。
#[tokio::test(flavor = "multi_thread")]
async fn view_read_refresh_does_not_block_the_main_loop() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let junk = dir.path().join("dist");
    std::fs::create_dir_all(&junk).unwrap();
    let chunk = "y".repeat(1024 * 1024);
    for n in 0..60 {
        std::fs::write(junk.join(format!("chunk{n}.log")), &chunk).unwrap();
    }
    {
        // 结构化帧(无 command)：refresh 会走 structured_frame -> thread_diff。
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: None,
                as_kind: "diff".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: None,
                base_kind: String::new(),
            },
        );
    }

    let t0 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":90,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(90)))
        .await;
    let baseline = t0.elapsed();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":91,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let t1 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":92,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(92)))
        .await;
    let blocked = t1.elapsed();

    assert!(
        blocked < baseline * 50 + std::time::Duration::from_millis(100),
        "a view refresh must not block the main loop: \
         baseline={baseline:?} list_behind_refresh={blocked:?}"
    );
    let v = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(91)))
        .await;
    assert!(v["result"].is_object(), "view/read must still answer: {v}");
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// Task 2: 跨进程会话写锁（app-server 侧）
// ---------------------------------------------------------------------------

/// 桌面（app-server）与终端（另一个 writer）不能同时成为同一 thread 的 writer。
///
/// 关键反例：若把锁实现成「app-server 一旦打开 thread 就长期持锁」，
/// 桌面挂着某会话时终端永远 resume 不了。锁必须只在启动 driver 时取。
#[tokio::test(flavor = "multi_thread")]
async fn writer_lock_refuses_a_second_writer_on_the_same_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg.clone(), build_checkpoint_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    // 直接在盘上造一个会话，**不**经 `thread/start`。
    //
    // 走 `thread/start` 的话 app-server 自己会先持锁，后续子进程根本抢不到，
    // 测不出「跨进程拒绝」这件事。这里模拟的是真实场景：终端先跑了这个会话
    // 并落盘，桌面随后才打开它。
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    let tid = "thread-lock-conflict".to_string();
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: tid.clone(),
            cwd: cfg.workdir.display().to_string(),
            model: "claude-sonnet-4-5".to_string(),
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
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
        })
        .unwrap();

    // 模拟另一个 writer（终端）已持有该 thread 的锁。
    //
    // **必须是真子进程**：`flock` 按「打开文件描述」加锁，同一进程内对同一
    // 路径再 open 一次拿到的描述符会与已持有的锁冲突——但本进程的去重表会先
    // 命中，根本走不到 flock。所以这里起一个子进程，让内核的跨进程语义生效。
    let lock_path =
        crate::thread_store::thread_lock_path(&cfg.workdir.join(".yi-agent/threads"), &tid);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

    let exe = std::env::current_exe().unwrap();
    let mut holder = std::process::Command::new(exe)
        .args([
            "--exact",
            "server::tests::child_holds_thread_lock",
            "--nocapture",
            "--ignored",
        ])
        .env("YI_LOCK_PATH", &lock_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // 等子进程报告已持锁，避免与下面的 resume 竞速。
    //
    // 用逐行读到 `LOCK_HELD` 为止：libtest 会先打 `running 1 test`，
    // 单次 read 很可能只拿到那行。
    {
        use std::io::{BufRead, BufReader};
        let stdout = holder.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut seen = String::new();
        let mut got = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            seen.push_str(&line);
            if line.contains("LOCK_HELD") {
                got = true;
                break;
            }
        }
        assert!(got, "子进程必须先成功持锁，实际: {seen}");
    }

    // 桌面尝试 resume（= 启动该 thread 的 driver）：必须被拒
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 3).await;
    assert!(
        v.get("error").is_some(),
        "第二个 writer 必须被拒绝，实际: {v}"
    );
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("正被另一个") || message.contains("busy"),
        "错误信息要能指导用户，实际: {message}"
    );

    // 释放占位锁后必须能正常 resume（否则就是锁没释放的死锁 bug）
    holder.kill().ok();
    holder.wait().ok();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let ok = read_response(&mut h, 4).await;
    assert!(
        ok.get("result").is_some(),
        "锁释放后必须能 resume，实际: {ok}"
    );
    h.shutdown().await;
}

/// 同一 app-server 内的多客户端订阅不受 writer lock 影响（桌面 + 手机）。
#[tokio::test(flavor = "multi_thread")]
async fn writer_lock_does_not_block_subscriptions() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 订阅不该被 writer lock 挡住
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "thread/subscribe",
            "params": { "threadIds": [tid] }
        })
        .to_string(),
    )
    .await;
    let v = read_response(&mut h, 3).await;
    assert!(
        v.get("error").is_none(),
        "订阅不该受 writer lock 影响，实际: {v}"
    );
    h.shutdown().await;
}
