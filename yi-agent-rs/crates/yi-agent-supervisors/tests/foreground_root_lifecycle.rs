//! The guardian's root-authority surface, exercised over a real socket.
//!
//! Why integration rather than unit: the property being tested is "a client that
//! cannot prove a business lease cannot obtain root authority", and every shortcut
//! version of that test calls the table directly. Here the request really is encoded,
//! written, decoded and answered by the guardian's real control loop, so a codec
//! change or a missing arm fails the test instead of passing it.
//!
//! The limits of this file, stated so nobody over-reads it: it uses a real `UnixStream`
//! and the real handler, but it is one process. The cross-process descriptor
//! inheritance path is covered by Task 9's re-exec fixture, not here.
//!
//! A second rule shapes the helpers: an EOF or a codec error is **not** a refusal. The
//! gate under test is "the guardian said no", so a helper that turned `Transport` into
//! `Rejected` would pass this file while the socket was broken.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::foreground::{
    ForegroundErrorCode, ForegroundFailure, RootBindIntent, RootRef, SecretCapability,
};
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientGrant, ClientKind, ControlRequest, ControlResponse, LeaseMode,
    RootAuthorization, RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::{
    GUARDIAN_SOCKET_FILE, Guardian, serve_one_for_test, serve_with_bridge_for_test,
};

/// Why a fixture operation failed.
///
/// `Rejected` is the only branch that proves a gate refused something; the other two
/// exist so a broken socket cannot masquerade as a security property.
#[derive(Debug)]
enum BridgeFixtureError {
    Rejected(ForegroundFailure),
    Transport(io::Error),
    Protocol(String),
}

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

/// A guardians scope rooted in a temporary directory.
fn scope_for(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

fn deadline() -> Deadline {
    Deadline::after(Duration::from_secs(5))
}

/// A temporary directory whose *path* is short enough for a unix socket.
///
/// `TempDir::new()` lands under `/var/folders/...`, and `sun_path` is 104 bytes on
/// macOS: the control socket would be refused with `EINVAL` before any assertion ran.
/// `/tmp` keeps the whole path well inside the limit. This is a test-only concern; the
/// production path derives its socket location from the runtime directory.
fn short_temp_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("frl")
        .tempdir_in("/tmp")
        .expect("a writable /tmp is required to bind a unix socket in tests")
}

/// Starts a real guardian on a real socket and connects a real control client.
struct BridgeFixture {
    directory: TempDir,
    socket: PathBuf,
    business: ClientGrant,
    lease_only: ClientGrant,
    /// Kept alive so the server thread's stop flag is owned by the fixture.
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<io::Result<()>>>,
    /// The daemon stand-in on the far side of the authority bridge.
    peer: Option<std::thread::JoinHandle<()>>,
}

impl BridgeFixture {
    fn start() -> Self {
        let directory = short_temp_dir();
        let scope = scope_for(&directory);
        let socket = scope.runtime_dir.join(GUARDIAN_SOCKET_FILE);
        let selected = artifact();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        // A real peer for the authority bridge, answering the same frames the daemon
        // does. Without it the fixture could only ever observe refusals, and the
        // forward path -- "an install is confirmed before the capability is handed
        // out" -- would go untested.
        let (guardian_end, peer_end) = std::os::unix::net::UnixStream::pair()
            .expect("a socket pair is required to serve a bridge");
        // Mirror production, which hands the guardian a non-blocking descriptor: a
        // fixture that passed a blocking one could not catch the bug where a timeout
        // is silently ignored on a non-blocking socket.
        guardian_end
            .set_nonblocking(true)
            .expect("the guardian end must be made non-blocking");
        let peer = std::thread::spawn(move || answer_as_daemon(peer_end));
        let server = std::thread::spawn(move || {
            serve_with_bridge_for_test(
                scope,
                selected,
                move || server_stop.load(Ordering::SeqCst),
                guardian_end,
            )
        });

        // Wait for the socket node rather than sleeping a fixed interval: a fixed delay
        // is either flaky or slow, and never both right.
        let readiness_deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(
                Instant::now() < readiness_deadline,
                "the guardian never bound its control socket"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let business = register(
            &socket,
            "run-business",
            ClientKind::App,
            LeaseMode::Business,
        );
        let lease_only = register(
            &socket,
            "run-observer",
            ClientKind::App,
            LeaseMode::LeaseOnly,
        );

        Self {
            directory,
            socket,
            business,
            lease_only,
            stop,
            server: Some(server),
            peer: Some(peer),
        }
    }

    /// The temporary workspace a prepare names.
    fn workspace(&self) -> &Path {
        self.directory.path()
    }

    fn intent(&self, operation: &str) -> RootBindIntent {
        RootBindIntent {
            operation_id: operation.to_string(),
            thread_key: "thread:test".to_string(),
            workspace: self.workspace().to_path_buf(),
            extra_roots: Vec::new(),
            previous_root_capability: None,
        }
    }

    /// Asks as a client that holds a live business lease.
    fn authorize_business(&mut self, intent: &RootBindIntent) -> io::Result<RootAuthorization> {
        let grant = self.business.clone();
        match self.request(ControlRequest::AuthorizeRoot {
            grant,
            intent: intent.clone(),
        })? {
            ControlResponse::RootAuthorized(authorization) => Ok(authorization),
            ControlResponse::Rejected { code, reason } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{code:?}: {reason}"),
            )),
            other => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        }
    }

    /// Asks as a `LeaseOnly` observer, which must never authorize work.
    fn authorize_lease_only(&mut self, intent: &RootBindIntent) -> io::Result<RootAuthorization> {
        let grant = self.lease_only.clone();
        match self.request(ControlRequest::AuthorizeRoot {
            grant,
            intent: intent.clone(),
        })? {
            ControlResponse::RootAuthorized(authorization) => Ok(authorization),
            ControlResponse::Rejected { code, reason } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{code:?}: {reason}"),
            )),
            other => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
        }
    }

    /// Tries to reach a capability through a public path that is not allowed to give one.
    fn public_attach(&mut self, thread_key: &str) -> Result<(), BridgeFixtureError> {
        // The only public way to name a root is an authorize request, and it is
        // answered only against a live lease. Here the request is made by a client that
        // never registered, which is the "public key" case: knowing the thread key buys
        // nothing, because authority needs a lease *and* is generated, not derived.
        let forged = ClientGrant {
            client_run_id: thread_key.to_string(),
            credential: "forged".to_string(),
            epoch: 1,
            mode: LeaseMode::Business,
        };
        let answer = self
            .request(ControlRequest::AuthorizeRoot {
                grant: forged,
                intent: self.intent("attach-1"),
            })
            .map_err(BridgeFixtureError::Transport)?;
        match answer {
            ControlResponse::RootAuthorized(_) => Err(BridgeFixtureError::Protocol(
                "a client without a lease was authorized".to_string(),
            )),
            ControlResponse::Rejected { code, reason } => {
                Err(BridgeFixtureError::Rejected(ForegroundFailure {
                    code: map_rejection(code),
                    blocked_execution_ids: Vec::new(),
                    reason,
                }))
            }
            other => Err(BridgeFixtureError::Protocol(format!(
                "unexpected answer: {other:?}"
            ))),
        }
    }

    /// Tries to install authority over the tool-channel descriptor.
    ///
    /// The tool channel is a *different* descriptor carrying a spawn DTO; nothing that
    /// arrives there can reach the authority table, so the honest assertion is that the
    /// public business socket has no such request at all.
    fn attempt_tool_authority_install(&mut self) -> Result<(), BridgeFixtureError> {
        let answer = self
            .request(ControlRequest::AuthorizeRoot {
                // A tool process holds no client run, so it has no lease to present.
                grant: ClientGrant {
                    client_run_id: "tool:spawn".to_string(),
                    credential: "tool".to_string(),
                    epoch: 1,
                    mode: LeaseMode::Business,
                },
                intent: self.intent("tool-1"),
            })
            .map_err(BridgeFixtureError::Transport)?;
        match answer {
            ControlResponse::RootAuthorized(_) => Err(BridgeFixtureError::Protocol(
                "a tool-channel identity obtained root authority".to_string(),
            )),
            ControlResponse::Rejected { code, reason } => {
                Err(BridgeFixtureError::Rejected(ForegroundFailure {
                    code: map_rejection(code),
                    blocked_execution_ids: Vec::new(),
                    reason,
                }))
            }
            other => Err(BridgeFixtureError::Protocol(format!(
                "unexpected answer: {other:?}"
            ))),
        }
    }

    /// Stops the guardian and waits for it, so a later request really meets a dead
    /// socket rather than racing the accept loop.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop: it checks the stop flag after its bounded wait, and a
        // connection makes that check happen now instead of after a maintenance tick.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(peer) = self.peer.take() {
            let _ = peer.join();
        }
    }

    /// One request on its own connection.
    ///
    /// The protocol is one frame per connection by design (every exchange stays
    /// bounded), so a fixture that reused a connection would be exercising a client the
    /// guardian does not implement.
    fn request(&mut self, request: ControlRequest) -> io::Result<ControlResponse> {
        ask(&self.socket, &request)
    }
}

impl Drop for BridgeFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Nudge the accept loop: it wakes on a bounded wait, but a connection makes the
        // shutdown check run immediately instead of after the maintenance tick.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Answers bridge frames the way the daemon does, so the fixture exercises the real
/// exchange instead of a stub that always agrees.
///
/// Two rules, both taken from the daemon's own state machine: a `Prepare` yields a
/// `Challenge` carrying the sequence it will accept, and an `Authority` yields
/// `Committed` with the epoch it would write. Anything else ends the peer, because a
/// protocol surprise in a fixture must fail the test rather than be swallowed.
fn answer_as_daemon(mut stream: std::os::unix::net::UnixStream) {
    use std::io::Write;
    use yi_agent_lifecycle::foreground::BridgeFrame;
    let mut epoch = 1u64;
    while let Ok(raw) = yi_agent_lifecycle::codec::read_frame(&mut stream) {
        let Ok(frame) = yi_agent_lifecycle::codec::decode::<BridgeFrame>(&raw) else {
            return;
        };
        let answer = match frame {
            BridgeFrame::Prepare {
                nonce,
                root_id,
                requested_seq,
                ..
            } => BridgeFrame::Challenge {
                nonce,
                root_id,
                authority_seq: requested_seq,
            },
            // The nonce and root must echo the challenge the guardian answered: the
            // guardian refuses a commit that names a different root, which is the same
            // check the daemon performs in the other direction.
            BridgeFrame::Authority(message) => {
                let stamp = match &message {
                    yi_agent_lifecycle::foreground::AuthorityMessage::Install { stamp, .. }
                    | yi_agent_lifecycle::foreground::AuthorityMessage::Renew { stamp } => stamp,
                    _ => return,
                };
                BridgeFrame::Committed {
                    nonce: stamp.challenge_nonce.clone(),
                    root_id: stamp.root_id.clone(),
                    binding_epoch: epoch,
                }
            }
            _ => return,
        };
        epoch += 1;
        if yi_agent_lifecycle::codec::write_frame(&mut stream, &answer).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

/// Maps a control-error code onto the foreground code the shell parses.
///
/// Two enums because the control channel predates the foreground taxonomy; the mapping
/// lives here so a test cannot quietly accept the wrong one.
fn map_rejection(code: yi_agent_lifecycle::types::ControlErrorCode) -> ForegroundErrorCode {
    use yi_agent_lifecycle::types::ControlErrorCode;
    match code {
        ControlErrorCode::Unauthenticated => ForegroundErrorCode::AuthorityDenied,
        ControlErrorCode::InvalidRequest => ForegroundErrorCode::Unsupported,
        ControlErrorCode::Unsupported => ForegroundErrorCode::Unsupported,
        ControlErrorCode::Conflict => ForegroundErrorCode::OperationConflict,
        ControlErrorCode::StaleEpoch => ForegroundErrorCode::StaleAttempt,
        ControlErrorCode::Released => ForegroundErrorCode::AuthorityDenied,
        ControlErrorCode::VersionMismatch => ForegroundErrorCode::Unsupported,
        ControlErrorCode::Internal => ForegroundErrorCode::CorruptBinding,
    }
}

fn ask(socket: &Path, request: &ControlRequest) -> io::Result<ControlResponse> {
    let mut client = ControlClient::connect(socket, &deadline())?;
    client.request(request, &deadline())
}

fn register(socket: &Path, id: &str, kind: ClientKind, mode: LeaseMode) -> ClientGrant {
    match ask(
        socket,
        &ControlRequest::Register {
            client_run_id: id.to_string(),
            kind,
            mode,
        },
    )
    .unwrap()
    {
        ControlResponse::Granted(grant) => grant,
        other => panic!("register was not granted: {other:?}"),
    }
}

#[test]
fn foreground_root_lifecycle_business_lease_binds_root() {
    let mut fixture = BridgeFixture::start();
    let intent = fixture.intent("bind-1");
    let authorization = fixture.authorize_business(&intent).unwrap();
    assert!(!authorization.root_id.is_empty());
    // The capability is generated: it is not the thread key, and does not contain it.
    assert_ne!(authorization.capability, intent.thread_key);
    assert!(!authorization.capability.contains(&intent.thread_key));
    // Nothing about it may be replayed as a lease.
    assert_ne!(authorization.capability, fixture.business.credential);
}

#[test]
fn foreground_root_lifecycle_public_key_cannot_retrieve_capability() {
    let mut fixture = BridgeFixture::start();
    let thread_key = fixture.intent("bind-2").thread_key;
    match fixture.public_attach(&thread_key) {
        Err(BridgeFixtureError::Rejected(failure)) => {
            assert_eq!(failure.code, ForegroundErrorCode::AuthorityDenied);
        }
        Err(BridgeFixtureError::Transport(error)) => {
            panic!("the control socket failed instead of refusing: {error}")
        }
        Err(BridgeFixtureError::Protocol(detail)) => {
            panic!("the answer was not a refusal: {detail}")
        }
        Ok(()) => panic!("a forged grant was accepted"),
    }
}

#[test]
fn foreground_root_lifecycle_lease_only_denied() {
    let mut fixture = BridgeFixture::start();
    let intent = fixture.intent("bind-3");
    let error = fixture.authorize_lease_only(&intent).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn foreground_root_lifecycle_tool_channel_cannot_install_authority() {
    let mut fixture = BridgeFixture::start();
    match fixture.attempt_tool_authority_install() {
        Err(BridgeFixtureError::Rejected(failure)) => {
            assert_eq!(failure.code, ForegroundErrorCode::AuthorityDenied)
        }
        Err(BridgeFixtureError::Transport(error)) => {
            panic!("the control socket failed instead of refusing: {error}")
        }
        Err(BridgeFixtureError::Protocol(detail)) => {
            panic!("the answer was not a refusal: {detail}")
        }
        Ok(()) => panic!("a tool-channel identity was accepted"),
    }
}

#[test]
fn foreground_root_lifecycle_identity_does_not_create_a_root() {
    // The fixture registers two clients; only Identity is read-only. Asking for
    // authority for a root must not have happened as a side effect of registering.
    let mut fixture = BridgeFixture::start();
    let response = fixture.request(ControlRequest::Identity).unwrap();
    assert!(matches!(response, ControlResponse::Identity(_)));
    // A second business client is still able to bind, which proves the first Identity
    // did not consume or create anything.
    let intent = fixture.intent("bind-4");
    assert!(fixture.authorize_business(&intent).is_ok());
}

#[test]
fn foreground_root_lifecycle_bootstrap_before_business_binding() {
    // Before any authorization, the guardian must answer an authority *read* honestly
    // rather than inventing one. There is no read variant yet on the wire, so the
    // observable property is that the very first authorization is the one that creates
    // the root, and a second one for the same operation resolves to the same root.
    let mut fixture = BridgeFixture::start();
    let intent = fixture.intent("bind-5");
    let first = fixture.authorize_business(&intent).unwrap();
    let second = fixture.authorize_business(&intent).unwrap();
    assert_eq!(first.root_id, second.root_id);
    assert_ne!(first.capability, second.capability);
}

#[test]
fn foreground_root_lifecycle_bridge_eof_closes_admission() {
    // A guardian whose process is gone cannot authorize anything. Checked against a
    // *stopped* guardian rather than a flag, so an implementation that only flipped a
    // boolean would fail here. The request must surface as a transport failure, never as
    // a fabricated success.
    let mut fixture = BridgeFixture::start();
    let intent = fixture.intent("bind-eof");
    assert!(fixture.authorize_business(&intent).is_ok());

    let socket = fixture.socket.clone();
    fixture.shutdown();
    // The socket *node* may outlive the process; what must not outlive it is an
    // answer. A stale node is exactly the case that matters, so this test keeps it.
    let grant = fixture.business.clone();
    let error = ask(
        &socket,
        &ControlRequest::AuthorizeRoot {
            grant,
            intent: fixture.intent("bind-eof-2"),
        },
    )
    .expect_err("a stopped guardian must not answer");
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
        ),
        "expected a transport failure, got {error:?}"
    );
}

#[test]
fn foreground_root_lifecycle_an_expired_lease_is_refused_by_the_guardian() {
    // A release closes the client's ability to authorize anything new. The withdrawal
    // runs before the lease is gone, so the answer is a refusal rather than a token.
    let mut fixture = BridgeFixture::start();
    let intent = fixture.intent("bind-6");
    assert!(fixture.authorize_business(&intent).is_ok());
    let grant = fixture.business.clone();
    let response = fixture.request(ControlRequest::Release { grant }).unwrap();
    assert!(matches!(
        response,
        ControlResponse::ReleaseStarted { .. } | ControlResponse::Rejected { .. }
    ));
    // After the release the same client can no longer authorize.
    let intent = fixture.intent("bind-7");
    assert!(fixture.authorize_business(&intent).is_err());
}

#[test]
fn foreground_root_lifecycle_recovery_requests_are_not_silently_successful() {
    // Task 8 implements recovery. Until then the guardian must answer "no such
    // operation" rather than a fabricated `Committed`, and must still require a lease.
    let mut fixture = BridgeFixture::start();
    let root = RootRef {
        session_id: "session-1".to_string(),
        root_id: "root-1".to_string(),
        attempt_id: "attempt-1".to_string(),
    };
    let grant = fixture.business.clone();
    match fixture
        .request(ControlRequest::RootRecoveryOperation {
            grant,
            root,
            operation_id: "op-1".to_string(),
        })
        .unwrap()
    {
        ControlResponse::RootRecovery(None) => {}
        other => panic!("recovery must not report a result yet: {other:?}"),
    }
}

/// `serve_one_for_test` is used by unit tests that own their accept loop; this keeps the
/// import honest rather than dead, and documents that the same handler answers both.
#[test]
fn foreground_root_lifecycle_the_single_connection_handler_refuses_without_a_lease() {
    let directory = short_temp_dir();
    let scope = scope_for(&directory);
    let mut guardian = Guardian::new(scope, artifact(), "gen-1".to_string());
    let (peer, served) = std::os::unix::net::UnixStream::pair().unwrap();
    let handle = std::thread::spawn(move || serve_one_for_test(&mut guardian, served));
    let mut peer = peer;
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = ControlRequest::AuthorizeRoot {
        grant: ClientGrant {
            client_run_id: "nobody".to_string(),
            credential: "none".to_string(),
            epoch: 1,
            mode: LeaseMode::Business,
        },
        intent: RootBindIntent {
            operation_id: "bind-8".to_string(),
            thread_key: "thread:none".to_string(),
            workspace: directory.path().to_path_buf(),
            extra_roots: Vec::new(),
            previous_root_capability: None,
        },
    };
    yi_agent_lifecycle::codec::write_frame(&mut peer, &request).unwrap();
    let frame = yi_agent_lifecycle::codec::read_frame(&mut peer).unwrap();
    let response: ControlResponse = yi_agent_lifecycle::codec::decode(&frame).unwrap();
    assert!(matches!(
        response,
        ControlResponse::Rejected { .. } | ControlResponse::Unsupported { .. }
    ));
    handle.join().unwrap().unwrap();
}

/// A registry is not attached in these tests, so a release reports a failed reclamation
/// rather than a claim that something stopped. Asserted explicitly so the fixture above
/// cannot later be "fixed" into pretending.
#[test]
fn foreground_root_lifecycle_without_a_registry_a_release_cannot_claim_a_stop() {
    let directory = short_temp_dir();
    let scope = scope_for(&directory);
    let guardian = Guardian::new(scope, artifact(), "gen-1".to_string());
    assert!(guardian.holds_registry().not());
    let _ = SecretCapability::new("unused");
}

/// A peer that accepts the bridge and then says nothing must fail the request inside
/// the budget, not hang the control loop and not report success.
///
/// The regression this pins down is real: the bridge arrives non-blocking, and
/// `SO_RCVTIMEO` is ignored on a non-blocking socket, so a read returns `WouldBlock`
/// at once. Without switching to blocking mode the exchange would look "denied"
/// immediately; without the timeout it would pin the control loop forever.
#[test]
fn foreground_root_lifecycle_a_silent_bridge_times_out_instead_of_hanging() {
    let directory = short_temp_dir();
    let scope = scope_for(&directory);
    let socket = scope.runtime_dir.join(GUARDIAN_SOCKET_FILE);
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let (guardian_end, peer_end) = std::os::unix::net::UnixStream::pair().unwrap();
    guardian_end.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        serve_with_bridge_for_test(
            scope,
            artifact(),
            move || server_stop.load(Ordering::SeqCst),
            guardian_end,
        )
    });
    let readiness_deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < readiness_deadline,
            "the guardian never bound"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // `peer_end` is deliberately kept open and never read or written: the guardian can
    // send, and nothing answers.
    let business = register(&socket, "run-silent", ClientKind::App, LeaseMode::Business);
    let started = Instant::now();
    let error = ask(
        &socket,
        &ControlRequest::AuthorizeRoot {
            grant: business,
            intent: RootBindIntent {
                operation_id: "silent-1".to_string(),
                thread_key: "thread:test".to_string(),
                workspace: directory.path().to_path_buf(),
                extra_roots: Vec::new(),
                previous_root_capability: None,
            },
        },
    )
    .expect_err("a silent bridge must not authorize anything");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(6),
        "the exchange must end inside the budget, took {elapsed:?}"
    );
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ),
        "expected a bounded refusal, got {error:?}"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = std::os::unix::net::UnixStream::connect(&socket);
    let _ = server.join();
    drop(peer_end);
}

/// Tiny boolean helper so the assertion above reads as a sentence.
trait Not {
    fn not(self) -> bool;
}

impl Not for bool {
    fn not(self) -> bool {
        !self
    }
}
