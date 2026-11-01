use super::*;

/// 清单为空且无显式注入时，新建会话必须给出可识别的"未配置模型"错误，
/// 而不是拿一个猜测的名字去发请求。
///
/// **必须显式把 `cfg.model` 置空**：`test_config()` 的 model 是
/// `TEST_MODEL`（`test_support.rs:10,57`，非空），默认 Harness 永远不会
/// 触发拦截——那样这个测试会"通过"却什么都没验证。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_reports_a_missing_model_configuration() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    cfg.model = String::new();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    // `thread/started` 是同一轮里的广播帧,可能与响应交错;按 id 取。
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["code"], -32027, "got {v}");
    assert_eq!(
        v["error"]["data"]["code"], "model_not_configured",
        "got {v}"
    );
}

/// 整包注入（--model + --api-key）时即使清单为空也必须能开会话：
/// 这条路径不经过清单条目，拦截只能看 `cfg.model` 是否为空。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_accepts_an_explicit_injection_without_a_catalog() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    // Harness 本来就注入隔离的 models_path（空清单）；这里只补注入标记。
    cfg.model = "claude-injected".to_string();
    cfg.api_key = "sk-injected".to_string();
    cfg.model_injected = true;
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert!(v.get("error").is_none(), "got {v}");
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

/// `/config` 是「当前会话的配置」，不是 sidecar 进程启动时的快照。
///
/// 桌面端一个 sidecar 服务多个会话：每个会话有自己的 cwd、模型覆盖与权限档。
/// 用进程快照（workdir = sidecar 启动目录、model = 清单默认、yolo = 启动 flag）
/// 回答 `/config`，会在卡片里摊出用户没选过的 provider / 模型 / 工作目录。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_with_thread_id_reports_that_session_config() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{"cwd": cwd.to_string_lossy()}})
        .to_string(),
    )
    .await;
    let started = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    let thread_id = started["result"]["thread_id"]
        .as_str()
        .expect("thread/start must return a thread id")
        .to_string();

    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":3,"method":"config/read",
            "params":{"threadId": thread_id}})
        .to_string(),
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(3)))
        .await;
    assert_eq!(
        v["result"]["workdir"],
        serde_json::json!(cwd.to_string_lossy()),
        "workdir must be the session's cwd, not the sidecar's startup cwd: {v}"
    );
    assert_eq!(
        v["result"]["model"],
        serde_json::json!(crate::test_support::TEST_MODEL),
        "model must be the session's effective model: {v}"
    );
    assert_eq!(
        v["result"]["yolo"],
        serde_json::json!(false),
        "yolo must be the session's permission mode: {v}"
    );
    h.shutdown().await;
}

/// 换了会话模型后，卡片必须跟着换 —— 这是用户最初看到的错误：
/// 会话跑 HORIZON-DeepSeek，卡片却报进程默认的 claude-sonnet-4。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_follows_the_session_model_override() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"model/upsert","params":{"name":"Session Only","provider":"openai","api_url":"https://llm.example/v1","model":"deepseek-x","api_key":"sk-secret-1234"}}"#,
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    assert_eq!(v["result"]["ok"], true, "upsert must land: {v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
        .await;
    let started = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(3)))
        .await;
    let thread_id = started["result"]["thread_id"]
        .as_str()
        .expect("thread/start must return a thread id")
        .to_string();

    // 运行期切到该条目：这是用户真正会做的动作（模型选择器）。
    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":4,"method":"thread/setModel",
            "params":{"threadId": thread_id, "name":"Session Only"}})
        .to_string(),
    )
    .await;
    let switched = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(4)))
        .await;
    assert_eq!(
        switched["result"]["model"], "deepseek-x",
        "the session must actually run the override: {switched}"
    );

    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":5,"method":"config/read",
            "params":{"threadId": thread_id}})
        .to_string(),
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(5)))
        .await;
    assert_eq!(
        v["result"]["model"], "deepseek-x",
        "card must show the session model: {v}"
    );
    assert_eq!(
        v["result"]["provider"], "openai",
        "card must show the session provider: {v}"
    );
    assert_eq!(
        v["result"]["api_url"], "https://llm.example/v1",
        "card must show the session endpoint: {v}"
    );
    h.shutdown().await;
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
        None,
        &std::env::temp_dir(),
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
        None,
        &std::env::temp_dir(),
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
        None,
        &std::env::temp_dir(),
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
        tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).1,
        None,
        std::env::temp_dir(),
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

/// `mcp/set` 改用户的持久配置,必须 Control;只读客户端只能看不能改。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_may_not_write_mcp_config() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":9,"method":"mcp/set","params":{"scope":"master","enabled":false}}"#,
    )
    .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["error"]["code"], -32014, "Observe 客户端必须被拒: {v}");
    h.shutdown().await;
}

/// 反例:Control 客户端(手机)能改——门禁只挡最低档,不是挡所有人。
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_may_write_mcp_config() {
    let mut h = Harness::with_scope(Scope::Control).await;
    initialize(&mut h).await;
    h.send(
        r#"{"jsonrpc":"2.0","id":9,"method":"mcp/set","params":{"scope":"master","enabled":false}}"#,
    )
    .await;
    let v = read_response(&mut h, 9).await;
    // 该 harness 的 workdir 下没有 mcp.json,故这里是业务错误而非权限错误:
    // 关键是它**没有**被判 -32014。
    assert_ne!(
        v["error"]["code"], -32014,
        "Control 客户端不该被判权限不足: {v}"
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

/// `model/probe` 的后台探测：先回 `started`，完成后把权威能力写进清单并广播。
///
/// 上游用 wiremock 模拟「所有档位都接受」。断言两条主线：
/// 1. 响应立即返回（不阻塞 30s 的探测）；
/// 2. `model/capabilitiesChanged` 带上的正是落盘后的能力。
#[tokio::test(flavor = "multi_thread")]
async fn model_probe_records_capabilities_and_broadcasts_them() {
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

    // upsert 已经自动起了一轮探测；先收下它的结果，再测显式触发的那一轮
    // （否则显式请求会被去重表判成重复，测不到 `started:true` 这条路径）。
    let first = read_notification(&mut h, "model/capabilitiesChanged").await;
    assert_eq!(first["params"]["name"], "A");

    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"model/probe","params":{"name":"A"}}"#)
        .await;
    let v = read_response(&mut h, 10).await;
    assert_eq!(v["result"]["started"], true, "probe must start: {v}");

    let n = read_notification(&mut h, "model/capabilitiesChanged").await;
    assert_eq!(n["params"]["name"], "A");
    assert_eq!(
        n["params"]["reasoning_capabilities"]["chat"]["supported"],
        serde_json::json!(["none", "minimal", "low", "medium", "high", "xhigh", "max"]),
        "the broadcast must carry the persisted result: {n}"
    );

    // 落盘一致：`model/list` 读到的就是广播出去的那份。
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"model/list","params":{}}"#)
        .await;
    let v = read_response(&mut h, 11).await;
    assert_eq!(
        v["result"]["models"][0]["reasoning_capabilities"]["chat"]["supported"],
        serde_json::json!(["none", "minimal", "low", "medium", "high", "xhigh", "max"]),
        "persisted capabilities must match the broadcast: {v}"
    );
    h.shutdown().await;
}

/// 同名条目的探测不得并发跑两轮：第二次请求直接回 `started:false`。
#[tokio::test(flavor = "multi_thread")]
async fn model_probe_is_deduplicated_while_one_is_running() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_string("data: [DONE]\n\n"),
        )
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

    // upsert 自动起的那一轮先跑完，去重表才会空出来。
    read_notification(&mut h, "model/capabilitiesChanged").await;

    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"model/probe","params":{"name":"A"}}"#)
        .await;
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"model/probe","params":{"name":"A"}}"#)
        .await;
    let first = read_response(&mut h, 10).await;
    let second = read_response(&mut h, 11).await;
    assert_eq!(first["result"]["started"], true, "{first}");
    assert_eq!(
        second["result"]["started"], false,
        "a second concurrent probe must be rejected: {second}"
    );

    // 让这一轮跑完，避免 harness 结束时留下悬挂任务。
    read_notification(&mut h, "model/capabilitiesChanged").await;
    h.shutdown().await;
}

/// 探测未知条目回结构化 `model_not_found`，不是内部错误。
#[tokio::test(flavor = "multi_thread")]
async fn model_probe_on_an_unknown_entry_is_a_structured_error() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"model/probe","params":{"name":"nope"}}"#)
        .await;
    let v = read_response(&mut h, 9).await;
    assert_eq!(v["error"]["data"]["code"], "model_not_found", "{v}");
    h.shutdown().await;
}

/// 只改密钥不触发重测：能力与连接字段都未变，重测是纯浪费。
#[tokio::test(flavor = "multi_thread")]
async fn an_upsert_that_only_changes_the_key_does_not_reprobe() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
        .mount(&server)
        .await;

    let mut h = Harness::new();
    initialize(&mut h).await;
    let upsert = |id: u64, key: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"model/upsert","params":{{"name":"A","provider":"openai","api_url":"{}","model":"m","api_key":"{key}","api_style":"chat"}}}}"#,
            server.uri()
        )
    };
    h.send(&upsert(9, "k1")).await;
    assert_eq!(read_response(&mut h, 9).await["result"]["ok"], true);
    read_notification(&mut h, "model/capabilitiesChanged").await;

    h.send(&upsert(10, "k2")).await;
    assert_eq!(read_response(&mut h, 10).await["result"]["ok"], true);
    // 短窗口负等待：不该有第二轮探测广播。
    assert!(
        h.try_read_value(Duration::from_millis(600))
            .await
            .map(|v| v.get("method").and_then(|m| m.as_str()) != Some("model/capabilitiesChanged"))
            .unwrap_or(true),
        "changing only the key must not re-probe"
    );
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

/// `websearch/get` 只读盘且只回掩码,`Observe` 即可;`set` / `test` 触及密钥,
/// 必须 `Control` —— 与 `model/*` 同一档位划分。
#[tokio::test(flavor = "multi_thread")]
async fn an_observe_client_may_read_websearch_but_not_write_it() {
    let mut h = (Harness::with_scope(Scope::Observe)).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":20,"method":"websearch/get","params":{}}"#)
        .await;
    let v = read_response(&mut h, 20).await;
    assert!(
        v["result"]["engines"]["bocha"].is_object(),
        "Observe may read the masked config: {v}"
    );

    h.send(r#"{"jsonrpc":"2.0","id":21,"method":"websearch/set","params":{"enabled":false}}"#)
        .await;
    let v = read_response(&mut h, 21).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "writes require Control, Observe must be refused: {v}"
    );

    h.send(r#"{"jsonrpc":"2.0","id":22,"method":"websearch/test","params":{}}"#)
        .await;
    let v = read_response(&mut h, 22).await;
    assert_eq!(
        v["error"]["code"], -32014,
        "connection tests touch the key, so they require Control: {v}"
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

/// `onboarding/apply` 在连接测试失败时必须零写入并给出稳定错误码。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_apply_refuses_when_the_probe_fails() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    // 端口 1 上没有监听者:连接被立即拒绝,不会等满 15s 超时。
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply","params":{"provider":"anthropic","model":"m","api_url":"http://127.0.0.1:1","api_key":"k"}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["data"]["code"], "model_test_failed", "got {v}");
    assert_eq!(v["error"]["code"], -32602, "got {v}");

    // 清单没有被写进去,状态仍是 needed。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"onboarding/status","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["needed"], true, "got {v}");
}

/// `onboarding/apply` 只落清单(不再写 `.env`);探测通过后 `status` 立刻翻成 ready。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_apply_lands_the_catalog_after_a_passing_probe() {
    let server = anthropic_sse_server().await;
    let mut h = Harness::new();
    initialize(&mut h).await;
    let req = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"onboarding/apply",
        "params":{"provider":"anthropic","model":"m","api_url":server.uri(),"api_key":"sk-x"}
    });
    h.send(&req.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
    assert!(v["result"].get("name").is_some(), "got {v}");
    assert!(v["result"].get("env_written").is_none(), "got {v}");
    assert!(v["result"].get("imported").is_none(), "got {v}");

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
    let server = anthropic_sse_server().await;

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

/// 能力可信时不支持的档位必须被拒，且零副作用（不落盘、不广播）。
#[tokio::test(flavor = "multi_thread")]
async fn set_reasoning_effort_rejects_a_tier_the_model_does_not_support() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    seed_capabilities(
        &h._models_dir.path().join("models.json"),
        "L",
        &["low", "medium"],
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"high"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(v["error"]["code"], -32602, "got {v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("low, medium"),
        "the error must name what IS supported: {message}"
    );

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert_eq!(
        loaded.meta.reasoning_effort, None,
        "a rejected tier must not be persisted"
    );
    h.shutdown().await;
}

/// 能力可信且档位在列表内：正常落盘。
#[tokio::test(flavor = "multi_thread")]
async fn set_reasoning_effort_accepts_a_supported_tier() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    seed_capabilities(
        &h._models_dir.path().join("models.json"),
        "L",
        &["low", "medium"],
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"medium"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(v["result"]["ok"], true, "got {v}");

    let loaded = crate::thread_store::ThreadStore::new(dir.path())
        .load(&tid)
        .expect("load must not fail")
        .expect("thread persists");
    assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("medium"));
    h.shutdown().await;
}

/// 能力未知（没有探测记录）时保持宽松：七档仍然都能设。
#[tokio::test(flavor = "multi_thread")]
async fn set_reasoning_effort_stays_permissive_when_capabilities_are_unknown() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"thread/setReasoningEffort","params":{{"threadId":"{tid}","effort":"max"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 20).await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
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

/// `settings/read` 经真实 dispatch 路径可达：每个字段都带 `value` 与 `default`，
/// 未设置的字段 `value` 为 null（而不是默认值）。
#[tokio::test(flavor = "multi_thread")]
async fn settings_read_over_rpc_reports_unset_fields_as_null() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;

    let fields = v["result"]["fields"].as_array().expect("fields array");
    assert!(!fields.is_empty(), "the field list must not be empty");

    let max_turns = fields
        .iter()
        .find(|f| f["key"] == "agent.max_turns")
        .expect("agent.max_turns must be listed");
    assert_eq!(max_turns["default"], "200");
    assert!(
        max_turns["value"].is_null(),
        "a fresh instance has nothing set: {max_turns}"
    );

    let ratio = fields
        .iter()
        .find(|f| f["key"] == "compact.ratio")
        .expect("compact.ratio must be listed");
    assert_eq!(ratio["default"], "80");

    let sandbox = fields
        .iter()
        .find(|f| f["key"] == "startup.sandbox")
        .expect("startup.sandbox must be listed");
    assert_eq!(sandbox["type"], "select");
    assert!(
        sandbox["options"]
            .as_array()
            .is_some_and(|options| options.contains(&serde_json::json!("workspace-write"))),
        "the select must carry its options: {sandbox}"
    );

    assert!(
        v["result"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("settings.json")),
        "the response must report where the file lives: {v}"
    );
}

/// `settings/write` 落盘后可被 `settings/read` 读回，且未设置的字段仍为 null。
#[tokio::test(flavor = "multi_thread")]
async fn settings_write_then_read_round_trips() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"settings/write","params":{"patch":{"agent.max_turns":500,"startup.sandbox":"read-only"}}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let fields = v["result"]["fields"].as_array().expect("fields array");
    let max_turns = fields
        .iter()
        .find(|f| f["key"] == "agent.max_turns")
        .unwrap();
    assert_eq!(max_turns["value"], 500);
    let sandbox = fields
        .iter()
        .find(|f| f["key"] == "startup.sandbox")
        .unwrap();
    assert_eq!(sandbox["value"], "read-only");
    let ratio = fields.iter().find(|f| f["key"] == "compact.ratio").unwrap();
    assert!(
        ratio["value"].is_null(),
        "writing one field must not materialise the others: {ratio}"
    );
}

/// 非法值必须报 `invalid_setting` 并带字段名，供 UI 内联提示；且零写入。
#[tokio::test(flavor = "multi_thread")]
async fn settings_write_rejects_an_invalid_value() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"settings/write","params":{"patch":{"agent.max_turns":0}}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["error"]["data"]["code"], "invalid_setting", "got: {v}");
    assert_eq!(v["error"]["data"]["key"], "agent.max_turns", "got: {v}");

    // 零写入：读回来仍是未设置。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let fields = v["result"]["fields"].as_array().expect("fields array");
    let max_turns = fields
        .iter()
        .find(|f| f["key"] == "agent.max_turns")
        .unwrap();
    assert!(
        max_turns["value"].is_null(),
        "a rejected write must not land"
    );
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
    assert_eq!(
        v["result"][crate::settings_store::WATCHMAN_KEY],
        true,
        "缺省为开:{v}"
    );

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
        "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-old\n",
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
