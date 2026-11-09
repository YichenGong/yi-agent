//! The owner of one `/side` generation.
//!
//! # Why an owner object exists
//!
//! A side has three lifetimes that used to be confused with one another:
//!
//! * **The environment** — a resource slot, a runner, its transcript. It must be
//!   gone the moment the user leaves, not when the model happens to finish.
//! * **The authorisation** — the entry ticket. It must be revocable exactly once
//!   and must keep pointing at *this* environment, never at a later one.
//! * **The clients** — several transports may watch one generation, so closing is
//!   a single decision many callers observe, not a request each of them makes.
//!
//! [`GenerationOwner`] is the object all three refer to. It is created before any
//! expensive work, so a `Creating` generation can already be cancelled, and it
//! outlives the runner, so a failed cleanup still has an owner to retry against.
//!
//! # Why the phase is a watch channel
//!
//! Callers wait for a generation to become usable or to disappear. With a bare
//! `Notify`, a waiter that subscribes after the transition sleeps until the next
//! one — the classic lost wake-up that turns "open waits for creating" into a
//! hang. A `watch` channel stores the current phase, so a late waiter reads it
//! instead of missing it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use yi_agent_runtime::side_conversation::SideError;

use super::registry::SideEntry;
use super::tickets::SideGeneration;

/// How long a generation may take to stop its runner and prove the join.
///
/// Matches the runtime's own `SIDE_STOP_GRACE`: the pump already bounds a stop at
/// five seconds, so a second, shorter budget here would report a failure the
/// runtime was still within its rights to complete.
pub(crate) const SIDE_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long an in-flight construction may take to observe a cancellation.
///
/// A construction that is blocked on reading the parent is not aborted: aborting
/// would drop the work mid-flight and leave the resource slot's cleanup racing the
/// reader. It is cancelled instead, and this is how long the host waits for it to
/// notice before reporting `failed` and keeping the owner for a retry.
pub(crate) const SIDE_CONSTRUCTION_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the terminal notification may block an otherwise finished cleanup.
///
/// The resources are already reclaimed when this is spent; a client that missed
/// the frame reconciles through `side/exit` or `side/list`. Notifications are
/// never allowed to hold a destroyed environment open.
pub(crate) const SIDE_CLOSED_NOTIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// Where one generation is in its life.
///
/// There is deliberately no `Hidden` variant: a side cannot outlive the moment the
/// user left it, and a state that says "still running but invisible" is exactly
/// what this feature removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The environment is being built. It is cancellable and invisible.
    Creating,
    /// The environment exists and may accept turns.
    Open,
    /// Exit was accepted: no new turns, no body reads, cleanup in flight.
    Closing,
    /// The environment is gone and its resources were confirmed released.
    Closed,
    /// Cleanup could not prove the release. The owner is kept so a retry drains
    /// the *same* generation instead of abandoning it.
    Failed,
}

/// One generation's identity, cancellation and cleanup evidence.
pub(crate) struct GenerationOwner {
    side: SideGeneration,
    parent: String,
    cancel: CancellationToken,
    phase: watch::Sender<Phase>,
    /// Present from the moment the environment is installed until cleanup takes
    /// it. `Mutex` because installing happens once, under the registry's state
    /// lock, and taking happens once, from the drain task.
    entry: Mutex<Option<Arc<Mutex<SideEntry>>>>,
    /// The construction task, so a drain can wait for it instead of racing it.
    construction: Mutex<Option<JoinHandle<()>>>,
    /// Forwarders (one per accepted turn) that still hold the entry alive.
    publishers: Mutex<Vec<JoinHandle<()>>>,
    /// Set while one drain task owns this generation's cleanup, so a retried exit
    /// observes `Closing` instead of starting a second drain.
    draining: AtomicBool,
    /// Set by whichever caller wins the right to spawn the construction task.
    ///
    /// A plain `take`/`is_some` check would let two concurrent opens both see "no
    /// handle yet" and build two environments for one generation.
    construction_started: AtomicBool,
    /// Why construction or cleanup failed, for the caller waiting on it.
    error: std::sync::Mutex<Option<SideError>>,
}

impl GenerationOwner {
    pub(crate) fn new(
        side_id: impl Into<String>,
        generation: u64,
        parent: impl Into<String>,
    ) -> Arc<Self> {
        let (phase, _) = watch::channel(Phase::Creating);
        Arc::new(Self {
            side: SideGeneration::new(side_id, generation),
            parent: parent.into(),
            cancel: CancellationToken::new(),
            phase,
            entry: Mutex::new(None),
            construction: Mutex::new(None),
            publishers: Mutex::new(Vec::new()),
            draining: AtomicBool::new(false),
            construction_started: AtomicBool::new(false),
            error: std::sync::Mutex::new(None),
        })
    }

    pub(crate) fn side(&self) -> &SideGeneration {
        &self.side
    }

    pub(crate) fn parent(&self) -> &str {
        &self.parent
    }

    pub(crate) fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub(crate) fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    /// A receiver that starts at the current phase.
    pub(crate) fn watch(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    pub(crate) fn set_phase(&self, next: Phase) {
        // `send_replace` never fails: the sender lives as long as the owner, and a
        // generation that already finished must still be readable by a late
        // observer (a retried exit asks "did it close?" and must get `Closed`).
        self.phase.send_replace(next);
    }

    /// Move into `Closing` unless the generation already finished.
    ///
    /// `Failed` is deliberately included: a retried exit is the documented way to
    /// resume a cleanup that could not prove its release.
    pub(crate) fn begin_closing(&self) {
        match self.phase() {
            Phase::Creating | Phase::Open | Phase::Failed => self.set_phase(Phase::Closing),
            Phase::Closing | Phase::Closed => {}
        }
    }

    /// Move `Creating` to `Open` only if nobody has asked to close meanwhile.
    ///
    /// A construction that finishes after an exit must not resurrect the side by
    /// publishing `Open` over `Closing`; the flip is a compare-and-set for exactly
    /// that race.
    pub(crate) fn try_open(&self) -> bool {
        let mut opened = false;
        self.phase.send_if_modified(|phase| {
            if *phase == Phase::Creating {
                *phase = Phase::Open;
                opened = true;
                true
            } else {
                false
            }
        });
        opened
    }

    pub(crate) fn is_closing(&self) -> bool {
        !matches!(self.phase(), Phase::Creating | Phase::Open)
    }

    pub(crate) async fn entry(&self) -> Option<Arc<Mutex<SideEntry>>> {
        self.entry.lock().await.clone()
    }

    /// Install the environment only while the generation is still `Creating`.
    ///
    /// The check and the store happen under the entry mutex, and `begin_closing`
    /// sets the phase *before* a drain touches that mutex. So either:
    ///
    /// * construction wins — the drain finds the entry and reclaims it, or
    /// * closing wins — construction refuses and drops the entry itself.
    ///
    /// There is no third interleaving in which a built environment is stored after
    /// the drain already looked, which would leak its resource lease forever.
    pub(crate) async fn install_entry_if_creating(&self, entry: Arc<Mutex<SideEntry>>) -> bool {
        let mut slot = self.entry.lock().await;
        if *self.phase.borrow() != Phase::Creating || self.cancel.is_cancelled() {
            return false;
        }
        *slot = Some(entry);
        true
    }

    pub(crate) async fn take_entry(&self) -> Option<Arc<Mutex<SideEntry>>> {
        self.entry.lock().await.take()
    }

    pub(crate) async fn set_construction(&self, handle: JoinHandle<()>) {
        *self.construction.lock().await = Some(handle);
    }

    pub(crate) async fn take_construction(&self) -> Option<JoinHandle<()>> {
        self.construction.lock().await.take()
    }

    pub(crate) async fn push_publisher(&self, handle: JoinHandle<()>) {
        self.publishers.lock().await.push(handle);
    }

    pub(crate) async fn take_publishers(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.publishers.lock().await)
    }

    /// Claim the right to run this generation's drain, once.
    pub(crate) fn begin_drain(&self) -> bool {
        !self.draining.swap(true, Ordering::SeqCst)
    }

    /// Claim the right to spawn this generation's construction task.
    ///
    /// Returns `true` exactly once per generation: two concurrent opens on one
    /// ticket must not build two environments.
    pub(crate) fn claim_construction(&self) -> bool {
        !self.construction_started.swap(true, Ordering::SeqCst)
    }

    pub(crate) fn end_drain(&self) {
        self.draining.store(false, Ordering::SeqCst);
    }

    pub(crate) fn set_error(&self, error: SideError) {
        *self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
    }

    pub(crate) fn error(&self) -> Option<SideError> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl std::fmt::Debug for GenerationOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Identity and phase only: never the parent's content, never a ticket.
        f.debug_struct("GenerationOwner")
            .field("generation", &self.side.generation)
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}
