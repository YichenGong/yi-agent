//! Live settings applied at real provider operation boundaries.
//!
//! Every assertion reads the actual `ProviderRequest` the provider received,
//! the real session handle identity, or gate/tool counters. Asserting on
//! `AgentConfig` would pass even if the loop never consulted the source.

#[path = "common/live_settings.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{
    BlockingTool, GateBarrier, MutableSettings, RecordingProvider, assert_tool_use_result_pairing,
    end_turn_text, snapshot, tool_use_turn,
};
use tokio::time::{Duration, timeout};
use yi_agent_core::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentSettingsSource, ContentBlock,
    LiveAgentSnapshot, LiveSettingsError, Provider, ProviderError, ProviderEvent, ProviderRequest,
    ToolRegistry,
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

fn assert_error_code(events: &[AgentEvent], code: &str) {
    let found = events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::Error(AgentError::LiveSettings(actual)) if *actual == code
        )
    });
    assert!(
        found,
        "expected AgentError::LiveSettings({code:?}), got {events:#?}"
    );
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

fn systems(requests: &[ProviderRequest]) -> Vec<Option<String>> {
    requests.iter().map(|req| req.system.clone()).collect()
}

fn s(value: &str) -> Option<String> {
    Some(value.to_string())
}

/// A source that counts how many times it was consulted.
struct CountingSettings {
    inner: Arc<MutableSettings>,
    calls: Arc<AtomicUsize>,
}

impl CountingSettings {
    fn new(snapshot: LiveAgentSnapshot) -> Arc<Self> {
        Arc::new(Self {
            inner: MutableSettings::new(snapshot),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl AgentSettingsSource for CountingSettings {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.snapshot()
    }
}

/// Wraps a scripted provider and records the source's value at call time, so a
/// test can prove the request was built after the snapshot was taken.
struct ProbeProvider {
    inner: RecordingProvider,
    probe: Arc<MutableSettings>,
    observed: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

#[async_trait::async_trait]
impl Provider for ProbeProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        let current = self.probe.0.lock().unwrap().system_prompt.clone();
        self.observed.lock().unwrap().push(current);
        self.inner.call_stream(req).await
    }
}

/// The core RED assertion: a connected source must win over the static prompt
/// on the very first operation, and the next operation must see the new value.
#[tokio::test(flavor = "multi_thread")]
async fn live_source_wins_over_static_prompt_and_refreshes_between_runs() {
    let provider = RecordingProvider::new(vec![end_turn_text("a"), end_turn_text("b")]);
    let requests = provider.requests.clone();
    let settings = MutableSettings::new(snapshot("rev-1", "first"));
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(Some(settings.as_source()));
    let session = agent.session_handle();

    assert_no_errors(&drain(agent.run("hello".into()).await.unwrap()).await);

    settings.set(snapshot("rev-2", "second"));
    assert_no_errors(&drain(agent.run("again".into()).await.unwrap()).await);

    assert_eq!(
        systems(&requests.lock().unwrap()),
        vec![s("first"), s("second")],
        "each operation must build its request from the current snapshot"
    );
    assert!(
        Arc::ptr_eq(&session, &agent.session_handle()),
        "the session Arc must survive live settings refreshes"
    );
}

/// Without a source, the static config must keep identical behaviour: this is
/// the compatibility gate for every existing caller.
#[tokio::test(flavor = "multi_thread")]
async fn static_agent_without_source_keeps_config_prompt() {
    let provider = RecordingProvider::new(vec![end_turn_text("a")]);
    let requests = provider.requests.clone();
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );

    assert_no_errors(&drain(agent.run("hello".into()).await.unwrap()).await);
    assert_eq!(requests.lock().unwrap()[0].system, s("old"));
}

/// A source error must surface as the controlled recoverable branch, emit no
/// provider request, and leave no half-built assistant turn in the session.
#[tokio::test(flavor = "multi_thread")]
async fn failing_source_reports_controlled_error_without_provider_call() {
    struct Failing {
        code: &'static str,
        calls: Arc<AtomicUsize>,
    }
    impl AgentSettingsSource for Failing {
        fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LiveSettingsError::new(self.code))
        }
    }

    let provider = RecordingProvider::new(vec![end_turn_text("never")]);
    let requests = provider.requests.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(Some(Arc::new(Failing {
        code: "snapshot_unavailable",
        calls: calls.clone(),
    })));

    let events = drain(agent.run("hello".into()).await.unwrap()).await;

    assert_error_code(&events, "snapshot_unavailable");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one snapshot per operation"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "no provider request may be built from a failed snapshot"
    );
    let messages = agent.session_handle().lock().unwrap().messages().to_vec();
    assert_eq!(messages.len(), 1, "only the user prompt is committed");
    assert!(
        messages[0]
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text(_))),
        "no assistant-shaped content may leak into the session"
    );
}

/// A source failure happens after admission succeeded, so the already acquired
/// lease must be released by the stop path (not leaked until the run ends).
#[tokio::test(flavor = "multi_thread")]
async fn source_error_releases_the_admission_lease() {
    struct Failing;
    impl AgentSettingsSource for Failing {
        fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
            Err(LiveSettingsError::new("io_degraded"))
        }
    }

    let gate = GateBarrier::blocking_all();
    let provider = RecordingProvider::new(vec![end_turn_text("never")]);
    let requests = provider.requests.clone();
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    )
    .with_provider_turn_gate(gate.clone());
    agent.set_live_settings(Some(Arc::new(Failing)));

    let stream = agent.run("hello".into()).await.unwrap();
    gate.wait_entered().await;
    gate.release_one();

    let events = drain(stream).await;
    assert_error_code(&events, "io_degraded");
    assert_eq!(gate.acquisitions(), 1, "no request, so no second admission");
    assert_eq!(gate.live_leases(), 0, "the acquired lease must be released");
    assert!(requests.lock().unwrap().is_empty());
}

/// Snapshot timing: it must be read only after the first admission succeeded,
/// the first lease must be carried into attempt 1 (no double acquire), and the
/// provider must see the value published while admission was pending.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_is_read_after_first_admission_and_lease_is_reused() {
    let gate = GateBarrier::blocking_all();
    let settings = MutableSettings::new(snapshot("rev-1", "first"));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ProbeProvider {
        inner: RecordingProvider::new(vec![end_turn_text("done")]),
        probe: settings.clone(),
        observed: observed.clone(),
    };
    let requests = provider.inner.requests.clone();
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    )
    .with_provider_turn_gate(gate.clone());
    agent.set_live_settings(Some(settings.as_source()));

    let stream = agent.run("hello".into()).await.unwrap();
    // Park inside the first admission, then publish. The operation has not
    // started, so the new value must win.
    gate.wait_entered().await;
    settings.set(snapshot("rev-2", "admitted"));
    gate.release_one();

    assert_no_errors(&drain(stream).await);
    assert_eq!(
        gate.acquisitions(),
        1,
        "attempt 1 must reuse its lease, not acquire twice"
    );
    assert_eq!(gate.live_leases(), 0, "the lease must be released");
    assert_eq!(systems(&requests.lock().unwrap()), vec![s("admitted")]);
    assert_eq!(
        observed.lock().unwrap().clone(),
        vec![s("admitted")],
        "the request must be built after the snapshot is taken"
    );
}

/// A transport retry is the SAME semantic operation: the request, including the
/// system prompt, must not be rebuilt from a newer snapshot mid-retry even
/// though the retry re-acquires admission.
#[tokio::test(flavor = "multi_thread")]
async fn transport_retry_reuses_the_same_request_snapshot() {
    // Admit attempt 1 immediately; park the retry admission until the test has
    // republished the settings.
    let gate = GateBarrier::blocking_from(1);
    let settings = MutableSettings::new(snapshot("rev-1", "first"));
    let mut config = static_config();
    config.provider_error_retry_limit = 3;
    let provider = RecordingProvider::new(vec![end_turn_text("done")]).rejecting_first(1);
    let requests = provider.requests.clone();
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        config,
    )
    .with_provider_turn_gate(gate.clone());
    agent.set_live_settings(Some(settings.as_source()));

    let stream = agent.run("hello".into()).await.unwrap();
    let first = wait_for_requests(&requests, 1).await;
    assert_eq!(first[0].system, s("first"));

    // Retry admission is now parked. Publish a new value, then let it proceed.
    gate.wait_entered().await;
    settings.set(snapshot("rev-2", "second"));
    gate.release_one();

    assert_no_errors(&drain(stream).await);
    assert_eq!(
        systems(&requests.lock().unwrap()),
        vec![s("first"), s("first")],
        "a retry must reuse the original operation snapshot"
    );
    assert_eq!(gate.live_leases(), 0, "every retry lease must be released");
}

/// In-flight tool barrier: a save during a running tool applies to the NEXT
/// THINK, keeps history paired, and never replays the tool.
#[tokio::test(flavor = "multi_thread")]
async fn edit_during_inflight_tool_applies_to_next_think_without_replay() {
    let tool = BlockingTool::new("blocking_tool");
    let mut registry = ToolRegistry::default();
    registry.register(tool.clone());
    let provider = RecordingProvider::new(vec![
        tool_use_turn("tool-1", "blocking_tool", r#"{"command":"sleep"}"#),
        end_turn_text("after"),
    ]);
    let requests = provider.requests.clone();
    let settings = MutableSettings::new(snapshot("rev-1", "before"));
    let mut agent = Agent::new(Arc::new(provider), Arc::new(registry), static_config());
    agent.set_live_settings(Some(settings.as_source()));

    let stream = agent.run("go".into()).await.unwrap();
    let first = wait_for_requests(&requests, 1).await;
    assert_eq!(first[0].system, s("before"));

    tool.wait_entered().await;
    settings.set(snapshot("rev-2", "after"));
    tool.release();

    assert_no_errors(&drain(stream).await);

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(
        systems(&recorded),
        vec![s("before"), s("after")],
        "the in-flight tool keeps its snapshot; only the next THINK refreshes"
    );
    assert_eq!(tool.calls(), 1, "the tool must run exactly once");
    assert_tool_use_result_pairing(&recorded[1].messages);
    assert_tool_use_result_pairing(agent.session_handle().lock().unwrap().messages());
}

/// The source is consulted once per THINK operation, not per streamed token.
#[tokio::test(flavor = "multi_thread")]
async fn source_is_sampled_once_per_think_operation() {
    let counting = CountingSettings::new(snapshot("rev-1", "first"));
    let provider = RecordingProvider::new(vec![end_turn_text("a"), end_turn_text("b")]);
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(Some(counting.clone() as _));

    assert_no_errors(&drain(agent.run("one".into()).await.unwrap()).await);
    assert_eq!(counting.calls(), 1);
    assert_no_errors(&drain(agent.run("two".into()).await.unwrap()).await);
    assert_eq!(counting.calls(), 2);
}

/// A missing source after `set_live_settings(None)` falls back to static config.
#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_source_restores_static_behaviour() {
    let provider = RecordingProvider::new(vec![end_turn_text("a"), end_turn_text("b")]);
    let requests = provider.requests.clone();
    let settings = MutableSettings::new(snapshot("rev-1", "live"));
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::default()),
        static_config(),
    );
    agent.set_live_settings(Some(settings.as_source()));
    assert_no_errors(&drain(agent.run("one".into()).await.unwrap()).await);

    agent.set_live_settings(None);
    assert!(agent.live_settings_source().is_none());
    assert_no_errors(&drain(agent.run("two".into()).await.unwrap()).await);

    assert_eq!(
        systems(&requests.lock().unwrap()),
        vec![s("live"), s("old")],
        "clearing the source must restore the static prompt"
    );
}
