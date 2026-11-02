use super::*;

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
        if let Ok(Some(loaded)) = store.load(&tid)
            && let Some(t) = loaded.meta.title
        {
            if t == "修复登录报错" {
                title = Some(t);
                break;
            }
            title = Some(t);
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

/// 标题必须在**用户输入落地时**生成，而不是等本轮结束。
///
/// 顺序不靠 sleep 赌：对话轮的 provider 会卡在 `title_written` 上，测试只有读到
/// `thread/titleChanged` 才放行它。于是「标题先到」是被构造出来的因果——
/// 只要实现把标题挂在 turn 收尾之后，两边互等就会超时失败。
#[tokio::test(flavor = "multi_thread")]
async fn first_turn_broadcasts_llm_title_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let title_seen = Arc::new(tokio::sync::Notify::new());
    let title_written = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(GatedByTitleProvider {
        title_seen: Arc::clone(&title_seen),
        title_written: Arc::clone(&title_written),
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

    // 先握手：标题请求确实到达过，证明这条路径被走到（而不是压根没生成）。
    tokio::time::timeout(EVENT_WAIT, title_seen.notified())
        .await
        .expect("a title request must reach the provider");

    // 标题生成发生在 turn **开始**时（用户输入一落地就发起），与 turn 并行，
    // 因此 `thread/titleChanged` 必须先于 `turn/completed` 到达。
    let mut saw_title_change = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("thread/titleChanged") => {
                saw_title_change = Some(v);
                break;
            }
            Some("turn/completed") => {
                panic!("titleChanged must arrive before turn/completed; got completion first: {v}")
            }
            _ => {}
        }
    }
    // 放行对话轮，避免后台任务长期悬挂。
    title_written.notify_one();
    let ev = saw_title_change.expect("expected a thread/titleChanged notification");
    assert_eq!(ev["params"]["thread_id"], tid);
    assert_eq!(
        ev["params"]["title"], "修复登录报错",
        "通知必须携带 LLM 标题，而不是兜底截断值"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn title_material_uses_only_the_user_input() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let materials = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MaterialCapturingProvider {
        materials: materials.clone(),
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

    let mut materials_seen = None;
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Some(first) = materials.lock().unwrap().first().cloned() {
            materials_seen = Some(first);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let material = materials_seen.expect("title request must carry material");
    assert!(
        material.starts_with("用户提问：登录页报错了"),
        "material must start from the user input: {material:?}"
    );
    assert!(
        !material.contains("助手回复"),
        "assistant side must be absent when generating right after input: {material:?}"
    );
    h.shutdown().await;
}

/// 只有首轮生成标题：第二次 `turn/start` 不得再调标题模型。
#[tokio::test(flavor = "multi_thread")]
async fn second_turn_does_not_regenerate_the_title() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let title_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CountingTitleProvider {
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
    for (idx, (id, text)) in [(3, "登录页报错了"), (4, "再看看别的")]
        .into_iter()
        .enumerate()
    {
        // 第二次 turn/start 可能撞上上一轮尚未释放的 `active_turn_id`（`turn/completed`
        // 先于收尾发出），这不是产品缺陷：按 `-32012` 重试即可。
        //
        // **必须读到自己那条响应（id == {id}）为止**，不能只读一帧就下结论：流里
        // 还夹着 `thread/titleChanged` / `thread/status/updated` 等通知，把通知当成
        // 响应会漏掉真正的 `-32012`，于是第二轮从未启动、后面等 `turn/completed`
        // 直接超时。标题生成前移到 turn 开始后，这类通知出现得更早，更要守住这条。
        loop {
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"{text}"}}]}}}}"#
            ))
            .await;
            let v = h
                .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
                .await;
            match v.get("error") {
                None => break,
                Some(_) if idx == 0 => panic!("first turn must start: {v}"),
                Some(err) if err["code"] == -32012 => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Some(_) => panic!("unexpected turn/start error: {v}"),
            }
        }
        let _ = read_until_method(&mut h, "turn/completed").await;
    }
    // 给可能迟到的第二次标题请求留一个短窗口：若实现按轮生成，这里会变 2。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        title_calls.load(Ordering::SeqCst),
        1,
        "title must be generated exactly once"
    );
    h.shutdown().await;
}

/// 插件驱动会话（标题创建时已写入、且非组长）**不得**调用标题模型。
///
/// 这条钉的是 `ThreadStore::touch` 里的 `meta.title.is_none()` 判据：若该条件被
/// 放宽，已命名的会话会在每轮首输入时被重新命名，覆盖用户自己起的名字。
#[tokio::test(flavor = "multi_thread")]
async fn a_thread_that_already_has_a_title_never_calls_the_title_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let tid = "thread-with-title";
    // 非组长、标题已存在：插件驱动会话的真实形态。
    write_meta(
        dir.path(),
        tid,
        "my-app 组长",
        Some("my-app"),
        Some("refs/heads/main"),
    );

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
        "a thread that already has a title must not be renamed"
    );
    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(tid)
        .ok()
        .flatten()
        .expect("meta must survive");
    assert_eq!(loaded.meta.title.as_deref(), Some("my-app 组长"));
    h.shutdown().await;
}

/// 标题生成期间用户手动改名：CAS 必须让手写标题胜出。
///
/// 时间窗是真实构造的：标题 provider 停在 `release_title` 上，测试在窗口内发
/// `thread/rename`。若 `generate_title_for_first_input` 把 CAS 退回无条件
/// `rename`，手写标题会被覆盖成「自动标题」，本用例即失败。
///
/// 注意：不要在等 rename 响应时用 `read_matching`——它会丢弃通知帧，把随后
/// 要等的 `turn/completed` 一起吞掉。这里用单循环同时处理两种帧。
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_rename_during_generation_wins_over_the_generated_title() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let title_seen = Arc::new(tokio::sync::Notify::new());
    let release_title = Arc::new(tokio::sync::Notify::new());
    let title_returned = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(TitleBlockingProvider {
        title_seen: Arc::clone(&title_seen),
        release_title: Arc::clone(&release_title),
        title_returned: Arc::clone(&title_returned),
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

    // 标题生成已经发起并卡住：此刻是用户手写改名的真实窗口。
    tokio::time::timeout(EVENT_WAIT, title_seen.notified())
        .await
        .expect("title generation must start before the rename window");
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/rename","params":{{"threadId":"{tid}","title":"我的登录排查"}}}}"#
    ))
    .await;

    // 单循环同时收两条关键帧：rename 响应（id=4）与 turn 收尾。
    // 注意 `read_matching` 会丢弃通知帧，用它等 rename 会把 `turn/completed`
    // 一起吞掉，故这里不用它。
    let mut rename_ok = false;
    let mut turn_done = false;
    for _ in 0..60 {
        if rename_ok && turn_done {
            break;
        }
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            assert!(v.get("error").is_none(), "rename must succeed: {v}");
            rename_ok = true;
            // 手写标题已落盘，放行自动标题：CAS 应让它放弃覆盖。
            release_title.notify_one();
        } else if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            turn_done = true;
        }
    }
    assert!(rename_ok, "rename response never arrived");

    // 等标题任务真的返回。CAS 紧跟在生成之后，用一个有界的轮询给它落地时间
    // （不断言中间态，只断言最终值；若被覆盖，最终值就是「自动标题」）。
    tokio::time::timeout(EVENT_WAIT, title_returned.notified())
        .await
        .expect("title generation must finish once released");
    let mut loaded = None;
    for _ in 0..100 {
        let meta = crate::thread_store::ThreadStore::new(dir.path())
            .read_meta(&tid)
            .ok()
            .flatten();
        let title = meta.as_ref().and_then(|m| m.title.clone());
        loaded = meta;
        // 只在「被覆盖」时提前退出：手写标题本来就已是当前值，看到它不代表
        // CAS 已经跑完，提前 break 会让断言在 CAS 之前下结论。
        if title.as_deref() == Some("自动标题") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let loaded = loaded.expect("meta must exist");
    assert_eq!(loaded.title.as_deref(), Some("我的登录排查"));
    h.shutdown().await;
}
