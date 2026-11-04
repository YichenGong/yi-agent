//! The private managed-process channel, against real child processes.
//!
//! Every assertion here is about something the kernel decides: which descriptor a
//! child gets on fd 1, whether a caller's read reaches EOF, whether a group kill
//! reaches a grandchild, and whether a channel issued to one owner can act on
//! another owner's process. A fake registry would prove none of it.

mod support;

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use support::process_fixture as fixture;
use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::managed::client::{ManagedSpawnClient, StdioFds};
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ManagedSpawnRequest, ManagedSpawnResponse, ManagedSpawnSpec, RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::{ProcessOwner, ProcessRegistry};
use yi_agent_supervisors::lifecycle::managed_channel::{ChannelGrant, serve_channel};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".to_string(),
        display_version: "0.0.0".to_string(),
        executable: "/bin/true".into(),
        sha256: "00".to_string(),
        schema_min: 1,
        schema_max: 1,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    }
}

fn scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

/// Starts the channel server on a background thread and returns the peer and the
/// shared registry.
///
/// The registry is `Arc<Mutex<..>>` because that is what a real guardian holds: a
/// channel that owned it outright would stop the guardian's maintenance for as long
/// as the channel lived. The test keeps a clone so it can inspect `records()` after
/// the channel ends and so dropping the last clone still stops every group.
fn start_channel_with_registry(
    directory: &TempDir,
    grant: ChannelGrant,
) -> (
    UnixStream,
    std::sync::Arc<std::sync::Mutex<ProcessRegistry>>,
) {
    let (guardian_side, client_side) = UnixStream::pair().expect("socketpair");
    let (registry, report) = match ProcessRegistry::open(&scope(directory), "gen-1", &artifact()) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("registry open failed: {error}");
            panic!("registry open failed: {error}");
        }
    };
    assert!(report.is_safe(), "a fresh runtime has nothing to recover");
    let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));
    let shared = std::sync::Arc::clone(&registry);
    std::thread::spawn(move || {
        if let Err(error) = serve_channel(shared, &grant, guardian_side, Duration::from_secs(10)) {
            eprintln!("serve_channel error: {error}");
        }
        // Dropping the last registry clone stops every group it started: the
        // test's cleanup path, and the same guarantee a real guardian gives.
    });
    (client_side, registry)
}

/// Starts the channel server on a background thread and returns the peer.
fn start_channel(directory: &TempDir, grant: ChannelGrant) -> UnixStream {
    start_channel_with_registry(directory, grant).0
}

fn pipes() -> (
    std::fs::File,
    std::fs::File,
    std::fs::File,
    std::fs::File,
    std::fs::File,
    std::fs::File,
) {
    fn one() -> (std::fs::File, std::fs::File) {
        let mut ends = [0 as std::os::fd::RawFd; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        unsafe {
            (
                std::fs::File::from_raw_fd(ends[0]),
                std::fs::File::from_raw_fd(ends[1]),
            )
        }
    }
    let (stdin_read, stdin_write) = one();
    let (stdout_read, stdout_write) = one();
    let (stderr_read, stderr_write) = one();
    (
        stdin_read,
        stdin_write,
        stdout_read,
        stdout_write,
        stderr_read,
        stderr_write,
    )
}

fn owned(file: std::fs::File) -> OwnedFd {
    use std::os::fd::IntoRawFd;
    unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) }
}

/// The same for a connected stream: the client takes ownership of the number.
fn owned_stream(stream: UnixStream) -> OwnedFd {
    use std::os::fd::IntoRawFd;
    unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) }
}

fn helper_spec(role_args: &[&str]) -> ManagedSpawnSpec {
    let mut args = vec![
        "--exact".to_string(),
        fixture::HELPER_TEST.to_string(),
        "--ignored".to_string(),
        "--nocapture".to_string(),
    ];
    args.extend(role_args.iter().map(|arg| arg.to_string()));
    ManagedSpawnSpec {
        executable: std::env::current_exe().expect("test binary path"),
        args,
        env: Vec::new(),
        cwd: std::env::temp_dir(),
    }
}

/// The headline stream property: a child's stdout and stderr stay two streams and
/// each reaches EOF when the child exits.
#[test]
fn managed_child_preserves_exit_signal_and_streams() {
    let directory = TempDir::new().unwrap();
    let peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("app")));
    let mut client = ManagedSpawnClient::connect(owned_stream(peer)).expect("connect");

    let (stdin_read, _stdin_write, stdout_read, stdout_write, stderr_read, stderr_write) = pipes();
    let receipt = client
        .spawn(
            helper_spec(&["streams"]),
            StdioFds {
                stdin: owned(stdin_read),
                stdout: owned(stdout_write),
                stderr: owned(stderr_write),
            },
        )
        .expect("spawn");
    assert!(!receipt.process_id.is_empty());

    let mut out = String::new();
    let mut out_file = stdout_read;
    out_file.read_to_string(&mut out).expect("read stdout");
    // libtest prints its own banner on stdout before the helper runs, so this is
    // a containment check: what matters is that the child's stdout line is here
    // and the stderr line is not.
    assert!(
        out.contains("on-stdout\n"),
        "stdout must carry the child's stdout: {out:?}"
    );
    assert!(
        !out.contains("on-stderr"),
        "stderr must not be merged into stdout: {out:?}"
    );

    let mut err = String::new();
    let mut err_file = stderr_read;
    err_file.read_to_string(&mut err).expect("read stderr");
    assert!(
        err.contains("on-stderr\n"),
        "stderr must carry the child's stderr: {err:?}"
    );
    assert!(
        !err.contains("on-stdout"),
        "stdout must not be merged into stderr: {err:?}"
    );

    let exit = client
        .wait(
            &receipt.process_id,
            &Deadline::after(Duration::from_secs(10)),
        )
        .expect("wait");
    assert_eq!(exit.code, Some(0));
    assert!(
        exit.group_stopped,
        "an exited single process group is stopped"
    );
}

/// stdin is really stdin: the child reads what the caller writes and the echo
/// arrives on stdout.
#[test]
fn a_managed_child_reads_the_stdin_it_was_given() {
    let directory = TempDir::new().unwrap();
    let peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("app")));
    let mut client = ManagedSpawnClient::connect(owned_stream(peer)).expect("connect");

    let (stdin_read, mut stdin_write, stdout_read, stdout_write, stderr_read, stderr_write) =
        pipes();
    let receipt = client
        .spawn(
            helper_spec(&["stdin-echo"]),
            StdioFds {
                stdin: owned(stdin_read),
                stdout: owned(stdout_write),
                stderr: owned(stderr_write),
            },
        )
        .expect("spawn");
    stdin_write.write_all(b"piped-input").expect("write stdin");
    // Closing the write end is the EOF the child's `read_to_end` waits for.
    drop(stdin_write);

    let mut out = String::new();
    let mut out_file = stdout_read;
    out_file.read_to_string(&mut out).expect("read stdout");
    assert!(
        out.contains("piped-input"),
        "the child must echo exactly what stdin carried: {out:?}"
    );

    let _ = stderr_read;
    let exit = client
        .wait(
            &receipt.process_id,
            &Deadline::after(Duration::from_secs(10)),
        )
        .expect("wait");
    assert_eq!(exit.code, Some(0));
}

/// Terminating a group stops a grandchild too, and the reported exit says so.
#[test]
fn terminating_a_group_reaches_a_grandchild() {
    let directory = TempDir::new().unwrap();
    let peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("app")));
    let mut client = ManagedSpawnClient::connect(owned_stream(peer)).expect("connect");

    let (stdin_read, _stdin_write, stdout_read, stdout_write, stderr_read, stderr_write) = pipes();
    let receipt = client
        .spawn(
            helper_spec(&["streams-then-hang"]),
            StdioFds {
                stdin: owned(stdin_read),
                stdout: owned(stdout_write),
                stderr: owned(stderr_write),
            },
        )
        .expect("spawn");

    // The leader is alive, so a `try_wait` must not claim it stopped.
    assert!(
        client
            .try_wait(&receipt.process_id)
            .expect("try_wait")
            .is_none(),
        "a running leader must not be reported as exited"
    );

    let exit = client
        .terminate(
            &receipt.process_id,
            &Deadline::after(Duration::from_secs(15)),
        )
        .expect("terminate");
    assert!(
        exit.group_stopped,
        "termination must wait for the whole group, not just the leader"
    );

    // And the group really is empty afterwards.
    assert!(
        client
            .try_wait(&receipt.process_id)
            .expect("try_wait")
            .is_some(),
        "a terminated group is stopped"
    );
    let _ = (stdout_read, stderr_read);
}

/// A channel issued to one owner cannot wait on or kill another owner's process.
#[test]
fn a_channel_cannot_act_on_another_owners_process() {
    let directory = TempDir::new().unwrap();
    let app_peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("app")));
    let mut app = ManagedSpawnClient::connect(owned_stream(app_peer)).expect("connect app");

    let (stdin_read, _stdin_write, _stdout_read, stdout_write, _stderr_read, stderr_write) =
        pipes();
    let receipt = app
        .spawn(
            helper_spec(&["streams-then-hang"]),
            StdioFds {
                stdin: owned(stdin_read),
                stdout: owned(stdout_write),
                stderr: owned(stderr_write),
            },
        )
        .expect("spawn for app");

    // A second guardian thread for a *different* owner, sharing the same runtime
    // directory and therefore the same journal.
    let tmp_peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("tui")));
    let mut tui = ManagedSpawnClient::connect(owned_stream(tmp_peer)).expect("connect tui");
    let refusal = tui
        .terminate(
            &receipt.process_id,
            &Deadline::after(Duration::from_secs(5)),
        )
        .expect_err("another owner's channel must not terminate this process");
    assert!(
        refusal.to_string().contains("not owned")
            || refusal.to_string().contains("does not belong"),
        "the refusal must say why: {refusal}"
    );

    // The owner itself can still stop it.
    let exit = app
        .terminate(
            &receipt.process_id,
            &Deadline::after(Duration::from_secs(15)),
        )
        .expect("owner terminate");
    assert!(exit.group_stopped);
}

/// A request carrying the wrong number of descriptors is refused, not silently
/// started with inherited stdio.
#[test]
fn a_spawn_without_three_stdio_descriptors_is_refused() {
    let directory = TempDir::new().unwrap();
    let peer = start_channel(&directory, ChannelGrant::new(ProcessOwner::frontend("app")));
    let mut raw = peer;
    let body =
        yi_agent_lifecycle::codec::encode(&ManagedSpawnRequest::Spawn(helper_spec(&["exit"])))
            .expect("encode");
    raw.write_all(&body).expect("write frame");
    let frame = yi_agent_lifecycle::codec::read_frame(&mut raw).expect("response frame");
    let response: ManagedSpawnResponse = yi_agent_lifecycle::codec::decode(&frame).expect("decode");
    match response {
        ManagedSpawnResponse::Failed { reason } => {
            assert!(
                reason.contains("three stdio"),
                "must name the rule: {reason}"
            )
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A pending `Wait` must not hold the registry.
///
/// This is the property that makes the channel usable from a real guardian: the
/// guardian's lease maintenance and owner reclamation need the same registry, and a
/// channel that held it for the whole wait would stop them for as long as any tool
/// was running.
///
/// The child is a long sleeper stopped only at the end, so the wait window is
/// deterministic. Two channels share one registry, exactly as a real guardian's do:
/// one holds the `Wait`, the other performs the `Terminate` (a single channel answers
/// requests in order, so it cannot do both). With a wait that keeps the lock across
/// its budget, the probe below cannot succeed.
#[test]
fn a_pending_wait_does_not_hold_the_registry() {
    let directory = TempDir::new().unwrap();
    let (registry, report) =
        ProcessRegistry::open(&scope(&directory), "gen-1", &artifact()).expect("registry");
    assert!(report.is_safe(), "a fresh runtime has nothing to recover");
    let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));

    let spawn_channel = |shared: std::sync::Arc<std::sync::Mutex<ProcessRegistry>>| {
        let (guardian_side, client_side) = UnixStream::pair().expect("socketpair");
        let grant = ChannelGrant::new(ProcessOwner::frontend("app"));
        std::thread::spawn(move || {
            let _ = serve_channel(shared, &grant, guardian_side, Duration::from_secs(30));
        });
        client_side
    };

    // Channel 1: spawn and hold the wait.
    let mut waiter_client = ManagedSpawnClient::connect(owned_stream(spawn_channel(
        std::sync::Arc::clone(&registry),
    )))
    .expect("connect");
    let (stdin_read, _stdin_write, stdout_read, stdout_write, stderr_read, stderr_write) = pipes();
    let receipt = waiter_client
        .spawn(
            helper_spec(&["streams-then-hang"]),
            StdioFds {
                stdin: owned(stdin_read),
                stdout: owned(stdout_write),
                stderr: owned(stderr_write),
            },
        )
        .expect("spawn");
    // Keep the reader ends open; dropping them early would let the child die on its
    // first write and shrink the window this test depends on.
    let _keep_stdout = stdout_read;
    let _keep_stderr = stderr_read;
    let process_id = receipt.process_id.clone();

    let waiter = {
        let process_id = process_id.clone();
        std::thread::spawn(move || {
            waiter_client
                .wait(&process_id, &Deadline::after(Duration::from_secs(20)))
                .expect("the wait answers once the group is stopped")
        })
    };

    // Channel 2: same owner, own connection -- the shape a guardian really has when
    // two tools of one frontend are live at once.
    let mut terminator = ManagedSpawnClient::connect(owned_stream(spawn_channel(
        std::sync::Arc::clone(&registry),
    )))
    .expect("connect");

    // The child sleeps far longer than the window below, so a `Wait` that held the
    // lock could not release it in time.
    std::thread::sleep(Duration::from_millis(200));
    let start = std::time::Instant::now();
    let mut acquired = false;
    while start.elapsed() < Duration::from_millis(800) {
        match registry.try_lock() {
            Ok(guard) => {
                // Reading records is what a recovering guardian does under this lock.
                let _ = guard.records().len();
                acquired = true;
                break;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let _ = poisoned.into_inner().records().len();
                acquired = true;
                break;
            }
        }
    }

    // End the child regardless of the verdict, so the in-flight wait answers and the
    // thread joins instead of leaking into the next test.
    let terminated = terminator.terminate(&process_id, &Deadline::after(Duration::from_secs(10)));
    let exit = waiter.join().expect("wait thread");
    let _ = exit;

    assert!(
        acquired,
        "a pending Wait must not hold the registry: the guardian's reclaim loop \
         could never run while a tool was alive"
    );
    assert!(
        terminated.map(|e| e.group_stopped).unwrap_or(false),
        "the terminating channel must prove the group stopped"
    );
}

/// The helper entry point, same contract as the other supervisor test binaries:
/// returns immediately unless this process was re-executed as the fixture.
///
/// Without this test the fixture's `--exact process_fixture_helper` selector
/// matches nothing, libtest exits, and the guardian's spawn sees `EPIPE` instead
/// of a child holding its ready barrier.
#[test]
#[ignore]
fn process_fixture_helper() {
    fixture::run_helper_if_requested();
}
