//! One managed bring-up, decided under one lock.
//!
//! The failures these tests pin are all "decided too early": a probe that timed out
//! being read as absent, an inspection that created a runtime, two attaches each
//! starting a daemon. Every one of them looks like a successful start from inside
//! the process that did it.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeIdentity, RuntimeMode, RuntimeScope};
use yi_agent_supervisors::lifecycle::CoordinationGuard;
use yi_agent_supervisors::lifecycle::bringup::{
    BringUpIntent, BringUpResult, Compatibility, ControlProbe, GuardianLauncher, ProbeOutcome,
    classify_compatibility, ensure_managed_with, pin_artifact, verify_pinned,
};

fn test_scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

fn artifact(build_id: &str) -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: build_id.to_string(),
        display_version: "1.0.0".to_string(),
        executable: PathBuf::from("/usr/local/bin/yi-agent"),
        sha256: String::new(),
        schema_min: 1,
        schema_max: 20,
        handover_version: 1,
        business_protocol_min: 7,
        business_protocol_max: 7,
        capabilities: vec!["control.v1".to_string(), "lease.v1".to_string()],
    }
}

fn identity(build_id: &str, mode: RuntimeMode) -> RuntimeIdentity {
    RuntimeIdentity {
        scope_key: "/scope".to_string(),
        daemon_instance_id: Some("instance".to_string()),
        guardian_build_id: Some(build_id.to_string()),
        daemon_build_id: Some(build_id.to_string()),
        mode,
        business_protocol: Some(7),
        daemon_display_version: Some("1.0.0".to_string()),
        guardian_display_version: Some("1.0.0".to_string()),
        capabilities: vec!["control.v1".to_string(), "lease.v1".to_string()],
        schema_min: 1,
        schema_max: 20,
    }
}

/// Reports a scripted probe outcome and counts probes.
struct ScriptedProbe {
    outcomes: RefCell<Vec<ProbeOutcome>>,
    last: ProbeOutcome,
    probes: RefCell<usize>,
}

impl ScriptedProbe {
    fn always(outcome: ProbeOutcome) -> Self {
        Self {
            outcomes: RefCell::new(Vec::new()),
            last: outcome,
            probes: RefCell::new(0),
        }
    }

    /// Answers with each outcome in turn; the last one repeats.
    fn sequence(outcomes: Vec<ProbeOutcome>) -> Self {
        let last = outcomes.last().cloned().unwrap_or(ProbeOutcome::Absent);
        Self {
            outcomes: RefCell::new(outcomes),
            last,
            probes: RefCell::new(0),
        }
    }
}

impl ControlProbe for ScriptedProbe {
    fn probe(&self, _control_path: &Path, _deadline: &Deadline) -> ProbeOutcome {
        *self.probes.borrow_mut() += 1;
        let mut outcomes = self.outcomes.borrow_mut();
        if outcomes.is_empty() {
            self.last.clone()
        } else {
            outcomes.remove(0)
        }
    }
}

/// Records launches instead of starting processes.
#[derive(Default)]
struct RecordingLauncher {
    launches: RefCell<usize>,
    fail: bool,
}

impl GuardianLauncher for RecordingLauncher {
    fn launch(&self, _scope: &RuntimeScope, _expected: &ArtifactIdentity) -> std::io::Result<()> {
        *self.launches.borrow_mut() += 1;
        if self.fail {
            return Err(std::io::Error::other("launch refused"));
        }
        Ok(())
    }
}

/// Inspection must not create the runtime it is inspecting.
///
/// It runs on a UI poll; a runtime directory appearing because a user looked at a
/// version banner is the side effect this intent exists to rule out.
#[test]
fn inspection_cannot_create_runtime() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Inspect,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    assert!(matches!(result, BringUpResult::ManualActionRequired { .. }));
    assert!(
        !scope.runtime_dir.join("runtime.sqlite").exists(),
        "inspection created a database"
    );
    assert!(
        !scope.runtime_dir.join("coordination.lock").exists(),
        "inspection took the coordination lock, which creates the runtime directory"
    );
    assert_eq!(*launcher.launches.borrow(), 0);
}

/// A probe that does not answer is not "no runtime".
///
/// The expensive mistake is treating a timeout, a permission error, or a wedged
/// listener as absence and starting a second daemon on the same database.
#[test]
fn an_unresponsive_runtime_is_never_treated_as_absent() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Unresponsive(
        "timed out waiting for a reply".to_string(),
    ));
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    let BringUpResult::ManualActionRequired { reason } = result else {
        panic!("expected manual action, got {result:?}");
    };
    assert!(reason.contains("did not answer"), "{reason}");
    assert_eq!(*launcher.launches.borrow(), 0, "nothing may be started");
}

/// An absent runtime is started exactly once, under the lock.
#[test]
fn an_absent_runtime_is_started_once() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::sequence(vec![
        ProbeOutcome::Absent,
        ProbeOutcome::Absent,
        ProbeOutcome::Answered(identity("build-a", RuntimeMode::Managed)),
    ]);
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    let BringUpResult::Connected {
        identity,
        control_path,
    } = result
    else {
        panic!("expected a connection, got {result:?}");
    };
    assert_eq!(identity.daemon_build_id.as_deref(), Some("build-a"));
    assert!(control_path.ends_with("guardian-control.sock"));
    assert_eq!(*launcher.launches.borrow(), 1, "exactly one start");
}

/// A runtime that is already up is reused, never replaced by a second start.
///
/// Two clients opening the same project must converge on one daemon: the second
/// observes the first instead of starting a sibling that would fight it for the
/// instance lock (and, on the losing branch, for the database).
#[test]
fn a_runtime_that_appeared_is_reused_not_replaced() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let launcher = RecordingLauncher::default();

    // First client: nothing is running, so it starts one.
    let first = ScriptedProbe::sequence(vec![
        ProbeOutcome::Absent,
        ProbeOutcome::Answered(identity("build-a", RuntimeMode::Managed)),
    ]);
    let started = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &first,
        &launcher,
        &[],
    )
    .unwrap();
    assert!(
        matches!(started, BringUpResult::Connected { .. }),
        "{started:?}"
    );
    assert_eq!(*launcher.launches.borrow(), 1, "the first client starts it");

    // Second client: the peer is answering, so it reuses it.
    let second = ScriptedProbe::always(ProbeOutcome::Answered(identity(
        "build-a",
        RuntimeMode::Managed,
    )));
    let reused = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &second,
        &launcher,
        &[],
    )
    .unwrap();
    assert!(
        matches!(reused, BringUpResult::Connected { .. }),
        "{reused:?}"
    );
    assert_eq!(
        *launcher.launches.borrow(),
        1,
        "the second client must not start a second daemon"
    );
}

/// A launch failure is reported, not retried into a second start.
#[test]
fn a_failed_launch_reports_manual_action() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let launcher = RecordingLauncher {
        launches: RefCell::new(0),
        fail: true,
    };

    let result = ensure_managed_with(
        &scope,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    let BringUpResult::ManualActionRequired { reason } = result else {
        panic!("expected manual action, got {result:?}");
    };
    assert!(reason.contains("could not start"), "{reason}");
    assert_eq!(*launcher.launches.borrow(), 1);
}

/// A newer database schema stops the start before anything writes to it.
///
/// The daemon migrates the moment it opens the file, so "start it and see" would
/// run this build's migrations over data a newer build owns.
#[test]
fn a_future_schema_blocks_the_start() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let database = scope.runtime_dir.join("runtime.sqlite");
    std::fs::create_dir_all(&scope.runtime_dir).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT); \
             INSERT INTO schema_migrations (version, applied_at) VALUES (9999, 'now');",
        )
        .unwrap();
    drop(connection);

    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let launcher = RecordingLauncher::default();
    let result = ensure_managed_with(
        &scope,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    let BringUpResult::ManualActionRequired { reason } = result else {
        panic!("expected manual action, got {result:?}");
    };
    assert!(reason.contains("schema"), "{reason}");
    assert_eq!(*launcher.launches.borrow(), 0);
}

/// The start records what the runtime is supposed to run.
///
/// Without it, a crash between launch and handshake leaves a runtime nobody can
/// explain: no way to tell which build was meant to be running.
#[test]
fn a_start_records_the_selection() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-recorded");
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let launcher = RecordingLauncher::default();

    let _ = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_millis(300)),
        &probe,
        &launcher,
        &[],
    );

    let selection: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scope.runtime_dir.join("selection.json")).unwrap())
            .unwrap();
    assert_eq!(selection["target"]["build_id"], "build-recorded");
}

/// Same bytes is the only thing that does not need the user to decide.
#[test]
fn the_same_build_is_compatible() {
    let expected = artifact("build-a");
    assert_eq!(
        classify_compatibility(&identity("build-a", RuntimeMode::Managed), &expected, &[]),
        Compatibility::SameBuild
    );
}

/// A different build with the same business protocol needs a choice, not a refusal.
///
/// The distinction matters: a user who is told "incompatible" cannot use the
/// runtime at all, when in fact business works and only the version differs.
#[test]
fn a_different_build_with_the_same_protocol_requires_a_choice() {
    let expected = artifact("build-a");
    assert_eq!(
        classify_compatibility(&identity("build-b", RuntimeMode::Managed), &expected, &[]),
        Compatibility::CompatibleMismatch
    );
}

/// A legacy process is never "same build", even with identical bytes.
///
/// What it lacks is the lifecycle contract, which no build id can express.
#[test]
fn a_legacy_runtime_never_counts_as_the_same_build() {
    let expected = artifact("build-a");
    for mode in [RuntimeMode::LegacyIndependent, RuntimeMode::LegacyEmbedded] {
        assert_eq!(
            classify_compatibility(&identity("build-a", mode), &expected, &[]),
            Compatibility::LegacyTemporaryChoiceRequired,
            "{mode:?} must not be reported as current"
        );
    }
}

/// An unknown identity is never business-usable.
#[test]
fn an_unknown_identity_is_never_usable_for_business() {
    let expected = artifact("build-a");
    let mut unknown = identity("build-a", RuntimeMode::Unknown);
    assert_eq!(
        classify_compatibility(&unknown, &expected, &[]),
        Compatibility::Unknown
    );
    unknown.mode = RuntimeMode::Managed;
    unknown.daemon_build_id = None;
    assert_eq!(
        classify_compatibility(&unknown, &expected, &[]),
        Compatibility::Unknown
    );
}

/// A missing required capability is control-only, not "compatible".
#[test]
fn a_missing_capability_downgrades_to_control_only() {
    let expected = artifact("build-a");
    assert_eq!(
        classify_compatibility(
            &identity("build-a", RuntimeMode::Managed),
            &expected,
            &["switch.preview.v1".to_string()],
        ),
        Compatibility::ControlOnly
    );
}

/// A schema range that does not overlap is unknown, not "old".
///
/// "Old" would invite a migration; the truth is that this build cannot read it.
#[test]
fn a_disjoint_schema_range_is_unknown() {
    let expected = artifact("build-a");
    let mut newer = identity("build-a", RuntimeMode::Managed);
    newer.schema_min = 500;
    newer.schema_max = 600;
    assert_eq!(
        classify_compatibility(&newer, &expected, &[]),
        Compatibility::Unknown
    );
}

/// A non-exact match always surfaces as a decision for the user.
#[test]
fn a_compatible_mismatch_is_reported_as_a_version_choice() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Answered(identity(
        "build-b",
        RuntimeMode::Managed,
    )));
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();

    assert!(
        matches!(result, BringUpResult::VersionChoiceRequired { .. }),
        "{result:?}"
    );
    assert_eq!(*launcher.launches.borrow(), 0);
}

/// The pinned copy is private, keeps its bytes, and is what gets launched.
///
/// A bundle can be replaced after a selection is made; pointing the launcher at
/// the bundle path would silently change which bytes a committed selection means.
#[test]
fn the_pinned_artifact_is_a_verified_private_copy() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    std::fs::write(&source_path, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut source = artifact("build-pin");
    source.executable = source_path.clone();
    let pinned = pin_artifact(directory.path(), &source).unwrap();

    assert_ne!(pinned.path, source_path);
    assert_eq!(
        std::fs::read(&pinned.path).unwrap(),
        std::fs::read(&source_path).unwrap()
    );
    let mode = std::fs::metadata(&pinned.path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "only this user may run the pinned copy");
    assert!(!pinned.identity.sha256.is_empty());
    // The bundle disappearing does not invalidate a correct cache.
    std::fs::remove_file(&source_path).unwrap();
    assert_eq!(
        verify_pinned(directory.path(), &pinned.identity).unwrap(),
        pinned.path
    );
}

/// A cache that no longer matches its recorded digest is an error.
///
/// Repairing it from the bundle or `PATH` would run bytes the selection never
/// approved, which is how an "upgrade" quietly becomes a downgrade.
#[test]
fn a_tampered_pinned_artifact_is_refused() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    std::fs::write(&source_path, b"original").unwrap();
    let mut source = artifact("build-tamper");
    source.executable = source_path;

    let pinned = pin_artifact(directory.path(), &source).unwrap();
    std::fs::write(&pinned.path, b"replaced behind our back").unwrap();

    let error = verify_pinned(directory.path(), &pinned.identity).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

/// Pinning a source that changed since the identity was computed is refused.
#[test]
fn a_changed_source_during_pinning_is_refused() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    std::fs::write(&source_path, b"the bytes we hashed").unwrap();
    let mut source = artifact("build-changed");
    source.executable = source_path;
    source.sha256 = "0000000000000000000000000000000000000000000000000000000000000000".to_string();

    let error = pin_artifact(directory.path(), &source).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        !directory
            .path()
            .join("artifacts/build-changed/yi-agent")
            .exists(),
        "a rejected pin must not leave a half-trusted file behind"
    );
}

/// A lease-only caller still gets the runtime, without claiming business rights.
#[test]
fn a_lease_only_caller_connects_without_a_business_claim() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let probe = ScriptedProbe::always(ProbeOutcome::Answered(identity(
        "build-a",
        RuntimeMode::Managed,
    )));
    let launcher = RecordingLauncher::default();
    let result = ensure_managed_with(
        &scope,
        &artifact("build-a"),
        BringUpIntent::LeaseOnly,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &launcher,
        &[],
    )
    .unwrap();
    assert!(
        matches!(result, BringUpResult::Connected { .. }),
        "{result:?}"
    );
}

// ---- one directory, one scope ----------------------------------------------

/// Two configuration instances pointed at one runtime directory must not share it.
///
/// The failure this prevents is silent: both would open one `runtime.sqlite` with
/// different model catalogues and different `YI_AGENT_HOME` roots, so a session
/// started under one configuration would be served by the other's daemon.
#[test]
fn a_runtime_owned_by_another_instance_is_refused() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let home_a = directory.path().join("home-a");
    let home_b = directory.path().join("home-b");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_b).unwrap();

    let first =
        yi_agent_supervisors::lifecycle::scope::resolve(&runtime, &project, &home_a).unwrap();
    let second =
        yi_agent_supervisors::lifecycle::scope::resolve(&runtime, &project, &home_b).unwrap();
    assert_eq!(first.key, second.key, "one directory is one scope key");

    let launcher = RecordingLauncher::default();
    // The first client claims the directory and starts the runtime.
    let started = ensure_managed_with(
        &first,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::sequence(vec![
            ProbeOutcome::Absent,
            ProbeOutcome::Absent,
            ProbeOutcome::Answered(identity("build-a", RuntimeMode::Managed)),
        ]),
        &launcher,
        &["control.v1".to_string()],
    )
    .unwrap();
    assert!(
        matches!(started, BringUpResult::Connected { .. }),
        "{started:?}"
    );
    assert_eq!(*launcher.launches.borrow(), 1);

    // The second client must be told, not given a second daemon.
    let refused = ensure_managed_with(
        &second,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &launcher,
        &["control.v1".to_string()],
    )
    .unwrap();
    let BringUpResult::ManualActionRequired { reason } = refused else {
        panic!("a foreign instance must be refused, got {refused:?}");
    };
    assert!(reason.contains("instance home"), "{reason}");
    assert_eq!(
        *launcher.launches.borrow(),
        1,
        "a refused client must not start a second daemon"
    );
}

/// Two projects pointed at one runtime directory are refused the same way.
#[test]
fn a_runtime_owned_by_another_project_is_refused() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let project_a = directory.path().join("a");
    let project_b = directory.path().join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();

    let first =
        yi_agent_supervisors::lifecycle::scope::resolve(&runtime, &project_a, directory.path())
            .unwrap();
    let second =
        yi_agent_supervisors::lifecycle::scope::resolve(&runtime, &project_b, directory.path())
            .unwrap();

    let launcher = RecordingLauncher::default();
    ensure_managed_with(
        &first,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &launcher,
        &[],
    )
    .unwrap();
    let refused = ensure_managed_with(
        &second,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &launcher,
        &[],
    )
    .unwrap();
    let BringUpResult::ManualActionRequired { reason } = refused else {
        panic!("a foreign project must be refused, got {refused:?}");
    };
    assert!(reason.contains("project"), "{reason}");
    assert_eq!(*launcher.launches.borrow(), 1);
}

/// A lock held by a peer, with no socket yet, is a start in progress — not an
/// absent runtime, and not an invitation to start another one.
#[test]
fn a_peer_starting_the_runtime_is_waited_for() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let launcher = RecordingLauncher::default();

    // Hold the coordination lock as a peer would while its guardian binds.
    let peer = CoordinationGuard::acquire(&scope, Deadline::after(Duration::from_secs(1))).unwrap();

    let worker = {
        let scope = scope.clone();
        std::thread::spawn(move || {
            ensure_managed_with(
                &scope,
                &artifact("build-a"),
                BringUpIntent::Business,
                // Long enough to observe the wait, short enough to fail the test
                // fast if the loop never yields.
                Deadline::after(Duration::from_secs(3)),
                &ScriptedProbe::sequence(vec![
                    ProbeOutcome::Unresponsive("socket not bound yet".to_string()),
                    ProbeOutcome::Answered(identity("build-a", RuntimeMode::Managed)),
                ]),
                &RecordingLauncher::default(),
                &[],
            )
        })
    };

    std::thread::sleep(Duration::from_millis(150));
    drop(peer);

    let result = worker.join().unwrap().unwrap();
    assert!(
        matches!(result, BringUpResult::Connected { .. }),
        "the peer's runtime must be adopted, got {result:?}"
    );
    assert_eq!(
        *launcher.launches.borrow(),
        0,
        "the waiting client has no launcher of its own to use"
    );
}

/// A selection recorded for one build must not be satisfied by another build that
/// merely shares the business protocol.
#[test]
fn a_changed_bundle_requires_an_explicit_choice() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-b");

    let result = ensure_managed_with(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Answered(identity(
            "build-a",
            RuntimeMode::Managed,
        ))),
        &RecordingLauncher::default(),
        &["control.v1".to_string()],
    )
    .unwrap();

    let BringUpResult::VersionChoiceRequired { identity } = result else {
        panic!("a different build must not be used silently, got {result:?}");
    };
    assert_eq!(identity.daemon_build_id.as_deref(), Some("build-a"));
}
