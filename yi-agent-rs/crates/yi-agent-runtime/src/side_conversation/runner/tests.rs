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
        let tools = SideToolSetupFactory::new(
            source.clone(),
            Arc::clone(&prepared.access),
            Arc::new(SideBlockingPool::new(4, 1, 4, 1)),
            TEST_SIDE_ID.to_string(),
            Arc::clone(&lease) as Arc<dyn Send + Sync>,
        );
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
