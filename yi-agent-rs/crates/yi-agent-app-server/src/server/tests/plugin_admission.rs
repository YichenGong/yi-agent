//! 插件准入在 app-server 侧的接线（T15 Step 3）。
//!
//! 这里测的是**宿主行为**，不是 `Admission` 自己的语义（那在 `yi-agent-plugins`
//! 里已有覆盖）。三条要守住的性质：
//!
//! 1. 插件会话的回合在排空期间被拒，且**不留下** `active_turn_id`——否则这个
//!    会话会永远报 `turn_in_progress`，直到进程重启。
//! 2. 回合正常结束（`TurnEvent::Finished`）时 permit 被释放，屏障看得见归零。
//! 3. 会话被移除 / 重建时，挂着的 permit 一并消失——这是防"卸载永远排空不完"
//!    的最后一道保险。
//!
//! 第 1 条是"取准入必须在占位之前"的直接原因：`admit_turn` 在排空期间返回
//! `Err`，若先写了 `active_turn_id` 再取 permit，那次失败就会把会话卡死。

use super::*;
use yi_agent_plugins::{Admission, Binding};

fn binding() -> Binding {
    Binding {
        host_id: "host-test".to_string(),
        release: yi_agent_plugins::ReleaseId {
            plugin: yi_agent_plugins::PluginId::parse("demo").expect("plugin id"),
            version: "1.0.0".to_string(),
            target: "aarch64-apple-darwin".to_string(),
            sha256: "0".repeat(64),
        },
        activation: 1,
        project: yi_agent_plugins::ProjectId::parse("/tmp/proj").expect("project id"),
        grant: 1,
    }
}

fn admission() -> (tempfile::TempDir, Admission) {
    let home = tempfile::tempdir().expect("tempdir");
    let admission = Admission::new(home.path());
    (home, admission)
}

/// 排空期间插件回合被拒，且会话没被占住。
#[tokio::test]
async fn draining_refuses_plugin_turn_without_claiming_it() {
    let (_home, admission) = admission();
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let pending = HashMap::new();
    let mut uploads = crate::image_upload::UploadRegistry::default();

    // 会话必须是"已存在且空闲"的——否则会先撞 UnknownThread，测不到准入。
    let project = tempfile::tempdir().expect("project");
    threads.insert(
        "t1".to_string(),
        super::tick_test_session(project.path(), "t1", crate::protocol::ThreadStatus::Idle),
    );

    admission.begin_drain().expect("begin drain");

    let params = serde_json::json!({
        "threadId": "t1",
        "input": [{ "type": "text", "text": "hello" }],
    });
    let error = match prepare_turn_core(
        &mut threads,
        &pending,
        &mut uploads,
        &params,
        Some((&admission, binding())),
    )
    .await
    {
        Ok(_) => panic!("draining must refuse a new plugin turn"),
        Err(error) => error,
    };

    assert!(
        matches!(error, TurnPrepareError::NotAdmitted(_)),
        "want NotAdmitted, got {}",
        turn_prepare_rpc_error(error).message
    );
    assert!(
        threads["t1"].active_turn_id.is_none(),
        "a refused turn must not leave the session claimed"
    );
    assert!(threads["t1"].turn_permit.is_none());
    assert_eq!(admission.live_before_barrier_permits(), 0);
}

/// 准入正常时拿到 permit，且它随 `turn_permit` 一起被清掉即释放。
#[tokio::test]
async fn permit_is_released_when_session_clears_the_turn() {
    let (_home, admission) = admission();
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let pending = HashMap::new();
    let mut uploads = crate::image_upload::UploadRegistry::default();
    let project = tempfile::tempdir().expect("project");
    threads.insert(
        "t1".to_string(),
        super::tick_test_session(project.path(), "t1", crate::protocol::ThreadStatus::Idle),
    );

    let params = serde_json::json!({
        "threadId": "t1",
        "input": [{ "type": "text", "text": "hello" }],
    });
    let prepared = prepare_turn_core(
        &mut threads,
        &pending,
        &mut uploads,
        &params,
        Some((&admission, binding())),
    )
    .await
    .expect("admission is open");

    assert_eq!(
        admission.live_before_barrier_permits(),
        1,
        "the turn must be visible to the drain barrier"
    );

    // 模拟"回合结束"那条路径：活跃 turn 与 permit 同时清空。
    let session = threads.get_mut(&prepared.thread_id).expect("session");
    crate::server::transport::complete_session_turn(session, "stale-turn");
    assert_eq!(
        admission.live_before_barrier_permits(),
        1,
        "stale completion must preserve the permit"
    );
    crate::server::transport::complete_session_turn(session, &prepared.turn_id);

    assert_eq!(
        admission.live_before_barrier_permits(),
        0,
        "clearing the slot must release the permit"
    );
}

/// 移除整个会话同样释放 permit（重建 / 关闭项目的路径）。
#[tokio::test]
async fn removing_the_session_releases_the_permit() {
    let (_home, admission) = admission();
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let pending = HashMap::new();
    let mut uploads = crate::image_upload::UploadRegistry::default();
    let project = tempfile::tempdir().expect("project");
    threads.insert(
        "t1".to_string(),
        super::tick_test_session(project.path(), "t1", crate::protocol::ThreadStatus::Idle),
    );

    let params = serde_json::json!({
        "threadId": "t1",
        "input": [{ "type": "text", "text": "hello" }],
    });
    prepare_turn_core(
        &mut threads,
        &pending,
        &mut uploads,
        &params,
        Some((&admission, binding())),
    )
    .await
    .expect("admission is open");
    assert_eq!(admission.live_before_barrier_permits(), 1);

    threads.remove("t1");

    assert_eq!(
        admission.live_before_barrier_permits(),
        0,
        "dropping the session must drop the permit"
    );
}

/// 不传准入的普通回合：即使正在排空也照常开，且不占屏障名额。
///
/// 这是"用户聊天不该被插件维护卡住"的契约。
#[tokio::test]
async fn plain_turn_ignores_draining() {
    let (_home, admission) = admission();
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    let pending = HashMap::new();
    let mut uploads = crate::image_upload::UploadRegistry::default();
    let project = tempfile::tempdir().expect("project");
    threads.insert(
        "t1".to_string(),
        super::tick_test_session(project.path(), "t1", crate::protocol::ThreadStatus::Idle),
    );

    admission.begin_drain().expect("begin drain");

    let params = serde_json::json!({
        "threadId": "t1",
        "input": [{ "type": "text", "text": "hello" }],
    });
    prepare_turn_core(&mut threads, &pending, &mut uploads, &params, None)
        .await
        .expect("a plain turn is none of the plugin's business");

    assert!(threads["t1"].active_turn_id.is_some());
    assert!(threads["t1"].turn_permit.is_none());
    assert_eq!(admission.live_before_barrier_permits(), 0);
}
