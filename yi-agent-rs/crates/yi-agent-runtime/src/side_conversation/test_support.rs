//! Fixtures shared by the side-conversation tests.
//!
//! Everything here is `#[cfg(test)]`: production code must never depend on a test
//! provider or a synthetic history.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::stream;
use yi_agent_core::{
    AgentConfig, Message, Provider, ProviderError, ProviderEvent, ProviderRequest, Session,
    StopReason,
};

use super::source::{EffectiveSideSource, SideWebPolicy};

/// Roots that never change, for tests that do not exercise revocation.
#[derive(Debug)]
pub struct StaticRoots(pub Vec<PathBuf>);

impl yi_agent_tools::SideRoots for StaticRoots {
    fn current_roots(&self) -> Vec<PathBuf> {
        self.0.clone()
    }
}

/// A provider that replies with `reply` and records every request it receives.
pub struct CapturingProvider {
    reply: String,
    requests: Mutex<Vec<ProviderRequest>>,
}

impl CapturingProvider {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request the agent has sent so far.
    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// How many requests the agent has sent.
    pub fn call_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let reply = self.reply.clone();
        Ok(Box::pin(stream::iter(vec![
            ProviderEvent::TextDelta(reply),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ])))
    }
}

/// A capturing provider behind the `Arc<dyn Provider>` the source requires.
pub fn mock_provider(reply: &str) -> Arc<dyn Provider> {
    Arc::new(CapturingProvider::new(reply))
}

/// A parent history with one user turn and one assistant reply.
pub fn source_with_history(user: &str, assistant: &str) -> EffectiveSideSource {
    let mut session = Session::new();
    session.push(Message::user(user));
    session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
        assistant.to_string(),
    )]));
    source_from_session(session)
}

/// A source whose history starts empty, so it must fail before any request.
pub fn source_with_empty_history() -> EffectiveSideSource {
    source_from_session(Session::new())
}

fn source_from_session(session: Session) -> EffectiveSideSource {
    let root = PathBuf::from("/tmp/yi-agent-side-test-root");
    let config = AgentConfig {
        model: "parent-model".to_string(),
        system_prompt: Some("parent prompt".to_string()),
        gen_params: yi_agent_core::GenParams {
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    EffectiveSideSource {
        session: Arc::new(Mutex::new(session)),
        provider: mock_provider("旁支回答"),
        config,
        primary: root.clone(),
        roots: vec![root.clone()],
        current_roots: Arc::new(StaticRoots(vec![root])),
        attachments: Default::default(),
        revision: 1,
        generation: 1,
        provider_gate: None,
        web_policy: SideWebPolicy::default(),
        live: None,
    }
}

/// A provider that blocks until its stream is dropped, for stop/close tests.
///
/// `entered` fires once the request arrived (so a test never guesses with a
/// sleep), and `dropped` proves the real provider stream was torn down rather than
/// merely abandoned by a consumer.
pub struct PendingProvider {
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    calls: Mutex<usize>,
}

impl PendingProvider {
    pub fn new() -> Self {
        Self {
            entered: Arc::new(tokio::sync::Notify::new()),
            dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Mutex::new(0),
        }
    }

    /// Wait until a request has been received.
    pub async fn entered(&self) {
        self.entered.notified().await;
    }

    /// Whether the blocking stream has been dropped.
    pub fn dropped(&self) -> bool {
        self.dropped.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl Default for PendingProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream that never yields and records its own drop.
struct PendingStream {
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    notified: bool,
}

impl futures::Stream for PendingStream {
    type Item = ProviderEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if !self.notified {
            self.notified = true;
            self.entered.notify_waiters();
        }
        let _ = cx.waker().clone();
        std::task::Poll::Pending
    }
}

impl Drop for PendingStream {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Provider for PendingProvider {
    async fn call_stream(
        &self,
        _req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        Ok(Box::pin(PendingStream {
            entered: Arc::clone(&self.entered),
            dropped: Arc::clone(&self.dropped),
            notified: false,
        }))
    }
}

/// A source whose provider blocks until the run is stopped.
pub fn source_with_pending_provider() -> (EffectiveSideSource, Arc<PendingProvider>) {
    let mut source = source_with_history("父问题", "父回答");
    let provider = Arc::new(PendingProvider::new());
    source.provider = provider.clone();
    (source, provider)
}
