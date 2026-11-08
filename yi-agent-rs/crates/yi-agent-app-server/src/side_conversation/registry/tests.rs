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
        plugin_bindings: Vec::new(),
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
    registry_fixture_with_provider(None).await
}

struct ProviderResolver {
    inner: ThreadStoreResolver,
    provider: Arc<dyn yi_agent_core::Provider>,
}

impl super::SideParentResolver for ProviderResolver {
    fn resolve<'a>(
        &'a self,
        parent: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        yi_agent_runtime::side_conversation::SideResult<
            yi_agent_runtime::side_conversation::EffectiveSideSource,
        >,
    > {
        Box::pin(async move {
            let mut source = self.inner.resolve(parent).await?;
            source.provider = Arc::clone(&self.provider);
            Ok(source)
        })
    }
}

async fn registry_fixture_with_provider(
    provider: Option<Arc<dyn yi_agent_core::Provider>>,
) -> (TempDir, TempDir, SideRegistry) {
    let project = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(project.path()));
    seed_parent(&store, "p1");

    let mut cfg = crate::test_support::test_config();
    cfg.model = "parent-model".into();
    cfg.workdir = project.path().to_path_buf();
    let inner = ThreadStoreResolver::new(store, cfg, Default::default());
    let resolver: Arc<dyn super::SideParentResolver> = match provider {
        Some(provider) => Arc::new(ProviderResolver { inner, provider }),
        None => Arc::new(inner),
    };

    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(2, 1, 8, 4));
    let resources = yi_agent_runtime::side_conversation::SideHostResources::new(
        home.path().to_path_buf(),
        Arc::clone(&pool),
    )
    .expect("host resources");
    let registry = SideRegistry::new(resolver, resources, pool);
    (project, home, registry)
}

struct ReplyProvider {
    fail: bool,
}

#[async_trait::async_trait]
impl yi_agent_core::Provider for ReplyProvider {
    async fn call_stream(
        &self,
        _request: yi_agent_core::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::ProviderEvent>,
        yi_agent_core::ProviderError,
    > {
        if self.fail {
            return Err(yi_agent_core::ProviderError::InvalidRequest(
                "private response".into(),
            ));
        }
        Ok(Box::pin(futures::stream::iter(vec![
            yi_agent_core::ProviderEvent::TextDelta("旁问回答".into()),
            yi_agent_core::ProviderEvent::Stop {
                reason: yi_agent_core::StopReason::EndTurn,
            },
        ])))
    }
}

async fn published_turn(fail: bool) -> (serde_json::Value, Vec<serde_json::Value>, SideSnapshot) {
    let (_project, _home, registry) =
        registry_fixture_with_provider(Some(Arc::new(ReplyProvider { fail }))).await;
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    registry.attach_hub(Arc::clone(&hub));
    let client = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let mut rx = hub.register(client.clone());
    hub.subscribe(&client, vec!["p1".into()]);
    let side = registry.open("p1").await.unwrap();
    let turn = registry
        .turn_start(&side.side_id, "旁问问题".into(), uuid())
        .await
        .unwrap();
    let (completed, items) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut items = Vec::new();
        loop {
            let frame = rx.recv().await.expect("broadcast remains open");
            if frame["method"] == "side/itemUpdated" {
                items.push(frame["params"]["item"].clone());
            }
            if frame["method"] == "side/turnCompleted" && frame["params"]["turn_id"] == turn {
                break (frame["params"].clone(), items);
            }
        }
    })
    .await
    .expect("turn must complete");
    let snapshot = registry.snapshot(&side.side_id).await.unwrap();
    registry.shutdown().await;
    (completed, items, snapshot)
}

#[tokio::test]
async fn side_provider_reply_reaches_broadcast_and_snapshot() {
    let (completed, items, snapshot) = published_turn(false).await;
    assert_eq!(completed["status"], "idle");
    for (kind, expected) in [("userMessage", "旁问问题"), ("agentMessage", "旁问回答")] {
        assert!(
            items
                .iter()
                .any(|item| item["type"] == kind && item["text"] == expected),
            "missing {kind}: {items:?}"
        );
    }
    assert!(snapshot.items.iter().any(|item| matches!(
        &item.content,
        yi_agent_runtime::side_conversation::SideItemContent::AgentMessage { text } if text == "旁问回答"
    )));
}

#[tokio::test]
async fn side_provider_failure_publishes_feedback_instead_of_idle() {
    let (completed, items, _snapshot) = published_turn(true).await;
    assert!(items.iter().any(|item| item["type"] == "userMessage"));
    assert_eq!(completed["status"], "failed");
    assert!(
        completed["error"]
            .as_str()
            .is_some_and(|text| !text.is_empty() && !text.contains("private response"))
    );
}

#[tokio::test]
async fn side_transcript_refusal_does_not_start_or_accept_a_turn() {
    let (_project, _home, registry) =
        registry_fixture_with_provider(Some(Arc::new(ReplyProvider { fail: false }))).await;
    let side = registry.open("p1").await.unwrap();
    let entry = registry.entry(&side.side_id).await.unwrap();
    entry.lock().await.transcript = SideTranscript::new(1, 1);
    let request = uuid();
    assert!(
        registry
            .turn_start(&side.side_id, "问题".into(), request.clone())
            .await
            .is_err()
    );
    {
        let guard = entry.lock().await;
        assert!(
            guard.runner.active_turn_id().is_none(),
            "refused transcript must not leave a running turn"
        );
        assert!(
            guard.ledger.lookup(&request, "问题").unwrap().is_none(),
            "refusal must not accept the request"
        );
        assert!(guard.transcript.items().is_empty());
    }
    registry.shutdown().await;
}

#[tokio::test]
async fn side_accepted_question_is_in_snapshot_and_published_once() {
    let (_project, _home, registry) =
        registry_fixture_with_provider(Some(Arc::new(ReplyProvider { fail: false }))).await;
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    registry.attach_hub(Arc::clone(&hub));
    let client = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    let mut rx = hub.register(client.clone());
    hub.subscribe(&client, vec!["p1".to_string()]);
    let summary = registry.open("p1").await.unwrap();
    let request = uuid();
    let turn = registry
        .turn_start(&summary.side_id, "旁支问题".into(), request.clone())
        .await
        .unwrap();
    let snapshot = registry.snapshot(&summary.side_id).await.unwrap();
    assert!(
        snapshot.items.iter().any(|item| matches!(
            &item.content,
            yi_agent_runtime::side_conversation::SideItemContent::UserMessage { text }
                if text == "旁支问题" && item.turn_id == turn
        )),
        "an accepted question must be readable before the provider replies"
    );
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let frame = rx.recv().await.expect("channel open");
            if frame["method"] == "side/itemUpdated"
                && frame["params"]["item"]["type"] == "userMessage"
            {
                break frame;
            }
        }
    })
    .await
    .expect("the accepted question must be broadcast");
    assert_eq!(frame["params"]["item"]["text"], "旁支问题");
    assert_eq!(frame["params"]["turn_id"], turn);
    assert_eq!(
        registry
            .turn_start(&summary.side_id, "旁支问题".into(), request)
            .await
            .unwrap(),
        turn
    );
    let snapshot = registry.snapshot(&summary.side_id).await.unwrap();
    assert_eq!(
        snapshot
            .items
            .iter()
            .filter(|item| matches!(
                &item.content,
                yi_agent_runtime::side_conversation::SideItemContent::UserMessage { .. }
            ))
            .count(),
        1,
        "retrying must not duplicate the question"
    );
    registry.shutdown().await;
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

/// 父正在跑一轮时开旁支，父的持久文件必须**字节不变**。
///
/// spec §8.2/§12.2：`open` 只读父的持久状态（外加写 partial 的那一份），
/// 不 resume、不 fork、不 append。这条用字节比对来钉：任何"顺手把父的状态
/// 规范化一下"的改动都会让父的 jsonl/meta 变化，而客户端会把它当成一轮新内容。
#[tokio::test]
async fn side_open_during_a_parent_turn_does_not_touch_parent_files() {
    let project = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(project.path()));
    seed_parent(&store, "p1");
    // 父此刻正跑到一半：磁盘上有一个未收尾的 partial。
    store
        .write_partial(
            "p1",
            &crate::thread_store::PartialTurn {
                turn_id: "inflight".into(),
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-inflight".into(),
                    text: "父的第二个问题".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                messages: vec![
                    yi_agent_core::Message::user("父问题"),
                    yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                        "父回答".into(),
                    )]),
                    yi_agent_core::Message::user("父的第二个问题"),
                ],
                usage: None,
                usage_total: None,
            },
        )
        .unwrap();

    let log_path = store.log_path("p1");
    let meta_path = store.meta_path("p1");
    let partial_path = store.partial_path("p1");
    let before = (
        std::fs::read(&log_path).unwrap(),
        std::fs::read(&meta_path).unwrap(),
        std::fs::read(&partial_path).unwrap(),
    );

    let mut cfg = crate::test_support::test_config();
    cfg.model = "parent-model".into();
    cfg.workdir = project.path().to_path_buf();
    let resolver = Arc::new(ThreadStoreResolver::new(
        Arc::clone(&store),
        cfg,
        Default::default(),
    ));
    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(2, 1, 8, 4));
    let resources = yi_agent_runtime::side_conversation::SideHostResources::new(
        home.path().to_path_buf(),
        Arc::clone(&pool),
    )
    .expect("host resources");
    let registry = SideRegistry::new(resolver, resources, pool);

    let summary = registry.open("p1").await.expect("父运行中必须能开旁支");
    assert_eq!(summary.parent_thread_id, "p1");
    assert_eq!(registry.list("p1").await.len(), 1, "只该有一个旁支");

    let after = (
        std::fs::read(&log_path).unwrap(),
        std::fs::read(&meta_path).unwrap(),
        std::fs::read(&partial_path).unwrap(),
    );
    assert_eq!(before.0, after.0, "父 jsonl 不得被改写");
    assert_eq!(before.1, after.1, "父 meta 不得被改写");
    assert_eq!(before.2, after.2, "父 partial 不得被改写");

    registry.shutdown().await;
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
    //
    // 判定条件必须是「取到了这条旁支，且它没有进行中的轮次」。原先用
    // `listed.first().and_then(active_turn_id).is_none()`：`list()` 在父槽位仍是
    // `Creating` 或摘要读不出时返回空数组，`first()` 一次 `None` 就让循环提前
    // 退出，随后 `turn_start` 撞上 `TurnInProgress`——失败时报「旁支不可复用」，
    // 真因却是「还没开始等」。
    //
    // 窗口不能短于产品自己的收尾上界：pump 用 `SIDE_STOP_GRACE`(5s，见
    // `yi-agent-runtime/src/side_conversation/runner.rs:47`) 等核心任务 join
    // 完，`finish()` 也必须等这次 join 才发布 `Completed`。原来只给 2s，比产品
    // 允许的收尾时间还短，于是宿主机稍有负载就误报失败——实测未改动的 main 上
    // 10 次里 4 次失败，窗口放到 6s 后 10/10 通过。这里取 10s 留一倍余量；
    // 断言强度不变，仍然要求本轮确实收尾。
    const IDLE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let deadline = std::time::Instant::now() + IDLE_WAIT;
    let mut idle = false;
    while std::time::Instant::now() < deadline {
        let listed = registry.list("p1").await;
        if let Some(side) = listed.iter().find(|side| side.side_id == summary.side_id)
            && side.active_turn_id.is_none()
        {
            idle = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        idle,
        "the first turn must finish before the side is reusable"
    );
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

/// 关掉开关后，所有旁支必须消失，且 registry 仍可用。
///
/// 与既有的 `shutdown` 系列并列存在：那个测的是不可逆的进程退出，这个测的是
/// 可逆的开关。
#[tokio::test]
async fn close_all_stops_every_side_and_leaves_the_registry_usable() {
    let (_project, _home, registry) = registry_fixture().await;
    let first = registry.open("p1").await.unwrap();

    registry.close_all().await;

    assert_eq!(registry.side_count().await, 0, "every side must be gone");
    assert!(registry.list("p1").await.is_empty());

    // 可逆性：关掉再打开必须还能用。这条断言是 `close_all` 与 `shutdown()` 的
    // 分界线——复用 `shutdown()` 会让它失败。
    let again = registry
        .open("p1")
        .await
        .expect("close_all must not be terminal");
    assert_ne!(again.side_id, first.side_id, "a new generation");
}

/// 清扫与在途创建竞争时，不能留下一条活旁支。
///
/// 两种在途形状都要覆盖：
/// 1. 创建者已经跑在 `create_inner` 里（文件操作之后才 `sides.insert`）。
/// 2. 第二条 `open` 在同一父的 `Creating` 占位上等，清扫后它醒来**不能**
///    摇身变成新的创建者。
///
/// 第 2 种是 epoch 必须同时护住 `open()` 的原因：只护 `create_inner` 的话，
/// 等待者会带着清扫之后的新代次重试，成功建出一条活旁支。
#[tokio::test]
async fn close_all_wins_against_a_concurrent_open() {
    for _ in 0..20 {
        let (_project, _home, registry) = registry_fixture().await;
        let registry = Arc::new(registry);
        let a = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move { registry.open("p1").await }
        });
        let b = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move { registry.open("p1").await }
        });
        tokio::task::yield_now().await;
        registry.close_all().await;
        let _ = a.await;
        let _ = b.await;
        assert_eq!(
            registry.side_count().await,
            0,
            "a create or a woken waiter that outlived the sweep must not leave a live side"
        );
    }
}
