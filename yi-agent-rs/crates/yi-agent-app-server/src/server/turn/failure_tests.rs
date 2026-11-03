//! `turn::failure` 的真实会话夹具与回归用例。
//!
//! 夹具刻意**不**启动 driver：这里测的是启动期失败收口，只需要真实的
//! `ThreadSession`（真实 store、真实 mpsc、真实状态句柄），不需要模型。

use super::failure::{fail_announced_turn, rollback_turn};
use super::{StartStage, start_turn_core, start_turn_core_inner};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::protocol::{RequestId, ThreadStatus};
use crate::session::{ThreadSession, TurnPrompt};

/// 构造一个最小真实会话：四个 mpsc 通道、subagent watch、状态句柄、真实
/// `ThreadStore`。store 按现有 `create` API 登记 meta，返回前不 spawn driver。
///
/// 返回 prompt 的接收端，测试可以直接 `drop` 它来模拟「driver 已退出」。
fn test_session(dir: &Path, thread_id: &str) -> (ThreadSession, mpsc::Receiver<TurnPrompt>) {
    let (prompt_tx, receiver) = mpsc::channel(8);
    let (interrupt_tx, _) = mpsc::channel(1);
    let (interject_tx, _) = mpsc::channel(1);
    let (session_tx, _) = mpsc::channel(1);
    let store = Arc::new(crate::thread_store::ThreadStore::new(dir));
    // 用 serde 读一份最小合法 meta，避免手抄所有带默认值的字段。
    let meta = serde_json::from_value(serde_json::json!({
        "thread_id": thread_id,
        "cwd": dir.display().to_string(),
        "model": "test",
        "created_at": 1,
        "updated_at": 1
    }))
    .expect("minimal thread meta must deserialize");
    store.create(&meta).expect("register the thread meta");
    (
        ThreadSession {
            thread_id: thread_id.into(),
            cwd: dir.display().to_string(),
            model: "test".into(),
            active_turn_id: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(dir.to_path_buf()),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store,
            status: ThreadSession::new_status(),
            subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
            writer_lock: None,
        },
        receiver,
    )
}

fn threads_of(session: ThreadSession) -> HashMap<String, ThreadSession> {
    HashMap::from([(session.thread_id.clone(), session)])
}

fn frames(rx: &mut mpsc::Receiver<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        out.push(frame);
    }
    out
}

fn failed_completed_count(frames: &[serde_json::Value]) -> usize {
    frames
        .iter()
        .filter(|f| f["method"] == "turn/completed" && f["params"]["status"] == "failed")
        .count()
}

// ---- T1：RPC receiver 已关闭 -------------------------------------------

/// T1：RPC 路径的 driver 通道已关闭。
///
/// 客户端仍然收到 `turn/started`、opening item、Running 与**成功响应**；
/// 投递失败再补一条 failed `turn/completed`（恰好一条），占位被回滚、状态回
/// Idle，会话不再算忙，且**没有第二个同 id 的 RPC 响应**。
#[tokio::test]
async fn sleep_hardening_rpc_start_with_closed_receiver_settles_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let (session, receiver) = test_session(dir.path(), "thread-a");
    drop(receiver);
    let mut threads = threads_of(session);

    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    let mut output = hub.register(client.clone());
    let mut uploads = crate::image_upload::UploadRegistry::new();

    let result = start_turn_core(
        &mut threads,
        &HashMap::new(),
        &mut uploads,
        &hub,
        &client,
        &serde_json::json!({
            "threadId": "thread-a",
            "input": [{ "type": "text", "text": "hello" }],
        }),
        RequestId::Num(7),
    )
    .await;

    assert!(result.is_ok(), "a dead driver must not kill the server");
    assert!(
        threads["thread-a"].active_turn_id.is_none(),
        "the placeholder must be rolled back"
    );
    assert_eq!(
        *threads["thread-a"].status.lock().unwrap(),
        ThreadStatus::Idle,
        "the session must return to Idle"
    );
    assert!(
        !crate::power_busy::local_busy(&threads),
        "a settled turn must not keep the machine awake"
    );

    let frames = frames(&mut output);
    // 成功响应急在 turn/started 之后、失败完成通知之前。
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.get("id") == Some(&serde_json::json!(7)))
            .count(),
        1,
        "exactly one RPC response for id 7, no second frame after send failed"
    );
    assert_eq!(
        failed_completed_count(&frames),
        1,
        "exactly one failed turn/completed"
    );
}

// ---- T3：身份回滚 -------------------------------------------------------

/// T3：另一个 turn 已经占了位——回滚旧 turn 不得动新 turn 的 id 或 Running 状态。
#[tokio::test]
async fn sleep_hardening_rollback_by_identity_spares_the_new_turn() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut session, _receiver) = test_session(dir.path(), "thread-a");
    session.active_turn_id = Some("new".into());
    *session.status.lock().unwrap() = ThreadStatus::Running;
    let status = Arc::clone(&session.status);
    let mut threads = threads_of(session);
    let hub = crate::broadcast::Broadcaster::new();

    let rolled_back = rollback_turn(&mut threads, &hub, "thread-a", "old").await;

    assert!(!rolled_back, "a foreign turn is not ours to roll back");
    assert_eq!(
        threads["thread-a"].active_turn_id.as_deref(),
        Some("new"),
        "the new turn keeps its placeholder"
    );
    assert_eq!(*status.lock().unwrap(), ThreadStatus::Running);
}

/// 占位仍属于自己时，回滚必须清掉并恢复 Idle（与上一用例成对，避免「永不回滚」也能过）。
#[tokio::test]
async fn sleep_hardening_rollback_clears_its_own_placeholder() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut session, _receiver) = test_session(dir.path(), "thread-a");
    session.active_turn_id = Some("mine".into());
    *session.status.lock().unwrap() = ThreadStatus::Running;
    let status = Arc::clone(&session.status);
    let mut threads = threads_of(session);
    let hub = crate::broadcast::Broadcaster::new();

    let rolled_back = rollback_turn(&mut threads, &hub, "thread-a", "mine").await;

    assert!(rolled_back);
    assert!(threads["thread-a"].active_turn_id.is_none());
    assert_eq!(*status.lock().unwrap(), ThreadStatus::Idle);
}

// ---- failed 通知与完成水位同源 -----------------------------------------

/// 已宣告的 turn 失败时，`turn/completed.completed_at` 必须等于 store 里
/// 落下的 `last_completed_at`——两处同源，客户端按 max 合入才收敛。
#[tokio::test]
async fn sleep_hardening_failed_completion_matches_the_stored_watermark() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut session, _receiver) = test_session(dir.path(), "thread-a");
    session.active_turn_id = Some("turn-1".into());
    *session.status.lock().unwrap() = ThreadStatus::Running;
    let store = Arc::clone(&session.store);
    let mut threads = threads_of(session);

    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    let mut output = hub.register(client);

    fail_announced_turn(&mut threads, &hub, "thread-a", "turn-1", "injected failure").await;

    let frames = frames(&mut output);
    assert_eq!(failed_completed_count(&frames), 1);
    let broadcast_at = frames
        .iter()
        .find(|f| f["method"] == "turn/completed")
        .and_then(|f| f["params"]["completed_at"].as_i64())
        .expect("failed turn/completed carries completed_at");

    let meta = store
        .read_meta("thread-a")
        .expect("read meta")
        .expect("meta exists");
    assert_eq!(
        meta.last_completed_at, broadcast_at,
        "the watermark and the broadcast must be the same instant"
    );
    assert!(threads["thread-a"].active_turn_id.is_none());
    assert_eq!(
        *threads["thread-a"].status.lock().unwrap(),
        ThreadStatus::Idle
    );
}

// ---- T7：通知/落盘失败不阻止回滚 ---------------------------------------

/// T7：store 不可写（thread meta 已被删掉/目录不可用）时落盘会失败，但内存
/// 回滚仍必须完成，原始错误不被覆盖。
#[tokio::test]
async fn sleep_hardening_store_failure_still_rolls_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut session, _receiver) = test_session(dir.path(), "thread-missing");
    session.active_turn_id = Some("turn-1".into());
    *session.status.lock().unwrap() = ThreadStatus::Running;
    let status = Arc::clone(&session.status);
    let mut threads = threads_of(session);
    // 让 `mark_turn_completed` 的读-改-写找不到 meta。
    std::fs::remove_dir_all(dir.path().join(".yi-agent")).unwrap();

    let hub = crate::broadcast::Broadcaster::new();
    fail_announced_turn(&mut threads, &hub, "thread-missing", "turn-1", "boom").await;

    assert!(
        threads["thread-missing"].active_turn_id.is_none(),
        "a failed watermark write must not stop the rollback"
    );
    assert_eq!(*status.lock().unwrap(), ThreadStatus::Idle);
}

/// 广播无人订阅/通道关闭也不能跳过回滚。`write_notification` 对状态机帧会
/// 忽略 `Closed`，这里显式验证结果：占位仍被清掉。
#[tokio::test]
async fn sleep_hardening_notify_failure_still_rolls_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut session, _receiver) = test_session(dir.path(), "thread-a");
    session.active_turn_id = Some("turn-1".into());
    *session.status.lock().unwrap() = ThreadStatus::Running;
    let status = Arc::clone(&session.status);
    let mut threads = threads_of(session);

    let hub = crate::broadcast::Broadcaster::new();
    // 注册又注销：广播时没有可靠订阅者。
    let client = crate::broadcast::ClientId::local();
    drop(hub.register(client.clone()));
    hub.unregister(&client);

    fail_announced_turn(&mut threads, &hub, "thread-a", "turn-1", "boom").await;

    assert!(threads["thread-a"].active_turn_id.is_none());
    assert_eq!(*status.lock().unwrap(), ThreadStatus::Idle);
}

// ---- T2：宣告之后三个出站阶段失败，都走同一失败收口 ----------------

/// 直接调 inner 并注入阶段失败；返回 (result, frames)。
///
/// inner 是 `start_turn_core` 的全部原参数再加 `fault`，生产包装永远传 `None`，
/// 所以这里测的仍是生产那一段 async 流程。
async fn run_injected(
    dir: &Path,
    stage: StartStage,
) -> (
    anyhow::Result<Option<String>>,
    HashMap<String, ThreadSession>,
    Vec<serde_json::Value>,
) {
    let (session, _receiver) = test_session(dir, "thread-a");
    let mut threads = threads_of(session);
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    let mut output = hub.register(client.clone());
    let mut uploads = crate::image_upload::UploadRegistry::new();

    let result = start_turn_core_inner(
        &mut threads,
        &HashMap::new(),
        &mut uploads,
        &hub,
        &client,
        &serde_json::json!({
            "threadId": "thread-a",
            "input": [{ "type": "text", "text": "hello" }],
        }),
        RequestId::Num(7),
        Some(stage),
    )
    .await;
    let frames = frames(&mut output);
    (result, threads, frames)
}

/// 三个注入点都必须：原始错误向上传播、恰好一条 failed 完成通知、按身份回滚
/// 到 Idle，且绝不出现第二个同 id 的成功响应。
async fn assert_stage_failure_settles(stage: StartStage, expected: &str) {
    let dir = tempfile::TempDir::new().unwrap();
    let (result, threads, frames) = run_injected(dir.path(), stage).await;

    let error = result.expect_err("an injected stage failure must propagate");
    assert!(
        error.to_string().contains(expected),
        "original error must not be replaced, got {error}"
    );
    assert!(
        threads["thread-a"].active_turn_id.is_none(),
        "the placeholder must be rolled back"
    );
    assert_eq!(
        *threads["thread-a"].status.lock().unwrap(),
        ThreadStatus::Idle
    );
    assert_eq!(
        failed_completed_count(&frames),
        1,
        "exactly one failed turn/completed"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.get("id") == Some(&serde_json::json!(7)))
            .count(),
        0,
        "a stage failure before the response must not emit a success response"
    );
    // turn/started 一定已经发出（注入都在宣告之后）。
    assert_eq!(
        frames
            .iter()
            .filter(|f| f["method"] == "turn/started")
            .count(),
        1
    );
}

#[tokio::test]
async fn sleep_hardening_opening_failure_settles_the_turn() {
    assert_stage_failure_settles(StartStage::Opening, "injected opening failure").await;
}

#[tokio::test]
async fn sleep_hardening_running_failure_settles_the_turn() {
    assert_stage_failure_settles(StartStage::Running, "injected running failure").await;
}

#[tokio::test]
async fn sleep_hardening_response_failure_settles_the_turn() {
    assert_stage_failure_settles(StartStage::Response, "injected response failure").await;
}

/// 生产包装（`fault = None`）在一切正常时不得发 failed、不得回滚：prompt 投递
/// 成功即由 driver 接手。
#[tokio::test]
async fn sleep_hardening_successful_start_hands_off_without_failure() {
    let dir = tempfile::TempDir::new().unwrap();
    let (session, mut receiver) = test_session(dir.path(), "thread-a");
    let mut threads = threads_of(session);
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    let mut output = hub.register(client.clone());
    let mut uploads = crate::image_upload::UploadRegistry::new();

    let result = start_turn_core(
        &mut threads,
        &HashMap::new(),
        &mut uploads,
        &hub,
        &client,
        &serde_json::json!({
            "threadId": "thread-a",
            "input": [{ "type": "text", "text": "hello" }],
        }),
        RequestId::Num(7),
    )
    .await
    .expect("a live driver accepts the prompt");
    let turn_id = result.expect("the turn started");

    assert!(
        threads[&turn_id].active_turn_id.is_some(),
        "a delivered turn keeps its placeholder for the driver"
    );
    assert!(
        receiver.try_recv().is_ok(),
        "the driver received the prompt"
    );
    let frames = frames(&mut output);
    assert_eq!(failed_completed_count(&frames), 0, "no failure on success");
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.get("id") == Some(&serde_json::json!(7)))
            .count(),
        1
    );
}
