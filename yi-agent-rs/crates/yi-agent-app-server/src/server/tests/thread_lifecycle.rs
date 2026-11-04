use super::*;

/// 新建会话时，`show_git_diff` 的默认基准必须落在**本会话的起点**上。
///
/// 只有走真实 `thread/start` 才验得出：起点在 `start_thread_core` 里取（那时
/// meta 尚未落盘），若 `attach_delegation` 反过来去读盘拿锚点，读到的永远是
/// `None`——工具默认基准就落到默认分支候选链（`local-default`），与它自己
/// `description()` 的承诺不符，也和 refresh 路径（用真锚点）分叉。断言
/// `baseKind == "session"` 正是钉住「先取起点、再交给工具」这个次序。
#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_anchors_the_default_diff_to_its_starting_commit() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    // 一次提交之后再改文件：会话起点应是那次提交，而不是「仅工作区」。
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
    let head = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
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
    let mut h = Harness::with_config(cfg, build_git_diff_tool_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"show me"}}]}}}}"#
    ))
    .await;
    // 等这一轮收尾：工具跑完，帧已由 watcher 落表。帧的落表是另一条任务，
    // 故按 id 收敛 + 轮询，不假设「一旦 turn/completed 就一定已落表」。
    loop {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let frame = loop {
        if let Some(frame) = h.view_frames.lock().await.get(&tid).cloned() {
            break frame;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tool must have filed a frame for this session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        frame.base_kind, "session",
        "a fresh session's default base must be its starting commit, not a candidate branch: {frame:?}"
    );
    assert_eq!(
        frame.base.as_deref(),
        Some(head.as_str()),
        "the base must be the sha captured when the session started: {frame:?}"
    );
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
            subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
            writer_lock: None,
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
            mcp: None,
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
                usage_total: None,
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
        if let Ok(t) = std::fs::read_to_string(&log)
            && !t.trim().is_empty()
        {
            persisted = true;
            break;
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
            mcp: None,
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

/// upsert 新建条目后自动探测，不阻塞 upsert 的响应。
#[tokio::test(flavor = "multi_thread")]
async fn an_upsert_with_a_new_connection_starts_a_probe() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
        .mount(&server)
        .await;

    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"model/upsert","params":{{"name":"A","provider":"openai","api_url":"{}","model":"m","api_key":"k","api_style":"chat"}}}}"#,
        server.uri()
    ))
    .await;
    assert_eq!(read_response(&mut h, 9).await["result"]["ok"], true);

    let n = read_notification(&mut h, "model/capabilitiesChanged").await;
    assert_eq!(n["params"]["name"], "A");
    assert!(
        n["params"]["reasoning_capabilities"]["chat"]["supported"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "upsert must trigger a probe and broadcast its result: {n}"
    );
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

/// `thread/readItems` 返回已落盘的 items；`afterItemId` 只返回其后的部分。
#[tokio::test(flavor = "multi_thread")]
async fn read_items_returns_persisted_items_and_slices_after_id() {
    // 要断言 `usage` 非空，provider 必须真的报一次用量（见 `UsageProvider`）。
    let mut h = Harness::with_factory(build_usage_agent, PERMISSION_TIMEOUT);
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
    // 同一份 `load` 已经读出了最后一轮用量：不回传等于把已付出的读盘丢掉，
    // 而「运行中的会话只走只读补齐、绝不 resume」那条路径上，这是唯一的数据来源。
    let usage = v["result"]["usage"]
        .as_object()
        .unwrap_or_else(|| panic!("readItems 必须回传持久化用量: {v}"));
    assert!(
        usage["input_tokens"].as_u64().unwrap_or(0) > 0,
        "回传的用量应来自真实跑过的一轮: {v}"
    );
    assert!(
        usage["model"].as_str().is_some_and(|m| !m.is_empty()),
        "用量必须带模型名: {v}"
    );
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
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        // 组长开关：这些用例钉的是「组长之外的判定」（卡片会话、worktree、
        // 谁是谁的组长），开关必须开着它们才测得到目标逻辑。
        Some(true),
        Vec::new(),
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
        mcp: None,
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
                None,
                &std::env::temp_dir(),
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
                None,
                &std::env::temp_dir(),
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

/// 打回到不在场的冷会话：回错，不起轮。
#[tokio::test(flavor = "multi_thread")]
async fn a_reject_to_an_unknown_thread_is_an_error() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-2".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(h.lead_control.send(crate::lead_control::LeadControlCmd {
        project_key,
        kind: crate::lead_control::LeadControlKind::Reject,
        thread_id: "thread-does-not-exist".to_string(),
        reason: "x".to_string(),
        request_id,
    }));
    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("an outcome");
    assert!(outcome.is_err(), "an unreachable target must be an error");
    h.shutdown().await;
}

/// `thread/fork` 的最小闭环：返回新 id、落盘能看到、源会话字节不变。
#[tokio::test]
async fn thread_fork_copies_a_settled_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    write_meta(&cwd, "t-fork-src", "源会话", None, None);

    // workdir 必须与 `write_meta` 的目录一致，否则 `store_lookup` 找不到它。
    let mut cfg = test_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"thread/fork","params":{"threadId":"t-fork-src"}}"#)
        .await;
    let v = read_response(&mut h, 7).await;
    assert!(v.get("error").is_none(), "fork 应当成功: {v}");
    let new_id = v["result"]["thread_id"].as_str().unwrap().to_string();
    assert_ne!(new_id, "t-fork-src");
    assert_eq!(
        v["result"]["title"].as_str(),
        Some("源会话 (fork)"),
        "回包要带上落盘的标题，前端才能立刻显示"
    );

    let store = crate::thread_store::ThreadStore::new(&cwd);
    let loaded = store.load(&new_id).unwrap().expect("新会话必须落盘");
    assert_eq!(loaded.meta.forked_from.as_deref(), Some("t-fork-src"));
    assert!(
        store.load("t-fork-src").unwrap().is_some(),
        "源会话必须还在"
    );
    h.shutdown().await;
}

/// 未知 id 要说是「不认识」，不能悄悄建一个空会话出来。
#[tokio::test]
async fn thread_fork_rejects_an_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":8,"method":"thread/fork","params":{"threadId":"t-nope"}}"#)
        .await;
    let v = read_response(&mut h, 8).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32011), "{v}");
    h.shutdown().await;
}

/// 运行中的会话一律拒绝：复制到半轮会产生一个「开头有提问、结尾是断掉的
/// tool_use」的坏副本。拒绝必须发生在**任何磁盘写入之前**。
#[tokio::test(flavor = "multi_thread")]
async fn thread_fork_refuses_a_running_thread_and_writes_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 起一轮不会立刻结束的 turn。`CheckpointProvider` 第一次调用给 tool_use、
    // 第二次起 `pending()` 永不收尾，故这一轮不会自己结束。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"do work"}}]}}}}"#
    ))
    .await;
    let _ = read_response(&mut h, 10).await;

    // 只数 `.meta.json`：运行中的 turn 会往同目录写 `<tid>.partial.json`
    // （turn 开始写一次、之后每 500ms 去抖刷新），数全部条目会与其竞态而偶发红。
    let count_metas = || {
        std::fs::read_dir(dir.path().join(".yi-agent/threads"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
                    .count()
            })
            .unwrap_or(0)
    };
    let before = count_metas();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"thread/fork","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 11).await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(-32012),
        "运行中的会话必须被拒绝: {v}"
    );

    assert_eq!(count_metas(), before, "被拒绝的 fork 不该建出任何新会话");
    h.shutdown().await;
}

/// 源目录被删掉后不该 fork 出一个打不开的会话。
///
/// 会话必须先在内存里（`thread/start` 时给了 cwd），`thread_cwd` 才会返回那条
/// 路径并走到 `!cwd.is_dir()` 分支。若走冷会话路径，`store_lookup` 定位不到
/// 目录就回落到 `cfg.workdir`，最终报的是 `-32011`（未知会话）——那测的就不是
/// 这条分支了。
#[tokio::test]
async fn thread_fork_refuses_when_the_working_directory_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let gone = dir.path().join("vanished");
    std::fs::create_dir_all(&gone).unwrap();
    let gone_str = gone.to_string_lossy().to_string();

    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "thread/start",
            "params": { "cwd": gone_str }
        })
        .to_string(),
    )
    .await;
    let started = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(9)))
        .await;
    let tid = started["result"]["thread_id"].as_str().unwrap().to_string();

    // 起会话之后目录才消失——这正是「项目目录被删/改名」的真机情形。
    std::fs::remove_dir_all(&gone).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"thread/fork","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 12).await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(-32602),
        "目录已消失必须明确报错，而不是落盘一个打不开的会话: {v}"
    );
    // 关键判据：**没有多出任何会话**。若 cwd 检查缺失，`fork_thread` 的
    // `create` 会 `create_dir_all(&self.root)` 把整棵 `<gone>/.yi-agent/threads`
    // 复活并写进一个新会话。
    //
    // 不断言 `!gone.exists()`：`thread/start` 的标题生成是异步的，可能在删除之后
    // 才回写 meta，把目录重新建出来——那是无关写入，不该让这条测试偶发变红。
    // `ThreadStore::new` 接受的是**工作区根**（它自己拼 `.yi-agent/threads`）。
    let store = crate::thread_store::ThreadStore::new(&gone);
    let ids: Vec<String> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.thread_id)
        .collect();
    assert!(
        ids.is_empty(),
        "被拒绝的 fork 不该在已删除的目录里留下会话: {ids:?}"
    );
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 看板加载观测:普通 RPC `thread/resume` 恢复的会话必须被热发现
// ---------------------------------------------------------------------------

/// 走**普通** `thread/resume`（不是插件的 `resume_append`）把一个冷会话拉回热表，
/// 看板必须从「未观测到 / 未加载」变成「已加载」，且不额外花钱。
///
/// 这是整条链路唯一不能靠插件自测证明的一环：`loaded_threads` 必须由 serve 主循环
/// 从**真实** `threads` 表采样发出。插件自己的测试只能证明「给了快照我就会用」。
///
/// 断言用真实请求与真实 `list`：
/// - 首个 tick 之后是 `not_loaded`（热表确实空，看板的卡预置在 running）；
/// - resume 之后 `loaded`，`thread_id` / `state` 一个不动；
/// - 计数 mock Provider 证明恢复与 tick **没有**多跑一次模型回合——观测绝不能
///   为了清角标而发 turn。
#[tokio::test(flavor = "multi_thread")]
async fn kanban_observation_plain_rpc_resume_is_discovered() {
    use std::sync::atomic::AtomicUsize;

    let project_dir = tempfile::TempDir::new().unwrap();
    let project = project_dir.path().canonicalize().unwrap();
    init_git_repo(&project);
    let thread_id = "thread-observation-cold";

    // 开发工作目录先建出来：卡的 workdir 与冷会话的 cwd 都用它（见下文隔离说明）。
    let workdir = project.join("dev-tree");
    std::fs::create_dir_all(&workdir).unwrap();

    // 一张预置的 running 卡，执行会话就是这个 ID（fixture 数据，不是生产卡）。
    let state_dir = project.join(".yi-agent/superpowers-kanban");
    std::fs::create_dir_all(&state_dir).unwrap();
    let card_id = superpowers_kanban_core::card::CardId::new("card-obs");
    let mut board = superpowers_kanban_core::board::Board::new();
    board.enqueue(
        card_id.clone(),
        "a.spec.md".into(),
        "a.plan.md".into(),
        chrono::Local::now(),
    );
    board
        .transition(&card_id, superpowers_kanban_core::card::CardState::Running)
        .unwrap();
    board
        .set_thread_id(&card_id, thread_id.to_string())
        .unwrap();
    board.set_workdir(&card_id, workdir.clone()).unwrap();
    superpowers_kanban_runner::persist::save_board(&state_dir.join("board.json"), &board).unwrap();

    // 一个**真的**存在于磁盘、但不在热表里的会话（合法空日志）。
    //
    // cwd 刻意指到 `dev-tree` 而不是项目根：`thread/resume` 会给会话所属目录 attach
    // 一个**真** runtime（`<cwd>/.yi-agent/runtime`），而假 daemon 正听着项目根的那个
    // 路径。两者撞上时真 daemon 启动会按路径 unlink 掉假 daemon 的 socket 节点，
    // 假 daemon 就再也收不到请求、也醒不过来。把两者分开放，才是真实隔离。
    let store = crate::thread_store::ThreadStore::new(&project);
    store
        .create(&crate::thread_store::ThreadMeta {
            thread_id: thread_id.to_string(),
            cwd: workdir.to_string_lossy().to_string(),
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            extra_roots: Vec::new(),
            role: None,
        })
        .unwrap();

    let home = tempfile::TempDir::new().unwrap();
    let service = Arc::new(superpowers_kanban_runner::service::BoardService::new(
        state_dir.clone(),
        project.clone(),
        Some(home.path().to_path_buf()),
    ));
    // 假 transport、真判定：驱动插件的每个请求原样交给真实 dispatch。
    let daemon = super::super::session_driver_tests::FakeDaemon::bind_at(&project, {
        let service = Arc::clone(&service);
        let state_dir = state_dir.clone();
        move |_project, method, params| {
            superpowers_kanban_runner::dispatch::dispatch_with_service(
                &service, &state_dir, None, method, params,
            )
        }
    });

    // 计数 provider：只记**真正**发起的模型调用，用来证明观测不发 turn。
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl yi_agent_core::Provider for CountingProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(futures::stream::iter(Vec::new()).boxed())
        }
    }
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CountingProvider {
        calls: Arc::clone(&counted),
    });

    let mut cfg = test_config();
    cfg.workdir = project.clone();
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(Arc::clone(&provider), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    super::super::session_driver_tests::install_driver_plugin(
        &project,
        h.resident_dir.path(),
        "superpowers-kanban",
    );
    initialize(&mut h).await;
    let calls_before = calls.load(Ordering::SeqCst);

    // 首个 tick 之后：热表是空的，所以是「明确未加载」。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let listed = service.list();
        assert!(
            tokio::time::Instant::now() < deadline,
            "the driver tick never reported the held card: {listed}"
        );
        if listed["cards"][0]["session_observation"] == "not_loaded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 真实普通入口：不是插件的 `resume_append`。
    let resumed = rpc(
        &mut h,
        2,
        "thread/resume",
        serde_json::json!({ "threadId": thread_id }),
    )
    .await;
    assert!(resumed.get("error").is_none(), "{resumed}");

    // 下一次 tick 就能在完整快照里看到它。
    loop {
        let listed = service.list();
        if listed["cards"][0]["session_observation"] == "loaded" {
            assert_eq!(listed["cards"][0]["thread_id"], thread_id);
            assert_eq!(listed["cards"][0]["state"], "running");
            assert_eq!(listed["cards"][0]["stale"], false);
            assert_eq!(
                listed["cards"][0]["workdir"],
                serde_json::Value::String(workdir.to_string_lossy().into_owned()),
                "观测不得改写工作目录"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed session was never observed: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 观测不花钱：恢复本身不会起 turn，tick 也不会。
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "observation must not spend a model turn"
    );

    // 请求里确实带了完整快照，且没伪造 Launch/ResumeAppend 结果。
    let ticks = daemon.calls_to("driver/next");
    assert!(!ticks.is_empty(), "the tick must have asked the plugin");
    assert!(
        ticks.iter().any(
            |call| call["params"]["loaded_threads"]
                .as_array()
                .is_some_and(|loaded| loaded
                    .iter()
                    .any(|fact| fact["thread_id"] == serde_json::Value::String(thread_id.into())))
        ),
        "the serve loop must report the resumed session in loaded_threads"
    );
    assert!(
        ticks.iter().all(|call| call["params"]["results"]
            .as_array()
            .is_some_and(|results| results.is_empty())),
        "a plain resume is not a driver action: no fabricated results"
    );

    // 先收 daemon 再收 server:`shutdown` 会拆掉项目 runtime 目录连带删掉 socket,
    // 之后 `FakeDaemon::drop` 的「连接一下把自己唤醒」就落到不存在的路径上,
    // `join()` 会永远等一个还在 `accept()` 的线程。
    drop(daemon);
    h.shutdown().await;
}
