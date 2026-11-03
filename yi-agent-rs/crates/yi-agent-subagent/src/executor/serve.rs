//! The executor process: one client's workers, in a process the daemon can kill.
//!
//! The daemon never runs a worker thread itself. It sends `Start` to this process
//! over an inherited private descriptor, receives events back, and -- when a client
//! must be reclaimed -- terminates the process group. That last step is what a
//! thread-based worker cannot offer: a wedged worker that ignores its cancellation
//! token keeps its thread, its tool children, and its provider stream alive
//! forever.
//!
//! The descriptor is inherited, never a public socket. An executor that accepted
//! arbitrary "run this" commands from any local process would be a remote code
//! execution hole, so the entry point refuses to run without the private FD.

use std::collections::HashMap;
use std::io::{self, BufReader};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use yi_agent_core::agent::ProviderTurnGate;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerHandle, WorkerStart};

use super::gate::{GateRouter, GateWire, ProxyTurnGate};
use super::protocol::{
    DaemonFrame, ExecutorEvent, ExecutorFrame, ExecutorRequest, GateMessage, WorkerEventWire,
    WorkerWatchdogEventWire, read_frame, write_frame,
};

/// The environment variable naming the inherited control descriptor.
pub const EXECUTOR_FD_ENV: &str = "YI_AGENT_EXECUTOR_FD";

/// One running attempt inside this executor.
struct RunningAttempt {
    handle: WorkerHandle,
    cancellation: CancellationToken,
}

/// The write half of the control channel, shared by every reporting thread.
///
/// Shared rather than cloned per thread on purpose: two threads writing frames to
/// the same socket with independent handles interleave their bytes, and the daemon
/// then reads a length header followed by the wrong body. One lock makes each frame
/// atomic on the wire.
#[derive(Clone)]
struct EventWriter {
    stream: Arc<Mutex<UnixStream>>,
}

impl GateWire for EventWriter {
    fn send_gate(&self, message: GateMessage) -> io::Result<()> {
        self.send_gate_message(message)
    }
}

impl EventWriter {
    fn send(&self, event: ExecutorEvent) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *stream, &ExecutorFrame::Event(Box::new(event)))
    }

    /// Sends one gate message to the daemon.
    fn send_gate_message(&self, message: GateMessage) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *stream, &ExecutorFrame::Gate(message))
    }
}

/// State shared by the reader thread and every worker.
#[derive(Default)]
struct ExecutorState {
    attempts: Mutex<HashMap<(String, String), RunningAttempt>>,
    /// Gate answers the reader routes to their waiting attempt.
    gate: Arc<GateRouter>,
    /// Outcomes of `Start`s already applied, keyed by command id.
    ///
    /// A reconnect must not replay a Start whose effect already happened: the
    /// worker would be started twice and the client would pay for both.
    applied: Mutex<HashMap<String, Result<(), String>>>,
    stopping: AtomicBool,
}

impl ExecutorState {
    fn claim(&self, command_id: &str) -> Claim {
        let mut applied = self
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = applied.get(command_id) {
            return match previous {
                Ok(()) => Claim::AlreadyAppliedOk,
                Err(reason) => Claim::AlreadyAppliedErr(reason.clone()),
            };
        }
        // Reserve the id *before* doing the work, so two concurrent copies of the
        // same command cannot both run.
        applied.insert(command_id.to_string(), Err("in progress".into()));
        Claim::First
    }

    fn record(&self, command_id: &str, outcome: Result<(), String>) {
        self.applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(command_id.to_string(), outcome);
    }
}

enum Claim {
    First,
    AlreadyAppliedOk,
    AlreadyAppliedErr(String),
}

/// Serves one client's workers until the daemon closes the channel.
///
/// `factory` is built *locally* by the entry point from inherited configuration,
/// never from the control channel: the channel carries work, not authority to
/// reconstruct a runtime with different credentials.
///
/// `gate_offered` says whether the daemon will answer gate questions for this
/// executor. It is a plain flag rather than a gate object on purpose: the executor
/// must never be handed the *real* gate, and a parameter that merely looks like one
/// invites exactly that mistake.
pub fn serve_executor(
    control: OwnedFd,
    owner: String,
    factory: Arc<dyn AgentWorkerFactory>,
    gate_offered: bool,
) -> io::Result<()> {
    let stream = unsafe { UnixStream::from_raw_fd(control_into_raw(control)) };
    let writer = EventWriter {
        stream: Arc::new(Mutex::new(stream.try_clone()?)),
    };
    let reader = BufReader::new(stream);
    let state = Arc::new(ExecutorState::default());
    let owner = Arc::new(owner);
    let gate_plumbing = GatePlumbing {
        wire: Arc::new(writer.clone()),
        router: Arc::clone(&state.gate),
        offered: gate_offered,
    };

    // A single reader loop keeps request ordering well defined, which is what
    // makes cancel-after-start reliable.
    let mut reader = reader;
    loop {
        let frame: DaemonFrame = match read_frame(&mut reader) {
            Ok(frame) => frame,
            // EOF means the daemon is gone. Nothing may keep running: an executor
            // outliving its daemon is a leak the next generation cannot account for.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        match frame {
            DaemonFrame::Request(request) => {
                handle_request(&state, &factory, &owner, *request, &writer, &gate_plumbing)?;
            }
            DaemonFrame::Gate(message) => {
                handle_gate(&state, message)?;
            }
        }
        if state.stopping.load(Ordering::SeqCst) {
            break;
        }
    }

    // Stopping the process is the guarantee. Ask politely first so a cooperative
    // worker can finish its current turn, then let the process exit.
    state.shutdown();
    // A worker blocked in `gate.acquire()` would otherwise wait forever: the
    // daemon can no longer carry an answer, so fail the wait explicitly.
    state.gate.fail_all("the daemon channel closed");
    Ok(())
}

fn control_into_raw(control: OwnedFd) -> RawFd {
    use std::os::fd::IntoRawFd;
    control.into_raw_fd()
}

impl ExecutorState {
    fn shutdown(&self) {
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for attempt in attempts.values() {
            attempt.cancellation.cancel();
        }
    }
}

struct GatePlumbing {
    wire: Arc<dyn GateWire>,
    router: Arc<GateRouter>,
    /// Present only when the daemon offered a real gate to proxy.
    offered: bool,
}

fn handle_request(
    state: &Arc<ExecutorState>,
    factory: &Arc<dyn AgentWorkerFactory>,
    owner: &Arc<String>,
    request: ExecutorRequest,
    writer: &EventWriter,
    gate: &GatePlumbing,
) -> io::Result<()> {
    match request {
        ExecutorRequest::Start {
            command_id,
            generation,
            owner: request_owner,
            payload,
        } => {
            if request_owner != **owner {
                // The owner is established by the inherited descriptor, not by the
                // message. A mismatch means the daemon is confused about which
                // client this executor serves; refuse rather than run as someone
                // else.
                return reject(writer, &command_id, "executor serves a different owner");
            }
            if generation.is_empty() {
                return reject(writer, &command_id, "a Start must name its generation");
            }
            match state.claim(&command_id) {
                Claim::AlreadyAppliedOk => return Ok(()),
                Claim::AlreadyAppliedErr(reason) => return reject(writer, &command_id, &reason),
                Claim::First => {}
            }
            if payload.ensure_frameable().is_err() {
                state.record(
                    &command_id,
                    Err("payload exceeds the executor frame limit".into()),
                );
                return reject(
                    writer,
                    &command_id,
                    "payload exceeds the executor frame limit",
                );
            }
            let payload = *payload;
            let task = payload.task_id.clone();
            let attempt = payload.attempt_id.clone();
            let cancellation = CancellationToken::new();
            let start = match payload.to_start(cancellation.clone()) {
                Ok(start) => start,
                Err(reason) => {
                    state.record(&command_id, Err(reason.clone()));
                    return reject(writer, &command_id, &reason);
                }
            };
            match start_attempt(
                state,
                factory,
                &task,
                &attempt,
                start,
                cancellation,
                writer,
                Arc::clone(&gate.wire),
                Arc::clone(&gate.router),
                gate.offered,
            ) {
                Ok(()) => {
                    state.record(&command_id, Ok(()));
                    Ok(())
                }
                Err(error) => {
                    let reason = error.to_string();
                    state.record(&command_id, Err(reason.clone()));
                    reject(writer, &command_id, &reason)
                }
            }
        }
        ExecutorRequest::Cancel { task, attempt } => {
            // A cancel for an unknown attempt is not an error: it may have stopped
            // a moment ago, and the daemon's belief is only ever a hint.
            if let Some(running) = state
                .attempts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(task, attempt))
            {
                running.cancellation.cancel();
            }
            Ok(())
        }
        ExecutorRequest::Pause { task, attempt } => {
            if let Some(running) = state
                .attempts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(task, attempt))
            {
                running.handle.request_pause();
            }
            Ok(())
        }
        ExecutorRequest::Message {
            task,
            attempt,
            id,
            body,
        } => {
            let attempts = state
                .attempts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(running) = attempts.get(&(task, attempt)) else {
                return Ok(());
            };
            let message = yi_agent_core::subagent::worker::WorkerMessage {
                id: id
                    .parse()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad message id"))?,
                body,
            };
            // Delivered to the handle's own queue -- the same queue the worker's
            // agent loop reads. A second inbox beside it would be a queue nothing
            // ever drains, which looks like delivery and silently loses the
            // message.
            running.handle.deliver_worker_message(message);
            Ok(())
        }
    }
}

/// Handles a daemon-bound gate message.
fn handle_gate(state: &Arc<ExecutorState>, message: GateMessage) -> io::Result<()> {
    if !message.is_daemon_bound() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the executor does not accept its own gate questions",
        ));
    }
    match message {
        GateMessage::TurnGranted {
            request_id,
            permit_id,
        } => {
            // A grant arriving after the attempt stopped finds no waiter and is
            // dropped. That does not leak the global slot: the permit is the
            // daemon's, and its record is released when the daemon decides the
            // attempt is over.
            state.gate.deliver(&request_id, Ok(permit_id));
            Ok(())
        }
        GateMessage::Denied { request_id, reason } => {
            state.gate.deliver(&request_id, Err(reason));
            Ok(())
        }
        // `ReleaseTurn` is daemon-bound in shape, but the executor only releases
        // through its proxy lease's Drop, which sends its own request.
        GateMessage::ReleaseTurn { .. } => Ok(()),
        // `AcquireTurn` is the executor's own question; the daemon never answers a
        // question with a question.
        GateMessage::AcquireTurn { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the executor does not accept AcquireTurn from the daemon",
        )),
    }
}

fn reject(writer: &EventWriter, command_id: &str, reason: &str) -> io::Result<()> {
    writer.send(ExecutorEvent::Rejected {
        command_id: command_id.to_string(),
        reason: reason.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn start_attempt(
    state: &Arc<ExecutorState>,
    factory: &Arc<dyn AgentWorkerFactory>,
    task: &str,
    attempt: &str,
    start: WorkerStart,
    cancellation: CancellationToken,
    writer: &EventWriter,
    gate_wire: Arc<dyn GateWire>,
    gate_router: Arc<GateRouter>,
    gate_enabled: bool,
) -> io::Result<()> {
    // The proxy gate only exists when the daemon offered one: an executor with no
    // daemon-side gate has nothing to pace against, and inventing a local counter
    // would bypass the machine-wide limit.
    let proxy_gate: Option<Arc<dyn ProviderTurnGate>> = gate_enabled.then(|| {
        Arc::new(ProxyTurnGate::new(
            Arc::clone(&gate_wire),
            Arc::clone(&gate_router),
            task,
            attempt,
            "daemon-default",
        )) as Arc<dyn ProviderTurnGate>
    });
    let handle = block_on_start(Arc::clone(factory), start, proxy_gate)?;
    // The *same* token the Start was built with. A fresh token here would make
    // every daemon-side Cancel a no-op.
    let attempts_entry = RunningAttempt {
        handle: handle.clone(),
        cancellation,
    };
    state
        .attempts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert((task.to_string(), attempt.to_string()), attempts_entry);

    // One reporting thread per attempt: it drains the worker's buffers and
    // forwards them, then reports `Stopped` only after the worker's *join* proof
    // exists.
    let writer = writer.clone();
    let state = Arc::clone(state);
    let task = task.to_string();
    let attempt = attempt.to_string();
    std::thread::spawn(move || {
        loop {
            for event in handle.take_events() {
                if writer
                    .send(ExecutorEvent::Worker {
                        task: task.clone(),
                        attempt: attempt.clone(),
                        payload: WorkerEventWire::from(event),
                    })
                    .is_err()
                {
                    return;
                }
            }
            for event in handle.take_watchdog_events() {
                if writer
                    .send(ExecutorEvent::Watchdog {
                        task: task.clone(),
                        attempt: attempt.clone(),
                        payload: WorkerWatchdogEventWire::from(event),
                    })
                    .is_err()
                {
                    return;
                }
            }
            for fact in handle.take_trace_events() {
                if writer
                    .send(ExecutorEvent::Trace {
                        task: task.clone(),
                        attempt: attempt.clone(),
                        payload: fact,
                    })
                    .is_err()
                {
                    return;
                }
            }
            if handle.execution_stopped() {
                // Only the join proof produces this event. A worker that reported
                // `Completed` but whose thread is still unwinding is not stopped.
                let _ = writer.send(ExecutorEvent::Stopped {
                    task: task.clone(),
                    attempt: attempt.clone(),
                });
                state
                    .attempts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&(task, attempt));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    Ok(())
}

/// Runs the async `start` to completion on a private runtime.
fn block_on_start(
    factory: Arc<dyn AgentWorkerFactory>,
    start: WorkerStart,
    gate: Option<Arc<dyn ProviderTurnGate>>,
) -> io::Result<WorkerHandle> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(factory.start_with_provider_turn_gate(start, gate))
        .map_err(|error| io::Error::other(error.to_string()))
}

/// A counter for locally minted command ids (used by the daemon side).
pub fn next_command_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}
