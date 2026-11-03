//! `ProcessWorkerFactory`: the daemon's `AgentWorkerFactory` backed by a child
//! process per client.
//!
//! Everything the daemon needs from a worker -- a `WorkerHandle` it can cancel,
//! message, pause, and read events from -- is reconstructed here from the
//! executor's wire events. The handle is a *local* object; only the work is
//! remote.
//!
//! Three invariants are worth stating because each one is easy to get wrong:
//!
//! 1. **Signals have to cross.** A `WorkerHandle` is not an RPC stub: cancelling it
//!    cancels a local token that the remote worker never reads. So every signal the
//!    handle can emit is mirrored onto the wire here; without the mirror a stop
//!    would be recorded locally and the remote worker would keep running.
//! 2. **A queued gate never blocks a cancel.** Gate questions are answered off
//!    the read loop, because acquiring a real permit can wait for the whole
//!    machine-wide limit while the read loop is the only path for Cancel and
//!    `Stopped`.
//! 3. **`Completed` is not `Stopped`.** The stop proof is satisfied only by the
//!    executor's `Stopped`, which its own observer emits after the worker's join.
//!    A channel that simply closes proves nothing and deliberately leaves the
//!    proof unsatisfied, so the lease stays held.
//! 4. **One reader per executor.** Frames from one socket must stay in order, so
//!    exactly one thread reads it and routes by `(task, attempt)`.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use yi_agent_core::agent::ProviderTurnGate;
use yi_agent_core::subagent::task::{AttemptId, TaskId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, StopWitness, WorkerError, WorkerHandle, WorkerStart,
};

use super::protocol::{
    ExecutorEvent, ExecutorRequest, GateMessage, RootExecutionConfig, WorkerStartWire,
};
use super::proxy::{ExecutorProxy, FrameAction, classify};

/// How long the mirrored-signal thread waits between checks.
///
/// Short enough that a stop is noticed promptly, long enough to cost nothing. The
/// cancellation token also wakes it, so this is only the fallback rate.
const SIGNAL_POLL: Duration = Duration::from_millis(25);

/// One live executor connection and the work routed to it.
struct ClientExecutor {
    /// Synchronised internally: reads block, and a cancel must still get out.
    proxy: Arc<ExecutorProxy>,
    /// Attempts the reader routes frames to, keyed by `(task, attempt)`.
    sinks: Mutex<RoutedSinks>,
    /// Set once the channel is known dead. A dead executor is never handed out
    /// again: its `ExecutorProxy` would accept a Start and then silently drop it,
    /// so the client would wait forever on work nobody received.
    dead: AtomicBool,
    /// Set by the thread that starts the reader, so a second `attach` can never
    /// start a second reader on the same socket.
    reader_started: AtomicBool,
}

/// Per-attempt routing state, owned by the single reader thread.
struct AttemptSink {
    handle: WorkerHandle,
    witness: Option<StopWitness>,
    finished: Arc<AtomicBool>,
}

/// The mutable half of the factory.
///
/// Behind an `Arc` so a returned future can own it: `start` must hand out a
/// `'static` future, and the future still needs the executor map when it runs.
struct Shared {
    /// One executor per client. Consulted under the same lock that starts it, so
    /// two concurrent Starts for one client cannot create two executors.
    executors: Mutex<HashMap<String, Arc<ClientExecutor>>>,
    /// Opens (or reuses) the executor connection for one owner. In a managed
    /// runtime this spawns a sibling process behind the guardian's ready barrier;
    /// a test installs a socket pair instead.
    connector: Arc<dyn ExecutorConnector>,
}

/// A worker factory whose workers live in per-client executor processes.
pub struct ProcessWorkerFactory {
    shared: Arc<Shared>,
    generation: String,
    provider_profile: Option<String>,
    /// The root's effective configuration, captured once by the daemon.
    ///
    /// Passed on every Start rather than inherited from "the first root this
    /// executor saw": one executor serves several tasks for the same client, and a
    /// later task must not silently run with an earlier task's model or sandbox.
    root_execution: RootExecutionConfig,
    /// The generation barrier. `None` means an unmanaged runtime, where there is
    /// no guardian to ask and therefore nothing to fence against.
    admission: Option<Arc<dyn ExecutionAdmissionCheck>>,
}

/// Asks the guardian whether one execution may start.
///
/// Called *before* the Start is framed, so a refused admission means the executor
/// never sees the work at all. Kept as a trait because the guardian is not always
/// reachable -- an unmanaged runtime has none -- and a test supplies its own.
pub trait ExecutionAdmissionCheck: Send + Sync {
    /// `Ok(())` admits the execution; `Err` carries the refusal reason.
    fn admit(&self, generation: &str, owner: &str, task: &str, attempt: &str)
    -> Result<(), String>;
}

/// How the daemon obtains a connection to a client's executor.
///
/// An interface rather than a concrete spawn so the guardian-owned spawn and the
/// in-memory test harness share one factory implementation.
pub trait ExecutorConnector: Send + Sync {
    /// Establishes a connection for `owner`, or returns why it could not.
    fn connect(&self, owner: &str) -> io::Result<ExecutorProxy>;
}

impl ProcessWorkerFactory {
    pub fn new(
        generation: impl Into<String>,
        connector: Arc<dyn ExecutorConnector>,
        provider_profile: Option<String>,
        root_execution: RootExecutionConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                executors: Mutex::new(HashMap::new()),
                connector,
            }),
            generation: generation.into(),
            provider_profile,
            root_execution,
            admission: None,
        }
    }

    /// Installs the generation barrier.
    ///
    /// Without this, a Start would go straight to the executor -- and an executor
    /// channel that already exists is exactly what a released client's stale work
    /// would use to bypass the barrier. So this is set whenever a guardian
    /// supervises the runtime.
    pub fn with_admission(mut self, admission: Arc<dyn ExecutionAdmissionCheck>) -> Self {
        self.admission = Some(admission);
        self
    }
}

impl Shared {
    /// The executor serving `owner`, starting it on first use.
    ///
    /// Lazily created on purpose: a client that never spawns a subagent pays for no
    /// process at all.
    fn executor_for(&self, owner: &str) -> Result<Arc<ClientExecutor>, String> {
        let mut executors = self
            .executors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = executors.get(owner) {
            if !existing.dead.load(Ordering::SeqCst) {
                return Ok(Arc::clone(existing));
            }
            // Its channel is gone; replacing it is the only way the client can
            // make progress. Its permits were already released when it died.
            executors.remove(owner);
        }
        let proxy = self
            .connector
            .connect(owner)
            .map_err(|error| format!("cannot start an executor for {owner}: {error}"))?;
        let executor = Arc::new(ClientExecutor {
            proxy: Arc::new(proxy),
            sinks: Mutex::new(RoutedSinks::default()),
            dead: AtomicBool::new(false),
            reader_started: AtomicBool::new(false),
        });
        executors.insert(owner.to_string(), Arc::clone(&executor));
        Ok(executor)
    }
}

impl AgentWorkerFactory for ProcessWorkerFactory {
    fn provider_profile_id(&self) -> Option<String> {
        self.provider_profile.clone()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.start_with_provider_turn_gate(request, None)
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let owner = request.root_session_id.to_string();
        let generation = self.generation.clone();
        let command_id = super::serve::next_command_id();
        // Copied before `WorkerStartWire::from_start` borrows the request.
        let task_id = request.task_id.clone();
        let attempt_id = request.attempt_id.clone();
        let cancellation = request.cancellation.clone();
        let wire = WorkerStartWire::from_start(&request, self.root_execution.clone());

        // Asked synchronously, before the future exists and before any executor is
        // started: a refused client must not pay for a process it will never be
        // allowed to use. The call is bounded by its own 2-second budget, so it
        // cannot turn `start` into a long blocking call.
        if let Some(admission) = &self.admission
            && let Err(reason) = admission.admit(
                &generation,
                &owner,
                &task_id.to_string(),
                &attempt_id.to_string(),
            )
        {
            let message = format!("the guardian refused this execution: {reason}");
            return Box::pin(async move { Err(WorkerError::Startup(message)) });
        }

        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            let executor = match shared.executor_for(&owner) {
                Ok(executor) => executor,
                Err(reason) => return Err(WorkerError::Startup(reason)),
            };
            if executor.dead.load(Ordering::SeqCst) {
                return Err(WorkerError::Startup(
                    "the executor for this client has died".into(),
                ));
            }
            let sent = executor
                .proxy
                .send_start(&command_id, &generation, &owner, wire);
            if let Err(error) = sent {
                return Err(WorkerError::Startup(error.to_string()));
            }

            let (stop_witness, confirmation) = StopWitness::pair();
            let handle = WorkerHandle::new(cancellation).with_confirmation(confirmation);
            let finished = Arc::new(AtomicBool::new(false));

            let sink = AttemptSink {
                handle: handle.clone(),
                witness: Some(stop_witness),
                finished: Arc::clone(&finished),
            };

            // The reader is shared per executor: one thread per socket keeps frame
            // order, and a newcomer registers its attempt with that thread.
            Reader::attach(
                Arc::clone(&executor),
                gate,
                task_id.to_string(),
                attempt_id.to_string(),
                sink,
            );

            // Signals go the other way. Without this thread a cancel would be a
            // local flag the remote worker never reads.
            SignalMirror::spawn(
                Arc::clone(&executor),
                handle.cancellation_token(),
                handle.subscribe_pause(),
                handle.subscribe_messages(),
                task_id,
                attempt_id,
                finished,
            );
            Ok(handle)
        })
    }
}

/// The single frame reader for one executor.
///
/// Registration happens on first use and the thread lives with the executor: a
/// client's second task must not open a second reader, because two readers on one
/// socket would interleave frames and each would see the other's attempts.
struct Reader;

impl Reader {
    fn attach(
        executor: Arc<ClientExecutor>,
        gate: Option<Arc<dyn ProviderTurnGate>>,
        task: String,
        attempt: String,
        sink: AttemptSink,
    ) {
        let key = (task, attempt);
        {
            let mut shared = executor
                .sinks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            shared.sinks.insert(key, sink);
        }
        // Tested and set under no lock race: only the first attach may own the
        // read loop. Checking "are there sinks" instead would start a second
        // reader whenever the last attempt finished while the old reader was
        // still parked in `read_frame`, and two readers on one socket interleave
        // frames.
        if executor.reader_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let reader = Arc::clone(&executor);
        std::thread::spawn(move || reader.run(gate));
    }
}

/// Routing state shared between the reader thread and every signal mirror.
#[derive(Default)]
struct RoutedSinks {
    sinks: HashMap<(String, String), AttemptSink>,
}

impl ClientExecutor {
    fn run(self: Arc<Self>, gate: Option<Arc<dyn ProviderTurnGate>>) {
        loop {
            let frame = self.proxy.receive();
            let frame = match frame {
                Ok(frame) => frame,
                // The executor is gone. Deliberately no stop proof: a closed
                // channel does not prove the worker's tools stopped. The
                // guardian's owner reclamation is what confirms that.
                Err(_) => {
                    self.mark_dead();
                    return;
                }
            };
            match classify(frame) {
                FrameAction::Gate(request) => {
                    // Never answer here. Acquiring a real permit can wait as long
                    // as the machine-wide limit is full, and the read loop is the
                    // only path for Cancel and Stopped: blocking it would make a
                    // queued turn request wedge every attempt sharing this
                    // executor.
                    if let Some(gate) = &gate {
                        let waiter = request.waiter();
                        let proxy = Arc::clone(&self.proxy);
                        let gate = Arc::clone(gate);
                        let executor = Arc::clone(&self);
                        // A dedicated thread, not a shared worker pool: an
                        // acquire may block for a long time by design, and a
                        // pool would let one queued turn starve the others.
                        std::thread::spawn(move || {
                            let live = || match &waiter {
                                Some((task, attempt)) => executor.attempt_is_live(task, attempt),
                                // Without an attempt identity there is nothing
                                // to drop the grant for; answer normally.
                                None => true,
                            };
                            let _ = proxy.answer_gate_for(request, gate.as_ref(), &live);
                        });
                    }
                }
                FrameAction::Event(event) => self.route(event),
            }
        }
    }

    /// Whether `(task, attempt)` is still registered and unfinished.
    ///
    /// Asked after a permit is acquired so a grant for an attempt that stopped
    /// while queued is released instead of being held.
    fn attempt_is_live(&self, task: &str, attempt: &str) -> bool {
        let sinks = self
            .sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match sinks.sinks.get(&(task.to_string(), attempt.to_string())) {
            Some(sink) => !sink.finished.load(Ordering::SeqCst),
            None => false,
        }
    }

    /// Records that the channel is gone and fails everything waiting on it.
    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
        // A dead channel can never carry an answer. Without this, an executor
        // whose daemon vanished would sit on `gate.acquire()` forever.
        let sinks = self
            .sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for sink in sinks.sinks.values() {
            sink.finished.store(true, Ordering::SeqCst);
        }
    }

    fn route(&self, event: ExecutorEvent) {
        let key = match &event {
            ExecutorEvent::Worker { task, attempt, .. }
            | ExecutorEvent::Watchdog { task, attempt, .. }
            | ExecutorEvent::Trace { task, attempt, .. }
            | ExecutorEvent::Stopped { task, attempt } => Some((task.clone(), attempt.clone())),
            ExecutorEvent::Rejected { .. } => None,
        };
        let mut sinks = self
            .sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(key) = key
            && let Some(sink) = sinks.sinks.get_mut(&key)
        {
            apply_event(sink, event);
            // A finished attempt is removed so its handle is not written to
            // after the coordinator has moved on.
            if sink.finished.load(Ordering::SeqCst) {
                sinks.sinks.remove(&key);
            }
        }
        // A rejection with no attempt identity can only mean the executor refused
        // before a worker existed; the factory's `send_start` already surfaced the
        // transport error, so there is no handle to tell.
    }
}

fn apply_event(sink: &mut AttemptSink, event: ExecutorEvent) {
    match event {
        ExecutorEvent::Worker { payload, .. } => {
            if let Ok(event) = payload.to_event() {
                sink.handle.report_transported_event(event);
            }
        }
        ExecutorEvent::Watchdog { payload, .. } => {
            sink.handle.report_transported_watchdog(payload.to_event());
        }
        ExecutorEvent::Trace { payload, .. } => {
            sink.handle.report_trace(payload);
        }
        ExecutorEvent::Stopped { .. } => {
            // Only now is the execution really over.
            if let Some(witness) = sink.witness.take() {
                witness.mark_joined();
            }
            sink.finished.store(true, Ordering::SeqCst);
        }
        ExecutorEvent::Rejected { reason, .. } => {
            sink.handle
                .report_failure(format!("executor refused the start: {reason}"));
            // A refused start never ran, so its execution is trivially over.
            if let Some(witness) = sink.witness.take() {
                witness.mark_joined();
            }
            sink.finished.store(true, Ordering::SeqCst);
        }
    }
}

/// Mirrors a handle's local signals onto the wire.
struct SignalMirror;

impl SignalMirror {
    fn spawn(
        executor: Arc<ClientExecutor>,
        cancellation: CancellationToken,
        pause: yi_agent_core::subagent::worker::WorkerPauseSignal,
        mut messages: yi_agent_core::subagent::worker::WorkerMailbox,
        task: TaskId,
        attempt: AttemptId,
        finished: Arc<AtomicBool>,
    ) {
        let task = task.to_string();
        let attempt = attempt.to_string();

        // Cancel: one frame, then done. Cancellation is not revocable, so polling
        // for the token once is enough -- the executor's token is idempotent
        // anyway, but re-sending every tick would be pure noise on the wire.
        {
            let proxy = Arc::clone(&executor.proxy);
            let cancellation = cancellation.clone();
            let task = task.clone();
            let attempt = attempt.clone();
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                loop {
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    if cancellation.is_cancelled() {
                        let _ = proxy.send(&ExecutorRequest::Cancel {
                            task: task.clone(),
                            attempt: attempt.clone(),
                        });
                        return;
                    }
                    std::thread::sleep(SIGNAL_POLL);
                }
            });
        }

        // Pause: also one-shot. The core signal latches `true`, so waiting for it
        // to clear would fire this frame on every tick forever.
        {
            let proxy = Arc::clone(&executor.proxy);
            let task = task.clone();
            let attempt = attempt.clone();
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                let mut pause = pause;
                if !futures::executor::block_on(pause.requested()) {
                    return;
                }
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                let _ = proxy.send(&ExecutorRequest::Pause { task, attempt });
            });
        }

        // Messages: awaits, because a blocked mailbox is exactly the state that
        // should park this thread rather than spin it.
        {
            let proxy = Arc::clone(&executor.proxy);
            std::thread::spawn(move || {
                loop {
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    let Some(message) = futures::executor::block_on(messages.recv()) else {
                        return;
                    };
                    if proxy
                        .send(&ExecutorRequest::Message {
                            task: task.clone(),
                            attempt: attempt.clone(),
                            id: message.id.to_string(),
                            body: message.body,
                        })
                        .is_err()
                    {
                        // The channel is gone; retrying cannot help and would spin.
                        return;
                    }
                }
            });
        }
    }
}

/// Answers a gate request using the daemon's real gate.
///
/// Exposed for a caller that owns its own read loop instead of using
/// [`ClientExecutor`].
pub fn answer_gate(
    proxy: &ExecutorProxy,
    message: GateMessage,
    gate: &dyn ProviderTurnGate,
) -> io::Result<()> {
    proxy.answer_gate(message, gate)
}
