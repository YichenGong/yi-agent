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

    /// Capacity available to ordinary (non-terminal) events.
    ///
    /// A terminal event (`Done`/`Error`/`Cancelled`) is the only signal that a
    /// run ended, so one slot and one maximum-size event are kept out of the
    /// body's reach. Without this, a full queue would swallow the very event
    /// that tells the consumer to stop (spec §7.1).
    /// Largest single event this queue accepts.
    pub(crate) fn max_event_bytes(&self) -> usize {
        self.limits.max_event_bytes
    }

    fn body_limits(&self) -> ManagedRunLimits {
        let mut limits = self.limits;
        limits.event_count = limits.event_count.saturating_sub(1);
        limits.event_bytes = limits.event_bytes.saturating_sub(limits.max_event_bytes);
        limits
    }

    fn fits(state: &QueueState, limits: &ManagedRunLimits, bytes: usize) -> bool {
        state.items < limits.event_count && state.bytes.saturating_add(bytes) <= limits.event_bytes
    }

    /// Wait until the queue has room for one more event of `bytes`.
    ///
    /// Cancellation wins over waiting: a stopped run must not park here. Body
    /// events may not consume the terminal headroom.
    pub(crate) async fn reserve(
        self: &Arc<Self>,
        bytes: usize,
        cancel: &CancellationToken,
    ) -> Result<QueueLease, QueueFull> {
        let body = self.body_limits();
        loop {
            // Register the waiter before re-checking: otherwise a release that
            // lands between the check and `notified()` would be missed and this
            // task would sleep forever.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap();
                if Self::fits(&state, &body, bytes) {
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
    ///
    /// Body events honour the terminal headroom; only
    /// [`Self::try_reserve_terminal`] may spend it.
    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<QueueLease> {
        let body = self.body_limits();
        let mut state = self.state.lock().unwrap();
        if Self::fits(&state, &body, bytes) {
            state.items += 1;
            state.bytes += bytes;
            return Some(QueueLease {
                budget: Arc::clone(self),
                bytes,
            });
        }
        None
    }

    /// Reserve from the full queue, including the headroom kept for a terminal
    /// event. Used only for `Done`/`Error`/`Cancelled`.
    pub(crate) fn try_reserve_terminal(self: &Arc<Self>, bytes: usize) -> Option<QueueLease> {
        let mut state = self.state.lock().unwrap();
        let fits = state.items < self.limits.event_count
            && state.bytes.saturating_add(bytes) <= self.limits.event_bytes;
        if fits {
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

/// What one queued event costs the *queue*.
///
/// Splittable text is bounded by [`split_event_for_queue`] and charged its
/// length. A payload-carrying event (tool call/result, thinking block) travels
/// as an `Arc` and its body is charged to the run's payload budget instead, so
/// the queue only pays for the handle — never for a second copy of a 64 MiB
/// image or a large tool result (spec §7.1).
pub(crate) fn queue_event_bytes(event: &AgentEvent) -> usize {
    const OVERHEAD: usize = 64;
    const HANDLE: usize = 64;
    match event {
        AgentEvent::AssistantText(text)
        | AgentEvent::DecodeDelta(text)
        | AgentEvent::ReasoningDelta { text, .. }
        | AgentEvent::Notice { text, .. }
        | AgentEvent::ToolOutputDelta { text, .. } => OVERHEAD + text.len(),
        // Payload-carrying: the body is already charged to the payload budget.
        AgentEvent::ThinkingBlock { .. }
        | AgentEvent::ToolCall { .. }
        | AgentEvent::ToolResult { .. } => OVERHEAD + HANDLE,
        AgentEvent::Error(error) => OVERHEAD + error.to_string().len(),
        _ => OVERHEAD,
    }
}

/// Split one event so that no queued event exceeds `max_bytes`.
///
/// Only text-carrying deltas are splittable; they are pure display fragments
/// that consumers concatenate, so splitting is transparent. Splits never land
/// inside a UTF-8 sequence. A credential, tool argument or tool result is *not*
/// splittable and is returned unchanged: it travels as a shared handle instead
/// of being copied into the queue.
pub(crate) fn split_event_for_queue(event: AgentEvent, max_bytes: usize) -> Vec<AgentEvent> {
    let max = max_bytes.saturating_sub(64).max(1);
    match event {
        AgentEvent::AssistantText(text) if text.len() > max => split_utf8(&text, max)
            .into_iter()
            .map(AgentEvent::AssistantText)
            .collect(),
        AgentEvent::DecodeDelta(text) if text.len() > max => split_utf8(&text, max)
            .into_iter()
            .map(AgentEvent::DecodeDelta)
            .collect(),
        AgentEvent::ToolOutputDelta { id, stream, text } if text.len() > max => {
            split_utf8(&text, max)
                .into_iter()
                .map(|chunk| AgentEvent::ToolOutputDelta {
                    id: id.clone(),
                    stream,
                    text: chunk,
                })
                .collect()
        }
        other => vec![other],
    }
}

/// Split `text` into chunks of at most `max` bytes on char boundaries.
pub(crate) fn split_utf8(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + max).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // `max` is smaller than one character; take the whole character so
            // the loop always advances.
            end = start
                + text[start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Whether this event is the run's terminal signal.
pub(crate) fn is_terminal_event(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Done { .. } | AgentEvent::Error(_) | AgentEvent::Cancelled
    )
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
    /// Set once the root handle has been awaited to completion. A `JoinHandle`
    /// must never be polled again after it resolves, so a bounded join that is
    /// retried has to know whether there is anything left to wait for.
    root_joined: bool,
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
            root_joined: false,
        }
    }

    /// Cancel this run's loop and its spawned children.
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.children.abort_all();
    }

    /// Whether the root task has been awaited to completion by a join.
    ///
    /// Exposed so a caller can tell "still waiting" from "already joined" without
    /// re-polling the `JoinHandle`.
    pub fn root_joined(&self) -> bool {
        self.root_joined
    }

    /// Cancel, then wait up to `grace` for the root **and** its children, reporting
    /// whether both really finished.
    ///
    /// Unlike [`Self::stop_and_join`], this never hides a timeout: `false` means an
    /// owner is still alive (the root handle is kept, only aborted), so a caller
    /// that must prove quiescence can retry against the same control instead of
    /// claiming the resources were released.
    pub async fn stop_and_join_bounded(&mut self, grace: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + grace;
        self.cancel();
        self.children.tracker.close();
        if !self.root_joined {
            if tokio::time::timeout_at(deadline, &mut self.root)
                .await
                .is_err()
            {
                // Abort, but keep the handle: the owner is still draining, and a
                // later retry must be able to observe it finish.
                self.root.abort();
                return false;
            }
            self.root_joined = true;
        }
        tokio::time::timeout_at(deadline, self.children.tracker.wait())
            .await
            .is_ok()
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
        // Either branch awaited the handle to completion, so a later bounded join
        // must not poll it again.
        self.root_joined = true;
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

    fn text_event(len: usize) -> AgentEvent {
        AgentEvent::AssistantText("x".repeat(len))
    }

    #[test]
    fn side_terminal_events_are_recognised() {
        assert!(is_terminal_event(&AgentEvent::Done {
            reason: crate::agent::DoneReason::EndTurn
        }));
        assert!(is_terminal_event(&AgentEvent::Cancelled));
        assert!(!is_terminal_event(&text_event(1)));
    }

    #[test]
    fn side_queue_reserves_headroom_for_the_terminal_event() {
        let limits = ManagedRunLimits::side_defaults();
        let budget = QueueBudget::new(ManagedRunLimits {
            event_count: 2,
            event_bytes: 1024 + limits.max_event_bytes,
            ..limits
        });
        // Body events may only use one slot and 1 KiB: the second slot and one
        // maximum-size event stay free.
        let _first = budget.try_reserve(1024).expect("first body event fits");
        assert!(
            budget.try_reserve(1).is_none(),
            "the last slot and one max event must stay reserved for the terminal"
        );
        // The terminal event still fits, which is what makes it observable.
        assert!(budget.try_reserve_terminal(1024).is_some());
    }

    #[test]
    fn side_text_event_splitting_never_breaks_utf8() {
        let event = text_event(4096);
        let parts = split_event_for_queue(event, 512);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(queue_event_bytes(part) <= 512);
        }

        // Multi-byte characters must not be cut in half.
        let cjk = AgentEvent::AssistantText("中".repeat(200));
        let parts = split_event_for_queue(cjk, 512);
        let joined: String = parts
            .iter()
            .map(|part| match part {
                AgentEvent::AssistantText(text) => text.as_str(),
                _ => panic!("unexpected variant"),
            })
            .collect();
        assert_eq!(joined, "中".repeat(200));
        for part in &parts {
            if let AgentEvent::AssistantText(text) = part {
                assert!(text.len() <= 512 - 64 || text.chars().count() == 1);
            }
        }
    }

    #[test]
    fn side_credentials_are_never_split_into_the_queue() {
        // A thinking credential is not a display fragment: splitting it would
        // corrupt the value the provider must verify.
        let event = AgentEvent::ThinkingBlock {
            text: String::new(),
            token: crate::message::ThinkingToken::Signature("s".repeat(4096)),
        };
        let parts = split_event_for_queue(event, 512);
        assert_eq!(parts.len(), 1);
        // Only the handle is charged to the queue, not the credential body:
        // the payload budget owns those bytes.
        assert!(queue_event_bytes(&parts[0]) <= 512);
    }

    #[test]
    fn side_queue_size_estimate_does_not_copy_payloads() {
        // A 1 MiB inline image is measured by its handle, so the estimate stays
        // tiny; formatting it into a debug string would have allocated 1 MiB.
        let event = AgentEvent::ToolResult {
            id: "t".into(),
            result: crate::tool::ToolResult {
                content: vec![crate::message::ContentBlock::Image {
                    source: crate::message::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "A".repeat(1024 * 1024),
                    },
                    detail: crate::message::ImageDetail::High,
                    path: None,
                }],
                is_error: false,
            },
        };
        assert!(queue_event_bytes(&event) < 256);
    }

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

#[cfg(test)]
mod control_tests {
    use std::time::Duration;

    use super::*;

    /// A control whose root ignores cancellation and parks on `gate`, so a
    /// bounded join provably times out instead of reporting success by accident.
    /// Returns the sender: dropping it releases the root.
    fn gated_control() -> (ManagedRunControl, tokio::sync::oneshot::Sender<()>) {
        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let root = tokio::spawn(async move {
            let _ = rx.await;
        });
        (ManagedRunControl::new(cancel, root, ChildTasks::new()), tx)
    }

    #[tokio::test(start_paused = true)]
    async fn a_bounded_join_reports_false_when_the_root_does_not_finish() {
        let (mut control, _gate) = gated_control();
        assert!(
            !control
                .stop_and_join_bounded(Duration::from_millis(50))
                .await,
            "an unfinished root must not be reported as joined"
        );
        assert!(!control.root_joined(), "the owner must stay unconfirmed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_bounded_join_after_completion_is_not_a_panic() {
        let cancel = CancellationToken::new();
        let root = tokio::spawn(async {});
        let mut control = ManagedRunControl::new(cancel, root, ChildTasks::new());
        assert!(control.stop_and_join_bounded(Duration::from_secs(1)).await);
        assert!(control.root_joined());
        // The root handle is consumed: a retry must skip it rather than poll a
        // finished JoinHandle again (which panics).
        assert!(control.stop_and_join_bounded(Duration::from_secs(1)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_the_owner_finishes_reaches_joined() {
        let (mut control, gate) = gated_control();
        assert!(
            !control
                .stop_and_join_bounded(Duration::from_millis(20))
                .await
        );
        // The first attempt aborted the root; releasing the gate lets it really
        // finish, and the retry must observe the same handle instead of leaking it.
        drop(gate);
        assert!(
            control.stop_and_join_bounded(Duration::from_secs(5)).await,
            "the retry must observe the now-finished root"
        );
        assert!(control.root_joined());
    }

    #[tokio::test(start_paused = true)]
    async fn the_existing_stop_and_join_also_records_the_joined_root() {
        // Callers of the older API must not leave a handle that a later bounded
        // join would poll a second time.
        let cancel = CancellationToken::new();
        let root = tokio::spawn(async {});
        let mut control = ManagedRunControl::new(cancel, root, ChildTasks::new());
        control.stop_and_join(Duration::from_secs(1)).await.unwrap();
        assert!(control.root_joined());
        assert!(control.stop_and_join_bounded(Duration::from_secs(1)).await);
    }
}
