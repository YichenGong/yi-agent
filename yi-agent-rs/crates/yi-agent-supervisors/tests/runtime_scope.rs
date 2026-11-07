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

// ---- launcher identity ----------------------------------------------------

/// The launcher names the project explicitly instead of leaving the guardian to
/// infer it from its working directory.
///
/// This is the A19 failure mode made testable: the guardian's `current_dir()` is
/// the *runtime* directory, so a guardian that derived its project from the working
/// directory would resolve a scope belonging to whoever owns that directory. The
/// assertion is on the environment the launcher builds, because that environment is
/// the entire contract between the two processes.
#[test]
fn the_launcher_names_the_project_scope_and_instance_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let linked = dir.path().join("project-alias");
    std::os::unix::fs::symlink(&project, &linked).unwrap();
    let runtime = dir.path().join("runtime");
    let instance_home = dir.path().join("home");
    std::fs::create_dir_all(&instance_home).unwrap();

    // The scope is resolved through the *symlink* spelling; `resolve` must collapse
    // it to the canonical project before the launcher ever sees it.
    let scope = resolve(&runtime, &linked, &instance_home).unwrap();
    assert_eq!(
        scope.project_key,
        std::fs::canonicalize(&project).unwrap().to_string_lossy()
    );

    let launcher = yi_agent_supervisors::lifecycle::bringup::ProcessGuardian {
        instance_home: instance_home.clone(),
    };
    let (command, _parent, _child) = yi_agent_supervisors::lifecycle::bringup::guardian_command(
        &launcher,
        &scope,
        &artifact("b1"),
    )
    .unwrap();

    let environment: std::collections::HashMap<String, Option<std::ffi::OsString>> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|v| v.to_owned()),
            )
        })
        .collect();
    let read = |name: &str| {
        environment
            .get(name)
            .and_then(|value| value.as_ref())
            .map(|value| value.to_string_lossy().into_owned())
    };

    assert_eq!(
        read("YI_AGENT_PROJECT_KEY").as_deref(),
        Some(scope.project_key.as_str()),
        "the project must be named, not inferred from the runtime cwd"
    );
    assert_eq!(
        read("YI_AGENT_SCOPE_KEY").as_deref(),
        Some(scope.key.as_str())
    );
    assert_eq!(
        read("YI_AGENT_INSTANCE_HOME").as_deref(),
        Some(scope.instance_home.to_string_lossy().as_ref())
    );
    // The control cwd is the runtime directory, and that is exactly why the project
    // cannot be derived from it.
    assert_eq!(command.get_current_dir(), Some(scope.runtime_dir.as_path()));
}

/// A real child, launched with the launcher's own cwd and environment, reports the
/// identity it was given on an isolated pipe.
///
/// The environment assertion above proves what the launcher *declares*. This proves
/// the declaration survives into a running process whose working directory is the
/// runtime directory -- the A19 shape. A shell stands in for the guardian because
/// the guardian's own bootstrap handshake is covered by `managed_runtime_lifecycle`;
/// what matters here is that no step in between rewrites or drops the identity, and
/// that the identity cannot have been derived from where the child stands.
#[test]
fn a_real_child_reports_the_identity_the_launcher_declared() {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let linked = dir.path().join("project-alias");
    std::os::unix::fs::symlink(&project, &linked).unwrap();
    let runtime = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let instance_home = dir.path().join("home");
    std::fs::create_dir_all(&instance_home).unwrap();

    // Resolved through the symlink; the launcher must still name the real project.
    let scope = resolve(&runtime, &linked, &instance_home).unwrap();
    let launcher = yi_agent_supervisors::lifecycle::bringup::ProcessGuardian {
        instance_home: instance_home.clone(),
    };
    let (declared, _parent, _child) = yi_agent_supervisors::lifecycle::bringup::guardian_command(
        &launcher,
        &scope,
        &artifact("b1"),
    )
    .unwrap();

    let child_cwd = declared
        .get_current_dir()
        .expect("the launcher sets the runtime cwd")
        .to_path_buf();
    let canonical_project = std::fs::canonicalize(&project).unwrap();
    // The premise of the whole check: a child standing in the runtime directory
    // cannot name the project by looking down.
    assert_ne!(child_cwd, canonical_project);

    // The shell stands in for the guardian; the cwd and every environment binding
    // are taken from the launcher verbatim, so this test cannot pass by agreeing
    // with itself.
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(
            "printf 'project=%s\\nscope=%s\\ninstance=%s\\ncwd=%s\\n' \
             \"$YI_AGENT_PROJECT_KEY\" \"$YI_AGENT_SCOPE_KEY\" \
             \"$YI_AGENT_INSTANCE_HOME\" \"$(pwd -P)\" >&3",
        )
        .current_dir(&child_cwd)
        .stderr(std::process::Stdio::inherit());
    for (key, value) in declared.get_envs() {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }

    // An isolated pipe on descriptor 3, not stdout: the guardian's stdout is a log,
    // and a report that shared it would be unreadable the moment anything else
    // wrote a line.
    let mut ends = [0i32; 2];
    assert_eq!(
        unsafe { libc::pipe(ends.as_mut_ptr()) },
        0,
        "pipe: {}",
        std::io::Error::last_os_error()
    );
    let (read_end, write_end) = (ends[0], ends[1]);
    // SAFETY: `pre_exec` runs between fork and exec in a single-threaded child; it
    // only rearranges that child's own descriptor table.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(write_end, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    };

    let mut child = command.spawn().unwrap();
    // Our copy of the child's end must go, or the read never reaches EOF.
    assert_eq!(unsafe { libc::close(write_end) }, 0);
    let mut report = String::new();
    // SAFETY: `read_end` came from `pipe` above and is owned by this test.
    unsafe { std::fs::File::from_raw_fd(read_end) }
        .read_to_string(&mut report)
        .unwrap();
    let status = child.wait().unwrap();

    assert!(status.success(), "the reporting child failed: {report}");
    let field = |name: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("no {name} in {report:?}"))
    };
    assert_eq!(field("project"), canonical_project.to_string_lossy());
    assert_eq!(field("scope"), scope.key);
    assert_eq!(
        field("instance"),
        scope.instance_home.to_string_lossy().as_ref()
    );
    assert_eq!(field("cwd"), scope.runtime_dir.to_string_lossy().as_ref());
}

/// A guardian that cannot be told its project refuses instead of guessing one.
///
/// Run in a child process because the environment is process-global: the claim is
/// "when the variable is absent, the role refuses", which no in-process assertion
/// can make.
#[test]
fn a_guardian_without_a_named_project_has_no_fallback() {
    use std::process::{Command, Stdio};
    let binary = std::env::var("CARGO_BIN_EXE_yi-agent").ok();
    let Some(binary) = binary else {
        // `runtime_scope` is a supervisors test binary; the CLI binary is not built
        // for it. The refusal is covered by yi-agent's own `managed_roles` tests.
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let filler = std::fs::File::open("/dev/null").unwrap();
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let raw = filler.as_raw_fd();
    let mut command = Command::new(binary);
    command
        .args(["internal-runtime", "guardian"])
        .current_dir(&runtime)
        .env_remove("YI_AGENT_PROJECT_KEY")
        .env("YI_AGENT_SCOPE_KEY", runtime.to_string_lossy().to_string())
        .env("YI_AGENT_BOOTSTRAP_FD", "9")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `pre_exec` only rearranges this fork's descriptor table before `exec`.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(raw, 9) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("YI_AGENT_PROJECT_KEY"),
        "the refusal must name the missing declaration: {stderr}"
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
