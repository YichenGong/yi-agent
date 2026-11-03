//! Execution admission over the real guardian control socket.
//!
//! The property under test is the one that cannot be proven in a unit test of the
//! lease table: a *released* client's work is refused by the guardian itself, and
//! a second generation's ask is refused too. Both are asked through the real
//! `ControlClient` -> `serve_control_connection` path, on a real unix socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::ArtifactIdentity;
use yi_agent_lifecycle::types::{
    ClientKind, ControlRequest, ControlResponse, ExecutionAdmission, LeaseMode, RuntimeScope,
};
use yi_agent_supervisors::lifecycle::admission::GuardianAdmission;
use yi_agent_supervisors::lifecycle::guardian::{GUARDIAN_SOCKET_FILE, Guardian};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
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
    }
}

fn scope(directory: &std::path::Path) -> RuntimeScope {
    RuntimeScope {
        key: directory.to_string_lossy().into_owned(),
        runtime_dir: directory.to_path_buf(),
        project_key: "/project".into(),
        instance_home: directory.to_path_buf(),
    }
}

/// Serves the guardian's control socket on a thread until `stop` is set.
///
/// Uses `Guardian::bind_control` and the same per-connection handler `serve` uses,
/// so the test exercises the real accept loop shape rather than a stand-in.
fn spawn_guardian(
    scope: RuntimeScope,
    generation: &str,
    stop: Arc<AtomicBool>,
) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let guardian = Guardian::new(scope.clone(), artifact(), generation.to_string());
    let deadline = Deadline::after(Duration::from_secs(2));
    let listener = guardian.bind_control(&deadline).unwrap();
    // Non-blocking accept, so setting `stop` really ends the thread. A blocking
    // `accept` would leave the test's `join` waiting for a connection that never
    // comes.
    listener.set_nonblocking(true).unwrap();
    let socket = scope.runtime_dir.join(GUARDIAN_SOCKET_FILE);
    let handle = std::thread::spawn(move || {
        let mut guardian = guardian;
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = yi_agent_supervisors::lifecycle::guardian::serve_one_for_test(
                        &mut guardian,
                        stream,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (socket, handle)
}

fn register(socket: &std::path::Path, client: &str) -> yi_agent_lifecycle::types::ClientGrant {
    let deadline = Deadline::after(Duration::from_secs(2));
    let mut client_handle = ControlClient::connect(socket, &deadline).unwrap();
    match client_handle
        .request(
            &ControlRequest::Register {
                client_run_id: client.to_string(),
                kind: ClientKind::App,
                mode: LeaseMode::Business,
            },
            &deadline,
        )
        .unwrap()
    {
        ControlResponse::Granted(grant) => grant,
        other => panic!("expected a grant, got {other:?}"),
    }
}

#[test]
fn a_registered_client_is_admitted() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let grant = register(&socket, "app");

    let admission = GuardianAdmission::new(directory.path(), grant);
    assert!(admission.admit("gen-1", "task-1", "attempt-1").is_ok());
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn a_released_client_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let grant = register(&socket, "app");

    // Release through the real path: this is what closes the client's admission.
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut releaser = ControlClient::connect(&socket, &deadline).unwrap();
    releaser
        .request(
            &ControlRequest::Release {
                grant: grant.clone(),
            },
            &deadline,
        )
        .unwrap();

    let admission = GuardianAdmission::new(directory.path(), grant);
    let refused = admission.admit("gen-1", "task-1", "attempt-1");
    assert!(
        refused.is_err(),
        "a released client must not be admitted, got {refused:?}"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn a_stale_generation_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-2", Arc::clone(&stop));
    let grant = register(&socket, "app");

    let admission = GuardianAdmission::new(directory.path(), grant);
    let refused = admission.admit("gen-1", "task-1", "attempt-1");
    assert!(
        refused.is_err(),
        "the previous generation must not be admitted, got {refused:?}"
    );
    assert!(admission.admit("gen-2", "task-1", "attempt-1").is_ok());
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn a_forged_grant_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let mut grant = register(&socket, "app");
    grant.credential = "0".repeat(64);

    let admission = GuardianAdmission::new(directory.path(), grant);
    assert!(
        admission.admit("gen-1", "task-1", "attempt-1").is_err(),
        "a forged credential must not be admitted"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

/// An admission question for an owner with no lease is refused, not admitted.
///
/// "Never registered" must not read as "nothing forbids it": that would make the
/// barrier skippable by inventing an owner.
#[test]
fn an_unknown_owner_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let grant = register(&socket, "app");
    let other = yi_agent_lifecycle::types::ClientGrant {
        client_run_id: "never-registered".into(),
        credential: grant.credential.clone(),
        epoch: grant.epoch,
        mode: grant.mode,
    };
    let admission = GuardianAdmission::new(directory.path(), other);
    assert!(admission.admit("gen-1", "task-1", "attempt-1").is_err());
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

/// A lease-only client may observe but never run work.
#[test]
fn a_lease_only_client_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let deadline = Deadline::after(Duration::from_secs(2));
    let mut client = ControlClient::connect(&socket, &deadline).unwrap();
    let grant = match client
        .request(
            &ControlRequest::Register {
                client_run_id: "observer".into(),
                kind: ClientKind::App,
                mode: LeaseMode::LeaseOnly,
            },
            &deadline,
        )
        .unwrap()
    {
        ControlResponse::Granted(grant) => grant,
        other => panic!("expected a grant, got {other:?}"),
    };
    let admission = GuardianAdmission::new(directory.path(), grant);
    assert!(
        admission.admit("gen-1", "task-1", "attempt-1").is_err(),
        "a lease-only client must never be admitted for work"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

/// The question itself must name the exact execution it is about.
///
/// A blank task or attempt means the daemon is asking about nothing, and the
/// guardian should not answer "yes" to that.
#[test]
fn an_empty_execution_identity_is_not_admitted() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope(directory.path()), "gen-1", Arc::clone(&stop));
    let grant = register(&socket, "app");
    let deadline = Deadline::after(Duration::from_secs(2));
    let mut client = ControlClient::connect(&socket, &deadline).unwrap();
    let answer = client
        .request(
            &ControlRequest::Admission {
                asked: ExecutionAdmission {
                    grant,
                    generation: "gen-1".into(),
                    task: String::new(),
                    attempt: String::new(),
                },
            },
            &deadline,
        )
        .unwrap();
    assert!(
        matches!(answer, ControlResponse::Rejected { .. }),
        "an empty execution identity must be refused, got {answer:?}"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}
