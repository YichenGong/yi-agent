//! `ControlRequest::Status`: what a UI is allowed to read, and what it must never
//! turn into.
//!
//! A status read is the one control request a frontend polls continuously, so the
//! properties that matter are about it being *boring*: it must not create a lease,
//! must not extend one, must not report a partial page as the whole answer, and
//! must never report a build it could not verify as compatible.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientGrant, ClientKind, ControlRequest, ControlResponse, LeaseMode,
    RuntimeMode, RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::{Guardian, STATUS_CLIENT_PAGE, serve_one_for_test};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".into(),
        display_version: "0.2.0".into(),
        executable: "/usr/bin/true".into(),
        sha256: "00".into(),
        schema_min: 1,
        schema_max: 16,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: vec!["lease.v1".into()],
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

/// Serves control connections on a thread until `stop` is set.
fn spawn_guardian(
    directory: &std::path::Path,
    stop: Arc<AtomicBool>,
) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let scope = scope(directory);
    let guardian = Guardian::new(scope.clone(), artifact(), "gen-1".to_string());
    let deadline = Deadline::after(Duration::from_secs(2));
    let listener = guardian.bind_control(&deadline).unwrap();
    listener.set_nonblocking(true).unwrap();
    let socket = scope.runtime_dir.join("guardian-control.sock");
    let handle = std::thread::spawn(move || {
        let mut guardian = guardian;
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_one_for_test(&mut guardian, stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (socket, handle)
}

fn register(socket: &std::path::Path, client: &str, kind: ClientKind) -> ClientGrant {
    let deadline = Deadline::after(Duration::from_secs(2));
    let mut handle = ControlClient::connect(socket, &deadline).unwrap();
    match handle
        .request(
            &ControlRequest::Register {
                client_run_id: client.to_string(),
                kind,
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

fn status(socket: &std::path::Path, cursor: Option<&str>) -> ControlResponse {
    let deadline = Deadline::after(Duration::from_secs(2));
    let mut handle = ControlClient::connect(socket, &deadline).unwrap();
    handle
        .request(
            &ControlRequest::Status {
                cursor: cursor.map(str::to_string),
            },
            &deadline,
        )
        .unwrap()
}

/// A client that is polling status does not thereby exist: the runtime must not
/// gain a lease, and the answer must not depend on anything being written.
#[test]
fn status_never_registers_anything() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _grant = register(&socket, "app", ClientKind::App);

    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    // The App itself, and nobody else: a status call adds no client.
    assert_eq!(snapshot.counts.clients, 1);
    assert_eq!(snapshot.clients.len(), 1);
    assert_eq!(snapshot.clients[0].client_run_id, "app");
    assert_eq!(snapshot.clients[0].kind, ClientKind::App);

    // Read twice: still one, so the read is not a registration in disguise.
    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert_eq!(snapshot.counts.clients, 1);

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// The count must describe the whole runtime while the page carries only some of
/// it. Otherwise a UI reads "1 client" and lists one of twelve.
#[test]
fn the_page_is_bounded_but_the_count_is_whole() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _owner = register(&socket, "app", ClientKind::App);
    let total = STATUS_CLIENT_PAGE + 3;
    for index in 0..total - 1 {
        register(&socket, &format!("tui-{index:02}"), ClientKind::Tui);
    }

    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert_eq!(
        snapshot.counts.clients, total as u64,
        "the count must be the whole runtime"
    );
    assert_eq!(snapshot.clients.len(), STATUS_CLIENT_PAGE);
    let cursor = snapshot
        .clients_next_cursor
        .expect("a truncated page must say how to continue");

    // Walk the rest: no row may repeat, and the union must be the whole set.
    let mut seen: Vec<String> = snapshot
        .clients
        .iter()
        .map(|row| row.client_run_id.clone())
        .collect();
    let mut cursor = Some(cursor);
    while let Some(after) = cursor {
        let response = status(&socket, Some(&after));
        let ControlResponse::Status(page) = response else {
            panic!("expected a status, got {response:?}");
        };
        let before = seen.len();
        seen.extend(page.clients.iter().map(|row| row.client_run_id.clone()));
        assert_eq!(
            seen.len(),
            before + page.clients.len(),
            "a repeated row means the cursor is not stable"
        );
        cursor = page.clients_next_cursor;
    }
    assert_eq!(seen.len(), total);
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), total, "every client appears exactly once");

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// A status read neither needs nor creates a lease: that is the whole reason it is
/// safe for a UI to poll, and the reason the app-server (which must not hold the
/// App's renewable credential) can answer it at all.
#[test]
fn status_needs_no_lease_and_creates_none() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));

    // No client has registered at all.
    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert_eq!(snapshot.counts.clients, 0);
    assert!(snapshot.clients.is_empty());

    // Reading it again must not have registered anything.
    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert_eq!(
        snapshot.counts.clients, 0,
        "a status read must not create a lease, or polling would keep a runtime alive"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// The identity in a status answer is the same one `Identity` reports. Two paths
/// that disagreed would let a UI show one build in the banner and another in the
/// status panel.
#[test]
fn status_and_identity_describe_the_same_runtime() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _grant = register(&socket, "app", ClientKind::App);

    let deadline = Deadline::after(Duration::from_secs(2));
    let mut client = ControlClient::connect(&socket, &deadline).unwrap();
    let ControlResponse::Identity(identity) = client
        .request(&ControlRequest::Identity, &deadline)
        .unwrap()
    else {
        panic!("expected an identity");
    };

    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert_eq!(snapshot.identity, identity);
    assert_eq!(snapshot.identity.mode, RuntimeMode::Managed);
    assert_eq!(
        snapshot.identity.guardian_build_id.as_deref(),
        Some("test-build")
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// A runtime that cannot switch says so instead of pretending a restart is
/// available. `local_restart_available` is that answer, and `active_operation`
/// being `None` is a fact rather than an error.
#[test]
fn a_guardian_without_a_switch_controller_says_so() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _grant = register(&socket, "app", ClientKind::App);

    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert!(!snapshot.local_restart_available);
    assert!(snapshot.active_operation.is_none());

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// Counts from a status read are explicitly incomplete: the guardian cannot see
/// the daemon's executor queue, so `complete: false` is the only honest answer.
#[test]
fn status_counts_are_marked_incomplete() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _grant = register(&socket, "app", ClientKind::App);

    let response = status(&socket, None);
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert!(
        !snapshot.counts.complete,
        "a status read cannot see the daemon's queue, so it must not claim completeness"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

/// A cursor naming a client that has since gone must not make the reader replay
/// rows from before its own cursor.
#[test]
fn a_stale_cursor_yields_no_repeated_rows() {
    let directory = tempfile::TempDir::new().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(directory.path(), Arc::clone(&stop));
    let _owner = register(&socket, "app", ClientKind::App);

    let response = status(&socket, Some("a-client-that-never-existed"));
    let ControlResponse::Status(snapshot) = response else {
        panic!("expected a status, got {response:?}");
    };
    assert!(
        snapshot.clients.is_empty(),
        "an unknown cursor means the page it described is gone, not that the list restarts"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}
