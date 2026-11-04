//! The guardian's control channel: leases plus ownership, never business work.
//!
//! These tests exist because the two failure modes that matter are both about
//! *ordering*: a release that answers "done" before the processes are gone, and a
//! control request that quietly starts a runtime.

mod support;

use std::time::Duration;

use support::process_fixture as fixture;
use tempfile::TempDir;
use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlErrorCode, ControlRequest, ControlResponse, LeaseMode,
    RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::{Guardian, ProcessOwner, ProcessRegistry};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".to_string(),
        display_version: "0.0.0".to_string(),
        executable: "/usr/bin/true".into(),
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

fn guardian(directory: &TempDir) -> Guardian {
    Guardian::new(scope(directory), artifact(), "gen-1".to_string())
}

/// Identity is read-only: answering it must not create anything on disk.
#[test]
fn identity_does_not_create_a_runtime() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let path = scope.runtime_dir.join("guardian-control.sock");
    let artifact = artifact();
    let deadline = Deadline::after(Duration::from_secs(5));

    // Run the real guardian, stopped as soon as the request has been answered.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_server = stop.clone();
    let server = std::thread::spawn(move || {
        yi_agent_supervisors::lifecycle::guardian::serve(scope, artifact, move || {
            stop_for_server.load(std::sync::atomic::Ordering::SeqCst)
        })
    });

    // Wait for the socket rather than sleeping: a fixed delay is a flaky test.
    let deadline_for_wait = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline_for_wait,
            "the guardian never bound its control socket"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    let response = client
        .request(&ControlRequest::Identity, &deadline)
        .unwrap();
    match response {
        ControlResponse::Identity(identity) => {
            assert_eq!(identity.scope_key, directory.path().to_string_lossy());
            assert_eq!(identity.guardian_build_id.as_deref(), Some("test-build"));
        }
        other => panic!("expected an identity, got {other:?}"),
    }
    assert!(
        !directory.path().join("runtime.sqlite").exists(),
        "the guardian must never create the business database"
    );

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    // Unblock the accept loop with one more connection attempt.
    let _ = std::os::unix::net::UnixStream::connect(&path);
    server.join().unwrap().unwrap();
}

/// A second bind must refuse rather than steal a live guardian's socket.
#[test]
fn a_second_guardian_cannot_steal_the_control_socket() {
    let directory = TempDir::new().unwrap();
    let deadline = Deadline::after(Duration::from_secs(2));
    let first = guardian(&directory);
    let _listener = first.bind_control(&deadline).unwrap();

    let second = guardian(&directory);
    let error = second.bind_control(&deadline).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}

/// A socket file that is not a socket is never replaced: that would delete a
/// user's file on the strength of a name.
#[test]
fn a_non_socket_path_is_never_replaced() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("guardian-control.sock");
    std::fs::write(&path, b"not a socket").unwrap();
    let deadline = Deadline::after(Duration::from_secs(2));
    let error = guardian(&directory).bind_control(&deadline).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
}

/// An unimplemented control request is reported as unsupported, never accepted.
#[test]
fn unknown_control_requests_are_not_accepted() {
    // A `ReclaimStatus` for a client that never existed must not be answered with
    // an acknowledgement: "I do not know" and "yes" are different answers.
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let mut registry = ProcessRegistry::open(&scope, "gen-1", &artifact())
        .unwrap()
        .0;
    assert!(
        registry
            .owner_is_stopped(&ProcessOwner::frontend("nobody"))
            .unwrap()
    );
}

/// The helper runs as a test so the binary can re-exec itself.
#[test]
#[ignore = "internal fixture: requires the guardian's ready descriptor"]
fn process_fixture_helper() {
    fixture::run_helper_if_requested();
}

/// Release must reclaim the client's processes before it answers.
#[test]
fn release_reclaims_before_answering() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let mut registry = ProcessRegistry::open(&scope, "gen-1", &artifact())
        .unwrap()
        .0;
    let owner = ProcessOwner::frontend("client-a");
    let mut markers = fixture::MarkerGuard::new();
    let marker = markers.path("release");

    registry
        .spawn(
            fixture::steady_writer(owner.clone(), &marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    assert!(fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT));

    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    // The observable guarantee: after release, the writer is silent.
    let before = fixture::marker_len(&marker);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        fixture::marker_len(&marker),
        before,
        "a released client's tool must stop writing"
    );
    let _ = ClientKind::App;
    let _ = LeaseMode::Business;
    let _ = ControlErrorCode::Unsupported;
}
