//! Who a managed channel is issued to, and who is refused.
//!
//! The property under test is the one the whole nested-spawn design rests on: the
//! descriptor *is* the authorization. The guardian decides the owner when it creates
//! the pair, so a tool that holds a channel cannot ask to run as somebody else. What
//! this file adds is the other half -- the guardian must not hand a channel to a
//! client that has no business lease, because a channel is the ability to start
//! work, and work is exactly what a lease-only observer never authorizes.

mod support;

use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::client::{ControlClient, FrontendLease};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlErrorCode, ControlResponse, LeaseMode, RuntimeScope,
};

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

/// Runs the real guardian until `stop` is set, returning the control socket path.
///
/// The guardian is started with no daemon program: this file is about who may open
/// a channel, not about what the channel starts. A guardian that owned no registry
/// would answer `Unsupported`, which is a different assertion (see below).
fn start_guardian(
    directory: &TempDir,
) -> (
    std::path::PathBuf,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let scope = scope(directory);
    let path = scope
        .runtime_dir
        .join(yi_agent_lifecycle::GUARDIAN_SOCKET_FILE);
    let artifact = artifact();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_server = stop.clone();
    let server = std::thread::spawn(move || {
        yi_agent_supervisors::lifecycle::guardian::serve(scope, artifact, move || {
            stop_for_server.load(std::sync::atomic::Ordering::SeqCst)
        })
    });
    // Wait for the socket rather than sleeping: a fixed delay is a flaky test.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the guardian never bound its control socket"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    (path, stop, server)
}

fn stop_guardian(
    path: &std::path::Path,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    server: std::thread::JoinHandle<std::io::Result<()>>,
) {
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    // Unblock the accept loop with one more connection attempt.
    let _ = std::os::unix::net::UnixStream::connect(path);
    server.join().unwrap().unwrap();
}

/// A frontend holding a business lease can open a channel, and the answer names the
/// owner the guardian bound rather than the one the caller claimed.
#[test]
fn a_business_lease_opens_a_channel_bound_to_its_own_owner() {
    let directory = TempDir::new().unwrap();
    let (path, stop, server) = start_guardian(&directory);
    let deadline = Deadline::after(Duration::from_secs(5));

    let lease = FrontendLease::register(
        &path,
        "tui-1".to_string(),
        ClientKind::Tui,
        LeaseMode::Business,
        &deadline,
    )
    .expect("a fresh guardian must accept a business registration");
    assert_eq!(lease.grant().mode, LeaseMode::Business);

    let channel = lease
        .open_managed_channel(&deadline)
        .expect("a business lease must be issued a channel");
    assert_eq!(
        channel.owner(),
        "tui-1",
        "the owner must come from the grant, not the request"
    );
    drop(channel);

    stop_guardian(&path, stop, server);
}

/// A `LeaseOnly` observer is refused, and the refusal names the reason.
///
/// This is not ceremony: a channel is the ability to start process groups, and the
/// lease mode is what says whether that client may start work at all. Accepting a
/// channel here would make the mode advisory.
#[test]
fn a_lease_only_observer_is_refused_a_channel() {
    let directory = TempDir::new().unwrap();
    let (path, stop, server) = start_guardian(&directory);
    let deadline = Deadline::after(Duration::from_secs(5));

    let lease = FrontendLease::register(
        &path,
        "observer".to_string(),
        // The App is the only client kind that asks for a non-business lease.
        ClientKind::App,
        LeaseMode::LeaseOnly,
        &deadline,
    )
    .expect("an observer still registers");
    assert_eq!(lease.grant().mode, LeaseMode::LeaseOnly);

    let error = lease
        .open_managed_channel(&deadline)
        .expect_err("an observer must never be issued a spawn channel");
    let text = error.to_string();
    assert!(
        text.contains("LeaseOnly") || text.contains("never authorizes a task"),
        "the refusal must say why, got {text}"
    );

    stop_guardian(&path, stop, server);
}

/// A released client's credential stops working, so it cannot open a channel with
/// an identity the guardian already withdrew.
#[test]
fn a_released_client_cannot_open_a_channel() {
    let directory = TempDir::new().unwrap();
    let (path, stop, server) = start_guardian(&directory);
    let deadline = Deadline::after(Duration::from_secs(5));

    let lease = FrontendLease::register(
        &path,
        "gone".to_string(),
        ClientKind::Headless,
        LeaseMode::Business,
        &deadline,
    )
    .expect("a business registration must be accepted");
    let grant = lease.grant().clone();
    lease.release(&deadline).expect("release must be accepted");

    // Ask directly, with the credential that was just withdrawn: this is exactly
    // what a stale client would do, and it must not be honoured.
    let asked = yi_agent_lifecycle::types::ControlRequest::OpenManagedChannel { grant };
    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    match client.request(&asked, &deadline).unwrap() {
        ControlResponse::Rejected { code, reason } => {
            assert!(
                matches!(
                    code,
                    ControlErrorCode::StaleEpoch | ControlErrorCode::Unauthenticated
                ),
                "expected an authentication refusal, got {code:?}: {reason}"
            );
        }
        other => panic!("a released credential must not receive a channel: {other:?}"),
    }

    stop_guardian(&path, stop, server);
}
