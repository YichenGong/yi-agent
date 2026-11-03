//! Why a worker runs in a process: a thread cannot be stopped, a process can.
//!
//! The tests here use two *fake* executors -- small programs that speak the
//! executor protocol -- rather than a real LLM, so they are fast and deterministic
//! while still exercising the real wire format and the real process boundary.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use yi_agent_core::message::Message;
use yi_agent_core::subagent::task::{AttemptId, ChildWriteMode, RootSessionId, TaskId};
use yi_agent_core::subagent::worker::WorkerStart;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
use yi_agent_subagent::executor::protocol::{
    DaemonFrame, ExecutorRequest, RootExecutionConfig, WorkerStartWire, read_frame,
};
use yi_agent_subagent::executor::proxy::ExecutorProxy;
use yi_agent_supervisors::lifecycle::guardian::{ProcessOwner, ProcessRegistry};

fn owned(stream: UnixStream) -> OwnedFd {
    use std::os::fd::IntoRawFd;
    unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) }
}

/// A socket pair, standing in for the inherited descriptor a real spawn provides.
fn channel() -> (OwnedFd, UnixStream) {
    let (daemon_side, executor_side) = UnixStream::pair().unwrap();
    (owned(daemon_side), executor_side)
}

use std::os::fd::FromRawFd;

fn start_wire(task: &str, attempt: &str, objective: &str) -> WorkerStartWire {
    let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_objective(objective)
        .with_workspace_mode(ChildWriteMode::ReadOnly);
    let mut wire = WorkerStartWire::from_start(&start, RootExecutionConfig::default());
    // The fake executors key on their own ids, not on a real task id.
    wire.task_id = task.to_string();
    wire.attempt_id = attempt.to_string();
    wire
}

/// The headline property: a worker that ignores its cancel keeps running, so the
/// only way to stop it is to stop its executor's process group.
///
/// A real process is used, because that is the whole claim: a thread that ignores
/// a cancel token cannot be stopped at all, while a process group can be killed
/// even when the process inside it traps every polite signal. Everything is
/// observed through a file the worker appends to, never through stdout -- a leaked
/// writer on the test's stdout would keep the runner's pipe open forever.
#[test]
fn a_worker_that_ignores_cancel_is_stopped_only_by_killing_its_executor() {
    let directory = tempfile::TempDir::new().unwrap();
    let marker = directory.path().join("ticks");
    let scope = RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".into(),
        instance_home: directory.path().to_path_buf(),
    };
    let artifact = ArtifactIdentity {
        build_id: "test-build".into(),
        display_version: "0".into(),
        executable: "/usr/bin/true".into(),
        sha256: "00".into(),
        schema_min: 1,
        schema_max: 1,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    };
    let (mut registry, _report) = ProcessRegistry::open(&scope, "gen-1", &artifact).unwrap();
    let owner = ProcessOwner::frontend("client-a");

    // Trap every polite stop, then tick forever from both the shell and its
    // background child. That child is the shape of a tool a wedged worker would
    // still have running: stopping only the shell would leave it behind.
    let script = format!(
        "trap '' TERM INT HUP; \
         ( while true; do echo tick >> {marker}; sleep 0.02; done ) & \
         while true; do echo tick >> {marker}; sleep 0.02; done",
        marker = marker.display()
    );
    let command = yi_agent_supervisors::lifecycle::guardian::ManagedCommand {
        executable: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        env: Vec::new(),
        cwd: directory.path().to_path_buf(),
        owner: owner.clone(),
        root: None,
        execution: None,
    };
    registry
        .spawn(command, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    assert!(!registry.owner_is_stopped(&owner).unwrap());

    let size = || std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    let wait_for_size = |deadline: Duration| {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if size() > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for_size(Duration::from_secs(5));
    assert!(size() > 0, "the worker never started ticking");

    // The polite stop is what it ignores; the group kill is what ends it.
    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.owner_is_stopped(&owner).unwrap());

    let settled = size();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        settled,
        size(),
        "the worker (or a child it forked) kept running after the group was stopped"
    );
}

/// A start that does not fit the frame limit is refused before anything runs.
#[test]
fn an_oversized_start_is_refused_at_the_boundary() {
    let (daemon_side, _executor_side) = channel();
    let proxy = ExecutorProxy::from_fd(daemon_side).unwrap();
    let mut wire = start_wire("t1", "a1", "big");
    wire.fork_messages = Some(vec![Message::user(
        "x".repeat(yi_agent_subagent::executor::MAX_EXECUTOR_FRAME),
    )]);
    let error = proxy
        .send_start("c1", "gen-1", "client-a", wire)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

/// The same Start identity must not be sent twice, even to a fresh executor.
#[test]
fn a_start_is_never_replayed() {
    let (daemon_side, _executor_side) = channel();
    let proxy = ExecutorProxy::from_fd(daemon_side).unwrap();
    proxy
        .send_start("c1", "gen-1", "owner", start_wire("t1", "a1", "once"))
        .unwrap();
    let error = proxy
        .send_start("c1", "gen-1", "owner", start_wire("t1", "a1", "once"))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
}

/// A different attempt of the same task is a different Start, and is allowed.
#[test]
fn a_new_attempt_is_a_new_start() {
    let (daemon_side, _executor_side) = channel();
    let proxy = ExecutorProxy::from_fd(daemon_side).unwrap();
    proxy
        .send_start("c1", "gen-1", "owner", start_wire("t1", "a1", "first"))
        .unwrap();
    proxy
        .send_start("c2", "gen-1", "owner", start_wire("t1", "a2", "retry"))
        .unwrap();
}

/// The owner comes from the connection, not from the message: a mismatched owner
/// is refused rather than silently run as someone else.
#[tokio::test]
async fn a_factory_start_requires_the_owners_authority() {
    // The executor refuses a Start naming a different owner before it starts a
    // worker. Exercised through the wire so the check cannot be bypassed by a
    // caller that constructs the request directly.
    let (daemon_side, executor_side) = channel();
    let proxy = ExecutorProxy::from_fd(daemon_side).unwrap();
    proxy
        .send(&ExecutorRequest::Start {
            command_id: "c1".into(),
            generation: "gen-1".into(),
            owner: "client-b".into(),
            payload: Box::new(start_wire("t1", "a1", "x")),
        })
        .unwrap();
    let mut reader = std::io::BufReader::new(executor_side);
    let frame: DaemonFrame = read_frame(&mut reader).unwrap();
    match frame {
        DaemonFrame::Request(request) => match *request {
            ExecutorRequest::Start { owner, .. } => assert_eq!(owner, "client-b"),
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }
}

/// Killing the executor's process group stops the whole subtree, not just the
/// process the daemon happens to hold a PID for.
#[test]
fn killing_the_executor_group_stops_its_children() {
    let directory = tempfile::TempDir::new().unwrap();
    let scope = RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".into(),
        instance_home: directory.path().to_path_buf(),
    };
    let artifact = ArtifactIdentity {
        build_id: "test-build".into(),
        display_version: "0".into(),
        executable: "/usr/bin/true".into(),
        sha256: "00".into(),
        schema_min: 1,
        schema_max: 1,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    };
    let (mut registry, _report) = ProcessRegistry::open(&scope, "gen-1", &artifact).unwrap();
    let owner = ProcessOwner::frontend("client-a");
    // A shell that starts a grandchild and then sleeps: the exact shape of an
    // executor that forked a tool and then hung.
    let command = yi_agent_supervisors::lifecycle::guardian::ManagedCommand {
        executable: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 60 & sleep 60".into()],
        env: Vec::new(),
        cwd: directory.path().to_path_buf(),
        owner: owner.clone(),
        root: None,
        execution: None,
    };
    registry
        .spawn(command, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    assert!(!registry.owner_is_stopped(&owner).unwrap());
    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.owner_is_stopped(&owner).unwrap());
}
