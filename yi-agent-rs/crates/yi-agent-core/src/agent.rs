//! Agent loop: think -> act -> observe.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, Message, Role};
use crate::provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason, StreamEnd,
    TokenUsage,
};
use crate::tool::{ToolEvent, ToolRegistry, ToolResult};

use tracing::{Instrument, debug, info, info_span, warn};

/// Shared decision channel used to gate tool execution behind user confirmation.
type DecisionRx = Arc<tokio::sync::Mutex<mpsc::Receiver<(u64, crate::permission::Decision)>>>;

/// A handle held only while one provider request is active. Implementations
/// release their runtime lease when the boxed value is dropped.
pub trait ProviderTurnLease: Send {}

impl<T: Send> ProviderTurnLease for T {}

/// Runtime-owned admission for individual provider turns. This keeps the core
/// loop independent from the store while enforcing real provider boundaries.
pub trait ProviderTurnGate: Send + Sync {
    fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>>;
}

/// In-memory message container. No persistence.
#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
    last_input_tokens: Option<u32>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn last_input_tokens(&self) -> Option<u32> {
        self.last_input_tokens
    }

    pub fn set_last_input_tokens(&mut self, tokens: Option<u32>) {
        self.last_input_tokens = tokens;
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model identifier passed to the provider (e.g. "claude-sonnet-4-5").
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub gen_params: GenParams,
    /// Token count threshold to trigger auto-compact.
    pub compact_threshold: Option<u32>,
    /// Real user input token budget retained during compact.
    pub compact_user_budget_tokens: usize,
    /// Complete raw tool interaction token budget retained during compact.
    pub compact_tool_budget_tokens: usize,
    /// Max idle time (no provider events) during THINK before the stream is
    /// considered stalled. When elapsed, the stream is treated as stalled and
    /// retried up to `think_stall_retry_limit` times before the turn is
    /// interrupted. `None` disables the idle timeout (stream can hang forever,
    /// as before).
    pub think_idle_timeout: Option<std::time::Duration>,
    /// Max automatic retries when the THINK stream stalls (no provider event
    /// within `think_idle_timeout`). `0` disables stall retries.
    pub think_stall_retry_limit: u16,
    /// Base delay for stall retry backoff. Attempt `n` (1-based) waits
    /// `base * 2^(n-1)`, capped at 30s. Defaults to 2s → 2s/4s/8s.
    pub think_stall_backoff_base: std::time::Duration,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".to_string(),
            system_prompt: Some(Self::default_system_prompt()),
            max_turns: Some(200),
            gen_params: Default::default(),
            compact_threshold: Some(100_000),
            compact_user_budget_tokens: crate::compact::DEFAULT_COMPACT_USER_BUDGET_TOKENS,
            compact_tool_budget_tokens: crate::compact::DEFAULT_COMPACT_TOOL_BUDGET_TOKENS,
            // Default idle timeout: 60s between provider events. This is
            // intentionally generous (LLMs can pause between deltas while
            // thinking) but bounded so a stalled connection eventually
            // resolves instead of hanging forever.
            think_idle_timeout: Some(std::time::Duration::from_secs(60)),
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_secs(2),
        }
    }
}

impl AgentConfig {
    /// Built-in system prompt encouraging batch tool calls and persistent
    /// task execution. Used when the user does not provide a custom
    /// `system_prompt`.
    pub fn default_system_prompt() -> String {
        r#"You are yi-agent. You are a helpful general purpose agent designed by Gong Yichen (宫一尘). You have logical thinking, aim for the best, execute perfectly and always speak with evidence.

You work efficiently by minimizing round-trips. Tool use strategy:
- Independent operations (reading multiple files, parallel searches): issue
  MULTIPLE tool calls in a single response. They will be executed in parallel.
- Dependent operations that must run in sequence (create dir → write file →
  run tests): combine them into ONE bash call using && so the whole sequence
  completes in a single step.
- Only split work across turns when a later step genuinely depends on the
  RESULT of an earlier step.

Example: instead of 3 turns (mkdir, write, test), use one bash call:
  mkdir -p src/utils && echo '...' > src/utils/mod.rs && cargo test

Style: Never use emoji in any response. All communication must be plain text only.

Progress narration:
- Say what you are doing while you do it: a response that issues tool calls
  opens with 1-2 sentences of prose saying what you are about to do and why.
- Never let a long run of tool calls go silent. At least every ~10 tool calls,
  stop and narrate where you are, what you found so far, and what is next;
  narrating more often is fine.
- Narration rides along with the tool call in the same response. It never means
  splitting one tool call into several.
- Narrate in the language the user writes in.

Task execution:
- Keep working until the user's request is fully resolved. Only end your
  turn when you are confident the task is complete.
- Verify your work before declaring done: for code changes, run the
  relevant build/test commands; for factual claims, cite the source.
- After writing or editing a file, prefer write/edit tools for file changes; use
  bash primarily for checks or batch mechanical operations.
- If a tool call fails, diagnose the error and retry with a fix rather
  than reporting failure and stopping.
- When information is missing, make a reasonable assumption, state it
  briefly, and continue. Do not stop to ask unless the assumption would
  be risky or irreversible.
- Do not substitute a narrower or easier task for the one requested.

File discovery:
- Avoid unbounded recursive glob calls at repository roots, such as
  glob({"path":".","pattern":"**/*"}). Prefer `rg --files`, targeted
  subdirectories, file-type constrained patterns, or search-first workflows.
- Avoid scanning generated or heavy directories such as `.git/`, `target/`,
  `node_modules/`, `.worktrees/`, caches, and build outputs unless explicitly
  required.

Subagent integration:
- A child runs in the `workdir` you pass to `spawn_agent`. When a change needs
  isolation, create that directory yourself first: `git worktree add <path> -b
  <branch>`, then pass `<path>` as the child's `workdir`. When it does not, pass
  the directory the child should work in directly.
- You are responsible for integrating a delivered commit: run
  `git merge --no-ff <commit>` yourself, resolve any conflicts, and re-run the
  relevant verification. The runtime never creates, tracks, or merges a
  worktree for you."#
            .to_string()
    }
}

/// Agent runtime.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
    permission_checker: Option<Arc<crate::permission::PermissionChecker>>,
    decision_rx: Option<DecisionRx>,
    provider_turn_gate: Option<Arc<dyn ProviderTurnGate>>,
    /// Interjection inbox shared with the spawned `run_loop`, so a test (or a
    /// caller holding an `InboxHandle`) can deliver without a live `&Agent`.
    /// Only present while a run is active.
    inbox: Option<InboxHandle>,
}

/// Events emitted during agent loop.
#[derive(Debug, Clone, Serialize)]
pub enum AgentEvent {
    Start,
    AssistantText(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        result: ToolResult,
    },
    /// An explicitly retryable tool failed and is being retried internally.
    ToolRetry {
        id: String,
    },
    /// The THINK stream failed transiently and is being retried internally.
    /// `attempt` is the 1-based retry number, `max` the configured retry limit,
    /// `idle_secs` how long the stream was silent, and `cause` why it retried.
    /// Consumers must surface this to the user: a silent retry leaves the user
    /// staring at a frozen screen.
    ProviderRetry {
        attempt: u16,
        max: u16,
        idle_secs: u64,
        cause: RetryCause,
    },
    ToolOutputDelta {
        id: String,
        stream: crate::tool::OutputStream,
        text: String,
    },
    ToolExit {
        id: String,
        code: Option<i32>,
    },
    ToolTimeout {
        id: String,
    },
    Usage {
        model: String,
        usage: TokenUsage,
    },
    /// Heuristic estimate of prefill (input) tokens, emitted before the
    /// provider returns real usage. Lets the status bar show activity.
    EstimatedPrefill(u32),
    /// Streamed delta that counts toward decode (output) tokens but is not
    /// assistant-visible text (e.g. tool-call argument JSON). Used by the
    /// status bar for flow-style decode estimation during tool-call turns.
    DecodeDelta(String),
    Done {
        reason: DoneReason,
    },
    /// Auto-compact 完成事件。old_msg_count 是 compact 前的消息数,
    /// new_msg_count 是 compact 后(含 summary + 保留轮)。
    AutoCompacting {
        old_msg_count: usize,
        new_msg_count: usize,
    },
    /// Manual `/compact` completed and replaced the current session.
    ManualCompacted {
        old_msg_count: usize,
        new_msg_count: usize,
    },
    /// Manual `/compact` failed before it could replace the current session.
    ManualCompactFailed {
        message: String,
    },
    Cancelled,
    /// A mid-turn user message was pushed into the session as a user message.
    /// Emitted only after the text is actually in the transcript, so consumers
    /// can promote a "delivered, pending" entry to a real one.
    InterjectionAccepted {
        seq: u64,
        text: String,
        tag: Option<String>,
    },
    /// Mid-turn user messages that were never consumed, handed back on any
    /// terminal exit. MUST be emitted before `Cancelled` / `Done`.
    InterjectionsReturned {
        items: Vec<Interjection>,
    },
    /// The subagent runtime the user asked for could not be started or
    /// activated, so this session runs without delegation.
    ///
    /// Distinct from `Error`: bring-up is best-effort and the session stays
    /// usable, so consumers must report this as a notice that names the remedy
    /// (restart) instead of rendering it as a turn failure. `stage` is the
    /// internal bring-up step and `cause` the raw diagnostic; both are for the
    /// trace and for non-TUI consumers, not for the user-visible line.
    SubagentRuntimeUnavailable {
        stage: String,
        cause: String,
    },
    Error(AgentError),
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        tool_input: Value,
        prefix_suggestion: Option<String>,
        kind: crate::permission::PermissionKind,
    },
    PermissionResolved {
        request_id: u64,
        decision: crate::permission::Decision,
    },
}

/// Why a THINK attempt was retried. Distinguishes the two transient failure
/// modes so consumers can explain the delay accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RetryCause {
    /// No provider event within the idle timeout.
    IdleStall,
    /// The provider request hit its total-deadline timeout.
    RequestTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DoneReason {
    EndTurn,
    MaxTurns,
    Interrupted { reason: String },
}

const CONTINUE_AFTER_TRUNCATION: &str =
    "Continue the interrupted task from where you stopped. Do not repeat completed work.";

/// Wrapper placed around user text that arrives mid-turn, so the model reads it
/// as a revision of the current task rather than a brand-new request.
const INTERJECTION_PREFIX: &str = "The user added the following requirement while you were working on this task. \
Fold it into the current task without repeating work you have already completed:\n";

/// One user message submitted while a turn was in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Interjection {
    /// Monotonic token assigned by `Inbox`; the frontend's reconciliation key.
    pub seq: u64,
    pub text: String,
    /// Caller-supplied reconciliation id. `None` when the caller does not need
    /// cross-process matching (the TUI).
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InterjectError {
    #[error("interjection inbox is full")]
    Full,
    #[error("no run is active")]
    NotRunning,
}

/// Bounded FIFO of interjections, drained by `run_loop`.
#[derive(Debug)]
pub struct Inbox {
    items: VecDeque<Interjection>,
    next_seq: u64,
}

impl Inbox {
    /// Matches `PendingQueue::CAPACITY` and the TUI input channel capacity.
    pub const CAPACITY: usize = 16;

    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
            next_seq: 0,
        }
    }

    pub fn push(&mut self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        if self.items.len() >= Self::CAPACITY {
            return Err(InterjectError::Full);
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        self.items.push_back(Interjection { seq, text, tag });
        Ok(seq)
    }

    pub fn drain_all(&mut self) -> Vec<Interjection> {
        self.items.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Cloneable delivery handle for the active run's inbox.
#[derive(Debug, Clone)]
pub struct InboxHandle(Arc<Mutex<Inbox>>);

impl InboxHandle {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Inbox::new())))
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.0.lock().unwrap()
    }

    pub fn interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        self.lock().push(text, tag)
    }
}

impl Default for InboxHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("provider turn admission failed: {0}")]
    ProviderTurnAdmission(String),
    #[error("compaction error: {0}")]
    Compact(#[from] crate::compact::CompactError),
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            session: Arc::new(Mutex::new(Session::new())),
            config,
            cancel_token: CancellationToken::new(),
            permission_checker: None,
            decision_rx: None,
            provider_turn_gate: None,
            inbox: None,
        }
    }

    pub fn with_session(self, session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ..self
        }
    }

    /// Restrict provider turns through the daemon-owned admission gate.
    pub fn with_provider_turn_gate(mut self, gate: Arc<dyn ProviderTurnGate>) -> Self {
        self.provider_turn_gate = Some(gate);
        self
    }

    /// Attach a permission checker and decision channel for tool gating.
    ///
    /// `decision_rx` is shared via `Arc<Mutex<Receiver>>` so that the same
    /// receiver can be re-attached when the agent is reconstructed (e.g. on
    /// `/clear` or `/model` in inline mode).
    pub fn with_permission(
        mut self,
        checker: Arc<crate::permission::PermissionChecker>,
        decision_rx: DecisionRx,
    ) -> Self {
        self.permission_checker = Some(checker);
        self.decision_rx = Some(decision_rx);
        self
    }

    /// Replace the system prompt used by subsequent runs.
    ///
    /// `run()` clones the config at the start of each run, so setting this
    /// before `run()` takes effect for that run. Used by hot-reload paths
    /// (e.g. the skills catalog) to refresh the prompt between messages.
    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        self.config.system_prompt = prompt;
    }

    pub fn session(&self) -> Session {
        self.session.lock().unwrap().clone()
    }

    /// Trigger cancellation. The run loop will exit at the nearest check point.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Get a clone of the cancellation token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Deliver a mid-turn user message. Returns the inbox sequence number, or
    /// `NotRunning` when no run is active / `Full` when the inbox is saturated.
    pub fn interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError> {
        match &self.inbox {
            Some(handle) => handle.interject(text, tag),
            None => Err(InterjectError::NotRunning),
        }
    }

    /// Handle for the active run's inbox. A caller can clone it before moving
    /// the event stream away and still deliver into that same run.
    pub fn inbox_handle(&self) -> Option<InboxHandle> {
        self.inbox.clone()
    }

    /// Run the agent loop, returning a stream of events.
    pub async fn run(
        &mut self,
        user_prompt: String,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.start_run(Some(user_prompt)).await
    }

    /// Restarts execution from the current session after a transient provider
    /// failure without duplicating the user prompt already in that session.
    pub async fn retry_current_session(
        &mut self,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.start_run(None).await
    }

    async fn start_run(
        &mut self,
        user_prompt: Option<String>,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        // Every run uses a fresh cancel token.
        self.cancel_token = CancellationToken::new();
        // Each run gets a fresh inbox: a handle from the previous run must not
        // feed the next one.
        self.inbox = Some(InboxHandle::new());
        if let Some(user_prompt) = user_prompt {
            self.session
                .lock()
                .unwrap()
                .push(Message::user(user_prompt));
        }

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let session = self.session.clone();
        let cancel_token = self.cancel_token.clone();
        let permission_checker = self.permission_checker.clone();
        let decision_rx = self.decision_rx.clone();
        let provider_turn_gate = self.provider_turn_gate.clone();
        // Clone before the loop owns it: the `Agent` keeps its handle so
        // `interject()` works even after the stream has been moved away.
        let inbox = self.inbox.clone();

        let (tx, rx) = mpsc::unbounded_channel();
        let tx = EventTx(tx);
        tokio::spawn(async move {
            if tx.send(AgentEvent::Start).await.is_err() {
                return; // Receiver dropped, stop the loop
            }
            run_loop(
                tx,
                provider,
                tools,
                session,
                config,
                cancel_token,
                permission_checker,
                decision_rx,
                provider_turn_gate,
                inbox,
            )
            .await;
        });

        Ok(tokio_stream::wrappers::UnboundedReceiverStream::new(rx).boxed())
    }
}

/// Rollback length for a cancelled run under the "preserve completed work"
/// policy: keep this run's user prompt plus every assistant/tool round-trip that
/// already completed, and drop only a trailing assistant message whose tool_use
/// has no matching tool_result.
///
/// A cancel during THINK leaves the session ending on the user message (nothing
/// is dropped); a cancel during ACT leaves a trailing `assistant(tool_use)` whose
/// results were never observed, which must go or the next provider request is
/// rejected for an unpaired tool_use.
fn safe_cancel_truncate_len(session: &Session) -> usize {
    let messages = session.messages();
    match messages.last() {
        Some(last) if last.role == Role::Assistant && has_tool_use(last) => messages.len() - 1,
        // The session is empty, ends on the user prompt, or ends on an already
        // paired tool_result round-trip: everything present is safe to keep.
        _ => messages.len(),
    }
}

/// Whether a message carries any `tool_use` block (and therefore needs a
/// following `tool_result` to keep the transcript valid for the provider).
fn has_tool_use(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
}

/// Delay before stall retry `attempt` (1-based): `base * 2^(attempt-1)`,
/// capped at 30s to match the runtime retry policy.
fn stall_backoff_delay(base: std::time::Duration, attempt: u16) -> std::time::Duration {
    let shift = u32::from(attempt.saturating_sub(1)).min(5);
    let scaled = base.saturating_mul(1u32 << shift);
    scaled.min(std::time::Duration::from_secs(30))
}

/// Sender half of the agent's event stream.
///
/// Backed by an **unbounded** channel so streamed events are never dropped when
/// the consumer renders more slowly than the provider emits. Previously this
/// was a bounded `mpsc::Sender` whose streamed-text forwarding used `try_send`,
/// which silently discarded deltas — including the tail of long messages — as
/// soon as the buffer filled (see
/// `lagging_consumer_still_receives_every_text_delta`).
///
/// The method shapes mirror `tokio::sync::mpsc::Sender` (`async send` +
/// `try_send`) so call sites read uniformly. Neither call blocks, and both only
/// fail once the receiver has been dropped.
#[derive(Clone)]
struct EventTx(tokio::sync::mpsc::UnboundedSender<AgentEvent>);

impl EventTx {
    fn send(
        &self,
        event: AgentEvent,
    ) -> std::future::Ready<Result<(), tokio::sync::mpsc::error::SendError<AgentEvent>>> {
        std::future::ready(self.0.send(event))
    }

    fn try_send(
        &self,
        event: AgentEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<AgentEvent>> {
        self.0.send(event)
    }
}

/// Move every queued interjection into the transcript, announcing each one.
///
/// Returns `true` when at least one message was injected, so the `EndTurn` check
/// can decide whether to keep looping.
async fn inject_pending(
    tx: &EventTx,
    inbox: &Option<InboxHandle>,
    messages: &mut Vec<Message>,
    session: &Arc<Mutex<Session>>,
) -> bool {
    let Some(handle) = inbox else {
        return false;
    };
    let pending = handle.lock().drain_all();
    if pending.is_empty() {
        return false;
    }
    for item in pending {
        let text = format!("{INTERJECTION_PREFIX}{}", item.text);
        messages.push(Message::user(text.clone()));
        session.lock().unwrap().push(Message::user(text));
        let _ = tx
            .send(AgentEvent::InterjectionAccepted {
                seq: item.seq,
                text: item.text,
                tag: item.tag,
            })
            .await;
    }
    true
}

/// Give back every interjection that never made it into the transcript.
///
/// Called before each terminal event. A user message that is neither accepted
/// nor returned is silently lost, so this runs on every exit path that can be
/// reached while the inbox may still hold something.
async fn flush_unconsumed(tx: &EventTx, inbox: &Option<InboxHandle>) {
    let Some(handle) = inbox else {
        return;
    };
    let items = handle.lock().drain_all();
    if items.is_empty() {
        return;
    }
    let _ = tx.send(AgentEvent::InterjectionsReturned { items }).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    tx: EventTx,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
    permission_checker: Option<Arc<crate::permission::PermissionChecker>>,
    decision_rx: Option<DecisionRx>,
    provider_turn_gate: Option<Arc<dyn ProviderTurnGate>>,
    inbox: Option<InboxHandle>,
) {
    let mut messages = session.lock().unwrap().messages().to_vec();
    let mut turn = 0u32;
    // Cursor for incremental request logging: only log messages[last_logged..] each turn.
    let mut last_logged = 0usize;

    let model = config.model.clone();
    let loop_span = info_span!("agent_loop", model = %model, msg_count = messages.len());
    let _loop_enter = loop_span.enter();

    loop {
        // Check 1: THINK 前
        if cancel_token.is_cancelled() {
            info!(turn, "agent loop cancelled before think");
            flush_unconsumed(&tx, &inbox).await;
            let _ = tx.send(AgentEvent::Cancelled).await;
            return;
        }

        if let Some(new_messages) = maybe_auto_compact(&tx, &provider, &config, &session).await {
            messages = new_messages;
            // Compaction replaced the history, so the incremental request-log
            // cursor no longer refers to this message vector.
            last_logged = 0;
        }

        turn += 1;
        if let Some(max) = config.max_turns {
            if turn > max {
                info!(turn, max, "agent loop reached max turns");
                flush_unconsumed(&tx, &inbox).await;
                if tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::MaxTurns,
                    })
                    .await
                    .is_err()
                {
                    return; // Receiver dropped, stop the loop
                }
                return;
            }
        }

        // Fold in anything the user sent while the previous request/tools ran.
        // Placed after the max_turns check (so an interjection consumes a turn)
        // and before the request-delta log (so it shows up there).
        inject_pending(&tx, &inbox, &mut messages, &session).await;

        info!(turn, msg_count = messages.len(), "think: calling provider");

        // Log the request delta (only new messages since last turn) at debug level.
        // Avoids O(N^2) duplication of conversation history across turns.
        debug!(
            turn,
            system = ?config.system_prompt,
            new_msgs = ?&messages[last_logged..],
            "think: request delta"
        );
        last_logged = messages.len();

        // 1. THINK
        let req = ProviderRequest {
            model: config.model.clone(),
            system: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: tools.schemas(),
            params: config.gen_params.clone(),
        };
        // Emit a heuristic prefill estimate so the status bar shows activity
        // before the provider returns real usage (OpenAI-compatible APIs only
        // send usage at stream end).
        let prefill_estimate = estimate_prefill_tokens(&req);
        let _ = tx.try_send(AgentEvent::EstimatedPrefill(prefill_estimate));

        // Retry loop for a transiently failed THINK stream (idle stall or
        // request deadline). Each attempt is a fresh provider turn (own lease);
        // a failed attempt's partial content is not committed to the session.
        let mut stall_retries: u16 = 0;
        let (content, end, last_usage) = loop {
            let provider_turn_lease = match &provider_turn_gate {
                Some(gate) => match tokio::select! {
                    lease = gate.acquire() => lease,
                    _ = cancel_token.cancelled() => {
                        flush_unconsumed(&tx, &inbox).await;
                        let _ = tx.send(AgentEvent::Cancelled).await;
                        return;
                    }
                } {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let _ = tx
                            .send(AgentEvent::Error(AgentError::ProviderTurnAdmission(error)))
                            .await;
                        return;
                    }
                },
                None => None,
            };

            let stream = match provider.call_stream(req.clone()).await {
                Ok(s) => {
                    tracing::info!(
                        turn,
                        "provider call_stream returned Ok, entering accumulate"
                    );
                    s
                }
                Err(e) => {
                    warn!(turn, error = %e, "provider call failed");
                    if tx
                        .send(AgentEvent::Error(AgentError::Provider(e)))
                        .await
                        .is_err()
                    {
                        return; // Receiver dropped, stop the loop
                    }
                    return;
                }
            };

            // Check 2: THINK 中 — select! between accumulate and cancel
            let attempt = tokio::select! {
                result = accumulate_provider_stream(stream, &tx, &model, config.think_idle_timeout) => match result {
                    Ok(v) => {
                        tracing::info!(turn, stop_reason = ?v.1, content_blocks = v.0.len(), "accumulate returned Ok");
                        v
                    }
                    Err(e) => {
                        warn!(turn, error = %e, "provider stream error");
                        if tx.send(AgentEvent::Error(e)).await.is_err() {
                            return;
                        }
                        return;
                    }
                },
                _ = cancel_token.cancelled() => {
                    info!(turn, "agent loop cancelled during think");
                    // Cancel during THINK: no assistant reply exists yet, so this
                    // keeps the run's user prompt and every completed round-trip.
                    let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                    session.lock().unwrap().truncate(keep);
                    flush_unconsumed(&tx, &inbox).await;
                    let _ = tx.send(AgentEvent::Cancelled).await;
                    return;
                }
            };

            // Release the turn lease before sleeping: a retry backoff must not
            // hold provider capacity.
            drop(provider_turn_lease);

            // Two transient modes are retryable: an idle stall and a request
            // deadline. Everything else (malformed SSE payloads, connection
            // resets, auth failures) is terminal.
            //
            // `Failed(Network)` is a load-bearing shorthand for "the request
            // deadline elapsed": the providers only classify a *timeout* as
            // `Network` here, and keep every other transport break as `Stream`.
            // Widening that classification would silently start retrying
            // connection resets, which the design explicitly excludes.
            let retry_cause = match &attempt.1 {
                StreamEnd::Stopped(StopReason::Stalled) => Some(RetryCause::IdleStall),
                StreamEnd::Failed(ProviderError::Network(_)) => Some(RetryCause::RequestTimeout),
                _ => None,
            };
            if let Some(cause) = retry_cause {
                if stall_retries < config.think_stall_retry_limit {
                    stall_retries += 1;
                    let delay = stall_backoff_delay(config.think_stall_backoff_base, stall_retries);
                    let idle_secs = config.think_idle_timeout.map(|t| t.as_secs()).unwrap_or(0);
                    tracing::warn!(
                        turn,
                        attempt = stall_retries,
                        max = config.think_stall_retry_limit,
                        delay_ms = delay.as_millis() as u64,
                        ?cause,
                        "think phase failed transiently; retrying after backoff"
                    );
                    // Surface the retry: a silent backoff looks like a hang.
                    if tx
                        .send(AgentEvent::ProviderRetry {
                            attempt: stall_retries,
                            max: config.think_stall_retry_limit,
                            idle_secs,
                            cause,
                        })
                        .await
                        .is_err()
                    {
                        return; // Receiver dropped, stop the loop
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel_token.cancelled() => {
                            info!(turn, "agent loop cancelled during retry backoff");
                            // Same policy as the other cancel points: keep the
                            // run's user prompt and completed round-trips.
                            let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                            session.lock().unwrap().truncate(keep);
                            flush_unconsumed(&tx, &inbox).await;
                            let _ = tx.send(AgentEvent::Cancelled).await;
                            return;
                        }
                    }
                    continue;
                }
            }
            break attempt;
        };

        if let Some(usage) = last_usage {
            session
                .lock()
                .unwrap()
                .set_last_input_tokens(Some(usage.input_tokens));
        }

        // Log the full accumulated response content at debug level (never repeats across turns).
        debug!(turn, content = ?content, "think: response");

        // A non-retryable transport failure (malformed SSE payload, connection
        // reset, ...) is terminal. Retryable timeouts never reach here: they
        // either succeeded on a later attempt or exhausted the retry budget,
        // in which case the terminal reason below explains it.
        if let StreamEnd::Failed(error) = &end {
            if !matches!(error, ProviderError::Network(_)) {
                let _ = tx
                    .send(AgentEvent::Error(AgentError::Provider(error.clone())))
                    .await;
                return;
            }
        }

        // An exhausted transient failure is terminal and its partial must NOT
        // enter the session: a stream can end mid-tool-call, leaving an
        // unpaired `tool_use` that would make the next provider request
        // invalid. The same applies to a failed attempt (no valid stop).
        let exhausted_stall = matches!(end, StreamEnd::Stopped(StopReason::Stalled));
        let exhausted_timeout = matches!(end, StreamEnd::Failed(ProviderError::Network(_)));
        if !exhausted_stall && !exhausted_timeout {
            messages.push(Message::assistant(content.clone()));
            session
                .lock()
                .unwrap()
                .push(Message::assistant(content.clone()));
        }

        let stop_reason = match end {
            StreamEnd::Stopped(reason) => reason,
            StreamEnd::Failed(ProviderError::Network(_)) => {
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: format!("request timeout after {stall_retries} retries"),
                        },
                    })
                    .await;
                return;
            }
            // Any other failure was already reported as a terminal Error above.
            StreamEnd::Failed(_) => return,
        };

        match stop_reason {
            StopReason::Stalled => {
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: format!("idle timeout after {stall_retries} retries"),
                        },
                    })
                    .await;
                return;
            }
            StopReason::EndTurn => {}
            StopReason::MaxTokens => {
                messages.push(Message::user(CONTINUE_AFTER_TRUNCATION));
                session
                    .lock()
                    .unwrap()
                    .push(Message::user(CONTINUE_AFTER_TRUNCATION));
                continue;
            }
            StopReason::StopSequence => {
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: "stop sequence".into(),
                        },
                    })
                    .await;
                return;
            }
            StopReason::Other(reason) => {
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted { reason },
                    })
                    .await;
                return;
            }
        }

        // 2. Termination check
        let tool_uses: Vec<(String, String, Value)> = content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolUse { id, name, input } = b {
                    Some((id.clone(), name.clone(), input.clone()))
                } else {
                    None
                }
            })
            .collect();

        if tool_uses.is_empty() {
            // A message can land after the model's last tool call but before the
            // loop decides to finish. Check once more so it joins this turn
            // instead of being replayed as a brand-new prompt later.
            if inject_pending(&tx, &inbox, &mut messages, &session).await {
                continue;
            }
            info!(turn, "agent loop done: end_turn");
            tracing::info!(turn, "emitting AgentEvent::Done(EndTurn)");
            flush_unconsumed(&tx, &inbox).await;
            if tx
                .send(AgentEvent::Done {
                    reason: DoneReason::EndTurn,
                })
                .await
                .is_err()
            {
                return; // Receiver dropped, stop the loop
            }
            return;
        }

        // 3. ACT - permission check + parallel execution
        info!(turn, tool_count = tool_uses.len(), tools = ?tool_uses.iter().map(|(_, n, _)| n.as_str()).collect::<Vec<_>>(), "act: executing tools");

        // 权限检查阶段: 在并行执行前逐个检查,过滤被拒绝的工具
        let mut checked_uses: Vec<(String, String, Value)> = Vec::new();
        let mut denied_results: Vec<(String, ToolResult)> = Vec::new();
        for (id, name, input) in tool_uses {
            if let Some(checker) = &permission_checker {
                let check_result = checker.check(&name, &input);
                match check_result {
                    crate::permission::CheckResult::Allow => {
                        checked_uses.push((id, name, input));
                    }
                    crate::permission::CheckResult::Deny => {
                        let _ = tx
                            .send(AgentEvent::ToolResult {
                                id: id.clone(),
                                result: ToolResult::error("permission denied"),
                            })
                            .await;
                        denied_results.push((id.clone(), ToolResult::error("permission denied")));
                    }
                    crate::permission::CheckResult::NeedConfirm(req) => {
                        if let Some(decision_rx) = &decision_rx {
                            let id_clone = id.clone();
                            match handle_confirmation(
                                &tx,
                                checker,
                                decision_rx,
                                &cancel_token,
                                id,
                                name,
                                input,
                                req,
                                "user denied",
                            )
                            .await
                            {
                                Some((id, name, input)) => checked_uses.push((id, name, input)),
                                None => denied_results
                                    .push((id_clone, ToolResult::error("user denied"))),
                            }
                        } else {
                            // No decision channel - deny by default
                            let _ = tx
                                .send(AgentEvent::ToolResult {
                                    id: id.clone(),
                                    result: ToolResult::error(
                                        "permission required but no decision channel",
                                    ),
                                })
                                .await;
                            denied_results.push((
                                id.clone(),
                                ToolResult::error("permission required but no decision channel"),
                            ));
                        }
                    }
                    crate::permission::CheckResult::Blacklisted(req) => {
                        // 黑名单是硬红线:不可通过 Allow once / Always allow 绕过,
                        // 因此不走确认流程。这里仍先发 ToolCall,让 TUI 能渲染出
                        // 这次被拒的调用;否则拒绝会变成静默,用户会误以为命令已执行。
                        let reason = match &req.kind {
                            crate::permission::PermissionKind::Blacklisted(reason) => {
                                reason.clone()
                            }
                            _ => "blacklisted command".to_string(),
                        };
                        let message = format!("blocked by safety filter: {reason}");
                        let _ = tx
                            .send(AgentEvent::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            })
                            .await;
                        let _ = tx
                            .send(AgentEvent::ToolResult {
                                id: id.clone(),
                                result: ToolResult::error(message.clone()),
                            })
                            .await;
                        denied_results.push((id.clone(), ToolResult::error(message)));
                    }
                }
            } else {
                // No permission checker - allow all (backward compatible)
                checked_uses.push((id, name, input));
            }
        }

        let futures: Vec<_> = checked_uses
            .iter()
            .map(|(id, name, input)| {
                let tools = tools.clone();
                let tx = tx.clone();
                async move {
                    let tool_span = info_span!("tool_call", tool = %name, id = %id);
                    let _enter = tool_span.enter();
                    info!(input = %input, "tool call start");

                    if tx
                        .send(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return (id.clone(), None);
                    }

                    let tool = match tools.get(name) {
                        Some(t) => t,
                        None => {
                            let result = ToolResult::error(format!("tool not found: {}", name));
                            let _ = tx
                                .send(AgentEvent::ToolResult {
                                    id: id.clone(),
                                    result: result.clone(),
                                })
                                .await;
                            return (id.clone(), Some(result));
                        }
                    };

                    // Set up streaming channel + forwarder
                    let (event_tx, mut event_rx) = mpsc::channel::<ToolEvent>(64);
                    let fwd_tx = tx.clone();
                    let fwd_id = id.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = event_rx.recv().await {
                            let agent_ev = match ev {
                                ToolEvent::OutputDelta { stream, text } => {
                                    AgentEvent::ToolOutputDelta {
                                        id: fwd_id.clone(),
                                        stream,
                                        text,
                                    }
                                }
                                ToolEvent::Exit { code } => AgentEvent::ToolExit {
                                    id: fwd_id.clone(),
                                    code,
                                },
                                ToolEvent::Timeout => {
                                    AgentEvent::ToolTimeout { id: fwd_id.clone() }
                                }
                                ToolEvent::Truncated { .. } => continue,
                            };
                            let _ = fwd_tx.send(agent_ev).await;
                        }
                    });

                    let result = tool.call_stream(input.clone(), event_tx).await;

                    info!(is_error = result.is_error, "tool call done");

                    if tx
                        .send(AgentEvent::ToolResult {
                            id: id.clone(),
                            result: result.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return (id.clone(), None);
                    }

                    (id.clone(), Some(result))
                }
                .instrument(info_span!("tool", name = %name, id = %id))
            })
            .collect();

        // Check 3: ACT 中 — select! between join_all and cancel
        let results = tokio::select! {
            r = futures::future::join_all(futures) => r,
            _ = cancel_token.cancelled() => {
                info!(turn, "agent loop cancelled during act");
                // Cancel during ACT: the session holds this run's user prompt,
                // every completed round-trip, and a trailing assistant(tool_use)
                // whose results were never observed. Dropping that last message
                // keeps the transcript valid (no unpaired tool_use) while
                // preserving the completed work.
                let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                session.lock().unwrap().truncate(keep);
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
        };

        // 4. OBSERVE - feed results back in tool_use_id order
        let mut tool_results: Vec<ContentBlock> = results
            .into_iter()
            .filter_map(|(id, result)| {
                result.map(|r| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: r.content,
                    is_error: r.is_error,
                })
            })
            .collect();
        // Add denied tool results so LLM sees them
        for (id, result) in denied_results {
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.content,
                is_error: result.is_error,
            });
        }
        let tool_results_msg = Message::tool_results(tool_results);
        messages.push(tool_results_msg.clone());
        session.lock().unwrap().push(tool_results_msg);
    }
}

/// Wait for a user decision matching `expected_id` on the decision channel.
/// Discards any mismatched messages (defensive) and returns `Deny` if the
/// channel is closed or the cancel token is triggered.
async fn wait_for_decision(
    decision_rx: &DecisionRx,
    expected_id: u64,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> crate::permission::Decision {
    let mut rx = decision_rx.lock().await;
    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return crate::permission::Decision::Deny,
            msg = rx.recv() => match msg {
                Some((id, d)) if id == expected_id => return d,
                Some(_) => continue,
                None => return crate::permission::Decision::Deny,
            }
        }
    }
}

/// Handles a permission request that needs user confirmation (NeedConfirm only).
/// Blacklisted commands never reach here: they are hard-denied in the check loop.
/// Sends PermissionRequest event, waits for decision, sends PermissionResolved event.
/// Returns Some((id, name, input)) if user allows execution, None if user denies.
#[allow(clippy::too_many_arguments)]
async fn handle_confirmation(
    tx: &EventTx,
    checker: &Arc<crate::permission::PermissionChecker>,
    decision_rx: &DecisionRx,
    cancel_token: &tokio_util::sync::CancellationToken,
    id: String,
    name: String,
    input: Value,
    req: crate::permission::PermissionRequest,
    deny_message: &str,
) -> Option<(String, String, Value)> {
    let _ = tx
        .send(AgentEvent::PermissionRequest {
            request_id: req.request_id,
            tool_name: req.tool_name.clone(),
            tool_input: req.tool_input.clone(),
            prefix_suggestion: req.prefix_suggestion.clone(),
            kind: req.kind.clone(),
        })
        .await;

    let decision = wait_for_decision(decision_rx, req.request_id, cancel_token).await;

    let _ = tx
        .send(AgentEvent::PermissionResolved {
            request_id: req.request_id,
            decision: decision.clone(),
        })
        .await;

    match decision {
        crate::permission::Decision::AllowOnce
        | crate::permission::Decision::AlwaysAllowTool
        | crate::permission::Decision::AlwaysAllowPrefix(_) => {
            if let Err(e) = checker.apply_decision(&name, &decision, &req.kind).await {
                tracing::warn!("failed to persist permission decision: {e}");
            }
            Some((id, name, input))
        }
        crate::permission::Decision::Deny => {
            let _ = tx
                .send(AgentEvent::ToolResult {
                    id: id.clone(),
                    result: ToolResult::error(deny_message),
                })
                .await;
            None
        }
    }
}

async fn maybe_auto_compact(
    tx: &EventTx,
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    session: &Arc<Mutex<Session>>,
) -> Option<Vec<Message>> {
    let threshold = config
        .compact_threshold
        .filter(|threshold| *threshold > 0)?;
    let snapshot = session.lock().unwrap().clone();
    if snapshot.last_input_tokens()? < threshold {
        return None;
    }

    let old_count = snapshot.len();
    match crate::compact::compact_session(provider, config, &snapshot).await {
        Ok(Some(new_session)) if new_session.len() < old_count => {
            let new_messages = new_session.messages().to_vec();
            {
                let mut current = session.lock().unwrap();
                current.replace_messages(new_messages.clone());
                current.set_last_input_tokens(None);
            }
            let _ = tx
                .send(AgentEvent::AutoCompacting {
                    old_msg_count: old_count,
                    new_msg_count: new_messages.len(),
                })
                .await;
            Some(new_messages)
        }
        Ok(Some(_)) | Ok(None) => None,
        Err(error) => {
            tracing::warn!(error = %error, "auto-compact failed, will retry next think");
            None
        }
    }
}
async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &EventTx,
    model: &str,
    idle_timeout: Option<std::time::Duration>,
) -> Result<(Vec<ContentBlock>, StreamEnd, Option<TokenUsage>), AgentError> {
    let tx = tx.clone();
    let model = model.to_string();
    let (content, end, last_usage) = crate::provider::accumulate_stream(
        stream,
        move |event| match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage {
                    model: model.clone(),
                    usage: u,
                });
            }
            ProviderEvent::ToolUseDelta { partial_json, .. } => {
                let _ = tx.try_send(AgentEvent::DecodeDelta(partial_json));
            }
            _ => {}
        },
        idle_timeout,
    )
    .await?;
    Ok((content, end, last_usage))
}

/// Heuristic token estimate: ASCII ~4 chars/token, non-ASCII (CJK etc.) ~1.5 chars/token.
fn estimate_tokens(text: &str) -> u32 {
    let mut ascii = 0u32;
    let mut non_ascii = 0u32;
    for c in text.chars() {
        if (c as u32) < 0x80 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    (ascii as f32 / 4.0 + non_ascii as f32 / 1.5) as u32
}

/// Estimate prefill tokens from the full request (system + tools + all messages).
fn estimate_prefill_tokens(req: &crate::provider::ProviderRequest) -> u32 {
    let mut total = req.system.as_deref().map(estimate_tokens).unwrap_or(0);
    // Tools schema (name + description + input_schema JSON) is part of prefill.
    for tool in &req.tools {
        total += estimate_tokens(&tool.name);
        total += estimate_tokens(&tool.description);
        total += estimate_tokens(&tool.input_schema.to_string());
    }
    for msg in &req.messages {
        for block in &msg.content {
            match block {
                crate::message::ContentBlock::Text(t) => total += estimate_tokens(t),
                crate::message::ContentBlock::ToolUse { name, input, .. } => {
                    total += estimate_tokens(name);
                    total += estimate_tokens(&input.to_string());
                }
                crate::message::ContentBlock::ToolResult { content, .. } => {
                    for b in content {
                        if let crate::message::ContentBlock::Text(t) = b {
                            total += estimate_tokens(t);
                        }
                    }
                }
                crate::message::ContentBlock::Image { .. } => {
                    total += crate::compact::IMAGE_TOKEN_ESTIMATE as u32;
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Role};
    use crate::permission::{Decision, PermissionKind};
    use crate::provider::{
        GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason,
    };
    use crate::tool::{Tool, ToolMetadata, ToolRegistry, ToolResult};
    use async_trait::async_trait;
    use futures::stream::BoxStream;

    /// Provider that returns a fixed sequence of events.
    /// Each call returns the next script; if scripts exhausted, returns empty (EndTurn).
    struct ScriptedProvider {
        scripts: Vec<Vec<ProviderEvent>>,
        call_index: std::sync::Mutex<usize>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                scripts,
                call_index: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let script = self.scripts.get(*idx).cloned().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }]
            });
            *idx += 1;
            Ok(futures::stream::iter(script).boxed())
        }
    }

    struct UpperEchoTool;

    #[async_trait]
    impl Tool for UpperEchoTool {
        fn name(&self) -> &str {
            "upper"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn description(&self) -> &str {
            "Uppercases text"
        }
        async fn call(&self, args: serde_json::Value) -> ToolResult {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ToolResult::text(text.to_uppercase())
        }
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                read_only: true,
                ..Default::default()
            }
        }
    }

    fn collect_events(stream: BoxStream<'static, AgentEvent>) -> Vec<AgentEvent> {
        futures::executor::block_on_stream(stream).collect()
    }

    /// A provider whose response is fed event-by-event from the test, so the
    /// test knows precisely when the stream has been consumed. Used to place an
    /// interjection deterministically inside a request's lifetime.
    struct FedProvider {
        tx: tokio::sync::Mutex<Option<mpsc::Sender<ProviderEvent>>>,
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    impl FedProvider {
        fn new() -> Self {
            Self {
                tx: tokio::sync::Mutex::new(None),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn messages_of(&self, index: usize) -> Vec<Message> {
            self.seen.lock().unwrap()[index].clone()
        }

        /// Wait until request `index` is open, then hand it one event.
        ///
        /// A `Stop` closes the stream: `ReceiverStream` only ends once every
        /// sender is dropped, so leaving the sender alive would make the
        /// provider stream (and therefore the agent loop) hang forever.
        async fn feed_to(&self, index: usize, event: ProviderEvent) {
            let mut event = Some(event);
            let is_stop = matches!(event.as_ref().unwrap(), ProviderEvent::Stop { .. });
            loop {
                if self.request_count() > index {
                    let mut guard = self.tx.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        tx.send(event.take().unwrap()).await.unwrap();
                        if is_stop {
                            *guard = None;
                        }
                        return;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    #[async_trait]
    impl Provider for FedProvider {
        async fn call_stream(
            &self,
            req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.seen.lock().unwrap().push(req.messages.clone());
            let (tx, rx) = mpsc::channel(8);
            *self.tx.lock().await = Some(tx);
            Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
        }
    }

    /// Wait until `provider` has opened request `index`.
    async fn wait_for_request(provider: &FedProvider, index: usize) {
        for _ in 0..600 {
            if provider.request_count() > index {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("request {index} never opened");
    }

    /// A read-only tool that always succeeds, so the loop round-trips through
    /// ACT and comes back to the top of the loop (the loop-top drain), never
    /// reaching the `EndTurn` check.
    struct NoopTool;

    #[async_trait]
    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn description(&self) -> &str {
            "Does nothing"
        }
        async fn call(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("noop")
        }
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                read_only: true,
                ..Default::default()
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interjection_lands_in_context_before_the_next_request() {
        let provider = Arc::new(FedProvider::new());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(NoopTool));
        let mut agent = Agent::new(provider.clone(), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("original task".into()).await.unwrap();
        let collector = tokio::spawn(async move { collect_events_async(stream).await });

        // Request 1 asks for a tool. The loop then goes through ACT and returns
        // to the top, which is where the loop-top drain runs.
        wait_for_request(&provider, 0).await;
        agent.interject("also check the logs".into(), None).unwrap();
        provider
            .feed_to(
                0,
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "noop".into(),
                },
            )
            .await;
        provider
            .feed_to(
                0,
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: "{}".into(),
                },
            )
            .await;
        provider
            .feed_to(0, ProviderEvent::ToolUseEnd { id: "t1".into() })
            .await;
        provider
            .feed_to(
                0,
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            )
            .await;

        wait_for_request(&provider, 1).await;

        let events_so_far = provider.request_count();
        let second = provider.messages_of(1);
        let injected = second
            .iter()
            .find(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text(t) if t.contains("also check the logs")),
                )
            })
            .unwrap_or_else(|| panic!("request 1 of {events_so_far} must carry the interjection"));
        assert!(
            injected
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("without repeating"))),
            "the interjection must carry the prefix: {injected:?}"
        );

        provider
            .feed_to(
                1,
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            )
            .await;
        let events = collector.await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::InterjectionAccepted { .. })),
            "expected InterjectionAccepted: {events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interjection_before_end_turn_keeps_the_turn_alive() {
        let provider = Arc::new(FedProvider::new());
        let mut agent = Agent::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );

        let stream = agent.run("task".into()).await.unwrap();
        let collector = tokio::spawn(async move { collect_events_async(stream).await });

        // Deliver the interjection while the only request is still open, then
        // let that request finish with no tool call. Without the EndTurn drain
        // the run ends here and only one request is ever made.
        wait_for_request(&provider, 0).await;
        agent.interject("one more thing".into(), None).unwrap();
        provider
            .feed_to(
                0,
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            )
            .await;

        for _ in 0..600 {
            if provider.request_count() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            provider.request_count(),
            2,
            "the interjection must open a second request instead of ending the turn"
        );
        provider
            .feed_to(
                1,
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            )
            .await;

        let events = collector.await.unwrap();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::InterjectionAccepted { .. })),
            "expected InterjectionAccepted: {events:?}"
        );
        let second = provider.messages_of(1);
        assert!(
            second.iter().any(|m| m.content.iter().any(|b| matches!(
                b, ContentBlock::Text(t) if t.contains("one more thing")
            ))),
            "the follow-up request must carry the interjection: {second:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Done {
                    reason: DoneReason::EndTurn
                })
            ),
            "the turn must still finish: {events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_returns_unconsumed_interjections_before_cancelled() {
        let provider = Arc::new(StallOnceThenSucceedProvider::new());
        let mut agent = Agent::new(provider, Arc::new(ToolRegistry::new()), fast_stall_config());

        let stream = agent.run("task".into()).await.unwrap();
        // Clone the handle, then cancel: the stream (and so the run) is owned by
        // the consumer task, which is spawned here so this task stays free to
        // cancel and inspect.
        let handle = agent.inbox_handle().expect("handle after run starts");
        let collector = tokio::spawn(async move { collect_events_async(stream).await });

        handle
            .interject("never consumed".into(), Some("tag-9".into()))
            .unwrap();
        // Cancel at the ACT check: no call reaches the provider yet (the first
        // call's idle timeout is 50ms in `fast_stall_config`).
        agent.cancel();

        let events = collector.await.unwrap();

        let returned_at = events.iter().position(|e| {
            matches!(e, AgentEvent::InterjectionsReturned { items }
                if items.len() == 1
                   && items[0].text == "never consumed"
                   && items[0].tag.as_deref() == Some("tag-9"))
        });
        let cancelled_at = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Cancelled));

        assert!(
            returned_at.is_some(),
            "the unconsumed interjection must come back: {events:?}"
        );
        assert!(cancelled_at.is_some(), "expected Cancelled: {events:?}");
        assert!(
            returned_at.unwrap() < cancelled_at.unwrap(),
            "InterjectionsReturned must precede Cancelled: {events:?}"
        );
    }

    #[test]
    fn inbox_assigns_monotonic_seq_and_rejects_when_full() {
        let mut inbox = Inbox::new();
        assert_eq!(inbox.push("a".into(), None).unwrap(), 1);
        assert_eq!(inbox.push("b".into(), Some("tag-1".into())).unwrap(), 2);
        assert!(!inbox.is_empty());
        for i in 0..(Inbox::CAPACITY - 2) {
            inbox.push(format!("m{i}"), None).unwrap();
        }
        assert_eq!(
            inbox.push("overflow".into(), None),
            Err(InterjectError::Full)
        );
        assert_eq!(inbox.len(), Inbox::CAPACITY);
        let drained = inbox.drain_all();
        assert_eq!(drained.len(), Inbox::CAPACITY);
        assert_eq!(drained[0].text, "a");
        assert_eq!(drained[1].tag.as_deref(), Some("tag-1"));
        assert!(inbox.drain_all().is_empty());
    }

    #[tokio::test]
    async fn interject_before_first_run_reports_not_running() {
        let provider = ScriptedProvider::new(vec![]);
        let agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        assert!(matches!(
            agent.interject("late".into(), None),
            Err(InterjectError::NotRunning)
        ));
        assert!(agent.inbox_handle().is_none());
    }

    #[tokio::test]
    async fn interject_after_run_starts_is_accepted_in_order() {
        let provider = ScriptedProvider::new(vec![vec![ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        }]]);
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        let stream = agent.run("hello".into()).await.unwrap();
        assert_eq!(agent.interject("first".into(), None).unwrap(), 1);
        assert_eq!(
            agent.interject("second".into(), Some("t".into())).unwrap(),
            2
        );
        let handle = agent.inbox_handle().expect("handle after run starts");
        assert_eq!(handle.lock().len(), 2);
        drop(stream);
    }

    #[tokio::test]
    async fn session_tracks_and_clears_last_input_tokens() {
        let mut session = Session::new();
        assert_eq!(session.last_input_tokens(), None);
        session.set_last_input_tokens(Some(160_000));
        assert_eq!(session.last_input_tokens(), Some(160_000));
        session.set_last_input_tokens(None);
        assert_eq!(session.last_input_tokens(), None);
    }

    #[tokio::test]
    async fn session_basic_ops() {
        let mut s = Session::new();
        assert!(s.is_empty());
        s.push(Message::user("hi"));
        assert_eq!(s.len(), 1);
        s.truncate(0);
        assert!(s.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_terminates_on_end_turn_no_tools() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(matches!(events.first(), Some(AgentEvent::Start)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "Hello"))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_continues_after_max_tokens() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("partial".into()),
                ProviderEvent::Stop {
                    reason: StopReason::MaxTokens,
                },
            ],
            vec![
                ProviderEvent::TextDelta(" complete".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let events = collect_events(agent.run("write a file".into()).await.unwrap());

        assert!(
            events.iter().any(
                |event| matches!(event, AgentEvent::AssistantText(text) if text == " complete")
            )
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    /// A consumer that renders more slowly than the provider streams must still
    /// observe every text delta. Regression: `accumulate_provider_stream`
    /// forwarded deltas with a lossy `try_send` into a 64-slot channel, so a
    /// lagging UI silently lost the tail of long messages.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lagging_consumer_still_receives_every_text_delta() {
        use futures::StreamExt;

        const TOTAL: usize = 2000;
        let mut script: Vec<ProviderEvent> = (0..TOTAL)
            .map(|i| ProviderEvent::TextDelta(format!("{i},")))
            .collect();
        script.push(ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        });
        let provider = ScriptedProvider::new(vec![script]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let mut stream = agent.run("hi".into()).await.unwrap();
        let mut received = String::new();
        let mut count = 0usize;
        while let Some(ev) = stream.next().await {
            if let AgentEvent::AssistantText(t) = ev {
                received.push_str(&t);
                count += 1;
                // Model a UI that renders more slowly than the provider emits.
                if count % 16 == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
        }

        assert_eq!(
            count, TOTAL,
            "every streamed delta must reach the consumer, even when it lags"
        );
        let expected: String = (0..TOTAL).map(|i| format!("{i},")).collect();
        assert_eq!(received, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_does_not_report_abnormal_stop_as_end_turn() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::Stop {
                reason: StopReason::Other("idle timeout".into()),
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let events = collect_events(agent.run("write a file".into()).await.unwrap());

        assert!(!matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_executes_tool_and_loops() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("Let me uppercase".into()),
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"#.to_string(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#""hi"}"#.to_string(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("Result: HI".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("uppercase hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "upper"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_handles_tool_not_found() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "ghost".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: "{}".into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("call ghost".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if result.is_error))
        );
        let session = agent.session();
        let tool_result_message = session
            .messages()
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(matches!(
            tool_result_message.content.as_slice(),
            [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "t1"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_reports_provider_stream_stop_as_error() {
        // A malformed SSE payload is a non-retryable transport failure: it must
        // surface as a terminal Error, never as a retry.
        let provider = ScriptedProvider::new(vec![vec![ProviderEvent::StreamError(
            ProviderError::Stream("stream error: invalid SSE payload".into()),
        )]]);
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );

        let events = collect_events(agent.run("hello".into()).await.unwrap());
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::Provider(ProviderError::Stream(message)))
                if message.contains("invalid SSE payload")
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Done { .. }))
        );
    }

    /// Fails the first call with a request timeout, succeeds afterwards.
    struct TimeoutOnceThenSucceedProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl TimeoutOnceThenSucceedProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for TimeoutOnceThenSucceedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let stream = if *calls == 1 {
                futures::stream::iter(vec![
                    ProviderEvent::TextDelta("stale".into()),
                    ProviderEvent::StreamError(ProviderError::Network(
                        "stream error: timed out".into(),
                    )),
                ])
                .boxed()
            } else {
                futures::stream::iter(vec![
                    ProviderEvent::TextDelta("fresh".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ])
                .boxed()
            };
            Ok(stream)
        }
    }

    /// Call 1 stalls, call 2 times out, call 3 succeeds. Proves the retry budget
    /// is shared between the two failure modes rather than granted separately.
    struct StallThenTimeoutThenSucceedProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl StallThenTimeoutThenSucceedProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for StallThenTimeoutThenSucceedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let stream = match *calls {
                1 => futures::stream::iter(vec![ProviderEvent::TextDelta("stale".into())])
                    .chain(futures::stream::pending())
                    .boxed(),
                2 => futures::stream::iter(vec![ProviderEvent::StreamError(
                    ProviderError::Network("stream error: timed out".into()),
                )])
                .boxed(),
                _ => futures::stream::iter(vec![
                    ProviderEvent::TextDelta("fresh".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ])
                .boxed(),
            };
            Ok(stream)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_shares_one_retry_budget_across_stall_and_timeout() {
        let provider = Arc::new(StallThenTimeoutThenSucceedProvider::new());
        let calls = provider.clone();
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // One stall + one timeout = 2 of the shared 3-retry budget, then success.
        assert_eq!(*calls.calls.lock().unwrap(), 3);
        let causes: Vec<RetryCause> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ProviderRetry { cause, .. } => Some(*cause),
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![RetryCause::IdleStall, RetryCause::RequestTimeout],
            "the budget is shared, and each retry names its own cause: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    /// Every call fails with a request timeout, so retries are always exhausted.
    struct AlwaysTimeoutProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl AlwaysTimeoutProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for AlwaysTimeoutProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            let stream = futures::stream::iter(vec![ProviderEvent::StreamError(
                ProviderError::Network("stream error: timed out".into()),
            )])
            .boxed();
            Ok(stream)
        }
    }

    /// A request deadline is transient: the turn must survive it.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_retries_a_request_timeout_and_completes() {
        let provider = Arc::new(TimeoutOnceThenSucceedProvider::new());
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // The retry is announced, with a cause that names the timeout.
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ProviderRetry {
                    attempt: 1,
                    max: 3,
                    cause: RetryCause::RequestTimeout,
                    ..
                }
            )),
            "expected a RequestTimeout ProviderRetry, got: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::Done {
                    reason: DoneReason::Interrupted { .. }
                } | AgentEvent::Error(_)
            )),
            "a transient timeout must not surface as Interrupted/Error: {events:?}"
        );
        // The timed-out partial ("stale") is NOT committed: only "fresh" is kept.
        let session = agent.session();
        let assistants: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["fresh"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_exhausts_timeout_retries_then_reports_interruption() {
        let provider = Arc::new(AlwaysTimeoutProvider::new());
        let calls = provider.clone();
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // 1 initial attempt + 3 retries.
        assert_eq!(*calls.calls.lock().unwrap(), 4);
        let retries = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
            .count();
        assert_eq!(retries, 3, "expected 3 retries, got: {events:?}");
        match events.last() {
            Some(AgentEvent::Done {
                reason: DoneReason::Interrupted { reason },
            }) => assert_eq!(reason, "request timeout after 3 retries"),
            other => panic!("expected Interrupted after retries, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_does_not_retry_a_connection_reset() {
        // A transport break that is not a deadline stays `Stream` and must be
        // terminal: only a timeout earns a retry (see the design's table).
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::StreamError(ProviderError::Stream(
                "stream error: connection reset by peer".into(),
            )),
        ]]));
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Error(AgentError::Provider(ProviderError::Stream(_)))
            )),
            "a non-timeout transport break must be a terminal Error, got: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ProviderRetry { .. })),
            "a non-timeout transport break must never be retried: {events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_does_not_retry_a_malformed_sse_payload() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::StreamError(ProviderError::Stream("invalid SSE JSON".into())),
        ]]));
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Error(AgentError::Provider(ProviderError::Stream(m)))
                    if m.contains("invalid SSE JSON")
            )),
            "a malformed payload must be a terminal Error, got: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ProviderRetry { .. })),
            "a malformed payload must never be retried"
        );
    }

    #[test]
    fn default_agent_budget_allows_a_real_task_to_finish() {
        // A delegated coding task routinely runs past a hundred turns. The old
        // default of 20 cut children off mid-sentence, so this floor is a
        // regression guard, not a tuning knob: raising it is fine, lowering it
        // silently truncates reports again.
        let agent_config = AgentConfig::default();
        assert!(
            agent_config.max_turns.unwrap_or(u32::MAX) >= 200,
            "the default turn ceiling must leave room for a real task, got {:?}",
            agent_config.max_turns
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_respects_max_turns() {
        // Provider always emits a tool call -> would infinite loop without cap.
        // With max_turns=1: turn 1 executes tool, turn 2 > max -> MaxTurns.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "upper".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: r#"{"text":"x"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            max_turns: Some(1),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("loop".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                reason: DoneReason::MaxTurns
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_session_restores_history() {
        let mut session = Session::new();
        session.push(Message::user("previous"));
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent =
            Agent::new(Arc::new(provider), tools, AgentConfig::default()).with_session(session);

        assert_eq!(agent.session().len(), 1); // restored
        let stream = agent.run("next".into()).await.unwrap();
        // Consume all events to ensure the spawned task completes.
        let events = collect_events(stream);
        // restored(1) + user_prompt(1) + assistant(1) = 3
        assert_eq!(agent.session().len(), 3);
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_forwards_usage_events() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(crate::provider::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        let usage_events: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, 10);
        assert_eq!(usage_events[0].output_tokens, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_token_is_cancellable() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let token = agent.cancel_token();
        assert!(!token.is_cancelled());
        agent.cancel();
        assert!(token.is_cancelled());
    }

    /// Provider whose stream never produces events (simulates a long LLM call).
    struct HangingProvider;

    #[async_trait]
    impl Provider for HangingProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            // A stream that never yields — pending forever.
            let pending = futures::stream::pending();
            Ok(pending.boxed())
        }
    }

    /// Provider that emits one TextDelta then stalls forever (no Stop, no None).
    /// Simulates a real-world stall where the server sends partial text then
    /// the connection goes silent without a proper terminal event.
    struct StallAfterDeltaProvider;

    #[async_trait]
    impl Provider for StallAfterDeltaProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            // Emit one TextDelta, then pending forever.
            let stream = futures::stream::iter(vec![ProviderEvent::TextDelta("partial".into())])
                .chain(futures::stream::pending());
            Ok(stream.boxed())
        }
    }

    /// Emits one delta then stalls on the first call; succeeds on later calls.
    struct StallOnceThenSucceedProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl StallOnceThenSucceedProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for StallOnceThenSucceedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let stream = if *calls == 1 {
                futures::stream::iter(vec![ProviderEvent::TextDelta("stale".into())])
                    .chain(futures::stream::pending())
                    .boxed()
            } else {
                futures::stream::iter(vec![
                    ProviderEvent::TextDelta("fresh".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ])
                .boxed()
            };
            Ok(stream)
        }
    }

    /// Stalls after emitting a complete tool_use, so the partial contains an
    /// unpaired `tool_use` that must never reach the session.
    struct StallingToolUseProvider;

    #[async_trait]
    impl Provider for StallingToolUseProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let stream = futures::stream::iter(vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"hi"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
            ])
            .chain(futures::stream::pending())
            .boxed();
            Ok(stream)
        }
    }

    /// Every call stalls, so retries are always exhausted.
    struct AlwaysStallProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl AlwaysStallProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for AlwaysStallProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            let stream = futures::stream::iter(vec![ProviderEvent::TextDelta("partial".into())])
                .chain(futures::stream::pending())
                .boxed();
            Ok(stream)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_think_emits_cancelled() {
        let provider = Arc::new(HangingProvider);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        // run() resets the cancel token (Fix 1), so we must capture the
        // token AFTER run() returns to cancel the correct (new) token.
        let stream = agent.run("hi".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
        let events = collect_events(stream);
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "should have Cancelled event"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "should NOT have Done event"
        );
        // Cancel during THINK keeps this run's user prompt (B policy: preserve
        // completed work); nothing has been produced yet, so the session ends on
        // the user message.
        assert_eq!(
            agent.session().len(),
            1,
            "THINK cancel must keep the run's user prompt: {:?}",
            agent.session().messages()
        );
        assert_eq!(agent.session().messages()[0].role, Role::User);
    }

    /// When the provider stream emits a TextDelta then stalls (no Stop event),
    /// the agent must not hang forever. It should detect the idle stall and
    /// emit a terminal event (Done or Error) within a bounded time.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_think_stream_stall_emits_terminal_within_timeout() {
        let provider = Arc::new(StallAfterDeltaProvider);
        let tools = Arc::new(ToolRegistry::new());
        // Short idle timeout so the test runs fast (the stall is detected
        // quickly instead of waiting for the 60s default). Stall retries are
        // disabled here: this test asserts the *terminal* outcome, and the
        // retry path is covered by the dedicated stall-retry tests below.
        let config = AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(500)),
            think_stall_retry_limit: 0,
            ..Default::default()
        };
        let mut agent = Agent::new(provider, tools, config);

        let stream = agent.run("hi".into()).await.unwrap();

        // Wrap in a timeout: if the agent hangs forever (the bug), this fails.
        // The idle timeout under test should be much shorter than this.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            collect_events_async(stream),
        )
        .await;

        let events = result
            .expect("agent hung forever — idle stall not detected (no terminal event within 10s)");

        // Must have received the partial text.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "partial")),
            "should have the partial AssistantText"
        );
        // Must have a terminal event (Done or Error), not just hang.
        let has_terminal = events
            .iter()
            .any(|e| matches!(e, AgentEvent::Done { .. } | AgentEvent::Error(_)));
        assert!(
            has_terminal,
            "should emit a terminal event (Done or Error) after stall, got: {:?}",
            events
        );
    }

    fn fast_stall_config() -> AgentConfig {
        AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(50)),
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_millis(10),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_retries_a_transient_idle_stall_and_completes() {
        let provider = Arc::new(StallOnceThenSucceedProvider::new());
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // The retry is announced so the user is not left in silent limbo...
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ProviderRetry {
                    attempt: 1,
                    max: 3,
                    ..
                }
            )),
            "expected a ProviderRetry event, got: {events:?}"
        );
        // ...and the turn completes normally instead of reporting an interruption.
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::Done {
                    reason: DoneReason::Interrupted { .. }
                }
            )),
            "a transient stall must not surface as Interrupted"
        );
        // The stalled partial ("stale") is NOT committed: only the retried text is kept.
        let session = agent.session();
        let assistants: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["fresh"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_exhausts_stall_retries_then_reports_interruption() {
        let provider = Arc::new(AlwaysStallProvider::new());
        let calls = provider.clone();
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // 1 initial attempt + 3 retries.
        assert_eq!(*calls.calls.lock().unwrap(), 4);
        let retries = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
            .count();
        assert_eq!(retries, 3, "expected 3 retries, got: {events:?}");
        match events.last() {
            Some(AgentEvent::Done {
                reason: DoneReason::Interrupted { reason },
            }) => assert_eq!(reason, "idle timeout after 3 retries"),
            other => panic!("expected Interrupted after retries, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_exhausted_stall_does_not_commit_partial_to_session() {
        let provider = Arc::new(StallingToolUseProvider);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::Interrupted { .. }
            })
        ));
        // The stalled partial holds an unpaired `tool_use`; committing it would
        // make the next provider request invalid.
        let session = agent.session();
        assert!(
            !session.messages().iter().any(|m| m.role == Role::Assistant),
            "stalled partial must not be committed, got: {:?}",
            session.messages()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancels_during_stall_backoff() {
        let provider = Arc::new(AlwaysStallProvider::new());
        let tools = Arc::new(ToolRegistry::new());
        // Long backoff so the cancel lands while sleeping, not mid-stream.
        let config = AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(50)),
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_secs(30),
            ..Default::default()
        };
        let mut agent = Agent::new(provider, tools, config);

        let stream = agent.run("hi".into()).await.unwrap();
        // run() resets the cancel token, so capture it AFTER run() returns.
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancel_token.cancel();
        });

        let events = collect_events_async(stream).await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "expected Cancelled, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "a cancelled run must not also report Done"
        );
    }

    #[test]
    fn agent_config_defaults_stall_retry_policy() {
        let config = AgentConfig::default();
        assert_eq!(config.think_stall_retry_limit, 3);
        assert_eq!(
            config.think_stall_backoff_base,
            std::time::Duration::from_secs(2)
        );
    }

    #[test]
    fn stall_backoff_delay_doubles_and_caps() {
        let base = std::time::Duration::from_secs(2);
        assert_eq!(
            stall_backoff_delay(base, 1),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(
            stall_backoff_delay(base, 2),
            std::time::Duration::from_secs(4)
        );
        assert_eq!(
            stall_backoff_delay(base, 3),
            std::time::Duration::from_secs(8)
        );
        // Capped at 30s no matter how many retries.
        assert_eq!(
            stall_backoff_delay(base, 10),
            std::time::Duration::from_secs(30)
        );
    }

    /// Async version of collect_events for use with tokio::time::timeout.
    async fn collect_events_async(mut stream: BoxStream<'static, AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        use futures::StreamExt;
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    /// Tool that never completes (simulates a long-running tool).
    struct HangingTool;

    #[async_trait]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hang"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn description(&self) -> &str {
            "A tool that hangs forever"
        }
        async fn call(&self, _args: serde_json::Value) -> ToolResult {
            // Never returns
            std::future::pending::<()>().await;
            ToolResult::text("unreachable")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_act_emits_cancelled() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "hang".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HangingTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        // run() resets the cancel token (Fix 1), so capture AFTER run().
        let stream = agent.run("hang".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "should have Cancelled event"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { .. })),
            "should NOT have ToolResult (tool was still running)"
        );

        // B policy: keep this run's user prompt, drop only the trailing
        // assistant(tool_use) whose results were never observed.
        let session = agent.session();
        assert_eq!(
            session.len(),
            1,
            "session should keep the user prompt and drop the dangling tool_use"
        );
        assert_eq!(session.messages()[0].role, Role::User);
        assert!(
            !session.messages().iter().any(|m| m.role == Role::Assistant),
            "no dangling assistant(tool_use) may remain"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_act_rolls_back_session() {
        // Fix 2 专项测试:ACT 阶段 cancel 后,session 中不能留下悬空的
        // assistant(tool_use)(否则下次 run 会被 Anthropic API 拒绝,
        // 因为 tool_use 必须跟 tool_result)。
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "hang".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HangingTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let session_before = agent.session().len();
        assert_eq!(session_before, 0, "fresh agent has empty session");

        let stream = agent.run("hang".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
        let _ = collect_events(stream);

        let session = agent.session();
        // B policy: keep the user prompt; only the dangling assistant(tool_use)
        // is dropped, so the session ends on the user message.
        assert_eq!(
            session.len(),
            1,
            "session should keep the user prompt after ACT cancel"
        );
        assert_eq!(session.messages()[0].role, Role::User);
        assert!(
            !session.messages().iter().any(|m| m.role == Role::Assistant),
            "no Assistant message should remain after ACT cancel"
        );
    }

    /// B policy: a multi-step turn that already completed a round-trip must keep
    /// that work when a *later* step is cancelled — only the trailing, unpaired
    /// assistant(tool_use) is dropped. This is the case that distinguishes the
    /// "preserve completed work" policy from dropping the whole turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_keeps_completed_round_trips_and_drops_only_the_dangling_tool_use() {
        // Step 1: a text-free, read-only tool round-trip that completes. Because
        // it emits no text, the loop continues (rather than ending) and asks the
        // provider again.
        // Step 2: another tool_use whose tool hangs until cancelled.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "done".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "done".into(),
                    partial_json: "{}".into(),
                },
                ProviderEvent::ToolUseEnd { id: "done".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::ToolUseStart {
                    id: "hang".into(),
                    name: "hang".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "hang".into(),
                    partial_json: "{}".into(),
                },
                ProviderEvent::ToolUseEnd { id: "hang".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool)); // name "upper"
        tools.register(Arc::new(HangingTool)); // name "hang"
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("go".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            // Long enough for step 1's round-trip to commit and step 2's tool to
            // start hanging, then cancel.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cancel_token.cancel();
        });
        let events = collect_events(stream);
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "expected Cancelled, got: {events:?}"
        );

        let session = agent.session();
        let roles: Vec<Role> = session.messages().iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool],
            "user + completed round-trip (assistant + tool_results) must survive"
        );
        // The surviving assistant must be step 1's (paired) tool_use, not the
        // dangling step-2 tool_use.
        let ast = &session.messages()[1];
        let ids: Vec<&str> = ast
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["done"], "only the completed tool_use survives");
        // The trailing message is the paired tool_results, and no unpaired
        // assistant(tool_use) remains.
        let tool_results = &session.messages()[2];
        let paired: Vec<&str> = tool_results
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            paired,
            vec!["done"],
            "the completed tool_result is retained"
        );
        assert!(
            !session.messages().iter().any(|m| m.role == Role::Assistant
                && m.content.iter().any(|b| matches!(
                    b,
                    ContentBlock::ToolUse { id, .. } if id == "hang"
                ))),
            "the dangling step-2 tool_use must not remain"
        );
    }

    #[test]
    fn agent_config_has_compact_fields() {
        let config = AgentConfig::default();
        assert!(config.compact_threshold.is_some());
        assert_eq!(config.compact_user_budget_tokens, 20_000);
        assert_eq!(config.compact_tool_budget_tokens, 12_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_drop_receiver_does_not_panic() {
        let provider = HangingProvider;
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        // Drop the stream immediately without consuming.
        drop(stream);
        // Give the spawned task time to notice the dropped receiver.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No panic means success.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_executes_parallel_tools_in_single_turn() {
        // Provider emits two tool calls in one turn; both should execute.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t2".into(),
                    partial_json: r#"{"text":"b"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("parallel".into()).await.unwrap();
        let events = collect_events(stream);

        let tool_calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 2);

        let tool_results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { id, result } => Some((id.clone(), result.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 2);
        // Both results should be successful
        assert!(tool_results.iter().all(|(_, r)| !r.is_error));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_multi_turn_loop_three_turns() {
        // Turn 1: tool call -> Turn 2: tool call -> Turn 3: final text
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"first"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t2".into(),
                    partial_json: r#"{"text":"second"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("final answer".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("multi".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have 2 ToolCall events
        let tool_calls = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
            .count();
        assert_eq!(tool_calls, 2);

        // Should end with Done(EndTurn)
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_propagates_provider_error() {
        struct ErrorProvider;
        #[async_trait]
        impl Provider for ErrorProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
                Err(ProviderError::Auth("invalid key".into()))
            }
        }

        let provider = ErrorProvider;
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Error(AgentError::Provider(ProviderError::Auth(_)))
            )),
            "should have Provider Auth error event"
        );
        // Should NOT have a Done event
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "should NOT have Done event after error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_session_history_after_multi_turn() {
        // After 2 tool turns + final text:
        // user(1) + assistant_turn1(1) + tool_results(1) + assistant_turn2(1) + tool_results(1) + assistant_final(1) = 6
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("final".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("start".into()).await.unwrap();
        let _ = collect_events(stream);

        let session = agent.session();
        // user(1) + assistant(1) + tool_results(1) + assistant(1) = 4
        assert_eq!(session.len(), 4);
        assert_eq!(session.messages()[0].role, Role::User);
        assert_eq!(session.messages()[1].role, Role::Assistant);
        assert_eq!(session.messages()[2].role, Role::Tool);
        assert_eq!(session.messages()[3].role, Role::Assistant);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_sequential_runs_accumulate_session() {
        // First run: text only -> user + assistant = 2 messages
        // Second run: text only -> + user + assistant = 4 messages
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("first".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("second".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        // First run
        let stream1 = agent.run("prompt1".into()).await.unwrap();
        let _ = collect_events(stream1);
        assert_eq!(agent.session().len(), 2);

        // Second run — session should accumulate
        let stream2 = agent.run("prompt2".into()).await.unwrap();
        let _ = collect_events(stream2);
        assert_eq!(agent.session().len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_assistant_text_event_preserves_content() {
        // Verify AssistantText events carry the full text from provider.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hello ".into()),
            ProviderEvent::TextDelta("World".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello World");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_max_turns_zero_immediate_done() {
        // With max_turns=0: turn 1 > 0 immediately -> MaxTurns before any provider call.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("unreachable".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let config = AgentConfig {
            max_turns: Some(0),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), tools, config);

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                reason: DoneReason::MaxTurns
            }
        )));
        // Should not have any assistant text since provider was never called
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(_))),
            "should NOT have AssistantText with max_turns=0"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_then_run_works() {
        // Fix 1: cancel() was permanent. After Fix 1, run() resets the
        // cancel token, so a previously cancelled agent can still run to
        // completion. This regression test guards against reintroducing
        // the "agent permanently stuck after cancel" bug.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        // Cancel before run starts. With Fix 1, run() resets the token,
        // so this cancel must NOT cause the upcoming run to emit Cancelled.
        agent.cancel();
        assert!(
            agent.cancel_token().is_cancelled(),
            "precondition: token cancelled"
        );

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        // Should complete normally, ending with Done(EndTurn), NOT Cancelled.
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "run after cancel should not be cancelled (Fix 1 reset token)"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[test]
    fn agent_config_default_model() {
        let config = AgentConfig::default();
        assert_eq!(config.model, "claude-sonnet-4-5");
        assert_eq!(config.max_turns, Some(200));
    }

    #[test]
    fn default_system_prompt_contains_identity_and_strategy() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(prompt.contains("yi-agent"));
        assert!(prompt.contains("Gong Yichen"));
        assert!(prompt.contains("minimizing round-trips"));
        assert!(prompt.contains("MULTIPLE tool calls"));
        assert!(prompt.contains("&&"));
    }

    #[test]
    fn default_system_prompt_discourages_unbounded_glob() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(prompt.contains("glob({\"path\":\".\",\"pattern\":\"**/*\"})"));
        assert!(prompt.contains("rg --files"));
        assert!(prompt.contains("target/"));
        assert!(prompt.contains(".worktrees/"));
    }

    #[test]
    fn default_system_prompt_requires_progress_narration() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("Progress narration:"),
            "prompt must name the narration section so a reviewer can find it"
        );
        assert!(
            prompt.contains("1-2 sentences"),
            "prompt must ask for a short prose lead-in on tool-calling responses"
        );
        assert!(
            prompt.contains("At least every ~10 tool calls"),
            "prompt must bound how long the agent may stay silent"
        );
        assert!(
            prompt.contains("Narration rides along with the tool call in the same response"),
            "narration must not be readable as a reason to split tool calls"
        );
        assert!(
            prompt.contains("in the language the user writes in"),
            "narration must follow the user's language"
        );
    }

    #[test]
    fn agent_config_default_uses_default_system_prompt() {
        let config = AgentConfig::default();
        assert_eq!(
            config.system_prompt.as_deref(),
            Some(AgentConfig::default_system_prompt().as_str())
        );
    }

    #[test]
    fn agent_config_custom_system_prompt_overrides_default() {
        let config = AgentConfig {
            system_prompt: Some("be brief".into()),
            ..Default::default()
        };
        assert_eq!(config.system_prompt.as_deref(), Some("be brief"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_system_prompt_is_used_by_next_run() {
        use crate::provider::{ProviderError, ProviderEvent};

        struct InspectingProvider(std::sync::Mutex<Option<ProviderRequest>>);

        #[async_trait]
        impl Provider for InspectingProvider {
            async fn call_stream(
                &self,
                request: ProviderRequest,
            ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError>
            {
                *self.0.lock().unwrap() = Some(request);
                Ok(futures::stream::iter(vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }])
                .boxed())
            }
        }

        let provider = Arc::new(InspectingProvider(std::sync::Mutex::new(None)));
        let config = AgentConfig {
            system_prompt: Some("original".into()),
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider.clone() as Arc<dyn Provider>,
            Arc::new(ToolRegistry::new()),
            config,
        );

        agent.set_system_prompt(Some("refreshed".into()));
        let stream = agent.run("hi".into()).await.unwrap();
        let _ = collect_events(stream);

        let guard = provider.0.lock().unwrap();
        let seen = guard.as_ref().expect("provider was called");
        assert_eq!(seen.system.as_deref(), Some("refreshed"));
    }

    #[test]
    fn agent_config_custom_values() {
        let config = AgentConfig {
            model: "custom-model".into(),
            system_prompt: Some("be brief".into()),
            max_turns: Some(50),
            gen_params: GenParams {
                temperature: Some(0.7),
                max_tokens: Some(4096),
                ..Default::default()
            },
            compact_threshold: Some(50_000),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            think_idle_timeout: None,
            think_stall_retry_limit: 0,
            think_stall_backoff_base: std::time::Duration::from_secs(1),
        };
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.max_turns, Some(50));
        assert_eq!(config.gen_params.temperature, Some(0.7));
    }

    #[test]
    fn permission_request_event_constructs() {
        let ev = AgentEvent::PermissionRequest {
            request_id: 1,
            tool_name: "bash".to_string(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: Some("ls".to_string()),
            kind: PermissionKind::Normal,
        };
        match ev {
            AgentEvent::PermissionRequest {
                request_id,
                tool_name,
                ..
            } => {
                assert_eq!(request_id, 1);
                assert_eq!(tool_name, "bash");
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_resolved_event_constructs() {
        let ev = AgentEvent::PermissionResolved {
            request_id: 1,
            decision: Decision::AllowOnce,
        };
        match ev {
            AgentEvent::PermissionResolved {
                request_id,
                decision,
            } => {
                assert_eq!(request_id, 1);
                assert_eq!(decision, Decision::AllowOnce);
            }
            _ => panic!("expected PermissionResolved"),
        }
    }

    #[test]
    fn agent_with_permission_builder_sets_fields() {
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            crate::autonomy::YoloSwitch::new(false),
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (_tx, rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let provider = ScriptedProvider::new(vec![]);
        let tools = Arc::new(ToolRegistry::new());
        let agent = Agent::new(Arc::new(provider), tools, AgentConfig::default())
            .with_permission(checker, rx);
        assert!(agent.permission_checker.is_some());
        assert!(agent.decision_rx.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_allow_executes_tool() {
        // PermissionChecker with yolo=true allows all bash commands (no blacklist match).
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            crate::autonomy::YoloSwitch::new(true), // yolo: allow all (except blacklist, which is empty)
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (_decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"hi"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // "upper" tool is not bash/write/edit, so check() returns Allow immediately.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error)),
            "tool should execute and return success"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_need_confirm_user_denies() {
        // No whitelist, no yolo -> bash tool call triggers NeedConfirm.
        // User sends Deny -> tool result is an error.
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            crate::autonomy::YoloSwitch::new(false), // not yolo
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"command":"ls"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        // Spawn a task to respond with Deny when a PermissionRequest arrives.
        let _handle = tokio::spawn(async move {
            // Wait for the request_id from the channel, then send Deny.
            // The receiver is in the agent, so we just send a decision for any request_id.
            // We need to know the request_id. It starts at 1.
            decision_tx
                .send((1, crate::permission::Decision::Deny))
                .await
                .ok();
        });

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have a PermissionRequest event
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. })),
            "should emit PermissionRequest event"
        );
        // Should have a PermissionResolved event with Deny
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::PermissionResolved {
                    decision: crate::permission::Decision::Deny,
                    ..
                }
            )),
            "should emit PermissionResolved with Deny"
        );
        // Should have a ToolResult with error (denied)
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if result.is_error)),
            "should have an error ToolResult for denied tool"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_need_confirm_user_allows() {
        // No whitelist, no yolo -> bash tool call triggers NeedConfirm.
        // User sends AllowOnce -> tool executes normally.
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            crate::autonomy::YoloSwitch::new(false),
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"command":"ls"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        // Register a tool named "bash" that just echoes.
        struct BashEchoTool;
        #[async_trait]
        impl Tool for BashEchoTool {
            fn name(&self) -> &str {
                "bash"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
            }
            fn description(&self) -> &str {
                "Echoes the command back"
            }
            async fn call(&self, args: serde_json::Value) -> ToolResult {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                ToolResult::text(format!("ran: {}", cmd))
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BashEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        let _handle = tokio::spawn(async move {
            decision_tx
                .send((1, crate::permission::Decision::AllowOnce))
                .await
                .ok();
        });

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have PermissionRequest and PermissionResolved events
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. })),
            "should emit PermissionRequest event"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::PermissionResolved {
                    decision: crate::permission::Decision::AllowOnce,
                    ..
                }
            )),
            "should emit PermissionResolved with AllowOnce"
        );
        // Should have a successful ToolResult
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error)),
            "should have a successful ToolResult for allowed tool"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    /// 黑名单命令在 yolo 下必须硬拒绝:不弹确认框,且拒绝要可见。
    #[tokio::test(flavor = "multi_thread")]
    async fn blacklisted_command_hard_denies_without_confirmation() {
        // 黑名单函数:任何含 "blocked-cmd" 的命令都视为黑名单。
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|cmd: &str| {
            cmd.contains("blocked-cmd").then(|| "test rule".to_string())
        });
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            crate::autonomy::YoloSwitch::new(true), // yolo
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        // 通道的 sender 必须 drop:通道关闭后,当前(未修复)代码会走
        // `recv()` 返回 None 的分支快速失败。若 sender 存活且无人应答,
        // 该路径会永久阻塞、测试挂起而不是失败 —— 已实测确认。
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        drop(decision_tx);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"command":"blocked-cmd"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // 1. 不得出现确认框。
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. })),
            "blacklisted command must not prompt for confirmation"
        );
        // 2. 拒绝必须可见:先有 ToolCall。
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "bash")),
            "deny must emit ToolCall so the TUI can render it"
        );
        // 3. 拒绝原因出现在错误 ToolResult 中。
        //    注意:ToolResult::error 会把文本包成 "error: {text}"
        //    (见 yi-agent-core/src/tool.rs),所以匹配子串而非整串。
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult { result, .. }
                    if result.is_error
                        && result.content.iter().any(|b| matches!(
                            b,
                            crate::message::ContentBlock::Text(t)
                                if t.contains("blocked by safety filter: test rule")
                        ))
            )),
            "deny must carry the blocklist reason in an error ToolResult"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_runs_before_second_user_turn_from_session_usage() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("first answer".into()),
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("checkpoint".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("second answer".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let config = AgentConfig {
            compact_threshold: Some(100),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(ToolRegistry::new()), config);
        let first = agent.run("first request".into()).await.unwrap();
        let _ = collect_events(first);
        assert_eq!(agent.session().last_input_tokens(), Some(200));
        let second = agent.run("second request".into()).await.unwrap();
        let events = collect_events(second);
        assert!(events.iter().any(|event| matches!(event, AgentEvent::AutoCompacting { old_msg_count, new_msg_count } if old_msg_count > new_msg_count)));
        assert_eq!(agent.session().last_input_tokens(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_triggers_when_threshold_exceeded() {
        // Pre-populate session with 4 messages (2 user/assistant pairs) so that
        // after turn 1 the session has 7 messages — enough for compact_session
        // to find a non-zero split point with keep_turns=1.
        // Turn 1: tool_use + Usage(input=200). After turn 1:
        //   [user1, asst1, user2, asst2, user_prompt, asst_tool_use, tool_results] = 7
        // Turn 2 THINK前: last_input_tokens=200 >= threshold=100, 7 > 4 → compact.
        // compact_session 调 provider.call() → Script[1]: "summary text".
        // session 替换为 [summary, recent...]. emit AutoCompacting.
        // Turn 2 THINK → Script[2]: "done" + EndTurn.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary text".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::AutoCompacting {
                    old_msg_count,
                    new_msg_count
                } if *old_msg_count > *new_msg_count
            )),
            "should emit AutoCompacting with old > new, events: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_below_threshold() {
        // Usage(input=50) < threshold=100 → no compact.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 50,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when below threshold"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_when_threshold_none() {
        // threshold=None → no compact even if Usage claims 200.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: None,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when threshold is None"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_when_threshold_zero() {
        // threshold=Some(0) → filtered out by `filter(|&t| t > 0)`, no compact.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(0),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when threshold is zero"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_on_first_turn() {
        // First turn: last_input_tokens is None → no compact even if
        // Usage claims 999. No with_session needed — the pre-check
        // happens before any Usage is captured.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 999,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting on first turn"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    /// Provider that returns scripted events for the first N calls,
    /// then returns an error on the (N)-th call, then resumes scripted events.
    struct ScriptThenFail {
        scripts: Vec<Vec<ProviderEvent>>,
        fail_at: usize,
        call_index: std::sync::Mutex<usize>,
        fail_error: ProviderError,
    }

    impl ScriptThenFail {
        fn new(scripts: Vec<Vec<ProviderEvent>>, fail_at: usize, error: ProviderError) -> Self {
            Self {
                scripts,
                fail_at,
                call_index: std::sync::Mutex::new(0),
                fail_error: error,
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptThenFail {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let current = *idx;
            *idx += 1;
            if current == self.fail_at {
                return Err(self.fail_error.clone());
            }
            let script = self.scripts.get(current).cloned().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }]
            });
            Ok(futures::stream::iter(script).boxed())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_failure_continues_loop() {
        // compact fails (provider error on compact call), run_loop continues.
        // Call 0 (turn 1 THINK): tool_use + Usage(input=200) + Stop
        // Call 1 (compact_session): Err(Auth)
        // Call 2 (turn 2 THINK): "done" + EndTurn
        let provider = ScriptThenFail::new(
            vec![
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "upper".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "t1".into(),
                        partial_json: r#"{"text":"a"}"#.into(),
                    },
                    ProviderEvent::ToolUseEnd { id: "t1".into() },
                    ProviderEvent::Usage(TokenUsage {
                        input_tokens: 200,
                        ..Default::default()
                    }),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
                // Call 1 is consumed by compact_session → will fail
                vec![],
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
            ],
            1,
            ProviderError::Auth("compact auth failed".into()),
        );
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when compact fails"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_success_clears_baseline_before_next_think() {
        // Verify compact → next THINK → compact again works.
        // Scripts:
        // 0: turn 1 — tool_use + Usage(200) + Stop
        // 1: compact call — "summary1" + Stop
        // 2: turn 2 THINK — tool_use + Usage(200) + Stop
        // 3: compact call — "summary2" + Stop
        // 4: turn 3 THINK — "done" + EndTurn
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary1".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t2".into(),
                    partial_json: r#"{"text":"b"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary2".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        let compact_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AutoCompacting { .. }))
            .count();
        assert_eq!(
            compact_count, 1,
            "successful compact clears usage until a later provider report; got {compact_count}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[test]
    fn estimate_tokens_ascii() {
        // 8 ASCII chars → 8/4 = 2 tokens
        assert_eq!(estimate_tokens("hello!!!"), 2);
    }

    #[test]
    fn estimate_tokens_cjk() {
        // 3 CJK chars → 3/1.5 = 2 tokens
        assert_eq!(estimate_tokens("你好吗"), 2);
    }

    #[test]
    fn estimate_tokens_mixed() {
        // 4 ASCII + 3 CJK → 1 + 2 = 3 tokens
        assert_eq!(estimate_tokens("hi!!你好吗"), 3);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn prefill_estimate_counts_image_tokens() {
        let req = crate::provider::ProviderRequest {
            model: "test-model".into(),
            system: None,
            messages: vec![crate::message::Message {
                role: crate::message::Role::User,
                content: vec![crate::message::ContentBlock::Image {
                    source: crate::message::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: crate::message::ImageDetail::High,
                }],
            }],
            tools: Vec::new(),
            params: crate::provider::GenParams::default(),
        };
        assert_eq!(
            estimate_prefill_tokens(&req),
            crate::compact::IMAGE_TOKEN_ESTIMATE as u32
        );
    }
}
