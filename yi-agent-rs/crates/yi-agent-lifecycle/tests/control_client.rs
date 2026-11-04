//! The control client must talk to a real Unix socket under one total budget, and
//! must refuse to start I/O once the budget is gone.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::time::Duration;

use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::codec;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ClientGrant, ClientKind, ControlRequest, ControlResponse, LeaseMode, RuntimeIdentity,
    RuntimeMode,
};

fn listener_in_tempdir() -> (tempfile::TempDir, UnixListener, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (dir, listener, path)
}

#[test]
fn identity_round_trips_over_a_real_socket() {
    let (_dir, listener, path) = listener_in_tempdir();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let frame = codec::read_frame(&mut stream).unwrap();
        let request: ControlRequest = codec::decode(&frame).unwrap();
        assert_eq!(request, ControlRequest::Identity);
        let identity = RuntimeIdentity {
            scope_key: "scope".into(),
            daemon_instance_id: Some("gen-1".into()),
            guardian_build_id: Some("build-g".into()),
            daemon_build_id: Some("build-d".into()),
            mode: RuntimeMode::Managed,
            business_protocol: Some(7),
            daemon_display_version: Some("0.1.13".into()),
            guardian_display_version: Some("0.1.13".into()),
            capabilities: vec!["control".into()],
            schema_min: 1,
            schema_max: 15,
        };
        codec::write_frame(&mut stream, &ControlResponse::Identity(identity)).unwrap();
    });

    let deadline = Deadline::after(Duration::from_secs(5));
    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    let response = client
        .request(&ControlRequest::Identity, &deadline)
        .unwrap();
    match response {
        ControlResponse::Identity(identity) => {
            assert_eq!(identity.daemon_instance_id.as_deref(), Some("gen-1"));
            assert_eq!(identity.mode, RuntimeMode::Managed);
        }
        other => panic!("expected identity, got {other:?}"),
    }
    server.join().unwrap();
}

#[test]
fn register_returns_a_grant_without_leaking_the_secret_in_debug() {
    let (_dir, listener, path) = listener_in_tempdir();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let frame = codec::read_frame(&mut stream).unwrap();
        let _: ControlRequest = codec::decode(&frame).unwrap();
        let grant = ClientGrant {
            client_run_id: "app-1".into(),
            credential: "super-secret-credential".into(),
            epoch: 1,
            mode: LeaseMode::Business,
        };
        codec::write_frame(&mut stream, &ControlResponse::Granted(grant)).unwrap();
    });

    let deadline = Deadline::after(Duration::from_secs(5));
    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    let response = client
        .request(
            &ControlRequest::Register {
                client_run_id: "app-1".into(),
                kind: ClientKind::App,
                mode: LeaseMode::Business,
            },
            &deadline,
        )
        .unwrap();
    let rendered = format!("{response:?}");
    assert!(!rendered.contains("super-secret-credential"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    server.join().unwrap();
}

#[test]
fn an_expired_budget_refuses_before_touching_the_socket() {
    let (_dir, _listener, path) = listener_in_tempdir();
    let deadline = Deadline::after(Duration::ZERO);
    let error = ControlClient::connect(&path, &deadline).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn a_server_that_never_answers_is_bounded_by_the_budget() {
    let (_dir, listener, path) = listener_in_tempdir();
    // Accept and then sit on the connection without replying: the client's read
    // must give up from its own budget, not the server's cooperation.
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        drop(stream);
    });

    let deadline = Deadline::after(Duration::from_millis(150));
    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    let started = std::time::Instant::now();
    let error = client
        .request(&ControlRequest::Identity, &deadline)
        .unwrap_err();
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "unexpected error kind: {error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    handle.join().unwrap();
}

#[test]
fn a_rejected_response_is_not_a_success() {
    let (_dir, listener, path) = listener_in_tempdir();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let frame = codec::read_frame(&mut stream).unwrap();
        let _: ControlRequest = codec::decode(&frame).unwrap();
        codec::write_frame(
            &mut stream,
            &ControlResponse::Unsupported {
                detail: "not implemented".into(),
            },
        )
        .unwrap();
        stream.flush().unwrap();
    });

    let deadline = Deadline::after(Duration::from_secs(5));
    let mut client = ControlClient::connect(&path, &deadline).unwrap();
    let response = client
        .request(&ControlRequest::Identity, &deadline)
        .unwrap();
    assert!(matches!(response, ControlResponse::Unsupported { .. }));
    server.join().unwrap();
}
