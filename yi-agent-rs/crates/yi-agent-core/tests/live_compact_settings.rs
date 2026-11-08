//! Live turn limits and compaction at real operation boundaries.
//!
//! Each test asserts on the actual provider requests, the real session
//! history, or gate counters — never on a config object.

#[path = "common/live_settings.rs"]
mod support;

use std::sync::Arc;

use support::{
    BlockingTool, GateBarrier, MutableSettings, RecordingProvider, assert_tool_use_result_pairing,
    end_turn_text, snapshot, tool_use_turn,
};
use tokio::time::{Duration, timeout};
use yi_agent_core::{
    Agent, AgentConfig, AgentEvent, AgentSettingsSource, DoneReason, Message, Provider,
    ProviderEvent, ProviderRequest, Session, ToolRegistry,
};

fn static_config() -> AgentConfig {
    AgentConfig {
        system_prompt: Some("old".to_string()),
        think_stall_backoff_base: Duration::from_millis(1),
        provider_error_retry_limit: 3,
        ..AgentConfig::default()
    }
}

async fn drain(mut stream: futures::stream::BoxStream<'static, AgentEvent>) -> Vec<AgentEvent> {
    use futures::StreamExt;
    timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("run did not finish")
}

fn assert_no_errors(events: &[AgentEvent]) {
    assert!(
        events.iter().all(|e| !matches!(e, AgentEvent::Error(_))),
        "unexpected error events: {events:#?}"
    );
}

fn done_reasons(events: &[AgentEvent]) -> Vec<DoneReason> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Done { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

async fn wait_for_requests(
    requests: &Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    count: usize,
) -> Vec<ProviderRequest> {
    let mut spins = 0;
    loop {
        {
            let recorded = requests.lock().unwrap().clone();
            if recorded.len() >= count {
                return recorded;
            }
        }
        assert!(spins < 5_000, "provider was not called");
        spins += 1;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A lowered limit inside a live tool phase must stop the loop before the next
/// THINK: the tool already ran exactly once, no new request is built, and the
/// run ends on `MaxTurns`.
#[tokio::test(flavor = "multi_thread")]
async fn lowered_turn_limit_stops_before_the_next_think() {
    let tool = BlockingTool::new("blocking_tool");
    let mut registry = ToolRegistry::default();
    registry.register(tool.clone());
    let provider = RecordingProvider::new(vec![
        tool_use_turn("tool-1", "blocking_tool", r#"{"command":"work"}"#),
        end_turn_text("must not be requested"),
    ]);
    let requests = provider.requests.clone();
    let settings = MutableSettings::new(snapshot("rev-1", "prompt"));
    let mut agent = Agent::new(Arc::new(provider), Arc::new(registry), static_config());
    agent.set_live_settings(settings.as_source());

    let stream = agent.run("go".into()).await.unwrap();
    wait_for_requests(&requests, 1).await;

    tool.wait_entered().await;
    // Lower the cap inside the in-flight tool of the same user turn.
    let mut lowered = snapshot("rev-2", "prompt");
    lowered.max_turns = Some(1);
    settings.set(lowered);
    tool.release();

    let events = drain(stream).await;
    assert_no_errors(&events);
    assert_eq!(
        done_reasons(&events),
        vec![DoneReason::MaxTurns],
        "the lowered limit must end the run on the cap"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "no second THINK may be started after the cap was lowered"
    );
    assert_eq!(tool.calls(), 1, "the tool must run exactly once");
    assert_tool_use_result_pairing(agent.session_handle().lock().unwrap().messages());
}

/// Raising the limit mid-tool lets the same run continue: the next THINK uses
/// the new value without replaying the tool.
#[tokio::test(flavor = "multi_thread")]
async fn raised_turn_limit_continues_without_replaying_the_tool() {
    let tool = BlockingTool::new("blocking_tool");
    let mut registry = ToolRegistry::default();
    registry.register(tool.clone());
    let provider = RecordingProvider::new(vec![
        tool_use_turn("tool-1", "blocking_tool", r#"{"command":"work"}"#),
        end_turn_text("after"),
    ]);
    let requests = provider.requests.clone();
    let mut initial = snapshot("rev-1", "prompt");
    initial.max_turns = Some(1);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(Arc::new(provider), Arc::new(registry), static_config());
    agent.set_live_settings(settings.as_source());

    let stream = agent.run("go".into()).await.unwrap();
    wait_for_requests(&requests, 1).await;

    tool.wait_entered().await;
    let mut raised = snapshot("rev-2", "prompt");
    raised.max_turns = Some(3);
    settings.set(raised);
    tool.release();

    let events = drain(stream).await;
    assert_no_errors(&events);
    assert_eq!(done_reasons(&events), vec![DoneReason::EndTurn]);
    assert_eq!(tool.calls(), 1, "the tool must not be replayed");
    assert_tool_use_result_pairing(agent.session_handle().lock().unwrap().messages());
}

/// A limit raised after the run already ended must not start a new run — no
/// automatic retry of finished work.
#[tokio::test(flavor = "multi_thread")]
async fn raising_the_limit_after_completion_does_not_restart_the_run() {
    let provider = RecordingProvider::new(vec![end_turn_text("done")]);
    let requests = provider.requests.clone();
    let mut initial = snapshot("rev-1", "prompt");
    initial.max_turns = Some(1);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(settings.as_source());

    let events = drain(agent.run("one".into()).await.unwrap()).await;
    assert_no_errors(&events);
    assert_eq!(done_reasons(&events), vec![DoneReason::EndTurn]);

    let mut raised = snapshot("rev-2", "prompt");
    raised.max_turns = Some(5);
    settings.set(raised);

    // Nothing may be re-dispatched by the settings change alone.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "a settings change must never re-run finished work"
    );
}

/// The manual compaction entry point takes its own lease, applies the live
/// budgets, and does not render the normal system prompt.
#[tokio::test(flavor = "multi_thread")]
async fn manual_compaction_uses_live_budgets_and_its_own_lease() {
    let provider = RecordingProvider::new(vec![end_turn_text("checkpoint")]);
    let requests = provider.requests.clone();
    let gate = GateBarrier::blocking_from(0);
    let settings = MutableSettings::new(snapshot("rev-1", "normal prompt"));

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    session.push(Message::user("recent question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "recent answer".into(),
    )]));

    let source: Arc<dyn AgentSettingsSource> = settings.as_source();
    let gate_dyn: Arc<dyn yi_agent_core::ProviderTurnGate> = gate.clone();
    let provider_arc: Arc<dyn Provider> = Arc::new(provider);
    let config = static_config();
    let task = tokio::spawn(async move {
        yi_agent_core::compact_session_with_live_settings(
            &provider_arc,
            &config,
            &session,
            Some(source.as_ref()),
            Some(gate_dyn.as_ref()),
        )
        .await
    });

    gate.wait_entered().await;
    assert_eq!(
        gate.live_leases(),
        0,
        "the lease is granted only on release"
    );
    gate.release_one();

    let compacted = task.await.unwrap().unwrap();
    assert!(compacted.is_some(), "the history must be compacted");
    assert_eq!(
        gate.live_leases(),
        0,
        "the compaction lease must be released"
    );

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].system, None,
        "a compaction must never render the normal system prompt"
    );
    assert!(
        recorded[0].tools.is_empty(),
        "a compaction request must not advertise tools"
    );
}

/// A failing source on the manual entry point is a controlled error and still
/// releases the lease.
#[tokio::test(flavor = "multi_thread")]
async fn manual_compaction_source_failure_releases_the_lease() {
    struct Failing;
    impl AgentSettingsSource for Failing {
        fn snapshot(
            &self,
        ) -> Result<yi_agent_core::LiveAgentSnapshot, yi_agent_core::LiveSettingsError> {
            Err(yi_agent_core::LiveSettingsError::new("unavailable"))
        }
    }

    let provider = RecordingProvider::new(vec![end_turn_text("never")]);
    let requests = provider.requests.clone();
    let gate = GateBarrier::blocking_from(1);
    let mut session = Session::new();
    session.push(Message::user("old"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "answer".into(),
    )]));

    let source = Failing;
    let gate_dyn: Arc<dyn yi_agent_core::ProviderTurnGate> = gate.clone();
    let result = yi_agent_core::compact_session_with_live_settings(
        &(Arc::new(provider) as Arc<dyn Provider>),
        &static_config(),
        &session,
        Some(&source),
        Some(gate_dyn.as_ref()),
    )
    .await;

    match result {
        Err(yi_agent_core::AgentError::LiveSettings(code)) => assert_eq!(code, "unavailable"),
        other => panic!("expected a controlled live-settings error, got {other:?}"),
    }
    assert_eq!(gate.live_leases(), 0, "the lease must be released on error");
    assert!(requests.lock().unwrap().is_empty());
}

/// Without a source the manual entry point keeps the static behaviour: the
/// configured budgets and no added lease.
#[tokio::test(flavor = "multi_thread")]
async fn manual_compaction_without_source_stays_static() {
    let provider = RecordingProvider::new(vec![end_turn_text("checkpoint")]);
    let requests = provider.requests.clone();
    let mut session = Session::new();
    session.push(Message::user("old"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "answer".into(),
    )]));
    session.push(Message::user("recent"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "latest".into(),
    )]));

    let compacted = yi_agent_core::compact_session_with_live_settings(
        &(Arc::new(provider) as Arc<dyn Provider>),
        &static_config(),
        &session,
        None,
        None,
    )
    .await
    .unwrap();

    assert!(compacted.is_some());
    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].system, None);
}

/// The static facade still works unchanged for existing callers.
#[tokio::test(flavor = "multi_thread")]
async fn static_compact_session_facade_is_unchanged() {
    let provider = RecordingProvider::new(vec![end_turn_text("checkpoint")]);
    let mut session = Session::new();
    session.push(Message::user("old"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "answer".into(),
    )]));
    session.push(Message::user("recent"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "latest".into(),
    )]));

    let compacted = yi_agent_core::compact_session(
        &(Arc::new(provider) as Arc<dyn Provider>),
        &static_config(),
        &session,
    )
    .await
    .unwrap();
    assert!(compacted.is_some());
}

/// A save during the auto-compaction summary wait is applied after it: the
/// normal THINK that follows uses the new prompt, while the summary itself kept
/// the snapshot it started with.
#[tokio::test(flavor = "multi_thread")]
async fn auto_compaction_keeps_its_snapshot_and_next_think_refreshes() {
    // Park the first provider call (the summary) so the save lands while the
    // compaction operation is genuinely in flight.
    let provider = RecordingProvider::new(vec![
        end_turn_text("summary"),
        end_turn_text("after compaction"),
    ])
    .blocking_range(0, 1);
    let requests = provider.requests.clone();
    let provider_handle = provider.clone();
    let mut initial = snapshot("rev-1", "before");
    initial.compact_threshold = Some(1);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(settings.as_source());

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    session.push(Message::user("recent question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "recent answer".into(),
    )]));
    session.set_last_input_tokens(Some(500));
    agent.set_session_messages(session.messages().to_vec());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .set_last_input_tokens(Some(500));

    let stream = agent.run("new question".into()).await.unwrap();
    // The summary call is parked inside the provider: publish, then release.
    provider_handle.wait_entered().await;
    settings.set_prompt("after");
    provider_handle.release_one();

    let events = drain(stream).await;
    assert_no_errors(&events);

    let recorded = requests.lock().unwrap().clone();
    assert!(
        recorded.len() >= 2,
        "expected a summary and a following THINK, got {recorded:#?}"
    );
    assert_eq!(
        recorded[0].system, None,
        "the compaction request must not render the normal prompt"
    );
    let last = recorded.last().unwrap();
    assert_eq!(
        last.system,
        Some("after".to_string()),
        "the THINK after compaction must read the refreshed prompt"
    );
}

/// A cap lowered while the compaction summary is in flight ends the run instead
/// of spending the next iteration.
#[tokio::test(flavor = "multi_thread")]
async fn lowered_cap_during_compaction_wait_ends_the_run() {
    let provider = RecordingProvider::new(vec![end_turn_text("summary")]).blocking_range(0, 1);
    let requests = provider.requests.clone();
    let provider_handle = provider.clone();
    let mut initial = snapshot("rev-1", "before");
    initial.compact_threshold = Some(1);
    initial.max_turns = Some(5);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(settings.as_source());

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    agent.set_session_messages(session.messages().to_vec());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .set_last_input_tokens(Some(500));

    let stream = agent.run("new question".into()).await.unwrap();
    provider_handle.wait_entered().await;
    let mut lowered = snapshot("rev-2", "before");
    lowered.compact_threshold = Some(1);
    lowered.max_turns = Some(0);
    settings.set(lowered);
    provider_handle.release_one();

    let events = drain(stream).await;
    assert_no_errors(&events);
    assert_eq!(done_reasons(&events), vec![DoneReason::MaxTurns]);
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "no THINK may follow a cap that now forbids it"
    );
}

#[allow(dead_code)]
fn unused_provider_event(_event: ProviderEvent) {}

/// An already-exhausted cap must be checked before compaction: an iteration
/// that will not run must not spend a summary request.
#[tokio::test(flavor = "multi_thread")]
async fn exhausted_cap_skips_the_summary_request() {
    let provider = RecordingProvider::new(vec![end_turn_text("must not be requested")]);
    let requests = provider.requests.clone();
    let mut initial = snapshot("rev-1", "prompt");
    initial.compact_threshold = Some(1);
    initial.max_turns = Some(0);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(settings.as_source());

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    agent.set_session_messages(session.messages().to_vec());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .set_last_input_tokens(Some(500));

    let events = drain(agent.run("new".into()).await.unwrap()).await;
    assert_no_errors(&events);
    assert_eq!(done_reasons(&events), vec![DoneReason::MaxTurns]);
    assert!(
        requests.lock().unwrap().is_empty(),
        "an exhausted cap must not spend a compaction summary request"
    );
}

/// The live compaction budget must actually decide what is retained: a budget
/// too small for the oldest long user message drops it, while the static default
/// keeps it.
#[tokio::test(flavor = "multi_thread")]
async fn live_compaction_budget_changes_retained_history() {
    fn history() -> Session {
        let mut session = Session::new();
        // Deliberately long: ~250 tokens, so it fits the default budget but not
        // a 100-token one.
        let long = "alpha ".repeat(200);
        session.push(Message::user(long));
        session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
            "first answer".into(),
        )]));
        session.push(Message::user("second question"));
        session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
            "second answer".into(),
        )]));
        session.push(Message::user("third question"));
        session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
            "third answer".into(),
        )]));
        session
    }

    let generous_provider = RecordingProvider::new(vec![end_turn_text("summary")]);
    let generous = yi_agent_core::compact_session_with_live_settings(
        &(Arc::new(generous_provider) as Arc<dyn Provider>),
        &static_config(),
        &history(),
        None,
        None,
    )
    .await
    .unwrap()
    .expect("the default budget must compact");

    let tight_provider = RecordingProvider::new(vec![end_turn_text("summary")]);
    let mut tight_snapshot = snapshot("rev-1", "prompt");
    tight_snapshot.compact_user_budget_tokens = 100;
    tight_snapshot.compact_tool_budget_tokens = 1;
    let tight_settings = MutableSettings::new(tight_snapshot);
    let tight_source: Arc<dyn AgentSettingsSource> = tight_settings.as_source();
    let tight = yi_agent_core::compact_session_with_live_settings(
        &(Arc::new(tight_provider) as Arc<dyn Provider>),
        &static_config(),
        &history(),
        Some(tight_source.as_ref()),
        None,
    )
    .await
    .unwrap()
    .expect("a tight budget must still compact");

    let retained = |session: &Session| {
        session
            .messages()
            .iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        retained(&generous).contains("alpha"),
        "the default budget must keep the long older user message"
    );
    assert!(
        !retained(&tight).contains("alpha"),
        "a tight live budget must drop the long older user message"
    );
}

/// A Stop pressed while the summary is in flight must not leave the gate
/// chargeable: the summary future is dropped and its lease goes with it.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_during_summary_releases_its_lease() {
    let provider = RecordingProvider::new(vec![end_turn_text("summary")]).blocking_range(0, 1);
    let provider_handle = provider.clone();
    let requests = provider.requests.clone();
    let mut initial = snapshot("rev-1", "prompt");
    initial.compact_threshold = Some(1);
    let settings = MutableSettings::new(initial);
    let gate = GateBarrier::blocking_from(0);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    )
    .with_provider_turn_gate(gate.clone());
    agent.set_live_settings(settings.as_source());

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    agent.set_session_messages(session.messages().to_vec());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .set_last_input_tokens(Some(500));

    let stream = agent.run("new question".into()).await.unwrap();
    // `run` resets the token, so it must be captured afterwards.
    let cancel_token = agent.cancel_token();

    // The summary request is parked inside the provider; Stop now.
    provider_handle.wait_entered().await;
    assert_eq!(
        gate.live_leases(),
        0,
        "the lease is only granted once the gate lets the call through"
    );
    cancel_token.cancel();
    provider_handle.release_one();

    let events = drain(stream).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Cancelled)),
        "a Stop during the summary must surface as a cancellation: {events:#?}"
    );
    assert_eq!(
        gate.live_leases(),
        0,
        "the cancelled summary must release its admission lease"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// The summary may take long enough for a save to raise the trigger. When the
/// history would no longer be compacted under the latest threshold, the run
/// must keep the original messages rather than install a summary nobody asked
/// for any more.
#[tokio::test(flavor = "multi_thread")]
async fn raised_threshold_during_summary_keeps_the_original_history() {
    let provider = RecordingProvider::new(vec![
        end_turn_text("summary that must not be installed"),
        end_turn_text("normal"),
    ])
    .blocking_range(0, 1);
    let provider_handle = provider.clone();
    let mut initial = snapshot("rev-1", "prompt");
    initial.compact_threshold = Some(1);
    let settings = MutableSettings::new(initial);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(settings.as_source());

    let mut session = Session::new();
    session.push(Message::user("old question"));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        "old answer".into(),
    )]));
    agent.set_session_messages(session.messages().to_vec());
    agent
        .session_handle()
        .lock()
        .unwrap()
        .set_last_input_tokens(Some(500));

    let stream = agent.run("new question".into()).await.unwrap();
    provider_handle.wait_entered().await;

    // Raise the threshold far above the observed usage: the history no longer
    // needs compacting, so the finished summary must be discarded.
    let mut raised = snapshot("rev-2", "prompt");
    raised.compact_threshold = Some(100_000);
    settings.set(raised);
    provider_handle.release_one();

    let events = drain(stream).await;
    assert_no_errors(&events);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::AutoCompacting { .. })),
        "a summary that is no longer warranted must not be announced: {events:#?}"
    );
    let messages = agent.session_handle().lock().unwrap().messages().to_vec();
    assert!(
        messages
            .iter()
            .any(|message| format!("{message:?}").contains("old answer")),
        "the original history must survive an obsolete summary"
    );
    assert!(
        !messages
            .iter()
            .any(|message| format!("{message:?}").contains("must not be installed")),
        "the obsolete summary must not be written into the session"
    );
}
