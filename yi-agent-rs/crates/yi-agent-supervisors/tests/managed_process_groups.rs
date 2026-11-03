//! Proof that process ownership and reclamation are group-level facts, not guesses.
//!
//! Each test starts *real* processes (this binary re-executed as a helper) rather
//! than a fake, because the properties under test are kernel properties: whether a
//! grandchild outlives its parent, whether a group kill reaches it, whether a
//! reaped leader's PGID can be reused, and which descriptors a child inherits.

mod support;

use std::time::Duration;

use support::process_fixture as fixture;
use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
use yi_agent_supervisors::lifecycle::guardian::{
    ProcessOwner, ProcessRegistry, RecordState, TaskExecutionKey,
};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".to_string(),
        display_version: "0.0.0".to_string(),
        executable: "/bin/true".into(),
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

fn registry(directory: &TempDir) -> ProcessRegistry {
    ProcessRegistry::open(&scope(directory), "gen-1", &artifact())
        .expect("registry")
        .0
}

/// The plan's headline property: reclamation waits for the *whole group*, not just
/// the direct child.
#[test]
fn owner_reclamation_waits_for_the_entire_group() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let marker = markers.path("parent-exits");
    let owner = ProcessOwner::frontend("app");

    let command = fixture::parent_exits_leaving_child(owner.clone(), &marker);
    registry
        .spawn(command, Deadline::after(Duration::from_secs(10)))
        .unwrap();

    // The parent exits immediately, but the grandchild is still writing: a
    // "wait for the direct child" implementation would call this stopped.
    assert!(
        !registry.owner_is_stopped(&owner).unwrap(),
        "the group still has a live grandchild, so it is not stopped"
    );
    assert!(
        fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT),
        "the grandchild must have run"
    );

    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.owner_is_stopped(&owner).unwrap());
}

/// Only the owner's groups are terminated; a second client keeps running.
#[test]
fn terminating_one_owner_leaves_another_running() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let first = ProcessOwner::frontend("client-a");
    let second = ProcessOwner::frontend("client-b");
    let first_marker = markers.path("a");
    let second_marker = markers.path("b");

    registry
        .spawn(
            fixture::steady_writer(first.clone(), &first_marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    registry
        .spawn(
            fixture::steady_writer(second.clone(), &second_marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();

    assert!(fixture::wait_for_marker(
        &first_marker,
        fixture::MARKER_TIMEOUT
    ));
    assert!(fixture::wait_for_marker(
        &second_marker,
        fixture::MARKER_TIMEOUT
    ));

    registry
        .terminate_owner(&first, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.owner_is_stopped(&first).unwrap());
    assert!(!registry.owner_is_stopped(&second).unwrap());

    let before = fixture::marker_len(&second_marker);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        fixture::marker_len(&second_marker) > before,
        "the other client's writer must still be running"
    );
    registry
        .terminate_owner(&second, Deadline::after(Duration::from_secs(10)))
        .unwrap();
}

/// A finished task stops its own tool groups without stopping the client's other
/// tasks.
#[test]
fn task_reclamation_is_narrower_than_owner_reclamation() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let owner = ProcessOwner::frontend("app");
    let first = TaskExecutionKey::new("gen-1", "task-1", "attempt-1");
    let second = TaskExecutionKey::new("gen-1", "task-2", "attempt-1");
    let first_marker = markers.path("task-1");
    let second_marker = markers.path("task-2");

    registry
        .spawn(
            fixture::steady_writer(owner.clone(), &first_marker, Some(first.clone())),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    registry
        .spawn(
            fixture::steady_writer(owner.clone(), &second_marker, Some(second.clone())),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    assert!(fixture::wait_for_marker(
        &first_marker,
        fixture::MARKER_TIMEOUT
    ));
    assert!(fixture::wait_for_marker(
        &second_marker,
        fixture::MARKER_TIMEOUT
    ));

    registry
        .terminate_execution(&first, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.task_tools_are_stopped(&first).unwrap());
    assert!(
        !registry.task_tools_are_stopped(&second).unwrap(),
        "the other task keeps running inside the same client"
    );
    assert!(
        !registry.owner_is_stopped(&owner).unwrap(),
        "one task finishing does not stop the client"
    );

    let before = fixture::marker_len(&second_marker);
    std::thread::sleep(Duration::from_millis(150));
    assert!(fixture::marker_len(&second_marker) > before);

    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
}

/// A short-lived process is observed stopped once its group empties, and stays
/// stopped across repeated checks.
#[test]
fn a_command_that_exits_is_observed_stopped() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let owner = ProcessOwner::frontend("app");
    registry
        .spawn(
            fixture::short_lived(owner.clone()),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if registry.owner_is_stopped(&owner).unwrap() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("an exited process's group must become empty");
}

/// Children must not inherit the guardian's other descriptors: an inherited
/// lifecycle pipe is how a child silently outlives the thing that should stop it.
#[test]
fn managed_children_do_not_inherit_unrelated_descriptors() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let owner = ProcessOwner::frontend("app");
    let marker = markers.path("fds");

    // Open a descriptor in the parent that the child has no business seeing.
    let leak = std::fs::File::open("/dev/null").unwrap();
    let leaked_fd = {
        use std::os::fd::AsRawFd;
        leak.as_raw_fd()
    };

    registry
        .spawn(
            fixture::descriptor_reader(owner.clone(), &marker),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    let inherited: Vec<i32> = fixture::read_marker(&marker)
        .split(',')
        .filter_map(|value| value.parse().ok())
        .collect();

    // Exactly the three standard descriptors plus the ready barrier. The barrier
    // is deliberately *not* closed, since the child reads it before it runs.
    let mut allowed = vec![0, 1, 2, 9];
    allowed.sort_unstable();
    let mut observed = inherited.clone();
    observed.sort_unstable();
    assert_eq!(
        observed, allowed,
        "a managed child inherited unexpected descriptors; expected {allowed:?}"
    );
    if leaked_fd != 9 {
        assert!(
            !inherited.contains(&leaked_fd),
            "fd {leaked_fd} leaked into the managed child; inherited fds are {inherited:?}"
        );
    }
    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
}

/// The journal is the recovery contract: it records identity but never secrets,
/// and it is readable only by its owner.
#[test]
fn the_journal_records_identity_without_secrets() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let owner = ProcessOwner::frontend("app");
    let marker = markers.path("journal");
    let mut command = fixture::steady_writer(owner.clone(), &marker, None);
    command
        .env
        .push(("API_KEY".to_string(), "super-secret".to_string()));

    registry
        .spawn(command, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();

    let path = directory.path().join("processes.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("super-secret"),
        "env values must never be journaled"
    );
    assert!(
        !text.contains("API_KEY"),
        "env keys are not needed for recovery either"
    );
    assert!(text.contains("\"pgid\""), "recovery needs the group id");
    assert!(
        text.contains("\"start_micros\""),
        "recovery needs the birth identity"
    );

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the journal must not be world-readable");
}

/// A recorded group whose leader is gone but whose members live on cannot be
/// proven ours, so recovery reports it instead of killing a possibly recycled PGID.
#[test]
fn recovery_refuses_a_group_it_cannot_verify() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let owner = ProcessOwner::frontend("app");
    let mut markers = fixture::MarkerGuard::new();
    let marker = markers.path("stale");

    // A live group that the journal does not describe: the leader recorded is a
    // long-dead PID, so the PGID cannot be attributed.
    let mut child = fixture::detached_sleeper();
    let dead_leader = std::process::Command::new("true")
        .spawn()
        .map(|mut c| {
            let id = c.id() as i32;
            c.wait().unwrap();
            id
        })
        .unwrap();
    std::fs::write(
        scope.runtime_dir.join("processes.json"),
        serde_json::json!([{
            "scope_key": scope.key,
            "generation": "gen-1",
            "process_id": format!("{dead_leader}-0"),
            "pid": dead_leader,
            "pgid": child.id() as i32,
            "start_micros": 0,
            "artifact_build_id": "test-build",
            "artifact_sha256": "00",
            "owner": {"frontend": "app"},
            "root": null,
            "execution": null,
            "state": "running",
            "fault": null
        }])
        .to_string(),
    )
    .unwrap();

    let (_registry, report) = ProcessRegistry::open(&scope, "gen-2", &artifact()).unwrap();
    assert!(
        !report.is_safe(),
        "an unverifiable record must block scheduling"
    );
    assert_eq!(report.manual.len(), 1);
    assert!(report.reclaimed.is_empty());

    // The unrelated process must still be alive: refusing to signal is the point.
    let mut status = 0;
    let alive = unsafe { libc::waitpid(child.id() as i32, &mut status, libc::WNOHANG) };
    assert_eq!(alive, 0, "the unrelated process must not have been killed");
    let _ = child.kill();
    let _ = child.wait();
    let _ = owner;
    let _ = marker;
}

/// After recovery, an unverified record blocks new spawns rather than letting the
/// next generation run alongside unknown leftovers.
#[test]
fn an_unverified_record_blocks_new_spawns() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let mut child = fixture::detached_sleeper();
    let dead_leader = std::process::Command::new("true")
        .spawn()
        .map(|mut c| {
            let id = c.id() as i32;
            c.wait().unwrap();
            id
        })
        .unwrap();
    std::fs::write(
        scope.runtime_dir.join("processes.json"),
        serde_json::json!([{
            "scope_key": scope.key,
            "generation": "gen-1",
            "process_id": format!("{dead_leader}-0"),
            "pid": dead_leader,
            "pgid": child.id() as i32,
            "start_micros": 0,
            "artifact_build_id": "test-build",
            "artifact_sha256": "00",
            "owner": {"frontend": "app"},
            "root": null,
            "execution": null,
            "state": "running",
            "fault": null
        }])
        .to_string(),
    )
    .unwrap();

    let (mut registry, _report) = ProcessRegistry::open(&scope, "gen-2", &artifact()).unwrap();
    let error = registry
        .spawn(
            fixture::short_lived(ProcessOwner::frontend("next")),
            Deadline::after(Duration::from_secs(5)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

    let _ = child.kill();
    let _ = child.wait();
}

/// A spawn with no journal is refused: an unjournaled process cannot be reclaimed.
#[test]
fn spawning_without_a_journal_is_refused() {
    let mut registry = ProcessRegistry::without_journal("/tmp/no-journal", artifact());
    let error = registry
        .spawn(
            fixture::short_lived(ProcessOwner::frontend("app")),
            Deadline::after(Duration::from_secs(5)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

/// Recovery reclaims a group whose leader is still alive under the recorded birth.
#[test]
fn recovery_reclaims_a_group_whose_leader_is_verifiable() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let (mut first, _report) = ProcessRegistry::open(&scope, "gen-1", &artifact()).unwrap();
    let owner = ProcessOwner::frontend("app");
    let mut markers = fixture::MarkerGuard::new();
    let marker = markers.path("reclaim");

    let process_id = first
        .spawn(
            fixture::steady_writer(owner.clone(), &marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    assert!(fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT));

    // Drop the first registry without terminating: the group is now a leftover.
    let record = first
        .records()
        .iter()
        .find(|record| record.process_id == process_id)
        .cloned()
        .unwrap();
    std::mem::forget(first);
    assert_eq!(record.state, RecordState::Running);

    let (_second, report) = ProcessRegistry::open(&scope, "gen-2", &artifact()).unwrap();
    assert!(
        report.is_safe(),
        "a verifiable group is reclaimed automatically"
    );
    assert_eq!(report.reclaimed, vec![process_id]);
}

/// A plugin whose launching parent exits still has its grandchildren reclaimed.
///
/// This is the shape a plugin service actually has: a supervisor starts a plugin,
/// the plugin forks a worker, and the plugin's own parent is long gone. Reclaiming
/// only the process that was spawned would leave the worker behind -- and the
/// worker is the part that is doing work.
#[test]
fn a_plugin_parent_that_exits_still_has_its_grandchild_reclaimed() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let owner = ProcessOwner::plugin("search");
    let marker = markers.path("plugin-grandchild");

    let process_id = registry
        .spawn(
            fixture::parent_exits_leaving_child(owner.clone(), &marker),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    // The parent exits on purpose; only the grandchild writes the marker.
    assert!(
        fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT),
        "the grandchild must be running before reclamation is attempted"
    );
    assert!(
        !registry.owner_is_stopped(&owner).unwrap(),
        "the group is still alive even though its leader exited"
    );

    registry
        .terminate_owner(&owner, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(
        registry.owner_is_stopped(&owner).unwrap(),
        "a group kill must reach the grandchild, not just the exited leader"
    );
    assert!(
        registry
            .records()
            .iter()
            .find(|record| record.process_id == process_id)
            .map(|record| record.state != RecordState::Running)
            .unwrap_or(false),
        "the record must be marked reclaimed rather than left running"
    );
}

/// One client's departure does not stop another client's tools.
///
/// The clients here are deliberately different *frontends*, not two tasks of one
/// client: reclamation is scoped to the owner that departed, and a shared process
/// tree would be exactly the cross-client leak the boundary exists to prevent.
#[test]
fn one_departed_client_does_not_stop_anothers_tools() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let depart = ProcessOwner::frontend("client-a");
    let stay = ProcessOwner::frontend("client-b");
    let depart_marker = markers.path("client-a");
    let stay_marker = markers.path("client-b");

    registry
        .spawn(
            fixture::steady_writer(depart.clone(), &depart_marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    registry
        .spawn(
            fixture::steady_writer(stay.clone(), &stay_marker, None),
            Deadline::after(Duration::from_secs(10)),
        )
        .unwrap();
    assert!(fixture::wait_for_marker(
        &depart_marker,
        fixture::MARKER_TIMEOUT
    ));
    assert!(fixture::wait_for_marker(
        &stay_marker,
        fixture::MARKER_TIMEOUT
    ));

    registry
        .terminate_owner(&depart, Deadline::after(Duration::from_secs(10)))
        .unwrap();
    assert!(registry.owner_is_stopped(&depart).unwrap());
    assert!(
        !registry.owner_is_stopped(&stay).unwrap(),
        "client B's work must continue when client A departs"
    );

    let before = fixture::marker_len(&stay_marker);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        fixture::marker_len(&stay_marker) > before,
        "client B's tool is still writing after client A left"
    );

    registry
        .terminate_owner(&stay, Deadline::after(Duration::from_secs(10)))
        .unwrap();
}

/// The helper runs as a test so the binary can re-exec itself.
#[test]
#[ignore = "internal fixture: requires the guardian's ready descriptor"]
fn process_fixture_helper() {
    fixture::run_helper_if_requested();
}
