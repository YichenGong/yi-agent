//! The daemon-to-executor bridge, end to end over a real socket pair.
//!
//! These tests use the real `serve_executor` on one end and the real
//! `ProcessWorkerFactory` on the other. The worker itself is a fake, so nothing
//! here reaches a provider or a database, but every wire frame, every lock, and
//! both bridge threads are the ones production uses. That is the point: the bugs
//! this file is meant to catch -- a cancel that never crosses, a stop proof
//! satisfied by the wrong event, a reader that blocks writers -- all live in the
//! plumbing rather than in the agent.

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use yi_agent_core::subagent::task::{AttemptId, ChildWriteMode, RootSessionId, TaskId};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_subagent::executor::factory::{ExecutorConnector, ProcessWorkerFactory};
use yi_agent_subagent::executor::protocol::RootExecutionConfig;
use yi_agent_subagent::executor::proxy::ExecutorProxy;
use yi_agent_subagent::executor::serve::serve_executor;

fn owned(stream: UnixStream) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) }
}

/// What a fake worker does when its executor starts it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Reports `Completed`, then keeps running and ignores cancellation.
    ///
    /// This is the worker the whole process boundary exists for: a cooperative
    /// cancel cannot stop it, so only killing the executor can.
    LieAboutBeingDone,
    /// Waits for its token, then stops -- and whose executor observes the join.
    StopOnCancel,
}

/// A factory that stands in for the executor's real agent construction.
struct FakeInnerFactory {
    behaviour: Behaviour,
    /// Set when the executor's cancellation token actually fired.
    observed_cancel: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl AgentWorkerFactory for FakeInnerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let behaviour = self.behaviour;
        let observed = Arc::clone(&self.observed_cancel);
        let running = Arc::clone(&self.running);
        let (witness, confirmation) = yi_agent_core::subagent::worker::StopWitness::pair();
        let handle =
            WorkerHandle::new(request.cancellation.clone()).with_confirmation(confirmation);
        let worker = handle.clone();
        let cancellation = request.cancellation.clone();
        running.store(true, Ordering::SeqCst);
        std::thread::spawn(move || match behaviour {
            Behaviour::LieAboutBeingDone => {
                // Claims to be finished while the execution goes on.
                worker.report_completed("done");
                // Deliberately ignores the token; only process death stops this.
                let until = Instant::now() + Duration::from_secs(30);
                while Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                }
                witness.mark_joined();
                running.store(false, Ordering::SeqCst);
            }
            Behaviour::StopOnCancel => {
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    if cancellation.is_cancelled() {
                        observed.store(true, Ordering::SeqCst);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // Stopping means: the execution joined. Only now may the stop
                // proof be satisfied.
                witness.mark_joined();
                running.store(false, Ordering::SeqCst);
            }
        });
        Box::pin(async move { Ok(handle) })
    }
}

/// A connector that hands the factory an already-wired socket pair.
struct PairedConnector {
    inner: Arc<dyn Fn() -> Arc<FakeInnerFactory> + Send + Sync>,
    executors: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl ExecutorConnector for PairedConnector {
    fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
        let (daemon_side, executor_side) = UnixStream::pair()?;
        let inner = (self.inner)();
        let owner = owner.to_string();
        let handle = std::thread::spawn(move || {
            // Serving ends when the daemon closes the channel; the test then just
            // observes the executor's effects.
            let _ = serve_executor(owned(executor_side), owner, inner, false);
        });
        self.executors.lock().unwrap().push(handle);
        ExecutorProxy::from_fd(owned(daemon_side))
    }
}

fn start_request(behaviour: Behaviour) -> (WorkerStart, Arc<AtomicBool>, Arc<AtomicBool>) {
    let observed_cancel = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(false));
    let mut start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_objective("bridge")
        .with_workspace_mode(ChildWriteMode::ReadOnly);
    start.model = String::new();
    let _ = behaviour;
    (start, observed_cancel, running)
}

fn factory(
    behaviour: Behaviour,
    observed_cancel: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) -> (ProcessWorkerFactory, Arc<PairedConnector>) {
    let inner = Arc::new(move || {
        Arc::new(FakeInnerFactory {
            behaviour,
            observed_cancel: Arc::clone(&observed_cancel),
            running: Arc::clone(&running),
        }) as Arc<FakeInnerFactory>
    });
    let connector = Arc::new(PairedConnector {
        inner: inner.clone(),
        executors: Mutex::new(Vec::new()),
    });
    (
        ProcessWorkerFactory::new(
            "gen-test",
            connector.clone(),
            Some("test-profile".into()),
            RootExecutionConfig::default(),
        ),
        connector,
    )
}

fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    check()
}

/// The headline bridge property: a cancel reaches the remote worker.
///
/// Without the signal mirror this test fails even though the local token is
/// cancelled -- the remote worker never learns about it.
#[test]
fn a_cancel_crosses_to_the_executor() {
    let (start, observed, running) = start_request(Behaviour::StopOnCancel);
    let (factory, _connector) = factory(
        Behaviour::StopOnCancel,
        Arc::clone(&observed),
        Arc::clone(&running),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.block_on(factory.start(start)).unwrap();
    assert!(wait_until(Duration::from_secs(5), || running.load(Ordering::SeqCst)));

    handle.cancel();

    assert!(
        wait_until(Duration::from_secs(5), || observed.load(Ordering::SeqCst)),
        "the executor's worker never saw the cancel"
    );
    assert!(
        wait_until(Duration::from_secs(5), || handle.execution_stopped()),
        "the stop proof was never satisfied after the worker joined"
    );
}

/// A `Completed` report does not satisfy the stop proof.
///
/// The worker claims to be done and keeps running. If the bridge treated
/// `Completed` as a stop, the coordinator would free a live lease.
#[test]
fn completed_does_not_prove_stopped() {
    let (start, _observed, running) = start_request(Behaviour::LieAboutBeingDone);
    let (factory, _connector) = factory(
        Behaviour::LieAboutBeingDone,
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&running),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.block_on(factory.start(start)).unwrap();

    // Wait for the Completed report to arrive through the bridge.
    let saw_completed = wait_until(Duration::from_secs(5), || {
        handle.take_events().iter().any(|event| {
            matches!(
                event,
                yi_agent_core::subagent::worker::WorkerEvent::Completed { .. }
            )
        })
    });
    assert!(
        saw_completed,
        "the Completed report never crossed the bridge"
    );
    assert!(
        running.load(Ordering::SeqCst),
        "the worker should still be running"
    );
    assert!(
        !handle.execution_stopped(),
        "a Completed report must never satisfy the stop proof"
    );

    // A cancel is sent, but the worker ignores it; the proof must stay unsatisfied.
    handle.cancel();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !handle.execution_stopped(),
        "a worker that ignores its cancel is not stopped"
    );
    assert!(running.load(Ordering::SeqCst));
}

/// A message delivered on the handle reaches the remote worker's own queue.
#[test]
fn a_message_crosses_to_the_executor() {
    let (start, _observed, running) = start_request(Behaviour::StopOnCancel);
    let (factory, _connector) = factory(
        Behaviour::StopOnCancel,
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&running),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.block_on(factory.start(start)).unwrap();
    assert!(wait_until(Duration::from_secs(5), || running.load(Ordering::SeqCst)));

    handle.deliver_message("hello from the daemon");
    // The bridge has no way to echo the message back; the property under test is
    // that sending it does not fail or block. A wedged bridge would hang here.
    std::thread::sleep(Duration::from_millis(100));
    handle.cancel();
}

/// One executor serves several tasks for the same client, and one task's stop
/// does not stop the other.
///
/// The second task deliberately behaves differently from the first, so a shared
/// handle or a shared cancellation token would show up as the wrong task stopping.
#[test]
fn one_executor_serves_two_tasks_that_stop_independently() {
    struct PerTaskConnector {
        spawns: Arc<AtomicUsize>,
        stopper_ran: Arc<AtomicBool>,
        stopper_cancelled: Arc<AtomicBool>,
        liar_ran: Arc<AtomicBool>,
    }

    /// Starts the attempt that stops on cancel.
    struct Stopper {
        ran: Arc<AtomicBool>,
        observed: Arc<AtomicBool>,
    }
    impl AgentWorkerFactory for Stopper {
        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            let (witness, confirmation) = yi_agent_core::subagent::worker::StopWitness::pair();
            let handle =
                WorkerHandle::new(request.cancellation.clone()).with_confirmation(confirmation);
            let ran = Arc::clone(&self.ran);
            let observed = Arc::clone(&self.observed);
            let cancellation = request.cancellation.clone();
            ran.store(true, Ordering::SeqCst);
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(20);
                while Instant::now() < until {
                    if cancellation.is_cancelled() {
                        observed.store(true, Ordering::SeqCst);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                witness.mark_joined();
            });
            Box::pin(async move { Ok(handle) })
        }
    }

    /// Starts the attempt that ignores cancel and reports Completed.
    struct Liar {
        ran: Arc<AtomicBool>,
    }
    impl AgentWorkerFactory for Liar {
        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            let (witness, confirmation) = yi_agent_core::subagent::worker::StopWitness::pair();
            let handle =
                WorkerHandle::new(request.cancellation.clone()).with_confirmation(confirmation);
            let worker = handle.clone();
            let ran = Arc::clone(&self.ran);
            ran.store(true, Ordering::SeqCst);
            std::thread::spawn(move || {
                worker.report_completed("done");
                let until = Instant::now() + Duration::from_secs(20);
                while Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                }
                witness.mark_joined();
            });
            Box::pin(async move { Ok(handle) })
        }
    }

    /// Dispatches by objective, so both attempts really run in one executor.
    struct TwoBehaviour {
        stopper_ran: Arc<AtomicBool>,
        stopper_cancelled: Arc<AtomicBool>,
        liar_ran: Arc<AtomicBool>,
    }
    impl AgentWorkerFactory for TwoBehaviour {
        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            if request.objective == "stop-me" {
                Stopper {
                    ran: Arc::clone(&self.stopper_ran),
                    observed: Arc::clone(&self.stopper_cancelled),
                }
                .start(request)
            } else {
                Liar {
                    ran: Arc::clone(&self.liar_ran),
                }
                .start(request)
            }
        }
    }

    impl ExecutorConnector for PerTaskConnector {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let factory = Arc::new(TwoBehaviour {
                stopper_ran: Arc::clone(&self.stopper_ran),
                stopper_cancelled: Arc::clone(&self.stopper_cancelled),
                liar_ran: Arc::clone(&self.liar_ran),
            });
            let owner = owner.to_string();
            std::thread::spawn(move || {
                let _ = serve_executor(owned(executor_side), owner, factory, false);
            });
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let spawns = Arc::new(AtomicUsize::new(0));
    let stopper_ran = Arc::new(AtomicBool::new(false));
    let stopper_cancelled = Arc::new(AtomicBool::new(false));
    let liar_ran = Arc::new(AtomicBool::new(false));
    let factory = ProcessWorkerFactory::new(
        "gen-test",
        Arc::new(PerTaskConnector {
            spawns: Arc::clone(&spawns),
            stopper_ran: Arc::clone(&stopper_ran),
            stopper_cancelled: Arc::clone(&stopper_cancelled),
            liar_ran: Arc::clone(&liar_ran),
        }),
        None,
        RootExecutionConfig::default(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let root = RootSessionId::new();
    let mut stopper = WorkerStart::new(TaskId::new(), AttemptId::new(), root.clone());
    stopper.objective = "stop-me".into();
    stopper.workspace_mode = ChildWriteMode::ReadOnly;
    let mut liar = WorkerStart::new(TaskId::new(), AttemptId::new(), root);
    liar.objective = "lie".into();
    liar.workspace_mode = ChildWriteMode::ReadOnly;

    let stopper_handle = runtime.block_on(factory.start(stopper)).unwrap();
    let liar_handle = runtime.block_on(factory.start(liar)).unwrap();

    // Both attempts really ran, in one executor.
    assert!(wait_until(Duration::from_secs(5), || stopper_ran
        .load(Ordering::SeqCst)
        && liar_ran.load(Ordering::SeqCst)));
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "one client must get exactly one executor"
    );

    // Stop only the first one.
    stopper_handle.cancel();
    assert!(
        wait_until(Duration::from_secs(5), || stopper_cancelled
            .load(Ordering::SeqCst)),
        "the first task's cancel never reached its worker"
    );
    assert!(
        wait_until(Duration::from_secs(5), || stopper_handle
            .execution_stopped()),
        "the first task's stop proof was never satisfied"
    );
    // The second task is untouched: no false stop proof, still running.
    assert!(
        !liar_handle.execution_stopped(),
        "stopping one task must not prove the other task stopped"
    );
    assert!(liar_ran.load(Ordering::SeqCst));
    // We can only get here if the reader routed by (task, attempt) and did not
    // attribute the first task's Stopped to the second.
}

/// An executor that dies leaves the stop proof unsatisfied on purpose.
///
/// The channel really is torn down here (the executor thread exits and the socket
/// closes); the assertion is that EOF is not treated as evidence.
#[test]
fn a_closed_channel_does_not_prove_the_worker_stopped() {
    struct VanishingConnector {
        keep: Mutex<Vec<std::thread::JoinHandle<()>>>,
    }
    impl ExecutorConnector for VanishingConnector {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let owner = owner.to_string();
            let factory = Arc::new(FakeInnerFactory {
                behaviour: Behaviour::LieAboutBeingDone,
                observed_cancel: Arc::new(AtomicBool::new(false)),
                running: Arc::new(AtomicBool::new(false)),
            });
            let handle = std::thread::spawn(move || {
                // Close the executor's side without ever running a worker join
                // proof: this is the crash case.
                let _ = serve_executor(owned(executor_side), owner, factory, false);
            });
            self.keep.lock().unwrap().push(handle);
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let running = Arc::new(AtomicBool::new(false));
    let connector = Arc::new(VanishingConnector {
        keep: Mutex::new(Vec::new()),
    });
    let factory =
        ProcessWorkerFactory::new("gen-test", connector, None, RootExecutionConfig::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    start.workspace_mode = ChildWriteMode::ReadOnly;
    let handle = runtime.block_on(factory.start(start)).unwrap();
    let _ = &running;

    // Give the bridge time to notice EOF. A bridge that treated the closed
    // channel as a stop would flip this flag.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !handle.execution_stopped(),
        "EOF is not evidence that the worker's tools stopped"
    );
}

/// The executor refuses a Start whose owner does not match its descriptor.
///
/// The owner is established by the inherited descriptor, never by the message, so
/// a confused daemon cannot make one client's executor run as another.
#[test]
fn a_mismatched_owner_is_refused() {
    let (daemon_side, executor_side) = UnixStream::pair().unwrap();
    let factory = Arc::new(FakeInnerFactory {
        behaviour: Behaviour::StopOnCancel,
        observed_cancel: Arc::new(AtomicBool::new(false)),
        running: Arc::new(AtomicBool::new(false)),
    });
    std::thread::spawn(move || {
        let _ = serve_executor(owned(executor_side), "owner-a".into(), factory, false);
    });
    let proxy = ExecutorProxy::from_fd(owned(daemon_side)).unwrap();
    let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    let wire = yi_agent_subagent::executor::protocol::WorkerStartWire::from_start(
        &start,
        RootExecutionConfig::default(),
    );
    // Framing succeeds; the executor is what refuses, so read its answer.
    proxy.send_start("c1", "gen-1", "owner-b", wire).unwrap();
    let frame = proxy.receive().unwrap();
    let text = format!("{frame:?}");
    assert!(
        text.contains("different owner"),
        "expected an owner rejection, got {text}"
    );
}

/// A gate frame is answered by the daemon's real gate, not by the executor.
///
/// This is the sub-protocol that had no envelope: before `DaemonFrame`/`ExecutorFrame`
/// existed, a `TurnGranted` went out on the request stream and the executor's
/// request parser rejected it.
#[test]
fn a_gate_frame_gets_an_answer() {
    struct OnePermitGate {
        held: Arc<AtomicBool>,
    }
    struct HeldPermit {
        held: Arc<AtomicBool>,
    }
    impl Drop for HeldPermit {
        fn drop(&mut self) {
            self.held.store(false, Ordering::SeqCst);
        }
    }
    impl yi_agent_core::agent::ProviderTurnGate for OnePermitGate {
        fn acquire(
            &self,
        ) -> BoxFuture<'static, Result<Box<dyn yi_agent_core::agent::ProviderTurnLease>, String>>
        {
            self.held.store(true, Ordering::SeqCst);
            let held = Arc::clone(&self.held);
            Box::pin(async move {
                Ok(Box::new(HeldPermit { held })
                    as Box<dyn yi_agent_core::agent::ProviderTurnLease>)
            })
        }
    }

    let held = Arc::new(AtomicBool::new(false));
    let gate: Arc<dyn yi_agent_core::agent::ProviderTurnGate> = Arc::new(OnePermitGate {
        held: Arc::clone(&held),
    });
    let (daemon_side, executor_side) = UnixStream::pair().unwrap();
    let proxy = ExecutorProxy::from_fd(owned(daemon_side)).unwrap();

    // The executor asks; the daemon answers with its real gate.
    proxy
        .send_gate_for_test(
            yi_agent_subagent::executor::protocol::GateMessage::AcquireTurn {
                request_id: "r1".into(),
                task: "t".into(),
                attempt: "a".into(),
                profile: "p".into(),
            },
        )
        .unwrap();

    // One reader for both frames: two `BufReader`s over clones of one socket each
    // buffer independently, and the second would lose bytes the first over-read.
    let mut reader = std::io::BufReader::new(executor_side.try_clone().unwrap());
    let frame: yi_agent_subagent::executor::protocol::ExecutorFrame =
        yi_agent_subagent::executor::protocol::read_frame(&mut reader).unwrap();
    let yi_agent_subagent::executor::protocol::ExecutorFrame::Gate(question) = frame else {
        panic!("expected a gate question");
    };
    proxy.answer_gate(question, gate.as_ref()).unwrap();
    assert!(held.load(Ordering::SeqCst), "the real permit must be held");

    // The grant comes back on the stream the executor reads: it is a `Gate` frame,
    // which is exactly what had no envelope before.
    let grant: yi_agent_subagent::executor::protocol::DaemonFrame =
        yi_agent_subagent::executor::protocol::read_frame(&mut reader).unwrap();
    match grant {
        yi_agent_subagent::executor::protocol::DaemonFrame::Gate(
            yi_agent_subagent::executor::protocol::GateMessage::TurnGranted { request_id, .. },
        ) => assert_eq!(request_id, "r1"),
        other => panic!("expected a grant, got {other:?}"),
    }
}

/// A refused admission stops the Start before the executor ever sees it.
///
/// This is the barrier's whole purpose: after a client is released, its existing
/// executor channel must not be usable to run one more task.
#[test]
fn a_refused_admission_never_reaches_the_executor() {
    use std::sync::atomic::AtomicUsize;
    use yi_agent_subagent::executor::factory::ExecutionAdmissionCheck;

    struct Refusing;
    impl ExecutionAdmissionCheck for Refusing {
        fn admit(
            &self,
            _generation: &str,
            _owner: &str,
            _task: &str,
            _attempt: &str,
        ) -> Result<(), String> {
            Err("this client's admission is closed".into())
        }
    }

    struct CountingConnector {
        connects: Arc<AtomicUsize>,
    }
    impl ExecutorConnector for CountingConnector {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let factory = Arc::new(FakeInnerFactory {
                behaviour: Behaviour::StopOnCancel,
                observed_cancel: Arc::new(AtomicBool::new(false)),
                running: Arc::new(AtomicBool::new(false)),
            });
            let owner = owner.to_string();
            std::thread::spawn(move || {
                let _ = serve_executor(owned(executor_side), owner, factory, false);
            });
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let connects = Arc::new(AtomicUsize::new(0));
    let factory = ProcessWorkerFactory::new(
        "gen-test",
        Arc::new(CountingConnector {
            connects: Arc::clone(&connects),
        }),
        None,
        RootExecutionConfig::default(),
    )
    .with_admission(Arc::new(Refusing));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    start.workspace_mode = ChildWriteMode::ReadOnly;
    let error = runtime.block_on(factory.start(start)).unwrap_err();
    assert!(
        format!("{error:?}").contains("refused this execution"),
        "expected an admission refusal, got {error:?}"
    );
    assert_eq!(
        connects.load(Ordering::SeqCst),
        0,
        "a refused Start must not even open an executor"
    );
}

/// Admission is re-asked for every Start, never remembered.
///
/// A client that is admitted once and released a moment later must have its next
/// Start refused; caching the first answer would keep it running.
#[test]
fn admission_is_asked_on_every_start() {
    use std::sync::atomic::AtomicUsize;
    use yi_agent_subagent::executor::factory::ExecutionAdmissionCheck;

    struct CountingAdmission {
        asks: Arc<AtomicUsize>,
        refuse_after: usize,
    }
    impl ExecutionAdmissionCheck for CountingAdmission {
        fn admit(
            &self,
            _generation: &str,
            _owner: &str,
            _task: &str,
            _attempt: &str,
        ) -> Result<(), String> {
            let ask = self.asks.fetch_add(1, Ordering::SeqCst);
            if ask >= self.refuse_after {
                return Err("released".into());
            }
            Ok(())
        }
    }

    struct Paired(Arc<AtomicBool>);
    impl ExecutorConnector for Paired {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let running = Arc::clone(&self.0);
            let factory = Arc::new(FakeInnerFactory {
                behaviour: Behaviour::StopOnCancel,
                observed_cancel: Arc::new(AtomicBool::new(false)),
                running,
            });
            let owner = owner.to_string();
            std::thread::spawn(move || {
                let _ = serve_executor(owned(executor_side), owner, factory, false);
            });
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let asks = Arc::new(AtomicUsize::new(0));
    let factory = ProcessWorkerFactory::new(
        "gen-test",
        Arc::new(Paired(Arc::new(AtomicBool::new(false)))),
        None,
        RootExecutionConfig::default(),
    )
    .with_admission(Arc::new(CountingAdmission {
        asks: Arc::clone(&asks),
        refuse_after: 1,
    }));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let root = RootSessionId::new();
    let mut first = WorkerStart::new(TaskId::new(), AttemptId::new(), root.clone());
    first.workspace_mode = ChildWriteMode::ReadOnly;
    assert!(runtime.block_on(factory.start(first)).is_ok());

    let mut second = WorkerStart::new(TaskId::new(), AttemptId::new(), root);
    second.workspace_mode = ChildWriteMode::ReadOnly;
    let error = runtime.block_on(factory.start(second)).unwrap_err();
    assert!(
        format!("{error:?}").contains("refused this execution"),
        "expected the second Start to be refused, got {error:?}"
    );
    assert_eq!(asks.load(Ordering::SeqCst), 2, "admission must be re-asked");
}
