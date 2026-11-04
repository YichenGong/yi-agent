//! A bounded executor for side-conversation blocking work.
//!
//! Document parsing and image decoding are CPU-bound and, in the worst case,
//! cannot be interrupted. Running them on `tokio`'s shared `spawn_blocking` pool
//! would let one side occupy the host's runtime and would make host shutdown
//! wait on work that never returns. So this pool owns **its own dedicated
//! threads**:
//!
//! * the thread count is capped by the host limit and never grows with the
//!   number of sides;
//! * admission is bounded per host *and* per side, so one side cannot consume
//!   every slot;
//! * work never starts once its cancellation token fires;
//! * a waiter that cancels stops waiting, but the slot and the `owner` stay held
//!   until the job really finishes — dropping a handle is not evidence that the
//!   work stopped (spec §7.3).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Cancelled before or while waiting for capacity.
fn interrupted() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "side blocking work was cancelled",
    )
}

/// The pool refused the job because it was shut down.
fn shutdown() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the side blocking pool has shut down",
    )
}

/// One queued unit of work.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Counters describing what the pool is doing, for the resource owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideBlockingStats {
    /// Jobs currently executing.
    pub running: usize,
    /// Jobs admitted but still queued.
    pub queued: usize,
    /// Threads currently alive.
    pub threads: usize,
}

/// A pool of dedicated worker threads shared by every side of one host.
pub struct SideBlockingPool {
    /// Admission per host: `host_limit` running + `host_queue` waiting.
    host_admission: Arc<Semaphore>,
    /// Admission per side: `per_side_limit` running + `per_side_queue` waiting.
    side_admission: Mutex<HashMap<String, Arc<Semaphore>>>,
    per_side_capacity: usize,
    shared: Arc<Shared>,
}

/// Thread pool state.
struct Shared {
    inner: Mutex<Inner>,
    /// A worker should pick up queued work.
    work_ready: Condvar,
    /// Capacity changed (a job finished); `shutdown` waits here.
    finished: Condvar,
    /// Threads never exceed this.
    host_limit: usize,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Job>,
    running: usize,
    threads: usize,
    shutdown: bool,
}

impl SideBlockingPool {
    /// `host_limit` threads, `per_side_limit` concurrent jobs per side, and the
    /// matching queue sizes. Threads are created on demand up to `host_limit` and
    /// never per side.
    pub fn new(
        host_limit: usize,
        per_side_limit: usize,
        host_queue: usize,
        per_side_queue: usize,
    ) -> Self {
        let host_limit = host_limit.max(1);
        let per_side_limit = per_side_limit.max(1);
        Self {
            host_admission: Arc::new(Semaphore::new(host_limit + host_queue)),
            side_admission: Mutex::new(HashMap::new()),
            per_side_capacity: per_side_limit + per_side_queue,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                work_ready: Condvar::new(),
                finished: Condvar::new(),
                host_limit,
            }),
        }
    }

    /// Current usage snapshot.
    pub fn stats(&self) -> SideBlockingStats {
        let inner = self.shared.inner.lock().unwrap();
        SideBlockingStats {
            running: inner.running,
            queued: inner.queue.len(),
            threads: inner.threads,
        }
    }

    /// How many admissions one side may hold at once (running + waiting).
    pub fn per_side_capacity(&self) -> usize {
        self.per_side_capacity
    }

    /// Run `job` on a pool thread, holding `owner` until it really finishes.
    ///
    /// `owner` is moved into the job, so the resource it represents stays alive
    /// for as long as the blocking work does — even if the caller has already
    /// given up waiting.
    pub async fn run<T, F>(
        &self,
        side_id: &str,
        cancel: CancellationToken,
        owner: Arc<dyn Send + Sync>,
        job: F,
    ) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        if cancel.is_cancelled() {
            return Err(interrupted());
        }
        let side_lease = self.acquire_side(side_id, &cancel).await?;
        let host_lease = match self.acquire_host(&cancel).await {
            Ok(lease) => lease,
            Err(error) => return Err(error),
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let worker_cancel = cancel.clone();
        self.enqueue(Box::new(move || {
            // The permits and the owner live until this closure returns, so a
            // cancelled waiter cannot free capacity that is still in use.
            let _held = (side_lease, host_lease, owner);
            if worker_cancel.is_cancelled() {
                // Never start work that was cancelled while queued.
                let _ = reply_tx.send(Err(interrupted()));
                return;
            }
            let _ = reply_tx.send(job());
        }))?;

        tokio::select! {
            result = reply_rx => map_reply(result),
            // Cancelling only drops the receiver. The worker keeps its permits
            // and its owner until it returns; nothing here pretends the I/O
            // died.
            _ = cancel.cancelled() => Err(interrupted()),
        }
    }

    /// Submit `job` without waiting for it, holding `owner` until it finishes.
    ///
    /// Used for work that must not block its initiator — cleanup after the last
    /// owner drops. The job still consumes a host admission and runs on a pool
    /// thread, so cleanup can never outrun the host's blocking quota.
    ///
    /// Returns `false` when the pool is closed or admission is unavailable; the
    /// caller then leaves the work for startup reclamation instead of spinning.
    pub fn submit_detached(
        &self,
        owner: Arc<dyn Send + Sync>,
        job: impl FnOnce() + Send + 'static,
    ) -> bool {
        let Some(host_lease) = self.host_admission.clone().try_acquire_owned().ok() else {
            return false;
        };
        self.enqueue(Box::new(move || {
            let _held = (host_lease, owner);
            job();
        }))
        .is_ok()
    }

    /// Take one per-side admission, waiting cancellably.
    async fn acquire_side(
        &self,
        side_id: &str,
        cancel: &CancellationToken,
    ) -> io::Result<OwnedSemaphorePermit> {
        let semaphore = {
            let mut map = self.side_admission.lock().unwrap();
            Arc::clone(
                map.entry(side_id.to_string())
                    .or_insert_with(|| Arc::new(Semaphore::new(self.per_side_capacity))),
            )
        };
        tokio::select! {
            // Cancellation wins deterministically: a free permit and an
            // already-cancelled token are both ready, and an unbiased `select!`
            // would pick randomly.
            biased;
            _ = cancel.cancelled() => Err(interrupted()),
            permit = semaphore.acquire_owned() => permit.map_err(|_| shutdown()),
        }
    }

    /// Take one host admission, waiting cancellably.
    async fn acquire_host(&self, cancel: &CancellationToken) -> io::Result<OwnedSemaphorePermit> {
        let semaphore = Arc::clone(&self.host_admission);
        tokio::select! {
            // Same reasoning as `acquire_side`: cancellation is checked first.
            biased;
            _ = cancel.cancelled() => Err(interrupted()),
            permit = semaphore.acquire_owned() => permit.map_err(|_| shutdown()),
        }
    }

    /// Queue an admitted job, growing the thread pool up to the host cap.
    fn enqueue(&self, job: Job) -> io::Result<()> {
        let mut inner = self.shared.inner.lock().unwrap();
        if inner.shutdown {
            return Err(shutdown());
        }
        inner.queue.push_back(job);
        let wanted = (inner.queue.len() + inner.running).min(self.shared.host_limit);
        while inner.threads < wanted {
            inner.threads += 1;
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new()
                .name("yi-side-blocking".into())
                .spawn(move || worker_loop(shared));
            if spawned.is_err() {
                inner.threads -= 1;
                break;
            }
        }
        self.shared.work_ready.notify_one();
        Ok(())
    }

    /// Stop admitting work, drop everything still queued, and wait up to `grace`
    /// for jobs already running.
    ///
    /// Returns `true` when every running job finished within `grace`. A `false`
    /// means the pool still owns live work: the threads and their owners stay
    /// accounted for, because an unfinished `read` cannot be killed.
    pub fn shutdown(&self, grace: std::time::Duration) -> bool {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.shutdown = true;
            // Queued jobs never start; dropping each releases its permits and
            // its owner.
            let queued: Vec<Job> = inner.queue.drain(..).collect();
            drop(inner);
            drop(queued);
        }
        self.shared.work_ready.notify_all();
        let deadline = std::time::Instant::now() + grace;
        let mut inner = self.shared.inner.lock().unwrap();
        while inner.running > 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .shared
                .finished
                .wait_timeout(inner, deadline - now)
                .unwrap();
            inner = guard;
        }
        true
    }
}

impl Drop for SideBlockingPool {
    fn drop(&mut self) {
        // Never block a drop on uninterruptible work: stop admitting, drop the
        // queued jobs, and let running work finish on its own thread.
        let mut inner = self.shared.inner.lock().unwrap();
        inner.shutdown = true;
        inner.queue.clear();
        self.shared.work_ready.notify_all();
    }
}

/// Worker loop: take a job, run it, record the outcome.
fn worker_loop(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut inner = shared.inner.lock().unwrap();
            loop {
                if let Some(job) = inner.queue.pop_front() {
                    inner.running += 1;
                    break job;
                }
                if inner.shutdown {
                    inner.threads = inner.threads.saturating_sub(1);
                    shared.finished.notify_all();
                    return;
                }
                let (guard, _) = shared
                    .work_ready
                    .wait_timeout(inner, std::time::Duration::from_millis(50))
                    .unwrap();
                inner = guard;
            }
        };
        // A panicking job must not take the pool down: the reply channel simply
        // closes and the waiter sees a failure.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
            tracing::error!("side blocking job panicked");
        }
        let mut inner = shared.inner.lock().unwrap();
        inner.running = inner.running.saturating_sub(1);
        drop(inner);
        shared.finished.notify_all();
        shared.work_ready.notify_one();
    }
}

/// Turn a reply-channel failure into a plain I/O error.
fn map_reply<T>(
    result: Result<io::Result<T>, tokio::sync::oneshot::error::RecvError>,
) -> io::Result<T> {
    match result {
        Ok(inner) => inner,
        Err(_) => Err(io::Error::other(
            "side blocking worker failed to report a result",
        )),
    }
}

/// Live worker threads.
pub fn live_threads(pool: &SideBlockingPool) -> usize {
    pool.shared.inner.lock().unwrap().threads
}

/// Admission still available host-wide (permits not held).
pub fn free_host_admissions(pool: &SideBlockingPool) -> usize {
    pool.host_admission.available_permits()
}

/// Admission still available for one side.
pub fn free_side_admissions(pool: &SideBlockingPool, side_id: &str) -> usize {
    pool.side_admission
        .lock()
        .unwrap()
        .get(side_id)
        .map(|semaphore| semaphore.available_permits())
        .unwrap_or(pool.per_side_capacity)
}
