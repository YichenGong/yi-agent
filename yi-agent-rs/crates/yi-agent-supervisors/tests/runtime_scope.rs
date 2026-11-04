//! Canonical runtime scope, the fixed-inode coordination lock, and the build
//! selection record.
//!
//! Every hazard here is about identity: the same directory reached by two paths,
//! the same lock file replaced underneath a holder, a selection record that must
//! not be mistaken for "no selection". Each test uses real files and real
//! symlinks, because the bugs these guard against do not exist in a mock.

use std::path::Path;
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, OperationStatus, SwitchPhase};
use yi_agent_supervisors::lifecycle::CoordinationGuard;
use yi_agent_supervisors::lifecycle::scope::{ScopeExt, resolve};
use yi_agent_supervisors::lifecycle::selection::{
    RuntimeSelection, read_selection, write_selection,
};

fn budget(millis: u64) -> Deadline {
    Deadline::after(Duration::from_millis(millis))
}

fn artifact(build_id: &str) -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: build_id.into(),
        display_version: "0.1.13".into(),
        executable: "/Applications/yi-agent.app/Contents/MacOS/yi-agent".into(),
        sha256: String::new(),
        schema_min: 0,
        schema_max: 15,
        handover_version: 1,
        business_protocol_min: 2,
        business_protocol_max: 2,
        capabilities: vec!["control.v1".into()],
    }
}

// ---- canonical scope ------------------------------------------------------

#[test]
fn a_missing_runtime_dir_resolves_against_its_nearest_existing_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let runtime = project.join(".yi-agent/runtime"); // does not exist yet

    let scope = resolve(&runtime, &project, dir.path()).unwrap();

    assert!(scope.runtime_dir.is_absolute());
    assert!(
        scope.runtime_dir.ends_with(".yi-agent/runtime"),
        "{scope:?}"
    );
    // Canonical, not merely absolute: the tempdir itself may sit behind a symlink
    // (`/var` -> `/private/var` on macOS), and two spellings of one directory would
    // otherwise look like two runtimes.
    assert_eq!(
        scope.runtime_dir,
        std::fs::canonicalize(dir.path())
            .unwrap()
            .join("project/.yi-agent/runtime")
    );
}

#[test]
fn two_spellings_of_one_directory_produce_one_key() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let direct = resolve(&real, &project, dir.path()).unwrap();
    let through_alias = resolve(&alias, &project, dir.path()).unwrap();

    assert_eq!(direct.runtime_dir, through_alias.runtime_dir);
    assert_eq!(direct.key, through_alias.key);
}

#[test]
fn a_different_runtime_dir_produces_a_different_key() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let a = resolve(&dir.path().join("one"), &project, dir.path()).unwrap();
    let b = resolve(&dir.path().join("two"), &project, dir.path()).unwrap();
    assert_ne!(a.key, b.key);
}

#[test]
fn one_override_dir_claimed_by_two_projects_is_a_conflict() {
    // The spec refuses ambiguous sharing: the same runtime directory cannot serve
    // two projects while claiming to be two independent scopes.
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared-runtime");
    let project_a = dir.path().join("a");
    let project_b = dir.path().join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();

    let a = resolve(&shared, &project_a, dir.path()).unwrap();
    let b = resolve(&shared, &project_b, dir.path()).unwrap();

    assert_eq!(a.key, b.key, "same directory must yield the same scope key");
    let conflict = a.conflict_with(&b).expect("two projects must conflict");
    assert!(
        conflict.contains("project"),
        "the conflict must name what differs: {conflict}"
    );
    assert!(a.conflict_with(&a).is_none());
}

#[test]
fn one_runtime_dir_claimed_by_two_instance_homes_is_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared-runtime");
    let project = dir.path().join("project");
    let home_a = dir.path().join("home-a");
    let home_b = dir.path().join("home-b");
    for path in [&project, &home_a, &home_b] {
        std::fs::create_dir_all(path).unwrap();
    }

    let a = resolve(&shared, &project, &home_a).unwrap();
    let b = resolve(&shared, &project, &home_b).unwrap();

    let conflict = a.conflict_with(&b).expect("two instances must conflict");
    assert!(conflict.contains("instance"), "{conflict}");
}

// ---- coordination lock ----------------------------------------------------

#[test]
fn one_scope_yields_exactly_one_holder() {
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();

    let first = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    let second = CoordinationGuard::acquire(&scope, budget(150));
    assert!(
        second.is_err(),
        "two coordinators must not believe they own one runtime"
    );
    drop(first);
    CoordinationGuard::acquire(&scope, budget(500)).expect("the lock is released on drop");
}

#[test]
fn a_symlink_alias_cannot_get_a_second_lock() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("runtime");
    std::fs::create_dir_all(&real).unwrap();
    let alias = dir.path().join("runtime-alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let direct = resolve(&real, &project, dir.path()).unwrap();
    let through_alias = resolve(&alias, &project, dir.path()).unwrap();
    // Resolution collapses the alias, so both scopes name one lock file. The
    // contention below is the consequence: were they to differ, the second
    // acquire would create a second lock in a directory that only looks different.
    assert_eq!(direct.runtime_dir, through_alias.runtime_dir);
    assert_eq!(direct.key, through_alias.key);

    let held = CoordinationGuard::acquire(&direct, budget(500)).unwrap();
    assert!(
        CoordinationGuard::acquire(&through_alias, budget(150)).is_err(),
        "two paths to one directory must contend for one lock"
    );
    // Same inode on disk, not merely the same string.
    let first_inode = inode(held.path());
    drop(held);
    assert_eq!(
        inode(&through_alias.runtime_dir.join("coordination.lock")),
        first_inode
    );
}

#[test]
fn the_lock_file_is_never_unlinked_and_keeps_its_inode() {
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();

    let guard = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    let path = guard.path().to_path_buf();
    let first = inode(&path);

    // Releasing must not delete the file: an acquirer that already opened this
    // inode would otherwise hold a lock on an unlinked file while a third process
    // created a fresh one, and two coordinators would both succeed.
    drop(guard);
    assert!(path.exists(), "the lock file must outlive its holder");
    assert_eq!(
        inode(&path),
        first,
        "the inode must be stable across holders"
    );

    let again = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    assert_eq!(inode(again.path()), first);
}

#[test]
fn the_returned_guard_locks_the_file_that_is_still_on_disk() {
    // If the file is replaced between open and flock, the descriptor we locked is
    // not the file a later acquirer will open. The guard must notice and retry.
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    let guard = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    assert_eq!(inode(guard.path()), inode_of(&guard));
}

#[test]
fn an_exhausted_budget_refuses_to_wait_instead_of_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    let _held = CoordinationGuard::acquire(&scope, budget(500)).unwrap();

    let started = std::time::Instant::now();
    let error = CoordinationGuard::acquire(&scope, budget(0)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a zero budget must not wait"
    );
}

// ---- build selection ------------------------------------------------------

fn selection(build_id: &str) -> RuntimeSelection {
    RuntimeSelection {
        target: artifact(build_id),
        last_operation: Some(OperationStatus {
            operation_id: "op-1".into(),
            phase: SwitchPhase::Ready,
            detail: None,
            ready_identity: None,
        }),
    }
}

#[test]
fn a_selection_round_trips_through_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    let guard = CoordinationGuard::acquire(&scope, budget(500)).unwrap();

    assert!(
        read_selection(&scope).unwrap().is_none(),
        "first start has none"
    );
    let written = selection("1123c23c8dde2e9c9b77941acf62ff56");
    write_selection(&guard, &written).unwrap();
    assert_eq!(read_selection(&scope).unwrap().unwrap(), written);
}

#[test]
fn a_corrupt_selection_is_an_error_not_an_empty_selection() {
    // Reporting `None` here would let a fresh start treat a broken record as "no
    // previous choice" and silently downgrade to whatever binary is convenient.
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    std::fs::create_dir_all(&scope.runtime_dir).unwrap();
    std::fs::write(scope.runtime_dir.join("selection.json"), b"{ not json").unwrap();

    let error = read_selection(&scope).expect_err("a corrupt record must not read as absent");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn the_selection_file_is_private_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    let guard = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    write_selection(&guard, &selection("1123c23c8dde2e9c9b77941acf62ff56")).unwrap();

    let path = scope.runtime_dir.join("selection.json");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600, "selection must not be world readable");

    let leftovers: Vec<_> = std::fs::read_dir(&scope.runtime_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("selection"))
        .collect();
    assert_eq!(
        leftovers,
        vec!["selection.json".to_string()],
        "{leftovers:?}"
    );
}

#[test]
fn the_selection_never_carries_credentials() {
    // `ClientGrant` is the only type allowed to carry a secret, and it must never
    // reach disk. Pin the absence structurally so a later field addition is caught.
    let dir = tempfile::tempdir().unwrap();
    let scope = resolve(&dir.path().join("runtime"), dir.path(), dir.path()).unwrap();
    let guard = CoordinationGuard::acquire(&scope, budget(500)).unwrap();
    write_selection(&guard, &selection("1123c23c8dde2e9c9b77941acf62ff56")).unwrap();

    let text = std::fs::read_to_string(scope.runtime_dir.join("selection.json")).unwrap();
    for forbidden in ["secret", "grant", "token", "password"] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "selection.json must not contain {forbidden}: {text}"
        );
    }
}

// ---- helpers --------------------------------------------------------------

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

fn inode_of(guard: &CoordinationGuard) -> u64 {
    use std::os::unix::fs::MetadataExt;
    guard.file_metadata().unwrap().ino()
}
