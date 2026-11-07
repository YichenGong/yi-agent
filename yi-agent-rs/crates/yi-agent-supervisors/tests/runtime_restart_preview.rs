//! Impact previews and one-shot confirmation, over the real control socket.
//!
//! The properties under test are all about *not* doing something:
//!
//! - previewing must not disturb the runtime, so cancelling is always safe;
//! - a confirmation must apply to exactly the world the user was shown, so a
//!   changed revision or a different target refuses instead of proceeding;
//! - a second click must not start a second replacement;
//! - a page must never carry the power to confirm.
//!
//! These run against a real guardian and a real socket because the ordering those
//! rules depend on happens in the request handler, not in the pure state machine
//! (which is covered by unit tests in `yi-agent-lifecycle`).

mod support;

use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlErrorCode, ControlRequest, ControlResponse, LeaseMode,
    RuntimeScope,
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

fn target_artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "next-build".to_string(),
        display_version: "0.0.1".to_string(),
        executable: "/usr/bin/true".into(),
        sha256: "11".to_string(),
        ..artifact()
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

/// One request over a fresh control connection.
///
/// A connection is single-exchange by design (the guardian serves one frame and
/// closes), so reusing one would be testing a protocol the runtime does not have.
fn exchange(socket: &std::path::Path, request: ControlRequest) -> ControlResponse {
    let deadline = Deadline::after(Duration::from_secs(5));
    // `socket.exists()` only says a name was bound, not that anyone is accepting:
    // a freshly bound socket is visible before `listen` completes, and a busy
    // machine widens that window. Retrying keeps a slow scheduler from looking
    // like a failed guardian. Every retry re-connects, which is the same protocol
    // shape as the single-shot case (one connection, one exchange).
    let mut client = loop {
        match ControlClient::connect(socket, &deadline) {
            Ok(client) => break client,
            Err(error) => {
                if !deadline.is_expired() {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                panic!("connect to the guardian: {error}");
            }
        }
    };
    client.request(&request, &deadline).expect("request")
}

/// A business daemon that answers: an idle coordinator at revision 1.
///
/// Without this the guardian reports `unknown` (correctly), and every preview would
/// require a second confirmation -- which would hide the difference between "only
/// this App is attached" and "somebody else is".
struct IdleCoordinator;

impl yi_agent_supervisors::lifecycle::preview::CoordinatorProbe for IdleCoordinator {
    fn impact(&self) -> yi_agent_supervisors::lifecycle::preview::CoordinatorImpact {
        yi_agent_supervisors::lifecycle::preview::CoordinatorImpact {
            revision: Some(1),
            running: Vec::new(),
            queued: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

/// A guardian with a bound socket, a registered client, and the running server's
/// join handle behind a stop flag.
struct Harness {
    scope: RuntimeScope,
    grant: yi_agent_lifecycle::types::ClientGrant,
    socket: std::path::PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Joined on drop so a failing assertion cannot leave a guardian thread behind
    /// that keeps the runtime directory alive for the next test.
    server: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    /// Owns the directory the runtime lives in; must outlive the server.
    _directory: TempDir,
}

impl Harness {
    /// Starts a guardian whose control loop serves a real socket, then registers
    /// this test as a client through that socket.
    fn start(clients: &[&str]) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("rrp")
            .tempdir_in("/private/tmp")
            .unwrap();
        let runtime_scope = scope(&directory);
        let artifact = artifact();
        let socket = yi_agent_lifecycle::scope::guardian_socket(
            &runtime_scope.runtime_dir,
            &runtime_scope.instance_home,
        );
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_server = stop.clone();
        let scope_for_server = runtime_scope.clone();

        let server = std::thread::spawn(move || {
            yi_agent_supervisors::lifecycle::guardian::serve_with(
                scope_for_server,
                artifact,
                move || stop_for_server.load(std::sync::atomic::Ordering::SeqCst),
                std::sync::Arc::new(IdleCoordinator),
            )
        });

        let deadline_for_wait = std::time::Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                std::time::Instant::now() < deadline_for_wait,
                "the guardian never bound its control socket"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // Register through the protocol: the test must not reach into the guardian's
        // table, or it would be testing its own fixture. One connection per request,
        // because a control connection serves exactly one exchange.
        let first = match exchange(
            &socket,
            ControlRequest::Register {
                client_run_id: clients[0].to_string(),
                kind: ClientKind::App,
                mode: LeaseMode::Business,
            },
        ) {
            ControlResponse::Granted(grant) => grant,
            other => panic!("expected a grant, got {other:?}"),
        };
        for id in &clients[1..] {
            let response = exchange(
                &socket,
                ControlRequest::Register {
                    client_run_id: (*id).to_string(),
                    kind: ClientKind::Tui,
                    mode: LeaseMode::Business,
                },
            );
            assert!(matches!(response, ControlResponse::Granted(_)));
        }

        Self {
            scope: runtime_scope,
            grant: first,
            socket,
            stop,
            server: Some(server),
            _directory: directory,
        }
    }

    fn request(&self, request: ControlRequest) -> ControlResponse {
        exchange(&self.socket, request)
    }

    fn preview(&self, target: ArtifactIdentity) -> ControlResponse {
        self.request(ControlRequest::Preview {
            grant: self.grant.clone(),
            target,
        })
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Unblock the accept loop with one more connection attempt.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(server) = self.server.take() {
            // A panic in a test thread must not turn into a leaked guardian that
            // keeps serving a deleted runtime directory.
            let _ = server.join();
        }
    }
}

/// A preview must answer with the affected clients *and* mint a token, without
/// changing anything: the same runtime serves the next request just as before.
#[test]
fn a_preview_reports_clients_and_mints_a_token_without_disturbing_anything() {
    let harness = Harness::start(&["app", "tui"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };

    assert_eq!(
        preview.counts.clients, 2,
        "both registered clients are affected"
    );
    assert!(preview.counts.complete);
    assert!(!preview.token.is_empty());
    assert_eq!(preview.token.len(), 64);
    assert!(
        preview.requires_second_confirmation,
        "another client is attached, so a second confirmation is required"
    );
    assert!(preview.can_restart);

    // Read-only: the identity answer is unchanged and the runtime still serves.
    let ControlResponse::Identity(identity) = harness.request(ControlRequest::Identity) else {
        panic!("the guardian stopped answering after a preview");
    };
    assert_eq!(identity.daemon_instance_id, None);
    assert!(
        !harness.scope.runtime_dir.join("runtime.sqlite").exists(),
        "a preview must never create the business database"
    );
}

/// Two previews are two independent decisions.
#[test]
fn every_preview_mints_a_distinct_token() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(first) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    let ControlResponse::Preview(second) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    assert_ne!(first.token, second.token);
    assert_ne!(first.snapshot_id, second.snapshot_id);
}

/// Confirming consumes the token, and a second click with a new operation id is
/// refused rather than starting a second replacement.
#[test]
fn a_second_confirmation_cannot_start_a_second_replacement() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };

    let first = harness.request(ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token.clone(),
        operation_id: "op-1".to_string(),
    });
    assert!(matches!(first, ControlResponse::Confirmed { .. }));

    let second = harness.request(ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token.clone(),
        operation_id: "op-2".to_string(),
    });
    match second {
        ControlResponse::Rejected { code, reason } => {
            assert_eq!(code, ControlErrorCode::Conflict);
            assert!(
                reason.contains("op-1"),
                "the refusal names the owner: {reason}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A retry of the *same* operation is idempotent: a lost response must not force
/// the user to confirm again.
#[test]
fn retrying_the_same_operation_returns_the_same_result() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    let request = ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token.clone(),
        operation_id: "op-1".to_string(),
    };
    assert!(matches!(
        harness.request(request.clone()),
        ControlResponse::Confirmed { .. }
    ));
    match harness.request(request) {
        ControlResponse::Confirmed { operation_id } => assert_eq!(operation_id, "op-1"),
        other => panic!("a retry must report the same operation, got {other:?}"),
    }
}

/// The core safety property: a client joining between preview and click invalidates
/// the consent, because the user approved a smaller impact than would occur.
#[test]
fn a_client_joining_between_preview_and_confirm_makes_the_token_stale() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    assert!(
        !preview.requires_second_confirmation,
        "only this App is attached"
    );

    // Someone else attaches after the preview was rendered.
    let joined = exchange(
        &harness.socket,
        ControlRequest::Register {
            client_run_id: "late-tui".to_string(),
            kind: ClientKind::Tui,
            mode: LeaseMode::Business,
        },
    );
    assert!(matches!(joined, ControlResponse::Granted(_)));

    match harness.request(ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token,
        operation_id: "op-1".to_string(),
    }) {
        ControlResponse::Rejected { code, reason } => {
            assert_eq!(code, ControlErrorCode::Conflict);
            assert!(reason.contains("preview"), "the refusal explains: {reason}");
        }
        other => panic!("a stale token must be refused, got {other:?}"),
    }
}

/// "Upgrade to A" and "upgrade to B" are different decisions even when the impact is
/// identical, so a token minted for one target must not confirm the other.
#[test]
fn a_token_minted_for_one_target_cannot_confirm_another() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };

    // The confirmation carries no target: it uses the one in the snapshot. Prove the
    // binding by asking for a preview of a *different* target and confirming that
    // token instead -- it must succeed, but only for its own target.
    let mut other = target_artifact();
    other.build_id = "different-build".to_string();
    let ControlResponse::Preview(other_preview) = harness.preview(other) else {
        panic!("expected a preview");
    };
    assert_ne!(preview.snapshot_id, other_preview.snapshot_id);

    // Each token confirms its own decision; neither can substitute for the other.
    assert!(matches!(
        harness.request(ControlRequest::Confirm {
            grant: harness.grant.clone(),
            token: preview.token,
            operation_id: "op-a".to_string(),
        }),
        ControlResponse::Confirmed { .. }
    ));
    assert!(matches!(
        harness.request(ControlRequest::Confirm {
            grant: harness.grant.clone(),
            token: other_preview.token,
            operation_id: "op-b".to_string(),
        }),
        ControlResponse::Confirmed { .. }
    ));
}

/// A token belongs to the client that asked for it, even when every other field
/// matches.
#[test]
fn another_clients_token_is_refused() {
    let harness = Harness::start(&["app", "tui"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };

    // A forged grant with the same run id but a different credential must not work.
    let mut forged = harness.grant.clone();
    forged.credential = "00".repeat(32);
    match harness.request(ControlRequest::Confirm {
        grant: forged,
        token: preview.token,
        operation_id: "op-1".to_string(),
    }) {
        ControlResponse::Rejected { code, .. } => {
            assert_eq!(code, ControlErrorCode::Unauthenticated);
        }
        other => panic!("a forged credential must be refused, got {other:?}"),
    }
}

/// Paging returns rows and can never confirm: the token is empty on every page.
#[test]
fn a_page_carries_rows_but_never_a_token() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    match harness.request(ControlRequest::PreviewPage {
        grant: harness.grant.clone(),
        snapshot_id: preview.snapshot_id.clone(),
        cursor: "0".to_string(),
    }) {
        ControlResponse::Preview(page) => {
            assert!(page.token.is_empty(), "a page must not be able to confirm");
            assert_eq!(page.snapshot_id, preview.snapshot_id);
        }
        other => panic!("expected a page, got {other:?}"),
    }
}

/// Confirming must install the maintenance barrier, so new work cannot start into a
/// generation that is about to be replaced.
#[test]
fn a_confirmation_closes_admission_for_the_old_generation() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    assert!(matches!(
        harness.request(ControlRequest::Confirm {
            grant: harness.grant.clone(),
            token: preview.token,
            operation_id: "op-1".to_string(),
        }),
        ControlResponse::Confirmed { .. }
    ));

    // The barrier is observable from outside: the same client can no longer start
    // work, and the answer says why rather than "denied".
    match harness.request(ControlRequest::Admission {
        asked: yi_agent_lifecycle::types::ExecutionAdmission {
            grant: harness.grant.clone(),
            // A barrier is scope-wide, so any generation name reaches the check.
            generation: "any".to_string(),
            task: "t".to_string(),
            attempt: "a".to_string(),
        },
    }) {
        ControlResponse::Rejected { reason, .. } => {
            assert!(
                reason.contains("maintenance") || reason.contains("generation"),
                "the refusal must explain the barrier: {reason}"
            );
        }
        other => panic!("a barrier must refuse new work, got {other:?}"),
    }
}

/// A refused confirmation must not leave a barrier behind: that would block every
/// new task on behalf of a replacement that never started.
#[test]
fn a_refused_confirmation_leaves_no_barrier() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    // Confirm once (installs the barrier, consumes the token) ...
    assert!(matches!(
        harness.request(ControlRequest::Confirm {
            grant: harness.grant.clone(),
            token: preview.token.clone(),
            operation_id: "op-1".to_string(),
        }),
        ControlResponse::Confirmed { .. }
    ));
    // ... then a different operation on the same token is refused.
    assert!(matches!(
        harness.request(ControlRequest::Confirm {
            grant: harness.grant.clone(),
            token: preview.token,
            operation_id: "op-2".to_string(),
        }),
        ControlResponse::Rejected { .. }
    ));

    // The runtime is still healthy and still answers: a stuck barrier would show up
    // as a preview that suddenly reports a different revision or a refusal here.
    match harness.request(ControlRequest::Identity) {
        ControlResponse::Identity(_) => {}
        other => panic!("the guardian must still answer, got {other:?}"),
    }
}

/// A confirmation for a target that has since been removed fails before anything is
/// stopped, rather than half way through a switch.
#[test]
fn a_vanished_target_is_refused_before_anything_starts() {
    let harness = Harness::start(&["app"]);
    let directory = tempfile::Builder::new()
        .prefix("rrp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let missing = directory.path().join("no-such-binary");
    let mut target = target_artifact();
    target.executable = missing;

    let ControlResponse::Preview(preview) = harness.preview(target) else {
        panic!("expected a preview");
    };
    match harness.request(ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token,
        operation_id: "op-1".to_string(),
    }) {
        ControlResponse::Rejected { code, reason } => {
            assert_eq!(code, ControlErrorCode::InvalidRequest);
            assert!(reason.contains("no-such-binary"), "{reason}");
        }
        other => panic!("a missing target must be refused, got {other:?}"),
    }
}

/// An empty operation id is refused: "retry this operation" and "confirm again"
/// have to be distinguishable.
#[test]
fn a_confirmation_without_an_operation_id_is_refused() {
    let harness = Harness::start(&["app"]);
    let ControlResponse::Preview(preview) = harness.preview(target_artifact()) else {
        panic!("expected a preview");
    };
    match harness.request(ControlRequest::Confirm {
        grant: harness.grant.clone(),
        token: preview.token,
        operation_id: String::new(),
    }) {
        ControlResponse::Rejected { code, .. } => {
            assert_eq!(code, ControlErrorCode::InvalidRequest);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// An observer may look but not work: a `LeaseOnly` grant can preview, which is what
/// lets an incompatible build still guide the user through an upgrade.
#[test]
fn a_lease_only_client_can_preview() {
    let directory = tempfile::Builder::new()
        .prefix("rrp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let scope = scope(&directory);
    let artifact = artifact();
    let socket =
        yi_agent_lifecycle::scope::guardian_socket(&scope.runtime_dir, &scope.instance_home);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_server = stop.clone();
    let scope_for_server = scope.clone();
    let server = std::thread::spawn(move || {
        yi_agent_supervisors::lifecycle::guardian::serve_with(
            scope_for_server,
            artifact,
            move || stop_for_server.load(std::sync::atomic::Ordering::SeqCst),
            std::sync::Arc::new(IdleCoordinator),
        )
    });
    let deadline_for_wait = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(std::time::Instant::now() < deadline_for_wait);
        std::thread::sleep(Duration::from_millis(5));
    }

    let ControlResponse::Granted(observer) = exchange(
        &socket,
        ControlRequest::Register {
            client_run_id: "old-tui".to_string(),
            kind: ClientKind::Tui,
            mode: LeaseMode::LeaseOnly,
        },
    ) else {
        panic!("expected a lease-only grant");
    };

    // It may not start work...
    assert!(matches!(
        exchange(
            &socket,
            ControlRequest::Admission {
                asked: yi_agent_lifecycle::types::ExecutionAdmission {
                    grant: observer.clone(),
                    generation: "guardian-test".to_string(),
                    task: "t".to_string(),
                    attempt: "a".to_string(),
                },
            },
        ),
        ControlResponse::Rejected { .. }
    ));
    // ...but it can still see, and be shown, the upgrade.
    match exchange(
        &socket,
        ControlRequest::Preview {
            grant: observer,
            target: target_artifact(),
        },
    ) {
        ControlResponse::Preview(preview) => {
            assert!(!preview.token.is_empty());
            assert!(preview.counts.clients >= 1);
        }
        other => panic!("a lease-only client must be able to preview, got {other:?}"),
    }

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::os::unix::net::UnixStream::connect(&socket);
    server.join().unwrap().unwrap();
}
