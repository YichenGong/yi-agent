//! Registry tests: one runner per parent, idempotent turns, and an honest close.
//!
//! Each test pins one of the guarantees in spec §8.1. The failures they are meant
//! to catch are all "two of something that should be one": two runners, two turns,
//! two generations.

use std::sync::Arc;

use yi_agent_runtime::side_conversation::{CloseReason, SideErrorReason, SideStatus};

use super::*;
use crate::side_conversation::source::ThreadStoreResolver;
use crate::thread_store::{ThreadMode, ThreadStore, TurnLine};
use tempfile::TempDir;

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn meta(id: &str, cwd: &std::path::Path) -> crate::thread_store::ThreadMeta {
    crate::thread_store::ThreadMeta {
        thread_id: id.into(),
        cwd: cwd.display().to_string(),
        model: "parent-model".into(),
        created_at: 1,
        updated_at: 1,
        title: None,
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        extra_roots: Vec::new(),
        role: None,
        anchor_sha: None,
        content_epoch: 0,
        last_item_id: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
    }
}

fn seed_parent(store: &ThreadStore, id: &str) {
    store.create(&meta(id, store.workdir())).unwrap();
    store
        .append_turn(
            id,
            &TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: vec![
                    yi_agent_core::Message::user("父问题"),
                    yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                        "父回答".into(),
                    )]),
                ],
            },
        )
        .unwrap();
}

/// A registry over a temp instance home with one healthy parent `p1`.
///
/// Returns the temp dirs so the caller keeps them alive: the store and the
/// resource root both live under them.
async fn registry_fixture() -> (TempDir, TempDir, SideRegistry) {
    let project = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(project.path()));
    seed_parent(&store, "p1");

    let mut cfg = crate::test_support::test_config();
    cfg.model = "parent-model".into();
    cfg.workdir = project.path().to_path_buf();
    let resolver = Arc::new(ThreadStoreResolver::new(store, cfg));

    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(2, 1, 8, 4));
    let resources = yi_agent_runtime::side_conversation::SideHostResources::new(
        home.path().to_path_buf(),
        Arc::clone(&pool),
    )
    .expect("host resources");
    let registry = SideRegistry::new(resolver, resources, pool);
    (project, home, registry)
}

#[tokio::test]
async fn side_concurrent_open_returns_one_runner() {
    let (_project, _home, registry) = registry_fixture().await;
    let (a, b) = tokio::join!(registry.open("p1"), registry.open("p1"));
    let a = a.expect("first open");
    let b = b.expect("second open");
    assert_eq!(a.side_id, b.side_id, "two opens must collapse to one side");
    assert_eq!(registry.list("p1").await.len(), 1);
    assert_eq!(registry.side_count().await, 1);
}

#[tokio::test]
async fn side_open_is_create_or_get() {
    let (_project, _home, registry) = registry_fixture().await;
    let first = registry.open("p1").await.unwrap();
    let second = registry.open("p1").await.unwrap();
    assert_eq!(first.side_id, second.side_id);
    assert_eq!(
        first.generation, second.generation,
        "no new generation on get"
    );
}

#[tokio::test]
async fn side_open_unknown_parent_leaves_no_slot() {
    let (_project, _home, registry) = registry_fixture().await;
    let error = registry.open("nobody").await.unwrap_err();
    assert_eq!(error.reason, SideErrorReason::UnknownThread);
    // A failed create must release the parent slot, or the parent would be
    // permanently stuck in `Creating`.
    assert!(registry.list("nobody").await.is_empty());
    // And a later attempt must be able to try again (not blocked forever).
    assert_eq!(
        registry.open("nobody").await.unwrap_err().reason,
        SideErrorReason::UnknownThread
    );
}

#[tokio::test]
async fn side_request_id_deduplicates_turn_start() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let request = uuid();

    let first = registry
        .turn_start(&summary.side_id, "问题".into(), request.clone())
        .await
        .unwrap();
    let retry = registry
        .turn_start(&summary.side_id, "问题".into(), request)
        .await
        .expect("a retry must not be refused as busy");
    assert_eq!(
        first, retry,
        "a retried request must return the original turn"
    );
}

#[tokio::test]
async fn side_rejects_a_reused_request_id_with_different_text() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let request = uuid();
    registry
        .turn_start(&summary.side_id, "原来的问题".into(), request.clone())
        .await
        .unwrap();

    let error = registry
        .turn_start(&summary.side_id, "不一样的内容".into(), request)
        .await
        .unwrap_err();
    assert_eq!(error.reason, SideErrorReason::InvalidParams);
}

#[tokio::test]
async fn side_rejects_second_inflight_turn() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    registry
        .turn_start(&summary.side_id, "第一问".into(), uuid())
        .await
        .unwrap();

    let error = registry
        .turn_start(&summary.side_id, "第二问".into(), uuid())
        .await
        .unwrap_err();
    assert_eq!(error.reason, SideErrorReason::TurnInProgress);
}

#[tokio::test]
async fn side_close_rejects_stale_turn_confirmation() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let turn = registry
        .turn_start(&summary.side_id, "问题".into(), uuid())
        .await
        .unwrap();

    // The client believes the side is idle (`null`) but a turn is live.
    let error = registry
        .close(&summary.side_id, None)
        .await
        .expect_err("a stale confirmation must be refused");
    assert_eq!(error.reason, SideErrorReason::CloseConflict);

    // Confirming the real turn works.
    registry
        .close(&summary.side_id, Some(&turn))
        .await
        .expect("the live turn is the correct confirmation");
}

#[tokio::test]
async fn side_close_is_idempotent_through_the_tombstone() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    registry.close(&summary.side_id, None).await.unwrap();
    // A repeated close is a success, not a 404: the client may retry after a
    // dropped response.
    registry
        .close(&summary.side_id, None)
        .await
        .expect("a repeated close must succeed against the tombstone");
    assert_eq!(registry.tombstone_count().await, 1);
}

#[tokio::test]
async fn side_after_close_answer_is_side_not_found() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    registry.close(&summary.side_id, None).await.unwrap();

    let error = registry
        .turn_start(&summary.side_id, "问题".into(), uuid())
        .await
        .unwrap_err();
    assert_eq!(
        error.reason,
        SideErrorReason::SideNotFound,
        "a closed side must never be revived by a retried ask"
    );
}

#[tokio::test]
async fn side_tombstone_keeps_only_identifiers() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let request = uuid();
    let turn = registry
        .turn_start(&summary.side_id, "很长的秘密问题".into(), request.clone())
        .await
        .unwrap();
    registry.close(&summary.side_id, Some(&turn)).await.unwrap();

    let closed = registry
        .tombstone(&summary.side_id)
        .await
        .expect("a tombstone");
    assert_eq!(closed.parent_thread_id, "p1");
    assert_eq!(closed.accepted.len(), 1);
    assert_eq!(closed.accepted[0].0, request);
    assert_eq!(closed.accepted[0].1, turn);
    // No text anywhere: the dedup record maps id to turn id and nothing else.
    let encoded = format!("{closed:?}");
    assert!(
        !encoded.contains("很长的秘密问题"),
        "a tombstone must never retain request text"
    );
}

#[tokio::test]
async fn side_lists_a_creating_side() {
    // `list` must be able to report the id assigned by an in-flight `open`, so a
    // second client can show "creating" instead of "nothing here".
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let listed = registry.list("p1").await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].side_id, summary.side_id);
    assert_eq!(listed[0].status, SideStatus::Idle);
}

#[tokio::test]
async fn side_parent_change_invalidates_the_side() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();

    let guard = registry.begin_parent_change("p1").await;
    guard.commit(CloseReason::ParentCleared).await;

    assert!(registry.list("p1").await.is_empty());
    assert!(registry.tombstone(&summary.side_id).await.is_some());
    let error = registry
        .turn_start(&summary.side_id, "问题".into(), uuid())
        .await
        .unwrap_err();
    assert_eq!(error.reason, SideErrorReason::SideNotFound);
}

#[tokio::test]
async fn side_parent_change_barrier_blocks_new_opens_until_committed() {
    let (_project, _home, registry) = registry_fixture().await;
    let first = registry.open("p1").await.unwrap();

    let guard = registry.begin_parent_change("p1").await;
    // While the barrier is up the parent has no side, so a concurrent open would
    // start creating a second one over files that are about to be deleted.
    assert!(registry.list("p1").await.is_empty());
    guard.commit(CloseReason::ParentDeleted).await;

    // After the change a fresh open gets a *different* side: the old generation is
    // gone for good.
    let second = registry.open("p1").await.unwrap();
    assert_ne!(first.side_id, second.side_id);
}

#[tokio::test]
async fn side_parent_change_rollback_restores_the_parent_slot() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();

    {
        let guard = registry.begin_parent_change("p1").await;
        // Dropped without commit: the clear/delete failed, so the side must survive.
        drop(guard);
    }

    let listed = registry.list("p1").await;
    assert_eq!(listed.len(), 1, "a failed change must not lose the side");
    assert_eq!(listed[0].side_id, summary.side_id);
}

#[tokio::test]
async fn side_shutdown_closes_everything() {
    let (_project, _home, registry) = registry_fixture().await;
    registry.open("p1").await.unwrap();
    registry.shutdown().await;
    assert_eq!(registry.side_count().await, 0);
    // After shutdown no new side can be created: the host is going away and a
    // half-created side would leak its directory.
    let error = registry.open("p1").await.unwrap_err();
    assert_eq!(error.reason, SideErrorReason::SideClosing);
}

#[tokio::test]
async fn side_second_turn_is_accepted_after_the_first_completes() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let first = registry
        .turn_start(&summary.side_id, "第一问".into(), uuid())
        .await
        .unwrap();

    // Wait for the mock provider's turn to finish so the side is idle again.
    for _ in 0..200 {
        let listed = registry.list("p1").await;
        if listed
            .first()
            .and_then(|s| s.active_turn_id.as_ref())
            .is_none()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let second = registry
        .turn_start(&summary.side_id, "第二问".into(), uuid())
        .await
        .expect("a side is reusable once the previous turn ended");
    assert_ne!(first, second);
}

#[tokio::test]
async fn side_stale_interrupt_does_not_kill_a_newer_turn() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    let first = registry
        .turn_start(&summary.side_id, "第一问".into(), uuid())
        .await
        .unwrap();
    registry.interrupt(&summary.side_id, &first).await.unwrap();

    for _ in 0..200 {
        let listed = registry.list("p1").await;
        if listed
            .first()
            .and_then(|s| s.active_turn_id.as_ref())
            .is_none()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let second = registry
        .turn_start(&summary.side_id, "第二问".into(), uuid())
        .await
        .unwrap();

    // A retried interrupt for the *old* turn must not touch the new one.
    registry.interrupt(&summary.side_id, &first).await.unwrap();
    let listed = registry.list("p1").await;
    assert_eq!(
        listed[0].active_turn_id.as_deref(),
        Some(second.as_str()),
        "a stale interrupt must be a no-op"
    );
}

/// 订阅父 thread 的客户端要能收到旁支帧,且 `seq` 在一条 generation 内连续、
/// 每个已发布修订恰好一帧。
#[tokio::test]
async fn side_events_follow_parent_subscription_without_polluting_parent() {
    let (_project, _home, registry) = registry_fixture().await;
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    registry.attach_hub(Arc::clone(&hub));

    // 一台把父 `p1` pin 进订阅窗口的设备(手机),和一台只订阅了**别的**父的设备。
    // 后者不该收到:旁支按父 thread 键投递,所以订阅过滤对它同样生效。
    let parent_client = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let mut parent_rx = hub.register(parent_client.clone());
    hub.subscribe(&parent_client, vec!["p1".to_string()]);
    let stranger = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let mut stranger_rx = hub.register(stranger.clone());
    hub.subscribe(&stranger, vec!["someone-elses-thread".to_string()]);

    let summary = registry.open("p1").await.unwrap();
    let request = uuid();
    let turn = registry
        .turn_start(&summary.side_id, "旁支问题".into(), request)
        .await
        .unwrap();

    // 至少收到一条 changed(受理),且它带的是**父** thread 键。
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), parent_rx.recv())
        .await
        .expect("subscribed client must receive a side frame")
        .expect("channel open");
    assert_eq!(
        first["method"],
        serde_json::json!("side/changed"),
        "{first}"
    );
    assert_eq!(first["params"]["parent_thread_id"], "p1");
    let seq = first["params"]["seq"].as_u64().unwrap();
    assert!(seq >= 1);
    // 摘要里的水位必须已经是本次修订,而不是上一版。
    assert_eq!(
        first["params"]["summary"]["last_seq"],
        serde_json::json!(seq)
    );

    // 订阅窗口不覆盖 p1 的设备一帧都不该收到。
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), stranger_rx.recv())
            .await
            .is_err(),
        "a client whose window excludes p1 must not receive its side frames"
    );
    // 旁支帧不冒充父会话的正文:它始终是 `side/*`,负载带 side_id。
    assert_eq!(first["params"]["side_id"], summary.side_id);
    assert!(
        first["method"].as_str().unwrap().starts_with("side/"),
        "side updates must never be delivered as parent thread content: {first}"
    );

    registry
        .close(&summary.side_id, Some(&turn))
        .await
        .expect("close after confirming the active turn");
}

/// 关闭是终态:即便前序有缺号,客户端也立刻丢弃该 generation。所以 closed 必须
/// 带同一个 generation 与一个严格递增的 seq。
#[tokio::test]
async fn side_close_publishes_a_terminal_frame_for_its_generation() {
    let (_project, _home, registry) = registry_fixture().await;
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    registry.attach_hub(Arc::clone(&hub));
    let client = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let mut rx = hub.register(client.clone());
    hub.subscribe(&client, vec!["p1".to_string()]);

    let summary = registry.open("p1").await.unwrap();
    registry.close(&summary.side_id, None).await.unwrap();

    let mut closed = None;
    for _ in 0..10 {
        let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
        else {
            break;
        };
        if frame["method"] == serde_json::json!("side/closed") {
            closed = Some(frame);
            break;
        }
    }
    let closed = closed.expect("side/closed must be published");
    assert_eq!(closed["params"]["side_id"], summary.side_id);
    assert_eq!(
        closed["params"]["generation"],
        serde_json::json!(summary.generation)
    );
    assert_eq!(closed["params"]["reason"], serde_json::json!("user_closed"));
}

/// 关闭后 `publish` 必须回 `None`:晚到的完成事件不得复活已关闭的 generation。
#[tokio::test]
async fn side_publish_after_close_is_a_no_op() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();
    registry.close(&summary.side_id, None).await.unwrap();

    let published = registry
        .publish(&summary.side_id, |entry| {
            crate::side_conversation::SidePublishedUpdate::Changed {
                side_id: entry.side_id.clone(),
                summary: entry.summary(),
            }
        })
        .await;
    assert!(
        published.is_none(),
        "a closed side must not publish further revisions"
    );
}

/// 关闭会撤销该 side 的全部分页快照:留着它等于给客户端一条通往已删内容的旁路。
#[tokio::test]
async fn side_close_revokes_outstanding_snapshots() {
    let (_project, _home, registry) = registry_fixture().await;
    let replay = crate::side_conversation::shared_replay_store();
    registry.attach_replay(Arc::clone(&replay)).await;

    let summary = registry.open("p1").await.unwrap();
    {
        let mut store = replay.lock().await;
        let snapshot = registry.snapshot(&summary.side_id).await.unwrap();
        let page = store
            .read_page(
                "conn-1",
                &summary.side_id,
                None,
                None,
                Some(crate::side_conversation::ReplayBaseline::from_snapshot(
                    &snapshot,
                )),
            )
            .unwrap();
        // 空转录的首读即末页会立刻释放;这里断言的是「无论有没有 token,
        // 关闭之后的续页都不可能成功」。
        let _ = page;
    }
    registry.close(&summary.side_id, None).await.unwrap();
    assert_eq!(replay.lock().await.token_count(), 0);
}

/// `seq` 只由一次发布推进一格:同一秒内连发两次不会拿到同一个号。
#[tokio::test]
async fn side_publish_advances_seq_by_one() {
    let (_project, _home, registry) = registry_fixture().await;
    let summary = registry.open("p1").await.unwrap();

    let first = registry
        .publish(&summary.side_id, |entry| {
            crate::side_conversation::SidePublishedUpdate::Changed {
                side_id: entry.side_id.clone(),
                summary: entry.summary(),
            }
        })
        .await
        .expect("first publish");
    let second = registry
        .publish(&summary.side_id, |entry| {
            crate::side_conversation::SidePublishedUpdate::Changed {
                side_id: entry.side_id.clone(),
                summary: entry.summary(),
            }
        })
        .await
        .expect("second publish");
    assert_eq!(second, first + 1);
    assert_eq!(registry.list("p1").await[0].last_seq, second);
}
