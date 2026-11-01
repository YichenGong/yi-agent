use super::*;

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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
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
        None,
        &std::env::temp_dir(),
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
        None,
        &std::env::temp_dir(),
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
        None,
        &std::env::temp_dir(),
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
        0,
        None,
        &[],
        &[],
        None,
        &agent,
        Vec::new(),
        None,
        None,
        &store,
        &hub,
        &turn_tx,
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
        0,
        None,
        &[],
        &[],
        None,
        &agent,
        Vec::new(),
        None,
        None,
        &store,
        &hub,
        &turn_tx,
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
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
    let mut h = Harness::with_config_and_lead(
        cfg,
        gated_lead_factory(Arc::clone(&provider)),
        PERMISSION_TIMEOUT,
        true,
    )
    .await;
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
                usage_total: None,
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
                usage_total: None,
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
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
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
                usage_total: None,
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
                usage_total: None,
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
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

/// 打回到 idle 目标：起一轮，目标转录里出现打回文本。
///
/// 扮演组长：直接往**与主循环共享**的写通道投递一条 `Reject`，绕过组长工具本身，
/// 单测主循环的执行路径（工具侧已由 `lead_control` 的单测覆盖）。
#[tokio::test(flavor = "multi_thread")]
async fn a_reject_to_an_idle_thread_starts_a_turn_that_carries_the_reason() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let target = start_thread(&mut h).await;

    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-1".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(
        h.lead_control.send(crate::lead_control::LeadControlCmd {
            project_key,
            kind: crate::lead_control::LeadControlKind::Reject,
            thread_id: target.clone(),
            reason: "redo with tests".to_string(),
            request_id,
        }),
        "the main loop must be subscribed"
    );

    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("the replies table hands back an outcome");
    assert!(
        outcome.is_ok(),
        "a reject to a live thread must succeed: {outcome:?}"
    );

    // 目标转录里出现打回文本（经注入落盘为新轮的用户条目）。注入是**异步**的：
    // `apply_lead_control` 只把 prompt 投给 driver 就返回了，用户条目落盘在目标那轮
    // 真正开始时才发生。故这里按 `EVENT_WAIT` 有界轮询，而不是读一次就断言（读一次
    // 会与落盘赛跑，稳定地读到空 items）。
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let mut landed = false;
    while tokio::time::Instant::now() < deadline {
        let read = rpc(
            &mut h,
            2,
            "thread/readItems",
            serde_json::json!({ "threadId": target }),
        )
        .await;
        if read.to_string().contains("redo with tests") {
            landed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        landed,
        "the rejection must land in the target transcript within {EVENT_WAIT:?}"
    );

    h.shutdown().await;
}

/// 回归（Stop 点不动）：对**没有活跃轮**的 thread 发 `turn/interrupt` 时，
/// 响应必须如实告知「已经空闲」，而不是回一个空的 `ok {}`。
///
/// 前端 `turnActive` 只由 `turn/started` / `turn/completed` 翻转，没有任何对账
/// 兜底。一旦 `turn/completed` 在出站队列被灌满时丢掉（见
/// `a_critical_notification_survives_a_full_outbound_queue`），前端会永远显示
/// Stop；此时点 Stop 会发来这条请求，而服务端因为 `active_turn_id` 已经是
/// `None` 而什么都不做——旧实现仍然回 `ok {}`，前端无从知道自己该把状态改回
/// Idle，按钮就此永久卡在 Stop。返回 `alreadyIdle: true` 让客户端能就地纠正。
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_on_an_idle_thread_reports_that_it_was_already_idle() {
    let (mut h, _dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let tid = start_thread(&mut h).await;

    // 这一轮从未启动过：thread 处于 Idle，没有 active_turn_id。
    let resp = rpc(
        &mut h,
        2,
        "turn/interrupt",
        serde_json::json!({ "threadId": tid }),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "interrupting an idle thread is not an error: {resp}"
    );
    assert_eq!(
        resp["result"]["alreadyIdle"],
        serde_json::json!(true),
        "the client must learn that nothing was running, so it can drop a stale Stop: {resp}"
    );
    h.shutdown().await;
}

/// 打断无活跃轮的目标：回错（不静默 no-op）。
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_without_an_active_turn_is_an_error() {
    let (mut h, dir) = isolated_harness().await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    let target = start_thread(&mut h).await;
    let project_key = crate::thread_store::ThreadStore::new(dir.path()).project_key();
    let request_id = "ctl-3".to_string();
    let rx = h.lead_control_replies.register(request_id.clone());
    assert!(h.lead_control.send(crate::lead_control::LeadControlCmd {
        project_key,
        kind: crate::lead_control::LeadControlKind::Interrupt,
        thread_id: target,
        reason: "stop".to_string(),
        request_id,
    }));
    let outcome = tokio::time::timeout(EVENT_WAIT, rx)
        .await
        .expect("the main loop must answer")
        .expect("an outcome");
    assert!(
        outcome.is_err(),
        "no active turn must be an error, not a silent no-op"
    );
    h.shutdown().await;
}

/// 回合结束记 `last_completed_at`，且不动 `updated_at` 的既有语义。
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_turn_records_its_completion_time() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    run_a_turn_and_wait_for_persist(&mut h, dir.path(), &tid, "hello").await;

    let meta_path = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.meta.json"));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert!(
        meta["last_completed_at"].as_u64().unwrap_or(0) > 0,
        "a finished turn must stamp last_completed_at: {meta}"
    );

    h.shutdown().await;
}

/// `clear` / `compact` 不是回合结束：不得记完成时间。
///
/// 若记了，刚清空上下文的会话会凭空亮起蓝点——另一端看到「有东西没读」，
/// 点进去却什么都没有。
#[tokio::test(flavor = "multi_thread")]
async fn clear_does_not_record_a_completion() {
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

    let meta_path = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.meta.json"));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        meta["last_completed_at"].as_u64(),
        Some(0),
        "clearing is not a completion: {meta}"
    );

    h.shutdown().await;
}

/// `turn/completed` 必须自带完成时间。
///
/// 广播发生在落盘之前，客户端收到后立刻重拉列表会读到旧的 `last_completed_at`。
/// 通知自带时间戳让客户端按 max 合并，那次竞态自然消失。
#[tokio::test(flavor = "multi_thread")]
async fn turn_completed_carries_its_completion_time() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 930, "method": "turn/start",
            "params": { "threadId": tid, "input": [{ "type": "text", "text": "hi" }] },
        })
        .to_string(),
    )
    .await;

    let frame = await_notification(&mut h, "turn/completed", EVENT_WAIT)
        .await
        .expect("the turn must report completion");
    assert!(
        frame["params"]["completed_at"].as_u64().unwrap_or(0) > 0,
        "turn/completed must carry completed_at so the client can merge it \
         without racing the persist: {frame}"
    );

    h.shutdown().await;
}

/// 广播的完成时刻必须与落盘的是**同一个值**（而不是「广播一个、随后落盘另一个」）。
///
/// 这是一条容易漏掉、且只会在跨端场景暴露的顺序约束：`turn/completed` 在流循环
/// 里就广播出去了，落盘发生在循环**之后**。若两处各取一次 `now_millis()`，落盘的
/// 那个必然更晚。于是另一端收到通知立刻发 `thread/markRead` 时，写下的已读水位
/// 会**低于**稍后才落盘的完成水位 ⇒ 服务端判为「读完之后又跑完了一轮」⇒ 重连
/// 后蓝点复活。用户看到的现象就是「我明明在手机上读过了，电脑上又亮了」。
///
/// 断言取 `<=` 而非 `==`：要紧的是落盘值**不晚于**广播值（已读才能盖住它）。
#[tokio::test(flavor = "multi_thread")]
async fn the_persisted_completion_is_not_later_than_the_broadcast_one() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 933, "method": "turn/start",
            "params": { "threadId": tid, "input": [{ "type": "text", "text": "hi" }] },
        })
        .to_string(),
    )
    .await;

    let frame = await_notification(&mut h, "turn/completed", EVENT_WAIT)
        .await
        .expect("the turn must report completion");
    let broadcast_at = frame["params"]["completed_at"]
        .as_u64()
        .expect("turn/completed must carry completed_at");

    // 等到落盘真的发生（meta 里的完成水位出现），再比这一刻的两个值。
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let mut persisted_at = 0u64;
    while tokio::time::Instant::now() < deadline {
        let path = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.meta.json"));
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&raw) {
                persisted_at = meta["last_completed_at"].as_u64().unwrap_or(0);
                if persisted_at > 0 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(persisted_at > 0, "the turn's completion must reach disk");

    assert!(
        persisted_at <= broadcast_at,
        "the persisted completion ({persisted_at}) must not be later than the \
         broadcast one ({broadcast_at}): a client that marks the thread read on \
         receiving turn/completed would otherwise write a read watermark that the \
         server then considers older than the completion, and the dot comes back \
         after reconnect"
    );

    h.shutdown().await;
}

/// 落盘失败（append 写不进去）**不得**连带丢掉「这一轮跑完了」这件事。
///
/// 两条失败是独立的：日志写失败只代表内容没存下来，回合本身确确实实结束了，用户
/// 也在流里看到了输出。若把完成时刻的记账绑在 append 成功分支上，这一轮在未读
/// 口径里就凭空消失——另一端重连后看不到任何「有新内容」的提示，直到下一轮跑完。
///
/// 失败注入：把 `<thread>.jsonl` 换成一个**目录**，`OpenOptions::append` 必然失败。
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_append_still_records_the_completion() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let log = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    std::fs::create_dir_all(&log).expect("sabotage: the log path becomes a directory");

    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 934, "method": "turn/start",
            "params": { "threadId": tid, "input": [{ "type": "text", "text": "hi" }] },
        })
        .to_string(),
    )
    .await;
    let frame = await_notification(&mut h, "turn/completed", EVENT_WAIT)
        .await
        .expect("the turn itself must still complete");
    let broadcast_at = frame["params"]["completed_at"].as_u64().unwrap_or(0);
    assert!(broadcast_at > 0);

    let meta_path = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.meta.json"));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        meta["last_completed_at"].as_u64().unwrap_or(0),
        broadcast_at,
        "a failed append must not swallow the completion stamp: {meta}"
    );

    h.shutdown().await;
}
