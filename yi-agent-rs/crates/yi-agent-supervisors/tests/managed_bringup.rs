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

mod support;

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeIdentity, RuntimeMode, RuntimeScope};
use yi_agent_supervisors::lifecycle::CoordinationGuard;
use yi_agent_supervisors::lifecycle::bringup::{
    BringUpIntent, BringUpResult, BusinessProbe, BusinessProbeOutcome, Compatibility, ControlProbe,
    GuardianLauncher, ProbeOutcome, classify_compatibility, ensure_managed_with,
    ensure_managed_with_business, pin_artifact, pinned_program, verify_pinned,
};

fn test_scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        // A `TempDir` under `/var/folders` is longer than `sun_path` allows once the
        // socket directory and hash are appended, and the derived socket path is then
        // *refused* rather than probed -- a real behaviour, but not what these tests
        // are about. The short home keeps the derived path usable.
        instance_home: support::short_instance_home(directory.path()),
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

/// Reports a scripted business-channel outcome and counts probes.
struct ScriptedBusinessProbe {
    last: BusinessProbeOutcome,
    probes: RefCell<usize>,
}

impl ScriptedBusinessProbe {
    fn always(outcome: BusinessProbeOutcome) -> Self {
        Self {
            last: outcome,
            probes: RefCell::new(0),
        }
    }
}

impl BusinessProbe for ScriptedBusinessProbe {
    fn probe(&self, _scope: &RuntimeScope, _deadline: &Deadline) -> BusinessProbeOutcome {
        *self.probes.borrow_mut() += 1;
        self.last.clone()
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

/// Inspection reports an unsupervised daemon instead of "nothing is running".
///
/// The two answers lead a user in opposite directions: "nothing" invites a start,
/// and a start here is what puts a second daemon on the database. Inspection is
/// still read-only -- it takes no lock and creates no runtime.
#[test]
fn inspection_reports_an_unsupervised_business_daemon() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Answered);
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with_business(
        &scope,
        &expected,
        BringUpIntent::Inspect,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &business,
        &launcher,
        &[],
    )
    .unwrap();

    let BringUpResult::ManualActionRequired { reason } = result else {
        panic!("inspection must report the daemon it cannot supervise, got {result:?}");
    };
    assert!(
        reason.contains("without a supervisor"),
        "the reason must name what was found: {reason}"
    );
    assert_eq!(*business.probes.borrow(), 1);
    assert_eq!(*launcher.launches.borrow(), 0);
    assert!(
        !scope.runtime_dir.join("runtime.sqlite").exists(),
        "inspection created a database"
    );
    assert!(
        !scope.runtime_dir.join("coordination.lock").exists(),
        "inspection took the coordination lock"
    );
}

/// A business daemon that is alive but has no guardian must stop the start.
///
/// This is the A6 case: the user's own `daemon serve` is running and answering on
/// the business socket, but nothing supervises it. Starting a managed runtime here
/// would put a second daemon on the same database, and the loser would be whichever
/// one the user was already using. The answer is a manual step, never a launch.
#[test]
fn business_live_without_guardian_never_launches() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    // No control channel: nothing supervised is running.
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    // But the business socket answers: an unsupervised daemon is alive.
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Answered);
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with_business(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &business,
        &launcher,
        &[],
    )
    .unwrap();

    assert!(
        matches!(result, BringUpResult::ManualActionRequired { .. }),
        "a live business daemon must be reported, not replaced: {result:?}"
    );
    assert_eq!(
        *launcher.launches.borrow(),
        0,
        "no second daemon may be started beside a live business daemon"
    );
    assert!(
        !scope.runtime_dir.join("runtime.sqlite").exists(),
        "refusing must not create the database it refused to touch"
    );
}

/// A business probe that cannot answer is not "nothing is there".
///
/// The distinction decides between "start one" and "stop and ask": reading a
/// timeout or a permission error as absence is how a live daemon gets replaced.
#[test]
fn business_unresponsive_is_not_absent() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Unresponsive(
        "timed out waiting for Status".to_string(),
    ));
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with_business(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &business,
        &launcher,
        &[],
    )
    .unwrap();

    assert!(
        matches!(result, BringUpResult::ManualActionRequired { .. }),
        "an unresponsive business channel must be refused, not read as absent: {result:?}"
    );
    assert_eq!(*launcher.launches.borrow(), 0);
}

/// The positive witness: with both channels absent, a normal start still happens.
///
/// Without this, the two tests above would be satisfied by a function that never
/// launches anything at all.
#[test]
fn both_channels_absent_still_launches() {
    let directory = TempDir::new().unwrap();
    let scope = test_scope(&directory);
    let expected = artifact("build-a");
    let probe = ScriptedProbe::always(ProbeOutcome::Absent);
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Absent);
    let launcher = RecordingLauncher::default();

    let result = ensure_managed_with_business(
        &scope,
        &expected,
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(2)),
        &probe,
        &business,
        &launcher,
        &[],
    )
    .unwrap();

    assert_eq!(
        *launcher.launches.borrow(),
        1,
        "with nothing running a start is the right answer: {result:?}"
    );
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

/// Re-signing one build changes its bytes, not its build id. Both approved
/// artifacts must remain runnable; pinning the second must not replace the first.
#[test]
fn different_digests_of_one_build_keep_independent_pinned_artifacts() {
    use sha2::{Digest, Sha256};

    let home = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let path = source_dir.path().join("yi-agent");
    let mut source = artifact("same-build-different-signatures");
    source.executable = path.clone();
    std::fs::write(&path, b"CLI signature").unwrap();
    let first = pin_artifact(home.path(), &source).unwrap();

    let bytes = b"desktop signature";
    std::fs::write(&path, bytes).unwrap();
    source.sha256 = format!("{:x}", Sha256::digest(bytes));
    let second = pinned_program(home.path(), &source)
        .expect("a separately approved signature must not conflict with the CLI cache");

    assert_ne!(first.path, second);
    assert_eq!(std::fs::read(&first.path).unwrap(), b"CLI signature");
    assert_eq!(std::fs::read(&second).unwrap(), bytes);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        verify_pinned(home.path(), &first.identity).unwrap(),
        first.path
    );
    assert_eq!(verify_pinned(home.path(), &source).unwrap(), second);
}

/// The old build-only cache may contain another signature. Leave it untouched
/// and stage the approved source separately, rather than weakening verification.
#[test]
fn a_legacy_build_only_cache_does_not_block_an_approved_artifact() {
    use sha2::{Digest, Sha256};

    let home = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let old = home.path().join("artifacts/build-legacy-cache/yi-agent");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"previous signature").unwrap();
    let bytes = b"approved desktop signature";
    let path = source_dir.path().join("yi-agent");
    std::fs::write(&path, bytes).unwrap();
    let mut expected = artifact("build-legacy-cache");
    expected.executable = path;
    expected.sha256 = format!("{:x}", Sha256::digest(bytes));

    let program = pinned_program(home.path(), &expected)
        .expect("the legacy cache must not reserve every signature of this build");
    assert_ne!(program, old);
    assert_eq!(std::fs::read(&program).unwrap(), bytes);
    assert_eq!(std::fs::read(&old).unwrap(), b"previous signature");
}

/// An upgrade must retain a previously approved private copy after its bundle is
/// gone; migration verifies those bytes again and leaves the old copy untouched.
#[test]
fn a_verified_legacy_cache_survives_the_original_bundle_disappearing() {
    use sha2::{Digest, Sha256};

    let home = TempDir::new().unwrap();
    let bytes = b"previously approved signature";
    let old = home.path().join("artifacts/build-migrate/yi-agent");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, bytes).unwrap();
    let mut expected = artifact("build-migrate");
    expected.executable = home.path().join("deleted-bundle/yi-agent");
    expected.sha256 = format!("{:x}", Sha256::digest(bytes));

    let program = pinned_program(home.path(), &expected)
        .expect("an approved legacy private copy must survive a layout upgrade");
    assert_ne!(program, old);
    assert_eq!(std::fs::read(&program).unwrap(), bytes);
    assert_eq!(std::fs::read(&old).unwrap(), bytes);
    assert_eq!(verify_pinned(home.path(), &expected).unwrap(), program);
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
    assert_eq!(
        std::fs::read_dir(directory.path().join("artifacts/build-changed"))
            .unwrap()
            .count(),
        0,
        "a rejected pin must not leave staged or temporary files behind"
    );
}

/// A first start for a build has no pinned copy yet, and that must not be fatal.
///
/// Regression (measured 2026-11-10): `record_selection` writes the intent but
/// nothing pins the bytes, so on a fresh install `artifacts/<build_id>/` does not
/// exist at all. The launcher then asked for the pinned copy, got `NotFound` from a
/// directory nobody created, and reported it as `could not start the managed
/// runtime`. The managed runtime never started, so the App never registered a lease,
/// the guardian reported zero clients, the daemon refused the project attach naming
/// an unauthorized client, and `spawn_agent` was never advertised.
///
/// Staging the copy is not a fallback to the bundle: `pin_artifact` refuses to
/// finish unless the digest it computed equals the recorded one, so the launched
/// bytes are the approved bytes.
#[test]
fn a_first_start_stages_the_pinned_artifact_it_was_never_given() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    let bytes = b"#!/bin/sh\nexit 0\n";
    std::fs::write(&source_path, bytes).unwrap();
    std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };

    let mut expected = artifact("build-first-start");
    expected.executable = source_path.clone();
    expected.sha256 = digest;

    assert!(
        !directory.path().join("artifacts").exists(),
        "the failure needs a home that has never staged anything"
    );

    let program = pinned_program(directory.path(), &expected).expect("a first start must not fail");
    assert_ne!(
        program, source_path,
        "the launched program must be the pinned copy, not the bundle"
    );
    assert_eq!(std::fs::read(&program).unwrap(), bytes);
    assert_eq!(
        program,
        directory
            .path()
            .join("artifacts/build-first-start")
            .join(&expected.sha256)
            .join("yi-agent")
    );
    // The second start reuses the copy instead of staging it again.
    assert_eq!(
        pinned_program(directory.path(), &expected).unwrap(),
        program
    );
}

/// A staged copy that disagrees with the selection is not silently replaced.
///
/// This is the case `verify_pinned`'s comment is about: the selection's bytes
/// changed underneath a committed runtime. Overwriting it would let an in-place
/// upgrade pass as if nothing happened.
#[test]
fn a_staged_copy_that_disagrees_is_refused_rather_than_overwritten() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    std::fs::write(&source_path, b"the approved bytes").unwrap();

    let mut expected = artifact("build-disagree");
    expected.executable = source_path;
    let pinned = pin_artifact(directory.path(), &expected).unwrap();
    std::fs::write(&pinned.path, b"someone replaced this").unwrap();
    expected.sha256 = pinned.identity.sha256;

    let error = pinned_program(directory.path(), &expected).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read(&pinned.path).unwrap(),
        b"someone replaced this",
        "a disagreement must be reported, not repaired"
    );
}

/// A source that does not match the selection is refused on the first start too.
///
/// Staging must not become a hole that lets unapproved bytes through just because
/// no copy existed yet.
#[test]
fn a_first_start_refuses_a_source_the_selection_did_not_approve() {
    let directory = TempDir::new().unwrap();
    let source_dir = TempDir::new().unwrap();
    let source_path = source_dir.path().join("yi-agent");
    std::fs::write(&source_path, b"not the approved bytes").unwrap();

    let mut expected = artifact("build-unapproved");
    expected.executable = source_path;
    expected.sha256 =
        "0000000000000000000000000000000000000000000000000000000000000000".to_string();

    let error = pinned_program(directory.path(), &expected).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read_dir(directory.path().join("artifacts/build-unapproved"))
            .unwrap()
            .count(),
        0,
        "an unapproved source must leave no staged or temporary files"
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
    // Injected for the same reason as the project case: the temporary path is too
    // long for a derived socket, and absence is what this test needs anyway.
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Absent);
    // The first client claims the directory and starts the runtime.
    let started = ensure_managed_with_business(
        &first,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::sequence(vec![
            ProbeOutcome::Absent,
            ProbeOutcome::Absent,
            ProbeOutcome::Answered(identity("build-a", RuntimeMode::Managed)),
        ]),
        &business,
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
    let refused = ensure_managed_with_business(
        &second,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &business,
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
    // The business channel is injected rather than derived: these scopes sit under
    // a deep temporary path, where the derived socket path is refused by length --
    // a real behaviour, but not the one this test is about.
    let business = ScriptedBusinessProbe::always(BusinessProbeOutcome::Absent);
    ensure_managed_with_business(
        &first,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &business,
        &launcher,
        &[],
    )
    .unwrap();
    let refused = ensure_managed_with_business(
        &second,
        &artifact("build-a"),
        BringUpIntent::Business,
        Deadline::after(Duration::from_secs(5)),
        &ScriptedProbe::always(ProbeOutcome::Absent),
        &business,
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

/// The real probe, against real sockets: only "nothing listening" may be absence.
///
/// The injected-probe tests above pin the *decision*; this one pins the
/// *classification* that feeds it. Without it, a probe that calls every error
/// "absent" would satisfy them and silently reintroduce the second-daemon start.
mod business_channel_probe {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use tempfile::TempDir;
    use yi_agent_lifecycle::deadline::Deadline;
    use yi_agent_lifecycle::types::RuntimeScope;
    use yi_agent_supervisors::lifecycle::bringup::{
        BusinessChannelProbe, BusinessProbe, BusinessProbeOutcome,
    };

    /// A scope whose derived business socket lives under `home`.
    fn scope_for(home: &std::path::Path, runtime: &std::path::Path) -> RuntimeScope {
        RuntimeScope {
            key: runtime.to_string_lossy().into_owned(),
            runtime_dir: runtime.to_path_buf(),
            project_key: "/project".to_string(),
            instance_home: home.to_path_buf(),
        }
    }

    fn short_home_path(seed: &TempDir) -> std::path::PathBuf {
        let home = crate::support::short_instance_home(seed.path());
        std::fs::create_dir_all(home.join("sockets")).unwrap();
        home
    }

    /// No socket file at all is the one honest "absent".
    #[test]
    fn a_missing_socket_is_absent() {
        let seed = TempDir::new().unwrap();
        let home = short_home_path(&seed);
        let runtime = seed.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let outcome = BusinessChannelProbe.probe(
            &scope_for(&home, &runtime),
            &Deadline::after(Duration::from_secs(2)),
        );
        assert_eq!(outcome, BusinessProbeOutcome::Absent);
    }

    /// A listener that accepts and never answers is *not* absent.
    ///
    /// This is the classification the whole check rests on: a wedged daemon that
    /// reads as absent gets replaced by a second one.
    #[test]
    fn a_silent_listener_is_unresponsive_not_absent() {
        let seed = TempDir::new().unwrap();
        let home = short_home_path(&seed);
        let runtime = seed.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let scope = scope_for(&home, &runtime);

        let socket = yi_agent_lifecycle::scope::daemon_socket(&runtime, &home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let held = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });

        let outcome =
            BusinessChannelProbe.probe(&scope, &Deadline::after(Duration::from_millis(300)));
        assert!(
            matches!(outcome, BusinessProbeOutcome::Unresponsive(_)),
            "a silent listener must be unresponsive, got {outcome:?}"
        );
        drop(held);
    }

    /// A listening socket that answers a real Status is `Answered`.
    #[test]
    fn a_status_answer_is_answered() {
        let seed = TempDir::new().unwrap();
        let home = short_home_path(&seed);
        let runtime = seed.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let scope = scope_for(&home, &runtime);

        let socket = yi_agent_lifecycle::scope::daemon_socket(&runtime, &home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&buffer[..read]).unwrap();
            let request_id = request["request_id"].as_str().unwrap().to_string();
            let result = serde_json::to_value(yi_agent_store::ipc::IpcResponse::Status {
                high_water_event_id: 7,
                live_apply_version: None,
            })
            .unwrap();
            let reply = serde_json::json!({
                "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                "request_id": request_id,
                "result": result,
            })
            .to_string();
            stream.write_all(reply.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        });

        let outcome = BusinessChannelProbe.probe(&scope, &Deadline::after(Duration::from_secs(3)));
        assert_eq!(outcome, BusinessProbeOutcome::Answered);
        worker.join().unwrap();
    }
}
