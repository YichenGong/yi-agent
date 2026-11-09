//! One side conversation's multi-turn lifecycle.
//!
//! The runner owns everything a side turn needs: the side's **own** `Session`
//! (forked once from a consistent parent snapshot), the managed core run, the
//! provider stream, the bounded event pump and the resource lease. Four
//! guarantees shape the design:
//!
//! 1. **A stop really stops.** `interrupt` and the 10-minute cap cancel the turn's
//!    IO token and the run's core control; the pump then joins the *actual* core
//!    task (spec §7.1). Dropping the event stream is never treated as evidence that
//!    the work ended.
//! 2. **Completion is evidence, not an assumption.** The pump is the single owner
//!    of `ManagedRunControl`, so exactly one place proves the core task finished
//!    and then publishes the terminal state.
//! 3. **A late turn cannot write a newer one.** The terminal state is fenced on the
//!    turn it belongs to; a stale completion only cleans up.
//! 4. **Stopping is observable.** `interrupt` answers as soon as the stop is
//!    recorded; `Idle` is published only after the join, so a caller never sees a
//!    quiet state while work is still live.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use yi_agent_core::managed_run::{BudgetedAgentEvent, ManagedRunControl, ManagedRunLimits};
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Session, TokenUsage, forkable_messages};
use yi_agent_tools::SideReadAccess;

use super::policy::SideToolSetupFactory;
use super::resources::PreparedSideResources;
use super::source::EffectiveSideSource;
use super::{SideError, SideErrorReason, SideResult};

/// Hard cap on one side turn.
pub const SIDE_TURN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Grace period before a stop escalates to a forced abort of the real core task.
///
/// This is the threshold for switching strategy, not a promise that OS-level
/// blocking work returns within it (spec §7.1): an unfinished blocking read cannot
/// be killed, so it keeps its resource slot and cleans up later.
pub const SIDE_STOP_GRACE: Duration = Duration::from_secs(5);

/// Lifecycle of one side conversation.
///
/// Not a wire type: the app-server mirrors it as `protocol::SideStatusWire` rather
/// than making this crate depend on the desktop binding generator. The mirror is
/// kept honest by an exhaustive `match` in the conversion, which stops compiling
/// when a variant is added here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideStatus {
    Creating,
    Idle,
    Running,
    Failed,
    Closing,
    Closed,
}

/// One streamed event, labelled with the conversation and turn it belongs to.
///
/// The labels are not decoration: a consumer demultiplexing several sides drops
/// stale frames by them, and a client that reconnects matches an interrupt against
/// the turn id.
#[derive(Debug)]
pub struct SideRunnerEvent {
    pub side_id: String,
    pub generation: u64,
    pub turn_id: String,
    pub event: BudgetedAgentEvent,
}

/// Accumulated token usage for one side, counted from zero.
///
/// The parent's usage is never inherited: a side's context is forked but its cost
/// is its own. A single provider reply fits in `u32`, but a long-lived side sums
/// many of them, so the totals are `u64`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SideTokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub reasoning: u64,
}

impl SideTokenTotals {
    /// Add another turn's totals, saturating rather than wrapping.
    fn add_totals(&mut self, other: &SideTokenTotals) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }

    /// Add one provider reply.
    ///
    /// Saturating rather than wrapping: a wrap would silently under-report real
    /// spend, which is worse than a visibly pinned maximum.
    fn add_usage(&mut self, usage: &TokenUsage) {
        self.input = self.input.saturating_add(u64::from(usage.input_tokens));
        self.output = self.output.saturating_add(u64::from(usage.output_tokens));
        self.cache_creation = self
            .cache_creation
            .saturating_add(u64::from(usage.cache_creation_input_tokens.unwrap_or(0)));
        self.cache_read = self
            .cache_read
            .saturating_add(u64::from(usage.cache_read_input_tokens.unwrap_or(0)));
        self.reasoning = self
            .reasoning
            .saturating_add(u64::from(usage.reasoning_tokens.unwrap_or(0)));
    }
}

/// A side's usage, and whether the provider actually reported totals.
///
/// `complete == false` means no final usage arrived (a cancelled or aborted turn).
/// Reporting zeros as if they were measured would be a lie, so the flag travels
/// with the numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SideUsage {
    pub total: SideTokenTotals,
    pub complete: bool,
}

/// What the pump publishes when a turn really ends.
#[derive(Debug, Default)]
struct Completion {
    /// Provider reported final usage for this turn.
    saw_usage: bool,
    /// The turn ended in an error rather than normally or by cancellation.
    error: Option<SideError>,
    /// The 10-minute cap fired.
    timed_out: bool,
    /// The real core task and its children were joined before this was published.
    joined: bool,
}

/// Signals completion to anything waiting outside the runner's own lock.
#[derive(Default)]
struct Quiescent {
    finished: AtomicBool,
    notify: Notify,
    completion: Mutex<Option<Completion>>,
}

impl Quiescent {
    fn publish(&self, completion: Completion) {
        *self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(completion);
        self.finished.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Wait for the pump to join the real core task and publish.
    async fn wait(&self) {
        loop {
            // Register the waiter *before* checking, so a publish between the check
            // and the await cannot be missed.
            let notified = self.notify.notified();
            if self.finished.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn take_completion(&self) -> Option<Completion> {
        self.completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Everything the runner and its pump must share, without sharing the session.
struct TurnShared {
    quiescent: Quiescent,
    /// Cancelled by `interrupt`; the pump turns it into `control.cancel()`.
    ///
    /// A token rather than a `Notify`: a stop that arrives before the pump's first
    /// poll must still be seen, and `notify_waiters` only wakes existing waiters.
    stop: CancellationToken,
    /// Totals for the active turn, filled by the pump as replies arrive.
    usage: Mutex<SideTokenTotals>,
}

impl Default for TurnShared {
    fn default() -> Self {
        Self {
            quiescent: Quiescent::default(),
            stop: CancellationToken::new(),
            usage: Mutex::new(SideTokenTotals::default()),
        }
    }
}

/// The turn currently owned by the runner.
struct ActiveTurn {
    turn_id: String,
    /// Cancels this turn's tool IO and blocking work. Never the side's lifetime.
    io: CancellationToken,
    stopping: bool,
    shared: Arc<TurnShared>,
}

/// One side conversation.
pub struct SideConversationRunner {
    side_id: String,
    generation: u64,
    source: EffectiveSideSource,
    /// The side's own session; never the parent's handle.
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    tools: SideToolSetupFactory,
    resources: PreparedSideResources,
    status: SideStatus,
    active: Option<ActiveTurn>,
    usage: SideUsage,
    /// Completed turns whose usage was added, so `complete` can mean "every turn
    /// reported final usage" rather than "the last one did".
    completed_turns: u32,
    /// At least one completed turn ended without a provider usage report.
    usage_incomplete: bool,
    /// The last finished turn proved its core task ended.
    last_turn_joined: bool,
    /// Set once close was requested.
    closing: bool,
    /// Diagnostic from the last failed turn.
    last_error: Option<SideError>,
}

impl SideConversationRunner {
    /// Prepare a runner from an already-consistent source.
    ///
    /// The session is forked **once**, here: from then on the side and the parent
    /// evolve independently, and a later parent compaction cannot reach into this
    /// conversation.
    pub fn new(
        side_id: String,
        source: EffectiveSideSource,
        resources: PreparedSideResources,
        tools: SideToolSetupFactory,
    ) -> SideResult<Self> {
        let (snapshot, config) = {
            let guard = source
                .session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let snapshot = forkable_messages(&guard);
            if snapshot.is_empty() {
                return Err(SideError::new(
                    SideErrorReason::ParentNotStarted,
                    "父会话还没有可继承的历史",
                ));
            }
            (snapshot, source.config.clone())
        };
        if config.model.trim().is_empty() {
            return Err(SideError::new(
                SideErrorReason::SourceChanged,
                "旁支没有可用的继承模型",
            ));
        }
        // The hidden boundary is appended **here**, once, on the copy that only
        // this side owns. Doing it on the parent's handle or again on every turn
        // would either leak side-only text into the parent or stack the marker.
        //
        // Reusing `snapshot_for_side` keeps trimming and marker insertion in the
        // one place core defines them, instead of open-coding the same rule in the
        // runner and letting the two drift.
        let side_snapshot = yi_agent_core::snapshot_for_side(&Session::from_messages(snapshot))
            .map_err(|_| {
                SideError::new(
                    SideErrorReason::ParentNotStarted,
                    "父会话还没有可继承的历史",
                )
            })?;
        let mut session = Session::new();
        for message in side_snapshot.messages {
            session.push(message);
        }
        let generation = source.generation;
        Ok(Self {
            side_id,
            generation,
            source,
            session: Arc::new(Mutex::new(session)),
            config,
            tools,
            resources,
            status: SideStatus::Idle,
            active: None,
            usage: SideUsage::default(),
            completed_turns: 0,
            usage_incomplete: false,
            last_turn_joined: false,
            closing: false,
            last_error: None,
        })
    }

    pub fn side_id(&self) -> &str {
        &self.side_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn status(&self) -> SideStatus {
        self.status
    }

    pub fn usage(&self) -> SideUsage {
        self.usage
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        self.active.as_ref().map(|turn| turn.turn_id.as_str())
    }

    /// Whether a stop is in flight for the active turn.
    pub fn is_stopping(&self) -> bool {
        self.active
            .as_ref()
            .map(|turn| turn.stopping)
            .unwrap_or(false)
    }

    pub fn last_error(&self) -> Option<&SideError> {
        self.last_error.as_ref()
    }

    /// The side's private directory; its lease keeps it alive until the runner and
    /// every work handle are gone.
    pub fn resource_dir(&self) -> &std::path::Path {
        self.resources.lease.dir()
    }

    pub fn access(&self) -> &Arc<SideReadAccess> {
        &self.resources.access
    }

    /// A copy of the side's own history, marker included.
    ///
    /// Exists so a test can assert on what the model actually receives (the
    /// inherited prefix **and** the hidden boundary) instead of on the source it
    /// was built from: the whole failure this guards against was a runner that
    /// looked correct but never appended the marker.
    pub fn session_snapshot(&self) -> Vec<yi_agent_core::Message> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .messages()
            .to_vec()
    }

    /// Accept one turn and hand back its event stream.
    ///
    /// Returns as soon as the turn is accepted: it does not wait for the provider,
    /// and the returned stream is not a proxy for completion. One turn at a time —
    /// a second call while a turn is live is refused, which is what stops a client
    /// retry from starting two runs.
    pub async fn start_turn(
        &mut self,
        turn_id: String,
        text: String,
    ) -> SideResult<BoxStream<'static, SideRunnerEvent>> {
        if self.closing || matches!(self.status, SideStatus::Closing | SideStatus::Closed) {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                "旁支正在关闭，不再接受新的问题",
            ));
        }
        if turn_id.trim().is_empty() {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                "缺少 turn id",
            ));
        }
        if text.trim().is_empty() {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                "旁支问题不能为空",
            ));
        }
        if let Some(active) = &self.active {
            return Err(SideError::new(
                SideErrorReason::TurnInProgress,
                format!("旁支当前正在进行 {}，请先停止或等待完成", active.turn_id),
            ));
        }

        // A fresh IO token per turn: the previous turn's token must never cancel
        // this one, and this one must never cancel the side's lifetime.
        let io = CancellationToken::new();
        let registry = self.tools.build_for_turn(io.clone())?;
        let mut agent = Agent::new(
            Arc::clone(&self.source.provider),
            Arc::new(registry),
            self.config.clone(),
        )
        .with_session_arc(Arc::clone(&self.session));
        if let Some(gate) = &self.source.provider_gate {
            agent = agent.with_provider_turn_gate(Arc::clone(gate));
        }
        // No live-settings source is attached. The side runs on the configuration
        // frozen at entry, which is what makes it a snapshot: a later save in the
        // parent must not change the side's prompt, model or history mid-way, and
        // enabling one here would also let a save reach into a conversation whose
        // premise is "this is what the parent looked like when I asked".

        let run = agent
            .run_managed_blocks(
                vec![yi_agent_core::ContentBlock::Text(text)],
                ManagedRunLimits::side_defaults(),
            )
            .await
            .map_err(|error| {
                SideError::new(
                    SideErrorReason::ResourceLimit,
                    format!("无法启动旁支本轮：{error}"),
                )
            })?;

        let shared = Arc::new(TurnShared::default());
        self.status = SideStatus::Running;
        self.last_error = None;
        self.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            io: io.clone(),
            stopping: false,
            shared: Arc::clone(&shared),
        });

        let (tx, rx) = futures::channel::mpsc::channel(SIDE_EVENT_STREAM_LAG);
        let side_id = self.side_id.clone();
        let generation = self.generation;
        let pump = Pump {
            side_id,
            generation,
            turn_id,
            control: run.control,
            io,
            shared: Arc::clone(&shared),
            consumer_open: true,
        };
        tokio::spawn(pump.run(run.events, tx));

        Ok(Box::pin(rx))
    }

    /// Stop the active turn, answering as soon as the stop is recorded.
    ///
    /// The terminal state is published later, when the pump has joined the real
    /// core task, so a caller never observes `Idle` while work is still live.
    pub async fn interrupt(&mut self, turn_id: &str) -> SideResult<()> {
        let Some(active) = self.active.as_mut() else {
            // An interrupt for a turn that already finished is idempotent.
            return Ok(());
        };
        if active.turn_id != turn_id || active.stopping {
            // A stale interrupt for an older turn must never cancel this one.
            return Ok(());
        }
        active.stopping = true;
        // Cancel this turn's tools and blocking work, then ask the pump to cancel
        // the real core task. Joining happens there, so this call never blocks the
        // caller on OS-level IO.
        active.io.cancel();
        active.shared.stop.cancel();
        Ok(())
    }

    /// Wait until the active run has really finished and its state is published.
    ///
    /// Call outside any registry lock. Idempotent when nothing is running.
    pub async fn wait_quiescent(&mut self) -> SideResult<()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let shared = Arc::clone(&active.shared);
        shared.quiescent.wait().await;

        let completion = shared.quiescent.take_completion().unwrap_or_default();
        let turn_usage = *shared
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only the active turn may write: a completion observed after a newer turn
        // started is stale and must not touch the newer state.
        let matches_active = self
            .active
            .as_ref()
            .map(|turn| Arc::ptr_eq(&turn.shared, &shared))
            .unwrap_or(false);
        if !matches_active {
            return Ok(());
        }

        // Usage accumulates across the side's life: it is the conversation's cost,
        // not the last turn's.
        let mut total = self.usage.total;
        total.add_totals(&turn_usage);
        self.usage.total = total;
        self.completed_turns = self.completed_turns.saturating_add(1);
        if !completion.saw_usage || completion.timed_out {
            self.usage_incomplete = true;
        }
        self.usage.complete = self.completed_turns > 0 && !self.usage_incomplete;
        self.last_turn_joined = completion.joined;
        if let Some(error) = completion.error {
            self.last_error = Some(error);
            self.status = SideStatus::Failed;
        } else if self.closing {
            self.status = SideStatus::Closing;
        } else {
            self.status = SideStatus::Idle;
        }
        self.active = None;
        Ok(())
    }

    /// Close the conversation: refuse new turns, stop the active one, and keep the
    /// resources until their last owner drops.
    pub async fn close(&mut self) -> SideResult<()> {
        if matches!(self.status, SideStatus::Closed) {
            return Ok(());
        }
        self.closing = true;
        self.status = SideStatus::Closing;
        if let Some(active) = self.active.as_mut() {
            active.stopping = true;
            active.io.cancel();
            active.shared.stop.cancel();
        }
        // The core task must be joined before this counts as closed; otherwise a
        // late tool result could still write into the side's session.
        self.wait_quiescent().await?;
        {
            // Converge to the safe prefix: a forced abort can leave a trailing
            // assistant message whose tool_use has no result, which the provider
            // would reject on the next turn.
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let safe = forkable_messages(&session);
            session.replace_messages(safe);
        }
        self.active = None;
        self.status = SideStatus::Closed;
        Ok(())
    }

    /// Whether the last finished turn proved its core task was joined.
    ///
    /// A host that needs completion evidence (a close response, a test) checks this
    /// rather than inferring it from a stream ending.
    pub fn last_turn_was_joined(&self) -> bool {
        self.last_turn_joined
    }

    /// The side's own transcript, for tests and diagnostics.
    pub fn messages(&self) -> Vec<yi_agent_core::Message> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .messages()
            .to_vec()
    }
}

/// How far the consumer may fall behind before the pump stops forwarding.
///
/// This bounds only the *stream* buffer. The run's own queue budgets are what bound
/// memory inside core; a slow consumer here can never grow either one without
/// limit.
pub const SIDE_EVENT_STREAM_LAG: usize = 256;

/// The single owner of one turn's run.
struct Pump {
    side_id: String,
    generation: u64,
    turn_id: String,
    control: ManagedRunControl,
    io: CancellationToken,
    shared: Arc<TurnShared>,
    /// Whether forwarding is still useful; false once the consumer went away.
    consumer_open: bool,
}

impl Pump {
    async fn run(
        mut self,
        mut events: BoxStream<'static, BudgetedAgentEvent>,
        mut tx: futures::channel::mpsc::Sender<SideRunnerEvent>,
    ) {
        let deadline = tokio::time::Instant::now() + SIDE_TURN_TIMEOUT;
        let deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline);
        let mut timed_out = false;
        let mut run_error = None;
        // `CancellationToken::cancelled()` stays ready forever once cancelled: without
        // this latch the biased branch would win every iteration and the pump would
        // never read the run's terminal event (so it could never join). The token is
        // still what makes a stop that lands before the first poll observable.
        let mut stop_handled = false;

        loop {
            tokio::select! {
                // A stop request wins over new output, so a stopped turn cannot
                // append more assistant text before it goes quiet.
                biased;
                _ = self.shared.stop.cancelled(), if !stop_handled => {
                    stop_handled = true;
                    self.control.cancel();
                }
                _ = &mut deadline, if !timed_out => {
                    timed_out = true;
                    self.control.cancel();
                }
                next = events.next() => {
                    let Some(queued) = next else { break };
                    self.account(queued.event());
                    if let AgentEvent::Error(error) = queued.event() {
                        // Provider errors can contain response bodies and credentials.
                        // Preserve the failed state, but publish only a safe summary.
                        run_error = Some(match error {
                            yi_agent_core::AgentError::Budget(_) => SideError::new(
                                SideErrorReason::ResourceLimit,
                                "旁问本轮已达到资源上限，请关闭后重新开启旁问",
                            ),
                            _ => SideError::new(
                                SideErrorReason::TurnFailed,
                                "旁问本轮调用失败，请检查模型配置或稍后重试",
                            ),
                        });
                    }
                    let terminal = is_terminal(queued.event());
                    if self.consumer_open {
                        let frame = SideRunnerEvent {
                            side_id: self.side_id.clone(),
                            generation: self.generation,
                            turn_id: self.turn_id.clone(),
                            event: queued,
                        };
                        // A lagging consumer is not a reason to stall the run: keep
                        // draining, just stop forwarding.
                        if tx.try_send(frame).is_err() {
                            self.consumer_open = false;
                        }
                    }
                    if terminal {
                        break;
                    }
                }
            }
        }

        // Completion evidence: join the actual core task (and its children). Only
        // after this may callers see the turn as finished. Dropping the control
        // aborts, but does not wait, so publishing before this would let a caller
        // observe Idle while the task was still unwinding.
        let joined = self.control.stop_and_join(SIDE_STOP_GRACE).await.is_ok();
        self.io.cancel();
        let error = if timed_out {
            Some(SideError::new(
                SideErrorReason::ResourceLimit,
                "旁支本轮超过 10 分钟上限，已停止",
            ))
        } else {
            run_error
        };
        let saw_usage = self
            .shared
            .usage
            .lock()
            .map(|u| *u != SideTokenTotals::default())
            .unwrap_or(false);
        self.shared.quiescent.publish(Completion {
            saw_usage,
            error,
            timed_out,
            joined,
        });
    }

    /// Fold one event into the turn's usage totals.
    fn account(&self, event: &AgentEvent) {
        if let AgentEvent::Usage { usage, .. } = event
            && let Ok(mut totals) = self.shared.usage.lock()
        {
            totals.add_usage(usage);
        }
    }
}

/// Whether an event ends a run.
fn is_terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
    )
}

#[cfg(test)]
mod tests;
