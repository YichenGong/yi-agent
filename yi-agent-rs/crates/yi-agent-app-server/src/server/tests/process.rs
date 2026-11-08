use super::*;

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

/// 子任务列表项携带它自己的累计用量。
#[test]
fn children_for_thread_carries_task_usage() {
    let mut task = summary("child-1", "running", Some("thread-a"));
    task.usage = yi_agent_store::ipc::IpcTaskUsage {
        input_tokens: 1_200,
        output_tokens: 34,
        cache_read_tokens: 56_000,
    };
    let children = children_for_thread(&[task], "thread-a");
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0].usage,
        crate::protocol::AgentChildUsage {
            input_tokens: 1_200,
            output_tokens: 34,
            cache_read_tokens: 56_000,
        }
    );
}

/// 汇总对整份列表求和：已完成的子任务烧掉的 token 同样是这个会话的开销。
#[test]
fn subagent_totals_sums_every_child() {
    let child = |input, output, cache_read| crate::protocol::AgentChild {
        task_id: "x".into(),
        objective: None,
        state: "completed".into(),
        last_step: None,
        parent_task_id: Some("root".into()),
        usage: crate::protocol::AgentChildUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
        },
    };
    let children = vec![child(10, 1, 100), child(20, 2, 200)];
    assert_eq!(
        subagent_totals(&children),
        SubagentTotals {
            input: 30,
            output: 3,
            cache_read: 300,
        }
    );
    // 空列表是零，不是错误。
    assert_eq!(subagent_totals(&[]), SubagentTotals::default());
}

/// `/clear` 之后要把 clear 之前的量扣掉：daemon 的 task 记录不会随 clear 删除，
/// 不扣的话旧数字会被下一次 children 推送带回来。
#[test]
fn subagent_baseline_subtracts_cleared_usage() {
    let raw = SubagentTotals {
        input: 900,
        output: 90,
        cache_read: 8000,
    };
    let baseline = SubagentTotals {
        input: 400,
        output: 40,
        cache_read: 3000,
    };
    assert_eq!(
        raw.saturating_sub(baseline),
        SubagentTotals {
            input: 500,
            output: 50,
            cache_read: 5000,
        }
    );
}

/// daemon 重启 / 任务被清理都可能让当前值短暂小于基线：报 0，不许回绕。
#[test]
fn subagent_baseline_never_underflows() {
    let raw = SubagentTotals {
        input: 10,
        output: 1,
        cache_read: 5,
    };
    let baseline = SubagentTotals {
        input: 900,
        output: 90,
        cache_read: 8000,
    };
    assert_eq!(raw.saturating_sub(baseline), SubagentTotals::default());
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
            usage: crate::protocol::AgentChildUsage {
                input_tokens: 1_200,
                output_tokens: 34,
                cache_read_tokens: 56_000,
            },
        }],
    };
    let json = serde_json::to_value(&children).unwrap();
    assert_eq!(json["method"], "agent/children/updated");
    assert_eq!(json["params"]["threadId"], "thread-1");
    assert_eq!(json["params"]["children"][0]["taskId"], "task-1");
    assert_eq!(json["params"]["children"][0]["lastStep"], "running tests");
    // 用量走 camelCase，与列表里其它字段一致。
    assert_eq!(json["params"]["children"][0]["usage"]["inputTokens"], 1_200);
    assert_eq!(json["params"]["children"][0]["usage"]["outputTokens"], 34);
    assert_eq!(
        json["params"]["children"][0]["usage"]["cacheReadTokens"],
        56_000
    );

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

/// 未知 `threadId` 必须显式报错，绝不静默回退成进程快照。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_rejects_an_unknown_thread_instead_of_the_process_snapshot() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"config/read",
            "params":{"threadId":"thread-does-not-exist"}})
        .to_string(),
    )
    .await;
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    assert!(
        v.get("error").is_some(),
        "unknown thread must be an error: {v}"
    );
    assert!(
        v.get("result").is_none(),
        "must not answer with the process snapshot: {v}"
    );
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
            prevent_idle_sleep: false,
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
                prevent_idle_sleep: false,
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
            prevent_idle_sleep: false,
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

/// 累计 input 必须不小于最后一轮快照，且四类累计都要落盘、可读回。
/// 证明 `/usage` 统计的是「所有 provider 调用之和」，不是最后一次请求的上下文长度。
#[tokio::test(flavor = "multi_thread")]
async fn read_items_returns_persisted_totals() {
    let mut h = Harness::with_factory(build_usage_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":32,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 32).await;
    let totals = v["result"]["totals"]
        .as_object()
        .unwrap_or_else(|| panic!("readItems 必须回传累计用量: {v}"));
    let usage = v["result"]["usage"].as_object().expect("usage");
    assert!(
        totals["input_tokens"].as_u64().unwrap_or(0) >= usage["input_tokens"].as_u64().unwrap_or(0),
        "累计值不得小于最后一轮快照: {v}"
    );
    assert!(totals["output_tokens"].as_u64().unwrap_or(0) > 0, "{v}");
}

/// `/clear` 截断日志后累计必须归零，否则重启前后数字会对不上。
#[tokio::test(flavor = "multi_thread")]
async fn clear_resets_persisted_totals() {
    let mut h = Harness::with_factory(build_usage_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":41,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }
    // 刚跑完一轮立刻 clear 可能撞上 `active_turn_id` 尚未清空的窗口（既有约定），
    // 用同一个重试 helper。
    let cleared = clear_thread_retrying(&mut h, &tid).await;
    assert!(
        cleared.get("error").is_none(),
        "clear must succeed: {cleared}"
    );
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":43,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 43).await;
    assert_eq!(
        v["result"]["totals"]["input_tokens"].as_u64().unwrap_or(1),
        0,
        "{v}"
    );
}

/// B7（RPC 侧）：`process/list` 与 `process/read` 都直接序列化 manager 的 DTO，
/// 因此新增的意图字段必须两处都出现；缺省 false 也必须可读。
///
/// 这里**不**新增 RPC 字段或 `process/updated` 载荷——只验证既有两条读路径。
#[tokio::test]
async fn sleep_hardening_process_dtos_expose_the_retained_intent() {
    use std::sync::Arc;

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

    let opted_in = held
        .start(yi_agent_tools::ProcessStartOptions {
            command: "printf READY; sleep 30".into(),
            name: Some("brake".into()),
            cwd: None,
            env: Default::default(),
            on_exit: Default::default(),
            ready_pattern: Some("READY".into()),
            ready_timeout_sec: Some(5),
            prevent_idle_sleep: true,
        })
        .await
        .expect("the opted-in process starts");
    let opted_out = held
        .start(yi_agent_tools::ProcessStartOptions {
            command: "sleep 30".into(),
            name: Some("resident".into()),
            cwd: None,
            env: Default::default(),
            on_exit: Default::default(),
            ready_pattern: None,
            ready_timeout_sec: None,
            prevent_idle_sleep: false,
        })
        .await
        .expect("the opted-out process starts");

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
    ))
    .await;
    let listed = read_response(&mut h, 3).await;
    assert!(listed["error"].is_null(), "{listed}");
    let by_name = |name: &str| -> serde_json::Value {
        listed["result"]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == serde_json::json!(name))
            .unwrap_or_else(|| panic!("{name} must be listed: {listed}"))
            .clone()
    };
    assert_eq!(
        by_name("brake")["prevent_idle_sleep"],
        serde_json::json!(true)
    );
    assert_eq!(
        by_name("resident")["prevent_idle_sleep"],
        serde_json::json!(false),
        "the omitted intent reads back as false, not as a missing key"
    );

    for (id, process_id) in [(4u64, &opted_in.process_id), (5u64, &opted_out.process_id)] {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{process_id}"}}}}"#
        ))
        .await;
        let read = read_response(&mut h, id).await;
        assert!(read["error"].is_null(), "{read}");
        let expected = if id == 4 {
            serde_json::json!(true)
        } else {
            serde_json::json!(false)
        };
        assert_eq!(read["result"]["prevent_idle_sleep"], expected, "{read}");
    }

    let _ = held.shutdown().await;
    h.shutdown().await;
}

/// 一个「收到 ping 就回 ack」的替身守望者 task。
///
/// 用真实 spawn 的 task 而不是桩:要断言的正是「task 还在跑」,只有真 task 才
/// 能证明 `stop()` 有没有 abort 它。容量 1 + 测试外层 timeout,失败不会永等。
type PingAck = (
    tokio::sync::mpsc::Sender<()>,
    tokio::sync::mpsc::Receiver<()>,
    tokio::task::JoinHandle<()>,
);

fn ping_ack() -> PingAck {
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (ack_tx, ack_rx) = tokio::sync::mpsc::channel::<()>(1);
    let task = tokio::spawn(async move {
        while ping_rx.recv().await.is_some() {
            if ack_tx.send(()).await.is_err() {
                break;
            }
        }
    });
    (ping_tx, ack_rx, task)
}

/// 等一个已被 `stop()` 的 task 真的结束。
///
/// `JoinHandle::abort` 只是投递取消请求:被 abort 的任务要等到下一次被调度才观察
/// 到取消,故紧接着调 `is_finished()` 可能仍读到 `false`。这里在既有外层 timeout
/// 之内做**有界让步等待**而不是固定 sleep——正常情况下第一两次 `yield_now` 就够。
async fn wait_until_finished(handle: &tokio::task::AbortHandle) {
    for _ in 0..10_000 {
        if handle.is_finished() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the watcher task must finish once it is stopped");
}

/// 只 unwatch 轨迹，不得顺手停掉同一个 thread 的子任务列表与进程守望者。
///
/// 返回任务详情页即 `agent/trace/unwatch`；旧实现把它当成「关掉这个 thread 的
/// 一切」，于是子 agent 列表和后台进程流一起断掉。这里用「ping 能收到 ack、
/// abort 未完成」证明两个兄弟守望者仍在运行，并另放 thread-b 证明没误清别人。
#[tokio::test]
async fn trace_unwatch_preserves_children_and_process_watchers() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (children_ping, mut children_ack, children_task) = ping_ack();
        let (process_ping, mut process_ack, process_task) = ping_ack();
        let (other_children_ping, mut other_children_ack, other_children_task) = ping_ack();
        // 三个 ping 端都活到测试结束:循环因 `recv()` 返回 `None` 自然退出是
        // 「自己结束」,那样 `is_finished()` 就证明不了 `stop()` 是否调过。
        let (_trace_ping, _, trace_task) = ping_ack();
        let (other_trace_ping, mut other_trace_ack, other_trace_task) = ping_ack();
        let (_other_process_ping, _, other_process_task) = ping_ack();
        let children_abort = children_task.abort_handle();
        let process_abort = process_task.abort_handle();
        let other_children_abort = other_children_task.abort_handle();
        let trace_abort = trace_task.abort_handle();
        let other_trace_abort = other_trace_task.abort_handle();

        let mut traces = HashMap::new();
        traces.insert("thread-a".to_string(), TraceWatch { task: trace_task });
        traces.insert(
            "thread-b".to_string(),
            TraceWatch {
                task: other_trace_task,
            },
        );
        let mut children = HashMap::new();
        children.insert(
            "thread-a".to_string(),
            ChildrenWatch {
                task: children_task,
            },
        );
        children.insert(
            "thread-b".to_string(),
            ChildrenWatch {
                task: other_children_task,
            },
        );
        let mut processes = HashMap::new();
        processes.insert("thread-a".to_string(), ProcessWatch { task: process_task });
        processes.insert(
            "thread-b".to_string(),
            ProcessWatch {
                task: other_process_task,
            },
        );

        stop_trace_watch("thread-a", &mut traces).await;

        assert!(!traces.contains_key("thread-a"), "trace watch is removed");
        assert!(
            children.contains_key("thread-a"),
            "children watch must survive a trace unwatch"
        );
        assert!(
            processes.contains_key("thread-a"),
            "process watch must survive a trace unwatch"
        );
        children_ping.send(()).await.unwrap();
        process_ping.send(()).await.unwrap();
        assert_eq!(children_ack.recv().await, Some(()));
        assert_eq!(process_ack.recv().await, Some(()));
        assert!(!children_abort.is_finished());
        assert!(!process_abort.is_finished());

        // 被 unwatch 的那条轨迹自己必须真的停掉。它的 ping 端仍在,所以只有
        // `stop()` 的 abort 能让它结束。
        wait_until_finished(&trace_abort).await;

        // thread-b 一点没动。
        assert!(traces.contains_key("thread-b"), "thread-b keeps its trace");
        assert!(
            children.contains_key("thread-b"),
            "thread-b keeps its children watch"
        );
        assert!(
            processes.contains_key("thread-b"),
            "thread-b keeps its process watch"
        );
        other_trace_ping.send(()).await.unwrap();
        other_children_ping.send(()).await.unwrap();
        assert_eq!(other_trace_ack.recv().await, Some(()));
        assert_eq!(other_children_ack.recv().await, Some(()));
        assert!(!other_trace_abort.is_finished());
        assert!(!other_children_abort.is_finished());
    })
    .await
    .expect("watcher assertions must not hang");
}

/// 删除会话时三类守望者都要停：与只 unwatch 轨迹相反，这里必须全清。
#[tokio::test]
async fn thread_delete_stops_all_watchers() {
    tokio::time::timeout(Duration::from_secs(2), async {
        // 三个 ping 端都活到测试结束:否则三个循环会因通道关闭自行退出,
        // `is_finished()` 就与 `stop_thread_watches` 到底有没有 abort 无关。
        let (_trace_ping, _, trace_task) = ping_ack();
        let (_children_ping, _, children_task) = ping_ack();
        let (_process_ping, _, process_task) = ping_ack();
        let (other_trace_ping, mut other_trace_ack, other_trace_task) = ping_ack();
        // thread-b 的两条流只用来断言「没被误清」,故保留 ping 端不丢,
        // 让它们的循环一直活着。
        let (_other_children_ping, _, other_children_task) = ping_ack();
        let (_other_process_ping, _, other_process_task) = ping_ack();
        let trace_abort = trace_task.abort_handle();
        let children_abort = children_task.abort_handle();
        let process_abort = process_task.abort_handle();
        let other_trace_abort = other_trace_task.abort_handle();
        let other_children_abort = other_children_task.abort_handle();
        let other_process_abort = other_process_task.abort_handle();

        let mut traces = HashMap::new();
        traces.insert("thread-a".to_string(), TraceWatch { task: trace_task });
        traces.insert(
            "thread-b".to_string(),
            TraceWatch {
                task: other_trace_task,
            },
        );
        let mut children = HashMap::new();
        children.insert(
            "thread-a".to_string(),
            ChildrenWatch {
                task: children_task,
            },
        );
        children.insert(
            "thread-b".to_string(),
            ChildrenWatch {
                task: other_children_task,
            },
        );
        let mut processes = HashMap::new();
        processes.insert("thread-a".to_string(), ProcessWatch { task: process_task });
        processes.insert(
            "thread-b".to_string(),
            ProcessWatch {
                task: other_process_task,
            },
        );

        stop_thread_watches("thread-a", &mut traces, &mut children, &mut processes).await;

        assert!(!traces.contains_key("thread-a"), "trace watch is removed");
        assert!(!children.contains_key("thread-a"), "children watch stops");
        assert!(!processes.contains_key("thread-a"), "process watch stops");
        // 三个 ping 端都还活着,故只有 `stop()` 真 abort 了它们才会结束。
        wait_until_finished(&trace_abort).await;
        wait_until_finished(&children_abort).await;
        wait_until_finished(&process_abort).await;

        // 另一个 thread 不受影响。
        assert!(traces.contains_key("thread-b"), "thread-b keeps its trace");
        assert!(
            children.contains_key("thread-b"),
            "thread-b keeps its children watch"
        );
        assert!(
            processes.contains_key("thread-b"),
            "thread-b keeps its process watch"
        );
        other_trace_ping.send(()).await.unwrap();
        assert_eq!(other_trace_ack.recv().await, Some(()));
        assert!(!other_trace_abort.is_finished());
        assert!(!other_children_abort.is_finished());
        assert!(!other_process_abort.is_finished());
    })
    .await
    .expect("watcher assertions must not hang");
}

/// `agent/trace/unwatch` 走真实 RPC 臂时也不得碰兄弟守望者。
///
/// 上面两个用例直接调 helper，证明不了「臂里又被加回一句 children/process 清理」
/// ——那种回归只在 `rpc/agent.rs` 的请求分派里。这里改用真实 `AppState` 驱动同一个
/// `handle`：children 守望者的 task 由本测试持有（挂着的 task，不依赖 daemon），
/// 发完请求后既要看到 `{"stopped": true}`，也要看到它仍在 map 里、仍未被 abort。
#[tokio::test]
async fn trace_unwatch_rpc_preserves_children_watch() {
    tokio::time::timeout(Duration::from_secs(2), async {
        use crate::protocol::{RequestEnvelope, RequestId, Scope};
        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::AtomicU64;
        use tokio::sync::{Mutex, oneshot};

        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let client = crate::broadcast::ClientId::local();
        let mut outbox = hub.register(client.clone());

        let dir = tempfile::TempDir::new().unwrap();
        let models_path = dir.path().join("models.json");
        let settings_path = dir.path().join("settings.json");
        let websearch_path = dir.path().join("websearch.json");
        let theme = crate::theme_tool::ThemeHandle::new(dir.path().to_path_buf());
        let cfg = test_config();
        let projects = dir.path().join("runtime");
        std::fs::create_dir_all(&projects).unwrap();

        let mut threads = HashMap::new();
        threads.insert(
            "thread-a".to_string(),
            tick_test_session(dir.path(), "thread-a", crate::protocol::ThreadStatus::Idle),
        );
        let mut uploads = crate::image_upload::UploadRegistry::new();
        let mut pending_activation = HashMap::new();
        let runtimes = Arc::new(StdMutex::new(HashMap::new()));
        let thread_roots = Arc::new(StdMutex::new(HashMap::new()));
        let mut trace_watches: HashMap<String, TraceWatch> = HashMap::new();
        let mut children_watches: HashMap<String, ChildrenWatch> = HashMap::new();
        let mut process_watches: HashMap<String, ProcessWatch> = HashMap::new();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let perm_seq = Arc::new(AtomicU64::new(0));
        let (turn_tx, mut turn_rx) = mpsc::channel(1);
        let mut pending_lead_replies = HashMap::new();
        let mut side_tasks = tokio::task::JoinSet::new();
        let side_replay = crate::side_conversation::shared_replay_store();
        let side_registry = test_side_registry(dir.path(), None);
        let probes = crate::model_rpc::ProbeRegistry::default();
        let workspaces = Arc::new(WorkspaceIndex::new(dir.path().join("workspaces.json")));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            dir.path().join("devices.json"),
        )));
        let git_diff = crate::git_diff_tool::GitDiffHandle::new();
        let view_frames: crate::server::ViewFrames = Arc::new(Mutex::new(HashMap::new()));
        let lead_inbox = crate::lead::LeadInbox::new();
        let lead_replies = crate::lead::LeadReplies::new();
        let lead_control = crate::lead_control::LeadControlInbox::new();
        let lead_control_replies = crate::lead_control::LeadControlReplies::new();
        let watchman: crate::server::WatchmanInstall = Arc::new(|_, _| Ok(()));
        let unwatchman: crate::server::WatchmanUninstall = Arc::new(|_| Ok(()));
        let build_agent = build_test_agent;
        // main 引入的托管授权槽位：本测试走内嵌路径，没有 shell 授权。
        let managed: crate::server::ManagedSlot = Arc::new(StdMutex::new(None));

        // 两个兄弟守望者的 task:收 ping 直到 ping 端被丢的循环。本测试持有 ping
        // 端,故它们一直挂着;断言 `!is_finished()` 才与「有没有被 abort」等价。
        let (children_ping, mut children_ack, children_task) = ping_ack();
        let children_abort = children_task.abort_handle();
        children_watches.insert(
            "thread-a".to_string(),
            ChildrenWatch {
                task: children_task,
            },
        );
        let (_process_ping, _, process_task) = ping_ack();
        let process_abort = process_task.abort_handle();
        process_watches.insert("thread-a".to_string(), ProcessWatch { task: process_task });

        let req = RequestEnvelope {
            jsonrpc: Some("2.0".to_string()),
            id: RequestId::Num(7),
            method: "agent/trace/unwatch".to_string(),
            params: json!({ "threadId": "thread-a" }),
        };

        // 具名局部变量：`&Arc<closure>` 不会自动擦成 `&SideGate`，且在结构体
        // 字面量里取临时值引用要靠生命周期延长，显式绑定更清楚。
        let side_gate: crate::side_gate::SideGate = Arc::new(|| true);
        let flow = {
            let mut state = crate::rpc::AppState {
                hub: &hub,
                client: &client,
                is_remote: false,
                shell_authorization: None,
                client_scope: Scope::Admin,
                cfg: &cfg,
                theme_handle: &theme,
                models_path: &models_path,
                websearch_path: &websearch_path,
                settings_path: &settings_path,
                probes: &probes,
                onboarding_preferences_path: &dir.path().join("preferences.json"),
                watchman_home: dir.path(),
                resident_dir: dir.path(),
                instance_home: dir.path(),
                watchman_install: &watchman,
                watchman_uninstall: &unwatchman,
                uploads: &mut uploads,
                threads: &mut threads,
                workspaces: &workspaces,
                pairing: &pairing,
                pending_activation: &mut pending_activation,
                runtimes: &runtimes,
                thread_roots: &thread_roots,
                managed: &managed,
                process_watches: &mut process_watches,
                trace_watches: &mut trace_watches,
                children_watches: &mut children_watches,
                pending: &pending,
                perm_seq: &perm_seq,
                permission_timeout: PERMISSION_TIMEOUT,
                turn_rx: &mut turn_rx,
                turn_tx: &turn_tx,
                build_agent: &build_agent,
                git_diff_handle: &git_diff,
                view_frames: &view_frames,
                skills_home: dir.path(),
                lead_inbox: &lead_inbox,
                lead_replies: &lead_replies,
                pending_lead_replies: &mut pending_lead_replies,
                lead_control: &lead_control,
                lead_control_replies: &lead_control_replies,
                lead_agent_enabled: false,
                side_gate: &side_gate,
                side_registry: &side_registry,
                side_tasks: &mut side_tasks,
                side_replay: &side_replay,
                remote_dir: &projects,
                relay_url: None,
                // 静态装配：这些用例不观察热更新。
                live_settings: None,
            };
            crate::rpc::agent::handle(&mut state, &req, req.id.clone())
                .await
                .expect("the unwatch arm must answer")
        };
        assert!(
            matches!(flow, crate::rpc::Flow::Continue),
            "the unwatch arm consumes the request"
        );

        // 应答真的写到了这个客户端。
        let reply = outbox.recv().await.expect("a response frame");
        assert_eq!(reply["id"], serde_json::json!(7));
        assert_eq!(
            reply["result"],
            serde_json::json!({ "stopped": true }),
            "unwatch answers stopped:true: {reply}"
        );

        // 回归点:臂里若又加回 children/process 的 remove+stop,这里就红。
        assert!(
            children_watches.contains_key("thread-a"),
            "agent/trace/unwatch must not drop the children watch"
        );
        assert!(
            !children_abort.is_finished(),
            "agent/trace/unwatch must not abort the children watch task"
        );
        assert!(
            process_watches.contains_key("thread-a"),
            "agent/trace/unwatch must not drop the process watch"
        );
        assert!(
            !process_abort.is_finished(),
            "agent/trace/unwatch must not abort the process watch task"
        );

        children_ping.send(()).await.unwrap();
        assert_eq!(
            children_ack.recv().await,
            Some(()),
            "the children watch loop is still serving"
        );
    })
    .await
    .expect("the RPC assertions must not hang");
}
