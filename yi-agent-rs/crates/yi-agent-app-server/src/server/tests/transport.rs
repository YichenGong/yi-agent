use super::*;

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
    let state = crate::rpc::PluginChannelState {
        hub: std::sync::Arc::new(hub),
        client: local,
        client_scope: crate::protocol::Scope::Admin,
        resident_dir: PathBuf::new(),
        theme_handle: crate::theme_tool::ThemeHandle::new(std::env::temp_dir()),
        watchman_home: PathBuf::new(),
        plugin_catalog: PathBuf::new(),
        plugin_home: PathBuf::new(),
        watchman_install: std::sync::Arc::new(|_exe: &Path, _home: &Path| -> Result<(), String> {
            Ok(())
        }),
        resolve_plugin: std::sync::Arc::new(|_name: &str| -> Option<PathBuf> { None }),
        ensure_daemon: std::sync::Arc::new(|_project: &Path| -> Result<bool, String> { Ok(true) }),
        watchman_uninstall: std::sync::Arc::new(|_home: &Path| -> Result<(), String> { Ok(()) }),
        plugin_gate: None,
        managed_session: false,
        bringup_policy: yi_agent_subagent::attach::BringUpPolicy::Embedded,
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
            side_gate: Arc::new(|| true),
            runtimes: Arc::new(StdMutex::new(HashMap::new())),
            thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            managed: Arc::new(StdMutex::new(None)),
            bringup: Arc::new(yi_agent_subagent::binding::BringUpChoice::default()),
            models_path: PathBuf::new(),
            websearch_path: PathBuf::new(),
            settings_path: PathBuf::new(),
            // 这些用例不涉运行期设置：静态装配。
            live_settings: None,
            lead_agent_enabled: false,
            onboarding_preferences_path: PathBuf::new(),
            resident_dir: PathBuf::new(),
            // 空路径 = 不读真实 home；这些用例不查 skill。
            skills_home: PathBuf::new(),
            instance_home: PathBuf::new(),
            remote_dir: PathBuf::new(),
            // 旁支宿主资源必须落在隔离目录:空 remote_dir + None 会让 serve()
            // 把 side-conversations/ 建到当前工作目录(源码树)。
            live_side_parents: None,
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
            plugin_catalog: PathBuf::new(),
            // 测试注入:绝不为一次安装真的拉起 detached daemon(那会拿测试
            // 二进制当 daemon、并白等 10s)。
            resolve_plugin: Arc::new(|_name: &str| -> Option<PathBuf> { None }),
            ensure_daemon: Arc::new(|_project: &Path| -> Result<bool, String> { Ok(true) }),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
        None,
        built.sandbox.clone(),
        Arc::clone(&built.process_manager),
        None,
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
        None,
        built.sandbox.clone(),
        Arc::clone(&built.process_manager),
        None,
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
    log_outcome(&MigrationOutcome::MissingModel, env);
    log_outcome(
        &MigrationOutcome::InvalidProvider {
            reason: "unknown provider: azure".into(),
        },
        env,
    );
}

/// 三个 tui 通知键必须能写入并读回非 null。`FIELDS` 驱动 UI，
/// `current_value` 决定读出的值——漏一个就会静默返回 null，
/// 设置页显示空白，用户改完像没生效。
#[tokio::test(flavor = "multi_thread")]
async fn every_tui_notification_field_is_writable_and_readable() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"settings/write","params":{"patch":{"tui.notifications":"false","tui.notification_method":"bel","tui.notification_condition":"always"}}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "write failed: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let fields = v["result"]["fields"].as_array().expect("fields array");

    for (key, expected) in [
        ("tui.notifications", "false"),
        ("tui.notification_method", "bel"),
        ("tui.notification_condition", "always"),
    ] {
        let field = fields
            .iter()
            .find(|f| f["key"] == key)
            .unwrap_or_else(|| panic!("{key} must be listed"));
        assert_eq!(field["type"], "select", "{key} must be a select: {field}");
        assert_eq!(
            field["value"], expected,
            "{key} must read back what was written: {field}"
        );
    }
}

// ---------------------------------------------------------------------------
// Desktop shell authorization over the local stdio handshake
// ---------------------------------------------------------------------------

/// The local stdio client is the only one that may speak for the shell, and the
/// handshake must say back what it accepted.
///
/// This is the end-to-end half of `shell_authorization`'s unit tests: those prove
/// the rules, this proves the rules are actually wired into `initialize`.
#[tokio::test]
async fn a_local_handshake_reports_the_shell_identity_it_accepted() {
    let mut h = Harness::new();
    // One line on purpose: the transport parses line by line, so a pretty-printed
    // frame would be rejected before any rule here is reached.
    h.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-1","scope_key":"/p/.yi-agent/runtime","issued_by_shell":true}}}"#,
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(1)))
        .await;
    assert_eq!(v["result"]["shell"]["clientRunId"], "app-run-1");
    assert_eq!(v["result"]["shell"]["scopeKey"], "/p/.yi-agent/runtime");
    h.shutdown().await;
}

/// A handshake without the field is the CLI's normal shape and must stay exactly
/// as it was: no `shell` key invented out of nothing.
#[tokio::test]
async fn a_plain_handshake_reports_no_shell() {
    let mut h = Harness::new();
    // Sent directly rather than through the `initialize` helper: that helper
    // consumes the response, and this test's whole assertion is about it.
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(1)))
        .await;
    assert!(
        v["result"].get("shell").is_none(),
        "a CLI handshake must not claim a shell: {v}"
    );
    h.shutdown().await;
}

/// A websocket client that sends the same field must not be believed: over the
/// network this is a phone claiming to be the App, not the shell speaking.
///
/// The merged harness carries one real ws client, so this exercises the identity
/// check on a non-local `ClientId` rather than only in a unit test.
#[tokio::test(flavor = "multi_thread")]
async fn a_websocket_handshake_cannot_claim_the_desktop_shell() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let h = MergedHarness::new().await;
    let mut ws = h.connect_ws().await;
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-1","scope_key":"/p/.runtime","issued_by_shell":true}}}"#
            .into(),
    ))
    .await
    .unwrap();

    // Bounded: an unanswered handshake would otherwise block this read forever and
    // leave the test binary (and its ports and children) alive after the run.
    let msg = tokio::time::timeout(crate::test_support::EVENT_WAIT, ws.next())
        .await
        .expect("the handshake must be answered before the test gives up")
        .expect("the stream must not end before the handshake is answered")
        .expect("transport error while waiting for the handshake response");
    let response: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(response["id"], 1);
    assert!(
        response.get("result").is_some(),
        "the handshake must still succeed: {response}"
    );
    assert!(
        response["result"].get("shell").is_none(),
        "a ws client must never be recognised as the shell: {response}"
    );
}

/// A field the bridge would never write is ignored, not fatal: one bad hint must
/// not turn a working handshake into a failed session.
#[tokio::test]
async fn a_handshake_claiming_a_forged_shell_is_ignored() {
    let mut h = Harness::new();
    h.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"x","issued_by_shell":false}}}"#,
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(1)))
        .await;
    assert!(
        v.get("result").is_some(),
        "the handshake must still succeed: {v}"
    );
    assert!(v["result"].get("shell").is_none());
    h.shutdown().await;
}
