//! Shared fixtures for live-settings integration tests.
//!
//! Deliberately test-only: no production hook, no feature flag, no backdoor in
//! `yi-agent-core` itself. Each integration test pulls this file in with
//! `#[path = "common/live_settings.rs"] mod support;`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::StreamExt;
use yi_agent_core::live_settings::{
    AgentSettingsSource, LiveAgentSnapshot, LiveSettingsError, SharedSettingsSource,
};
use yi_agent_core::{
    Message, Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderTurnGate,
    ProviderTurnLease, StopReason, Tool, ToolResult,
};

/// Provider that records every request it receives and replays scripted events.
///
/// All state is behind `Arc`, so `Clone` shares one recording — a test can keep
/// a handle for barrier control after handing the provider to the agent.
///
/// `reject_remaining` lets a test fail the first N calls with a retryable
/// error while still recording their request, so "when was the request built"
/// and "does a retry rebuild it" are observable from the requests alone.
#[derive(Clone)]
pub struct RecordingProvider {
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
    pub scripts: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    reject_remaining: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    block_from: usize,
    block_until: usize,
}

impl RecordingProvider {
    pub fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            scripts: Arc::new(Mutex::new(scripts.into())),
            reject_remaining: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            block_from: usize::MAX,
            block_until: usize::MAX,
        }
    }

    /// Park every call from index `n` until the test releases it, so a save can
    /// be published while that call is in flight without racing wall-clock time.
    pub fn blocking_from(mut self, n: usize) -> Self {
        self.block_from = n;
        self
    }

    /// Park calls in the half-open range `[from, until)` only. Everything
    /// outside it proceeds, so a test can park exactly the summary call.
    pub fn blocking_range(mut self, from: usize, until: usize) -> Self {
        self.block_from = from;
        self.block_until = until;
        self
    }

    /// Wait until a call is parked inside the provider.
    pub async fn wait_entered(&self) {
        let permit = self
            .entered
            .acquire()
            .await
            .expect("provider barrier entered semaphore closed");
        permit.forget();
    }

    /// Let exactly one parked call proceed.
    pub fn release_one(&self) {
        self.release.add_permits(1);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Reject the first `count` calls with `ProviderError::RateLimited`.
    pub fn rejecting_first(mut self, count: usize) -> Self {
        self.reject_remaining = Arc::new(AtomicUsize::new(count));
        self
    }

    pub fn scripted_systems(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.system.clone())
            .collect()
    }

    pub fn recorded(&self) -> Vec<ProviderRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index >= self.block_from && index < self.block_until {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|_| ProviderError::Network("provider barrier closed".into()))?
                .forget();
        }
        if self
            .reject_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ProviderError::RateLimited);
        }
        let events = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        Ok(futures::stream::iter(events).boxed())
    }
}

/// Mutable settings source a test can flip between operations.
pub struct MutableSettings(pub Mutex<LiveAgentSnapshot>);

impl MutableSettings {
    pub fn new(snapshot: LiveAgentSnapshot) -> Arc<Self> {
        Arc::new(Self(Mutex::new(snapshot)))
    }

    pub fn set(&self, snapshot: LiveAgentSnapshot) {
        *self.0.lock().unwrap() = snapshot;
    }

    pub fn set_prompt(&self, prompt: &str) {
        self.0.lock().unwrap().system_prompt = Some(prompt.to_string());
    }

    pub fn as_source(self: &Arc<Self>) -> SharedSettingsSource {
        self.clone()
    }
}

impl AgentSettingsSource for MutableSettings {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// Source that always fails, exercising the controlled error branch.
pub struct FailingSettings {
    pub code: &'static str,
    pub calls: AtomicUsize,
}

impl FailingSettings {
    pub fn new(code: &'static str) -> Arc<Self> {
        Arc::new(Self {
            code,
            calls: AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl AgentSettingsSource for FailingSettings {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LiveSettingsError::new(self.code))
    }
}

/// Snapshot helper with the live fields a test cares about.
pub fn snapshot(revision: &str, prompt: &str) -> LiveAgentSnapshot {
    LiveAgentSnapshot {
        revision: revision.to_string(),
        max_turns: Some(200),
        compact_threshold: Some(100_000),
        compact_user_budget_tokens: 40_000,
        compact_tool_budget_tokens: 80_000,
        system_prompt: Some(prompt.to_string()),
    }
}

/// Admission gate that parks acquisitions from a chosen index onward, counts
/// live leases (so "lease dropped" is observable) and counts acquisitions.
pub struct GateBarrier {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    block_from: usize,
    live_leases: Arc<AtomicUsize>,
    acquisitions: Arc<AtomicUsize>,
}

impl GateBarrier {
    /// Park every acquisition until the test releases it.
    pub fn blocking_all() -> Arc<Self> {
        Self::blocking_from(0)
    }

    /// Let the first `n` acquisitions through; park the rest.
    pub fn blocking_from(n: usize) -> Arc<Self> {
        Arc::new(Self {
            entered: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            block_from: n,
            live_leases: Arc::new(AtomicUsize::new(0)),
            acquisitions: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Wait until one acquire is parked inside the gate.
    pub async fn wait_entered(&self) {
        let permit = self
            .entered
            .acquire()
            .await
            .expect("gate barrier entered semaphore closed");
        permit.forget();
    }

    /// Let exactly one parked acquire proceed.
    pub fn release_one(&self) {
        self.release.add_permits(1);
    }

    pub fn acquisitions(&self) -> usize {
        self.acquisitions.load(Ordering::SeqCst)
    }

    pub fn live_leases(&self) -> usize {
        self.live_leases.load(Ordering::SeqCst)
    }
}

impl ProviderTurnGate for GateBarrier {
    fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        let live_leases = self.live_leases.clone();
        let acquisitions = self.acquisitions.clone();
        let block_from = self.block_from;
        Box::pin(async move {
            let index = acquisitions.fetch_add(1, Ordering::SeqCst);
            if index >= block_from {
                entered.add_permits(1);
                release
                    .acquire()
                    .await
                    .map_err(|_| "gate closed".to_string())?
                    .forget();
            }
            live_leases.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(CountedLease(live_leases)) as Box<dyn ProviderTurnLease>)
        })
    }
}

struct CountedLease(Arc<AtomicUsize>);

impl Drop for CountedLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Tool that blocks inside `call` until the test releases it, then succeeds
/// once. Counts calls so a replay would be visible.
pub struct BlockingTool {
    pub name: &'static str,
    pub entered: Arc<tokio::sync::Semaphore>,
    pub release: Arc<tokio::sync::Semaphore>,
    pub calls: Arc<AtomicUsize>,
}

impl BlockingTool {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            entered: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub async fn wait_entered(&self) {
        let permit = self.entered.acquire().await.expect("tool barrier closed");
        permit.forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        })
    }

    fn description(&self) -> &str {
        "blocking test tool"
    }

    async fn call(&self, _args: serde_json::Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        let permit = self
            .release
            .acquire()
            .await
            .expect("tool release semaphore closed");
        permit.forget();
        ToolResult::text("tool-ok")
    }
}

/// Script helpers.
pub fn end_turn_text(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.to_string()),
        ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        },
    ]
}

pub fn tool_use_turn(id: &str, tool: &str, input: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolUseStart {
            id: id.to_string(),
            name: tool.to_string(),
        },
        ProviderEvent::ToolUseDelta {
            id: id.to_string(),
            partial_json: input.to_string(),
        },
        ProviderEvent::ToolUseEnd { id: id.to_string() },
        ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        },
    ]
}

/// Every `tool_use` id in the request history must have exactly one matching
/// `tool_result`, and every result must answer an earlier use.
pub fn assert_tool_use_result_pairing(messages: &[Message]) {
    let mut uses = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        for block in &message.content {
            match block {
                yi_agent_core::ContentBlock::ToolUse { id, .. } => uses.push(id.clone()),
                yi_agent_core::ContentBlock::ToolResult { tool_use_id, .. } => {
                    results.push(tool_use_id.clone())
                }
                _ => {}
            }
        }
    }
    uses.sort();
    results.sort();
    assert_eq!(
        uses, results,
        "tool_use/tool_result ids must pair one-to-one"
    );
}
