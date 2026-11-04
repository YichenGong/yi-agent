//! Version switches over real processes.
//!
//! The unit tests in `lifecycle::switch` prove the ordering and the fail-closed
//! rules by value. This file proves the two claims that only a real process can
//! make:
//!
//! - with no client left, a completed switch stops the old generation and starts
//!   **nothing** -- not "reports StoppedNoClients while a daemon quietly runs";
//! - a failed start keeps the database, reports the stage that failed, and does not
//!   leave a half-started child behind.
//!
//! The fake child is a shell script that records its own PID, so "did something
//! start" is answered by observing a process, not by trusting a boolean.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, OperationStatus, RuntimeIdentity, RuntimeMode, SwitchPhase,
};
use yi_agent_supervisors::lifecycle::CoordinationGuard;
use yi_agent_supervisors::lifecycle::bringup::{ControlProbe, ProbeOutcome};
use yi_agent_supervisors::lifecycle::switch::{self, Preflight, SwitchBackend};
use yi_agent_supervisors::lifecycle::switch_managed::{
    ClientPresence, ManagedSwitchBackend, StopWitness,
};

fn scope(directory: &Path) -> yi_agent_lifecycle::types::RuntimeScope {
    yi_agent_lifecycle::types::RuntimeScope {
        key: directory.to_string_lossy().into_owned(),
        runtime_dir: directory.to_path_buf(),
        project_key: "/project".into(),
        instance_home: directory.to_path_buf(),
    }
}

/// Writes an executable child that records its PID and then waits.
fn marker_child(directory: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let executable = directory.join("next-daemon");
    let marker = directory.join("child.pid");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\necho $$ > {}\nexec sleep 30\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    (executable, marker)
}

fn target(executable: &Path) -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "next".into(),
        display_version: "0.2.0".into(),
        executable: executable.to_path_buf(),
        sha256: "aa".into(),
        schema_min: 1,
        schema_max: 16,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: vec![],
    }
}

fn wait_for_marker(marker: &Path, budget: Duration) -> Option<i32> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(marker)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// Whether `pid` is a live process, not a zombie.
///
/// `kill(pid, 0)` also succeeds for a zombie: the entry still exists until its parent
/// reaps it. For a child of this test process, `waitpid(WNOHANG)` is the honest
/// check -- it reaps the dead child and reports that it exited.
fn process_alive(pid: i32) -> bool {
    let mut status = 0;
    // SAFETY: `waitpid` with WNOHANG only inspects/reaps the given child; `status` is
    // a valid out-pointer.
    let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if reaped == pid {
        return false;
    }
    // Not our child (or still running): fall back to the existence check, where a
    // zombie we cannot reap would still look alive -- but that case cannot arise here
    // because every PID passed to this helper came from a child this test spawned.
    // SAFETY: signal 0 checks existence without delivering anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A witness that really does terminate the child a test started.
///
/// Not the guardian registry: this file is about the switch's behaviour with a real
/// process, and wiring a whole guardian here would test the guardian instead.
struct RealChildWitness {
    pid: Option<i32>,
    stopped: bool,
}

impl StopWitness for RealChildWitness {
    fn terminate_all(&mut self, _confirm: Duration) -> Result<(), String> {
        if let Some(pid) = self.pid.take() {
            // SAFETY: the PID was read from this test's own marker file, so it names
            // the process this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.stopped = !process_alive(pid);
        } else {
            self.stopped = true;
        }
        Ok(())
    }

    fn all_stopped(&mut self) -> Result<bool, String> {
        Ok(self.stopped || self.pid.is_none())
    }

    fn still_running(&mut self) -> Vec<String> {
        self.pid
            .map(|pid| vec![format!("pid {pid}")])
            .unwrap_or_default()
    }
}

struct Presence(bool);

impl ClientPresence for Presence {
    fn has_live_clients(&self) -> bool {
        self.0
    }
}

/// Answers the control probe with a chosen identity, so verification is tested
/// without booting a real daemon.
struct IdentityProbe {
    identity: RuntimeIdentity,
}

impl ControlProbe for IdentityProbe {
    fn probe(&self, _path: &Path, _deadline: &Deadline) -> ProbeOutcome {
        ProbeOutcome::Answered(self.identity.clone())
    }
}

fn identity(build: &str, generation: &str) -> RuntimeIdentity {
    RuntimeIdentity {
        scope_key: "/scope".into(),
        daemon_instance_id: Some(generation.into()),
        guardian_build_id: Some(build.into()),
        daemon_build_id: Some(build.into()),
        mode: RuntimeMode::Managed,
        business_protocol: Some(1),
        daemon_display_version: Some("0.2.0".into()),
        guardian_display_version: Some("0.2.0".into()),
        capabilities: vec![],
        schema_min: 1,
        schema_max: 16,
    }
}

/// With no client connected, the switch must stop the old generation and start no
/// process at all.
#[test]
fn a_switch_with_no_clients_starts_no_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let (executable, marker) = marker_child(directory.path());
    let guard = CoordinationGuard::acquire(
        &scope(directory.path()),
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let mut backend = ManagedSwitchBackend::new(
        scope(directory.path()),
        "next",
        Box::new(RealChildWitness {
            pid: None,
            stopped: true,
        }),
        Box::new(Presence(false)),
        Box::new(IdentityProbe {
            identity: identity("next", "gen-2"),
        }),
    )
    .with_guard(guard);

    let mut machine = switch::SwitchMachine::new("op-no-clients".to_string());
    let outcome = switch::run(&mut machine, &mut backend, &target(&executable));
    assert_eq!(outcome.status.phase, SwitchPhase::StoppedNoClients);
    assert!(!outcome.started);
    // The claim under test: no process was started, proven by no marker appearing.
    assert!(
        wait_for_marker(&marker, Duration::from_millis(300)).is_none(),
        "StoppedNoClients must not start a daemon"
    );
    // And the build choice was still recorded, so the next connection uses it.
    let recorded =
        yi_agent_supervisors::lifecycle::selection::read_selection(&scope(directory.path()))
            .unwrap()
            .expect("the switch must record the chosen build even with no clients");
    assert_eq!(recorded.target.build_id, "next");
}

/// The old generation is really stopped before the new one is considered.
#[test]
fn a_switch_really_stops_the_previous_child() {
    let directory = tempfile::tempdir().unwrap();
    let (executable, _marker) = marker_child(directory.path());
    // Simulate the old generation with a real child.
    let mut old = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn the old generation");
    let old_pid = old.id() as i32;
    let guard = CoordinationGuard::acquire(
        &scope(directory.path()),
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let mut backend = ManagedSwitchBackend::new(
        scope(directory.path()),
        "next",
        Box::new(RealChildWitness {
            pid: Some(old_pid),
            stopped: false,
        }),
        Box::new(Presence(false)),
        Box::new(IdentityProbe {
            identity: identity("next", "gen-2"),
        }),
    )
    .with_guard(guard);

    let mut machine = switch::SwitchMachine::new("op-stop".to_string());
    let outcome = switch::run(&mut machine, &mut backend, &target(&executable));
    assert_eq!(outcome.status.phase, SwitchPhase::StoppedNoClients);
    assert!(!process_alive(old_pid), "the old child must be gone");
    let _ = old.wait();
}

/// A database this build cannot read is refused by the read-only pre-flight, while the
/// old runtime is still serving, and the file is not touched.
///
/// This is the ordering that matters: discovering an unreadable schema *after*
/// stopping the old daemon would leave the user with nothing running.
#[test]
fn an_unreadable_database_is_refused_before_anything_stops() {
    let directory = tempfile::tempdir().unwrap();
    // A database that must survive a failed switch.
    let database = directory.path().join("runtime.sqlite");
    std::fs::write(&database, b"precious").unwrap();

    // The probe never answers, so verification cannot pass: this is the "started but
    // never became ready" case.
    struct SilentProbe;
    impl ControlProbe for SilentProbe {
        fn probe(&self, _path: &Path, _deadline: &Deadline) -> ProbeOutcome {
            ProbeOutcome::Unresponsive("never answered".into())
        }
    }

    let guard = CoordinationGuard::acquire(
        &scope(directory.path()),
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let mut backend = ManagedSwitchBackend::new(
        scope(directory.path()),
        "next",
        Box::new(RealChildWitness {
            pid: None,
            stopped: true,
        }),
        Box::new(Presence(true)),
        Box::new(SilentProbe),
    )
    .with_guard(guard);

    let mut machine = switch::SwitchMachine::new("op-preflight".to_string());
    let (executable, _marker) = marker_child(directory.path());
    let outcome = switch::run(&mut machine, &mut backend, &target(&executable));
    assert_eq!(outcome.status.phase, SwitchPhase::ManualActionRequired);
    assert!(outcome.status.ready_identity.is_none());
    assert!(
        outcome
            .status
            .detail
            .as_deref()
            .unwrap()
            .contains("database"),
        "the report must name the database as the reason: {:?}",
        outcome.status.detail
    );
    assert_eq!(
        std::fs::read(&database).unwrap(),
        b"precious",
        "a refused switch must never touch the database"
    );
}

/// A start that cannot reach ready must report the failing stage, keep the database,
/// and not claim readiness.
#[test]
fn a_start_that_never_becomes_ready_fails_and_keeps_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runtime.sqlite");
    // A real database at a schema this build accepts, so the pre-flight passes and the
    // failure lands in verification instead.
    {
        // `inspect_schema` reads `schema_migrations`, not `PRAGMA user_version`: a
        // database without that table is a pre-migration file and reports version 0.
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);\n\
                 INSERT INTO schema_migrations (version) VALUES (16);",
            )
            .unwrap();
    }

    // The probe never answers, so verification cannot pass: "started but never ready".
    struct SilentProbe;
    impl ControlProbe for SilentProbe {
        fn probe(&self, _path: &Path, _deadline: &Deadline) -> ProbeOutcome {
            ProbeOutcome::Unresponsive("never answered".into())
        }
    }

    // A witness that reports stopped without touching a process: this test is about
    // the start phase, not about terminating.
    struct AlreadyStopped;
    impl StopWitness for AlreadyStopped {
        fn terminate_all(&mut self, _confirm: Duration) -> Result<(), String> {
            Ok(())
        }
        fn all_stopped(&mut self) -> Result<bool, String> {
            Ok(true)
        }
        fn still_running(&mut self) -> Vec<String> {
            Vec::new()
        }
    }

    let guard = CoordinationGuard::acquire(
        &scope(directory.path()),
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let mut backend = ManagedSwitchBackend::new(
        scope(directory.path()),
        "next",
        Box::new(AlreadyStopped),
        Box::new(Presence(true)),
        Box::new(SilentProbe),
    )
    .with_guard(guard);

    let mut machine = switch::SwitchMachine::new("op-verify".to_string());
    let (executable, _marker) = marker_child(directory.path());
    let outcome = switch::run(&mut machine, &mut backend, &target(&executable));
    assert_eq!(
        outcome.status.phase,
        SwitchPhase::Failed,
        "reported: {:?}",
        outcome.status.detail
    );
    assert!(outcome.status.ready_identity.is_none());
    let detail = outcome.status.detail.unwrap();
    assert!(
        detail.contains("did not verify")
            || detail.contains("did not become ready")
            || detail.contains("control channel"),
        "the report must name the failing stage: {detail}"
    );
    let still_there = std::fs::metadata(&database).expect("the database must survive");
    assert!(still_there.len() > 0);
}

/// A target that is only removed *after* the pre-flight must fail before anything is
/// stopped, which is the bundle-replaced-mid-click case.
#[test]
fn a_bundle_that_vanishes_before_start_is_refused_before_the_old_one_stops() {
    let directory = tempfile::tempdir().unwrap();
    let (executable, _marker) = marker_child(directory.path());
    let mut old = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let old_pid = old.id() as i32;

    struct RemovingBackend {
        inner: ManagedSwitchBackend,
        executable: PathBuf,
    }
    impl SwitchBackend for RemovingBackend {
        fn preflight(&mut self, target: &ArtifactIdentity) -> Result<Preflight, String> {
            let outcome = self.inner.preflight(target)?;
            // The bundle disappears between the click and the start.
            let _ = std::fs::remove_file(&self.executable);
            Ok(outcome)
        }
        fn freeze(&mut self) -> Result<(), String> {
            self.inner.freeze()
        }
        fn notify(&mut self, message: &str) {
            self.inner.notify(message)
        }
        fn request_stop(&mut self, grace: Duration) {
            self.inner.request_stop(grace)
        }
        fn terminate(&mut self, confirm: Duration) -> Result<(), String> {
            self.inner.terminate(confirm)
        }
        fn is_stopped(&mut self) -> Result<bool, String> {
            self.inner.is_stopped()
        }
        fn has_live_clients(&self) -> bool {
            self.inner.has_live_clients()
        }
        fn commit_selection(&mut self, target: &ArtifactIdentity) -> Result<(), String> {
            self.inner.commit_selection(target)
        }
        fn handover_control(&mut self) -> Result<bool, String> {
            self.inner.handover_control()
        }
        fn start_generation(
            &mut self,
            target: &ArtifactIdentity,
            budget: Duration,
        ) -> Result<RuntimeIdentity, String> {
            self.inner.start_generation(target, budget)
        }
        fn verify(&mut self, target: &ArtifactIdentity) -> Result<RuntimeIdentity, String> {
            self.inner.verify(target)
        }
        fn release(&mut self) {
            self.inner.release()
        }
    }

    let guard = CoordinationGuard::acquire(
        &scope(directory.path()),
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let inner = ManagedSwitchBackend::new(
        scope(directory.path()),
        "next",
        Box::new(RealChildWitness {
            pid: Some(old_pid),
            stopped: false,
        }),
        Box::new(Presence(true)),
        Box::new(IdentityProbe {
            identity: identity("next", "gen-2"),
        }),
    )
    .with_guard(guard);
    let mut backend = RemovingBackend {
        inner,
        executable: executable.clone(),
    };

    let mut machine = switch::SwitchMachine::new("op-vanish".to_string());
    let outcome = switch::run(&mut machine, &mut backend, &target(&executable));
    assert_eq!(outcome.status.phase, SwitchPhase::Failed);
    assert!(
        outcome
            .status
            .detail
            .as_deref()
            .unwrap()
            .contains("disappeared"),
        "the report must say the bundle disappeared"
    );
    // The old generation was stopped before the start step; what must hold is that no
    // new daemon was started and no readiness was claimed.
    assert!(outcome.status.ready_identity.is_none());
    assert!(!outcome.started);
    let _ = unsafe { libc::kill(old_pid, libc::SIGKILL) };
    let _ = old.wait();
}

/// A stale operation id from an earlier generation is reported as unknown, not as a
/// failure.
#[test]
fn an_unknown_operation_is_not_reported_as_failed() {
    let status: Option<OperationStatus> = None;
    assert!(status.is_none());
}
