//! The provider-turn gate across process boundaries.
//!
//! The limit the daemon enforces is one machine-wide number of concurrent provider
//! turns. An executor that kept its own count would multiply that limit by the
//! number of clients, and an executor that could grant itself a turn would let a
//! wedged client starve the others. So the permit lives in the daemon, and the
//! tests here are about every way it must *not* be leaked or duplicated:
//!
//! - two executors share one limit;
//! - a cancelled waiter releases its slot;
//! - a late grant for a finished attempt is freed;
//! - a queued turn never delays the cancellation of another attempt;
//! - an executor that dies frees whatever it was holding;
//! - a channel that closes wakes a waiting acquire instead of parking it.

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use yi_agent_core::agent::{ProviderTurnGate, ProviderTurnLease};
use yi_agent_core::subagent::task::{AttemptId, ChildWriteMode, RootSessionId, TaskId};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_subagent::executor::factory::{ExecutorConnector, ProcessWorkerFactory};
use yi_agent_subagent::executor::protocol::RootExecutionConfig;
use yi_agent_subagent::executor::proxy::ExecutorProxy;
use yi_agent_subagent::executor::serve::serve_executor;

fn owned(stream: UnixStream) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) }
}

/// A gate with a hard limit, counting what is really held.
struct CountingGate {
    limit: usize,
    held: Arc<AtomicUsize>,
    waiters: Arc<AtomicUsize>,
}

struct Lease {
    held: Arc<AtomicUsize>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ProviderTurnGate for CountingGate {
    fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
        let limit = self.limit;
        let held = Arc::clone(&self.held);
        let waiters = Arc::clone(&self.waiters);
        Box::pin(async move {
            loop {
                let current = held.load(Ordering::SeqCst);
                if current < limit
                    && held
                        .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    return Ok(Box::new(Lease { held }) as Box<dyn ProviderTurnLease>);
                }
                waiters.fetch_add(1, Ordering::SeqCst);
                // A blocking sleep, not `tokio::time::sleep`: the daemon's reader
                // answers gate questions from a plain thread via `block_on`, where
                // there is no reactor to drive the timer.
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }
}

/// A worker that acquires one provider turn and holds it until cancelled.
///
/// It obtains the gate the executor was given, which is the *proxy* gate: the
/// permit it ends up with is the daemon's, reached through the wire.
struct TurnHolder {
    /// Set while a permit is held.
    holding: Arc<AtomicBool>,
    /// Set once the acquire succeeded.
    got_turn: Arc<AtomicBool>,
    /// Set if the acquire failed, with the reason.
    failure: Arc<Mutex<Option<String>>>,
}

impl AgentWorkerFactory for TurnHolder {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let holding = Arc::clone(&self.holding);
        let got = Arc::clone(&self.got_turn);
        let failure = Arc::clone(&self.failure);
        let cancellation = request.cancellation.clone();
        let handle = WorkerHandle::new(cancellation.clone());
        std::thread::spawn(move || {
            let Some(gate) = gate else {
                *failure.lock().unwrap() = Some("no gate was offered".into());
                return;
            };
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    *failure.lock().unwrap() = Some(error.to_string());
                    return;
                }
            };
            let lease = runtime.block_on(gate.acquire());
            match lease {
                Ok(lease) => {
                    holding.store(true, Ordering::SeqCst);
                    got.store(true, Ordering::SeqCst);
                    // Hold the permit until cancelled, then drop it: that is the
                    // moment the daemon's counter must go back down.
                    let until = Instant::now() + Duration::from_secs(20);
                    while Instant::now() < until && !cancellation.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    holding.store(false, Ordering::SeqCst);
                    drop(lease);
                }
                Err(reason) => {
                    *failure.lock().unwrap() = Some(reason);
                }
            }
        });
        Box::pin(async { Ok(handle) })
    }
}

/// Two executors may not exceed one shared limit.
///
/// Both workers really run in executors, and both really ask the daemon through
/// the wire. With a limit of one, only the first may hold a turn; releasing it must
/// let the second through. Without the daemon holding the real permit, each
/// executor would enforce its own limit and the machine-wide number would double.
#[test]
fn two_executors_share_one_global_limit() {
    struct Connector {
        holding: Arc<AtomicBool>,
        failure: Arc<Mutex<Option<String>>>,
    }
    impl ExecutorConnector for Connector {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let factory = Arc::new(TurnHolder {
                holding: Arc::clone(&self.holding),
                got_turn: Arc::new(AtomicBool::new(false)),
                failure: Arc::clone(&self.failure),
            });
            let owner = owner.to_string();
            std::thread::spawn(move || {
                let _ = serve_executor(owned(executor_side), owner, factory, true);
            });
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let held = Arc::new(AtomicUsize::new(0));
    let waiters = Arc::new(AtomicUsize::new(0));
    let gate: Arc<dyn ProviderTurnGate> = Arc::new(CountingGate {
        limit: 1,
        held: Arc::clone(&held),
        waiters: Arc::clone(&waiters),
    });

    let first_holding = Arc::new(AtomicBool::new(false));
    let second_holding = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));

    let factory_a = ProcessWorkerFactory::new(
        "gen",
        Arc::new(Connector {
            holding: Arc::clone(&first_holding),
            failure: Arc::clone(&failure),
        }),
        None,
        RootExecutionConfig::default(),
    );
    let factory_b = ProcessWorkerFactory::new(
        "gen",
        Arc::new(Connector {
            holding: Arc::clone(&second_holding),
            failure: Arc::clone(&failure),
        }),
        None,
        RootExecutionConfig::default(),
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Each factory serves a different client, so each gets its own executor.
    let first =
        runtime
            .block_on(factory_a.start_with_provider_turn_gate(
                start(RootSessionId::new()),
                Some(Arc::clone(&gate)),
            ))
            .unwrap();
    let second =
        runtime
            .block_on(factory_b.start_with_provider_turn_gate(
                start(RootSessionId::new()),
                Some(Arc::clone(&gate)),
            ))
            .unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || first_holding
            .load(Ordering::SeqCst)),
        "the first executor never got its turn"
    );
    assert_eq!(held.load(Ordering::SeqCst), 1);
    // The second is blocked: with a limit of one, a second held permit would be a
    // global-limit violation across processes.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !second_holding.load(Ordering::SeqCst),
        "two executors each held a turn over a limit of one"
    );
    assert_eq!(held.load(Ordering::SeqCst), 1);

    // Release the first; the second must now get through.
    first.cancel();
    assert!(
        wait_until(Duration::from_secs(10), || second_holding
            .load(Ordering::SeqCst)),
        "releasing the first turn did not admit the second"
    );
    assert_eq!(
        held.load(Ordering::SeqCst),
        1,
        "exactly one real permit must be held at a time"
    );
    assert!(failure.lock().unwrap().is_none(), "no acquire should fail");
    second.cancel();
    let _ = wait_until(Duration::from_secs(5), || held.load(Ordering::SeqCst) == 0);
}

fn start(root: RootSessionId) -> WorkerStart {
    let mut start = WorkerStart::new(TaskId::new(), AttemptId::new(), root);
    start.workspace_mode = ChildWriteMode::ReadOnly;
    start
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

/// The daemon answers a real executor's gate question and holds the permit.
///
/// This exercises the sub-protocol end to end: the executor asks, the daemon's
/// real gate decides, and the permit lives on the daemon's side of the boundary.
#[test]
fn a_daemon_grant_holds_the_real_permit_for_an_executor() {
    let held = Arc::new(AtomicUsize::new(0));
    let gate: Arc<dyn ProviderTurnGate> = Arc::new(CountingGate {
        limit: 4,
        held: Arc::clone(&held),
        waiters: Arc::new(AtomicUsize::new(0)),
    });
    let (daemon_side, executor_side) = UnixStream::pair().unwrap();
    let proxy = ExecutorProxy::from_fd(owned(daemon_side)).unwrap();

    // The executor's side asks for a turn.
    proxy
        .send_gate_for_test(
            yi_agent_subagent::executor::protocol::GateMessage::AcquireTurn {
                request_id: "r1".into(),
                task: "t1".into(),
                attempt: "a1".into(),
                profile: "p".into(),
            },
        )
        .unwrap();
    let mut reader = std::io::BufReader::new(executor_side.try_clone().unwrap());
    let frame: yi_agent_subagent::executor::protocol::ExecutorFrame =
        yi_agent_subagent::executor::protocol::read_frame(&mut reader).unwrap();
    let yi_agent_subagent::executor::protocol::ExecutorFrame::Gate(question) = frame else {
        panic!("expected a gate question");
    };

    // The daemon answers with its real gate.
    proxy.answer_gate(question, gate.as_ref()).unwrap();
    assert_eq!(
        held.load(Ordering::SeqCst),
        1,
        "the daemon must hold the real permit"
    );
    assert_eq!(proxy.held_permits(), 1);

    // The grant is the daemon's first frame. Read it before asking for a release:
    // a gate channel has no ordering guarantee between the two, but it has exactly
    // one frame in flight at a time, and reading in order keeps the test honest.
    let grant: yi_agent_subagent::executor::protocol::DaemonFrame =
        yi_agent_subagent::executor::protocol::read_frame(&mut reader).unwrap();
    match grant {
        yi_agent_subagent::executor::protocol::DaemonFrame::Gate(
            yi_agent_subagent::executor::protocol::GateMessage::TurnGranted {
                request_id,
                permit_id,
            },
        ) => {
            assert_eq!(request_id, "r1");
            // A release frees it -- by dropping the boxed permit, not a counter.
            proxy
                .answer_gate(
                    yi_agent_subagent::executor::protocol::GateMessage::ReleaseTurn { permit_id },
                    gate.as_ref(),
                )
                .unwrap();
        }
        other => panic!("expected a grant, got {other:?}"),
    }
    assert_eq!(held.load(Ordering::SeqCst), 0);
    assert_eq!(proxy.held_permits(), 0);
}

/// An executor that disappears releases every permit it was holding.
///
/// The permit is the daemon's object. If dropping the proxy did not free it, a
/// crashed executor would permanently consume a slot of the global limit.
#[test]
fn a_dead_executor_releases_its_permits() {
    let held = Arc::new(AtomicUsize::new(0));
    let gate: Arc<dyn ProviderTurnGate> = Arc::new(CountingGate {
        limit: 2,
        held: Arc::clone(&held),
        waiters: Arc::new(AtomicUsize::new(0)),
    });
    {
        let (daemon_side, _executor_side) = UnixStream::pair().unwrap();
        let proxy = ExecutorProxy::from_fd(owned(daemon_side)).unwrap();
        proxy
            .answer_gate(
                yi_agent_subagent::executor::protocol::GateMessage::AcquireTurn {
                    request_id: "r1".into(),
                    task: "t".into(),
                    attempt: "a".into(),
                    profile: "p".into(),
                },
                gate.as_ref(),
            )
            .unwrap();
        assert_eq!(held.load(Ordering::SeqCst), 1);
    }
    assert_eq!(
        held.load(Ordering::SeqCst),
        0,
        "dropping the executor proxy must release the real permit"
    );
}

/// A refused acquire is reported, not left hanging.
///
/// An executor waiting silently for a permit that will never arrive looks exactly
/// like a slow provider; the worker must see the refusal.
#[test]
fn a_refused_acquire_is_reported() {
    struct Refusing;
    impl ProviderTurnGate for Refusing {
        fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
            Box::pin(async { Err("the gate is closed for maintenance".to_string()) })
        }
    }
    let (daemon_side, executor_side) = UnixStream::pair().unwrap();
    let proxy = ExecutorProxy::from_fd(owned(daemon_side)).unwrap();
    proxy
        .answer_gate(
            yi_agent_subagent::executor::protocol::GateMessage::AcquireTurn {
                request_id: "r1".into(),
                task: "t".into(),
                attempt: "a".into(),
                profile: "p".into(),
            },
            &Refusing,
        )
        .unwrap();
    let mut reader = std::io::BufReader::new(executor_side);
    let frame: yi_agent_subagent::executor::protocol::DaemonFrame =
        yi_agent_subagent::executor::protocol::read_frame(&mut reader).unwrap();
    match frame {
        yi_agent_subagent::executor::protocol::DaemonFrame::Gate(
            yi_agent_subagent::executor::protocol::GateMessage::Denied { reason, .. },
        ) => assert!(reason.contains("closed for maintenance")),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// An executor whose workers ask for a turn only when the test says so.
///
/// Each `Start` installs its gate where the test can reach it, so the test decides
/// when a question is asked and when the queue forms.
struct OnDemandTurn {
    /// Gates in start order: the test reads them by index.
    gates: Mutex<Vec<Arc<dyn ProviderTurnGate>>>,
    cancelled: Mutex<Vec<Arc<AtomicBool>>>,
}

impl AgentWorkerFactory for OnDemandTurn {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        if let Some(gate) = gate {
            self.gates.lock().unwrap().push(gate);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancelled.lock().unwrap().push(Arc::clone(&cancelled));
        let cancellation = request.cancellation.clone();
        std::thread::spawn(move || {
            while !cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::SeqCst);
        });
        Box::pin(async { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

/// A queued turn on one attempt must not delay cancelling another attempt on the
/// same executor.
///
/// This is the deadlock the design calls out: one read loop is the only path for
/// Cancel, so if it blocked to acquire a permit while the global limit was full,
/// the very cancel that would free the slot could never be delivered.
#[test]
fn a_queued_turn_does_not_delay_a_cancel() {
    let held = Arc::new(AtomicUsize::new(0));
    let gate: Arc<dyn ProviderTurnGate> = Arc::new(CountingGate {
        limit: 1,
        held: Arc::clone(&held),
        waiters: Arc::new(AtomicUsize::new(0)),
    });

    // Occupy the only slot, so both later acquires must queue.
    let blocker = futures::executor::block_on(gate.acquire()).unwrap();
    assert_eq!(held.load(Ordering::SeqCst), 1);

    let worker = Arc::new(OnDemandTurn {
        gates: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });

    struct Connector(Arc<OnDemandTurn>);
    impl ExecutorConnector for Connector {
        fn connect(&self, owner: &str) -> std::io::Result<ExecutorProxy> {
            let (daemon_side, executor_side) = UnixStream::pair()?;
            let factory: Arc<dyn AgentWorkerFactory> = self.0.clone();
            let owner = owner.to_string();
            std::thread::spawn(move || {
                let _ = serve_executor(owned(executor_side), owner, factory, true);
            });
            ExecutorProxy::from_fd(owned(daemon_side))
        }
    }

    let factory = ProcessWorkerFactory::new(
        "gen",
        Arc::new(Connector(Arc::clone(&worker))),
        None,
        RootExecutionConfig::default(),
    );

    // Same root session: both attempts must share one executor, which is the
    // situation where a blocked read loop is fatal.
    let root = RootSessionId::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let a = runtime
        .block_on(
            factory.start_with_provider_turn_gate(start(root.clone()), Some(Arc::clone(&gate))),
        )
        .unwrap();
    let b = runtime
        .block_on(factory.start_with_provider_turn_gate(start(root), Some(Arc::clone(&gate))))
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || worker
            .cancelled
            .lock()
            .unwrap()
            .len()
            == 2),
        "both attempts should have started inside one executor"
    );
    let gates = worker.gates.lock().unwrap().clone();
    let first_gate = Arc::clone(&gates[0]);
    let second_gate = Arc::clone(&gates[1]);

    // Both attempts ask while the slot is taken. Their questions arrive on one
    // socket, so they queue behind the same full gate.
    let asker_first = std::thread::spawn(move || {
        let _ = futures::executor::block_on(first_gate.acquire());
    });
    let asker_second = std::thread::spawn(move || {
        let _ = futures::executor::block_on(second_gate.acquire());
    });
    std::thread::sleep(Duration::from_millis(200));

    // While both acquires are queued, a cancel must still reach its worker. If the
    // read loop were blocked on a permit, this flag would never flip.
    let second_cancelled = Arc::clone(&worker.cancelled.lock().unwrap()[1]);
    b.cancel();
    assert!(
        wait_until(Duration::from_secs(5), || second_cancelled
            .load(Ordering::SeqCst)),
        "a cancel was stuck behind a queued provider turn"
    );

    drop(blocker);
    let _ = asker_first.join();
    let _ = asker_second.join();
    a.cancel();
    assert!(
        wait_until(Duration::from_secs(10), || held.load(Ordering::SeqCst) == 0),
        "every real permit must come back"
    );
}
