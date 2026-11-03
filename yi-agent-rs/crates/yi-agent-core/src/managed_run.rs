//! Managed agent runs: the real core task tree, owned by the caller.
//!
//! `Agent::run_blocks` spawns the loop with `tokio::spawn` and drops the
//! `JoinHandle`, so an outer consumer can only stop watching — it cannot stop
//! the run. A side conversation must be able to prove the actual core task
//! finished (spec §7.1): that is what [`ManagedRunControl`] exists for.
//!
//! The same ownership is offered without side budgets as
//! `Agent::run_tracked_blocks`, so the TUI can join the main session's real
//! task on exit without changing ordinary event or text policy.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent::AgentEvent;

/// Which event channel a run uses.
#[derive(Debug, Clone, Copy)]
pub(crate) enum EventSink {
    /// Historical unbounded channel, plain events.
    Plain,
    /// Bounded-by-budget queue, lease-carrying events.
    Budgeted(ManagedRunLimits),
}

/// Byte and item budgets for one managed run.
///
/// Task 03 wires the payload budgets (`text_bytes`, `binary_bytes`,
/// `max_event_bytes`, `control_reserve`) into the production path; this module
/// defines the shape and the queue bound so both halves agree on one struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedRunLimits {
    /// Maximum queued events before the producer stops.
    pub event_count: usize,
    /// Maximum queued serialized-event bytes before the producer stops.
    pub event_bytes: usize,
    /// Maximum size of a single event; larger payloads must travel as a shared
    /// handle instead of being copied into the queue.
    pub max_event_bytes: usize,
    /// Budget for core text/tool history per side.
    pub text_bytes: usize,
    /// Budget for decoded binary (images) per side.
    pub binary_bytes: usize,
    /// Space reserved so a terminal/error event can always be written.
    pub control_reserve: usize,
}

/// 256 queued items and 1 MiB of queued events; 128 KiB per event; 16 MiB of
/// core text, 64 MiB of decoded binary and a 4 KiB control reserve.
pub const SIDE_EVENT_QUEUE_ITEMS: usize = 256;
pub const SIDE_EVENT_QUEUE_BYTES: usize = 1024 * 1024;
pub const SIDE_MAX_EVENT_BYTES: usize = 128 * 1024;
pub const SIDE_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const SIDE_BINARY_BYTES: usize = 64 * 1024 * 1024;
pub const SIDE_CONTROL_RESERVE_BYTES: usize = 4 * 1024;

impl ManagedRunLimits {
    /// The side-conversation budgets from the design's global constraints.
    pub fn side_defaults() -> Self {
        Self {
            event_count: SIDE_EVENT_QUEUE_ITEMS,
            event_bytes: SIDE_EVENT_QUEUE_BYTES,
            max_event_bytes: SIDE_MAX_EVENT_BYTES,
            text_bytes: SIDE_TEXT_BYTES,
            binary_bytes: SIDE_BINARY_BYTES,
            control_reserve: SIDE_CONTROL_RESERVE_BYTES,
        }
    }
}

/// One event from a managed run, shared rather than copied.
///
/// Large payloads (images, tool arguments, thinking credentials) must not be
/// duplicated into the queue, so the event travels behind an `Arc`. Task 03
/// attaches resource leases here so a queued event keeps its budget while it
/// waits; the public accessor stays `event()`.
#[derive(Debug)]
pub struct BudgetedAgentEvent {
    event: Arc<AgentEvent>,
    /// Queue slot held until this event is dropped by the consumer.
    _lease: Option<QueueLease>,
}

impl BudgetedAgentEvent {
    pub fn new(event: AgentEvent) -> Self {
        Self {
            event: Arc::new(event),
            _lease: None,
        }
    }

    pub(crate) fn with_lease(event: AgentEvent, lease: Option<QueueLease>) -> Self {
        Self {
            event: Arc::new(event),
            _lease: lease,
        }
    }

    pub fn event(&self) -> &AgentEvent {
        &self.event
    }

    /// The shared payload, for consumers that need to retain the event past the
    /// stream item (e.g. a transcript holding one stable version).
    pub fn shared(&self) -> Arc<AgentEvent> {
        Arc::clone(&self.event)
    }
}

/// Runtime accounting for one managed run's event queue.
///
/// Production waits here before enqueueing, so the queue cannot outgrow
/// `event_count` / `event_bytes`; the reservation is released when the consumer
/// drops the item. Task 03 extends this with the payload budgets.
pub(crate) struct QueueBudget {
    limits: ManagedRunLimits,
    state: std::sync::Mutex<QueueState>,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct QueueState {
    items: usize,
    bytes: usize,
}

impl QueueBudget {
    pub(crate) fn new(limits: ManagedRunLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: std::sync::Mutex::new(QueueState::default()),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn fits(state: &QueueState, limits: &ManagedRunLimits, bytes: usize) -> bool {
        state.items < limits.event_count && state.bytes.saturating_add(bytes) <= limits.event_bytes
    }

    /// Wait until the queue has room for one more event of `bytes`.
    ///
    /// Cancellation wins over waiting: a stopped run must not park here.
    pub(crate) async fn reserve(
        self: &Arc<Self>,
        bytes: usize,
        cancel: &CancellationToken,
    ) -> Result<QueueLease, QueueFull> {
        loop {
            // Register the waiter before re-checking: otherwise a release that
            // lands between the check and `notified()` would be missed and this
            // task would sleep forever.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap();
                if Self::fits(&state, &self.limits, bytes) {
                    state.items += 1;
                    state.bytes += bytes;
                    return Ok(QueueLease {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = cancel.cancelled() => return Err(QueueFull::Cancelled),
            }
        }
    }

    /// Non-blocking reservation: `None` means the queue has no room right now.
    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<QueueLease> {
        let mut state = self.state.lock().unwrap();
        if Self::fits(&state, &self.limits, bytes) {
            state.items += 1;
            state.bytes += bytes;
            return Some(QueueLease {
                budget: Arc::clone(self),
                bytes,
            });
        }
        None
    }

    fn release(&self, bytes: usize) {
        {
            let mut state = self.state.lock().unwrap();
            state.items = state.items.saturating_sub(1);
            state.bytes = state.bytes.saturating_sub(bytes);
        }
        self.notify.notify_one();
    }
}

/// Why a queue reservation did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueFull {
    Cancelled,
}

/// One reserved queue slot; released on drop.
pub(crate) struct QueueLease {
    budget: Arc<QueueBudget>,
    bytes: usize,
}

impl std::fmt::Debug for QueueLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueLease")
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Drop for QueueLease {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// Cheap size estimate for queue accounting.
///
/// Strings dominate an event's footprint; fixed per-variant overhead plus the
/// string lengths is enough to bound the queue without serializing every delta.
pub(crate) fn approximate_event_bytes(event: &AgentEvent) -> usize {
    const OVERHEAD: usize = 64;
    let payload = match event {
        AgentEvent::AssistantText(text)
        | AgentEvent::DecodeDelta(text)
        | AgentEvent::ReasoningDelta { text, .. }
        | AgentEvent::Notice { text, .. } => text.len(),
        AgentEvent::ThinkingBlock { text, token } => text.len() + format!("{token:?}").len(),
        AgentEvent::ToolCall { name, input, .. } => name.len() + input.to_string().len(),
        AgentEvent::ToolResult { result, .. } => result.content.iter().map(block_bytes).sum(),
        AgentEvent::ToolOutputDelta { text, .. } => text.len(),
        AgentEvent::Error(error) => format!("{error}").len(),
        _ => 0,
    };
    OVERHEAD + payload
}

fn block_bytes(block: &crate::message::ContentBlock) -> usize {
    match block {
        crate::message::ContentBlock::Text(text) => text.len(),
        crate::message::ContentBlock::ToolUse { name, input, .. } => {
            name.len() + input.to_string().len()
        }
        crate::message::ContentBlock::ToolResult { content, .. } => {
            content.iter().map(block_bytes).sum()
        }
        other => format!("{other:?}").len(),
    }
}

/// A tracked run: its event stream and the ownership of the real task tree.
pub struct TrackedAgentRun {
    pub events: BoxStream<'static, AgentEvent>,
    pub control: ManagedRunControl,
}

/// A managed run: its event stream and the ownership of the real task tree.
pub struct ManagedAgentRun {
    pub events: BoxStream<'static, BudgetedAgentEvent>,
    pub control: ManagedRunControl,
}

/// The tasks one run spawned besides its root loop (the ToolEvent forwarders).
///
/// A `TaskTracker` alone can close and wait but cannot abort, and a run that is
/// being force-stopped must not leave a child parked forever. So each spawn
/// keeps its `AbortHandle` alongside the tracker.
#[derive(Clone)]
pub(crate) struct ChildTasks {
    tracker: TaskTracker,
    aborts: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl ChildTasks {
    pub(crate) fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            aborts: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Spawn a child that is part of this run's task tree.
    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = self.tracker.spawn(future);
        self.aborts.lock().unwrap().push(handle.abort_handle());
    }

    /// Stop accepting children and abort every one that is still running.
    fn abort_all(&self) {
        self.tracker.close();
        for handle in self.aborts.lock().unwrap().drain(..) {
            handle.abort();
        }
    }

    /// Abort everything and wait for the tracker to drain.
    async fn abort_and_join(&self) {
        self.abort_all();
        self.tracker.wait().await;
    }
}

/// Ownership of one run's real core task: the loop root plus every task it
/// spawned (the ToolEvent forwarders).
///
/// `Drop` cancels and aborts everything so nothing is ever detached. Only
/// [`ManagedRunControl::stop_and_join`] produces completion evidence (the root
/// `JoinHandle` is awaited), which is what a side close or TUI exit must show.
pub struct ManagedRunControl {
    /// Cancel token of *this* run, valid after the run reset.
    cancel: CancellationToken,
    root: JoinHandle<()>,
    children: ChildTasks,
}

impl ManagedRunControl {
    pub(crate) fn new(
        cancel: CancellationToken,
        root: JoinHandle<()>,
        children: ChildTasks,
    ) -> Self {
        Self {
            cancel,
            root,
            children,
        }
    }

    /// Cancel this run's loop and its spawned children.
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.children.abort_all();
    }

    /// Whether the real core loop task has already completed.
    pub fn is_finished(&self) -> bool {
        self.root.is_finished()
    }

    /// Cancel, wait up to `grace` for the real root task, then abort it and
    /// drain every tracked child.
    ///
    /// `grace` is the threshold for switching to a forced abort; it is not a
    /// promise that OS-level blocking work inside a tool returns in that time.
    pub async fn stop_and_join(&mut self, grace: Duration) -> Result<(), tokio::task::JoinError> {
        self.cancel.cancel();
        // Refuse new children, then wait for the root. Dropping the timeout
        // future does not cancel the root; we abort it explicitly so the call
        // always returns with a definite state.
        self.children.tracker.close();
        if tokio::time::timeout(grace, &mut self.root).await.is_err() {
            self.root.abort();
            let _ = (&mut self.root).await;
        }
        // A forwarder is released once its tool future drops; the abort is the
        // backstop for anything still parked on the root's cancellation.
        self.children.abort_and_join().await;
        Ok(())
    }
}

impl Drop for ManagedRunControl {
    fn drop(&mut self) {
        // Never detach: a dropped control still stops the run. Completion is
        // not awaited here (Drop cannot), which is why `stop_and_join` exists.
        self.cancel.cancel();
        self.children.abort_all();
        self.root.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_defaults_match_the_design_constraints() {
        let limits = ManagedRunLimits::side_defaults();
        assert_eq!(limits.event_count, 256);
        assert_eq!(limits.event_bytes, 1024 * 1024);
        assert_eq!(limits.max_event_bytes, 128 * 1024);
        assert_eq!(limits.text_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.binary_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.control_reserve, 4 * 1024);
    }

    #[test]
    fn budgeted_event_shares_its_payload() {
        let event = BudgetedAgentEvent::new(AgentEvent::AssistantText("hi".into()));
        let shared = event.shared();
        assert!(matches!(event.event(), AgentEvent::AssistantText(t) if t == "hi"));
        // Same allocation: cloning the handle must not copy the payload.
        assert!(std::ptr::eq(
            Arc::as_ptr(&shared),
            Arc::as_ptr(&event.shared())
        ));
    }
}
