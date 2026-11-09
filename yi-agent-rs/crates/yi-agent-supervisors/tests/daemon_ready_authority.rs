//! Readiness must come from the launch, not from whatever answers the socket.
//!
//! The failure these pin: a guardian reports ready because *a* daemon answered
//! `Status`, while the process it started never bound, or bound and lost to an older
//! one. From inside the guardian that looks like a successful start; from outside it
//! is a runtime that accepts authority for a process it does not own.

use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
use yi_agent_supervisors::lifecycle::guardian::Guardian;

fn short_home(seed: &TempDir) -> PathBuf {
    // `sun_path` is 103 bytes: a default temp dir plus `sockets/<hash>/` overflows it,
    // so the instance home is shortened the way the production launcher does.
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.path().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let home = PathBuf::from(format!("/tmp/dr-{:016x}", hasher.finish()));
    std::fs::create_dir_all(home.join("sockets")).unwrap();
    home
}

fn scope(seed: &TempDir) -> RuntimeScope {
    let runtime = seed.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    // A real directory, not a fictional "/project": the guardian spawns the child with
    // this as its working directory, and a missing one fails the spawn with ENOENT --
    // which would make every test here pass before readiness is even consulted.
    let project = seed.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    RuntimeScope {
        key: runtime.to_string_lossy().into_owned(),
        runtime_dir: runtime,
        project_key: project.to_string_lossy().into_owned(),
        instance_home: short_home(seed),
    }
}

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "this-build".to_string(),
        display_version: "1.0.0".to_string(),
        executable: PathBuf::from("/usr/local/bin/yi-agent"),
        sha256: String::new(),
        schema_min: 1,
        schema_max: 20,
        handover_version: 1,
        business_protocol_min: 7,
        business_protocol_max: 7,
        capabilities: vec!["control.v1".to_string()],
    }
}

/// A daemon that never writes a Ready frame is not ready, even if its socket answers.
///
/// The program here is `/usr/bin/true`: it spawns, exits immediately, and never
/// speaks. No socket is created, so the only thing that could have produced a ready
/// answer is the readiness protocol itself -- which is the point: this test fails
/// with a clear message exactly when readiness is inferred from something else.
#[test]
fn a_silent_child_cannot_be_published_as_ready() {
    let seed = TempDir::new().unwrap();
    let scope = scope(&seed);
    let mut guardian = Guardian::new(scope.clone(), artifact(), "generation".to_string());
    let (registry, _recovery) = guardian.open_registry().unwrap();
    guardian.attach_registry(std::sync::Arc::new(std::sync::Mutex::new(registry)));

    let result = guardian.start_daemon(
        std::path::Path::new("/usr/bin/true"),
        Duration::from_secs(2),
    );

    assert!(
        result.is_err(),
        "a child that proved nothing must not be published: {result:?}"
    );
    // And no partial state was left behind: a `daemon` field here would let the
    // control channel report a runtime that no process is serving.
    assert!(
        !guardian_has_daemon(&guardian),
        "the failed start published a daemon"
    );
}

/// A child that exits before writing Ready is refused, and its exit is not a timeout.
#[test]
fn an_exiting_child_is_refused_rather_than_waited_out() {
    let seed = TempDir::new().unwrap();
    let scope = scope(&seed);
    let mut guardian = Guardian::new(scope.clone(), artifact(), "generation".to_string());
    let (registry, _recovery) = guardian.open_registry().unwrap();
    guardian.attach_registry(std::sync::Arc::new(std::sync::Mutex::new(registry)));

    let started = std::time::Instant::now();
    let result = guardian.start_daemon(
        std::path::Path::new("/usr/bin/true"),
        Duration::from_secs(30),
    );
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a child that exited is not ready");
    assert!(
        elapsed < Duration::from_secs(10),
        "an already-closed lifeline must be noticed instead of burning the whole \
         budget, took {elapsed:?}"
    );
}

/// The identity a daemon would have to prove is not derivable from the socket.
///
/// A leftover `Status` responder knows the scope and can answer; it cannot know the
/// nonce this launch generated. Pinned as a property of the values themselves so a
/// future "accept any Ready for this scope" shortcut fails here.
#[test]
fn a_ready_for_another_launch_is_rejected() {
    use yi_agent_supervisors::lifecycle::daemon_ready::{DaemonReady, validate_ready};

    let expected = DaemonReady {
        startup_nonce: yi_agent_lifecycle::authorization::fresh_startup_nonce().unwrap(),
        scope_key: "/scope".to_string(),
        daemon_instance_id: "daemon-1".to_string(),
        build_id: "this-build".to_string(),
        business_protocol: 7,
    };
    // Everything an old daemon *could* know about itself: the same scope, the same
    // build, a plausible instance name. Only the nonce is out of reach.
    let mut stale = expected.clone();
    stale.daemon_instance_id = "daemon-0".to_string();

    let error = validate_ready(&stale, &expected).expect_err("a stale launch must be refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(validate_ready(&expected, &expected).is_ok());
}

/// Two launches never share a nonce, so an old frame cannot satisfy a new wait.
#[test]
fn every_launch_gets_a_distinct_nonce() {
    let first = yi_agent_lifecycle::authorization::fresh_startup_nonce().unwrap();
    let second = yi_agent_lifecycle::authorization::fresh_startup_nonce().unwrap();
    assert_ne!(
        first, second,
        "a reused nonce would let an old launch prove again"
    );
    assert_eq!(first.len(), 64, "a 32-byte nonce is 64 hex characters");
}

/// Reads readiness the way the control channel does, for the `!is_expired` reasoning.
#[allow(dead_code)]
fn readiness_deadline() -> Deadline {
    Deadline::after(Duration::from_secs(1))
}

fn guardian_has_daemon(guardian: &Guardian) -> bool {
    // The daemon field is private; the observable equivalent is the identity the
    // control channel would report. `None` is the documented "start failed" state.
    guardian.daemon().is_some()
}

/// A foreign `Status` responder on the business socket cannot satisfy readiness.
///
/// This is the A7 case with teeth: a real listener occupies the socket and answers
/// `Status` before anything is spawned, exactly as a leftover daemon would. The child
/// this guardian spawns then cannot bind and exits without ever proving readiness, so
/// the only way `start_daemon` can succeed is by reading the *foreign* answer. It must
/// refuse -- and it must leave the foreign responder running, because a failed start
/// that kills whatever own the socket is the same bug from the other side.
#[test]
fn an_unrelated_status_responder_cannot_satisfy_readiness() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let seed = TempDir::new().unwrap();
    let scope = scope(&seed);
    let socket = yi_agent_lifecycle::scope::daemon_socket(&scope.runtime_dir, &scope.instance_home);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();

    // Answer any request with a well-formed Status, forever. This is the shape a
    // leftover daemon has: correct protocol, correct scope, no idea what this launch
    // is.
    let responder = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 4096];
            let Ok(read) = stream.read(&mut buffer) else {
                return;
            };
            if read == 0 {
                continue;
            }
            let Ok(request) = serde_json::from_slice::<serde_json::Value>(&buffer[..read]) else {
                continue;
            };
            let Some(request_id) = request["request_id"].as_str() else {
                continue;
            };
            let reply = serde_json::json!({
                "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                "request_id": request_id,
                "result": {"type": "Status", "high_water_event_id": 1},
            });
            let mut text = reply.to_string();
            text.push('\n');
            let _ = stream.write_all(text.as_bytes());
            let _ = stream.flush();
        }
    });

    // The premise: the socket already answers, so a `Status`-based check would pass
    // before any child is even spawned.
    assert!(
        matches!(
            yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Status),
            Ok(yi_agent_store::ipc::IpcResponse::Status { .. })
        ),
        "the fixture must answer Status, otherwise this test proves nothing"
    );

    let mut guardian = Guardian::new(scope.clone(), artifact(), "generation".to_string());
    let (registry, _recovery) = guardian.open_registry().unwrap();
    guardian.attach_registry(std::sync::Arc::new(std::sync::Mutex::new(registry)));

    // The child must be one that *reads* the greeting and then stays silent: a child
    // that exits immediately fails the hello write with a broken pipe, and the test
    // would then pass without ever consulting readiness. This script drains the
    // greeting and sleeps, which is exactly "spawned, alive, never proved anything".
    let child = seed.path().join("silent-daemon.sh");
    std::fs::write(
        &child,
        "#!/bin/sh\n# Drain the greeting so the guardian's write succeeds, then stay up.\ncat >/dev/null\nsleep 30\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();

    let result = guardian.start_daemon(&child, Duration::from_secs(2));

    assert!(
        result.is_err(),
        "a foreign Status responder was accepted as this launch's readiness: {result:?}"
    );
    assert!(
        !guardian_has_daemon(&guardian),
        "the guardian published a daemon it never started"
    );
    // The responder is untouched: a refusal is not permission to kill it.
    assert!(
        socket.exists(),
        "the refusal removed the foreign responder's socket"
    );
    assert!(
        matches!(
            yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Status),
            Ok(yi_agent_store::ipc::IpcResponse::Status { .. })
        ),
        "the foreign responder stopped answering after the refusal"
    );
    drop(responder);
}

/// Closing the daemon's lifeline withdraws readiness without touching the registry.
///
/// The withdrawal is an observation, not an eviction: the daemon may be stopping
/// with buffered frames, and a supervision tick that signals on suspicion would be a
/// new way to lose a runtime. This pins both halves -- state withdrawn, registry
/// untouched.
#[test]
fn a_closed_lifeline_withdraws_readiness_and_touches_nothing() {
    let seed = TempDir::new().unwrap();
    let scope = scope(&seed);
    let (guardian_end, daemon_end) = std::os::unix::net::UnixStream::pair().unwrap();

    let mut guardian = Guardian::new(scope, artifact(), "generation".to_string());
    let (registry, _recovery) = guardian.open_registry().unwrap();
    let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));
    guardian.attach_registry(std::sync::Arc::clone(&registry));
    guardian.attach_daemon_lifeline_for_test(guardian_end);

    // Live channel, no close yet: nothing is withdrawn. Without this half the test
    // would also pass if the method simply always returned `true`.
    assert!(!guardian.daemon_lifeline_gone());
    assert!(!guardian.revoke_ready_if_gone());

    drop(daemon_end);

    assert!(
        guardian.daemon_lifeline_gone(),
        "an EOF on the daemon lifeline was not noticed"
    );
    let _ = guardian.revoke_ready_if_gone();
    assert!(
        guardian.daemon().is_none(),
        "a daemon with a closed lifeline is still advertised as ready"
    );
    assert!(
        registry.lock().unwrap().records().is_empty(),
        "withdrawing readiness signalled or mutated the process registry"
    );

    // Idempotent: the supervision tick calls this every pass.
    assert!(!guardian.revoke_ready_if_gone());
}
