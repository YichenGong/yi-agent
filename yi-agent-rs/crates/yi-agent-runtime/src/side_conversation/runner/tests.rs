//! Runner tests: stop convergence, generation fencing and isolation.
//!
//! Every wait here is on a signal the production code actually produces
//! (`Notify`, the pending provider entering its stream), never a sleep that hopes a
//! race went the right way.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use super::*;
use crate::side_conversation::SideHostResources;
use crate::side_conversation::test_support::{
    CapturingProvider, PendingProvider, StaticRoots, source_with_pending_provider,
};
use yi_agent_core::{ContentBlock, Provider, ToolRegistry};
use yi_agent_tools::SideBlockingPool;

const TEST_SIDE_ID: &str = "11111111-1111-4111-8111-111111111111";

/// A feasible runner plus the temp dirs that must outlive it.
struct Fixture {
    _instance: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    host: Arc<SideHostResources>,
}

impl Fixture {
    fn new() -> Self {
        let instance = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let host = SideHostResources::new(
            instance.path().to_path_buf(),
            Arc::new(SideBlockingPool::new(8, 2, 8, 2)),
        )
        .unwrap();
        Self {
            _instance: instance,
            _workspace: workspace,
            host,
        }
    }

    /// Build a runner from `source`, preparing its resources for real.
    ///
    /// The source is repointed at the fixture's workspace: a real side always has
    /// a live parent cwd, and `SideReadAccess` canonicalises it at construction.
    async fn runner(&self, mut source: EffectiveSideSource) -> SideConversationRunner {
        let workspace = self._workspace.path().to_path_buf();
        source.primary = workspace.clone();
        source.roots = vec![workspace.clone()];
        source.current_roots = Arc::new(StaticRoots(vec![workspace]));
        let lease = self.host.reserve_slot(TEST_SIDE_ID).unwrap();
        let prepared = self
            .host
            .prepare(Arc::clone(&lease), &source, CancellationToken::new())
            .await
            .unwrap();
        // A side has no tools; the factory only carries its identity.
        let tools = SideToolSetupFactory::new(TEST_SIDE_ID.to_string());
        SideConversationRunner::new(TEST_SIDE_ID.to_string(), source, prepared, tools).unwrap()
    }
}

/// Drain a stream until it ends, so the pump is never left blocked.
async fn drain(mut stream: BoxStream<'static, SideRunnerEvent>) -> Vec<SideRunnerEvent> {
    let mut seen = Vec::new();
    while let Some(event) = stream.next().await {
        seen.push(event);
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn side_interrupt_waits_for_actual_quiescence() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    let events = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    assert_eq!(runner.status(), SideStatus::Running);
    provider.entered().await;

    runner.interrupt("turn-a").await.unwrap();
    assert!(
        runner.is_stopping(),
        "an interrupt must be visible immediately"
    );
    runner.wait_quiescent().await.unwrap();

    assert!(
        provider.dropped(),
        "the real provider stream must be torn down, not just detached"
    );
    assert_ne!(runner.status(), SideStatus::Running);
    assert!(
        runner.last_turn_was_joined(),
        "a finished turn must prove its core task was joined, not just dropped"
    );
    assert!(
        !runner.usage().complete,
        "a stopped turn has no final usage"
    );
    drop(events);
}

#[tokio::test(start_paused = true)]
async fn side_stale_interrupt_does_not_cancel_a_newer_turn() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    let first = runner
        .start_turn("turn-a".into(), "第一次".into())
        .await
        .unwrap();
    provider.entered().await;
    runner.interrupt("turn-a").await.unwrap();
    runner.wait_quiescent().await.unwrap();
    drop(first);

    // A second turn is live. An interrupt naming the old turn must not touch it.
    let second = runner
        .start_turn("turn-b".into(), "第二次".into())
        .await
        .unwrap();
    provider.entered().await;
    runner.interrupt("turn-a").await.unwrap();
    assert_eq!(
        runner.active_turn_id(),
        Some("turn-b"),
        "a stale interrupt must not end the newer turn"
    );
    assert!(!runner.is_stopping());
    assert_eq!(runner.status(), SideStatus::Running);

    runner.interrupt("turn-b").await.unwrap();
    runner.wait_quiescent().await.unwrap();
    drop(second);
}

/// 旁支取消不得波及父轮，父的停止也不得波及正在跑的旁支。
///
/// spec §12.1 要求「`side_cancel_does_not_cancel_parent` 与反向取消；独立
/// token/inbox/event」。这条用一个自己的 `PendingProvider` 冒充父轮：它的流
/// 是否被 drop、它的 core 任务是否结束，都是父是否被牵连的直接证据，不靠
/// 状态字段自证。
#[tokio::test(start_paused = true)]
async fn side_cancel_does_not_cancel_parent() {
    let fixture = Fixture::new();
    let (source, side_provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    // The parent turn: its own Agent, provider and cancellation, sharing nothing
    // with the side but the process.
    let parent_provider = Arc::new(PendingProvider::new());
    let mut parent_agent = Agent::new(
        Arc::clone(&parent_provider) as Arc<dyn Provider>,
        Arc::new(ToolRegistry::new()),
        AgentConfig::default(),
    );
    let mut parent_run = parent_agent
        .run_managed_blocks(
            vec![ContentBlock::Text("父问题".into())],
            ManagedRunLimits::side_defaults(),
        )
        .await
        .expect("the parent turn must start");
    parent_provider.entered().await;

    // Direction 1: cancelling the side must leave the parent alone.
    let stream = runner
        .start_turn("turn-a".into(), "旁问".into())
        .await
        .unwrap();
    side_provider.entered().await;

    runner.interrupt("turn-a").await.unwrap();
    runner.wait_quiescent().await.unwrap();
    drop(stream);

    assert!(
        side_provider.dropped(),
        "cancelling the side must tear down the side's own provider stream"
    );
    assert!(
        !parent_run.control.is_finished(),
        "cancelling a side must not end the parent's core task"
    );
    assert!(
        !parent_provider.dropped(),
        "cancelling a side must not tear down the parent's provider stream"
    );

    // Direction 2 needs its own side: `PendingProvider::dropped` is one flag per
    // provider instance, so reusing this one could not tell turn-b's stream apart
    // from the turn-a stream we just proved was torn down.
    let (source_b, side_provider_b) = source_with_pending_provider();
    let mut runner_b = fixture.runner(source_b).await;
    let second = runner_b
        .start_turn("turn-b".into(), "第二问".into())
        .await
        .unwrap();
    side_provider_b.entered().await;

    parent_run
        .control
        .stop_and_join(Duration::from_millis(50))
        .await
        .expect("the parent must join cleanly");
    assert!(parent_provider.dropped(), "the parent stop must be real");
    assert_eq!(
        runner_b.active_turn_id(),
        Some("turn-b"),
        "stopping the parent must not end the side's live turn"
    );
    assert_eq!(runner_b.status(), SideStatus::Running);
    assert!(
        !side_provider_b.dropped(),
        "stopping the parent must not tear down the side's provider stream"
    );

    runner_b.interrupt("turn-b").await.unwrap();
    runner_b.wait_quiescent().await.unwrap();
    drop(second);
}

#[tokio::test]
async fn side_rejects_a_second_inflight_turn() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    let stream = runner
        .start_turn("turn-a".into(), "第一个".into())
        .await
        .unwrap();
    provider.entered().await;

    let error = match runner.start_turn("turn-b".into(), "第二个".into()).await {
        Ok(_) => panic!("a second in-flight turn must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.reason, SideErrorReason::TurnInProgress);

    runner.interrupt("turn-a").await.unwrap();
    runner.wait_quiescent().await.unwrap();
    drop(stream);
}

#[tokio::test]
async fn side_rejects_an_empty_question_without_calling_the_provider() {
    let fixture = Fixture::new();
    let mut source = crate::side_conversation::test_support::source_with_history("q", "a");
    let provider = Arc::new(CapturingProvider::new("回答"));
    source.provider = provider.clone();
    let mut runner = fixture.runner(source).await;

    let error = match runner.start_turn("turn-a".into(), "   ".into()).await {
        Ok(_) => panic!("an empty question must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.reason, SideErrorReason::InvalidParams);
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn side_turn_streams_labeled_events_and_counts_usage() {
    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    let provider = Arc::new(CapturingProvider::new("旁支回答"));
    source.provider = provider.clone();
    let mut runner = fixture.runner(source).await;

    let stream = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    let events = drain(stream).await;
    runner.wait_quiescent().await.unwrap();

    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event.side_id, TEST_SIDE_ID);
        assert_eq!(event.turn_id, "turn-a");
        assert_eq!(event.generation, runner.generation());
    }
    assert_eq!(runner.status(), SideStatus::Idle);
    // The side keeps its own transcript: inherited history plus its question and
    // reply, and the parent's message count is untouched.
    assert_eq!(provider.call_count(), 1);
    let messages = runner.messages();
    assert!(messages.len() >= 4, "{messages:?}");
}

#[tokio::test]
async fn side_close_refuses_further_turns_and_joins_the_run() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    let stream = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    provider.entered().await;
    runner.close().await.unwrap();

    assert!(provider.dropped(), "close must tear down the run");
    assert_eq!(runner.status(), SideStatus::Closed);
    drop(stream);

    let error = match runner.start_turn("turn-b".into(), "关闭后".into()).await {
        Ok(_) => panic!("a closed side must refuse a new turn"),
        Err(error) => error,
    };
    assert_eq!(error.reason, SideErrorReason::SideClosing);
    assert_eq!(
        provider.calls(),
        1,
        "a closed side must never call the provider again"
    );
}

#[tokio::test]
async fn side_close_is_idempotent() {
    let fixture = Fixture::new();
    let (source, _provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;
    runner.close().await.unwrap();
    runner.close().await.unwrap();
    assert_eq!(runner.status(), SideStatus::Closed);
}

#[tokio::test(start_paused = true)]
async fn side_turn_timeout_stops_the_run() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;

    let stream = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    provider.entered().await;
    // The clock is paused: advancing past the cap must stop the turn for real.
    tokio::time::advance(SIDE_TURN_TIMEOUT + Duration::from_secs(1)).await;
    runner.wait_quiescent().await.unwrap();

    assert!(provider.dropped(), "the timeout must tear down the run");
    assert_eq!(runner.status(), SideStatus::Failed);
    let error = runner.last_error().expect("the cap is reported");
    assert!(error.message.contains("10 分钟"), "{}", error.message);
    // A timed-out turn has no final usage to report.
    assert!(!runner.usage().complete);
    drop(stream);
}

#[tokio::test]
async fn side_usage_starts_from_zero_and_is_incomplete_without_final_usage() {
    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    let provider = Arc::new(CapturingProvider::new("旁支回答"));
    source.provider = provider.clone();
    let mut runner = fixture.runner(source).await;

    assert_eq!(runner.usage(), SideUsage::default());
    let stream = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    drain(stream).await;
    runner.wait_quiescent().await.unwrap();
    // This provider reports no usage at all, so the totals must not claim zero was
    // measured.
    assert_eq!(runner.usage().total, SideTokenTotals::default());
    assert!(
        !runner.usage().complete,
        "without a provider usage report the total is incomplete"
    );
}

#[tokio::test]
async fn side_usage_accumulates_across_turns() {
    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    let provider = Arc::new(UsageProvider::default());
    source.provider = provider.clone();
    let mut runner = fixture.runner(source).await;

    for turn in ["turn-a", "turn-b"] {
        let stream = runner.start_turn(turn.into(), "问题".into()).await.unwrap();
        drain(stream).await;
        runner.wait_quiescent().await.unwrap();
    }
    // Two replies of 10 input / 5 output each.
    assert_eq!(runner.usage().total.input, 20);
    assert_eq!(runner.usage().total.output, 10);
    assert!(runner.usage().complete);
}

#[tokio::test]
async fn side_new_turn_after_a_completed_one_works() {
    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    let provider = Arc::new(CapturingProvider::new("旁支回答"));
    source.provider = provider.clone();
    let mut runner = fixture.runner(source).await;

    for turn in ["turn-a", "turn-b", "turn-c"] {
        let stream = runner.start_turn(turn.into(), "继续".into()).await.unwrap();
        drain(stream).await;
        runner.wait_quiescent().await.unwrap();
    }
    assert_eq!(provider.call_count(), 3);
    assert_eq!(runner.status(), SideStatus::Idle);
}

#[test]
fn side_status_is_serialisable_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&SideStatus::Creating).unwrap(),
        "\"creating\""
    );
    assert_eq!(
        serde_json::to_string(&SideStatus::Failed).unwrap(),
        "\"failed\""
    );
}

#[test]
fn side_token_totals_saturate_instead_of_wrapping() {
    let mut totals = SideTokenTotals {
        input: u64::MAX,
        ..Default::default()
    };
    totals.add_usage(&TokenUsage {
        input_tokens: 10,
        ..Default::default()
    });
    assert_eq!(
        totals.input,
        u64::MAX,
        "overflow must pin the maximum, never wrap to a small number"
    );
}

#[tokio::test]
async fn side_provider_failure_is_failed_with_a_safe_error() {
    struct RefusingProvider;

    #[async_trait::async_trait]
    impl Provider for RefusingProvider {
        async fn call_stream(
            &self,
            _request: yi_agent_core::ProviderRequest,
        ) -> Result<BoxStream<'static, yi_agent_core::ProviderEvent>, yi_agent_core::ProviderError>
        {
            Err(yi_agent_core::ProviderError::InvalidRequest(
                "private provider response must not be published".into(),
            ))
        }
    }

    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    source.provider = Arc::new(RefusingProvider);
    let mut runner = fixture.runner(source).await;
    let stream = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    let events = drain(stream).await;
    assert!(
        events
            .iter()
            .any(|frame| matches!(frame.event.event(), AgentEvent::Error(_)))
    );
    runner.wait_quiescent().await.unwrap();
    assert_eq!(
        runner.status(),
        SideStatus::Failed,
        "provider failure must not look idle"
    );
    let error = runner
        .last_error()
        .expect("failure must have visible feedback");
    assert!(!error.message.is_empty());
    assert!(!error.message.contains("private provider response"));
    assert!(runner.last_turn_was_joined());
}

/// Provider that reports a fixed usage per reply.
#[derive(Default)]
struct UsageProvider {
    calls: std::sync::Mutex<usize>,
}

#[async_trait::async_trait]
impl yi_agent_core::Provider for UsageProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::ProviderEvent>,
        yi_agent_core::ProviderError,
    > {
        *self.calls.lock().unwrap() += 1;
        Ok(Box::pin(futures::stream::iter(vec![
            yi_agent_core::ProviderEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            yi_agent_core::ProviderEvent::TextDelta("回答".into()),
            yi_agent_core::ProviderEvent::Stop {
                reason: yi_agent_core::StopReason::EndTurn,
            },
        ])))
    }
}

/// 旁支的模型必须**知道**自己在旁支里。
///
/// 这条测的是真实装配出来的 Session，而不是 `side_context` 自己的单元测试：
/// 那个模块的测试证明 `snapshot_for_side` 会追加边界，却证明不了 runner 真的
/// 调用了它。此前 runner 只是把 `forkable_messages` 原样 push 进新 Session，
/// 于是模型拿到父的完整历史 + 父的 system prompt、**没有任何一句话**说明这是
/// 一条只读旁支——现象就是它接着干主会话的活。
#[tokio::test]
async fn side_runner_injects_the_hidden_boundary_once() {
    let fixture = Fixture::new();
    let (source, _provider) = source_with_pending_provider();
    let runner = fixture.runner(source).await;

    let messages = runner.session_snapshot();
    let boundary_count = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|block| {
            matches!(block, ContentBlock::Text(text)
                if text == yi_agent_core::SIDE_BOUNDARY)
        })
        .count();
    assert_eq!(
        boundary_count, 1,
        "exactly one boundary: appending it per turn would stack, and skipping it \
         leaves the model with no idea it is a side conversation"
    );
    // 边界必须是**最后一条**：它之后的消息才是当前任务。
    assert_eq!(
        messages.last().unwrap().content,
        vec![ContentBlock::Text(yi_agent_core::SIDE_BOUNDARY.to_string())],
        "the inherited history must end on the boundary marker"
    );
    // 继承的父历史仍在边界之前，没有被裁剪掉。
    assert!(
        messages.len() > 1,
        "the parent's safe history must be inherited, not replaced by the marker"
    );
}

/// The snapshot is taken once at entry. A parent that keeps working afterwards
/// must not leak into the side, and the side's own Q&A must never reach the parent.
#[tokio::test]
async fn side_stays_on_its_entry_snapshot_across_questions() {
    use yi_agent_core::Message;

    let fixture = Fixture::new();
    let mut source =
        crate::side_conversation::test_support::source_with_history("父问题", "父回答");
    let provider = Arc::new(CapturingProvider::new("旁支回答"));
    source.provider = provider.clone();
    let parent = Arc::clone(&source.session);
    let mut runner = fixture.runner(source).await;

    // The parent keeps working after the user entered the side.
    parent
        .lock()
        .unwrap()
        .push(Message::user("父后来新增的问题"));

    for (turn, question) in [("turn-a", "第一问"), ("turn-b", "第二问")] {
        let stream = runner
            .start_turn(turn.into(), question.into())
            .await
            .unwrap();
        drain(stream).await;
        runner.wait_quiescent().await.unwrap();
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one request per question");
    for (index, request) in requests.iter().enumerate() {
        assert!(
            request.tools.is_empty(),
            "request {index} must carry zero tools"
        );
        let rendered = serde_json::to_string(&request.messages).unwrap();
        assert!(
            rendered.contains("父问题") && rendered.contains("父回答"),
            "request {index} must keep the entry snapshot"
        );
        assert!(
            !rendered.contains("父后来新增的问题"),
            "request {index} must not follow the parent past entry: {rendered}"
        );
        if index == 1 {
            assert!(
                rendered.contains("第一问"),
                "the side's own accepted Q&A carries over between questions: {rendered}"
            );
        }
    }

    // Nothing the side said reached the parent's session.
    let parent_now = parent.lock().unwrap().clone();
    let rendered_parent = serde_json::to_string(&parent_now.messages()).unwrap();
    assert!(
        !rendered_parent.contains("旁支回答") && !rendered_parent.contains("第一问"),
        "the parent transcript must never receive side content: {rendered_parent}"
    );
}

/// Closing must prove the real core task ended. A close that returns Ok while the
/// provider stream is still alive would let a late result write into a session the
/// host already believes is gone.
#[tokio::test(start_paused = true)]
async fn side_close_proves_core_join_and_stream_drop() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;
    let events = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    provider.entered().await;

    runner.close().await.unwrap();

    assert!(provider.dropped(), "the provider stream must be torn down");
    assert!(
        runner.last_turn_was_joined(),
        "close must observe the joined core task"
    );
    assert_eq!(runner.status(), SideStatus::Closed);
    drop(events);
}

/// The failure this guards: an unconfirmed join reported as `Closed`. The owner is
/// still alive, so the runner must keep its active entry, fail the close, and
/// refuse new turns — but stay able to finish the same owner later.
#[tokio::test(start_paused = true)]
async fn side_close_refuses_to_claim_closed_when_the_join_is_unconfirmed() {
    let fixture = Fixture::new();
    let (source, provider) = source_with_pending_provider();
    let mut runner = fixture.runner(source).await;
    let events = runner
        .start_turn("turn-a".into(), "问题".into())
        .await
        .unwrap();
    provider.entered().await;
    // Inject the timeout outcome: the same state a task that will not unwind
    // produces, without a real task that ignores cancellation.
    runner
        .active
        .as_ref()
        .unwrap()
        .shared
        .force_unjoined
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let error = runner.close().await.unwrap_err();
    assert_eq!(error.reason, SideErrorReason::SideClosing);
    assert_eq!(error.message, "core_join_unconfirmed");
    assert_eq!(
        runner.status(),
        SideStatus::Closing,
        "an unconfirmed close is Closing, never Closed"
    );
    assert!(!runner.last_turn_was_joined(), "the join stays unconfirmed");

    // The owner entry must survive, so a retry waits on the same task rather than
    // starting a fresh run or losing track of it.
    let retry = match runner.start_turn("turn-b".into(), "又问".into()).await {
        Ok(_) => panic!("a closing side must refuse new turns"),
        Err(error) => error,
    };
    assert!(
        matches!(
            retry.reason,
            SideErrorReason::TurnInProgress | SideErrorReason::SideClosing
        ),
        "a closing side must refuse new turns: {retry:?}"
    );

    // Once the join really completes, the same close succeeds.
    runner
        .active
        .as_ref()
        .unwrap()
        .shared
        .force_unjoined
        .store(false, std::sync::atomic::Ordering::SeqCst);
    runner.close().await.unwrap();
    assert_eq!(runner.status(), SideStatus::Closed);
    drop(events);
}
