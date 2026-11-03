//! The process-wide side-conversation registry.
//!
//! One registry per host process, cloned (as an `Arc`) into every transport. That
//! is a correctness requirement rather than an optimisation: a per-connection
//! registry would give the desktop and the phone two different sides for the same
//! parent, each with its own runner and its own copy of the history.
//!
//! Three rules drive the design:
//!
//! 1. **One parent, one side.** `open` is create-or-get. Two concurrent opens
//!    collapse to one runner; the second waits for the first instead of starting a
//!    second provider stream.
//! 2. **A retry is not a new turn.** `turn_start` consults the request ledger
//!    *before* the busy check, so a client that retried after its first attempt was
//!    accepted gets the original turn id back rather than `turn_in_progress`.
//! 3. **A parent change is a barrier, not a notification.** Clearing or deleting a
//!    parent runs `begin_parent_change`, which refuses new opens for that parent
//!    before the destructive work starts and either commits (invalidating the
//!    generation) or rolls back if that work failed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use yi_agent_runtime::side_conversation::{
    CloseReason, SideConversationRunner, SideHostResources, SideIdleDeadline, SideRequestLedger,
    SideStatus, SideToolSetupFactory, SideTranscript,
};

use yi_agent_runtime::side_conversation::{SideError, SideErrorReason, SideUsage};
use yi_agent_tools::SideBlockingPool;

use super::source::SideParentResolver;

/// How many closed sides a host remembers for idempotent retries.
pub const SIDE_TOMBSTONES: usize = 1024;

/// How long a tombstone lives.
///
/// Five minutes is enough for a reconnect to reconcile (spec §8.1) and short
/// enough that the table cannot become a second history.
pub const SIDE_TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The wire-facing summary of one side (spec §8.1).
///
/// This is a *wire* type. It intentionally does **not** derive ts-rs: the TS
/// bindings are generated from `yi-agent-app-server`, which may depend on this
/// crate's runtime but not the other way around, so a ts-rs derive here could
/// never reach the exporter. `protocol::SideSummaryWire` mirrors it field for
/// field, and the conversion is an exhaustive `match`/destructure, so adding a
/// field or a status variant breaks compilation instead of silently drifting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SideSummary {
    pub side_id: String,
    pub parent_thread_id: String,
    pub status: SideStatus,
    pub snapshot_revision: u64,
    pub last_seq: u64,
    #[serde(default)]
    pub active_turn_id: Option<String>,
    /// Epoch milliseconds; `None` while `Creating` or `Running`.
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub generation: u64,
    /// `true` while an accepted turn has been asked to stop but has not yet proven
    /// its core task ended.
    pub stopping: bool,
}

/// What a tombstone keeps.
///
/// Deliberately only identifiers and request digests: no text, no items, no
/// provider output. A tombstone answers "did you already accept this?" and
/// nothing else, so it can never be used to recover content after a close.
#[derive(Debug, Clone)]
struct Tombstone {
    side_id: String,
    parent: String,
    generation: u64,
    accepted: Vec<(String, String)>,
    created_at: tokio::time::Instant,
}

/// One side's mutable state, held behind an async mutex because a turn touches it
/// across awaits.
struct SideEntry {
    side_id: String,
    parent: String,
    generation: u64,
    runner: SideConversationRunner,
    ledger: SideRequestLedger,
    deadline: SideIdleDeadline,
    transcript: SideTranscript,
    /// Monotonic state revision. Bumped once per published change.
    seq: AtomicU64,
    /// `true` between an accepted stop and the pump's terminal publication.
    stopping: std::sync::atomic::AtomicBool,
    /// `true` once `close` entered `Closing`.
    closing: bool,
    /// Captured once, when the side is created.
    config: SideEffectiveConfig,
}

impl SideEntry {
    fn summary(&self) -> SideSummary {
        let status = self.runner.status();
        let expires_at = match status {
            SideStatus::Creating | SideStatus::Running | SideStatus::Closing => None,
            _ => Some(epoch_millis(self.deadline.expires_at())),
        };
        SideSummary {
            side_id: self.side_id.clone(),
            parent_thread_id: self.parent.clone(),
            status,
            snapshot_revision: self.runner_revision(),
            last_seq: self.seq.load(Ordering::SeqCst),
            active_turn_id: self.runner.active_turn_id().map(str::to_string),
            expires_at,
            generation: self.generation,
            stopping: self.stopping.load(Ordering::SeqCst),
        }
    }

    fn runner_revision(&self) -> u64 {
        self.generation
    }
}

/// How a parent is currently occupied.
enum ParentSlot {
    /// A creation is in flight; everyone else waits on the notify.
    Creating(Arc<Notify>),
    Ready(String),
}

/// The host's side registry.
pub struct SideRegistry {
    resolver: Arc<dyn SideParentResolver>,
    resources: Arc<SideHostResources>,
    pool: Arc<SideBlockingPool>,
    parents: Mutex<HashMap<String, ParentSlot>>,
    sides: Mutex<HashMap<String, Arc<Mutex<SideEntry>>>>,
    tombstones: Mutex<HashMap<String, Tombstone>>,
    /// The notify of each in-flight create, kept until the creator wakes waiters.
    ///
    /// Separate from `parents` because the placeholder is *replaced* by `Ready` on
    /// success while the notify still has waiters; mixing the two would lose them.
    creating_notify: Mutex<HashMap<String, Arc<Notify>>>,
    /// Bumped on every successful create so a side id is never reused.
    next_generation: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl SideRegistry {
    pub fn new(
        resolver: Arc<dyn SideParentResolver>,
        resources: Arc<SideHostResources>,
        pool: Arc<SideBlockingPool>,
    ) -> Self {
        Self {
            resolver,
            resources,
            pool,
            parents: Mutex::new(HashMap::new()),
            sides: Mutex::new(HashMap::new()),
            tombstones: Mutex::new(HashMap::new()),
            creating_notify: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// This host process's identity.
    ///
    /// Every transport in one process must answer with the same id, so clients can
    /// tell "same host, reconnected" from "host restarted" without guessing. The id
    /// lives on the resource layer because that is what a restart actually replaces.
    pub fn host_instance_id(&self) -> &str {
        self.resources.host_instance_id()
    }

    /// Create or return the parent's side.
    ///
    /// Concurrent calls for one parent collapse: the first becomes the creator and
    /// the rest wait on its notify, then re-read the table. That is what keeps two
    /// transports from starting two provider streams on one history.
    pub async fn open(&self, parent: &str) -> Result<SideSummary, SideError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SideError::new(SideErrorReason::SideClosing, "宿主正在关闭"));
        }
        loop {
            // The wait future is created and *enabled* while the map lock is held.
            // Creating it after dropping the lock leaves a window in which the
            // creator publishes `Ready` and notifies nobody; the waiter then sleeps
            // on a notification that already happened. `enable` registers now, so
            // that race cannot exist.
            let wait = {
                let mut parents = self.parents.lock().await;
                match parents.get(parent) {
                    Some(ParentSlot::Ready(side_id)) => Some(Err(side_id.clone())),
                    Some(ParentSlot::Creating(notify)) => {
                        let mut future = Box::pin(Arc::clone(notify).notified_owned());
                        future.as_mut().enable();
                        Some(Ok(future))
                    }
                    None => {
                        let notify = Arc::new(Notify::new());
                        self.creating_notify
                            .lock()
                            .await
                            .insert(parent.to_string(), Arc::clone(&notify));
                        parents.insert(
                            parent.to_string(),
                            ParentSlot::Creating(Arc::clone(&notify)),
                        );
                        None
                    }
                }
            };
            match wait {
                Some(Err(side_id)) => return self.summary_of(&side_id).await,
                // Someone else is creating: wait for them rather than racing.
                Some(Ok(future)) => future.await,
                None => return self.create(parent).await,
            }
        }
    }

    /// The body of `open` for the caller that won the creation race.
    async fn create(&self, parent: &str) -> Result<SideSummary, SideError> {
        let outcome = self.create_inner(parent).await;
        {
            let mut parents = self.parents.lock().await;
            match &outcome {
                Ok(summary) => {
                    parents.insert(
                        parent.to_string(),
                        ParentSlot::Ready(summary.side_id.clone()),
                    );
                }
                // A failed create must not leave a placeholder behind: that would
                // make the parent permanently un-openable.
                Err(_) => {
                    parents.remove(parent);
                }
            }
        }
        // Always wake the waiters, success or failure. On success they re-read the
        // map and find `Ready`; on failure they find no slot and become the next
        // creator.
        self.wake_creating(parent).await;
        outcome
    }

    /// Wake waiters registered on a parent's placeholder.
    ///
    /// The placeholder is replaced by `Ready` on success, so the only handle to its
    /// notify is the clone every waiter took. Keeping the notify in the entry would
    /// mean holding it for the side's whole life for one wake-up; instead the
    /// creator notifies through the same `Arc` it created. That is why the
    /// placeholder stores the `Arc` and the creator keeps its own clone.
    async fn wake_creating(&self, parent: &str) {
        let notify = self.creating_notify.lock().await.remove(parent);
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    async fn create_inner(&self, parent: &str) -> Result<SideSummary, SideError> {
        let source = self.resolver.resolve(parent).await?;
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let side_id = uuid::Uuid::new_v4().to_string();

        // The slot is reserved before any file work so a burst of opens cannot
        // exceed the host's allowance while the copies are still in flight.
        let lease = self.resources.reserve_slot(&side_id)?;
        let cancel = CancellationToken::new();
        let prepared = self
            .resources
            .prepare(Arc::clone(&lease), &source, cancel.clone())
            .await?;

        let tools = SideToolSetupFactory::new(
            source.clone(),
            Arc::clone(&prepared.access),
            Arc::clone(&self.pool),
            side_id.clone(),
            prepared.lease.clone() as Arc<dyn Send + Sync>,
        );
        // Captured before the source is moved into the runner: afterwards the fixed
        // values are only reachable through the runner, and the point of the snapshot
        // is that it never changes again.
        let config = SideEffectiveConfig {
            model: source.config.model.clone(),
            reasoning_effort: source.config.gen_params.reasoning_effort.clone(),
            preserve_thinking: source.config.preserve_thinking,
            tool_names: yi_agent_runtime::side_conversation::SideToolPolicy::default()
                .allowed_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        };
        let runner = SideConversationRunner::new(side_id.clone(), source, prepared, tools)?;
        let now = tokio::time::Instant::now();
        let entry = Arc::new(Mutex::new(SideEntry {
            side_id: side_id.clone(),
            parent: parent.to_string(),
            generation,
            runner,
            ledger: SideRequestLedger::new(),
            deadline: SideIdleDeadline::new(now),
            transcript: SideTranscript::side_defaults(),
            seq: AtomicU64::new(0),
            stopping: std::sync::atomic::AtomicBool::new(false),
            closing: false,
            config,
        }));
        {
            let mut sides = self.sides.lock().await;
            sides.insert(side_id.clone(), Arc::clone(&entry));
        }
        let guard = entry.lock().await;
        Ok(guard.summary())
    }

    /// Every live side for one parent (zero or one, but the wire shape is a list).
    pub async fn list(&self, parent: &str) -> Vec<SideSummary> {
        let side_ids: Vec<String> = {
            let parents = self.parents.lock().await;
            match parents.get(parent) {
                Some(ParentSlot::Ready(side_id)) => vec![side_id.clone()],
                _ => Vec::new(),
            }
        };
        let mut out = Vec::new();
        for side_id in side_ids {
            if let Ok(summary) = self.summary_of(&side_id).await {
                out.push(summary);
            }
        }
        out
    }

    async fn summary_of(&self, side_id: &str) -> Result<SideSummary, SideError> {
        let entry = self.entry(side_id).await?;
        let guard = entry.lock().await;
        Ok(guard.summary())
    }

    async fn entry(&self, side_id: &str) -> Result<Arc<Mutex<SideEntry>>, SideError> {
        let sides = self.sides.lock().await;
        sides.get(side_id).cloned().ok_or_else(|| {
            SideError::new(
                SideErrorReason::SideNotFound,
                format!("找不到旁支 {side_id}"),
            )
        })
    }

    /// Accept one turn, or return the turn a retried request already became.
    pub async fn turn_start(
        &self,
        side_id: &str,
        text: String,
        request_id: String,
    ) -> Result<String, SideError> {
        let entry = self.entry(side_id).await?;
        let mut guard = entry.lock().await;
        if guard.closing || guard.runner.status() == SideStatus::Closing {
            return Err(SideError::new(
                SideErrorReason::SideNotFound,
                format!("旁支 {side_id} 已关闭"),
            ));
        }

        // Dedup first: a retry of an accepted request must return the original turn
        // even though that turn made the side busy.
        if let Some(turn_id) = guard.ledger.lookup(&request_id, &text)? {
            return Ok(turn_id);
        }
        if guard.runner.active_turn_id().is_some() {
            return Err(SideError::new(
                SideErrorReason::TurnInProgress,
                "旁支已有一轮在跑",
            ));
        }

        let turn_id = uuid::Uuid::new_v4().to_string();
        // Record the acceptance before starting: a crash mid-start must leave a
        // dedup entry, not an accepted-but-unrecorded request.
        guard.ledger.accept(request_id, &text, turn_id.clone())?;

        let stream = match guard.runner.start_turn(turn_id.clone(), text).await {
            Ok(stream) => stream,
            Err(error) => {
                // The turn never started, so it must not stay in the ledger as if it
                // had: a retry would then report a turn id that does not exist.
                return Err(error);
            }
        };
        guard.seq.fetch_add(1, Ordering::SeqCst);
        guard.deadline.touch(tokio::time::Instant::now());

        // Drain the stream off the request path. The pump already joined the core
        // task before it publishes a terminal event, so this task only translates.
        let entry_for_task = Arc::clone(&entry);
        let side_id_for_task = guard.side_id.clone();
        let turn_for_task = turn_id.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                let mut guard = entry_for_task.lock().await;
                let _ = guard.transcript.apply(&turn_for_task, event.event.event());
                guard.seq.fetch_add(1, Ordering::SeqCst);
            }
            let mut guard = entry_for_task.lock().await;
            // The stream ended, but `active` is still set: only `wait_quiescent`
            // proves the pump joined the core task and publishes Idle. Without this
            // the side would look busy forever and every later turn would be refused
            // with `turn_in_progress` even though nothing was running.
            let _ = guard.runner.wait_quiescent().await;
            guard.stopping.store(false, Ordering::SeqCst);
            guard.deadline.touch(tokio::time::Instant::now());
            guard.seq.fetch_add(1, Ordering::SeqCst);
            let _ = side_id_for_task;
        });
        Ok(turn_id)
    }

    /// Ask the named turn to stop. A stale turn id is a no-op, never a way to kill
    /// a newer turn.
    pub async fn interrupt(&self, side_id: &str, turn_id: &str) -> Result<(), SideError> {
        let entry = self.entry(side_id).await?;
        let mut guard = entry.lock().await;
        // The runner itself matches the turn id, but setting `stopping` here means a
        // caller sees the accepted stop immediately, before the join completes.
        if guard.runner.active_turn_id() == Some(turn_id) {
            guard.stopping.store(true, Ordering::SeqCst);
        }
        guard.runner.interrupt(turn_id).await?;
        guard.seq.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Close the side, conditionally on the turn the caller believed was active.
    pub async fn close(&self, side_id: &str, expected: Option<&str>) -> Result<(), SideError> {
        let entry = match self.entry(side_id).await {
            Ok(entry) => entry,
            Err(_) => {
                // A repeated close against a tombstone is idempotent (spec §8.1).
                let tombstones = self.tombstones.lock().await;
                return if tombstones.contains_key(side_id) {
                    Ok(())
                } else {
                    Err(SideError::new(
                        SideErrorReason::SideNotFound,
                        format!("找不到旁支 {side_id}"),
                    ))
                };
            }
        };
        let (parent, generation, accepted) = {
            let mut guard = entry.lock().await;
            let actual = guard.runner.active_turn_id().map(str::to_string);
            // CAS and the transition into Closing happen in one critical section, so
            // a turn accepted concurrently cannot slip in between them.
            if actual.as_deref() != expected {
                let summary = guard.summary();
                return Err(SideError::new(
                    SideErrorReason::CloseConflict,
                    format!(
                        "旁支状态已变化：当前活跃轮为 {:?}，状态 {:?}",
                        summary.active_turn_id, summary.status
                    ),
                ));
            }
            guard.closing = true;
            let accepted: Vec<(String, String)> =
                guard.ledger.accepted_requests().into_iter().collect();
            (guard.parent.clone(), guard.generation, accepted)
        };

        {
            let mut guard = entry.lock().await;
            guard.runner.close().await?;
        }
        self.sides.lock().await.remove(side_id);
        {
            let mut parents = self.parents.lock().await;
            if let Some(ParentSlot::Ready(current)) = parents.get(&parent)
                && current == side_id
            {
                parents.remove(&parent);
            }
        }
        let mut tombstones = self.tombstones.lock().await;
        tombstones.insert(
            side_id.to_string(),
            Tombstone {
                side_id: side_id.to_string(),
                parent,
                generation,
                accepted,
                created_at: tokio::time::Instant::now(),
            },
        );
        Self::prune_tombstones(&mut tombstones, tokio::time::Instant::now());
        Ok(())
    }

    fn prune_tombstones(tombstones: &mut HashMap<String, Tombstone>, now: tokio::time::Instant) {
        tombstones.retain(|_, stone| now.duration_since(stone.created_at) < SIDE_TOMBSTONE_TTL);
        while tombstones.len() > SIDE_TOMBSTONES {
            let oldest = tombstones
                .iter()
                .min_by_key(|(_, stone)| stone.created_at)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    tombstones.remove(&id);
                }
                None => break,
            }
        }
    }

    /// What a tombstone remembers about a closed side.
    ///
    /// Used to answer "which parent/generation was this?" after the side itself is
    /// gone, which is what a reconnect needs to decide between "closed" and
    /// "never existed".
    pub async fn tombstone(&self, side_id: &str) -> Option<ClosedSide> {
        let tombstones = self.tombstones.lock().await;
        tombstones.get(side_id).map(|stone| ClosedSide {
            side_id: stone.side_id.clone(),
            parent_thread_id: stone.parent.clone(),
            generation: stone.generation,
            accepted: stone.accepted.clone(),
        })
    }

    /// The dedup table of a live or remembered side, for reconnect reconciliation.
    pub async fn accepted_requests(&self, side_id: &str) -> Option<Vec<(String, String)>> {
        if let Ok(entry) = self.entry(side_id).await {
            let guard = entry.lock().await;
            return Some(guard.ledger.accepted_requests().into_iter().collect());
        }
        let tombstones = self.tombstones.lock().await;
        tombstones.get(side_id).map(|stone| stone.accepted.clone())
    }

    /// A consistent snapshot of one side: items plus the metadata a client needs to
    /// reconcile.
    pub async fn snapshot(&self, side_id: &str) -> Result<SideSnapshot, SideError> {
        let entry = self.entry(side_id).await?;
        let guard = entry.lock().await;
        Ok(SideSnapshot {
            summary: guard.summary(),
            items: guard.transcript.items(),
            usage: guard.runner.usage(),
            accepted: guard.ledger.accepted_requests().into_iter().collect(),
            config: guard.config.clone(),
        })
    }

    /// Start a barrier around a destructive parent operation.
    pub async fn begin_parent_change(&self, parent: &str) -> ParentChangeGuard<'_> {
        let removed = {
            let mut parents = self.parents.lock().await;
            parents.remove(parent)
        };
        let side_id = match &removed {
            Some(ParentSlot::Ready(side_id)) => Some(side_id.clone()),
            _ => None,
        };
        ParentChangeGuard {
            registry: self,
            parent: parent.to_string(),
            slot: removed,
            side_id,
            committed: false,
        }
    }

    async fn apply_parent_change(&self, parent: &str, side_id: Option<&str>, reason: CloseReason) {
        let Some(side_id) = side_id else {
            return;
        };
        let Ok(entry) = self.entry(side_id).await else {
            return;
        };
        let (generation, accepted, side_parent) = {
            let mut guard = entry.lock().await;
            guard.closing = true;
            let accepted = guard.ledger.accepted_requests().into_iter().collect();
            (guard.generation, accepted, guard.parent.clone())
        };
        // The close is best effort here: the parent is already going away, and a
        // side that fails to converge must not keep the destructive operation
        // waiting. It still leaves a tombstone so a retry is answered honestly.
        {
            let mut guard = entry.lock().await;
            let _ = guard.runner.close().await;
        }
        let _ = reason;
        self.sides.lock().await.remove(side_id);
        let mut tombstones = self.tombstones.lock().await;
        tombstones.insert(
            side_id.to_string(),
            Tombstone {
                side_id: side_id.to_string(),
                parent: side_parent,
                generation,
                accepted,
                created_at: tokio::time::Instant::now(),
            },
        );
        Self::prune_tombstones(&mut tombstones, tokio::time::Instant::now());
        let _ = parent;
    }

    /// Stop every side and wait (bounded) for their resources.
    ///
    /// Never called from the request loop: the plan is explicit that shutdown must
    /// not block RPC handling on runner teardown.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let entries: Vec<(String, Arc<Mutex<SideEntry>>)> = {
            let sides = self.sides.lock().await;
            sides
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        for (side_id, entry) in &entries {
            let mut guard = entry.lock().await;
            guard.closing = true;
            let _ = guard.runner.close().await;
            let _ = side_id;
        }
        self.sides.lock().await.clear();
        self.parents.lock().await.clear();
    }

    pub async fn side_count(&self) -> usize {
        self.sides.lock().await.len()
    }

    pub async fn tombstone_count(&self) -> usize {
        self.tombstones.lock().await.len()
    }
}

/// A closed side, as remembered in the tombstone table.
#[derive(Debug, Clone)]
pub struct ClosedSide {
    pub side_id: String,
    pub parent_thread_id: String,
    pub generation: u64,
    pub accepted: Vec<(String, String)>,
}

/// A consistent read of one side.
pub struct SideSnapshot {
    pub summary: SideSummary,
    pub items: Vec<Arc<yi_agent_runtime::side_conversation::SideItem>>,
    pub usage: SideUsage,
    pub accepted: Vec<(String, String)>,
    pub config: SideEffectiveConfig,
}

/// The side's configuration as fixed **at creation**, in redacted form.
///
/// Deliberately not read from the parent when a client asks: a side inherits its
/// parent once, at open, and does not follow later edits. Re-reading `config/read`
/// would answer a different question than "what is this side running with".
///
/// Holds no secret: no API key, no headers, no system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectiveConfig {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub preserve_thinking: bool,
    pub tool_names: Vec<String>,
}

/// A barrier around a destructive parent operation.
///
/// Created before the destructive work, committed after it succeeded. Dropping it
/// uncommitted restores the parent's slot, so a failed clear does not leave the
/// parent permanently unable to open a side.
pub struct ParentChangeGuard<'a> {
    registry: &'a SideRegistry,
    parent: String,
    slot: Option<ParentSlot>,
    side_id: Option<String>,
    committed: bool,
}

impl ParentChangeGuard<'_> {
    /// Finish the change: the old generation is invalidated and its side is gone.
    pub async fn commit(mut self, reason: CloseReason) {
        self.committed = true;
        let side_id = self.side_id.take();
        self.registry
            .apply_parent_change(&self.parent, side_id.as_deref(), reason)
            .await;
    }
}

impl Drop for ParentChangeGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Roll back synchronously: `Drop` cannot await, and the slot map is small
        // enough for `try_lock` to be the pragmatic choice. If it is contended the
        // eager path already restored nothing, so simply not restoring is safe —
        // the next open would create a fresh side over the untouched parent files.
        if let Some(slot) = self.slot.take()
            && let Ok(mut parents) = self.registry.parents.try_lock()
        {
            parents.insert(self.parent.clone(), slot);
        }
    }
}

fn epoch_millis(instant: tokio::time::Instant) -> i64 {
    // The deadline lives on the tokio clock so tests can advance it; converting to
    // wall-clock milliseconds for the wire is done relative to now, which keeps the
    // two clocks consistent under `start_paused`.
    let now = tokio::time::Instant::now();
    let remaining = instant.saturating_duration_since(now);
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (wall + remaining).as_millis() as i64
}

#[cfg(test)]
mod tests;
